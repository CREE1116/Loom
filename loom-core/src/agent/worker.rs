//! One model/tool loop for both direct and scoped workers. No session/apply ownership.
use crate::agent::{
    Options,
    events::{Event, Observer, Stage},
    orchestration,
    tools::{self, Host},
    world::WorldHub,
};
use anyhow::{Result, ensure};
use crate::{
    effects::{Overlay, Patch, SandboxMode},
    instructions::Instructions,
    models::{cost_ceiling, Message, ModelPort, Need, Part, Request, Role, Stop, Usage},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs::File,
    io::Write,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Metrics {
    pub calls: usize,
    pub usage: Usage,
}
#[derive(Clone, Debug)]
pub struct Proposal {
    pub patch: Patch,
    pub reads: BTreeSet<PathBuf>,
    pub summary: String,
    pub complete: bool,
    pub delegated: bool,
}
pub struct Shared<'a> {
    pub run_id: String,
    pub task_id: String,
    pub hooks: crate::hooks::Hooks,
    pub root: &'a std::path::Path,
    pub model: &'a dyn ModelPort,
    pub options: &'a Options,
    pub hub: WorldHub,
    pub cancel: Arc<AtomicBool>,
    pub observe: &'a Observer<'a>,
    pub started: Instant,
    calls: AtomicUsize,
    metrics: Mutex<Metrics>,
    log: Mutex<File>,
    log_error: Mutex<Option<String>>,
}
impl<'a> Shared<'a> {
    pub fn new(
        root: &'a std::path::Path,
        model: &'a dyn ModelPort,
        options: &'a Options,
        hub: WorldHub,
        cancel: Arc<AtomicBool>,
        observe: &'a Observer<'a>,
        log: File,
    ) -> Self {
        Self {
            task_id: String::new(),
            hooks: Default::default(),
            run_id: format!(
                "{:x}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ),
            root,
            model,
            options,
            hub,
            cancel,
            observe,
            started: Instant::now(),
            calls: AtomicUsize::new(0),
            metrics: Mutex::new(Metrics {
                calls: 0,
                usage: Usage {
                    input: Some(0),
                    cached_input: Some(0),
                    output: Some(0),
                },
            }),
            log: Mutex::new(log),
            log_error: Mutex::new(None),
        }
    }
    pub fn remaining(&self) -> Duration {
        Duration::from_secs(self.options.seconds).saturating_sub(self.started.elapsed())
    }
    pub fn run_hooks(&self, event: crate::hooks::HookEvent, id: &str, overlay: &Overlay, mut input: Value, policy: crate::effects::ExecutionPolicy) -> Result<crate::hooks::HookOutcome> {
        input["run_id"] = json!(self.run_id);
        input["task_id"] = json!(self.task_id);
        input["workspace"] = json!(self.root);
        input["session_id"] = json!(self.options.session_id.as_deref().unwrap_or(&self.run_id));
        input["model"] = json!(self.options.model.as_deref().unwrap_or("auto"));
        self.hooks.run(crate::hooks::Invocation {
            event, worker: id, directory: overlay.dir(), input, policy,
            cancel: &self.cancel, remaining: self.worker_time(), observe: &|event| self.emit(event),
        }, overlay.artifacts())
    }
    pub fn worker_time(&self) -> Duration {
        self.remaining().saturating_sub(Duration::from_secs(
            self.options
                .validation_reserve
                .min(self.options.seconds / 4),
        ))
    }
    pub fn stopped(&self) -> bool {
        self.cancel.load(Ordering::Acquire) || self.worker_time().is_zero()
    }
    /// Deliver a transient event to the surface without recording it.
    pub fn show(&self, event: Event) {
        (self.observe)(event);
    }
    pub fn emit(&self, event: Event) {
        // Append under the lock, render only after releasing it.
        let result = (|| -> Result<()> {
            let mut file = self
                .log
                .lock()
                .map_err(|_| anyhow::anyhow!("event log lock poisoned"))?;
            serde_json::to_writer(&mut *file, &event)?;
            file.write_all(b"\n")?;
            Ok(())
        })();
        if let Err(error) = result {
            *self.log_error.lock().unwrap() = Some(error.to_string());
        }
        if !self.options.inquiry {
            self.options.control.observe(&event);
        }
        (self.observe)(event);
    }
    pub fn flush(&self) -> Result<()> {
        if let Some(error) = self.log_error.lock().unwrap().as_ref() {
            anyhow::bail!("event log failed: {error}");
        }
        self.log.lock().unwrap().sync_all()?;
        Ok(())
    }
    pub fn metrics(&self) -> Metrics {
        self.metrics.lock().unwrap().clone()
    }
    pub fn complete(&self, id: &str, request: &Request) -> Result<crate::models::Response> {
        use crate::models::ErrorKind;
        let attempts = self.options.model_attempts.clamp(1, 5);
        let mut attempt = 0;
        let served = loop {
            ensure!(!self.stopped(), "cancelled or deadline reached");
            let m = self.metrics();
            ensure!(
                self.options.tokens == 0 || m.usage
                    .input
                    .unwrap_or(0)
                    .saturating_add(m.usage.output.unwrap_or(0))
                    < self.options.tokens,
                "observed token budget exhausted"
            );
            self.calls
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                    (n < self.options.steps).then_some(n + 1)
                })
                .map_err(|_| anyhow::anyhow!("shared model-call budget exhausted"))?;
            attempt += 1;
            let mut request = request.clone();
            request.timeout = Some(
                request
                    .timeout
                    .unwrap_or(self.worker_time())
                    .min(self.worker_time()),
            );
            self.metrics.lock().unwrap().calls = self.calls.load(Ordering::SeqCst);
            self.emit(Event::new(
                id,
                Stage::Run,
                "model",
                json!({
                    "request":request.messages, "model":request.model, "attempt":attempt,
                    "max_attempts":attempts, "timeout_seconds":request.timeout.unwrap().as_secs(),
                    "reasoning_effort":request.reasoning_effort, "tools":request.tools.len()
                }),
            ));
            let mut partial = false;
            let result = self.model.complete_stream(Need {
                tools: !request.tools.is_empty(), max_cost: cost_ceiling(), ..Default::default()
            }, &request, &mut |event| {
                if let crate::models::StreamEvent::Speed { tokens, seconds } = &event
                    && *seconds > 0.0
                {
                    self.show(Event::new(id, Stage::Answer, "speed", json!({"tokens_per_second": *tokens as f64 / seconds})));
                }
                if let crate::models::StreamEvent::TextDelta(text) = &event
                    && !text.is_empty()
                {
                    partial = true;
                    // Shown live; the event log keeps only the final reply.
                    self.show(Event::new(id, Stage::Answer, "stream", json!({"text": text})));
                }
            });
            match result {
                Ok(served) => break served,
                Err(error) => {
                    let wait = error
                        .retry_after
                        .unwrap_or(Duration::from_millis(500 * attempt as u64));
                    let can_retry =
                        matches!(error.kind, ErrorKind::Transient | ErrorKind::TimedOut | ErrorKind::RateLimited)
                            && !partial
                            && attempt < attempts
                            && !self.stopped()
                            && self.calls.load(Ordering::SeqCst) < self.options.steps
                            && self.worker_time() > wait + Duration::from_secs(1);
                    if !can_retry {
                        let mut event = Event::new(
                            id,
                            Stage::Error,
                            "model_error",
                            json!({
                                "reason":error.to_string(), "attempt":attempt, "max_attempts":attempts
                            }),
                        );
                        event.failed = true;
                        self.emit(event);
                        return Err(error.into());
                    }
                    self.emit(Event::new(id, Stage::Run, "model_retry", json!({
                        "reason":error.to_string(), "attempt":attempt, "next_attempt":attempt + 1,
                        "max_attempts":attempts, "wait_seconds":wait.as_secs_f64()
                    })));
                    let until = Instant::now() + wait;
                    while Instant::now() < until {
                        ensure!(!self.stopped(), "cancelled during model retry wait");
                        std::thread::sleep(
                            Duration::from_millis(20)
                                .min(until.saturating_duration_since(Instant::now())),
                        );
                    }
                }
            }
        };
        ensure!(
            served.candidate.cost <= cost_ceiling(),
            "model exceeded cost ceiling"
        );
        let mut m = self.metrics.lock().unwrap();
        m.usage.input = m
            .usage
            .input
            .zip(served.response.usage.input)
            .map(|(a, b)| a.saturating_add(b));
        m.usage.cached_input = m
            .usage
            .cached_input
            .zip(served.response.usage.cached_input)
            .map(|(a, b)| a.saturating_add(b));
        m.usage.output = m
            .usage
            .output
            .zip(served.response.usage.output)
            .map(|(a, b)| a.saturating_add(b));
        drop(m);
        self.emit(Event::new(
            id,
            Stage::Run,
            "model_result",
            serde_json::to_value(&served.response)?,
        ));
        Ok(served.response)
    }
}

fn guidance(shared: &Shared<'_>, id: &str, goal: &str, instructions: &mut Instructions) -> Result<String> {
    let mut guidance = instructions.root_guidance()?;
    guidance.push_str(&shared.hooks.guidance());
    // Skills: the main worker sees them; delegated children get their goal only.
    if id == "main" {
        guidance.push_str(&crate::instructions::skill_guidance(shared.root, goal, &shared.options.skills, "load each relevant skill with use_skill before following it")?);
    }
    for path in instructions.loaded_paths() {
        shared.emit(Event::new(
            id,
            Stage::Source,
            path.display().to_string(),
            json!({"kind":"instructions", "completed":true}),
        ));
    }
    Ok(guidance)
}

/// Every caller supplies a freshly created overlay (main, DAG branch, or stale retry).
/// Subsequent mutations refresh through Host::refresh, which captures live content.
pub fn run(
    shared: &Shared<'_>,
    id: &str,
    goal: &str,
    overlay: &mut Overlay,
    scope: Option<BTreeSet<PathBuf>>,
    plan_mode: bool,
) -> Result<Proposal> {
    if shared.stopped() {
        overlay.defer_cleanup();
        anyhow::bail!("cancelled or deadline reached");
    }
    let readonly = shared.options.read_only
        || shared.options.policy.sandbox == SandboxMode::ReadOnly
        || scope.as_ref().is_some_and(|s| s.is_empty());
    let child = scope.is_some();
    let mut instructions = Instructions::open(shared.root)?;
    let mut guidance = guidance(shared, id, goal, &mut instructions)?;
    let mut lifecycle_policy = shared.options.policy;
    if readonly || child { lifecycle_policy.sandbox = SandboxMode::ReadOnly; }
    use crate::hooks::HookEvent;
    let mut startup_events = Vec::new();
    if child { startup_events.push((HookEvent::SubagentStart, json!({"agent_id":id,"agent_type":"worker","prompt":goal}))); }
    else if !shared.options.inquiry {
        if let Some(source) = &shared.options.session_start { startup_events.push((HookEvent::SessionStart,json!({"source":source}))); }
        startup_events.push((HookEvent::UserPromptSubmit,json!({"prompt":goal})));
    }
    let mut startup_context = Vec::new();
    for (event, input) in startup_events {
        let outcome = shared.run_hooks(event,id,overlay,input,lifecycle_policy)?;
        if let Some(reason) = outcome.halt { anyhow::bail!("{reason}"); }
        startup_context.extend(outcome.context);
    }
    if !startup_context.is_empty() {guidance.push_str(&format!("\nHook context:\n{}",tools::clip(&startup_context.join("\n"),16000)));}
    shared.emit(Event::new(
        id,
        Stage::Direct,
        "preparation",
        json!({"phase":"index", "started":true}),
    ));
    if shared.stopped() {
        overlay.defer_cleanup();
        anyhow::bail!("cancelled or deadline reached");
    }
    let mut host = Host {
        view: shared.hub.initial_view(overlay, &shared.cancel)?,
        sets: Default::default(),
        hub: shared.hub.clone(),
        overlay,
        scope,
        readonly,
        // /plan starts like a normal worker: it plans first, then delegates only when
        // independent parts benefit from agents, otherwise it does the work itself.
        orchestrating: false,
        checks: shared.options.checks.clone(),
        policy: shared.options.policy,
        instructions,
        extensions: None,
    };
    // Extensions are configured per workspace; without a config there is nothing to start.
    if shared.root.join(".loom/config.toml").is_file() {
        host.extensions = crate::extensions::Extensions::open_for_workspace(shared.root, host.overlay.dir(), shared.options.policy).ok();
    }
    if id == "main" && !shared.options.inquiry {
        shared.options.control.publish(
            host.view.world.snapshot.revision.clone(),
            goal,
            if plan_mode { "planning" } else { "direct" },
        );
    }
    // Entry names are index metadata, not implicit open/read tool results.
    let entries = host
        .view
        .nodes
        .iter()
        .filter(|node| {
            matches!(
                node,
                crate::agent::world::Node::File(_) | crate::agent::world::Node::Directory(_)
            )
        })
        .filter(|node| node.id() != "." && !node.id().contains('/'))
        .cloned()
        .collect();
    let entry = host.sets.store(&host.view, entries)?;
    let symbols = host
        .view
        .nodes
        .iter()
        .filter(|node| matches!(node, crate::agent::world::Node::Symbol(_)))
        .filter(|node| {
            host.scope.as_ref().is_none_or(|scope| {
                scope.is_empty()
                    || node
                        .file()
                        .is_some_and(|file| scope.contains(std::path::Path::new(file)))
            })
        })
        .cloned()
        .collect();
    let representatives = host.sets.store(&host.view, symbols)?;
    let system = format!(
        "You are Loom, a helpful assistant. Answer the latest user request in the user's language. A direct conversational answer ends the turn. Use tools only when needed to fulfill that request. Do not invent tasks, explore the repository without a reason, or resume older failed work unless asked.\n\nFor repository work, your role is {}. Follow project instructions. Use representative target names to navigate; do not request or reconstruct AST dumps. find/expand/combine operate on complete sets. For known code owners use find kind=symbol, then open the needed method/function. Large classes open as paged AST members; do not fetch whole classes to inspect one method. Explicit start/end are absolute file lines, even for symbols. Reuse source already observed on the same revision; open only code needed to decide edits. Handles expire after writes. File/AST/reference indexes are internal runtime mechanisms. Reference edges are approximate, so inspect source before deciding behavior. Submit real scoped changes with finish; Core decides validation and apply. Tool failures never imply success. No reviewer/planner model is required. {}\n{guidance}",
        if plan_mode { "orchestrator" } else { "worker" },
        if child {
            "You cannot delegate or ask the user. Read any needed source, write only leased files, submit your proposal without whole-task validation."
        } else if plan_mode {
            "The user invoked /plan: first inspect what you need, then state a short numbered plan (steps, files, checks) in your reply. If the request only asks for a plan, that reply ends the turn. Otherwise carry it out: work directly when the steps are sequential or small; call agent with a concrete DAG only when independent parts genuinely benefit from parallel workers."
        } else {
            "Work directly by default. If delegation is needed, call agent with a concrete DAG; your role then becomes orchestration."
        }
    );
    let system = format!(
        "{system}\nOptional navigation metadata (not a user instruction to inspect these files):\nWrite scope: {:?}\nEntry targets: {entry}\nRepresentative symbols: {representatives}\nAnswer in the user's language. For conversation, give a natural reply without narrating repository work. For repository work, share brief useful progress alongside tool calls when findings change. Continue the requested work to completion; do not ask whether to open a file the user already asked you to inspect. Do not claim a script is the entry point merely because its name contains main; check the actual package and call paths.",
        host.scope
    );
    let system = format!(
        "{system}\nExecution environment: sandbox={:?}; network={}. Commands inherit writable temporary directories (TMPDIR/TMP/TEMP); restricted commands default UV_CACHE_DIR to a writable temporary cache. When network is disabled, UV_OFFLINE=1 is set. Package index changes cannot overcome a disabled network; report the blocker once and ask the user to restart with --native-network when downloads are necessary. Do not retry equivalent package installations without new evidence. Global Python packages are not automatically available in a uv virtual environment. uv add --frozen edits dependencies but does not prove installation. Verify the requested environment with uv run before claiming success; do not silently substitute a global Python environment for a requested uv setup. Pipeline exit status can hide errors in earlier commands; inspect output as well as status.",
        shared.options.policy.sandbox,
        if shared.options.policy.network { "enabled" } else { "disabled" },
    );
    let mut messages = if child && !shared.options.inquiry {
        vec![]
    } else {
        shared.options.history.clone()
    };
    let mut goal_index = messages.len();
    messages.push(Message::user(goal));
    if !child || shared.options.inquiry {
        ensure!(shared.options.images.iter().all(|part| matches!(part, Part::Image { .. })), "attachments must be image parts");
        messages[goal_index].parts.extend(shared.options.images.clone());
    }
    let original_goal = messages[goal_index].text();
    let original_images = messages[goal_index].parts.iter()
        .filter(|part| matches!(part, Part::Image { .. })).cloned().collect::<Vec<_>>();
    let mut compact_receipts = std::collections::VecDeque::new();
    let mut pending_failure = false;
    let mut complete = false;
    let mut delegated = false;
    let mut failed_delegation = false;
    let mut finished = false;
    let mut summary = "worker did not finish".to_owned();
    let mut recent_calls = std::collections::VecDeque::new();
    let mut recent_revision = String::new();
    let mut repeated = 0;
    let mut unusable = 0;
    let mut connection = None;
    let mut exploring = 0usize;
    let mut last_call = Duration::ZERO;
    let mut stop_continuations = 0usize;
    let mut hook_context = Vec::<String>::new();
    while !shared.stopped() {
        // Near a configured limit, ask for the answer instead of letting the next tool
        // call run out the budget and lose everything gathered so far.
        let calls_left = shared
            .options
            .steps
            .saturating_sub(shared.calls.load(Ordering::SeqCst));
        // Tiny limits are deliberate (a single scripted step), so leave those alone.
        let wrap_up = shared.options.steps >= 8 && calls_left <= 1
            || !last_call.is_zero()
                && shared.worker_time() < last_call * 2 + Duration::from_secs(1);
        let mut available = tools::specs(!child, readonly || host.orchestrating);
        available.extend(host.extension_tools());
        if child {
            available.retain(|t| t.name != "ask");
        }
        if wrap_up {
            available.clear();
        }
        let mut request = Request {
            model: shared.options.model.clone().unwrap_or_default(),
            system: Some(format!(
                "{system}\nHook context:\n{}\nCurrent role: {}{}",
                tools::clip(&hook_context.join("\n"),16000),
                if host.orchestrating {
                    "orchestration; coordinate the DAG and its blockers"
                } else if shared.options.inquiry {
                    "read-only parallel inquiry; main task continues independently"
                } else {
                    "assistant; answer the current request, perform repository work only when needed"
                },
                if wrap_up {
                    "\nThe task limit is almost used up. Do not call tools. Give your final answer now from the evidence above and say briefly what you could not verify."
                } else {
                    ""
                }
            )),
            messages: messages.clone(),
            tools: available,
            max_output_tokens: Some(8192),
            reasoning_effort: shared.options.reasoning_effort.clone(),
            cancel: Some(shared.cancel.clone()),
            timeout: Some(
                shared
                    .worker_time()
                    .min(Duration::from_secs(shared.options.call_seconds)),
            ),
            gateway_connection: connection.clone(),
            ..Default::default()
        };
        let context_limit = shared.model.limits().into_iter()
            .filter(|limit| limit.tools && limit.disabled.is_none())
            .filter(|limit| request.model.is_empty() || request.model == limit.model
                || request.model == format!("{}/{}", limit.endpoint, limit.model))
            .filter_map(|limit| limit.context).min();
        if let Some(limit) = context_limit {
            fit_context(&mut request, goal_index, limit);
        }
        let asked = Instant::now();
        let response = match shared.complete(id, &request) {
            Ok(r) => r,
            Err(e) => {
                summary = e.to_string();
                break;
            }
        };
        connection = response.gateway_connection.clone();
        last_call = asked.elapsed();
        if shared.stopped() {
            summary = "cancelled or deadline reached".into();
            break;
        }
        let calls: Vec<_> = response
            .tool_calls()
            .map(|(cid, n, a)| (cid.to_owned(), n.to_owned(), a.clone()))
            .collect();
        if calls.is_empty() {
            if (response.stop == Stop::EndTurn || wrap_up)
                && !response.text().trim().is_empty()
                && !failed_delegation
                && !pending_failure
            {
                summary = crate::agent::conversation::without_receipts(&response.text());
                let outcome = stop_hooks(shared,id,host.overlay,&summary,stop_continuations > 0,child,lifecycle_policy)?;
                hook_context.extend(outcome.context);
                if let Some(reason) = outcome.halt { summary=reason; complete=false; break; }
                if outcome.continue_work {
                    if stop_continuations >= 3 { summary="Stop hook continuation limit reached".into(); break; }
                    stop_continuations += 1;
                    messages.push(Message {role:Role::Assistant,parts:response.parts});
                    messages.push(Message::user(outcome.feedback.unwrap_or_else(|| "Continue as requested by the Stop hook.".into())));
                    if lifecycle_policy.sandbox != SandboxMode::ReadOnly {host.refresh(&shared.cancel)?;}
                    continue;
                }
                complete = true;
                break;
            }
            // Re-asking a model that keeps answering with nothing usable only spins
            // under "waiting for the model"; stop and say what came back.
            unusable += 1;
            if unusable >= 2 {
                summary = if response.parts.is_empty() {
                    "the model returned empty replies twice; no text or tool call could be read".into()
                } else {
                    format!("the model replied twice without a usable tool call (stop: {:?})", response.stop)
                };
                break;
            }
            messages.push(Message {
                role: Role::Assistant,
                parts: response.parts,
            });
            messages.push(Message::user("Provide a usable tool call or finish. Truncated or empty output cannot finish."));
            continue;
        }
        unusable = 0;
        let commentary = response.text();
        if !commentary.trim().is_empty() {
            shared.emit(Event::new(id, Stage::Answer, "commentary", json!({"text":commentary})));
        }
        // A repeat is the same calls on the same revision returning the same results, so
        // reading further ranges or pages of one target is progress, not repetition.
        let revision = host.view.revision().to_owned();
        let call_key = serde_json::to_string(&calls.iter().map(|(_, n, a)| (n, a)).collect::<Vec<_>>())?;
        let calls_names: Vec<String> = calls.iter().map(|(_, n, _)| n.clone()).collect();
        messages.push(Message {
            role: Role::Assistant,
            parts: response.parts,
        });
        let mut results = Vec::new();
        let mut evidence = Vec::new();
        let mut failed = false;
        for (cid, name, mut args) in calls {
            if shared.stopped() {
                summary = "cancelled or deadline reached".into();
                break;
            }
            let stage = match name.as_str() {
                "find" | "expand" | "combine" | "project" => Stage::Navigation,
                "open" | "use_skill" => Stage::Source,
                "change" => Stage::Change,
                "agent" => Stage::Orchestrating,
                _ => Stage::Run,
            };
            let available = request.tools.iter().any(|tool| tool.name == name);
            let hook_policy = lifecycle_policy;
            let pre_hook = if available && shared.hooks.has(HookEvent::PreToolUse, &name) {
                shared.run_hooks(HookEvent::PreToolUse, id, host.overlay,
                    json!({"tool_name":name, "tool_input":args,"tool_use_id":cid}), hook_policy)
                    .and_then(|outcome| {
                        if let Some(reason) = outcome.halt { anyhow::bail!("{reason}"); }
                        if let Some(updated) = outcome.updated_input { args = updated; }
                        hook_context.extend(outcome.context);
                        if hook_policy.sandbox != SandboxMode::ReadOnly { host.refresh(&shared.cancel)?; }
                        Ok(())
                    })
            } else { Ok(()) };
            let allowed = available && pre_hook.is_ok();
            let target = args["target"].as_str().or(args["query"].as_str())
                .or(args["command"].as_str()).unwrap_or(&name).to_owned();
            shared.emit(Event::new(
                id,
                stage,
                target.clone(),
                json!({"tool":name,"args":args,"started":true}),
            ));
            let result: Result<Value> = if let Err(error) = pre_hook {
                Err(error)
            } else if !available {
                Err(anyhow::anyhow!("unavailable tool {name}"))
            } else if name == "finish" {
                if failed || pending_failure && !args["unresolved"].as_bool().unwrap_or(false) {
                    Err(anyhow::anyhow!(
                        "a tool failed in this batch; resolve the failure before finishing"
                    ))
                } else if failed_delegation && !args["unresolved"].as_bool().unwrap_or(false) {
                    Err(anyhow::anyhow!(
                        "DAG has unresolved work; repair delegation or finish unresolved"
                    ))
                } else {
                    finished = true;
                    summary = crate::agent::conversation::without_receipts(args["summary"].as_str().unwrap_or(""));
                    complete = !args["unresolved"].as_bool().unwrap_or(false)
                        && !summary.trim().is_empty();
                    Ok(json!({"proposal":complete}))
                }
            } else if name == "agent" {
                let plan = serde_json::from_value::<orchestration::Plan>(args.clone());
                match plan {
                    Ok(plan) => match plan.validate() {
                        Ok(()) => {
                            host.orchestrating = true;
                            delegated = true;
                            let outcome = orchestration::execute(shared, &plan, host.overlay);
                            host.refresh(&shared.cancel)?;
                            outcome
                        }
                        Err(e) => Err(e),
                    },
                    Err(e) => Err(e.into()),
                }
            } else if name == "ask" {
                shared
                    .options
                    .control
                    .ask(id, args["question"].as_str().unwrap_or(""), shared)
            } else {
                host.call(&name, &args, &shared.cancel, shared.worker_time())
            };
            if id == "main"
                && !shared.options.inquiry
                && matches!(name.as_str(), "change" | "run" | "agent")
            {
                shared.options.control.publish(
                    host.view.world.snapshot.revision.clone(),
                    goal,
                    if host.orchestrating {
                        "orchestrating"
                    } else {
                        "direct"
                    },
                );
            }
            let (value, error) = match result {
                Ok(v) => {
                    let command_failed = name == "run"
                        && v.as_array()
                            .is_some_and(|results| results.iter().any(|r| r["status"] != "Passed"));
                    (v, command_failed)
                }
                Err(e) => (json!({"error":e.to_string()}), true),
            };
            if name == "agent" {
                failed_delegation = error;
            }
            failed |= error;
            if name != "finish" {
                pending_failure = error;
            }
            let receipt = format!(
                "{name} · {target} · {} · {}",
                if error { "failed" } else { "observed" },
                tools::clip(&value.to_string(), 1200)
            );
            if name == "open" && !error {
                compact_receipts.retain(|previous| previous != &receipt);
            }
            compact_receipts.push_back(receipt);
            while compact_receipts.len() > 32 {
                compact_receipts.pop_front();
            }
            let mut event = Event::new(
                id,
                stage,
                target,
                json!({"tool":name,"args":args,"result":value,"completed":true}),
            );
            event.failed = error;
            shared.emit(event);
            let mut post_feedback = None;
            let post_warning = if allowed && shared.hooks.has(HookEvent::PostToolUse, &name) {
                let result = shared.run_hooks(HookEvent::PostToolUse, id, host.overlay,
                    json!({"tool_name":name,"tool_input":args,"tool_use_id":cid,"tool_response":value,"tool_failed":error}),hook_policy);
                if hook_policy.sandbox != SandboxMode::ReadOnly { host.refresh(&shared.cancel)?; }
                match result {
                    Ok(outcome) => {hook_context.extend(outcome.context); post_feedback=outcome.feedback; None}
                    Err(error) => Some(format!("\nPostToolUse hook failed; the tool already ran, do not replay it: {error}")),
                }
            } else { None };
            evidence.push(repetition_output(&name, &value));
            let deduplicate_source = name == "open" && !error && post_feedback.is_none() && post_warning.is_none();
            let output = post_feedback.map(|feedback| format!("PostToolUse feedback (tool already ran; do not replay): {feedback}")).unwrap_or_else(|| value.to_string());
            let output = post_warning.map(|warning| format!("{warning}\n{output}")).unwrap_or(output);
            // Preserve full durable events above, but keep only the latest identical
            // source result in model context. Repeated open calls must not multiply
            // the same source in every later input. The newest copy remains verbatim.
            if deduplicate_source {
                for message in &mut messages {
                    for part in &mut message.parts {
                        if let Part::ToolResult { output: previous, .. } = part
                            && (previous == &output || previous.strip_prefix("[same call as a recent step on an unchanged repository; the result is identical. Use it, try a different query or range, or finish.]\n") == Some(output.as_str()))
                        {
                            *previous = format!("Identical source is retained in the latest open result ({cid}).");
                        }
                    }
                }
            }
            results.push(Part::ToolResult {
                call_id: cid,
                output: tools::clip(&output, 20000),
                is_error: error,
            });
            if name == "finish" && !error {
                break;
            }
        }
        let fingerprint = (revision, call_key, evidence.join("\u{0}"));
        if recent_revision != fingerprint.0 {
            recent_calls.clear();
            recent_revision.clone_from(&fingerprint.0);
        }
        if recent_calls.contains(&fingerprint) {
            repeated += 1;
        } else {
            repeated = 0;
        }
        recent_calls.push_back(fingerprint);
        while recent_calls.len() > 8 {
            recent_calls.pop_front();
        }
        let navigation_only = calls_names
            .iter()
            .all(|n| matches!(n.as_str(), "find" | "expand" | "combine" | "project" | "open"));
        exploring = if navigation_only { exploring + 1 } else { 0 };
        if exploring >= 12 && exploring % 6 == 0 {
            if let Some(Part::ToolResult { output, .. }) = results.last_mut() {
                *output = format!(
                    "{output}\n[{exploring} steps of reading without changing or running anything. If the evidence already answers the request, answer now; otherwise take the next concrete step.]"
                );
            }
        }
        if repeated > 0 {
            // Tell the model inside the results: text beside tool results breaks the
            // message order chat-completions servers require.
            for part in &mut results {
                if let Part::ToolResult { output, .. } = part {
                    *output = format!(
                        "[same call as a recent step on an unchanged repository; the result is identical. Use it, try a different query or range, or finish.]\n{output}"
                    );
                }
            }
        }
        messages.push(Message {
            role: Role::User,
            parts: results,
        });
        if finished {
            let outcome = stop_hooks(shared,id,host.overlay,&summary,stop_continuations > 0,child,lifecycle_policy)?;
            hook_context.extend(outcome.context);
            if let Some(reason) = outcome.halt {summary=reason; complete=false; break;}
            if outcome.continue_work {
                if stop_continuations >= 3 {summary="Stop hook continuation limit reached".into(); complete=false; break;}
                stop_continuations += 1;
                finished=false; complete=false;
                messages.push(Message::user(outcome.feedback.unwrap_or_else(|| "Continue as requested by the Stop hook.".into())));
                if lifecycle_policy.sandbox != SandboxMode::ReadOnly {host.refresh(&shared.cancel)?;}
                continue;
            }
            break;
        }
        if repeated >= 4 {
            summary = "repeated identical operations on unchanged revision".into();
            break;
        }
        let needs_compact = messages.len() > 5 && messages.iter().map(context_bytes).sum::<usize>() > shared.options.context_bytes;
        if needs_compact {
            let outcome = shared.run_hooks(HookEvent::PreCompact,id,host.overlay,json!({"trigger":"auto"}),lifecycle_policy)?;
            if let Some(reason)=outcome.halt {summary=reason; break;}
            hook_context.extend(outcome.context);
        }
        let mut compacted = false;
        // Keep the immutable goal and newest complete exchanges, preserving raw trace on disk.
        while messages.len() > 5
            && messages
                .iter()
                .map(context_bytes)
                .sum::<usize>()
                > shared.options.context_bytes
        {
            compacted = true;
            if goal_index >= 2 {
                messages.drain(0..2);
                goal_index -= 2;
            } else {
                messages.drain(goal_index + 1..goal_index + 3);
            }
        }
        if compacted {
            let outcome=shared.run_hooks(HookEvent::PostCompact,id,host.overlay,json!({"trigger":"auto"}),lifecycle_policy)?;
            if let Some(reason)=outcome.halt {summary=reason; break;}
            hook_context.extend(outcome.context);
            let evidence = compact_receipts
                .iter()
                .rev()
                .cloned()
                .collect::<Vec<_>>()
                .join("\n");
            messages[goal_index] = Message::user(format!(
                "{original_goal}\nCompacted observations for this objective (historical evidence; reopen current source as needed):\n{}",
                tools::clip(&evidence, 8000)
            ));
            messages[goal_index].parts.extend(original_images.clone());
        }
    }
    if shared.stopped() {
        host.overlay.defer_cleanup();
        anyhow::bail!("cancelled or deadline reached");
    }
    Ok(Proposal {
        patch: host.overlay.patch()?,
        reads: host
            .overlay
            .read_paths()
            .into_iter()
            .map(|p| {
                p.strip_prefix(host.overlay.dir())
                    .map(std::path::Path::to_owned)
                    .unwrap_or(p)
            })
            .collect(),
        summary,
        complete,
        delegated,
    })
}

fn stop_hooks(shared: &Shared<'_>, id: &str, overlay: &Overlay, summary: &str, active: bool, child: bool, policy: crate::effects::ExecutionPolicy) -> Result<crate::hooks::HookOutcome> {
    shared.run_hooks(if child {crate::hooks::HookEvent::SubagentStop} else {crate::hooks::HookEvent::Stop},id,overlay,
        json!({"last_assistant_message":summary,"stop_hook_active":active,"agent_id":id,"agent_type":"worker"}),policy)
}

// Base64 size is transport overhead, not textual model context. Vision token use
// is reported by the provider; do not compact away evidence because a PNG is large.
fn context_bytes(message: &Message) -> usize {
    message.text().len() + message.parts.iter()
        .filter(|part| !matches!(part, Part::Image { .. }))
        .map(|part| serde_json::to_vec(part).map_or(0, |bytes| bytes.len())).sum::<usize>()
}

fn fit_context(request: &mut Request, mut goal_index: usize, limit: u64) {
    let output = request.max_output_tokens.unwrap_or(8192);
    request.max_output_tokens = Some(output.min((limit / 4).min(u32::MAX as u64) as u32).max(1));
    // Keep the objective and complete latest exchanges, including matching call IDs.
    while crate::models::router::estimate_tokens(request) > limit.saturating_mul(9) / 10
        && (goal_index > 0 || request.messages.len() > 3)
    {
        if goal_index >= 2 {
            request.messages.drain(0..2);
            goal_index -= 2;
        } else if goal_index == 1 {
            request.messages.remove(0);
            goal_index = 0;
        } else if goal_index + 3 < request.messages.len() {
            request.messages.drain(goal_index + 1..goal_index + 3);
        } else {
            break;
        }
    }
}

// Only execution metadata may be ignored; source and command output are evidence.
fn repetition_output(name: &str, value: &Value) -> String {
    let mut evidence = value.clone();
    if name == "run" {
        if let Some(results) = evidence.as_array_mut() {
            for result in results {
                if let Some(object) = result.as_object_mut() {
                    object.remove("ms");
                }
            }
        }
    }
    evidence.to_string()
}

#[cfg(test)]
mod startup_tests {
    use super::*;
    use crate::{base::Revision, models::{Response, ScriptedModel}};
    use std::{fs, process::Command};

    fn check_initial_host(stop_at: Option<&str>) {
        let root = tempfile::tempdir().unwrap();
        assert!(Command::new("git").args(["init", "-q"]).arg(root.path())
            .status().unwrap().success());
        fs::write(root.path().join("a.rs"), "pub fn initial() {}\n").unwrap();
        let base = Revision::capture(root.path()).unwrap();
        let mut overlay = Overlay::create(root.path(), &base).unwrap();
        let cache = overlay.dir().join(".loom/revision-stat-cache.json");
        if cache.exists() {
            fs::remove_file(&cache).unwrap();
        }
        let model = ScriptedModel::new([Response {
            parts: if stop_at == Some("change") {
                vec![Part::ToolCall {
                    id: "create".into(), name: "change".into(),
                    arguments: json!({"op":"create", "target":"new.txt", "content":"isolated edit"}),
                }]
            } else { vec![Part::Text("hello".into())] },
            usage: Usage::default(),
            stop: Stop::EndTurn,
            model: "scripted".into(),
            latency: Duration::ZERO,
            route: None,
            gateway_connection: None,
            gateway_upstream_ms: None,
        }]);
        let options = Options::default();
        let cancel = Arc::new(AtomicBool::new(stop_at == Some("before")));
        let observe = |event: Event| {
            if stop_at.is_none() && event.target == "model" {
                assert!(!cache.exists(), "initial Host indexing recaptured the overlay before the model");
            }
            if stop_at == Some("model_result") && event.target == "model_result"
                || stop_at == Some("change") && event.stage == Stage::Change && event.detail["completed"] == true
                || stop_at == Some("preparation") && event.target == "preparation" && event.detail["phase"] == "index"
            {
                if cache.exists() {
                    fs::remove_file(&cache).unwrap();
                }
                cancel.store(true, Ordering::Release);
            }
        };
        let shared = Shared::new(root.path(), &model, &options, WorldHub::default(),
            cancel.clone(), &observe, tempfile::tempfile().unwrap());
        let result = run(&shared, "main", "hello", &mut overlay, None, false);
        if stop_at.is_some() {
            let error = result.unwrap_err();
            assert!(error.to_string().contains("cancelled"), "{error}");
            assert!(!cache.exists());
            if stop_at == Some("before") || stop_at == Some("preparation") {
                assert_eq!(shared.hub.parses(), 0, "stopped workers must not index the Host");
                assert!(model.requests().is_empty());
            } else if stop_at == Some("change") {
                assert_eq!(fs::read_to_string(overlay.dir().join("new.txt")).unwrap(), "isolated edit");
                assert!(!root.path().join("new.txt").exists());
            }
        } else {
            assert!(result.unwrap().complete);
            assert_eq!(model.requests().len(), 1);
        }
    }

    /// Small local models often repeat a lookup; they get told so and may recover, and
    /// only a long unbroken run of the same call on an unchanged repository stops them.
    fn repeat_tool(repeats: usize, name: &str, arguments: Value) -> (Proposal, Vec<String>) {
        repeat_arguments(name, vec![arguments; repeats])
    }

    fn repeat_arguments(name: &str, arguments: Vec<Value>) -> (Proposal, Vec<String>) {
        let root = tempfile::tempdir().unwrap();
        assert!(Command::new("git").args(["init", "-q"]).arg(root.path())
            .status().unwrap().success());
        fs::write(root.path().join("a.rs"), "pub fn initial() {}\n").unwrap();
        // Git status must describe source state, not files created by the first run's
        // indexing/cache refresh. Otherwise the second output is legitimately new evidence.
        fs::write(root.path().join(".gitignore"), ".loom/\n").unwrap();
        let base = Revision::capture(root.path()).unwrap();
        let mut overlay = Overlay::create(root.path(), &base).unwrap();
        let reply = |name: &str, arguments: Value| Response {
            parts: vec![Part::ToolCall { id: "c".into(), name: name.into(), arguments }],
            usage: Usage::default(),
            stop: Stop::ToolUse,
            model: "scripted".into(),
            latency: Duration::ZERO,
            route: None,
            gateway_connection: None,
            gateway_upstream_ms: None,
        };
        let model = ScriptedModel::new(
            arguments.into_iter()
                .map(|args| reply(name, args))
                .chain([reply("finish", json!({"summary":"found initial"}))]),
        );
        let options = Options::default();
        let observe = |_: Event| {};
        let shared = Shared::new(root.path(), &model, &options, WorldHub::default(),
            Arc::new(AtomicBool::new(false)), &observe, tempfile::tempfile().unwrap());
        let proposal = run(&shared, "main", "find initial", &mut overlay, None, false).unwrap();
        let outputs = model.requests().last().unwrap().messages.iter()
            .flat_map(|message| message.parts.iter())
            .filter_map(|part| match part { Part::ToolResult { output, .. } => Some(output.clone()), _ => None })
            .collect();
        (proposal, outputs)
    }

    #[test]
    fn repeated_lookups_are_flagged_before_the_worker_gives_up() {
        let (proposal, outputs) = repeat_tool(4, "find", json!({"query":"initial","kind":"symbol"}));
        assert!(proposal.complete, "{}", proposal.summary);
        assert_eq!(proposal.summary, "found initial");
        assert!(!outputs[0].contains("same call"));
        assert!(outputs[1..].iter().all(|output| output.contains("same call")), "{outputs:?}");
        let (stuck, _) = repeat_tool(8, "find", json!({"query":"initial","kind":"symbol"}));
        assert_eq!(stuck.summary, "repeated identical operations on unchanged revision");
    }

    #[test]
    fn repeated_shell_queries_stop_before_consuming_the_call_budget() {
        let (proposal, outputs) = repeat_tool(10, "run",
            json!({"kind":"command", "command":"git status --short"}));
        assert!(!proposal.complete);
        assert_eq!(proposal.summary, "repeated identical operations on unchanged revision");
        assert_eq!(outputs.len(), 4, "the fifth repeated response stops the loop: {outputs:?}");
        assert!(outputs[1..].iter().all(|output| output.contains("same call")));
    }

    #[test]
    fn alternating_success_and_invalid_calls_cannot_reset_stagnation() {
        let arguments = (0..12).map(|n| json!({"command":"git status --short",
            "kind":if n % 2 == 0 { "command" } else { "invalid" }})).collect();
        let (proposal, outputs) = repeat_arguments("run", arguments);
        assert!(!proposal.complete);
        assert_eq!(proposal.summary, "repeated identical operations on unchanged revision");
        assert!(outputs.len() <= 6, "alternating repeats must stop early");
    }

    #[test]
    fn shell_command_without_kind_is_executed_and_returns_its_output() {
        let (proposal, outputs) = repeat_tool(1, "run", json!({"command":"git status --short"}));
        assert!(proposal.complete, "{}", proposal.summary);
        assert!(outputs[0].contains("Passed"), "{outputs:?}");
        assert!(!outputs[0].contains("missing kind"));
    }

    #[test]
    fn context_fit_preserves_goal_and_latest_complete_tool_exchange() {
        let mut request = Request {
            messages: vec![Message::user("original objective")],
            max_output_tokens: Some(8192),
            ..Default::default()
        };
        for n in 0..12 {
            request.messages.push(Message { role:Role::Assistant, parts:vec![
                Part::ToolCall { id:format!("c{n}"), name:"run".into(), arguments:json!({"command":"echo evidence"}) }] });
            request.messages.push(Message { role:Role::User, parts:vec![
                Part::ToolResult { call_id:format!("c{n}"), output:"evidence ".repeat(2500), is_error:false }] });
        }
        fit_context(&mut request, 0, 16384);
        assert!(crate::models::router::estimate_tokens(&request) <= 16384 * 9 / 10);
        assert_eq!(request.messages[0].text(), "original objective");
        assert!(matches!(&request.messages.last().unwrap().parts[0],
            Part::ToolResult{call_id,..} if call_id == "c11"));
        assert!(matches!(&request.messages[request.messages.len()-2].parts[0],
            Part::ToolCall{id,..} if id == "c11"));
    }

    #[test]
    fn command_timing_is_not_new_evidence() {
        let first = json!([{"status":"Passed", "ms":10,
            "artifact_target":"artifact:same", "output":"abc commit"}]);
        let mut next = first.clone();
        next[0]["ms"] = json!(91);
        assert_eq!(repetition_output("run", &first), repetition_output("run", &next));
        next[0]["output"] = json!("def new commit");
        assert_ne!(repetition_output("run", &first), repetition_output("run", &next));
        next = first.clone();
        next[0]["status"] = json!("Failed");
        assert_ne!(repetition_output("run", &first), repetition_output("run", &next));
        next = first.clone();
        next[0]["artifact_target"] = json!("artifact:changed");
        assert_ne!(repetition_output("run", &first), repetition_output("run", &next));
        let source = json!({"ms":10});
        assert_ne!(repetition_output("open", &source), repetition_output("open", &json!({"ms":91})));
    }

    #[test]
    fn initial_host_does_not_recapture_before_first_model_request() {
        check_initial_host(None);
    }

    #[test]
    fn initial_host_checks_stop_before_indexing() {
        check_initial_host(Some("before"));
    }

    #[test]
    fn initial_host_checks_stop_after_preparation_event_before_indexing() {
        check_initial_host(Some("preparation"));
    }

    #[test]
    fn initial_host_cancelled_idle_response_skips_final_capture() {
        check_initial_host(Some("model_result"));
    }

    #[test]
    fn initial_host_cancelled_after_change_keeps_isolated_edit_without_final_capture() {
        check_initial_host(Some("change"));
    }
}
