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
        Some(value) if value >= 1_000_000 => format!("{:.1}M", value as f64 / 1_000_000.0),
        Some(value) if value >= 1000 => format!("{:.1}k", value as f64 / 1000.0),
        Some(value) => value.to_string(),
        None => "—".into(),
    }
}
impl App {
    /// Short, measured indicators for panels. Precise counters stay in meter_rows.
    pub(super) fn meter_summary_rows(&self, width: usize) -> Vec<(String, Tone, Option<Action>)> {
        let mut rows = Vec::new();
        let gauge = |label: &str, percent: u32| {
            let filled = (percent.min(100) / 10) as usize;
            let bar = format!("{}{}", "━".repeat(filled), "─".repeat(10 - filled));
            if width >= 28 {
                format!("  {label} {percent}%  {bar}")
            } else {
                format!("  {label} {percent}%")
            }
        };
        if let (Some(last), Some(window)) = (self.tokens.last, self.tokens.context_window) {
            if window > 0 {
                let percent = ((u128::from(last) * 100) / u128::from(window)).min(100) as u32;
                rows.push((
                    gauge("Context", percent),
                    if percent >= 90 {
                        Tone::Warning
                    } else {
                        Tone::Accent
                    },
                    None,
                ));
            }
        } else {
            rows.push(("  Context —".into(), Tone::Muted, None));
        }
        if let Some(window) = self
            .quotas
            .iter()
            .find(|quota| {
                quota
                    .model
                    .as_deref()
                    .is_none_or(|model| model == self.model)
            })
            .and_then(|quota| quota.primary.as_ref().or(quota.secondary.as_ref()))
        {
            let percent = window.used_percent;
            rows.push((
                gauge("Quota", percent),
                if percent >= 90 {
                    Tone::Warning
                } else {
                    Tone::Muted
                },
                None,
            ));
        } else {
            rows.push(("  Quota —".into(), Tone::Muted, None));
        }
        rows.push((
            format!(
                "  Queue {} · Agent {}",
                self.queued.len(),
                self.agents.len()
            ),
            Tone::Muted,
            None,
        ));
        rows.push((
            "  토큰·할당량 상세 ▸".into(),
            Tone::Accent,
            Some(Action::Command("/usage".into())),
        ));
        rows
    }
    /// The status line's phrase: the whole progress summary (it is no longer repeated in
    /// the transcript, which keeps only a spinner), otherwise the working phrase.
    pub(super) fn status_phrase(&self) -> std::borrow::Cow<'_, str> {
        let progress = self.entries.iter().rev().find(|entry| {
            entry.kind == Kind::Assistant
                && entry.title == crate::agent::PROGRESS_TITLE
                && entry.status == "inProgress"
                && !entry.body.trim().is_empty()
        });
        match progress {
            Some(entry) if self.preferences.progress_labels != crate::preferences::ProgressLabels::Minimal => {
                std::borrow::Cow::Owned(
                    entry.body.lines().map(str::trim).filter(|line| !line.is_empty()).collect::<Vec<_>>().join(" · "),
                )
            }
            _ => std::borrow::Cow::Borrowed(self.working_phrase()),
        }
    }

    pub(super) fn working_phrase(&self) -> &str {
        if self.interrupt_requested {
            return match self.preferences.language {
                Language::Korean => "중단됨",
                Language::English => "Interrupted",
            };
        }
        if self.compacting {
            return match self.preferences.language {
                Language::Korean => "컨텍스트 압축 중",
                Language::English => "Compacting context",
            };
        }
        if self.preferences.progress_labels == crate::preferences::ProgressLabels::Minimal {
            return if self.preferences.language == Language::English {
                "Working"
            } else {
                "작업 중"
            };
        }
        // Only the progress entry's body is a status; text the model writes is a plain
        // reply entry (no progress title) and never becomes the status line.
        if let Some(entry) = self.entries.iter().rev().find(|entry| {
            entry.kind == Kind::Assistant
                && entry.title == crate::agent::PROGRESS_TITLE
                && entry.status == "inProgress"
                && !entry.body.is_empty()
        }) {
            return entry.body.lines().last().unwrap_or("진행 중");
        }
        match (self.work_phase, self.preferences.language) {
            (WorkPhase::Command, Language::Korean) => return "명령 실행 중",
            (WorkPhase::Command, Language::English) => return "Command running",
            (WorkPhase::Shell, Language::Korean) => return "셸 실행 중",
            (WorkPhase::Shell, Language::English) => return "Shell running",
            _ => {}
        }
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
        let (korean, english) = phrases[0];
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
        self.execution_limit_model = Some(self.model.clone());
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
    /// The limit belonged to the previous model. Sending is allowed again; queued
    /// messages still wait for an explicit resume because they were written for it.
    pub(super) fn lift_limit_for_model_change(&mut self) {
        let previous = self.execution_limit_model.take().unwrap_or_default();
        self.execution_limit = None;
        self.status = "준비됨".into();
        if self.queued.is_empty() {
            self.queue_paused = false;
            self.notify(format!(
                "{previous}의 사용 제한은 유지됩니다. 선택한 모델로 전송할 수 있습니다."
            ));
        } else {
            self.notify(format!(
                "{previous}의 사용 제한은 유지됩니다. 대기 중인 {}개는 /queue resume으로 선택한 모델에 전송하세요.",
                self.queued.len()
            ));
        }
    }
    pub(super) fn clear_execution_limit(&mut self) {
        if self.execution_limit.take().is_some() {
            self.execution_limit_model = None;
            // A fresh report permits manual recovery; never re-submit automatically.
            self.queue_paused = true;
            self.status = "준비됨".into();
            self.notify("사용 가능한 할당량이 확인됐습니다. /queue resume로 직접 재개하세요.");
        }
    }
    pub(crate) fn meter_rows(&self, width: usize) -> Vec<(String, Tone, Option<Action>)> {
        let mut rows = Vec::new();
        panel_section(&mut rows, "컨텍스트", width);
        match (self.tokens.last, self.tokens.context_window) {
            (Some(used), Some(window)) if window > 0 => {
                let percent = ((u128::from(used) * 100) / u128::from(window)).min(100) as usize;
                let bar_width = width.saturating_sub(14).clamp(6, 16);
                let filled = percent * bar_width / 100;
                rows.push((
                    format!(
                        "  사용 {percent}%  {}{}",
                        "━".repeat(filled),
                        "─".repeat(bar_width - filled)
                    ),
                    if percent >= 90 {
                        Tone::Warning
                    } else {
                        Tone::Accent
                    },
                    None,
                ));
                for line in wrap(
                    &format!(
                        "  {} / {} · 잔여 {}",
                        number(Some(used)),
                        number(Some(window)),
                        number(Some(window.saturating_sub(used)))
                    ),
                    width,
                ) {
                    rows.push((line, Tone::Muted, None));
                }
            }
            _ => {
                rows.push(("  사용률 미보고".into(), Tone::Muted, None));
                if let Some(window) = self.tokens.context_window {
                    rows.push((
                        format!("  최대 {} · 현재 사용량 미보고", number(Some(window))),
                        Tone::Faint,
                        None,
                    ));
                }
            }
        }

        panel_section(&mut rows, "토큰 · 누적", width);
        if [
            self.tokens.total,
            self.tokens.input,
            self.tokens.cached_input,
            self.tokens.output,
            self.tokens.reasoning_output,
        ]
        .iter()
        .all(Option::is_none)
        {
            rows.push(("아직 집계된 토큰 없음".into(), Tone::Muted, None));
        } else {
            rows.push((
                format!("  누적 {}", number(self.tokens.total)),
                Tone::Strong,
                None,
            ));
            // Cache belongs to input; reasoning belongs to output. Keep them visually
            // subordinate and never add either to the total a second time.
            meter_pair(
                &mut rows,
                width,
                format!("  입력 {}", number(self.tokens.input)),
                format!("출력 {}", number(self.tokens.output)),
                Tone::Normal,
            );
            if self.tokens.cached_input.is_some() || self.tokens.reasoning_output.is_some() {
                meter_pair(
                    &mut rows,
                    width,
                    format!("    ↳ 캐시 {}", number(self.tokens.cached_input)),
                    format!("↳ 추론 {}", number(self.tokens.reasoning_output)),
                    Tone::Muted,
                );
                rows.push(("  ↳ 각각 입력·출력에 포함".into(), Tone::Faint, None));
            }
        }

        panel_section(&mut rows, "런타임 할당량", width);
        if self.quotas.is_empty() {
            rows.push(("  런타임 보고 없음".into(), Tone::Muted, None));
        }
        for quota in &self.quotas {
            for line in wrap(&quota.label, width) {
                rows.push((line, Tone::Strong, None));
            }
            if quota.primary.is_none() && quota.secondary.is_none() {
                rows.push(("  사용량 미보고".into(), Tone::Muted, None));
            }
            for (label, window) in [("기본", &quota.primary), ("추가", &quota.secondary)] {
                if let Some(window) = window {
                    let bar_width = width.saturating_sub(19).clamp(6, 16);
                    let filled =
                        (window.used_percent.min(100) as usize * bar_width / 100).min(bar_width);
                    rows.push((
                        format!(
                            "  {label} {:>3}%  {}{}",
                            window.used_percent,
                            "━".repeat(filled),
                            "─".repeat(bar_width - filled)
                        ),
                        if window.used_percent >= 90 {
                            Tone::Warning
                        } else {
                            Tone::Muted
                        },
                        None,
                    ));
                    let details = [
                        window.duration_minutes.map(|m| format!("{m}분 기준")),
                        window.resets_at.map(reset_label),
                    ]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" · ");
                    if !details.is_empty() {
                        for line in wrap(&format!("    {details}"), width) {
                            rows.push((line, Tone::Faint, None));
                        }
                    }
                }
            }
            if let Some(kind) = quota.reached {
                rows.push((format!("  사용 제한: {kind:?}"), Tone::Warning, None));
            }
        }
        rows.push((
            "할당량 새로고침".into(),
            Tone::Accent,
            Some(Action::Command("/usage refresh".into())),
        ));
        rows.extend(self.model_usage_rows(width, now_secs()));
        rows
    }
    pub(super) fn reload_model_ledger(&mut self) {
        let Some(path) = &self.model_ledger_path else {
            return;
        };
        self.model_ledger = crate::core::model::router::load_ledger(path);
    }
    /// Loom's own router today: the total first, then one bar per model (tokens); a bar
    /// opens that model's details. Then today's free-tier use.
    pub(super) fn model_usage_rows(
        &self,
        width: usize,
        now: u64,
    ) -> Vec<(String, Tone, Option<Action>)> {
        use crate::core::model::{freetier, router};
        const MAX_MODELS: usize = 8;
        let mut rows = Vec::new();
        panel_section(&mut rows, "모델별 사용량 · 오늘 (Loom 라우터)", width);
        let start = freetier::day_start(now);
        let today: Vec<_> = self
            .model_ledger
            .iter()
            .filter(|r| r.at >= start)
            .cloned()
            .collect();
        if today.is_empty() {
            rows.push((
                "오늘 Loom 라우터 호출 없음".into(),
                Tone::Muted,
                None,
            ));
            return rows;
        }
        let mut stats = router::stats(&today);
        let tokens = |s: &router::ModelStats| s.input + s.output;
        stats.sort_by(|a, b| tokens(b).cmp(&tokens(a)).then(b.calls.cmp(&a.calls)));
        let calls: u64 = stats.iter().map(|s| s.calls).sum();
        let failures: u64 = stats.iter().map(|s| s.failures).sum();
        let (input, output) = (
            stats.iter().map(|s| s.input).sum::<u64>(),
            stats.iter().map(|s| s.output).sum::<u64>(),
        );
        let total = input + output;
        let cost = cost_label(
            stats
                .iter()
                .try_fold(0.0, |sum, s| s.usd.map(|usd| sum + usd)),
        );
        for line in wrap(
            &format!(
                "총 {} · {calls}회 · 모델 {}개",
                number(Some(total)),
                stats.len()
            ),
            width,
        ) {
            rows.push((line, Tone::Strong, None));
        }
        let failed = if failures > 0 {
            format!(" · 실패 {failures}")
        } else {
            String::new()
        };
        for line in wrap(
            &format!(
                "in {} · out {} · {cost}{failed}",
                number(Some(input)),
                number(Some(output))
            ),
            width,
        ) {
            rows.push((
                line,
                if failures * 2 > calls {
                    Tone::Warning
                } else {
                    Tone::Muted
                },
                None,
            ));
        }
        rows.push((String::new(), Tone::Normal, None));
        // The bar scales to the largest model. Wide panels: name · bar · value on one line;
        // narrow ones: full name and value, then the bar, so similar names stay distinguishable.
        let largest = stats.first().map(tokens).unwrap_or(1).max(1);
        let one_line = width >= 70;
        let name_width = if one_line {
            (width / 3).clamp(16, 32)
        } else {
            width.saturating_sub(10)
        };
        let value_width = 7;
        let bar_width = if one_line {
            width.saturating_sub(name_width + value_width + 6)
        } else {
            width.saturating_sub(4)
        };
        let bar = |n: u64| {
            let filled = ((n as u128 * bar_width as u128).div_ceil(largest as u128) as usize)
                .clamp(1, bar_width.max(1));
            format!(
                "{}{}",
                "━".repeat(filled),
                "─".repeat(bar_width.saturating_sub(filled))
            )
        };
        for s in stats.iter().take(MAX_MODELS) {
            let key = format!("{}|{}", s.endpoint, s.model);
            let open = self.usage_model.as_deref() == Some(key.as_str());
            let action = Some(Action::Command(format!("/usage model {key}")));
            let marker = if open { "▾" } else { "▸" };
            let value = format!("{:>value_width$}", number(Some(tokens(s))));
            let tone = if open { Tone::Selected } else { Tone::Accent };
            if one_line {
                rows.push((
                    format!(
                        "{marker} {} {} {value}",
                        fit(&s.model, name_width),
                        bar(tokens(s))
                    ),
                    tone,
                    action,
                ));
            } else {
                rows.push((
                    format!(
                        "{marker} {} {}",
                        fit(&s.model, name_width),
                        number(Some(tokens(s)))
                    ),
                    tone,
                    action.clone(),
                ));
                rows.push((format!("  {}", bar(tokens(s))), tone, action));
            }
            if open {
                let share = tokens(s) * 100 / total.max(1);
                let failures = if s.failures > 0 {
                    format!(" · 실패 {}", s.failures)
                } else {
                    String::new()
                };
                let cut = unicode_width::UnicodeWidthStr::width(s.model.as_str()) > name_width;
                for text in [
                    if cut {
                        format!("  {}", s.model)
                    } else {
                        String::new()
                    },
                    format!("  경로 {}", s.endpoint),
                    format!("  호출 {}회{failures} · 점유 {share}%", s.calls),
                    format!(
                        "  in {} · cached {} · out {}",
                        number(Some(s.input)),
                        number(Some(s.cached_input)),
                        number(Some(s.output))
                    ),
                    format!(
                        "  {} · 평균 {:.1}s",
                        cost_label(s.usd),
                        s.mean_latency_ms as f64 / 1000.0
                    ),
                ] {
                    for line in wrap(&text, width)
                        .into_iter()
                        .filter(|l| !l.trim().is_empty())
                    {
                        rows.push((
                            line,
                            if s.failures * 2 > s.calls && text.contains("호출") {
                                Tone::Warning
                            } else {
                                Tone::Muted
                            },
                            None,
                        ));
                    }
                }
            }
        }
        if stats.len() > MAX_MODELS {
            rows.push((
                format!(
                    "외 {}개 모델 · 전체: custom-tui --model-stats",
                    stats.len() - MAX_MODELS
                ),
                Tone::Muted,
                None,
            ));
        } else {
            rows.push(("막대를 누르면 모델 상세".into(), Tone::Faint, None));
        }
        let used = freetier::usage_today(&today, now);
        let allowances = freetier::allowances();
        let limited: Vec<_> = used
            .iter()
            .filter_map(|(name, t)| allowances.get(name).map(|a| (name, t, a)))
            .collect();
        if !limited.is_empty() {
            panel_section(&mut rows, "무료 등급 · 오늘", width);
            for (name, t, a) in limited {
                let part = |n: u64, limit: Option<u64>, unit: &str| {
                    limit.map(|l| {
                        format!(
                            "{unit} {}/{} ({}%)",
                            number(Some(n)),
                            number(Some(l)),
                            n * 100 / l.max(1)
                        )
                    })
                };
                let text = [
                    part(t.requests, a.requests_per_day, "요청"),
                    part(t.tokens, a.tokens_per_day, "토큰"),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · ");
                let exhausted = freetier::exhausted(a, t);
                rows.push((
                    format!("{name} · {text}{}", if exhausted { " · 소진" } else { "" }),
                    if exhausted {
                        Tone::Warning
                    } else {
                        Tone::Muted
                    },
                    None,
                ));
            }
        }
        rows
    }
    pub(super) fn paint_meter(&self, canvas: &mut Canvas, y: u16) {
        if !self.preferences.show_tokens {
            return;
        }
        let english = self.preferences.language == Language::English;
        // Unknown values are left out rather than shown as dashes.
        let tokens = self.tokens.total.map(|v| match v {
            0..1_000 => v.to_string(),
            1_000..10_000 => format!("{:.1}k", v as f64 / 1e3),
            10_000..1_000_000 => format!("{}k", (v as f64 / 1e3).round()),
            1_000_000..10_000_000 => format!("{:.1}M", v as f64 / 1e6),
            _ => format!("{}M", (v as f64 / 1e6).round()),
        });
        let quota = self
            .quotas
            .iter()
            .find(|q| q.model.as_deref() == Some(self.model.as_str()))
            .or_else(|| self.quotas.iter().find(|q| q.model.is_none()));
        let window = quota.and_then(|q| q.primary.as_ref().or(q.secondary.as_ref()));
        let remaining = window.map(|w| format!("{}%", 100u32.saturating_sub(w.used_percent)));
        let left = tokens.map(|tokens| format!("tokens {tokens}")).unwrap_or_default();
        let right = remaining
            .map(|remaining| format!("{} {remaining}", if english { "Limit" } else { "한도" }))
            .unwrap_or_default();
        let available = canvas.width.saturating_sub(4);
        let right_width = unicode_width::UnicodeWidthStr::width(right.as_str()) as u16;
        let left_width = available.saturating_sub(right_width + 2);
        canvas.text(
            2,
            y,
            &super::one_line(&left, usize::from(left_width)),
            Tone::Muted,
        );
        canvas.text(
            canvas.width.saturating_sub(right_width + 2),
            y,
            &right,
            if self.execution_limit.is_some() {
                Tone::Warning
            } else {
                Tone::Muted
            },
        );
        canvas.hits.push(crate::engine::Hit {
            x: 2,
            y,
            width: available,
            height: 1,
            action: Action::Command("/usage".into()),
        });
    }
}

/// Align related counters in two columns where possible; keep each label intact
/// when the activity pane gets narrow.
fn meter_pair(
    rows: &mut Vec<(String, Tone, Option<Action>)>,
    width: usize,
    left: String,
    right: String,
    tone: Tone,
) {
    if width >= 38 {
        let half = (width / 2).min(26);
        rows.push((format!("{}{}", fit(&left, half), right), tone, None));
    } else {
        rows.push((left, tone, None));
        rows.push((format!("  {right}"), tone, None));
    }
}

fn cost_label(usd: Option<f64>) -> String {
    match usd {
        None => "비용 미확인".into(),
        Some(usd) if usd == 0.0 => "$0".into(),
        Some(usd) => format!("${usd:.4}"),
    }
}

/// `text` cut to `width` columns (… when cut) and padded to exactly `width`.
fn fit(text: &str, width: usize) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;
    let mut out = String::new();
    let mut used = 0;
    let full = UnicodeWidthStr::width(text) <= width;
    for g in text.graphemes(true) {
        let w = UnicodeWidthStr::width(g);
        if used + w + usize::from(!full) > width {
            out.push('…');
            used += 1;
            break;
        }
        out.push_str(g);
        used += w;
    }
    out + &" ".repeat(width.saturating_sub(used))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn reset_label(timestamp: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if timestamp <= now {
        return "갱신 시각 경과".into();
    }
    let minutes = (timestamp - now).div_ceil(60);
    let relative = if minutes >= 1440 {
        format!("{}d {}h", minutes / 1440, (minutes % 1440) / 60)
    } else if minutes >= 60 {
        format!("{}h {}m", minutes / 60, minutes % 60)
    } else {
        format!("{minutes}m")
    };
    format!("{relative} 후 초기화")
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
        assert_eq!(app.working_phrase(), THINK[0].1);
    }
    #[test]
    fn loom_phrase_stays_stable_until_stage_changes_without_fabricating_tokens() {
        let mut app = App::new(false);
        app.busy = true;
        app.status = "실행 중".into();
        assert_eq!(app.working_phrase(), "실타래를 살피는 중");
        let phrase = app.working_phrase().to_owned();
        app.tick(Instant::now() + Duration::from_millis(2300));
        assert_eq!(
            phrase,
            app.working_phrase(),
            "time alone must not rotate the label"
        );
        app.apply_event(crate::agent::AgentEvent::global(
            crate::agent::AgentUpdate::EntryDelta {
                id: "output".into(),
                kind: Kind::Assistant,
                text: "text".into(),
            },
        ));
        assert_eq!(app.work_phase, WorkPhase::Writing);
        assert_eq!(app.working_phrase(), WRITE[0].0);
        assert!(app.tokens.total.is_none());
        let canvas = app.render(80, 24);
        let screen = (0..canvas.height)
            .map(|y| canvas.plain_line(y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!screen.contains("/ 명령"));
        assert!(!screen.contains("도움말"));
        let line = canvas.plain_line(0);
        assert!(line.find("세션").unwrap() < line.find("환경").unwrap());
        assert!(!screen.contains("tokens —") && !screen.contains("한도 —"), "unknown values are not shown as dashes");
    }
    #[test]
    fn minimal_progress_labels_hide_stage_changes_but_keep_compaction_visible() {
        let mut app = App::new(false);
        app.preferences.progress_labels = crate::preferences::ProgressLabels::Minimal;
        for stage in [WorkPhase::Thinking, WorkPhase::Tool, WorkPhase::Writing] {
            app.work_phase = stage;
            assert_eq!(app.working_phrase(), "작업 중");
        }
        app.compacting = true;
        assert_eq!(app.working_phrase(), "컨텍스트 압축 중");
    }
}

