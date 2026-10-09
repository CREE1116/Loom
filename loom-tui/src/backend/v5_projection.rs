//! Progressive disclosure of V5 domain events. The Core never chooses screen text.
use crate::agent::{Entry, Kind};
use loom_core::agent::events::{Event, Stage};
use std::collections::{BTreeMap, BTreeSet};

fn friendly_model_issue(reason: &str) -> &'static str {
    match reason.split_once(": ") {
        Some(("Cancelled", _)) => "작업을 중지했습니다.",
        Some(("TimedOut", _)) => "작업 시간이 초과되었습니다.",
        Some(("Transient", detail))
            if ["timed out", "time limit", "timeout"]
                .iter()
                .any(|phrase| detail.to_ascii_lowercase().contains(phrase)) =>
        {
            "모델 응답 시간이 초과되었습니다."
        }
        Some(("Transient", _)) => "모델 연결에 문제가 생겼습니다.",
        Some(("RateLimited", _)) => "모델 서버의 요청 제한에 걸렸습니다.",
        Some(("Auth", _)) => "모델 인증을 확인하지 못했습니다.",
        Some(("Quota", _)) => "사용 가능한 모델 한도를 초과했습니다.",
        Some(("BadRequest", _)) => "모델이 요청을 받아들이지 못했습니다.",
        _ => "모델 응답을 받지 못했습니다.",
    }
}

/// Task.summary retains canonical stop reasons, including saved sessions.
pub fn failure_summary(summary: &str) -> Option<&'static str> {
    if summary == "repeated identical operations on unchanged revision" {
        return Some("같은 도구 요청을 반복해 진전이 없어 멈췄습니다.");
    }
    match summary.split_once(": ") {
        Some(("Cancelled", _)) => Some("작업을 중지했습니다."),
        Some(("TimedOut", _)) => Some("작업 시간이 초과되었습니다."),
        Some(("Transient", detail)) => {
            let detail = detail.to_ascii_lowercase();
            Some(
                if ["timed out", "time limit", "timeout"]
                    .iter()
                    .any(|phrase| detail.contains(phrase))
                {
                    "모델 응답을 기다리다 시간 초과로 멈췄습니다."
                } else {
                    "모델 연결 오류로 응답을 받지 못했습니다."
                },
            )
        }
        Some(("Auth", _)) => Some("모델 연결의 인증을 확인하지 못해 멈췄습니다."),
        Some(("RateLimited", _)) => Some("모델 서버의 요청 제한으로 응답을 받지 못했습니다."),
        Some(("Quota", _)) => Some("모델 사용 한도로 응답을 받지 못했습니다."),
        Some(("BadRequest", _)) => Some("모델이 요청을 받아들이지 못했습니다."),
        _ if summary == "cancelled or deadline reached" => {
            Some("작업 시간 한도에 도달해 멈췄습니다. /settings에서 한도를 바꿀 수 있습니다.")
        }
        _ if summary.contains("model-call budget exhausted") => {
            Some("모델 요청 한도에 도달해 멈췄습니다. /settings에서 한도를 바꿀 수 있습니다.")
        }
        _ => None,
    }
}

#[derive(Default)]
pub struct Projection {
    pub agents: BTreeMap<String, (String, bool)>,
    pub dependencies: BTreeMap<String, Vec<String>>,
    failed_agents: BTreeSet<String>,
    running: BTreeMap<String, String>,
    running_records: BTreeMap<String, usize>,
    command: Option<String>,
    records: Vec<String>,
    commentary: Vec<(usize, String, String)>,
    terminal_status: Option<String>,
    current: String,
    orchestrating: bool,
    operations: usize,
    failures: usize,
    model_waiting: bool,
    active_model_record: Option<usize>,
    /// Text of the reply the model is writing now; replaced by the final reply.
    streaming: String,
}
impl Projection {
    pub fn observe(&mut self, event: &Event) {
        if event.target == "preparation" {
            if event.detail["initialized"] == true {
                self.commentary.push((
                    self.records.len(),
                    event.worker.clone(),
                    "이 폴더는 Git 저장소가 아니어서 변경 추적을 위해 `git init`을 실행했습니다. 파일은 그대로입니다.".into(),
                ));
            }
            self.current = match event.detail["phase"].as_str() {
                Some("repository") => "Git 저장소 준비 중",
                Some("revision") => "저장소 상태 확인 중",
                Some("overlay") => "작업 공간 준비 중",
                Some("index") => "코드 인덱스 확인 중",
                _ => "작업 준비 중",
            }.into();
            return;
        }
        if event.target == "stream" {
            if let Some(text) = event.detail["text"].as_str() {
                self.streaming.push_str(text);
            }
            return;
        }
        if event.target == "commentary" {
            if let Some(text) = event.detail["text"].as_str().filter(|text| !text.trim().is_empty()) {
                self.commentary.push((self.records.len(), event.worker.clone(), text.into()));
            }
            return;
        }
        if event.target == "model" {
            self.streaming.clear();
            self.command = None;
            self.model_waiting = true;
            let attempt = event.detail["attempt"].as_u64().unwrap_or(1);
            let timeout = event.detail["timeout_seconds"].as_u64().unwrap_or(0);
            let attempt_label = if attempt <= 1 {
                String::new()
            } else {
                format!(" · 재시도 {}", attempt - 1)
            };
            self.current = if attempt <= 1 {
                "모델 응답 기다리는 중".into()
            } else {
                format!("모델 응답 기다리는 중{attempt_label}")
            };
            let _ = timeout;
            self.active_model_record = None;
            return;
        }
        if event.target == "model_result" {
            self.streaming.clear();
            self.model_waiting = false;
            self.current = "응답 처리 중".into();
            self.active_model_record = None;
            return;
        }
        if event.target == "model_retry" {
            let next = event.detail["next_attempt"].as_u64().unwrap_or(0);
            let retry = next.saturating_sub(1);
            let wait = event.detail["wait_seconds"].as_f64().unwrap_or(0.0);
            self.current = format!("모델 응답 지연 · 재시도 {retry} · {wait:.1}s");
            return;
        }
        if event.target == "model_error" {
            self.model_waiting = false;
            self.failures += 1;
            let reason = event.detail["reason"].as_str().unwrap_or("연결 오류");
            self.current = friendly_model_issue(reason).into();
            self.active_model_record = None;
            return;
        }
        if event.detail["kind"] == "instructions" {
            return;
        }
        let started = event.detail["started"] == true;
        let command = event.detail["tool"] == "run"
            || event.stage == Stage::Check && (started || event.detail.get("status").is_some());
        self.command = command.then(|| event.target.clone());
        let completed = event.detail["completed"] == true
            || event.stage == Stage::Check && event.detail.get("status").is_some();
        if started {
            let target = match event.stage {
                Stage::Navigation => format!("탐색 · {}", event.target),
                Stage::Source => format!("파일 확인 · {}", event.target),
                Stage::Change => format!("수정 · {}", event.target),
                _ => event.target.clone(),
            };
            self.running.insert(event.worker.clone(), target);
            self.records.push(tool_record(event, "◌"));
            self.running_records
                .insert(event.worker.clone(), self.records.len() - 1);
        }
        if completed || event.failed || event.stage == Stage::AgentFinished {
            self.running.remove(&event.worker);
        }
        match event.stage {
            Stage::Planning => self.current = "계획 중".into(),
            Stage::Orchestrating => {
                self.orchestrating = true;
                if let Some(units) = event.detail["units"].as_array() {
                    for u in units {
                        if let (Some(id), Some(goal)) = (u["id"].as_str(), u["goal"].as_str()) {
                            self.agents.insert(id.into(), (goal.into(), false));
                            self.failed_agents.remove(id);
                            self.dependencies.insert(
                                id.into(),
                                u["depends_on"]
                                    .as_array()
                                    .into_iter()
                                    .flatten()
                                    .filter_map(|v| v.as_str().map(str::to_owned))
                                    .collect(),
                            );
                        }
                    }
                }
                self.current = "작업 나누는 중".into();
            }
            Stage::Direct => {
                self.orchestrating = false;
                self.current = "요청 확인 중".into();
            }
            Stage::AgentStarted => {
                self.failed_agents.remove(&event.worker);
                self.agents
                    .insert(event.worker.clone(), (event.target.clone(), false));
            }
            Stage::AgentFinished => {
                self.agents
                    .insert(event.worker.clone(), (event.target.clone(), !event.failed));
                if event.failed {
                    self.failed_agents.insert(event.worker.clone());
                }
            }
            Stage::Navigation => self.current = format!("탐색 · {}", event.target),
            Stage::Source => self.current = format!("코드 확인 · {}", event.target),
            Stage::Change => self.current = format!("수정 · {}", event.target),
            Stage::Apply => self.current = "변경 적용 중".into(),
            Stage::Merge => self.current = "작업 병합 중".into(),
            _ => {}
        }
        if command {
            self.current = format!(
                "{} · {}",
                if started {
                    "명령 실행 중"
                } else if event.detail["status"] == "Cancelled" {
                    "명령 중단"
                } else if event.failed {
                    "명령 실패"
                } else {
                    "명령 실행 완료"
                },
                event.target
            );
        }
        if completed || event.failed {
            self.operations += 1;
            self.failures += usize::from(event.failed);
            let marker = if event.failed { "!" } else { "✓" };
            let heading = tool_record(event, marker);
            if let Some(index) = self.running_records.remove(&event.worker) {
                if let Some(record) = self.records.get_mut(index) {
                    *record = heading;
                }
            } else if matches!(
                event.stage,
                Stage::Navigation | Stage::Source | Stage::Change | Stage::Run | Stage::Check
            ) {
                self.records.push(heading);
            }
        }
        if event.failed && !command {
            self.current = format!("확인 필요 · {}", event.target);
        }
    }
    pub fn finish(&mut self, success: bool, cancelled: bool, summary: &str) {
        self.model_waiting = false;
        self.running.clear();
        self.terminal_status = Some(
            if cancelled {
                "interrupted"
            } else if success {
                "completed"
            } else {
                "failed"
            }
            .into(),
        );
        if !success && !cancelled && self.failures == 0 {
            self.failures = 1;
        }
        let _ = summary;
    }
    /// Text the model was still writing, not yet part of a finished reply.
    pub fn streaming_text(&self) -> &str {
        self.streaming.trim()
    }
    pub fn summary(&self) -> String {
        if self.orchestrating && !self.agents.is_empty() {
            let done = self.agents.values().filter(|(_, done)| *done).count();
            format!("작업 {done}/{} 완료\n{}", self.agents.len(), self.current)
        } else {
            self.current.clone()
        }
    }
    pub fn entries(&self, turn: &str, done: bool) -> Vec<Entry> {
        let mut entries = self.records_and_commentary(turn);
        if !done {
            // Text the model is writing reads like any reply; the status line carries
            // the progress, so it is not drawn as a progress entry.
            let streaming = !self.streaming.trim().is_empty();
            entries.push(Entry {
                id: format!("{turn}-assistant"),
                kind: Kind::Assistant,
                title: if streaming { String::new() } else { crate::agent::PROGRESS_TITLE.into() },
                body: if !self.streaming.trim().is_empty() {
                    self.streaming.trim_start().to_owned()
                } else if self.running.is_empty() && self.command.is_none() {
                    self.summary()
                } else {
                    String::new()
                },
                status: "inProgress".into(),
                expanded: false,
            });
        }
        entries
    }

    fn records_and_commentary(&self, turn: &str) -> Vec<Entry> {
        let mut entries = Vec::new();
        for (index, record) in self.records.iter().enumerate() {
            self.append_commentary(&mut entries, turn, index);
            let (title, body) = record.split_once('\n').unwrap_or((record.as_str(), ""));
            let status = if title.starts_with('◌') {
                "inProgress"
            } else if title.starts_with('!') {
                "failed"
            } else {
                "completed"
            };
            let title = title
                .strip_prefix('◌')
                .or_else(|| title.strip_prefix('!'))
                .or_else(|| title.strip_prefix('✓'))
                .unwrap_or(title)
                .trim_start();
            entries.push(Entry {
                id: format!("{turn}-tool-{index}"),
                kind: Kind::Tool,
                title: title.to_owned(),
                body: body.to_owned(),
                status: status.into(),
                expanded: false,
            });
        }
        self.append_commentary(&mut entries, turn, self.records.len());
        entries
    }
    fn append_commentary(&self, entries: &mut Vec<Entry>, turn: &str, position: usize) {
        for (index, (_, worker, text)) in self.commentary.iter().enumerate().filter(|(_, (at, _, _))| *at == position) {
            entries.push(Entry {
                id: format!("{turn}-commentary-{index}"), kind: Kind::Assistant,
                title: if worker == "main" {String::new()} else {format!("담당 · {worker}")},
                body: text.clone(), status: "completed".into(), expanded: false,
            });
        }
    }
}

fn tool_heading(event: &Event, marker: &str) -> String {
    let Some(tool) = event.detail["tool"].as_str() else {
        return format!("{marker} {}", short_input(&event.target));
    };
    let args = &event.detail["args"];
    let input = if tool == "agent" {
        format!("작업 {}개", args["units"].as_array().map_or(0, Vec::len))
    } else {
        args["target"].as_str()
            .or_else(|| args["query"].as_str())
            .or_else(|| args["command"].as_str())
            .or_else(|| args["set"].as_str())
            .or_else(|| args["question"].as_str())
            .unwrap_or(&event.target).to_owned()
    };
    if input == tool { format!("{marker} {tool}") }
    else { format!("{marker} {tool} · {}", short_input(&input)) }
}

fn short_input(input: &str) -> String {
    let single_line = input.split_whitespace().collect::<Vec<_>>().join(" ");
    if single_line.chars().count() > 100 {
        format!("{}…", single_line.chars().take(100).collect::<String>())
    } else { single_line }
}

fn tool_record(event: &Event, marker: &str) -> String {
    let heading = tool_heading(event, marker);
    let detail = compact_tool_detail(event);
    if detail.is_empty() {
        heading
    } else {
        format!("{heading}\n{detail}")
    }
}

fn compact_tool_detail(event: &Event) -> String {
    let mut parts = Vec::new();
    if event.worker != "main" {
        parts.push(format!("담당 · {}", event.worker));
    }
    if let Some(args) = event.detail.get("args").and_then(|value| value.as_object()) {
        for (key, label) in [("query", "검색"), ("kind", "종류"), ("scope", "범위"),
            ("target", "대상"), ("set", "집합"), ("relation", "관계"),
            ("op", "동작"), ("command", "명령"), ("start", "시작"), ("end", "끝"),
            ("question", "질문")] {
            let Some(value) = args.get(key) else { continue; };
            let text = value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string());
            let text = if text.chars().count() > 120 {
                format!("{}…", text.chars().take(120).collect::<String>())
            } else {
                text
            };
            parts.push(format!("{label} · {text}"));
        }
        if let Some(units) = args.get("units").and_then(|value| value.as_array()) {
            for unit in units {
                if let (Some(id), Some(goal)) = (unit["id"].as_str(), unit["goal"].as_str()) {
                    parts.push(format!("{id} · {goal}"));
                }
            }
        }
    }
    if let Some(error) = event.detail.get("error").and_then(|value| value.as_str()) {
        parts.push(format!("오류 · {}", error.chars().take(180).collect::<String>()));
    }
    let result = event.detail.get("result");
    let object = result
        .and_then(|value| value.as_object())
        .or_else(|| {
            result
                .and_then(|value| value.as_array())
                .and_then(|values| values.first())
                .and_then(|value| value.as_object())
        });
    if let Some(object) = object {
        let mut summary = Vec::new();
        for key in ["status", "count", "set", "artifact_target"] {
            let Some(value) = object.get(key) else { continue; };
            let value = value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string());
            summary.push(format!("{key}={value}"));
        }
        if !summary.is_empty() {
            parts.push(format!("결과 · {}", summary.join(" · ")));
        }
    }
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streamed_text_appears_while_the_model_writes_and_yields_to_the_final_reply() {
        let mut projection = Projection::default();
        projection.observe(&Event::new("main", Stage::Direct, "model", serde_json::json!({"attempt":1})));
        for delta in ["안녕", "하세요"] {
            projection.observe(&Event::new("main", Stage::Answer, "stream", serde_json::json!({"text":delta})));
        }
        let live = projection.entries("turn", false);
        assert_eq!(live.last().unwrap().body, "안녕하세요", "{live:?}");
        assert!(live.last().unwrap().title.is_empty(), "streamed text is drawn as a plain reply");
        projection.observe(&Event::new("main", Stage::Direct, "model_result", serde_json::json!({})));
        projection.observe(&Event::new("main", Stage::Answer, "commentary", serde_json::json!({"text":"안녕하세요"})));
        let done = projection.entries("turn", false);
        assert_eq!(done.iter().filter(|entry| entry.body.contains("안녕하세요")).count(), 1, "{done:?}");
        // The next request starts with an empty live text.
        projection.observe(&Event::new("main", Stage::Direct, "model", serde_json::json!({"attempt":1})));
        assert_ne!(projection.entries("turn", false).last().unwrap().body, "안녕하세요");
    }

    #[test]
    fn preparation_reports_real_phases_without_fabricating_tool_calls() {
        let mut projection = Projection::default();
        for (phase, label) in [("revision", "저장소 상태 확인 중"), ("overlay", "작업 공간 준비 중"), ("index", "코드 인덱스 확인 중")] {
            projection.observe(&Event::new("main", Stage::Direct, "preparation", serde_json::json!({"phase":phase,"started":true})));
            let entries = projection.entries("turn", false);
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].kind, Kind::Assistant);
            assert_eq!(entries[0].body, label);
        }
    }

    #[test]
    fn initializing_a_repository_is_announced_once() {
        let mut p = Projection::default();
        p.observe(&Event::new("main", Stage::Direct, "preparation", serde_json::json!({"phase":"repository","initialized":true})));
        assert_eq!(p.current, "Git 저장소 준비 중");
        assert_eq!(p.commentary.len(), 1);
        assert!(p.commentary[0].2.contains("git init"));
        p.observe(&Event::new("main", Stage::Direct, "preparation", serde_json::json!({"phase":"revision","started":true})));
        assert_eq!(p.commentary.len(), 1);
    }

    #[test]
    fn operational_trace_is_compact_and_does_not_repeat_main_or_model_result() {
        let mut p = Projection::default();
        p.observe(&Event::new(
            "main",
            Stage::Source,
            "/home/user/project/AGENTS.md",
            serde_json::json!({"kind":"instructions"}),
        ));
        p.observe(&Event::new(
            "main",
            Stage::Run,
            "model",
            serde_json::json!({
                "model":"ollama/SparkLLM/Spark-X2.5-1.7B:latest",
                "attempt":1,
                "timeout_seconds":90,
                "reasoning_effort":"on",
                "tools":11
            }),
        ));
        p.observe(&Event::new(
            "main",
            Stage::Run,
            "model_result",
            serde_json::json!({}),
        ));
        p.observe(&Event::new(
            "main",
            Stage::Run,
            "git",
            serde_json::json!({"completed":true}),
        ));
        p.observe(&Event::new(
            "main",
            Stage::Source,
            "custom-tui/src/agent.rs",
            serde_json::json!({
                "completed":true,
                "tool":"open",
                "args":{"start":0,"end":250,"target":"custom-tui/src/agent.rs"},
                "result":[{
                    "revision":"3664090a24bc689824bc1f8498bdac2631b61d44",
                    "source":"pub struct Entry {\n    pub body: String,\n}\n".repeat(200)
                }]
            }),
        ));
        p.observe(&Event::new(
            "main",
            Stage::Run,
            "model",
            serde_json::json!({
                "model":"ollama/SparkLLM/Spark-X2.5-1.7B:latest",
                "attempt":1,
                "timeout_seconds":90,
                "reasoning_effort":"on",
                "tools":11
            }),
        ));

        let entries = p.entries("turn", false);
        let tools = entries
            .iter()
            .filter(|entry| entry.kind == Kind::Tool)
            .collect::<Vec<_>>();
        assert_eq!(tools.len(), 2);
        assert!(tools.iter().any(|entry| entry.title.contains("git")));
        assert!(tools
            .iter()
            .any(|entry| entry.title == "open · custom-tui/src/agent.rs"));
        let visible = tools
            .iter()
            .map(|entry| format!("{}\n{}", entry.title, entry.body))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!visible.contains("모델"));
        assert!(!visible.contains("AGENTS.md"));
        assert!(!visible.contains("\"args\""));
        assert!(!visible.contains("\"revision\""));
        assert!(!visible.contains("pub struct Entry"));
    }

    #[test]
    fn tool_titles_put_tool_before_input_without_worker_or_model_noise() {
        let mut p = Projection::default();
        for (tool, stage, target, args) in [
            ("find", Stage::Navigation, "\"v4 Core\"", serde_json::json!({"kind":"text","query":"\"v4 Core\"","scope":"custom-tui"})),
            ("open", Stage::Source, "src/lib.rs", serde_json::json!({"target":"src/lib.rs","start":10,"end":30})),
            ("run", Stage::Run, "cargo test", serde_json::json!({"kind":"command","command":"cargo test"})),
        ] {
            p.observe(&Event::new("find_v4_v5_diff", stage, target,
                serde_json::json!({"tool":tool,"args":args,"started":true})));
            // The live progress entry is last; the newest tool record precedes it.
            let entries = p.entries("turn", false);
            let entry = entries[entries.len() - 2].clone();
            assert_eq!(entry.title, format!("{tool} · {target}"));
            assert_eq!(entry.status, "inProgress");
            p.observe(&Event::new("find_v4_v5_diff", stage, target,
                serde_json::json!({"tool":tool,"args":args,"completed":true})));
            let entry = p.entries("turn", true).pop().unwrap();
            assert_eq!(entry.title, format!("{tool} · {target}"));
            assert_eq!(entry.status, "completed");
        }
        let entries = p.entries("turn", true);
        assert_eq!(entries.len(), 3);
        assert!(entries[0].body.contains("범위 · custom-tui"));
        assert!(entries[1].body.contains("시작 · 10") && entries[1].body.contains("끝 · 30"));
    }

    #[test]
    fn agent_call_is_one_compact_tool_record_with_expandable_goals() {
        let mut p = Projection::default();
        let goal = "Search the codebase for architectural differences. ".repeat(10);
        let args = serde_json::json!({"units":[{"id":"find_v4_v5_diff","goal":goal}]});
        p.observe(&Event::new("main", Stage::Orchestrating, "agent",
            serde_json::json!({"tool":"agent","args":args,"started":true})));
        p.observe(&Event::new("main", Stage::Orchestrating, "DAG", args.clone()));
        let tools = p.entries("turn", false).into_iter().filter(|e| e.kind == Kind::Tool).collect::<Vec<_>>();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].title, "agent · 작업 1개");
        assert!(tools[0].body.contains("find_v4_v5_diff") && tools[0].body.contains(&goal));
        p.observe(&Event::new("main", Stage::Orchestrating, "agent",
            serde_json::json!({"tool":"agent","args":args,"completed":true})));
        assert_eq!(p.entries("turn", true)[0].status, "completed");
    }

    #[test]
    fn waiting_keeps_an_operational_trace_without_repeating_model_input() {
        let mut p = Projection::default();
        p.observe(&Event::new(
            "main",
            Stage::Run,
            "model",
            serde_json::json!({
            "model": "fixture", "attempt": 1, "max_attempts": 3,
            "timeout_seconds": 45, "request": "private user input"}),
        ));
        assert_eq!(p.current, "모델 응답 기다리는 중");
        let entries = p.entries("turn", false);
        assert!(entries.iter().all(|entry| entry.kind != Kind::Tool));
        assert!(entries.iter().all(|entry| !entry.body.contains("private user input")));

        p.observe(&Event::new(
            "main",
            Stage::Run,
            "model",
            serde_json::json!({
            "model": "fixture", "attempt": 2, "max_attempts": 3,
            "timeout_seconds": 45}),
        ));
        assert_eq!(p.current, "모델 응답 기다리는 중 · 재시도 1");
        assert!(p.entries("turn", false).iter().all(|entry| entry.kind != Kind::Tool));

        p.observe(&Event::new(
            "main",
            Stage::Error,
            "model_error",
            serde_json::json!({"reason":"TimedOut: execution deadline reached"}),
        ));
        assert_eq!(p.current, "작업 시간이 초과되었습니다.");
    }
}
