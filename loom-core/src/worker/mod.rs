//! L4: solve in one overlay and propose a patch. Acceptance belongs to Core.
mod agents;
mod budget;
mod split;
pub use split::run_split;
mod control;
pub use control::WorkerControl;
mod memory;
mod progress;
pub use memory::WorkMemory;
mod tools;
use crate::{
    base::{ArtifactRef, ArtifactStore, Cost, Phase, PhaseLedger},
    effects::{Overlay, Patch},
    models::{cost_ceiling, Message, ModelPort, Need, Part, Request, Role},
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Budget {
    pub steps: usize,
    pub seconds: u64,
    pub tokens: u64,
}
impl Default for Budget {
    fn default() -> Self {
        Self {
            steps: 40,
            seconds: 300,
            tokens: 1_000_000,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolSet {
    ReadOnly,
    Edit,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolProfile {
    #[default]
    Progressive,
    AstFirst,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReadSpan {
    pub symbol: String,
    pub path: PathBuf,
    pub start: usize,
    pub end: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PreparedContext {
    pub text: String,
    #[serde(default)]
    pub supplied: Vec<ReadSpan>,
}
pub struct WorkerTask {
    pub goal: String,
    pub guidance: String,
    pub images: Vec<Part>,
    pub skills: Vec<std::path::PathBuf>,
    pub reasoning_effort: Option<String>,
    pub model: Option<String>,
    pub context_limit: usize,
    pub plan: Vec<crate::sessions::PlanStep>,
    pub policy: crate::effects::ExecutionPolicy,
    pub depth: usize,
    /// Automatic graph workers propose a part; Core validates the merged task.
    pub defer_validation_to_core: bool,
    pub control: WorkerControl,
    pub overlay: Overlay,
    pub context: Option<String>,
    pub supplied: Vec<ReadSpan>,
    pub handoff: Option<String>,
    pub artifacts: Vec<ArtifactRef>,
    pub tools: ToolSet,
    pub tool_profile: ToolProfile,
    /// Optional hard write boundary for a child worker; a missing value is the
    /// legacy unconstrained worker, and only an explicit scoped spawn sets it.
    pub allowed_writes: Option<BTreeSet<PathBuf>>,
    pub budget: Budget,
    pub need: Need,
    pub checks: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkerStatus {
    Proposed,
    Unresolved,
    Failed(String),
}

#[derive(Clone)]
struct CachedExploration {
    is_error: bool,
    artifact: String,
    source_file: Option<tools::SourceFile>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkerResult {
    pub status: WorkerStatus,
    /// This worker ended its proposal or explicitly reported unresolved.
    /// Core may repair an actual patch rejected by independent validation.
    #[serde(default)]
    pub finished: bool,
    /// The model answered in prose before changing anything: a reply, not a proposal.
    #[serde(default)]
    pub answered: bool,
    pub patch: Patch,
    pub touched: BTreeSet<PathBuf>,
    pub summary: String,
    #[serde(default)]
    pub plan: Vec<crate::sessions::PlanStep>,
    pub rereads: u32,
    pub cost_by_phase: BTreeMap<Phase, Cost>,
    pub trace: ArtifactRef,
    #[serde(default)]
    pub artifacts: Vec<ArtifactRef>,
    /// Core-observed evidence, independent of the worker's conversation history.
    #[serde(default)]
    pub work_memory: WorkMemory,
    #[serde(default)]
    pub activity_report: String,
    #[serde(default)]
    pub last_model_error: Option<String>,
    pub steps: usize,
    pub input_tokens: u64,
    #[serde(default)]
    pub remote_input_tokens: Option<u64>,
    #[serde(default)]
    pub remote_calls: u32,
    /// Elapsed time across successful and failed model requests.
    #[serde(default)]
    pub model_wait_ms: u64,
    #[serde(default)]
    pub gateway_overhead_ms: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub phase_bytes: BTreeMap<String, u64>,
    pub models: BTreeMap<String, u32>,
    pub seconds: u64,
}

/// Tools outside the core coding set, by group. Anything not listed here and not a core
/// tool (MCP and web extensions) falls in `extensions`.
const TOOL_GROUPS: &[(&str, &str)] = &[
    (
        "graph",
        "map, syntax search, references and graph splitting when the supplied code anchors or scoped reads are insufficient",
    ),
    (
        "terminal",
        "run and interact with commands: exec_command, write_stdin",
    ),
    (
        "symbols",
        "rename a symbol across files, replace or insert by symbol id",
    ),
    (
        "agents",
        "child workers in isolated copies: spawn, wait, send, interrupt, merge",
    ),
    ("plan", "record a step plan"),
    ("ask", "ask the user a clarifying question"),
    ("skills", "load a skill's instructions and files"),
    ("extensions", "web search/fetch and MCP server tools"),
];
const CORE_TOOLS: &[&str] = &[
    "find",
    "glob",
    "outline",
    "read",
    "read_artifact",
    "git_status",
    "git_diff",
    "git_log",
    "git_show",
    "git_blame",
    "edit",
    "write",
    "check",
    "diagnostics",
    "finish",
];

fn tool_group(name: &str) -> Option<&'static str> {
    Some(match name {
        n if CORE_TOOLS.contains(&n) || n == "enable_tools" => return None,
        "map" | "plan_split" | "symbols" | "symbol_context" | "inspect_symbol" | "file_context"
        | "symbol_impact" | "read_many" | "ast_search" | "refs" => "graph",
        "exec_command" | "write_stdin" => "terminal",
        "rename_symbol" | "replace_symbol" | "insert_after_symbol" => "symbols",
        "spawn_agent" | "wait_agent" | "send_agent" | "interrupt_agent" | "merge_agent" => "agents",
        "update_plan" => "plan",
        "request_user_input" => "ask",
        "use_skill" | "read_skill_resource" => "skills",
        _ => "extensions",
    })
}

const SYSTEM: &str = "You are a Loom worker in an isolated repository copy. For conversation, answer directly without tools. For code work, use supplied likely code anchors before searching; map is a global-reference overview, not task-ranked. If asked to inspect an entire directory without a specified path, start at the repository root using outline with path '.' and map; inspect relevant files before summarizing, and accurately state the scope inspected instead of asking which directory was meant. After outline/map succeeds, answer with the actual structure and findings from those results; never merely say that you already listed, mapped, or inspected them. Reuse current structural evidence instead of repeating the same outline/map on an unchanged revision. Read the relevant code once, edit promptly, then check the changed patch. Do not repeat unchanged checks or searches. Prefer git_status, git_diff and git_log over equivalent shell git commands. When asked to edit, a prose answer without a real patch is unfinished. Read before editing and copy old text without line numbers. Keep edits minimal. Commands run in the copy's root using relative paths; cd into subprojects before cargo/npm. Call finish with a proposal or explain why unresolved; Core validates and applies the patch. Use read_artifact for stored tool/check results. Network tools require task approval; never claim a fetch that failed.";

pub fn default_checks(root: &std::path::Path) -> Vec<String> {
    project_commands(root)
}

/// Validation for one project directory, by the build files it contains.
fn project_commands(dir: &std::path::Path) -> Vec<String> {
    if dir.join("Cargo.toml").exists() {
        vec![
            "cargo build --quiet".into(),
            "cargo test --quiet -- --test-threads=1".into(),
        ]
    } else if has_npm_tests(dir) {
        vec!["npm test --silent".into()]
    } else if dir.join("pyproject.toml").exists() || dir.join("setup.py").exists() {
        vec!["python3 -m pytest -q".into()]
    } else {
        vec![]
    }
}

/// A package.json with a real `test` script (not npm's "no test specified" stub).
fn has_npm_tests(dir: &std::path::Path) -> bool {
    std::fs::read(dir.join("package.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|package| package["scripts"]["test"].as_str().map(str::to_owned))
        .is_some_and(|test| !test.contains("no test specified"))
}

/// The checks to run for a change. User-given checks are kept. The default (decided
/// from the repository root) is replaced by the checks of each project that contains a
/// changed file: in a repository of several crates or packages, the root's build file
/// (often a tooling package.json) says nothing about the code that changed.
pub fn effective_checks(
    root: &std::path::Path,
    configured: &[String],
    touched: &BTreeSet<PathBuf>,
) -> Vec<String> {
    if configured != default_checks(root).as_slice() || touched.is_empty() {
        return configured.to_vec();
    }
    let mut projects = BTreeSet::new();
    for file in touched {
        let mut dir = root.join(file);
        while dir.pop() && dir.starts_with(root) {
            if !project_commands(&dir).is_empty() {
                projects.insert(dir.strip_prefix(root).unwrap_or(&dir).to_path_buf());
                break;
            }
        }
    }
    let mut checks = Vec::new();
    for project in projects {
        for command in project_commands(&root.join(&project)) {
            let quoted = project.to_string_lossy().replace('\'', "'\\''");
            checks.push(if project.as_os_str().is_empty() {
                command
            } else {
                format!("cd '{quoted}' && {command}")
            });
        }
    }
    if checks.is_empty() {
        configured.to_vec()
    } else {
        checks
    }
}

pub fn run(
    task: &mut WorkerTask,
    model: &dyn ModelPort,
    cancel: &AtomicBool,
    observe: impl FnMut(Value),
) -> Result<WorkerResult> {
    if task.depth == 0 {
        let shared = budget::SharedModel::new(model, task.budget);
        let mut result =
            std::thread::scope(|scope| run_scoped(task, &shared, cancel, observe, scope))?;
        result.steps = shared.calls();
        Ok(result)
    } else {
        std::thread::scope(|scope| run_scoped(task, model, cancel, observe, scope))
    }
}
fn run_scoped<'scope, 'env>(
    task: &mut WorkerTask,
    model: &'env dyn ModelPort,
    cancel: &'env AtomicBool,
    mut observe: impl FnMut(Value),
    scope: &'scope std::thread::Scope<'scope, 'env>,
) -> Result<WorkerResult> {
    let start = Instant::now();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let model_cancel = std::sync::Arc::new(AtomicBool::new(cancel.load(Ordering::SeqCst)));
    let done = std::sync::Arc::new(AtomicBool::new(false));
    let bridge = CancelBridge {
        flag: model_cancel.clone(),
        done: done.clone(),
    };
    let mirror = model_cancel.clone();
    scope.spawn(move || {
        while !done.load(Ordering::SeqCst) {
            if cancel.load(Ordering::SeqCst) {
                mirror.store(true, Ordering::SeqCst);
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    });
    let _bridge = bridge;
    let store = task.overlay.artifacts().clone();
    let mut children = agents::Agents::new(scope, model, task, cancel, start);
    if task.tools == ToolSet::ReadOnly {
        task.policy.sandbox = crate::effects::SandboxMode::ReadOnly;
    }
    let mut trace = Vec::new();
    let mut record = |event: Value| {
        observe(event.clone());
        trace.extend_from_slice(event.to_string().as_bytes());
        trace.push(b'\n');
    };
    let mut ledger = PhaseLedger::default();
    let mut phase_bytes = BTreeMap::new();
    let mut models = BTreeMap::new();
    let mut input = 0u64;
    let mut remote_input_tokens = Some(0u64);
    let mut remote_calls: u32 = 0;
    let mut model_wait_ms = 0u64;
    let mut gateway_overhead_ms = 0u64;
    let mut cached = 0u64;
    let mut output = 0u64;
    let mut steps = 0;
    let mut status = WorkerStatus::Unresolved;
    let mut terminal_finish = false;
    // Any call that may change the copy; until then a prose reply is the answer.
    let mut mutated = false;
    let mut answered = false;
    // Model chosen by the gateway on the first call, kept for this attempt.
    let mut pinned: Option<String> = None;
    let mut pinned_connection: Option<String> = None;
    let mut transient_retries = 0;
    // A read-only tool succeeded: a finish without changes then reports findings.
    let mut investigated = false;
    let mut summary = "budget exhausted".to_owned();
    let mut first_message = Message::user(&task.goal);
    first_message.parts.extend(task.images.clone());
    let mut messages = vec![first_message];
    if !task.guidance.is_empty() {
        messages.insert(0, Message::user(&task.guidance));
    }
    if let Some(context) = &task.context
        && !context.trim().is_empty()
    {
        messages.insert(0, Message::user(context));
    }
    if let Some(handoff) = &task.handoff {
        let projection = serde_json::from_str::<Value>(handoff)
            .ok()
            .filter(Value::is_object)
            .map(|mut value| {
                value.as_object_mut().unwrap().remove("work_memory");
                value.to_string()
            })
            .unwrap_or_else(|| handoff.clone());
        messages.push(Message::user(format!(
            "Previous attempt evidence:\n{projection}"
        )));
    }
    let mut reads = BTreeSet::new();
    let mut rereads = 0;
    let mut idle = 0;
    // Within one unchanged overlay revision, identical read-only queries cannot
    // produce new evidence. Reuse their output and stop an exploration loop after
    // three consecutive redundant queries. Core single-file edits invalidate
    // affected entries; commands and unknown write sets clear the whole cache.
    // The old cache retained full tool output, even though repeated calls only
    // need to know whether the original query failed. A digest of the actual
    // result also catches equivalent evidence produced by different arguments.
    // Preserve the content-addressed artifact ID so the first result remains
    // recoverable even after its message has left the model's context window.
    let mut exploration_cache: BTreeMap<String, CachedExploration> = BTreeMap::new();
    let mut seen_evidence: BTreeSet<(String, [u8; 32], bool)> = BTreeSet::new();
    let mut stagnant_queries = 0usize;
    let mut failed_calls = BTreeSet::new();
    let mut recovery_requested = false;
    // Validation against an identical patch cannot provide new evidence even
    // when other, unrelated exploration happens between check calls.
    let mut redundant_checks = 0usize;
    let mut work_memory: WorkMemory = task
        .handoff
        .as_ref()
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .and_then(|handoff| serde_json::from_value(handoff["work_memory"].clone()).ok())
        .unwrap_or_default();
    // The restored overlay may differ from the previous worker's final tree.
    // Preserve its findings, but revalidate before treating them as current code.
    if task.handoff.is_some() {
        work_memory.invalidate();
    }
    let mut web_permission_denied = false;
    let mut need = task.need;
    need.tools = true;
    need.max_cost = need.max_cost.min(cost_ceiling());
    // A compiled anchor came from this immutable base. Register its freshness
    // without resending the same body through a read tool result.
    for span in &task.supplied {
        task.overlay.read(&span.path, Some(0..0))?;
    }
    let mut workspace = tools::Workspace::new(
        &mut task.overlay,
        task.tools == ToolSet::ReadOnly,
        task.checks.clone(),
        cancel,
        Duration::from_secs(task.budget.seconds).saturating_sub(start.elapsed()),
        task.policy,
        &task.skills,
        task.allowed_writes.clone(),
    )?;
    workspace.register_artifacts(&task.artifacts);
    workspace.set_goal_find_scope(&task.goal);
    let mut specs = workspace.specs();
    if task.defer_validation_to_core {
        specs.retain(|tool| tool.name != "check");
    }
    specs.push(crate::models::ToolSpec{name:"request_user_input".into(),description:"Ask the user a clarifying question and wait for their explicit answer.".into(),parameters:json!({"type":"object","required":["questions"],"properties":{"questions":{"type":"array","minItems":1,"maxItems":3,"items":{"type":"object","required":["id","question"],"properties":{"id":{"type":"string"},"question":{"type":"string"},"options":{"type":"array","items":{"type":"string"}}}}}}})});
    for (name, description, properties, required) in [
        (
            "spawn_agent",
            "Start a child worker with its complete task in an isolated copy. Parent and children share the chosen model and request allowance. Explicit write_files/read_files reserve file-level scope until merged. Child files are invisible to the parent until merge_agent; wait and merge completed editing proposals before inspecting their files or finishing.",
            json!({"goal":{"type":"string"},"read_only":{"type":"boolean"},"write_files":{"type":"array","items":{"type":"string"}},"read_files":{"type":"array","items":{"type":"string"}},"anchors":{"type":"array","maxItems":32,"items":{"type":"string"},"description":"explicit plan_split anchors for a bounded graph slice"}}),
            json!(["goal"]),
        ),
        (
            "wait_agent",
            "Wait up to 30 seconds by default for a child and inspect its result; a running result is normal, not a reason to interrupt or respawn. Completed edits report requires_merge=true until merge_agent.",
            json!({"id":{"type":"string"},"timeout_ms":{"type":"integer"}}),
            json!(["id"]),
        ),
        (
            "send_agent",
            "Send a follow-up to a completed child, returning a new child id.",
            json!({"id":{"type":"string"},"text":{"type":"string"}}),
            json!(["id", "text"]),
        ),
        (
            "interrupt_agent",
            "Cancel a child worker.",
            json!({"id":{"type":"string"}}),
            json!(["id"]),
        ),
        (
            "merge_agent",
            "Merge a completed child proposal into the parent copy; conflicts preserve both originals.",
            json!({"id":{"type":"string"}}),
            json!(["id"]),
        ),
    ] {
        specs.push(crate::models::ToolSpec {
            name: name.into(),
            description: description.into(),
            parameters: json!({"type":"object","properties":properties,"required":required}),
        });
    }
    if task.defer_validation_to_core {
        specs.retain(|tool| !matches!(tool_group(&tool.name), Some("agents" | "terminal")));
    }
    if task.tool_profile == ToolProfile::AstFirst {
        // V4 is deliberately a direct single-worker fast path. Keep native AST
        // exploration and precise edits, but remove the planner/child-agent surface
        // so the model cannot silently recreate V3's coordination stack.
        specs.retain(|tool| {
            !matches!(
                tool.name.as_str(),
                "plan_split"
                    | "spawn_agent"
                    | "wait_agent"
                    | "send_agent"
                    | "interrupt_agent"
                    | "merge_agent"
                    | "update_plan"
            )
        });
    }
    // Progressive tool disclosure: every spec travels with every call, so only the core
    // coding tools are listed up front; the rest come in groups the model enables (or
    // that turn on when the model calls one of their tools anyway).
    let mut enabled_groups: BTreeSet<&'static str> = BTreeSet::new();
    if task.tool_profile == ToolProfile::AstFirst {
        enabled_groups.insert("graph");
        enabled_groups.insert("symbols");
    }
    if !task.skills.is_empty() {
        enabled_groups.insert("skills");
    }
    // MCP servers by name (from `mcp__<server>__<tool>`), so the hidden group says what it holds.
    let mcp_servers: BTreeSet<&str> = specs
        .iter()
        .filter_map(|spec| crate::extensions::mcp_server_of(&spec.name))
        .collect();
    // A request that names a configured service needs its tools up front.
    if !crate::extensions::servers_mentioned(&task.goal, mcp_servers.iter().copied()).is_empty() {
        enabled_groups.insert("extensions");
    }
    let visible = |enabled: &BTreeSet<&'static str>| -> Vec<crate::models::ToolSpec> {
        let mut out: Vec<_> = specs
            .iter()
            .filter(|spec| tool_group(&spec.name).is_none_or(|g| enabled.contains(g)))
            .cloned()
            .collect();
        let hidden: Vec<_> = TOOL_GROUPS
            .iter()
            .filter(|(group, _)| {
                !enabled.contains(group)
                    && specs
                        .iter()
                        .any(|spec| tool_group(&spec.name) == Some(*group))
            })
            .collect();
        if !hidden.is_empty() {
            out.push(crate::models::ToolSpec {
                name: "enable_tools".into(),
                description: format!(
                    "Add tool groups when the core tools are not enough: {}.",
                    hidden
                        .iter()
                        .map(|(g, d)| match *g {
                            "extensions" if !mcp_servers.is_empty() => format!(
                                "{g} ({d}; MCP servers: {})",
                                mcp_servers.iter().copied().collect::<Vec<_>>().join(", ")
                            ),
                            _ => format!("{g} ({d})"),
                        })
                        .collect::<Vec<_>>()
                        .join("; ")
                ),
                parameters: json!({"type":"object","required":["groups"],"properties":{"groups":{"type":"array","items":{"type":"string","enum":hidden.iter().map(|(g, _)| *g).collect::<Vec<_>>()}}}}),
            });
        }
        out
    };
    let anchors = workspace.find_anchor_candidates(&task.goal);
    // An unknown target needs the graph immediately. A target with concrete
    // anchors starts with cheaper reads; graph remains one enable_tools away.
    if anchors.is_empty() {
        enabled_groups.insert("graph");
    }
    if task
        .context
        .as_ref()
        .is_none_or(|text| text.trim().is_empty())
        && !anchors.is_empty()
    {
        messages.insert(
            0,
            Message::user(format!("Likely relevant code:\n{anchors}")),
        );
    }
    // All initial instructions, supplied context, the goal, and handoff are
    // immutable. Compaction may archive completed turns but must not evict them.
    let prefix_len = messages.len();
    // Limit requested SMALL attempts before Core tries a more capable tier.
    // The limit counts actual model requests; one response can still batch tools.
    let call_limit = if task.need.min_tier == crate::models::Tier::Small {
        task.budget.steps.min(8)
    } else {
        task.budget.steps
    };
    let mut closing_requested = false;
    let mut last_progress_phase = None;
    let mut last_model_error = None;
    let mut tool_generation_recovery_used = false;
    let mut system=task.allowed_writes.as_ref().map_or_else(||SYSTEM.to_owned(),|writes|format!("{SYSTEM}\n\nCore write lease: Owned write files: {}. Complete ONLY the requested portion in these files. Other workers handle the rest of the parent request. Do not search for or modify their work, or run full-task checks against an unmerged copy. Once your portion is edited, finish your scoped proposal for Core to merge and validate. A scope denial is not permission to retry another out-of-scope tool.",writes.iter().map(|path|path.display().to_string()).collect::<Vec<_>>().join(", ")));
    if task.tool_profile == ToolProfile::AstFirst {
        system.push_str("\nV4 AST-first mode: stay in this single worker. Use the shortest structural path that can identify the owning code. Tool playbook: when a symbol name is known, prefer inspect_symbol; when a file is known but the exact owner is not, use file_context or outline; when ownership is unknown, use one focused map; use symbol_impact before changing shared behavior when callers/tests may matter, and use refs only for a narrow direct caller/callee check; use ast_search for syntax shapes and find for literals, diagnostics, config keys, prose, or when no useful symbol is known. Read exact symbol ids when possible and batch independent known targets with read_many. Do not re-read structural evidence that is already current. Once the owning symbol and affected boundary are clear, stop exploring and edit. Prefer rename_symbol, replace_symbol, or insert_after_symbol when the requested change exactly matches those structural operations; otherwise use edit. Prefer diagnostics for a fast compile/type pass before the full check when useful. Use terminal commands for validation or behavior that repository tools cannot establish, not as a substitute for code discovery. After mutation, inspect only evidence invalidated by the change, run the relevant check, then finish concisely.");
    }
    if task.defer_validation_to_core {
        system.push_str("\nValidation ownership: Core after merge, not this child. The check tool is intentionally unavailable. Do NOT request it or declare unresolved because you cannot run whole-task tests. Submit your scoped patch; Core independently runs the parent checks on all merged changes. For completion, prefer EndTurn text containing exactly {\"summary\":\"your actual scoped changes\",\"unresolved\":false}. This closes only your proposal; Core decides success. Do not invent function-name aliases or delegate further.");
    }
    for step in 1..=call_limit {
        children.directives(&task.control, workspace.overlay_mut())?;
        for event in children.events() {
            record(event);
        }
        if cancel.load(Ordering::SeqCst) {
            summary = "cancelled".into();
            break;
        }
        let remaining = task
            .budget
            .seconds
            .saturating_sub(start.elapsed().as_secs());
        if remaining == 0 {
            summary = "time budget exhausted".into();
            break;
        }
        if input.saturating_add(output) >= task.budget.tokens {
            summary = "token budget exhausted".into();
            break;
        }
        steps = step;
        compact_messages(
            &mut messages,
            prefix_len,
            task.context_limit,
            &store,
            &mut workspace,
            &work_memory,
            &task.plan,
        )?;
        if !closing_requested
            && ((call_limit >= 4 && step >= call_limit - 1)
                || remaining <= (task.budget.seconds / 10).clamp(1, 15)
                || input.saturating_add(output) >= task.budget.tokens.saturating_mul(4) / 5)
        {
            closing_requested = true;
            let closing = format!(
                "Conclude the current attempt: {} model turns and {remaining}s remain. Stop broad exploration. Use existing evidence for the smallest useful edit and check, or finish unresolved with what you found, the exact blocker, and remaining work. Do not claim success without a real verified result.",
                call_limit.saturating_sub(step).saturating_add(1)
            );
            // Keep tool results and their guidance in the same user turn.
            // Never alter the immutable request/handoff prefix.
            if messages.len() > prefix_len
                && messages
                    .last()
                    .is_some_and(|message| message.role == Role::User)
            {
                messages.last_mut().unwrap().parts.push(Part::Text(closing));
            } else {
                messages.push(Message::user(closing));
            }
            record(
                json!({"progress":{"id":format!("{nonce:x}-closing"),"phase":"closing","message":"Concluding with the current evidence: edit and validate, or report the unresolved blocker."}}),
            );
        }
        record(json!({"model_started":{"step":step,"remaining_seconds":remaining}}));
        // Leave room for the next tool call and verification. An unresponsive
        // gateway must not consume the entire task budget in a single request.
        // Bootstrap may need one gateway fallback to find a healthy connection.
        // Once routing is known, use shorter calls; an explicit setting overrides both.
        let call_timeout = std::env::var("LOOM_MODEL_CALL_TIMEOUT_SECONDS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(if step == 1 { 90 } else { 45 });
        let request = Request {
            reasoning_effort: task.reasoning_effort.clone(),
            cancel: Some(model_cancel.clone()),
            model: task
                .model
                .clone()
                .or_else(|| pinned.clone())
                .unwrap_or_default(),
            system: Some(system.clone()),
            messages: messages.clone(),
            tools: visible(&enabled_groups),
            max_output_tokens: Some(4096),
            timeout: Some(Duration::from_secs(remaining.min(call_timeout))),
            gateway_connection: pinned_connection.clone(),
            temperature: std::env::var("LOOM_TEMPERATURE")
                .ok()
                .and_then(|s| s.parse().ok()),
        };
        let mut streamed = false;
        let call_started = Instant::now();
        let served = match model.complete_stream(need, &request, &mut |event| {
            if let crate::models::StreamEvent::TextDelta(delta) = event {
                streamed = true;
                record(json!({"step":step,"text_delta":delta}));
            }
        }) {
            Ok(served) => served,
            Err(error) => {
                last_model_error = Some(error.to_string());
                let elapsed = call_started.elapsed().as_millis() as u64;
                model_wait_ms = model_wait_ms.saturating_add(elapsed);
                record(
                    json!({"step":step,"error":error.to_string(),"model_wait_ms":elapsed,"model_timeout_seconds":remaining.min(call_timeout)}),
                );
                record(
                    json!({"progress":{"id":format!("{nonce:x}-model-error-{step}"),"phase":"waiting","message":format!("No model response was received; no tool ran for this call. {}", progress::short(&error.to_string(), 160))}}),
                );
                // Some providers reject a hallucinated tool before returning a
                // response. Treat only this explicit generation-error code as
                // repairable; invalid schemas, models and credentials still fail.
                if error.is_tool_generation_rejected() {
                    if !tool_generation_recovery_used && step < call_limit {
                        tool_generation_recovery_used = true;
                        let guidance = format!(
                            "The provider rejected your generated tool call; it was not executed. Continue from the existing source and patch. Use exactly one of these supplied tool names: {}. Do not invent namespaces. Make the remaining requested edit and check, or finish unresolved with the blocker.",
                            request
                                .tools
                                .iter()
                                .map(|tool| tool.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                        if messages.len() > prefix_len
                            && messages
                                .last()
                                .is_some_and(|message| message.role == Role::User)
                        {
                            messages
                                .last_mut()
                                .unwrap()
                                .parts
                                .push(Part::Text(guidance));
                        } else {
                            messages.push(Message::user(guidance));
                        }
                        record(
                            json!({"progress":{"id":format!("{nonce:x}-tool-recovery-{step}"),"phase":"tool_recovery","message":"The model generated an unavailable tool. Nothing ran; preserving existing edits and evidence for one recovery attempt."}}),
                        );
                        continue;
                    }
                    summary = error.to_string();
                    status = WorkerStatus::Failed(summary.clone());
                    break;
                }
                // A pinned gateway model that stopped answering: return to automatic
                // routing. A gateway timeout or 5xx: try again before giving up the attempt.
                if pinned.take().is_some() {
                    pinned_connection = None;
                    continue;
                }
                if matches!(error.kind, crate::models::ErrorKind::Transient | crate::models::ErrorKind::TimedOut) && transient_retries < 2 {
                    transient_retries += 1;
                    if step == call_limit {
                        break;
                    }
                    let duration = Duration::from_secs(task.budget.seconds);
                    if start.elapsed() >= duration {
                        break;
                    }
                    // A retry should respond promptly to Esc.
                    for _ in 0..20 {
                        let remaining = duration.saturating_sub(start.elapsed());
                        if cancel.load(Ordering::SeqCst) || remaining.is_zero() {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(100).min(remaining));
                    }
                    continue;
                }
                summary = error.to_string();
                status = WorkerStatus::Failed(summary.clone());
                break;
            }
        };
        last_model_error = None;
        let elapsed = call_started.elapsed().as_millis() as u64;
        model_wait_ms = model_wait_ms.saturating_add(elapsed);
        let overhead = served
            .response
            .gateway_upstream_ms
            .map(|ms| (served.response.latency.as_millis() as u64).saturating_sub(ms));
        gateway_overhead_ms = gateway_overhead_ms.saturating_add(overhead.unwrap_or(0));
        // Automatic routing through a gateway may pick a different model on every call;
        // a worker that changes models mid-task loses its thread of thought. Keep the
        // model that answered first for the rest of this attempt.
        if task.model.is_none()
            && pinned.is_none()
            && served.candidate.model == "auto"
            && crate::models::is_chat_model(&served.response.model)
            && let Some(route) = served.response.route.as_deref()
        {
            let served_model = served
                .response
                .model
                .trim_start_matches(&format!("{route}/"))
                .to_owned();
            pinned = Some(format!(
                "{}/{route}/{served_model}",
                served.candidate.endpoint
            ));
            pinned_connection = served.response.gateway_connection.clone();
            record(
                json!({"step":step,"pinned_model":pinned,"connection_pinned":pinned_connection.is_some()}),
            );
        }
        record(
            json!({"step":step,"model_wait_ms":elapsed,"gateway_upstream_ms":served.response.gateway_upstream_ms,"gateway_overhead_ms":overhead}),
        );
        // Defend against a custom port accidentally violating the Core ceiling.
        if served.candidate.cost > need.max_cost || served.candidate.cost > cost_ceiling() {
            summary = "model exceeded cost ceiling".into();
            status = WorkerStatus::Failed(summary.clone());
            break;
        }
        let response = served.response;
        if served.candidate.cost != crate::models::CostClass::Local {
            remote_calls += 1;
            remote_input_tokens = remote_input_tokens
                .zip(response.usage.input)
                .map(|(total, n)| total.saturating_add(n));
        }
        input = input.saturating_add(response.usage.input.unwrap_or(0));
        cached = cached.saturating_add(response.usage.cached_input.unwrap_or(0));
        output = output.saturating_add(response.usage.output.unwrap_or(0));
        *models
            .entry(format!("{}/{}", served.candidate.endpoint, response.model))
            .or_insert(0) += 1;
        let model_cost = Cost {
            input: response.usage.input.unwrap_or(0),
            cached: response.usage.cached_input.unwrap_or(0),
            output: response.usage.output.unwrap_or(0),
            usd: served.candidate.usd(&response.usage),
            ms: response.latency.as_millis() as u64,
        };
        let calls: Vec<_> = response
            .tool_calls()
            .map(|(i, n, a)| (i.to_owned(), n.to_owned(), a.clone()))
            .collect();
        let phase = if calls.iter().any(|(_, n, _)| n == "edit" || n == "write") {
            Phase::Edit
        } else if calls.iter().any(|(_, n, _)| n == "check") {
            Phase::Validate
        } else if calls.is_empty() {
            Phase::Message
        } else {
            Phase::Orient
        };
        ledger.record(phase, model_cost);
        *phase_bytes.entry("message".to_owned()).or_insert(0) += response.text().len() as u64;
        record(
            json!({"step":step,"requested_tier":task.need.min_tier,"requested_model":request.model,"model":response.model,"endpoint":served.candidate.endpoint,"route":response.route,"usage":response.usage,"calls":calls.iter().map(|(_,n,a)|json!({"tool":n,"args":a})).collect::<Vec<_>>(),"text":response.text(),"streamed":streamed}),
        );
        messages.push(Message {
            role: Role::Assistant,
            parts: response.parts.clone(),
        });
        if calls.is_empty() {
            // A reply in prose before anything was changed answers the turn (a greeting,
            // a question, an explanation). Pushing the model into the repository instead
            // turned "hello" into hours of exploration, validation and escalation.
            let text = response.text();
            // Some tool-capable models emit the finish payload as EndTurn text.
            // A strict structured payload closes a proposal, never proves success
            // or converts edited work into an answer; Core still validates it.
            if mutated
                && response.stop == crate::models::Stop::EndTurn
                && let Ok(Value::Object(payload)) = serde_json::from_str::<Value>(&text)
                && payload
                    .keys()
                    .all(|key| matches!(key.as_str(), "summary" | "answer" | "unresolved"))
                && payload.get("answer").is_none_or(Value::is_boolean)
                && payload.get("unresolved").is_some_and(Value::is_boolean)
                && let Some(done) = payload
                    .get("summary")
                    .and_then(Value::as_str)
                    .filter(|summary| !summary.trim().is_empty())
            {
                status = if payload["unresolved"] == true {
                    WorkerStatus::Unresolved
                } else {
                    WorkerStatus::Proposed
                };
                summary = done.trim().to_owned();
                terminal_finish = true;
                break;
            }
            if !mutated && !text.trim().is_empty() {
                status = WorkerStatus::Proposed;
                summary = text.trim().to_owned();
                terminal_finish = true;
                answered = true;
                break;
            }
            idle += 1;
            if idle >= 2 {
                summary = "model stopped without a proposal".into();
                break;
            }
            messages.push(Message::user(
                "Continue with tools. Call finish to propose the result.",
            ));
            continue;
        }
        idle = 0;
        let mut results = Vec::new();
        let mut finished = false;
        let mut stalled = false;
        let mut batch_activity = Vec::new();
        for (ordinal, (id, name, args)) in calls.into_iter().enumerate() {
            if cancel.load(Ordering::SeqCst) {
                summary = "cancelled".into();
                break;
            }
            if start.elapsed().as_secs() >= task.budget.seconds {
                summary = "time budget exhausted".into();
                break;
            }
            let operation_id = format!("{nonce:x}-{step}-{ordinal}");
            let description = progress::describe(&name, &args);
            let tool_started = Instant::now();
            record(
                json!({"tool_started":{"operation_id":operation_id,"step":step,"tool":name,"description":description,"args":args}}),
            );
            if name == "read" {
                let key = args["path"].as_str().unwrap_or("").to_owned();
                let start = args["start"].as_u64().unwrap_or(1) as usize;
                let end = args["end"].as_u64().unwrap_or(u64::MAX) as usize;
                let supplied = task.supplied.iter().any(|span| {
                    span.symbol == key
                        || (span.path.to_string_lossy() == key
                            && start <= span.end
                            && end >= span.start)
                });
                if !reads.insert(key) || supplied {
                    rereads += 1;
                }
            }
            if let Some(group) = tool_group(&name) {
                enabled_groups.insert(group);
            }
            let exploratory = matches!(
                name.as_str(),
                "map"
                    | "plan_split"
                    | "symbols"
                    | "symbol_context"
                    | "inspect_symbol"
                    | "file_context"
                    | "symbol_impact"
                    | "read_many"
                    | "find"
                    | "glob"
                    | "ast_search"
                    | "outline"
                    | "read"
                    | "read_artifact"
                    | "refs"
            );
            let query_key = exploratory.then(|| format!("{name}:{}", args));
            let reused = query_key
                .as_ref()
                .filter(|_| workspace.exploration_is_stable())
                .and_then(|key| exploration_cache.get(key))
                .cloned();
            let tool = if name == "check" && task.defer_validation_to_core {
                tools::ToolOutput::ok(Phase::Message,json!({"status":"deferred_to_core","passed":null,"reason":"No tests were run in this child. Core independently validates the full merged patch. Finish your scoped proposal now; do not retry check."}).to_string())
            } else if workspace.web_permission_required(&name) && web_permission_denied {
                tools::ToolOutput::err(Phase::Orient, "network permission denied for this task")
            } else if workspace.web_permission_required(&name) {
                // The model cannot self-grant network access. Wait for the
                // connected UI to explicitly authorize this task's web tools.
                let request_id = format!("web-{nonce:x}-{step}");
                record(json!({"network_approval_request":{
                    "id": request_id,
                    "tool": name,
                    "target": if name == "web_fetch" { &args["url"] } else { &args["query"] },
                }}));
                let granted = loop {
                    if cancel.load(Ordering::SeqCst)
                        || start.elapsed().as_secs() >= task.budget.seconds
                    {
                        break false;
                    }
                    if let Some(answer) = task.control.take_answer(&request_id) {
                        break answer == true;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                };
                record(json!({"network_approval_resolved":{"id":request_id,"granted":granted}}));
                if !granted {
                    web_permission_denied = true;
                    tools::ToolOutput::err(Phase::Orient, "network permission denied or timed out")
                } else {
                    match workspace.grant_web_for_task() {
                        Ok(()) => workspace.call(&name, &args),
                        Err(e) => tools::ToolOutput::err(
                            Phase::Orient,
                            format!("web access setup failed: {e:#}"),
                        ),
                    }
                }
            } else if let Some(cached) = &reused {
                // Repeating an identical query (weak models do) only re-sends the same
                // text. Point to the original artifact even if it was compacted.
                tools::ToolOutput {
                    text: format!(
                        "Same {name} call as before, so the result is unchanged (see the earlier result). \
                         Original evidence: read_artifact(path=\"{}\", start=1, end=1), \
                         or search with a different pattern.{}",
                        cached.artifact,
                        if cached.is_error {
                            " The earlier call failed; change the arguments."
                        } else {
                            ""
                        }
                    ),
                    is_error: cached.is_error,
                    phase: Phase::Orient,
                    finish: None,
                }
            } else if name == "enable_tools" {
                let mut added = Vec::new();
                for group in args["groups"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                {
                    if let Some((g, _)) = TOOL_GROUPS.iter().find(|(g, _)| *g == group) {
                        enabled_groups.insert(g);
                        added.extend(
                            specs
                                .iter()
                                .filter(|spec| tool_group(&spec.name) == Some(*g))
                                .map(|spec| spec.name.clone()),
                        );
                    }
                }
                if added.is_empty() {
                    tools::ToolOutput::err(Phase::Orient, "unknown or empty tool group")
                } else {
                    tools::ToolOutput::ok(
                        Phase::Orient,
                        format!("now available: {}", added.join(", ")),
                    )
                }
            } else if matches!(
                name.as_str(),
                "spawn_agent" | "wait_agent" | "send_agent" | "interrupt_agent" | "merge_agent"
            ) {
                children.parent_model(task.model.clone().or_else(|| pinned.clone()));
                match children.call(&name, &args, workspace.overlay_mut()) {
                    Ok(text) => tools::ToolOutput::ok(Phase::Orient, text),
                    Err(error) => tools::ToolOutput::err(Phase::Orient, error.to_string()),
                }
            } else if name == "request_user_input" {
                let request_id = format!("question-{nonce:x}-{step}");
                record(json!({"input_request":{"id":request_id,"questions":args["questions"]}}));
                loop {
                    if let Some(answer) = task.control.take_answer(&request_id) {
                        record(json!({"input_resolved":request_id}));
                        break tools::ToolOutput::ok(Phase::Message, answer.to_string());
                    }
                    if cancel.load(Ordering::SeqCst)
                        || start.elapsed().as_secs() >= task.budget.seconds
                    {
                        break tools::ToolOutput::err(
                            Phase::Message,
                            "input cancelled or timed out",
                        );
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            } else if name == "update_plan" {
                match parse_plan(&args) {
                    Ok(plan) => {
                        if serde_json::to_value(&plan)? == serde_json::to_value(&task.plan)? {
                            stagnant_queries += 1;
                            tools::ToolOutput::ok(Phase::Message, "plan unchanged")
                        } else {
                            task.plan = plan;
                            stagnant_queries = 0;
                            tools::ToolOutput::ok(
                                Phase::Message,
                                serde_json::to_string(&task.plan)?,
                            )
                        }
                    }
                    Err(error) => tools::ToolOutput::err(Phase::Message, error.to_string()),
                }
            } else {
                match children.parent_conflict(&name, &args) {
                    Some(reason) => tools::ToolOutput::err(Phase::Orient, reason),
                    None => workspace.call(&name, &args),
                }
            };
            // Checks, finish, and failed edits cannot change the repository.
            // An external command might have changed it even if it returned an error.
            let external_command = matches!(name.as_str(), "exec_command" | "write_stdin");
            let attempted_edit = matches!(
                name.as_str(),
                "edit"
                    | "write"
                    | "rename_symbol"
                    | "replace_symbol"
                    | "insert_after_symbol"
                    | "merge_agent"
            );
            let extension_mutation = tool_group(&name) == Some("extensions")
                && specs.iter().any(|spec| spec.name == name)
                && !workspace.extension_is_read_only(&name);
            let may_have_mutated =
                external_command || (!tool.is_error && (attempted_edit || extension_mutation));
            if may_have_mutated {
                if let Some(changed_file) = workspace.source_file(&name, &args) {
                    // Only a Core-owned single-file edit proves other file reads
                    // unchanged. Searches/graph queries still depend on this edit;
                    // unknown effects and commands invalidate all cached evidence.
                    exploration_cache.retain(|_, cached| {
                        cached
                            .source_file
                            .as_ref()
                            .is_some_and(|file| !file.same_file(&changed_file))
                    });
                } else {
                    exploration_cache.clear();
                }
                seen_evidence.clear();
                failed_calls.clear();
                stagnant_queries = 0;
                redundant_checks = 0;
                work_memory.invalidate();
            }
            if name == "check" {
                if serde_json::from_str::<Value>(&tool.text)
                    .ok()
                    .is_some_and(|result| result["unchanged"] == true)
                {
                    redundant_checks += 1;
                } else {
                    redundant_checks = 0;
                }
            }
            let artifact = store.put(tool.text.as_bytes())?;
            if reused.is_none() {
                // Cached reminders do not replace their original source evidence.
                work_memory.observe(&name, &args, &tool.text, tool.is_error, artifact.clone());
                workspace.register_artifacts(std::slice::from_ref(&artifact));
            }
            if let Some(key) = query_key {
                // File reads and outlines include their normalized source path
                // in the output. Other queries (notably searches) may return
                // the same negative text for genuinely different scopes, so
                // never deduplicate those solely by their response contents.
                let fresh = if reused.is_none()
                    && !tool.is_error
                    && workspace.exploration_is_stable()
                    && matches!(name.as_str(), "read" | "outline")
                {
                    let digest: [u8; 32] = Sha256::digest(tool.text.as_bytes()).into();
                    seen_evidence.insert((name.clone(), digest, false))
                } else {
                    reused.is_none()
                };
                if reused.is_some() || !fresh {
                    stagnant_queries += 1;
                } else {
                    stagnant_queries = 0;
                }
                if workspace.exploration_is_stable()
                    && let std::collections::btree_map::Entry::Vacant(entry) =
                        exploration_cache.entry(key)
                {
                    workspace.register_artifacts(std::slice::from_ref(&artifact));
                    entry.insert(CachedExploration {
                        is_error: tool.is_error,
                        artifact: artifact.id.clone(),
                        source_file: if tool.is_error {
                            None
                        } else {
                            workspace.source_file(&name, &args)
                        },
                    });
                }
            }
            // Repeating a rejected call supplies no new evidence either. In
            // particular, hallucinated tool names must not evade the loop guard.
            if tool.is_error
                && !exploratory
                && !may_have_mutated
                && !failed_calls.insert(format!("{name}:{args}"))
            {
                stagnant_queries += 1;
            }
            // Even an unsuccessful edit attempt rules out presenting a later
            // prose-only reply as if the code work had been completed.
            mutated |= attempted_edit || external_command || extension_mutation;
            if name != "finish" && !tool.is_error {
                investigated = true;
            }
            let phase = format!("{:?}", tool.phase).to_ascii_lowercase();
            *phase_bytes.entry(phase).or_insert(0) += tool.text.len() as u64;
            let result_summary =
                progress::result_summary(&name, &tool.text, tool.is_error, reused.is_some());
            if !matches!(name.as_str(), "finish" | "enable_tools" | "update_plan") {
                batch_activity.push(format!("{description} · {result_summary}"));
            }
            record(
                json!({"step":step,"operation_id":operation_id,"description":description,"result_summary":result_summary,"tool_ms":tool_started.elapsed().as_millis(),"tool":name,"args":args,"error":tool.is_error,"result":tool.text,"artifact":artifact,"cached":reused.is_some()}),
            );
            if let Some(done) = tool.finish {
                terminal_finish = true;
                // An explicit answer remains distinct from an edit proposal. A
                // failed or attempted mutation cannot be laundered as a reply.
                if args["answer"] == true && !mutated {
                    answered = true;
                }
                status = if args["unresolved"] == true {
                    WorkerStatus::Unresolved
                } else {
                    WorkerStatus::Proposed
                };
                summary = done;
                finished = true;
            }
            results.push(Part::ToolResult {
                call_id: id,
                output: tool.text,
                is_error: tool.is_error,
            });
            if finished {
                break;
            }
            if stagnant_queries >= 3 {
                stalled = true;
                summary = "exploration stalled".into();
                record(
                    json!({"step":step,"exploration_stalled":true,"consecutive_redundant_queries":stagnant_queries}),
                );
                break;
            }
            if redundant_checks >= 3 {
                stalled = true;
                summary = "validation stalled: no file changes since the first check".into();
                record(
                    json!({"step":step,"validation_stalled":true,"repeated_unchanged_checks":redundant_checks}),
                );
                break;
            }
        }
        messages.push(Message {
            role: Role::User,
            parts: results,
        });
        if stagnant_queries >= 2 && !recovery_requested && !finished && !stalled {
            recovery_requested = true;
            messages.last_mut().unwrap().parts.push(Part::Text(
                "Repeated tool calls produced no new evidence. Use the source and results already observed to make the smallest requested edit, then check the changed patch. Use only the supplied tool names. Do not repeat failed calls or broaden exploration. If the evidence is insufficient, call finish with unresolved=true and explain the specific blocker. This guidance does not extend the execution budget.".into()
            ));
            record(
                json!({"progress":{"id":format!("{nonce:x}-recovery-{step}"),"phase":"recovery","message":"Repeated calls produced no new evidence. Switching to edit/validate from known code or report the blocker."}}),
            );
        }
        if !batch_activity.is_empty()
            && (last_progress_phase != Some(phase) || step % 4 == 0 || finished || stalled)
        {
            record(
                json!({"progress":{"id":format!("{nonce:x}-progress-{step}"),"phase":format!("{phase:?}"),"message":batch_activity.into_iter().take(4).collect::<Vec<_>>().join("\n")}}),
            );
            last_progress_phase = Some(phase);
        }
        if finished || stalled {
            break;
        }
    }
    if task.need.min_tier == crate::models::Tier::Small
        && steps == call_limit
        && call_limit < task.budget.steps
        && summary == "budget exhausted"
        && !cancel.load(Ordering::SeqCst)
    {
        summary = "model-call budget reached".into();
    }
    if steps == call_limit && summary == "budget exhausted" && !cancel.load(Ordering::SeqCst) {
        summary = "model-call budget exhausted".into();
    }
    if let Err(error) = children.finish() {
        status = WorkerStatus::Unresolved;
        summary = format!("child worker failed: {error}");
    }
    for event in children.events() {
        record(event);
    }
    let mut artifacts = workspace.artifact_refs();
    artifacts.extend(children.artifacts());
    for child in children.results() {
        steps = steps.saturating_add(child.steps);
        input = input.saturating_add(child.input_tokens);
        output = output.saturating_add(child.output_tokens);
        cached = cached.saturating_add(child.cached_input_tokens);
        remote_calls = remote_calls.saturating_add(child.remote_calls);
        remote_input_tokens = remote_input_tokens
            .zip(child.remote_input_tokens)
            .map(|(a, b)| a.saturating_add(b));
        model_wait_ms = model_wait_ms.saturating_add(child.model_wait_ms);
        gateway_overhead_ms = gateway_overhead_ms.saturating_add(child.gateway_overhead_ms);
        for (phase, cost) in &child.cost_by_phase {
            ledger.costs.entry(*phase).or_default().accumulate(cost);
        }
        for (phase, bytes) in &child.phase_bytes {
            *phase_bytes.entry(phase.clone()).or_default() += bytes;
        }
        for (name, count) in &child.models {
            *models.entry(name.clone()).or_default() += count;
        }
    }
    drop(workspace);
    if cancel.load(Ordering::SeqCst) {
        status = WorkerStatus::Unresolved;
        summary = "cancelled".into();
        task.overlay.defer_cleanup();
    }
    let proposed = status == WorkerStatus::Proposed;
    let patch = if cancel.load(Ordering::SeqCst) && !mutated {
        Patch {
            base: task.overlay.base.clone(),
            files: Vec::new(),
            diff: Vec::new(),
        }
    } else {
        task.overlay.patch()?
    };
    let touched = patch.files.iter().map(|f| f.path.clone()).collect();
    let observed = work_memory.recent_activity();
    let changes = if patch.files.is_empty() {
        "none".into()
    } else {
        patch
            .files
            .iter()
            .take(6)
            .map(|file| file.path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let model_error = last_model_error
        .as_ref()
        .map(|error| format!("\nLast model error: {}", progress::short(error, 300)))
        .unwrap_or_default();
    let activity_report = format!(
        "Progress:\n{}\nIsolated-copy change proposal: {changes}\nAttempt: model requests {steps}/{} · tokens {}/{} · elapsed {}s/{}s · model wait {}s{model_error}",
        if observed.is_empty() {
            "no completed tool activity"
        } else {
            &observed
        },
        call_limit,
        input.saturating_add(output),
        task.budget.tokens,
        start.elapsed().as_secs(),
        task.budget.seconds,
        model_wait_ms / 1000
    );
    Ok(WorkerResult {
        status,
        finished: terminal_finish,
        // A reply in prose, or findings after read-only investigation, with nothing
        // changed. An immediate "done" with no tool use and no change stays unresolved.
        answered: patch.files.is_empty()
            && !mutated
            && (answered || (investigated && terminal_finish && proposed)),
        patch,
        touched,
        summary,
        plan: task.plan.clone(),
        rereads,
        cost_by_phase: ledger.costs,
        trace: store.put(&trace)?,
        artifacts,
        work_memory,
        activity_report,
        last_model_error,
        steps,
        input_tokens: input,
        remote_input_tokens,
        remote_calls,
        model_wait_ms,
        gateway_overhead_ms,
        cached_input_tokens: cached,
        output_tokens: output,
        phase_bytes,
        models,
        seconds: start.elapsed().as_secs(),
    })
}

fn parse_plan(args: &Value) -> Result<Vec<crate::sessions::PlanStep>> {
    let plan: Vec<crate::sessions::PlanStep> = serde_json::from_value(args["plan"].clone())?;
    anyhow::ensure!(plan.len() <= 40, "at most 40 plan steps");
    anyhow::ensure!(
        plan.iter().all(|p| !p.step.trim().is_empty()
            && matches!(p.status.as_str(), "pending" | "in_progress" | "completed")),
        "invalid plan status"
    );
    anyhow::ensure!(
        plan.iter().filter(|p| p.status == "in_progress").count() <= 1,
        "at most one active plan step"
    );
    Ok(plan)
}
#[allow(clippy::too_many_arguments)]
fn compact_messages(
    messages: &mut Vec<Message>,
    prefix_len: usize,
    limit: usize,
    store: &crate::base::FileArtifactStore,
    workspace: &mut tools::Workspace<'_>,
    memory: &WorkMemory,
    plan: &[crate::sessions::PlanStep],
) -> Result<()> {
    let limit = limit.max(8_000);
    if serde_json::to_vec(messages)?.len() <= limit || messages.len() <= prefix_len {
        return Ok(());
    }
    // Retaining four messages regardless of their size can still exceed the
    // context limit after a single large read. Retain as many *whole* recent
    // assistant/result turns as will fit alongside a bounded summary.
    // Serializing every candidate suffix would be quadratic in the number of
    // turns. Measure each message once, then walk suffix lengths backwards.
    let lengths = messages
        .iter()
        .map(|message| serde_json::to_vec(message).map(|value| value.len()))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let fixed = 2 + lengths[..prefix_len].iter().sum::<usize>() + prefix_len.saturating_sub(1);
    let recent_budget = limit.saturating_sub(fixed).saturating_sub(3_200);
    let mut cut = messages.len();
    let mut suffix_bytes = 2usize; // JSON array brackets
    for (retained, index) in (prefix_len + 1..messages.len()).rev().enumerate() {
        suffix_bytes += lengths[index] + usize::from(retained > 0);
        if suffix_bytes > recent_budget {
            break;
        }
        if messages[index].role == Role::Assistant {
            cut = index;
        }
    }
    // Store the full evicted transcript before shortening it. Prior archives
    // remain reachable through the archived summary's artifact reference. Use
    // one JSON message per line so read_artifact can retrieve individual turns
    // without clipping an entire archive serialized on one long line.
    let mut archive = Vec::new();
    for message in &messages[prefix_len..cut] {
        serde_json::to_writer(&mut archive, message)?;
        archive.push(b'\n');
    }
    let reference = store.put(&archive)?;
    workspace.register_artifacts(std::slice::from_ref(&reference));
    // The plan is a model proposal. Tool evidence is Core-owned observation.
    // Do not summarize arbitrary transcript tails and lose the original finding.
    let mut active_plan = String::new();
    for step in plan.iter().filter(|step| step.status != "completed") {
        let line = format!("{}: {}\n", step.status, step.step);
        if active_plan.len() + line.len() <= 600 {
            active_plan.push_str(&line);
        }
    }
    let observed = memory.render(2200);
    let recent = messages.split_off(cut);
    messages.truncate(prefix_len);
    messages.push(Message::user(format!(
        "Earlier work summary:\nActive plan (proposal):\n{active_plan}\nCore observations, not completion claims:\n{observed}\nFull conversation archive (read_artifact): {}",
        reference.id
    )));
    messages.extend(recent);
    Ok(())
}

struct CancelBridge {
    flag: std::sync::Arc<AtomicBool>,
    done: std::sync::Arc<AtomicBool>,
}
impl Drop for CancelBridge {
    fn drop(&mut self) {
        self.flag.store(true, Ordering::SeqCst);
        self.done.store(true, Ordering::SeqCst);
    }
}
