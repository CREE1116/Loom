//! Model effort is explicit, capability-checked, and captured with queued work.
use super::*;
use crate::agent::ModelProfile;
impl App {
    pub fn configure_model_memory(&mut self, engine: &str, effort: Option<String>) {
        self.model_default_allowed = !self.preferences.model_selections.contains_key(engine);
        self.model_engine = Some(engine.into());
        self.effort = effort;
    }
    pub(super) fn apply_initial_model_default(&mut self) {
        if !self.readonly
            && self.model_default_allowed
            && self.apply_fast_local_default()
        {
            self.remember_model_choice();
            self.notify("로컬 모델은 추론 끄기로 시작합니다. 환경 → 모델에서 바꾸면 기억합니다.");
        }
        self.model_default_allowed = false;
    }
    pub(super) fn apply_fast_local_default(&mut self) -> bool {
        if self.model_engine.as_deref() == Some("loom")
            && self.model.starts_with("ollama/")
            && self.effort.is_none()
            && self.model_profile().is_some_and(|profile| {
                profile.efforts.iter().any(|effort| effort.value == "none")
                    && profile.efforts.iter().any(|effort| effort.value == "on")
                    && profile.efforts.len() == 2
            })
        {
            self.effort = Some("none".into());
            true
        } else {
            false
        }
    }
    pub fn take_model_memory_dirty(&mut self) -> bool {
        std::mem::take(&mut self.model_memory_dirty)
    }
    pub fn remember_model_choice(&mut self) {
        if self.readonly {
            return;
        }
        let Some(engine) = &self.model_engine else {
            return;
        };
        let saved = self
            .preferences
            .model_selections
            .entry(engine.clone())
            .or_default();
        saved.model.clone_from(&self.model);
        saved
            .efforts
            .insert(self.model.clone(), self.effort.clone());
        self.model_memory_dirty = true;
    }

    pub(super) fn model_profile(&self) -> Option<&ModelProfile> {
        self.model_profiles
            .iter()
            .find(|profile| profile.id == self.model)
    }
    pub(super) fn select_effort(&mut self, value: &str) {
        if self.readonly || self.submitted.is_some() {
            return;
        }
        let effort = if value.is_empty() {
            None
        } else if value == "default" {
            self.model_profile()
                .and_then(|profile| profile.default_effort.clone())
        } else {
            Some(value.into())
        };
        if let Some(effort) = &effort
            && !self
                .model_profile()
                .is_some_and(|profile| profile.efforts.iter().any(|option| &option.value == effort))
        {
            self.error("선택한 모델이 지원하는 effort를 /model에서 확인하세요.");
            return;
        }
        if value == "default" && effort.is_none() {
            self.error("모델 기본 effort 정보가 없습니다. /model에서 확인하세요.");
            return;
        }
        self.effort = effort;
        self.remember_model_choice();
        self.notify("다음 메시지부터 선택한 effort를 사용합니다.");
    }
    pub(super) fn reconcile_effort(&mut self) {
        // SessionLoaded precedes capability discovery. Do not discard a restored
        // preference while the model catalogue is still loading.
        if self.model_profiles.is_empty() {
            return;
        }
        if self.effort.as_ref().is_some_and(|effort| {
            !self
                .model_profile()
                .is_some_and(|profile| profile.efforts.iter().any(|option| &option.value == effort))
        }) {
            self.effort = None;
            self.notify(
                "새 모델에서 지원하지 않는 effort를 해제했습니다. runtime 설정을 사용합니다.",
            );
        }
    }
    pub(super) fn effort_rows(&self, width: usize) -> Vec<(String, Tone, Option<Action>)> {
        let english = self.preferences.language == Language::English;
        let mut rows = vec![];
        panel_section(&mut rows, "Reasoning effort", width);
        // Name the default only when the runtime reported it; no placeholder dash.
        let runtime = self
            .runtime_effort
            .as_deref()
            .map(|effort| format!(" · {effort}"))
            .unwrap_or_default();
        rows.push((
            format!(
                "{} {}{runtime}",
                if self.effort.is_none() { "●" } else { "○" },
                if english {
                    "Keep runtime setting"
                } else {
                    "runtime 설정 유지"
                }
            ),
            Tone::Normal,
            Some(Action::Effort(String::new())),
        ));
        if let Some(profile) = self.model_profile() {
            if profile.efforts.is_empty() {
                rows.push((
                    if english {
                        "No effort options reported"
                    } else {
                        "지원 effort 정보 없음"
                    }
                    .into(),
                    Tone::Muted,
                    None,
                ));
            }
            for option in &profile.efforts {
                let default = profile.default_effort.as_ref() == Some(&option.value);
                let hint = match (option.value.as_str(), english) {
                    ("none", false) => "추론 없이",
                    ("on", false) => "추론 켜기",
                    ("on", true) => "Reasoning on",
                    ("minimal", false) => "최소 추론",
                    ("low", false) => "빠르게",
                    ("medium", false) => "균형 있게",
                    ("high", false) => "신중하게",
                    ("xhigh", false) => "깊게",
                    ("none", true) => "No reasoning",
                    ("minimal", true) => "Minimal",
                    ("low", true) => "Quick",
                    ("medium", true) => "Balanced",
                    ("high", true) => "Thorough",
                    ("xhigh", true) => "Deep",
                    _ => "",
                };
                rows.push((
                    format!(
                        "{} effort {}{}{}",
                        if self.effort.as_ref() == Some(&option.value) {
                            "●"
                        } else {
                            "○"
                        },
                        option.value,
                        if hint.is_empty() {
                            String::new()
                        } else {
                            format!(" · {hint}")
                        },
                        if default {
                            if english { " (default)" } else { " (기본)" }
                        } else {
                            ""
                        }
                    ),
                    Tone::Normal,
                    Some(Action::Effort(option.value.clone())),
                ));
            }
            let focused = match &self.focus {
                Some(Action::Effort(value)) => Some(value.as_str()),
                _ => self.effort.as_deref(),
            };
            if let Some(option) =
                focused.and_then(|value| profile.efforts.iter().find(|o| o.value == value))
            {
                rows.push((String::new(), Tone::Normal, None));
                for line in wrap(&option.description, width).into_iter().take(2) {
                    rows.push((line, Tone::Muted, None));
                }
            }
        } else {
            rows.push((
                if english {
                    "Choose a model to see effort options"
                } else {
                    "모델을 선택하면 추론 옵션이 표시됩니다"
                }
                .into(),
                Tone::Muted,
                None,
            ));
        }
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AgentEvent, AgentUpdate, EffortOption};
    #[test]
    fn first_local_boolean_thinking_choice_is_fast_and_explicit_choices_survive() {
        let mut app = App::new(false);
        app.model = "ollama/spark".into();
        app.configure_model_memory("loom", None);
        let profiles = vec![ModelProfile {
            id: app.model.clone(),
            default_effort: None,
            efforts: ["none", "on"]
                .into_iter()
                .map(|value| EffortOption {
                    value: value.into(),
                    description: String::new(),
                })
                .collect(),
        }];
        app.apply_event(AgentEvent::global(AgentUpdate::ModelProfilesLoaded(
            profiles.clone(),
        )));
        assert_eq!(app.effort.as_deref(), Some("none"));
        app.select_effort("on");
        let saved = app.preferences.clone();
        let mut restored = App::new(false);
        restored.preferences = saved;
        restored.model = app.model.clone();
        restored.configure_model_memory("loom", Some("on".into()));
        restored.apply_event(AgentEvent::global(AgentUpdate::ModelProfilesLoaded(
            profiles,
        )));
        assert_eq!(restored.effort.as_deref(), Some("on"));
    }

    #[test]
    fn newly_selected_local_thinking_model_defaults_fast_even_with_existing_engine_memory() {
        let mut app = App::new(false);
        app.models = vec!["auto".into(), "ollama/spark".into()];
        app.model = "auto".into();
        app.preferences.model_selections.insert(
            "loom".into(),
            crate::preferences::ModelSelection {
                model: "auto".into(),
                efforts: [("auto".into(), None)].into_iter().collect(),
            },
        );
        app.configure_model_memory("loom", None);
        app.apply_event(AgentEvent::global(AgentUpdate::ModelProfilesLoaded(vec![
            ModelProfile {
                id: "ollama/spark".into(),
                default_effort: None,
                efforts: ["none", "on"]
                    .into_iter()
                    .map(|value| EffortOption {
                        value: value.into(),
                        description: String::new(),
                    })
                    .collect(),
            },
        ])));
        let intent = app.activate(Action::Model("ollama/spark".into()));
        assert!(intent.is_none());
        assert_eq!(app.model, "ollama/spark");
        assert_eq!(app.effort.as_deref(), Some("none"));
    }
    #[test]
    fn choosing_model_keeps_settings_open_and_focuses_supported_effort() {
        let mut app = App::new(false);
        app.models = vec!["first".into(), "second".into()];
        app.model = "first".into();
        app.effort = Some("high".into());
        app.model_profiles = vec![ModelProfile {
            id: "second".into(),
            default_effort: Some("low".into()),
            efforts: vec![EffortOption {
                value: "low".into(),
                description: "빠른 추론".into(),
            }],
        }];
        app.editor.insert("보존할 초안");
        app.activate(Action::Command("/model".into()));
        app.scroll = 100;
        app.activate(Action::Model("second".into()));
        assert_eq!(app.view, View::Models);
        assert!(!app.follow);
        assert_eq!(app.focus, Some(Action::Effort(String::new())));
        assert!(app.effort.is_none());
        assert_eq!(app.editor.text, "보존할 초안");
        for width in [80, 120] {
            let canvas = app.render(width, 40);
            let screen = (0..canvas.height)
                .map(|y| canvas.plain_line(y))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(screen.contains("선택한 모델 · second"));
            assert!(screen.contains("effort low"));
        }
        let canvas = app.render(80, 40);
        app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &canvas);
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &canvas);
        assert_eq!(app.effort.as_deref(), Some("low"));
        assert_eq!(app.view, View::Models);
    }

    #[test]
    fn effort_is_captured_with_each_message_and_survives_failed_submission() {
        let mut app = App::new(false);
        app.thread = Some("main".into());
        app.model = "fixture".into();
        app.models = vec!["fixture".into(), "unknown".into()];
        app.apply_event(AgentEvent::global(AgentUpdate::ModelProfilesLoaded(vec![
            ModelProfile {
                id: "fixture".into(),
                default_effort: Some("low".into()),
                efforts: ["low", "high"]
                    .into_iter()
                    .map(|value| EffortOption {
                        value: value.into(),
                        description: String::new(),
                    })
                    .collect(),
            },
        ])));
        app.select_effort("high");
        app.busy = true;
        app.editor.insert("first");
        app.activate(Action::Send);
        app.select_effort("low");
        app.editor.insert("second");
        app.activate(Action::Send);
        assert_eq!(app.queued[0].effort.as_deref(), Some("high"));
        assert_eq!(app.queued[1].effort.as_deref(), Some("low"));
        app.busy = false;
        let intent = app.next_queued().unwrap();
        assert!(
            matches!(app.agent_command(&intent),Some(AgentCommand::Submit {effort:Some(ref e),..}) if e=="high")
        );
        app.fail_submission("connection failed");
        assert_eq!(app.queued[0].effort.as_deref(), Some("high"));
        app.select_effort("unsupported");
        assert_eq!(app.effort.as_deref(), Some("low"));
        app.activate(Action::Model("unknown".into()));
        assert!(app.effort.is_none());
    }
}
