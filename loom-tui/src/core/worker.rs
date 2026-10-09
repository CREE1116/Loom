//! Native worker loop: Core picks the model (router), owns the tools, and records cost.
//!
//! The user-owned provider policy bounds every call, including explicit selections.
use super::model::router::{Need, Router};
use super::model::{Message, Part, Request, Role};
use super::telemetry::Phase;
use super::tools::Workspace;
use serde::Serialize;
use serde_json::json;
use std::{collections::BTreeMap, io::Write, path::PathBuf, time::Instant};

/// Current user-authorized maximum cost class.
pub use loom_core::models::cost_ceiling;

const SYSTEM: &str =
    "You are a Loom worker: a careful software engineer working inside one repository.
You act only through the tools. Workflow:
1. Locate before reading: use `outline` on directories and files, `find` for symbols or text.
2. Read only what you need: a symbol by id (`path::Type::name`) or a line range.
3. Edit with `edit`: copy `old` exactly from what you read, without the `NNNNN| ` line prefix.
   Keep changes minimal and in the style of the surrounding code.
4. Run `check` after editing. Fix what it reports. If the summary is insufficient,
   use `read_artifact` with its log path and only the lines needed.
5. Call `finish` with a short summary: what you changed and how you verified it.
   If you cannot make progress, call `finish` and say why. Do not stop without `finish`.";

#[derive(Clone, Debug)]
pub struct WorkerConfig {
    pub need: Need,
    pub max_steps: usize,
    /// Wall-clock budget; the worker stops (and no single model call waits) beyond it.
    pub max_seconds: Option<u64>,
    /// JSONL trace of every step.
    pub trace: Option<PathBuf>,
}

#[derive(Debug, Default, Serialize)]
pub struct Outcome {
    pub finished: bool,
    pub summary: String,
    pub steps: usize,
    pub edits: Vec<String>,
    pub models: BTreeMap<String, u32>,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub phase_bytes: BTreeMap<String, u64>,
    pub seconds: u64,
    pub stop_reason: String,
}

fn phase_name(phase: Phase) -> &'static str {
    match phase {
        Phase::Orient => "orient",
        Phase::Validate => "validate",
        Phase::Edit => "edit",
        Phase::Message => "message",
    }
}

pub fn run(
    task: &str,
    workspace: &mut Workspace,
    router: &Router,
    config: &WorkerConfig,
) -> Outcome {
    let started = Instant::now();
    let mut need = config.need;
    need.tools = true;
    need.max_cost = need.max_cost.min(cost_ceiling());
    let mut outcome = Outcome::default();
    let mut trace = config.trace.as_ref().and_then(|p| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
            .ok()
    });
    let mut log = |event: serde_json::Value| {
        if let Some(file) = trace.as_mut() {
            let _ = writeln!(file, "{event}");
        }
    };
    let tools = workspace.specs();
    let mut messages = vec![Message::user(task)];
    let anchor_candidates = workspace.find_anchor_candidates(task);
    if !anchor_candidates.is_empty() {
        messages.insert(
            0,
            Message::user(format!("Likely relevant code:\n{anchor_candidates}")),
        );
    }
    let mut idle = 0;
    for step in 1..=config.max_steps {
        outcome.steps = step;
        let remaining = config
            .max_seconds
            .map(|m| m.saturating_sub(started.elapsed().as_secs()));
        if remaining == Some(0) {
            outcome.stop_reason = format!("time limit ({}s)", config.max_seconds.unwrap_or(0));
            break;
        }
        let request = Request {
            system: Some(SYSTEM.into()),
            messages: messages.clone(),
            tools: tools.clone(),
            max_output_tokens: Some(4096),
            temperature: std::env::var("LOOM_TEMPERATURE")
                .ok()
                .and_then(|t| t.parse().ok()),
            timeout: Some(std::time::Duration::from_secs(
                remaining.unwrap_or(300).clamp(10, 300),
            )),
            ..Request::default()
        };
        let served = match router.complete(need, &request) {
            Ok(served) => served,
            Err(error) => {
                outcome.stop_reason = format!("no model: {error}");
                log(json!({"step": step, "error": error.to_string()}));
                break;
            }
        };
        let response = served.response;
        *outcome
            .models
            .entry(format!("{}/{}", served.candidate.endpoint, response.model))
            .or_default() += 1;
        outcome.input_tokens += response.usage.input.unwrap_or(0);
        outcome.cached_input_tokens += response.usage.cached_input.unwrap_or(0);
        outcome.output_tokens += response.usage.output.unwrap_or(0);
        let text = response.text();
        *outcome.phase_bytes.entry("message".into()).or_default() += text.len() as u64;
        messages.push(Message {
            role: Role::Assistant,
            parts: response.parts.clone(),
        });
        let calls: Vec<(String, String, serde_json::Value)> = response
            .tool_calls()
            .map(|(i, n, a)| (i.to_owned(), n.to_owned(), a.clone()))
            .collect();
        log(json!({
            "step": step, "model": response.model, "endpoint": served.candidate.endpoint,
            "usage": {"input": response.usage.input, "cached": response.usage.cached_input, "output": response.usage.output},
            "text": text.chars().take(8000).collect::<String>(),
            "calls": calls.iter().map(|(_, n, a)| json!({"tool": n, "args": a})).collect::<Vec<_>>(),
        }));
        if calls.is_empty() {
            // Some models answer in prose instead of calling finish; nudge once, then stop.
            idle += 1;
            if idle >= 2 {
                outcome.summary = text;
                outcome.stop_reason = "model stopped calling tools".into();
                break;
            }
            messages.push(Message::user(
                "Continue using the tools. When the task is done, call `finish` with a summary.",
            ));
            continue;
        }
        idle = 0;
        let mut results = Vec::new();
        for (id, name, args) in calls {
            let output = workspace.call(&name, &args);
            *outcome
                .phase_bytes
                .entry(phase_name(output.phase).into())
                .or_default() += output.text.len() as u64;
            log(
                json!({"step": step, "tool": name, "error": output.is_error, "bytes": output.text.len(),
                       "result": output.text.chars().take(1500).collect::<String>()}),
            );
            if let Some(summary) = output.finish {
                outcome.finished = true;
                outcome.summary = summary;
            }
            results.push(Part::ToolResult {
                call_id: id,
                output: output.text,
                is_error: output.is_error,
            });
        }
        messages.push(Message {
            role: Role::User,
            parts: results,
        });
        if outcome.finished {
            outcome.stop_reason = "finished".into();
            break;
        }
    }
    if outcome.stop_reason.is_empty() {
        outcome.stop_reason = format!("step limit ({})", config.max_steps);
    }
    outcome.edits = workspace.edits.clone();
    outcome.seconds = started.elapsed().as_secs();
    log(json!({"outcome": &outcome}));
    outcome
}

/// `custom-tui --run TASK`: run one task headlessly in `cwd` and print the outcome.
#[allow(clippy::too_many_arguments)]
pub fn cli_run(
    task: &str,
    cwd: &std::path::Path,
    need: Need,
    max_steps: usize,
    max_seconds: Option<u64>,
    read_only: bool,
    trace: Option<PathBuf>,
    summary_file: Option<PathBuf>,
) -> anyhow::Result<bool> {
    if need.max_cost > cost_ceiling() {
        anyhow::bail!(
            "requested cost exceeds provider policy; configure loom provider policy first"
        );
    }
    let task = if task == "-" {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)?;
        text
    } else {
        task.to_owned()
    };
    let mut workspace = Workspace::open(cwd)?;
    workspace.read_only = read_only;
    let (router, sources) = super::model::discover::discover(super::model::cli::ledger_path());
    let usable: Vec<_> = router
        .plan(Need {
            tools: true,
            ..need
        })
        .into_iter()
        .filter(|(_, r)| r.is_none())
        .map(|(c, _)| format!("{}/{}", c.endpoint, c.model))
        .collect();
    if usable.is_empty() {
        let why: Vec<String> = sources
            .iter()
            .filter_map(|s| s.status.as_ref().err().map(|e| format!("{}: {e}", s.name)))
            .collect();
        anyhow::bail!(
            "no usable model within {:?} (tier ≥ {:?}). {}",
            need.max_cost,
            need.min_tier,
            why.join("; ")
        );
    }
    eprintln!(
        "loom worker · {} · candidates: {}",
        workspace.root().display(),
        usable
            .iter()
            .take(4)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    );
    let outcome = run(
        &task,
        &mut workspace,
        &router,
        &WorkerConfig {
            need,
            max_steps,
            max_seconds,
            trace,
        },
    );
    eprintln!(
        "{} after {} steps in {}s · {} · input {} (cached {}) output {} · edits: {}",
        outcome.stop_reason,
        outcome.steps,
        outcome.seconds,
        outcome
            .models
            .iter()
            .map(|(m, n)| format!("{m}×{n}"))
            .collect::<Vec<_>>()
            .join(", "),
        outcome.input_tokens,
        outcome.cached_input_tokens,
        outcome.output_tokens,
        if outcome.edits.is_empty() {
            "none".into()
        } else {
            outcome.edits.join(", ")
        },
    );
    eprintln!("context bytes by phase: {:?}", outcome.phase_bytes);
    println!("{}", outcome.summary);
    if let Some(path) = summary_file {
        std::fs::write(path, &outcome.summary)?;
    }
    Ok(outcome.finished)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::router::{Candidate, CostClass, Tier};
    use crate::core::model::{ErrorKind, Provider, ProviderError, Response, Stop, Usage};
    use std::{collections::VecDeque, sync::Mutex, time::Duration};

    struct Script(Mutex<VecDeque<Vec<Part>>>);
    impl Provider for Script {
        fn name(&self) -> &str {
            "local"
        }
        fn complete(&self, request: &Request) -> Result<Response, ProviderError> {
            let parts = self
                .0
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| ProviderError::new(ErrorKind::Transient, "script ended"))?;
            Ok(Response {
                parts,
                usage: Usage {
                    input: Some(100),
                    cached_input: Some(50),
                    output: Some(10),
                },
                stop: Stop::ToolUse,
                model: request.model.clone(),
                latency: Duration::ZERO,
                route: None,
                gateway_connection: None,
                gateway_upstream_ms: None,
            })
        }
        fn list_models(&self) -> Result<Vec<String>, ProviderError> {
            Ok(vec![])
        }
    }

    fn call(name: &str, args: serde_json::Value) -> Part {
        Part::ToolCall {
            id: format!("c-{name}"),
            name: name.into(),
            arguments: args,
        }
    }

    #[test]
    fn worker_reads_edits_and_finishes_through_core_tools() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.txt"), "status: todo\n").unwrap();
        let mut ws = Workspace::open(dir.path()).unwrap();
        let script = Script(Mutex::new(VecDeque::from(vec![
            vec![call("read", json!({"path": "notes.txt"}))],
            vec![call(
                "edit",
                json!({"path": "notes.txt", "old": "todo", "new": "done"}),
            )],
            vec![
                Part::Text("done".into()),
                call("finish", json!({"summary": "marked done"})),
            ],
        ])));
        let mut router = Router::new(None);
        router.add(
            Box::new(script),
            vec![Candidate {
                endpoint: "local".into(),
                model: "m".into(),
                tier: Tier::Small,
                cost: CostClass::Local,
                price: None,
                tools: true,
                context: None,
            }],
        );
        let outcome = run(
            "mark it done",
            &mut ws,
            &router,
            &WorkerConfig {
                need: Need::default(),
                max_steps: 10,
                max_seconds: None,
                trace: None,
            },
        );
        assert!(outcome.finished, "{outcome:?}");
        assert_eq!(outcome.summary, "marked done");
        assert_eq!(outcome.edits, ["notes.txt"]);
        assert_eq!(outcome.input_tokens, 300);
        assert!(outcome.phase_bytes["orient"] > 0 && outcome.phase_bytes["edit"] > 0);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("notes.txt")).unwrap(),
            "status: done\n"
        );
    }

    #[test]
    fn paid_models_are_unreachable_even_when_asked_for() {
        let dir = tempfile::tempdir().unwrap();
        let mut ws = Workspace::open(dir.path()).unwrap();
        let mut router = Router::new(None);
        router.add(
            Box::new(Script(Mutex::new(VecDeque::from(vec![vec![call(
                "finish",
                json!({"summary": "x"}),
            )]])))),
            vec![Candidate {
                endpoint: "local".into(),
                model: "paid-model".into(),
                tier: Tier::Flagship,
                cost: CostClass::Paid,
                price: Some((3.0, 15.0)),
                tools: true,
                context: None,
            }],
        );
        let need = Need {
            max_cost: CostClass::Unknown,
            ..Need::default()
        };
        let outcome = run(
            "anything",
            &mut ws,
            &router,
            &WorkerConfig {
                need,
                max_steps: 3,
                max_seconds: None,
                trace: None,
            },
        );
        assert!(!outcome.finished);
        assert!(
            outcome.stop_reason.starts_with("no model"),
            "{}",
            outcome.stop_reason
        );
    }
}
