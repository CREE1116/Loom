//! One conversational projection of any number of internal workers.
use crate::agent::{Entry, Kind, WorkPhase};
use crate::backend::activity_events::{Event, EventKind, GraphEvidence};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
pub struct Conversation {
    comments: Vec<String>,
    records: Vec<String>,
    workers: BTreeSet<String>,
    succeeded: usize,
    failed: usize,
    corrections: usize,
    current: String,
    last_action: Option<String>,
    running: BTreeMap<String, Event>,
    phase: WorkPhase,
    tools: BTreeSet<String>,
    index: Option<GraphEvidence>,
    contexts: BTreeMap<String, GraphEvidence>,
    plans: Vec<GraphEvidence>,
    skipped: Option<String>,
    stopped: bool,
    reconnects: usize,
    model_failed: bool,
}
impl Conversation {
    pub fn observe(&mut self, event: &Event) {
        if let Some(issue)=crate::backend::activity_events::ModelIssue::decode(event) {
            if issue.retrying {self.reconnects+=1;} else {self.model_failed=true;}
            self.records.push(format!("모델 연결 {} · 시도 {} · {:?} · 대기 {}ms\n{}",
                if issue.retrying {"재시도"} else {"중단"},issue.attempt,issue.category,issue.delay_ms,issue.error));
            self.current=[self.last_action.clone().unwrap_or_default(),event.text.clone()].into_iter().filter(|s|!s.is_empty()).collect::<Vec<_>>().join("\n");
            self.phase=WorkPhase::Thinking;return;
        }
        if let Some(correction) = crate::backend::activity_events::Correction::decode(event) {
            self.corrections += 1;
            self.records.push(format!("↻ {} 형식 교정 {}\n{}",correction.stage,correction.attempt,correction.error));
            self.current = format!("{}을 다듬고 있습니다.", if correction.stage == "계획" {"계획"} else {"실행 계획"});
            self.phase = WorkPhase::Thinking;
            return;
        }
        if event.kind != EventKind::GraphObserved && event.worker != "coordinator" && event.worker != "validation" {
            self.workers.insert(event.worker.clone());
        }
        match event.kind {
            EventKind::GraphObserved => {
                if let Some(evidence) = GraphEvidence::decode(event) {
                    match &evidence {
                        GraphEvidence::Index {..} => self.index = Some(evidence),
                        GraphEvidence::Context {..} => {self.contexts.insert(event.worker.clone(), evidence);}
                        GraphEvidence::Plan {..} => self.plans.push(evidence),
                        GraphEvidence::Skipped {reason} => self.skipped = Some(reason.clone()),
                    }
                }
            }
            EventKind::Commentary => {
                if !event.text.trim().is_empty() && self.comments.last() != Some(&event.text) {
                    self.comments.push(event.text.clone());
                }
            }
            EventKind::Started => self.current = event.text.clone(),
            EventKind::ModelRequest => {
                self.phase = WorkPhase::Thinking;
                self.current = self.last_action.as_ref().map(|action|format!("{action}\n응답을 기다리는 중입니다.")).unwrap_or_else(||"응답을 기다리는 중입니다.".into());
            }
            EventKind::ToolStarted => {
                self.remember_tool(event);
                self.phase = WorkPhase::Tool;
                self.running.insert(event.worker.clone(), event.clone());
                self.current = describe_action(event);
            }
            EventKind::ToolSucceeded | EventKind::ToolFailed => {
                self.remember_tool(event);
                self.running.remove(&event.worker);
                self.phase = WorkPhase::Tool;
                let failed = event.kind == EventKind::ToolFailed;
                if failed {
                    self.failed += 1;
                } else {
                    self.succeeded += 1;
                }
                self.records.push(format!(
                    "{} {} · {} · {} · {}\n{}",
                    if failed { "×" } else { "✓" },
                    event.worker,
                    event.tool.as_deref().unwrap_or("tool"),
                    operation(event.tool.as_deref()),
                    target(event),
                    event.text
                ));
                let action=describe_action(event);
                self.last_action=Some(action.clone());
                self.current=action;
            }
            EventKind::Stopped => {
                self.running.remove(&event.worker);
                self.stopped = true;
                self.records.push(format!("{} 중단\n{}",event.worker,event.text));
                self.current = "작업을 멈췄습니다. 진행한 내용은 작업 기록에 남겼습니다.".into();
            }
            EventKind::ModelResponse => {}
        }
    }
    fn remember_tool(&mut self, event: &Event) {
        if let Some(tool) = &event.tool {self.tools.insert(tool.clone());}
    }
    pub fn commentary(&self) -> String {
        self.comments.join("\n\n")
    }
    pub fn phase(&self) -> WorkPhase {
        if self.running.values().any(|e| matches!(e.tool.as_deref(), Some("shell" | "exec_command" | "write_stdin"))) {
            WorkPhase::Shell
        } else if self.running.values().any(|e| e.tool.as_deref() == Some("command")) {
            WorkPhase::Command
        } else if !self.running.is_empty() { WorkPhase::Tool } else { self.phase }
    }
    pub fn running(&self) -> bool { !self.running.is_empty() }
    fn running_text(&self) -> String {
        self.running.values().map(|event| {
            format!("{} · {} 실행 중 · {}", event.tool.as_deref().unwrap_or("tool"), operation(event.tool.as_deref()), target(event))
        }).collect::<Vec<_>>().join("\n")
    }
    fn running_summary(&self) -> String {
        self.running.values().map(describe_action).collect::<Vec<_>>().join("\n")
    }
    pub fn assistant(&self, turn: &str) -> Entry {
        let current = if self.running.is_empty() { self.current.clone() } else { self.running_summary() };
        let body = [self.commentary(), current]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        Entry {
            id: format!("{turn}-assistant"),
            kind: Kind::Assistant,
            title: String::new(),
            body,
            status: "inProgress".into(),
            expanded: false,
        }
    }
    pub fn log(&self, turn: &str, done: bool) -> Option<Entry> {
        if self.records.is_empty() && (done || self.running.is_empty()) {
            return None;
        }
        Some(Entry {
            id: format!("{turn}-work-log"),
            kind: Kind::Tool,
            title: if self.tools.is_empty() {String::new()} else {format!("{} · ", self.tools.iter().cloned().collect::<Vec<_>>().join(" · "))} + &format!(
                "작업 기록 · 워커 {} · 성공 {} · 실패 {}",
                self.workers.len(),
                self.succeeded,
                self.failed
            ) + &if self.corrections > 0 {format!(" · 교정 {}",self.corrections)} else {String::new()}
              + &if self.reconnects > 0 {format!(" · 재연결 {}",self.reconnects)} else {String::new()}
              + &if self.model_failed {" · 모델 연결 중단".into()} else {String::new()}
              + &if !done && !self.running.is_empty() {format!(" · 실행 {}", self.running.len())} else {String::new()},
            body: if !done && !self.running.is_empty() {
                [self.running_text(), self.records.join("\n\n")].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n\n")
            } else { self.records.join("\n\n") },
            status: if !done {
                "inProgress"
            } else if self.failed > 0 || self.model_failed {
                "failed"
            } else if self.stopped {
                "interrupted"
            } else {
                "completed"
            }
            .into(),
            expanded: false,
        })
    }
    pub fn graph_log(&self, turn: &str) -> Option<Entry> {
        if self.index.is_none() && self.contexts.is_empty() && self.plans.is_empty() && self.skipped.is_none() {return None;}
        let mut body = Vec::new();
        if let Some(GraphEvidence::Index {files,symbols,hashed,reused,graph_reused}) = &self.index {
            body.push(format!("인덱스: {files} 파일 · {symbols} 심볼\n파일 해시: 새로 읽음 {hashed} · 캐시 재사용 {reused}\nAST 그래프: {}", if *graph_reused {"캐시 재사용"} else {"구성"}));
        }
        let mut symbol_count = 0;
        let mut bytes = 0;
        for (worker, evidence) in &self.contexts {
            if let GraphEvidence::Context {bytes:n,symbol_count:count,symbols,truncated} = evidence {
                symbol_count += count;
                bytes += n;
                body.push(format!("{worker} 요청 문맥: {count} 심볼 · {n} bytes{}",if *truncated {" · 길이 제한으로 잘림"} else {""}));
                for symbol in symbols {
                    body.push(format!("  {} · {}:{}-{} · {} · {} bytes{}",symbol.name,symbol.path,symbol.start,symbol.end,
                        if symbol.role == "body" {"본문"} else {"개요"},symbol.bytes,if symbol.partial {" · 일부"} else {""}));
                }
                if symbols.len() < *count {body.push(format!("  그 외 {} 심볼", count-symbols.len()));}
            }
        }
        let mut edges = BTreeSet::new();
        let mut reads = BTreeSet::new();
        for plan in &self.plans {
            if let GraphEvidence::Plan {dependency_count,dependencies,read_count,reads:added} = plan {
                edges.extend(dependencies.iter().cloned());
                reads.extend(added.iter().cloned());
                body.push(format!("계획 그래프·범위 검사: 의존관계 {dependency_count}개 · 읽기 범위 {read_count}개 추가"));
                body.extend(dependencies.iter().map(|(unit,dependency)|format!("  {unit} ← {dependency}")));
                body.extend(added.iter().map(|(unit,path)|format!("  {unit} 읽기: {path}")));
            }
        }
        if let Some(reason) = &self.skipped {body.push(reason.clone());}
        let title = if symbol_count > 0 {format!("AST · 문맥 {symbol_count} 심볼 · {bytes} bytes")}
            else if !self.contexts.is_empty() {"AST · 선택 문맥 없음".into()}
            else if self.index.is_some() {"AST · 인덱스 준비됨".into()}
            else {"AST · 사용 안 함".into()};
        Some(Entry {id:format!("{turn}-ast-log"),kind:Kind::Tool,
            title:format!("{title} · 의존관계 {} · 읽기 범위 {}",edges.len(),reads.len()),
            body:body.join("\n"),status:"completed".into(),expanded:false})
    }
}
fn display_target(event: &Event) -> String {
    if event.tool.as_deref()==Some("read_artifact") {
        if let Ok(args)=serde_json::from_str::<serde_json::Value>(&event.detail) {
            if let Some(id)=args["id"].as_str().filter(|id|id.chars().count()>12) {
                return format!("{}…\noffset {}",id.chars().take(12).collect::<String>(),args["offset"].as_u64().unwrap_or(0));
            }
        }
    }
    target(event)
}
fn describe_action(event: &Event) -> String {
    let running=event.kind==EventKind::ToolStarted;
    let failed=event.kind==EventKind::ToolFailed;
    let (ongoing, finished)=match event.tool.as_deref() {
        Some("read_file") => ("내용을 읽고 있습니다", "내용을 읽었습니다"),
        Some("list_dir") => ("폴더 내용을 살펴보고 있습니다", "폴더 내용을 확인했습니다"),
        Some("search") => ("검색하고 있습니다", "검색 결과를 확인했습니다"),
        Some("edit_file") => ("파일을 수정하고 있습니다", "파일을 수정했습니다"),
        Some("write_file") => ("파일을 작성하고 있습니다", "파일을 작성했습니다"),
        Some("delete_file") => ("파일을 삭제하고 있습니다", "파일을 삭제했습니다"),
        Some("read_artifact") => ("실행 기록을 읽고 있습니다", "실행 기록을 읽었습니다"),
        Some("finish") => ("결과를 정리하고 있습니다", "결과를 정리했습니다"),
        Some("shell" | "exec_command" | "write_stdin") => ("셸 명령을 실행하고 있습니다", "셸 명령 실행이 끝났습니다"),
        Some("command") => ("명령을 실행하고 있습니다", "명령 실행이 끝났습니다"),
        _ => ("도구를 실행하고 있습니다", "도구 실행이 끝났습니다"),
    };
    let description=if failed {format!("{} 중 오류가 났습니다",operation(event.tool.as_deref()))}
        else {if running {ongoing} else {finished}.into()};
    format!("{}: {description}. ({})",display_target(event),event.tool.as_deref().unwrap_or("tool"))
}
fn target(event: &Event) -> String {
    let args: serde_json::Value = serde_json::from_str(&event.detail).unwrap_or_default();
    let value = if let Some(command)=args["command"].as_str().or_else(||args["cmd"].as_str()) {
        command.to_owned()
    } else if let Some(id)=args["id"].as_str() {
        format!("{id} · offset {}",args["offset"].as_u64().unwrap_or(0))
    } else {
        let path=event.path.as_deref().or_else(||args["path"].as_str());
        match (path,args["query"].as_str()) {
            (Some(path),Some(query)) => format!("{path} · {query}"),
            (Some(path),None) => path.to_owned(),
            (None,Some(query)) => query.to_owned(),
            _ => "결과".into(),
        }
    };
    value.chars().filter(|c| !c.is_control()).take(240).collect()
}
fn operation(tool: Option<&str>) -> &'static str {
    match tool {
        Some("edit_file") => "파일 수정",
        Some("write_file") => "파일 작성",
        Some("delete_file") => "파일 삭제",
        Some("read_file") => "파일 읽기",
        Some("read_artifact") => "기록 읽기",
        Some("search") => "검색",
        Some("list_dir") => "목록 조회",
        Some("finish") => "결과 정리",
        Some("shell" | "exec_command" | "write_stdin") => "셸",
        Some("command") => "명령",
        _ => "작업",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn internal_corrections_stay_in_the_folded_record() {
        let mut conversation=Conversation::default();
        let mut correction=Event::new("coordinator",EventKind::Commentary,"계획을 수정 중입니다.");
        correction.detail=serde_json::json!({"kind":"Correction","stage":"계획","attempt":1,"error":"tool assertion missing name"}).to_string();
        for _ in 0..3 {conversation.observe(&correction);}
        assert!(!conversation.commentary().contains("계획을 수정"));
        assert!(!conversation.assistant("turn").body.contains("tool assertion"));
        let log=conversation.log("turn",true).unwrap();
        assert!(log.title.contains("교정 3") && !log.title.contains("실패 3"),"{}",log.title);
        assert!(log.body.contains("tool assertion missing name"));
        assert_eq!(log.status,"completed");
    }
    #[test]
    fn completed_read_has_a_useful_target_without_failure_zero_noise() {
        let mut conversation=Conversation::default();
        let mut event=tool("worker-1","read_file");
        event.kind=EventKind::ToolSucceeded;event.detail=serde_json::json!({"path":"README.md"}).to_string();
        conversation.observe(&event);
        let body=conversation.assistant("turn").body;
        assert!(body.contains("README.md") && body.contains("읽었습니다"),"{body}");
        assert!(!body.contains("실패 0"));
        conversation.observe(&Event::new("worker-1",EventKind::ModelRequest,"next response"));
        let waiting=conversation.assistant("turn").body;
        assert!(waiting.contains("README.md")&&waiting.contains("응답"),"waiting erased the last observed action: {waiting}");
    }
    #[test]
    fn artifact_read_shows_receipt_id_and_requested_offset() {
        let mut conversation=Conversation::default();
        let mut event=tool("worker-1","read_artifact");
        let id="a".repeat(64);
        event.detail=serde_json::json!({"id":id,"offset":3000}).to_string();
        conversation.observe(&event);
        let body=conversation.assistant("turn").body;
        assert!(body.contains("aaaaaaaaaaaa…") && body.contains("3000") && !body.contains(&id),"{body}");
        assert!(conversation.log("turn",false).unwrap().body.contains(&id),"full receipt id must remain in details");
    }
    #[test]
    fn ast_index_does_not_claim_selected_context_and_metadata_does_not_change_phase() {
        let mut conversation = Conversation::default();
        conversation.observe(&tool("worker-1", "shell"));
        conversation.observe(&Event::graph("coordinator",GraphEvidence::Index {
            files:10,symbols:50,hashed:0,reused:10,graph_reused:true
        }));
        assert_eq!(conversation.phase(),WorkPhase::Shell);
        let log=conversation.graph_log("turn").unwrap();
        assert!(log.title.contains("인덱스 준비됨") && !log.title.contains("문맥 50"));
        assert!(log.body.contains("캐시 재사용 10"));
        conversation.observe(&Event::graph("worker-1",GraphEvidence::Context {
            bytes:0,symbol_count:0,symbols:vec![],truncated:false
        }));
        assert!(conversation.graph_log("turn").unwrap().title.contains("선택 문맥 없음"));
    }
    #[test]
    fn unrecognized_extension_tool_keeps_its_name_and_path() {
        let mut conversation=Conversation::default();
        let mut event=tool("worker-1","ast_search");
        event.detail=serde_json::json!({"path":"lib.rs"}).to_string();
        conversation.observe(&event);
        let assistant=conversation.assistant("turn");
        assert!(assistant.body.contains("ast_search") && assistant.body.contains("lib.rs"));
    }
    #[test]
    fn raw_tool_name_and_query_are_visible_without_expanding_the_log() {
        let mut conversation = Conversation::default();
        let mut event = Event::new("worker-1", EventKind::ToolStarted, "started");
        event.tool = Some("search".into());
        event.detail = serde_json::json!({"query":"alpha"}).to_string();
        conversation.observe(&event);
        let body = conversation.assistant("turn").body;
        assert!(body.contains("search") && body.contains("alpha"), "{body}");
        event.kind = EventKind::ToolSucceeded;
        event.text = "one match".into();
        conversation.observe(&event);
        let log = conversation.log("turn", true).unwrap();
        assert!(!log.expanded);
        assert!(log.title.contains("search"), "{}",log.title);
        assert!(log.body.contains("alpha") && log.body.contains("search"));
    }
    fn tool(worker: &str, name: &str) -> Event {
        let mut event = Event::new(worker, EventKind::ToolStarted, "started");
        event.tool = Some(name.into());
        event.detail = serde_json::json!({"command":"python job.py"}).to_string();
        event
    }
    #[test]
    fn first_tool_is_visible_while_running_and_failures_clear_it() {
        let mut conversation = Conversation::default();
        let mut event = tool("worker-1", "command");
        conversation.observe(&event);
        assert_eq!(conversation.phase(), WorkPhase::Command);
        let body=conversation.assistant("turn").body;
        assert!(body.contains("명령을 실행하고 있습니다")&&body.contains("python job.py")&&body.contains("command"),"{body}");
        let log = conversation.log("turn", false).unwrap();
        assert!(log.title.contains("성공 0 · 실패 0 · 실행 1"));
        assert_eq!(log.status, "inProgress");
        event.kind = EventKind::ToolFailed;
        event.text = "process failed".into();
        conversation.observe(&event);
        assert!(!conversation.running());
        let log = conversation.log("turn", true).unwrap();
        assert!(log.title.contains("실패 1") && !log.title.contains("실행 1"));
        assert!(log.body.contains("python job.py") && log.body.contains("process failed"));
        assert_eq!(log.status, "failed");
    }
    #[test]
    fn parallel_worker_events_do_not_hide_running_shell() {
        let mut conversation = Conversation::default();
        let mut shell = tool("worker-1", "exec_command");
        let mut read = tool("worker-2", "read_file");
        conversation.observe(&shell);
        conversation.observe(&read);
        conversation.observe(&Event::new("worker-2", EventKind::ModelRequest, "thinking"));
        assert_eq!(conversation.phase(), WorkPhase::Shell);
        assert!(conversation.log("turn", false).unwrap().title.contains("실행 2"));
        read.kind = EventKind::ToolSucceeded;
        conversation.observe(&read);
        assert!(conversation.assistant("turn").body.contains("셸 명령을 실행하고 있습니다"));
        assert_eq!(conversation.phase(), WorkPhase::Shell);
        shell.kind = EventKind::ToolSucceeded;
        conversation.observe(&shell);
        assert!(!conversation.assistant("turn").body.contains("실행하고 있습니다"));
        assert!(!conversation.running());
    }
    #[test]
    fn stopping_worker_clears_live_execution_without_claiming_success() {
        let mut conversation = Conversation::default();
        conversation.observe(&tool("worker-1", "shell"));
        conversation.observe(&Event::new("worker-1", EventKind::Stopped, "cancelled"));
        assert!(!conversation.running());
        let log=conversation.log("turn", true).unwrap();
        assert_eq!(log.status,"interrupted");
        assert!(log.body.contains("cancelled"));
        assert!(conversation.assistant("turn").body.contains("작업을 멈췄습니다"));
    }
}
