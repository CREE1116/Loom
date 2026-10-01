//! Model effort is explicit, capability-checked, and captured with queued work.
use super::*;
use crate::agent::ModelProfile;
impl App {
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
        self.notify("다음 메시지부터 선택한 effort를 사용합니다.");
    }
    pub(super) fn reconcile_effort(&mut self) {
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
        let mut rows = vec![("Reasoning effort · 다음 메시지".into(), Tone::Accent, None)];
        rows.push((
            format!(
                "{} runtime 설정 유지{}",
                if self.effort.is_none() { "●" } else { "○" },
                self.runtime_effort
                    .as_ref()
                    .map(|e| format!(" · 현재 {e}"))
                    .unwrap_or_default()
            ),
            Tone::Muted,
            Some(Action::Effort(String::new())),
        ));
        if let Some(profile) = self.model_profile() {
            if let Some(default) = &profile.default_effort {
                rows.push((
                    format!("모델 기본: {default}"),
                    Tone::Muted,
                    Some(Action::Effort("default".into())),
                ));
            }
            if profile.efforts.is_empty() {
                rows.push(("지원 effort 정보 없음".into(), Tone::Muted, None));
            }
            for option in &profile.efforts {
                rows.push((
                    format!(
                        "{} effort {}",
                        if self.effort.as_ref() == Some(&option.value) {
                            "●"
                        } else {
                            "○"
                        },
                        option.value
                    ),
                    Tone::Normal,
                    Some(Action::Effort(option.value.clone())),
                ));
                for line in wrap(&option.description, width) {
                    rows.push((line, Tone::Muted, None));
                }
            }
        } else {
            rows.push((
                "지원 effort 정보 없음 · 모델 목록을 확인하세요.".into(),
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
