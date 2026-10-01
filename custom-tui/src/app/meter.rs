//! Provider-reported counters and explicit resource limits; no inferred spend.
use super::*;
use crate::agent::{LimitKind, QuotaSnapshot, TokenUsage, WorkPhase};

// Phrases rotate within the observed activity, never inventing task progress.
const THINK: &[(&str, &str)] = &[
    ("실타래를 살피는 중", "Following the threads"),
    ("설계를 엮는 중", "Weaving the plan"),
    ("코드의 결을 읽는 중", "Reading the weave"),
    ("문제의 매듭을 찾는 중", "Tracing the knot"),
    ("해결의 실마리를 잇는 중", "Connecting the clues"),
    ("다음 수를 고르는 중", "Choosing the next step"),
    ("생각의 가닥을 정리하는 중", "Sorting the threads"),
    ("요구사항을 맞춰보는 중", "Checking the requirements"),
];
const WRITE: &[(&str, &str)] = &[
    ("코드를 짜는 중", "Weaving the code"),
    ("답변을 엮는 중", "Weaving the response"),
    ("한 줄씩 직조하는 중", "Weaving line by line"),
    ("생각을 문장으로 잇는 중", "Putting thoughts into words"),
    ("직조기에 한 줄 더 올리는 중", "Adding another thread"),
    ("풀이를 펼치는 중", "Unfolding the solution"),
];
const EXPLORE: &[(&str, &str)] = &[
    ("코드 사이의 실마리를 찾는 중", "Searching for code clues"),
    ("파일의 가닥을 따라가는 중", "Following the file threads"),
    ("코드 지도를 펼치는 중", "Mapping the code"),
    ("관련 코드를 모으는 중", "Gathering related code"),
    ("흩어진 단서를 잇는 중", "Connecting scattered clues"),
];
const TEST: &[(&str, &str)] = &[
    ("짜인 코드의 매듭을 검사하는 중", "Testing the code's knots"),
    ("테스트로 결을 확인하는 중", "Checking the weave with tests"),
    ("느슨한 실이 있는지 살피는 중", "Checking for loose threads"),
    ("테스트 결과를 기다리는 중", "Waiting for test results"),
];
const PATCH: &[(&str, &str)] = &[
    ("코드의 가닥을 다듬는 중", "Refining the code threads"),
    ("변경 내용을 엮는 중", "Weaving the changes"),
    (
        "직조기에 수정안을 올리는 중",
        "Putting the patch on the loom",
    ),
    ("파일에 새 결을 넣는 중", "Adding a new weave to the file"),
];
const TOOL: &[(&str, &str)] = &[
    ("도구를 돌리는 중", "Running the tools"),
    ("작업의 가닥을 확인하는 중", "Checking the work threads"),
    ("실행 결과를 기다리는 중", "Waiting for tool results"),
    ("직조기의 움직임을 살피는 중", "Watching the loom work"),
    (
        "다음 단계의 재료를 모으는 중",
        "Gathering the next step's inputs",
    ),
];

fn number(value: Option<u64>) -> String {
    match value {
        Some(value) if value >= 1000 => format!("{:.1}k", value as f64 / 1000.0),
        Some(value) => value.to_string(),
        None => "—".into(),
    }
}
impl App {
    pub(super) fn working_phrase(&self) -> &'static str {
        let local_exploration = !self.active_tasks.is_empty()
            && !self.busy
            && self.active_tasks.iter().all(|id| {
                self.tasks.iter().any(|task| {
                    &task.id == id && task.worker == Some(crate::agent::WorkerKind::Local)
                })
            });
        let tool = self.entries.iter().rev().find(|entry| {
            matches!(entry.kind, Kind::Tool | Kind::Change) && entry.status == "inProgress"
        });
        let phrases: &[(&str, &str)] = if local_exploration {
            EXPLORE
        } else if self.work_phase == WorkPhase::Tool {
            match tool {
                Some(entry) if entry.kind == Kind::Change => PATCH,
                Some(entry) => {
                    let words: Vec<_> = entry.title.split_whitespace().collect();
                    if words
                        .iter()
                        .any(|word| matches!(*word, "pytest" | "nextest"))
                        || words.windows(2).any(|pair| {
                            matches!(
                                (pair[0], pair[1]),
                                ("cargo" | "npm" | "pnpm" | "yarn" | "go", "test")
                                    | ("-m", "unittest")
                            )
                        })
                    {
                        TEST
                    } else if words
                        .iter()
                        .any(|word| matches!(*word, "rg" | "grep" | "fd" | "find"))
                    {
                        EXPLORE
                    } else {
                        TOOL
                    }
                }
                None => TOOL,
            }
        } else if self.work_phase == WorkPhase::Writing {
            WRITE
        } else {
            THINK
        };
        let (korean, english) = phrases[self.work_phrase_index % phrases.len()];
        if self.preferences.language == Language::English {
            english
        } else {
            korean
        }
    }
    pub(super) fn set_tokens(&mut self, usage: TokenUsage) {
        self.usage = format!(
            "tokens {} · in {} · cached {} · out {}",
            number(usage.total),
            number(usage.input),
            number(usage.cached_input),
            number(usage.output)
        );
        self.tokens = usage;
    }
    pub(super) fn update_quota(&mut self, snapshot: QuotaSnapshot) {
        if let Some(old) = self.quotas.iter_mut().find(|q| q.id == snapshot.id) {
            *old = snapshot;
        } else {
            self.quotas.push(snapshot);
        }
    }
    pub(super) fn execution_limited(&mut self, kind: LimitKind, reason: String) {
        self.execution_limit = Some(kind);
        self.queue_paused = true;
        self.force_after_interrupt = None;
        let title = match kind {
            LimitKind::UsageExhausted => "할당량 소진",
            LimitKind::RateLimited => "호출 제한",
        };
        self.status = title.into();
        self.error(format!("{title} · 원격 실행이 제한되어 대기 중인 지시를 보존했습니다. {reason} /usage refresh로 확인하세요."));
        self.notice_deadline = None;
    }
    pub(super) fn clear_execution_limit(&mut self) {
        if self.execution_limit.take().is_some() {
            // A fresh report permits manual recovery; never re-submit automatically.
            self.queue_paused = true;
            self.status = "준비됨".into();
            self.notify("사용 가능한 할당량이 확인됐습니다. /queue resume로 직접 재개하세요.");
        }
    }
    pub(super) fn meter_rows(&self, width: usize) -> Vec<(String, Tone, Option<Action>)> {
        let mut rows = vec![("토큰·할당량".into(), Tone::Accent, None)];
        let metrics = [
            ("Total tokens", self.tokens.total),
            ("Input", self.tokens.input),
            ("Cached input", self.tokens.cached_input),
            ("Output", self.tokens.output),
            ("Reasoning output", self.tokens.reasoning_output),
            ("Last response", self.tokens.last),
            ("Context window", self.tokens.context_window),
        ];
        for (label, value) in metrics {
            rows.push((
                format!(
                    "{label}: {}",
                    value.map(|n| n.to_string()).unwrap_or("미계측".into())
                ),
                Tone::Muted,
                None,
            ));
        }
        rows.push((
            "API cost: 미계측 · 토큰 수와 요금은 별개".into(),
            Tone::Muted,
            None,
        ));
        if self.quotas.is_empty() {
            rows.push(("계정 할당량: 정보 없음".into(), Tone::Muted, None));
        }
        for quota in &self.quotas {
            for line in wrap(&format!("Quota: {}", quota.label), width) {
                rows.push((line, Tone::Normal, None));
            }
            for (label, window) in [("Primary", &quota.primary), ("Secondary", &quota.secondary)] {
                if let Some(window) = window {
                    for line in wrap(
                        &format!(
                            "{label}: {}% 사용 · window {}m · reset {}",
                            window.used_percent,
                            window
                                .duration_minutes
                                .map(|n| n.to_string())
                                .unwrap_or("—".into()),
                            window
                                .resets_at
                                .map(reset_label)
                                .unwrap_or("정보 없음".into())
                        ),
                        width,
                    ) {
                        rows.push((
                            line,
                            if window.used_percent >= 100 {
                                Tone::Warning
                            } else {
                                Tone::Muted
                            },
                            None,
                        ));
                    }
                }
            }
            if let Some(kind) = quota.reached {
                rows.push((format!("Runtime limit: {kind:?}"), Tone::Warning, None));
            }
        }
        rows.push((
            "할당량 새로고침".into(),
            Tone::Accent,
            Some(Action::Command("/usage refresh".into())),
        ));
        rows
    }
    pub(super) fn paint_meter(&self, canvas: &mut Canvas, y: u16) {
        let width = usize::from(canvas.width.saturating_sub(4));
        let text = if let Some(limit) = self.execution_limit {
            format!(
                "tokens {} · {} · /usage",
                number(self.tokens.total),
                match limit {
                    LimitKind::UsageExhausted => "할당량 소진",
                    LimitKind::RateLimited => "호출 제한",
                }
            )
        } else if width >= 65 && !self.usage.is_empty() {
            self.usage.clone()
        } else {
            format!("tokens {} · /usage", number(self.tokens.total))
        };
        let mut strip = Canvas::new(canvas.width.saturating_sub(4), 1);
        strip.button(0, 0, &text, Action::Command("/usage".into()), false);
        if self.execution_limit.is_some() {
            strip.highlight(&Action::Command("/usage".into()), Tone::Warning);
        }
        canvas.blit(&strip, 2, y);
    }
}

fn reset_label(timestamp: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if timestamp <= now {
        return format!("갱신시각 경과 · Unix {timestamp}");
    }
    let minutes = (timestamp - now).div_ceil(60);
    let relative = if minutes >= 1440 {
        format!("{}d {}h", minutes / 1440, (minutes % 1440) / 60)
    } else if minutes >= 60 {
        format!("{}h {}m", minutes / 60, minutes % 60)
    } else {
        format!("{minutes}m")
    };
    format!("{relative} 후 · Unix {timestamp}")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn working_phrases_use_observed_tools_and_local_tasks() {
        let mut app = App::new(false);
        app.work_phase = WorkPhase::Tool;
        app.entries.push(Entry {
            id: "test".into(),
            kind: Kind::Tool,
            title: "cargo test --locked".into(),
            body: String::new(),
            status: "inProgress".into(),
            expanded: false,
        });
        assert_eq!(app.working_phrase(), TEST[0].0);
        app.entries[0].title = "rg pattern src".into();
        assert_eq!(app.working_phrase(), EXPLORE[0].0);
        app.entries[0].status = "completed".into();
        assert_eq!(
            app.working_phrase(),
            TOOL[0].0,
            "completed tools do not imply current work"
        );
        app.preferences.language = Language::English;
        assert_eq!(app.working_phrase(), TOOL[0].1);
        app.work_phase = WorkPhase::Thinking;
        let phrases: std::collections::HashSet<_> = (0..THINK.len())
            .map(|index| {
                app.work_phrase_index = index;
                app.working_phrase()
            })
            .collect();
        assert_eq!(phrases.len(), THINK.len());
    }
    #[test]
    fn loom_phrases_rotate_and_change_with_output_without_fabricating_tokens() {
        let mut app = App::new(false);
        app.busy = true;
        app.status = "실행 중".into();
        assert_eq!(app.working_phrase(), "실타래를 살피는 중");
        let phrase = app.working_phrase();
        app.tick(Instant::now() + Duration::from_millis(2300));
        assert_ne!(phrase, app.working_phrase());
        app.apply_event(crate::agent::AgentEvent::global(
            crate::agent::AgentUpdate::EntryDelta {
                id: "output".into(),
                kind: Kind::Assistant,
                text: "text".into(),
            },
        ));
        assert_eq!(app.work_phase, WorkPhase::Writing);
        assert!(app.tokens.total.is_none());
        let canvas = app.render(80, 24);
        let screen = (0..canvas.height)
            .map(|y| canvas.plain_line(y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!screen.contains("/ 명령"));
        assert!(!screen.contains("도움말"));
        let line = canvas.plain_line(1);
        assert!(line.find("모델").unwrap() < line.find("세션").unwrap());
        assert!(screen.contains("tokens —"));
    }
}
