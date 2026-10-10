use super::*;
use crate::agent::GoalAction;

impl App {
    pub(super) fn management_command(&mut self, name: &str, arg: &str) -> Option<Option<Intent>> {
        let mutation = matches!(
            name,
            "/clear" | "/compression" | "/compact" | "/compress" | "/stop" | "/cancel"
        ) || (name == "/goal" && !arg.is_empty())
            || (name == "/agent" && (arg.contains(" send ") || arg.ends_with(" stop")))
            || (name == "/skill" && !arg.is_empty() && arg != "list" && !arg.starts_with("info "));
        if mutation && self.readonly {
            self.error("열람 전용 창입니다. 메인 창에서 실행하세요.");
            return Some(None);
        }
        let intent = match name {
            "/retry" => return Some(self.retry_failed_turn()),
            "/goal" => {
                let action = match arg {
                    "" => GoalAction::Show,
                    "pause" => GoalAction::Pause,
                    "resume" => GoalAction::Resume,
                    "clear" => GoalAction::Clear,
                    _ => match arg.strip_prefix("set ").filter(|s| !s.trim().is_empty()) {
                        Some(text) => GoalAction::Set(text.trim().into()),
                        None => {
                            self.error("사용법: /goal [set TEXT|pause|resume|clear]");
                            return Some(None);
                        }
                    },
                };
                self.activate(Action::Agents);
                Some(Intent::Goal(action))
            }
            "/clear" => {
                if !arg.is_empty() {
                    self.error("사용법: /clear · 기록을 보존하고 새 대화를 엽니다.");
                    None
                } else if let Some(reason) = self.session_switch_block_reason() {
                    self.error(reason);
                    None
                } else {
                    Some(Intent::Session(None))
                }
            }
            "/compression" | "/compact" | "/compress" => {
                if !arg.is_empty() {
                    self.error("사용법: /compression 또는 /compact");
                    None
                } else if let Some(reason) = self.session_switch_block_reason() {
                    self.error(reason);
                    None
                } else if self.thread.is_none() {
                    self.error("세션 연결이 준비되지 않았습니다.");
                    None
                } else {
                    Some(Intent::Compact)
                }
            }
            "/remote" => match arg {
                "" | "on" | "local" => match self.remote.as_mut() {
                    // Already on: show the address and QR code again.
                    Some(view) => {
                        view.shown = true;
                        None
                    }
                    None if self.readonly => {
                        self.error("열람 전용 창입니다. 메인 창에서 실행하세요.");
                        None
                    }
                    None => Some(Intent::Remote(Some(arg != "local"))),
                },
                "off" | "stop" => self.remote.is_some().then_some(Intent::Remote(None)),
                "hide" => {
                    if let Some(view) = self.remote.as_mut() {
                        view.shown = false;
                    }
                    None
                }
                "copy" => self.remote.as_ref().map(|view| Intent::Copy(view.url.clone())),
                _ => {
                    self.error("사용법: /remote [local|off] · local은 같은 네트워크에서만 접속");
                    None
                }
            },
            "/stop" | "/cancel" => {
                if arg.is_empty() {
                    self.activate(Action::Interrupt)
                } else {
                    self.error("사용법: /stop");
                    None
                }
            }
            "/status" => {
                self.activate(Action::Agents);
                None
            }
            "/doctor" => {
                self.run_doctor();
                None
            }
            "/cost" => {
                self.run_cost();
                None
            }
            "/context" => {
                self.run_context();
                None
            }
            "/agent" if arg.is_empty() || arg == "list" => {
                self.activate(Action::Agents);
                None
            }
            "/agent" => {
                let (number, rest) = arg.split_once(' ').unwrap_or((arg, ""));
                let rest = rest.trim();
                if rest != "stop" && !rest.starts_with("send ") {
                    return None;
                }
                let Some(agent) = command_index(number).and_then(|i| self.agents.get(i)) else {
                    self.error("해당 번호의 하위 에이전트가 없습니다. /agent list로 확인하세요.");
                    return Some(None);
                };
                let id = agent.id.clone();
                let text = rest.strip_prefix("send ").map(str::trim);
                if text == Some("") {
                    self.error("사용법: /agent N send TEXT");
                    None
                } else {
                    Some(Intent::DirectAgent(id, text.map(str::to_owned)))
                }
            }
            "/image" => {
                self.image_command(arg);
                None
            }
            "/skill" if arg == "list" => {
                self.panel_open = true;
                self.switch_view(View::Skills);
                Some(Intent::RefreshSkills)
            }
            "/skill" if arg == "clear" => {
                self.selected_skills.clear();
                self.notify("선택된 스킬을 모두 해제했습니다.");
                None
            }
            "/skill" if arg.starts_with("info ") => {
                let name = arg[5..].trim();
                if let Some(skill) = self
                    .skills
                    .iter()
                    .find(|s| s.name.eq_ignore_ascii_case(name))
                {
                    let message =
                        format!("{} · {} · {}", skill.name, skill.description, skill.path);
                    self.notify(message);
                } else {
                    self.error("스킬을 찾지 못했습니다. /skill list로 확인하세요.");
                }
                None
            }
            _ => return None,
        };
        Some(intent)
    }

    pub(super) fn management_rows(&self, width: usize) -> Vec<ContentRow> {
        let mut rows = vec![(
            format!(
                "세션 · {} · 대기열 {}개{}",
                if self.compacting {
                    "컨텍스트 압축 중"
                } else if self.busy {
                    "실행 중"
                } else {
                    "대기"
                },
                self.queued.len(),
                if self.queue_paused {
                    " · 일시정지"
                } else {
                    ""
                }
            ),
            Tone::Accent,
            None,
        )];
        if let Some(goal) = &self.goal {
            rows.push((format!("목표 · {}", goal.status), Tone::Accent, None));
            rows.extend(
                wrap(&goal.objective, width)
                    .into_iter()
                    .map(|line| (line, Tone::Normal, None)),
            );
            if let Some(used) = goal.tokens_used {
                rows.push((
                    format!(
                        "목표 토큰 · {used} / {}",
                        goal.token_budget
                            .map_or("제한 없음".into(), |v| v.to_string())
                    ),
                    Tone::Muted,
                    None,
                ));
            }
        }
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn run(app: &mut App, text: &str) -> Option<Intent> {
        app.activate(Action::Command(text.into()))
    }
    #[test]
    fn management_commands_preserve_sessions_and_queue_guards() {
        let mut app = App::new(false);
        app.thread = Some("main".into());
        assert_eq!(run(&mut app, "/clear"), Some(Intent::Session(None)));
        assert_eq!(run(&mut app, "/compact"), Some(Intent::Compact));
        assert_eq!(run(&mut app, "/compression"), Some(Intent::Compact));
        assert_eq!(run(&mut app, "/compress"), Some(Intent::Compact));
        assert_eq!(
            run(&mut app, "/goal set 한국어 지원"),
            Some(Intent::Goal(GoalAction::Set("한국어 지원".into())))
        );
        app.compacting = true;
        assert!(run(&mut app, "/clear").is_none());
        app.editor.insert("보존할 지시");
        app.activate(Action::Send);
        assert_eq!(app.queued.len(), 1);
        assert!(app.next_queued().is_none());
        app.compacting = false;
        run(&mut app, "/queue pause");
        assert!(app.queue_paused);
        assert!(app.next_queued().is_none());
        assert!(run(&mut app, "/queue resume").is_none());
        assert!(!app.queue_paused);
        assert!(app.next_queued().is_some());
    }
    #[test]
    fn commands_are_discoverable_and_readonly_blocks_mutations() {
        let mut app = App::new(true);
        for command in [
            "/clear",
            "/compact",
            "/compression",
            "/goal set x",
            "/skill clear",
            "/agent 1 stop",
            "/stop",
        ] {
            assert!(run(&mut app, command).is_none());
            assert!(app.notice_error, "{command}");
        }
        assert_eq!(run(&mut app, "/goal"), Some(Intent::Goal(GoalAction::Show)));
        assert!(run(&mut app, "/agent list").is_none());
        assert_eq!(app.view, View::Agents);
        for name in [
            "/goal",
            "/clear",
            "/compression",
            "/stop",
            "/status",
            "/agent",
            "/skill",
            "/doctor",
            "/cost",
            "/context",
        ] {
            assert!(COMMANDS.iter().any(|(command, _)| *command == name));
        }
        assert!(run(&mut app, "/doctor").is_none());
        assert!(run(&mut app, "/cost").is_none());
        assert!(run(&mut app, "/context").is_none());
        assert_eq!(app.entries.len(), 3);
    }
}
