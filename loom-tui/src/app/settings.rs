//! Workspace preferences, grouped settings and their keyboard/mouse targets.
use super::*;
use crate::preferences::{InputHistory, StartupScreen};

fn web_search_provider(
    root: Option<&std::path::Path>,
) -> std::result::Result<Option<String>, String> {
    let Some(root) = root else {
        return Ok(None);
    };
    let path = root.join(".loom/config.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let value: toml::Value = toml::from_str(&text).map_err(|error| error.to_string())?;
    Ok(value
        .get("web_search")
        .and_then(|web| web.get("provider"))
        .and_then(toml::Value::as_str)
        .map(str::to_owned))
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) enum Page {
    #[default]
    Main,
    Accent,
    Experimental,
    Diagnostics,
    Reset,
}

fn section(rows: &mut Vec<ContentRow>, title: &str) {
    if !rows.is_empty() {
        rows.push((String::new(), Tone::Normal, None));
    }
    rows.push((title.into(), Tone::Muted, None));
}
fn link(rows: &mut Vec<ContentRow>, label: String, key: &str) {
    rows.push((label, Tone::Accent, Some(Action::Setting(key.into()))));
}
fn hint(rows: &mut Vec<ContentRow>, text: &str, width: usize) {
    for line in wrap(text, width) {
        rows.push((line, Tone::Muted, None));
    }
}
fn choices(
    rows: &mut Vec<ContentRow>,
    title: &str,
    key: &str,
    values: &[(&str, &str)],
    current: &str,
    width: usize,
) {
    let title_width = UnicodeWidthStr::width(title);
    let start = if title.is_empty() {
        2
    } else {
        title_width.max(18) + 2
    };
    let option_width: usize = values
        .iter()
        .map(|(_, label)| UnicodeWidthStr::width(*label) + 4)
        .sum();
    let mut prefix = if start + option_width <= width {
        format!("{title}{}", " ".repeat(start - title_width))
    } else {
        rows.push((title.into(), Tone::Normal, None));
        "  ".into()
    };
    let mut used = UnicodeWidthStr::width(prefix.as_str());
    let mut row = Vec::new();
    for (value, label) in values {
        let length = UnicodeWidthStr::width(*label) + 4;
        if used + length > width && !row.is_empty() {
            rows.push((
                prefix,
                Tone::Normal,
                Some(Action::SettingChoices(std::mem::take(&mut row))),
            ));
            prefix = "  ".into();
            used = 2;
        }
        row.push((
            format!("set:{key}:{value}"),
            (*label).into(),
            *value == current,
        ));
        used += length;
    }
    rows.push((prefix, Tone::Normal, Some(Action::SettingChoices(row))));
}

pub(super) fn paint_choices(canvas: &mut Canvas, title: &str, choices: &[(String, String, bool)]) {
    let mut x = canvas.text(0, 0, title, Tone::Normal);
    for (key, label, current) in choices {
        let end = canvas.text(
            x,
            0,
            &format!("{} {label}", if *current { "●" } else { "○" }),
            if *current { Tone::Accent } else { Tone::Normal },
        );
        if end > x {
            canvas.hits.push(crate::engine::Hit {
                x,
                y: 0,
                width: end - x,
                height: 1,
                action: Action::Setting(key.clone()),
            });
        }
        x = end.saturating_add(2);
    }
}

impl App {
    pub(super) fn tool_is_expanded(&self, id: &str) -> bool {
        self.tool_overrides.get(id).copied().unwrap_or_else(|| {
            match self.preferences.tool_details {
                crate::preferences::ToolDetails::Expanded => true,
                crate::preferences::ToolDetails::Collapsed => false,
                crate::preferences::ToolDetails::Auto => self
                    .entries
                    .iter()
                    .chain(self.agent_entries.iter())
                    .find(|entry| entry.id == id)
                    .is_some_and(|entry| entry.expanded),
            }
        })
    }
    pub(super) fn settings_rows(&self, width: usize) -> Vec<ContentRow> {
        let mut rows = Vec::new();
        let english = self.preferences.language == Language::English;
        let l = |ko, en| if english { en } else { ko };
        let data = serde_json::to_value(&self.preferences).unwrap();
        let option =
            |rows: &mut Vec<ContentRow>, title: &str, key: &str, values: &[(&str, &str)]| {
                let value = data[key]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| data[key].to_string());
                choices(rows, title, key, values, &value, width);
            };
        let toggle = [("true", l("표시", "Show")), ("false", l("숨김", "Hide"))];
        let enabled = [("true", l("사용", "On")), ("false", l("안 함", "Off"))];
        if self.settings_page != Page::Main {
            link(&mut rows, l("← 설정", "← Settings").into(), "page:main");
        }
        match self.settings_page {
            Page::Main => {
                rows.push((
                    l("권한 및 승인 정책 →", "Permissions and approvals →").into(),
                    Tone::Accent,
                    Some(Action::Command("/permissions".into())),
                ));
                section(&mut rows, l("표시", "Display"));
                link(
                    &mut rows,
                    format!(
                        "{}  ● {}  ›",
                        l("악센트 색상", "Accent color"),
                        self.preferences.accent.label(self.preferences.language)
                    ),
                    "page:accent",
                );
                option(
                    &mut rows,
                    l("인터페이스 밀도", "Interface density"),
                    "density",
                    &[
                        ("normal", l("기본", "Normal")),
                        ("compact", l("조밀", "Compact")),
                    ],
                );
                choices(
                    &mut rows,
                    l("대화 옆 현황", "Overview panel"),
                    "activity_panel",
                    &toggle,
                    if self.preferences.activity_panel {
                        "true"
                    } else {
                        "false"
                    },
                    width,
                );
                option(
                    &mut rows,
                    l("도구 실행 상세", "Tool details"),
                    "tool_details",
                    &[
                        ("auto", l("자동", "Auto")),
                        ("expanded", l("항상 펼침", "Expanded")),
                        ("collapsed", l("접기", "Collapsed")),
                    ],
                );
                option(
                    &mut rows,
                    l("진행 상태 문구", "Progress labels"),
                    "progress_labels",
                    &[
                        ("stages", l("단계별", "By stage")),
                        ("minimal", l("간단히", "Minimal")),
                    ],
                );
                option(
                    &mut rows,
                    l("토큰 사용량", "Token usage"),
                    "show_tokens",
                    &toggle,
                );
                option(
                    &mut rows,
                    l("타임스탬프", "Timestamps"),
                    "timestamps",
                    &toggle,
                );
                section(&mut rows, l("언어", "Language"));
                option(
                    &mut rows,
                    "",
                    "language",
                    &[("korean", "한국어"), ("english", "English")],
                );
                section(&mut rows, l("입력", "Input"));
                option(
                    &mut rows,
                    l("Enter 동작", "Enter action"),
                    "enter_mode",
                    &[
                        ("queue", l("대기열 추가", "Queue")),
                        ("immediate", l("즉시 전송", "Immediate")),
                    ],
                );
                hint(
                    &mut rows,
                    l(
                        "즉시 전송: 현재 응답 중단 후 우선 전송",
                        "Immediate: interrupt, then send first",
                    ),
                    width,
                );
                hint(
                    &mut rows,
                    &format!("{}  Shift+Enter / Alt+Enter", l("줄바꿈", "Newline")),
                    width,
                );
                option(
                    &mut rows,
                    l("붙여넣기 확인", "Confirm pasted input"),
                    "paste_confirm",
                    &[
                        ("multiline", l("여러 줄만", "Multiline")),
                        ("always", l("항상", "Always")),
                        ("never", l("안 함", "Never")),
                    ],
                );
                option(
                    &mut rows,
                    l("전송 후 입력 기록", "Sent input history"),
                    "input_history",
                    &[("keep", l("유지", "Keep")), ("clear", l("비움", "Clear"))],
                );
                section(&mut rows, l("알림", "Notifications"));
                option(
                    &mut rows,
                    l("작업 완료", "Task completed"),
                    "notify_completed",
                    &toggle,
                );
                option(
                    &mut rows,
                    l("승인 요청", "Approval request"),
                    "notify_approval",
                    &toggle,
                );
                option(&mut rows, l("오류", "Error"), "notify_errors", &toggle);
                option(
                    &mut rows,
                    l("터미널 벨", "Terminal bell"),
                    "terminal_bell",
                    &enabled,
                );
                hint(
                    &mut rows,
                    l(
                        "알림을 꺼도 승인·오류 상태는 유지됩니다",
                        "Status and required approvals remain visible",
                    ),
                    width,
                );
                section(&mut rows, l("시작", "Startup"));
                option(
                    &mut rows,
                    l("시작 화면", "Startup screen"),
                    "startup_screen",
                    &[
                        ("chat", l("대화", "Chat")),
                        ("last", l("마지막 화면", "Last view")),
                    ],
                );
                option(
                    &mut rows,
                    l("이전 세션 복원", "Restore previous session"),
                    "restore_session",
                    &enabled,
                );
                option(
                    &mut rows,
                    l("작업 패널", "Task panel on startup"),
                    "panel_open",
                    &[("true", l("펼침", "Show")), ("false", l("접힘", "Hide"))],
                );
                option(
                    &mut rows,
                    l("마지막 작업 폴더", "Reopen last workspace"),
                    "reopen_workspace",
                    &[
                        ("true", l("다시 열기", "Reopen")),
                        ("false", l("안 함", "Off")),
                    ],
                );
                hint(
                    &mut rows,
                    l(
                        "시작 설정은 다음 실행부터 적용 · --cwd 우선",
                        "Startup settings apply next launch · --cwd wins",
                    ),
                    width,
                );
                section(&mut rows, l("에이전트 실행", "Agent execution"));
                option(
                    &mut rows,
                    l("모델 응답 제한", "Model response timeout"),
                    "core_call_seconds",
                    &[("45", "45s"), ("90", "90s"), ("180", "180s")],
                );
                option(
                    &mut rows,
                    l("연결 시도 한도", "Connection attempt limit"),
                    "core_attempts",
                    &[("1", "1"), ("2", "2"), ("3", "3"), ("5", "5")],
                );
                hint(
                    &mut rows,
                    l(
                        "첫 요청 포함 · 연결 오류/시간 초과만 재시도 · 재시도 시 진행 상태 표시",
                        "Includes initial request · retries connection errors/timeouts · retry shown as progress",
                    ),
                    width,
                );

                option(
                    &mut rows,
                    l("모델 요청 한도", "Model request limit"),
                    "core_steps",
                    &[("40", "40"), ("80", "80"), ("160", "160")],
                );
                option(
                    &mut rows,
                    l("작업 시간 한도", "Task time limit"),
                    "core_seconds",
                    &[
                        ("300", l("5분", "5 min")),
                        ("600", l("10분", "10 min")),
                        ("1200", l("20분", "20 min")),
                    ],
                );
                hint(
                    &mut rows,
                    l(
                        "V4/V5에서 다음 작업부터 적용 · CLI --max-steps/--max-seconds가 우선",
                        "Applies to the next V4/V5 task · CLI --max-steps/--max-seconds take priority",
                    ),
                    width,
                );
                section(&mut rows, l("웹 도구", "Web tools"));
                rows.push((
                    l(
                        "web_fetch · 사용 가능 · 필요시 네트워크 승인",
                        "web_fetch · available · asks for network when needed",
                    )
                    .into(),
                    Tone::Normal,
                    None,
                ));
                match web_search_provider(self.workspace_root.as_deref()) {
                    Ok(Some(provider)) => rows.push((
                        format!("web_search · {}: {}", l("설정됨", "configured"), provider),
                        Tone::Success,
                        None,
                    )),
                    Ok(None) => rows.push((
                        l(
                            "web_search · 미설정 · .loom/config.toml",
                            "web_search · not configured · .loom/config.toml",
                        )
                        .into(),
                        Tone::Warning,
                        None,
                    )),
                    Err(_) => rows.push((
                        l(
                            "web_search · 설정 파일 오류 · .loom/config.toml",
                            "web_search · config error · .loom/config.toml",
                        )
                        .into(),
                        Tone::Danger,
                        None,
                    )),
                }
                section(&mut rows, l("고급", "Advanced"));
                link(
                    &mut rows,
                    l("실험 기능 →", "Experimental features →").into(),
                    "page:experimental",
                );
                link(
                    &mut rows,
                    l("로그 및 진단 →", "Logs and diagnostics →").into(),
                    "page:diagnostics",
                );
                link(
                    &mut rows,
                    l("설정 초기화 →", "Reset settings →").into(),
                    "page:reset",
                );
                rows.push((
                    l("이 작업 폴더에 저장됩니다", "Saved for this workspace").into(),
                    Tone::Muted,
                    None,
                ));
            }
            Page::Accent => {
                section(
                    &mut rows,
                    l(
                        "악센트 색상 · 상태 색상도 함께 변경",
                        "Accent color · includes status colors",
                    ),
                );
                for accent in Accent::ALL {
                    rows.push((
                        format!(
                            "{} {}",
                            if self.preferences.accent == accent {
                                "●"
                            } else {
                                "○"
                            },
                            accent.label(self.preferences.language)
                        ),
                        if self.preferences.accent == accent {
                            Tone::Accent
                        } else {
                            Tone::Normal
                        },
                        Some(Action::Setting(format!("accent:{}", accent.key()))),
                    ));
                }
            }
            Page::Experimental => {
                section(&mut rows, l("실험 기능", "Experimental features"));
                option(
                    &mut rows,
                    l("로컬 코드 탐색 /explore", "Local exploration /explore"),
                    "experimental_explore",
                    &enabled,
                );
                option(
                    &mut rows,
                    l("대화·코드 되돌리기", "Conversation/code rewind"),
                    "experimental_rewind",
                    &enabled,
                );
                rows.push((
                    l(
                        "되돌리기: 대화 화면에서 Esc 두 번",
                        "Rewind: press Esc twice in Chat",
                    )
                    .into(),
                    Tone::Muted,
                    None,
                ));
            }
            Page::Diagnostics => {
                section(&mut rows, l("로그 및 진단", "Logs and diagnostics"));
                for text in [
                    format!("Loom {}", env!("CARGO_PKG_VERSION")),
                    format!("Session: {}", self.thread.as_deref().unwrap_or("—")),
                    format!("Model: {}", self.model),
                    format!("Status: {}", self.status),
                    format!(
                        "Queue: {} · Approvals: {} · Tasks: {}",
                        self.queued.len(),
                        self.approvals.len(),
                        self.tasks.len()
                    ),
                    format!("Sandbox: {}", self.sandbox_mode.as_deref().unwrap_or("—")),
                    format!(
                        "{}: .custom-tui/runtime.log",
                        l("런타임 로그", "Runtime log")
                    ),
                ] {
                    for line in wrap(&text, width) {
                        rows.push((line, Tone::Muted, None));
                    }
                }
                link(
                    &mut rows,
                    l("진단 파일 저장 →", "Save diagnostics →").into(),
                    "diagnostics:export",
                );
                section(
                    &mut rows,
                    l("최근 알림과 오류", "Recent notices and errors"),
                );
                if self.notice_log.is_empty() {
                    rows.push((l("기록 없음", "No entries").into(), Tone::Muted, None));
                }
                for (error, message) in self.notice_log.iter().rev() {
                    for line in wrap(message, width) {
                        rows.push((line, if *error { Tone::Danger } else { Tone::Muted }, None));
                    }
                }
            }
            Page::Reset => {
                section(
                    &mut rows,
                    l(
                        "이 작업 폴더의 UI 설정을 초기화할까요?",
                        "Reset UI settings for this workspace?",
                    ),
                );
                rows.push((
                    l(
                        "저장된 대화·파일·권한은 유지됩니다",
                        "Conversations, files and permissions are preserved",
                    )
                    .into(),
                    Tone::Muted,
                    None,
                ));
                link(&mut rows, l("초기화", "Reset").into(), "reset:confirm");
                link(&mut rows, l("취소", "Cancel").into(), "page:main");
            }
        }
        rows
    }

    pub(super) fn setting_action(&mut self, setting: &str) -> Option<Intent> {
        if let Some(page) = setting.strip_prefix("page:") {
            self.settings_page = match page {
                "main" => Page::Main,
                "accent" => Page::Accent,
                "experimental" => Page::Experimental,
                "diagnostics" => Page::Diagnostics,
                "reset" => Page::Reset,
                _ => return None,
            };
            self.scroll = 0;
            self.follow = false;
            self.focus = None;
            self.composer_active = false;
            return None;
        }
        match setting {
            "reset:confirm" => {
                self.preferences = Preferences::default();
                self.panel_open = self.preferences.activity_panel;
                self.tool_overrides.clear();
                self.settings_page = Page::Main;
                self.input_history.clear();
                self.scroll = 0;
                self.focus = None;
            }
            "diagnostics:export" => return Some(Intent::ExportDiagnostics),
            "paste:confirm" => {
                self.paste_confirmed = true;
                self.paste_pending = false;
                self.focus = None;
                return self.activate(Action::Send);
            }
            "paste:cancel" => {
                self.paste_pending = false;
                self.focus = None;
                return None;
            }
            "korean" => self.preferences.language = Language::Korean,
            "english" => self.preferences.language = Language::English,
            "panel" => {
                self.preferences.panel_open = !self.preferences.panel_open;
                self.panel_open = self.preferences.panel_open;
                self.preferences.activity_panel = self.panel_open;
            }
            _ if setting.starts_with("accent:") => {
                self.preferences.accent = Accent::parse(&setting[7..])?;
            }
            _ if setting.starts_with("set:") => {
                let (key, value) = setting[4..].split_once(':')?;
                if key == "activity_panel" {
                    self.panel_open = match value {
                        "true" => true,
                        "false" => false,
                        _ => return None,
                    };
                    self.preferences.activity_panel = self.panel_open;
                    return Some(Intent::SaveSettings);
                }
                if !self.preferences.set(key, value) {
                    return None;
                }
                if key == "input_history" && self.preferences.input_history == InputHistory::Clear {
                    self.input_history.clear();
                    self.history_position = None;
                }
                if key == "notify_errors" && !self.preferences.notify_errors && self.notice_error {
                    self.notice.clear();
                }
                if key == "tool_details" {
                    self.tool_overrides.clear();
                }
            }
            _ => return None,
        }
        self.invalidate_chat_rows();
        Some(Intent::SaveSettings)
    }

    pub fn startup_view(&mut self) {
        if self.preferences.startup_screen == StartupScreen::Last {
            let view = match self.preferences.last_view.as_str() {
                "activity" => View::Agents,
                "model" => View::Models,
                "providers" => View::Providers,
                "routing" => View::Routing,
                "runtime" => View::Runtime,
                "sessions" => View::Sessions,
                "settings" => View::Settings,
                "permissions" => View::Permissions,
                "help" => View::Help,
                _ => View::Chat,
            };
            self.switch_view(view);
        }
    }
    pub fn remember_view(&mut self) {
        self.preferences.last_view = match self.view {
            View::Agents => "activity",
            View::Models => "model",
            View::Providers => "providers",
            View::Routing => "routing",
            View::Runtime => "runtime",
            View::Sessions => "sessions",
            View::Settings => "settings",
            View::Permissions => "permissions",
            View::Help => "help",
            _ => "chat",
        }
        .into();
    }
    pub fn diagnostics(&self) -> serde_json::Value {
        serde_json::json!({"version": env!("CARGO_PKG_VERSION"), "session": self.thread, "model": self.model, "status": self.status, "queue": self.queued.len(), "approvals": self.approvals.len(), "preferences": self.preferences, "notices": self.notice_log})
    }
    pub(super) fn log_notice(&mut self, error: bool, message: String) {
        if self.notice_log.len() == 100 {
            self.notice_log.pop_front();
        }
        self.notice_log.push_back((error, message));
    }
    pub(super) fn event_notice(&mut self, enabled: bool, message: &str) {
        if enabled {
            self.notify(message);
            if self.preferences.terminal_bell {
                self.bell_pending = true;
            }
        } else {
            self.log_notice(false, message.into());
        }
    }
}

pub(super) fn timestamp(seconds: u64) -> String {
    // Received-at times for live entries; restored history without a timestamp is —.
    format!(
        "{:02}:{:02}:{:02} UTC",
        (seconds / 3600) % 24,
        (seconds / 60) % 60,
        seconds % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AgentEvent, AgentUpdate};

    fn ready() -> App {
        let mut app = App::new(false);
        app.thread = Some("main".into());
        app
    }
    fn set(app: &mut App, key: &str, value: &str) {
        assert_eq!(
            app.activate(Action::Setting(format!("set:{key}:{value}"))),
            Some(Intent::SaveSettings)
        );
    }
    fn text(app: &mut App) -> String {
        let canvas = app.render(80, 60);
        (0..canvas.height)
            .map(|y| canvas.plain_line(y))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn display_preferences_change_real_transcript_and_can_be_overridden() {
        let mut app = ready();
        app.apply_event(AgentEvent::global(AgentUpdate::EntryUpdated(Entry {
            id: "tool".into(),
            kind: Kind::Tool,
            title: "cargo test".into(),
            body: "unique tool detail".into(),
            status: "completed".into(),
            expanded: false,
        })));
        app.upsert_entry(Entry {
            id: "reply".into(),
            kind: Kind::Assistant,
            title: "".into(),
            body: "answer".into(),
            status: "completed".into(),
            expanded: false,
        });
        assert!(!text(&mut app).contains("unique tool detail"));
        set(&mut app, "tool_details", "expanded");
        assert!(text(&mut app).contains("unique tool detail"));
        app.activate(Action::Toggle("tool".into()));
        assert!(!text(&mut app).contains("unique tool detail"));
        set(&mut app, "tool_details", "collapsed");
        app.activate(Action::Toggle("tool".into()));
        assert!(text(&mut app).contains("unique tool detail"));
        set(&mut app, "timestamps", "true");
        assert!(text(&mut app).contains("UTC"));
        let normal = app.content_rows_for(&View::Chat, 76).len();
        set(&mut app, "density", "compact");
        assert!(app.content_rows_for(&View::Chat, 76).len() < normal);
        app.tokens.total = Some(1234);
        assert!(text(&mut app).contains("tokens 1.2k"));
        set(&mut app, "show_tokens", "false");
        assert!(!text(&mut app).contains("tokens"));
    }

    #[test]
    fn paste_policies_cancel_preserves_input_and_confirm_sends_exact_text() {
        for (mode, pasted, confirms) in [
            ("multiline", "one\ntwo", true),
            ("multiline", "one", false),
            ("always", "one", true),
            ("never", "one\ntwo", false),
        ] {
            let mut app = ready();
            set(&mut app, "paste_confirm", mode);
            app.paste(pasted);
            if confirms {
                assert_eq!(app.activate(Action::Send), None);
                assert!(text(&mut app).contains("붙여넣은 내용을 전송할까요"));
                let canvas = app.render(80, 24);
                app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &canvas);
                assert!(!app.paste_pending && app.editor.text == pasted);
                app.activate(Action::Send);
                assert_eq!(
                    app.activate(Action::Setting("paste:confirm".into())),
                    Some(Intent::Submit(pasted.into()))
                );
            } else {
                assert_eq!(
                    app.activate(Action::Send),
                    Some(Intent::Submit(pasted.into()))
                );
            }
        }
    }

    #[test]
    fn immediate_send_waits_for_interrupt_and_preserves_other_queued_input() {
        let mut app = ready();
        set(&mut app, "enter_mode", "immediate");
        app.busy = true;
        app.turn = Some("running".into());
        app.editor.insert("urgent");
        assert_eq!(app.activate(Action::Send), Some(Intent::Interrupt));
        assert_eq!(app.next_queued(), None);
        app.apply_event(AgentEvent::scoped(
            "main",
            AgentUpdate::TurnCompleted {
                id: Some("running".into()),
                status: "interrupted".into(),
                error: None,
            },
        ));
        assert_eq!(app.next_queued(), Some(Intent::Submit("urgent".into())));
        let mut app = ready();
        set(&mut app, "enter_mode", "immediate");
        app.busy = true;
        app.submitted = Some("first".into());
        app.editor.insert("urgent before turn id");
        assert_eq!(app.activate(Action::Send), None);
        app.apply_event(AgentEvent::global(AgentUpdate::TurnSubmitted {
            id: Some("running".into()),
            status: "inProgress".into(),
        }));
        assert_eq!(app.next_queued(), Some(Intent::Interrupt));
    }

    #[test]
    fn history_notifications_bell_and_experimental_switches_have_effects() {
        let mut app = ready();
        set(&mut app, "input_history", "clear");
        app.editor.insert("private draft");
        app.activate(Action::Send);
        assert!(app.input_history.is_empty());
        app.submitted = None;
        set(&mut app, "notify_errors", "false");
        app.error("observed error");
        assert!(!app.notice.contains("observed error"));
        app.activate(Action::Setting("page:diagnostics".into()));
        app.switch_view(View::Settings);
        assert!(text(&mut app).contains("observed error"));
        set(&mut app, "notify_completed", "false");
        app.turn = Some("silent".into());
        app.apply_event(AgentEvent::global(AgentUpdate::TurnCompleted {
            id: Some("silent".into()),
            status: "completed".into(),
            error: None,
        }));
        assert!(!app.bell_pending);
        set(&mut app, "notify_completed", "true");
        set(&mut app, "terminal_bell", "true");
        app.turn = Some("done".into());
        app.apply_event(AgentEvent::global(AgentUpdate::TurnCompleted {
            id: Some("done".into()),
            status: "completed".into(),
            error: None,
        }));
        assert!(app.bell_pending && app.notice.contains("완료"));
        set(&mut app, "experimental_explore", "false");
        assert_eq!(app.activate(Action::Command("/explore test".into())), None);
        set(&mut app, "experimental_rewind", "false");
        app.switch_view(View::Chat);
        let canvas = app.render(80, 24);
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &canvas);
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &canvas);
        assert_eq!(app.view, View::Chat);
    }

    #[test]
    fn progress_label_setting_is_available_and_changes_the_working_header() {
        let mut app = ready();
        app.switch_view(View::Settings);
        assert!(text(&mut app).contains("진행 상태 문구"));
        set(&mut app, "progress_labels", "minimal");
        assert_eq!(
            app.preferences.progress_labels,
            crate::preferences::ProgressLabels::Minimal
        );
        app.busy = true;
        assert_eq!(app.working_phrase(), "작업 중");
        set(&mut app, "progress_labels", "stages");
        assert_eq!(app.working_phrase(), "실타래를 살피는 중");
    }

    #[test]
    fn settings_keyboard_reaches_offscreen_choices_and_reset_requires_confirmation() {
        for width in [40, 80, 140] {
            let mut app = ready();
            app.activate(Action::Command("/settings".into()));
            app.composer_active = true;
            app.setting_action("page:diagnostics");
            app.setting_action("page:main");
            assert!(!app.composer_active);
            for _ in 0..70 {
                let canvas = app.render(width, 24);
                app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &canvas);
                if app.focus == Some(Action::Setting("page:reset".into())) {
                    break;
                }
            }
            let canvas = app.render(width, 24);
            assert_eq!(app.focus, Some(Action::Setting("page:reset".into())));
            assert!(
                canvas
                    .hits
                    .iter()
                    .any(|hit| Some(&hit.action) == app.focus.as_ref())
            );
            set(&mut app, "accent", "hacker");
            set(&mut app, "activity_panel", "true");
            app.tool_overrides.insert("tool".into(), true);
            app.activate(Action::Setting("page:reset".into()));
            assert_eq!(app.preferences.accent, Accent::Hacker);
            app.activate(Action::Setting("reset:confirm".into()));
            assert_eq!(app.preferences.accent, Accent::Cyan);
            assert!(!app.panel_open && app.tool_overrides.is_empty());
            assert!(app.thread.is_some());
        }
    }
    #[test]
    fn startup_restores_last_view_and_approval_notice_is_optional() {
        let mut app = App::new(false);
        app.preferences.startup_screen = crate::preferences::StartupScreen::Last;
        app.preferences.last_view = "settings".into();
        app.apply_event(AgentEvent::global(AgentUpdate::SessionLoaded {
            snapshot: crate::agent::SessionSnapshot {
                id: "main".into(),
                title: None,
                status: Some("idle".into()),
                turns: None,
            },
            switching: true,
            model: None,
            permissions: Default::default(),
        }));
        assert_eq!(app.view, View::Settings);
        set(&mut app, "notify_approval", "false");
        app.notice.clear();
        app.apply_event(AgentEvent::global(AgentUpdate::ApprovalRequested(
            Approval {
                id: "request".into(),
                title: "Approve".into(),
                summary: "Required".into(),
                detail: "".into(),
                choices: vec![Choice {
                    label: "Allow".into(),
                    result: "yes".into(),
                }],
                answering: false,
                expanded: false,
            },
        )));
        assert!(app.notice.is_empty());
        assert!(app.approval_visible());
        assert!(text(&mut app).contains("승인 필요"));
    }
}
