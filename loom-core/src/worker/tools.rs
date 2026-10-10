//! Core-owned code tools for the native worker (docs/loom-core-tools.md).
//!
//! The tools decide how much of the repository reaches the model: large files come back
//! as an outline unless a range is asked for, searches are grouped by symbol and capped,
//! edits require a fresh read and are rejected if they introduce a Rust syntax error.
use crate::base::{ArtifactRef, ArtifactStore, Phase};
use crate::effects::{Overlay, Runner};
use crate::models::ToolSpec;
use crate::world::graph::{CodeGraph, GraphIndex};
use crate::world::repository::RepositoryExplorer;
use crate::world::{lang::Language, map, split, structural};
use serde_json::{Value, json};
use std::sync::atomic::AtomicBool;

use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    fs,
    path::{Component, Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

const READ_MAX_LINES: usize = 400;
const OUTPUT_MAX: usize = 12_000;
const FIND_MAX_LINES: usize = 80;
const SEARCH_CAPTURE_MAX_BYTES: usize = 128 * 1024;

#[derive(Clone)]
pub(super) struct SourceFile {
    path: PathBuf,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl SourceFile {
    pub(super) fn same_file(&self, other: &Self) -> bool {
        if self.path == other.path {
            return true;
        }
        #[cfg(unix)]
        {
            self.device == other.device && self.inode == other.inode
        }
        #[cfg(not(unix))]
        {
            false
        }
    }
}

#[derive(Clone)]
pub struct ToolOutput {
    pub text: String,
    pub is_error: bool,
    pub phase: Phase,
    /// The worker should stop: `finish` was called.
    pub finish: Option<String>,
}

impl ToolOutput {
    pub(super) fn ok(phase: Phase, text: impl Into<String>) -> Self {
        Self {
            text: clip(text.into()),
            is_error: false,
            phase,
            finish: None,
        }
    }
    pub(super) fn err(phase: Phase, text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
            phase,
            finish: None,
        }
    }
}

fn clip(text: String) -> String {
    clip_limit(&text, OUTPUT_MAX)
}

// Ripgrep exempts explicit file arguments from its glob filters. Apply the same
// globset syntax ourselves before handing it a file, including negated filters.
fn file_glob_matches(pattern: &str, relative: &str) -> anyhow::Result<bool> {
    let (negated, pattern) = pattern.strip_prefix('!').map_or((false, pattern), |pattern| (true, pattern));
    let matcher = globset::GlobBuilder::new(pattern).literal_separator(true).build()?.compile_matcher();
    let matched = matcher.is_match(relative)
        || (!pattern.contains('/') && Path::new(relative).file_name().is_some_and(|name| matcher.is_match(Path::new(name))));
    Ok(if negated { !matched } else { matched })
}

fn clip_limit(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut cut = limit;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!(
        "{}\n… output truncated ({} bytes total); narrow the request",
        &text[..cut],
        text.len()
    )
}

pub struct Workspace<'a> {
    root: PathBuf,
    explorer: Arc<RepositoryExplorer>,
    graph: Arc<GraphIndex>,
    structural_cache: Arc<Mutex<StructuralCache>>,
    overlay: &'a mut Overlay,
    pub read_only: bool,
    allowed_writes: Option<BTreeSet<PathBuf>>,
    pub checks: Vec<String>,
    cancel: &'a AtomicBool,
    timeout: Duration,
    deadline: Option<Instant>,
    artifacts: HashMap<String, ArtifactRef>,
    instructions: crate::instructions::Instructions,
    skills: Vec<crate::instructions::Skill>,
    terminals: crate::effects::TerminalManager,
    policy: crate::effects::ExecutionPolicy,
    extensions: crate::extensions::Extensions,
    /// Commands started with exec_command that are still running, by session id.
    running: HashMap<String, u64>,
    /// An unambiguous top-level directory explicitly named by the user.
    default_find_scope: Option<PathBuf>,
    /// Diff at the last check and its result: an unchanged copy is not built again.
    last_check: Option<(Vec<u8>, ToolOutput)>,
    extension_web_granted: bool,
}

#[derive(Default)]
struct SharedIndexes {
    graphs: HashMap<(PathBuf, String), Arc<GraphIndex>>,
    explorers: HashMap<PathBuf, Arc<RepositoryExplorer>>,
    structural: HashMap<PathBuf, Arc<Mutex<StructuralCache>>>,
}

#[derive(Default)]
struct StructuralCache {
    revision: String,
    entries: HashMap<String, ToolOutput>,
}

fn shared_indexes(
    workspace: &Path,
    overlay_root: &Path,
) -> anyhow::Result<(
    Arc<RepositoryExplorer>,
    Arc<GraphIndex>,
    Arc<Mutex<StructuralCache>>,
)> {
    static INDEXES: OnceLock<Mutex<SharedIndexes>> = OnceLock::new();
    let workspace = workspace.canonicalize()?;
    let overlay_root = overlay_root.canonicalize()?;
    // Overlay slots are stable execution lanes: slot 0 is reused by sequential
    // workers across revisions, while concurrent workers receive slot 1, 2, ... .
    // Sharing by lane preserves incremental parses without letting concurrent
    // revisions evict one another's in-memory graph state.
    let graph_lane = overlay_root
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty() && name.bytes().all(|byte| byte.is_ascii_digit()))
        .unwrap_or("default")
        .to_owned();
    let mut indexes = INDEXES
        .get_or_init(|| Mutex::new(SharedIndexes::default()))
        .lock()
        .map_err(|_| anyhow::anyhow!("shared repository index lock failed"))?;
    // A sequential worker removes its slot directory before the next one recreates it;
    // the slot's lease file outlives that gap and goes only when the revision is swept.
    let live = |path: &PathBuf| path.exists() || path.with_extension("lock").exists();
    indexes.explorers.retain(|path, _| live(path));
    indexes.structural.retain(|path, _| live(path));
    let graph = indexes
        .graphs
        .entry((workspace, graph_lane))
        .or_insert_with(|| Arc::new(GraphIndex::default()))
        .clone();
    let explorer = match indexes.explorers.get(&overlay_root) {
        Some(explorer) => Arc::clone(explorer),
        None => {
            let explorer = Arc::new(RepositoryExplorer::new(&overlay_root)?);
            indexes
                .explorers
                .insert(overlay_root.clone(), Arc::clone(&explorer));
            explorer
        }
    };
    let structural_cache = indexes
        .structural
        .entry(overlay_root)
        .or_insert_with(|| Arc::new(Mutex::new(StructuralCache::default())))
        .clone();
    Ok((explorer, graph, structural_cache))
}

impl<'a> Workspace<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        overlay: &'a mut Overlay,
        read_only: bool,
        checks: Vec<String>,
        cancel: &'a AtomicBool,
        timeout: Duration,
        policy: crate::effects::ExecutionPolicy,
        selected_skills: &[PathBuf],
        allowed_writes: Option<BTreeSet<PathBuf>>,
    ) -> anyhow::Result<Self> {
        let deadline = Instant::now().checked_add(timeout);
        let root = overlay.dir().to_owned();
        let mut instructions = crate::instructions::Instructions::open(overlay.workspace())?;
        instructions.root_guidance()?;
        let terminals =
            crate::effects::TerminalManager::new(&root, overlay.artifacts().clone(), policy);
        let (explorer, graph, structural_cache) = shared_indexes(overlay.workspace(), &root)?;
        let mut skills = crate::instructions::discover(overlay.workspace())?;
        for path in selected_skills {
            let path = path.canonicalize()?;
            if !skills.iter().any(|s| s.path == path) {
                skills.push(crate::instructions::Skill {
                    name: path
                        .parent()
                        .and_then(|p| p.file_name())
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "selected".into()),
                    path,
                    description: String::new(),
                });
            }
        }
        Ok(Self {
            skills,
            extensions: crate::extensions::Extensions::open_for_workspace(
                overlay.workspace(),
                &root,
                policy,
            )?,
            extension_web_granted: false,
            instructions,
            terminals,
            policy,
            explorer,
            graph,
            structural_cache,
            root,
            overlay,
            read_only,
            allowed_writes,
            checks,
            cancel,
            timeout,
            deadline,
            artifacts: HashMap::new(),
            running: HashMap::new(),
            default_find_scope: None,
            last_check: None,
        })
    }

    pub fn overlay_mut(&mut self) -> &mut Overlay {
        self.overlay
    }

    fn remaining_time(&self) -> Duration {
        self.deadline.map_or(self.timeout, |deadline| {
            deadline.saturating_duration_since(Instant::now())
        })
    }

    /// Canonical file identity for local reads and known single-file edits.
    /// Directories and queries over the graph cannot use this narrower cache.
    pub(super) fn source_file(&self, name: &str, args: &Value) -> Option<SourceFile> {
        let path = match name {
            "read" | "outline" | "edit" | "write" => args["path"].as_str()?,
            "replace_symbol" | "insert_after_symbol" => args["id"].as_str()?,
            _ => return None,
        };
        let file = path.split("::").next()?;
        let (absolute, _) = self.resolve(file).ok()?;
        let canonical = absolute.canonicalize().ok()?;
        let metadata = fs::metadata(&canonical).ok()?;
        if !metadata.is_file() {
            return None;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Some(SourceFile {
                path: canonical,
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        }
        // Without a reliable file identity, aliases cannot be excluded safely.
        #[cfg(not(unix))]
        {
            None
        }
    }

    /// Only an exact directory name in the request scopes an omitted find path.
    /// Multiple mentioned directories remain unrestricted so cross-project work works.
    pub fn set_goal_find_scope(&mut self, goal: &str) {
        let matches: Vec<_> = fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| !name.starts_with('.') && name.len() >= 3 && goal.contains(name))
            .collect();
        self.default_find_scope = if matches.len() == 1 {
            Some(PathBuf::from(&matches[0]))
        } else {
            None
        };
    }
    pub fn artifact_refs(&self) -> Vec<ArtifactRef> {
        let mut references = self.artifacts.values().cloned().collect::<Vec<_>>();
        references.sort_by(|a, b| a.id.cmp(&b.id));
        references
    }
    pub fn register_artifacts(&mut self, references: &[ArtifactRef]) {
        for reference in references {
            self.artifacts
                .insert(reference.id.clone(), reference.clone());
        }
    }

    fn graph(&self) -> Option<Arc<CodeGraph>> {
        let snapshot = self.explorer.snapshot().ok()?;
        self.graph.graph(&snapshot).ok()
    }

    /// Map paths in either the original workspace or the isolated worktree into the worktree.
    fn resolve(&self, path: &str) -> Result<(PathBuf, String), String> {
        let requested = Path::new(path.trim());
        let relative = Path::new(path.trim().trim_start_matches("./"));
        if requested.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err(format!("{path}: `..` is not allowed"));
        }
        let rel = if requested.is_absolute() {
            // macOS may spell the same workspace as /var/... or /private/var/....
            // Resolve the nearest existing ancestor so even a new file can be addressed.
            let (ancestor, canonical) = requested.ancestors()
                .find_map(|ancestor| dunce::canonicalize(ancestor).ok().map(|real| (ancestor, real)))
                .ok_or_else(|| format!("{path} is outside the workspace"))?;
            let normalized = canonical.join(requested.strip_prefix(ancestor).unwrap_or(Path::new("")));
            normalized.strip_prefix(&self.root)
                .or_else(|_| normalized.strip_prefix(self.overlay.workspace()))
                .map_err(|_| format!("{path} is outside the workspace"))?
                .to_path_buf()
        } else {
            relative.to_path_buf()
        };
        let shown = rel.to_string_lossy().replace('\\', "/");
        let absolute = if rel.as_os_str().is_empty() {
            self.root.clone()
        } else {
            self.overlay.resolve(&rel).map_err(|e| e.to_string())?
        };
        Ok((absolute, shown))
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = vec![
            spec(
                "map",
                "Overview when the relevant file is unknown. If Likely relevant code names a file or symbol, read it directly or use a scoped outline first; map is ranked by global references, not relevance to the current task. Set path for a focused map.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "repository-relative directory or file (default: entire repository)"},
                        "budget": {"type": "integer", "minimum": 256, "maximum": 12000, "description": "maximum output bytes; default 3000"}
                    }
                }),
            ),
            spec(
                "plan_split",
                "Suggest connected groups for explicit symbol IDs using Loom's approximate code graph; advisory only. Use scoped spawn_agent to reserve write/read files.",
                json!({"type":"object","required":["ids"],"properties":{"ids":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":32}}}),
            ),
            spec(
                "find",
                "Find text inside files (default kind=content) or file/directory names (kind=path). For a directory listing, use outline with path '.' or the directory name. An omitted path uses the directory named in the request, if any. Content results are grouped by enclosing symbol.",
                json!({
                    "type": "object", "required": ["pattern"],
                    "properties": {
                        "pattern": {"type": "string", "description": "text regex for content search; substring of file/directory name for kind=path"},
                        "kind": {"type": "string", "enum": ["content", "path"], "description": "path searches filenames and directory names; content searches text"},
                        "path": {"type": "string", "description": "file or directory to search in (default: whole workspace)"},
                        "literal": {"type": "boolean"},
                        "include": {"type": "string", "description": "content search file glob, e.g. *.rs or **/tests/**"},
                        "case_sensitive": {"type": "boolean", "description": "content search: default true; false ignores case"},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 80, "description": "maximum matching content lines; default 80. A truncated result is a preview, not an exhaustive search."}
                    }
                }),
            ),
            spec(
                "glob",
                "Find files by glob (e.g. **/*.rs or **/tests/**) without a shell. Applies ripgrep ignore/glob precedence, returns sorted workspace-relative paths, and marks incomplete previews. Use read_many for known results; use find for their content.",
                json!({"type":"object", "required":["pattern"], "properties":{
                    "pattern":{"type":"string"},
                    "path":{"type":"string", "description":"optional workspace-relative file or directory scope"},
                    "limit":{"type":"integer", "minimum":1, "maximum":80}
                }}),
            ),
            spec(
                "symbols",
                "Search the native AST symbol index by name or Loom symbol id. Prefer this when you know a type, function, method or module name; results include stable symbol ids for read, symbol_context and refs. Optional kind/test filters narrow ambiguous names without text search.",
                json!({
                    "type": "object", "required": ["query"],
                    "properties": {
                        "query": {"type": "string", "description": "symbol name or fragment"},
                        "path": {"type": "string", "description": "optional repository-relative file or directory scope"},
                        "kind": {"type": "string", "enum": ["fn", "method", "struct", "enum", "union", "trait", "impl", "const", "static", "type", "macro", "mod", "imports", "field", "variant"]},
                        "test": {"type": "boolean", "description": "optional test-symbol filter"},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 80}
                    }
                }),
            ),
            spec(
                "symbol_context",
                "Show one AST symbol's direct structural context in one call: owner, nearby symbols in the same file, direct callers and direct callees. Use this after symbols when deciding what to read or edit.",
                json!({
                    "type": "object", "required": ["id"],
                    "properties": {
                        "id": {"type": "string", "description": "stable symbol id from symbols/outline"},
                        "siblings": {"type": "integer", "minimum": 0, "maximum": 5, "description": "nearby symbols before/after the target; default 2"}
                    }
                }),
            ),
            spec(
                "inspect_symbol",
                "Fast path for a known symbol name: resolve one unambiguous AST symbol and return its structural context plus source body in one call. If ambiguous, returns the candidates instead of guessing.",
                json!({
                    "type":"object", "required":["query"],
                    "properties":{
                        "query":{"type":"string","description":"exact symbol name or stable symbol id"},
                        "path":{"type":"string","description":"optional repository-relative file or directory scope"}
                    }
                }),
            ),
            spec(
                "file_context",
                "Show a file as a structural module node: its AST symbols plus files it references and files that reference it. Use when ownership is known at file level but the exact symbol is not.",
                json!({
                    "type":"object", "required":["path"],
                    "properties":{"path":{"type":"string","description":"repository-relative source file"}}
                }),
            ),
            spec(
                "symbol_impact",
                "Show the deterministic reverse impact of one stable symbol id: direct dependencies/callers, every transitively affected file, and reachable test symbols. Use before changing shared behavior instead of chaining refs and test searches.",
                json!({
                    "type":"object", "required":["id"],
                    "properties":{"id":{"type":"string","description":"stable symbol id from inspect_symbol/symbols/outline"}}
                }),
            ),
            spec(
                "ast_search",
                "Find syntax patterns across Rust, Python, JS/TS/TSX, Go and Java with ast-grep metavariables (e.g. `$X.unwrap()` or `save($X)`). Returns file/line and enclosing Loom symbol id.",
                json!({
                    "type": "object", "required": ["pattern"],
                    "properties": {
                        "pattern": {"type": "string"},
                        "path": {"type": "string", "description": "repository-relative directory or file (default: all)"},
                        "language": {"type": "string", "enum": ["rust", "python", "javascript", "typescript", "tsx", "go", "java"]},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 80}
                    }
                }),
            ),
            spec(
                "outline",
                "List the symbols of a file (or the immediate entries of a directory) without bodies. Use path '.' for the repository root. Use this before reading a large file.",
                json!({
                    "type": "object", "required": ["path"], "properties": {"path": {"type": "string"}}
                }),
            ),
            spec(
                "read",
                "Read lines of a file with line numbers (max 400 per call), or a symbol by id `path::Type::name`. A large file without a range returns its outline.",
                json!({
                    "type": "object", "required": ["path"],
                    "properties": {
                        "path": {"type": "string", "description": "file path, or symbol id like src/lib.rs::Walk::next"},
                        "start": {"type": "integer"}, "end": {"type": "integer"}
                    }
                }),
            ),
            spec(
                "read_many",
                "Read up to 8 already-known symbols or narrow file ranges in one call. Prefer this after symbols/symbol_context when multiple independent targets are known; each item gets a fair share of the bounded output.",
                json!({
                    "type": "object", "required": ["items"],
                    "properties": {
                        "items": {
                            "type": "array", "minItems": 1, "maxItems": 8,
                            "items": {
                                "type": "object", "required": ["path"],
                                "properties": {
                                    "path": {"type": "string", "description": "file path or stable symbol id"},
                                    "start": {"type": "integer"},
                                    "end": {"type": "integer"}
                                }
                            }
                        }
                    }
                }),
            ),
            spec(
                "read_artifact",
                "Read numbered lines from a check log, cached tool result, or conversation archive using its artifact ID (max 400 lines per call). Archives contain one JSON message per line.",
                json!({
                    "type": "object", "required": ["path", "start", "end"],
                    "properties": {
                        "path": {"type": "string", "description": "artifact_path from check"},
                        "start": {"type": "integer", "description": "first line, 1-based, inclusive"},
                        "end": {"type": "integer", "description": "last line, 1-based, inclusive"}
                    }
                }),
            ),
            spec(
                "git_status",
                "Show the current isolated worktree branch and changed/untracked paths. Prefer this over shell git status.",
                json!({"type":"object","properties":{}}),
            ),
            spec(
                "git_diff",
                "Show the current isolated worktree diff. Optionally limit to one repository-relative path or inspect the staged diff.",
                json!({
                    "type":"object",
                    "properties":{
                        "path":{"type":"string","description":"optional repository-relative file or directory"},
                        "staged":{"type":"boolean","description":"show staged/index diff instead of working-tree diff"}
                    }
                }),
            ),
            spec(
                "git_log",
                "Show recent commits in compact one-line form. Optionally limit history to one repository-relative path.",
                json!({
                    "type":"object",
                    "properties":{
                        "path":{"type":"string","description":"optional repository-relative file or directory"},
                        "limit":{"type":"integer","minimum":1,"maximum":50}
                    }
                }),
            ),
            spec(
                "git_show",
                "Show a commit or revision, optionally limited to one repository-relative path.",
                json!({
                    "type":"object",
                    "properties":{
                        "rev":{"type":"string","description":"revision or commit; default HEAD"},
                        "path":{"type":"string","description":"optional repository-relative file or directory"}
                    }
                }),
            ),
            spec(
                "git_blame",
                "Show line provenance for one repository-relative file, optionally limited to a line range.",
                json!({
                    "type":"object", "required":["path"],
                    "properties":{
                        "path":{"type":"string"},
                        "start":{"type":"integer","minimum":1},
                        "end":{"type":"integer","minimum":1}
                    }
                }),
            ),
            spec(
                "refs",
                "List symbols that reference a given symbol (callers) and the symbols it references (callees).",
                json!({
                    "type": "object", "required": ["id"],
                    "properties": {"id": {"type": "string", "description": "symbol id like src/lib.rs::Type::name"}}
                }),
            ),
        ];
        if !self.read_only {
            specs.push(spec("edit", "Replace exact text in a file you have read. `old` must match the file (without line-number prefixes) and be unique unless replace_all. Small whitespace differences are tolerated and reported.", json!({
                "type": "object", "required": ["path", "old", "new"],
                "properties": {"path": {"type": "string"}, "old": {"type": "string"}, "new": {"type": "string"}, "replace_all": {"type": "boolean"}}
            })));
            specs.push(spec(
                "write",
                "Create a new file. To change an existing file use edit.",
                json!({
                    "type": "object", "required": ["path", "content"],
                    "properties": {"path": {"type": "string"}, "content": {"type": "string"}}
                }),
            ));
            specs.push(spec(
                "check",
                "Build and test the workspace. Returns JSON status, test counts, failing test names, first compiler error with location, and an artifact path for the full raw log.",
                json!({
                    "type": "object", "properties": {}
                }),
            ));
            specs.push(spec(
                "diagnostics",
                "Run the nearest project's fast compile/type diagnostics without the full test suite. Optionally provide a file path to select a project.",
                json!({
                    "type":"object",
                    "properties":{"path":{"type":"string","description":"optional repository-relative file used to locate the nearest project"}}
                }),
            ));
        }
        specs.push(spec(
            "use_skill",
            "Load the complete SKILL.md for an available skill before using it.",
            json!({"type":"object","required":["name"],"properties":{"name":{"type":"string"}}}),
        ));
        specs.push(spec("read_skill_resource","Read a supporting file relative to an available skill's directory.",json!({"type":"object","required":["skill","path"],"properties":{"skill":{"type":"string"},"path":{"type":"string"}}})));
        specs.push(spec("update_plan", "Record a plan with pending/in_progress/completed steps; at most one step in progress.",json!({"type":"object","required":["plan"],"properties":{"plan":{"type":"array","items":{"type":"object","required":["step","status"],"properties":{"step":{"type":"string"},"status":{"type":"string","enum":["pending","in_progress","completed"]}}}}}})));
        specs.push(spec("exec_command", "Run a command in the isolated workspace under the execution policy. Returns a PTY session_id while running and a raw artifact.",json!({"type":"object","required":["command"],"properties":{"command":{"type":"string"},"yield_ms":{"type":"integer"},"timeout_seconds":{"type":"integer"}}})));
        specs.push(spec("write_stdin", "Write input or poll a live PTY command.",json!({"type":"object","required":["session_id"],"properties":{"session_id":{"type":"integer"},"chars":{"type":"string"},"yield_ms":{"type":"integer"}}})));
        if !self.read_only {
            specs.push(spec("rename_symbol","Rename a unique Rust symbol and references, preserving comments and strings.",json!({"type":"object","required":["id","new_name"],"properties":{"id":{"type":"string"},"new_name":{"type":"string"}}})));
            specs.push(spec("replace_symbol","Replace a symbol body after reading it.",json!({"type":"object","required":["id","content"],"properties":{"id":{"type":"string"},"content":{"type":"string"}}})));
            specs.push(spec("insert_after_symbol","Insert text immediately after a symbol you have read.",json!({"type":"object","required":["id","content"],"properties":{"id":{"type":"string"},"content":{"type":"string"}}})));
        }
        specs.push(spec("finish", "Stop with a proposal summary. For a conversational answer with no requested code edits, set answer=true and put your answer in summary. For edits, propose actual changes before finishing. If you cannot solve the task, set unresolved=true and explain why. Core decides whether a proposal is accepted.", json!({
            "type": "object", "required": ["summary"], "properties": {"summary": {"type": "string"}, "unresolved": {"type": "boolean"}, "answer": {"type": "boolean", "description": "True only for a conversational reply or explanation that needs no code changes"}}
        })));
        specs.extend(
            self.extensions
                .specs()
                .into_iter()
                .filter(|spec| !self.read_only || self.extensions.is_read_only(&spec.name)),
        );
        // Keep web tools discoverable even when the current sandbox denies the
        // network. Calling them then triggers an explicit one-task approval.
        if !self.policy.network {
            specs.push(spec(
                "web_fetch",
                "Fetch an HTTP(S) text page; requests network approval when disabled.",
                json!({
                    "type":"object", "required":["url"], "properties":{"url":{"type":"string"}}
                }),
            ));
            specs.push(spec("web_search", "Search configured SearxNG/Brave; requests network approval when disabled.", json!({
                "type":"object", "required":["query"], "properties":{"query":{"type":"string"}, "count":{"type":"integer","minimum":1,"maximum":20}}
            })));
        }
        specs
    }

    /// Only an explicit consent response may elevate the extension's network
    /// policy; terminal commands and the parent task keep their original policy.
    pub(crate) fn web_permission_required(&self, name: &str) -> bool {
        !self.extensions_network_allowed() && matches!(name, "web_fetch" | "web_search")
    }

    fn extensions_network_allowed(&self) -> bool {
        self.extension_web_granted || self.policy.network
    }

    pub(crate) fn grant_web_for_task(&mut self) -> anyhow::Result<()> {
        let mut policy = self.policy;
        policy.network = true;
        self.extensions = crate::extensions::Extensions::open_for_workspace(
            self.overlay.workspace(),
            &self.root,
            policy,
        )?;
        self.extension_web_granted = true;
        Ok(())
    }

    pub(crate) fn extension_is_read_only(&self, name: &str) -> bool {
        self.extensions.is_read_only(name)
    }

    pub(crate) fn exploration_is_stable(&self) -> bool {
        // A command may change files between calls until it is observed exited.
        self.running.is_empty()
    }

    fn structural_cache_key(&self, name: &str, args: &Value) -> Option<(String, String)> {
        if !matches!(
            name,
            "map"
                | "plan_split"
                | "symbols"
                | "symbol_context"
                | "file_context"
                | "symbol_impact"
                | "ast_search"
                | "refs"
        ) || !self.exploration_is_stable()
        {
            return None;
        }
        let revision = self.explorer.snapshot().ok()?.revision.clone();
        Some((revision, format!("{name}:{args}")))
    }

    fn cached_structural(&self, name: &str, args: &Value) -> Option<ToolOutput> {
        let (revision, key) = self.structural_cache_key(name, args)?;
        let mut cache = self.structural_cache.lock().ok()?;
        if cache.revision != revision {
            cache.revision = revision;
            cache.entries.clear();
        }
        cache.entries.get(&key).cloned()
    }

    fn remember_structural(&self, name: &str, args: &Value, output: &ToolOutput) {
        if output.is_error {
            return;
        }
        let Some((revision, key)) = self.structural_cache_key(name, args) else {
            return;
        };
        let Ok(mut cache) = self.structural_cache.lock() else {
            return;
        };
        if cache.revision != revision {
            cache.revision = revision;
            cache.entries.clear();
        }
        cache.entries.insert(key, output.clone());
    }

    pub fn call(&mut self, name: &str, args: &Value) -> ToolOutput {
        if let Some(cached) = self.cached_structural(name, args) {
            return cached;
        }
        let s = |k: &str| args[k].as_str().unwrap_or_default();
        if let Some(allowed) = &self.allowed_writes {
            // Terminal commands and renames have unknown write sets. A scoped
            // worker uses only bounded Core edits; checks run in its overlay.
            if matches!(name, "exec_command" | "write_stdin" | "rename_symbol")
                || (self.extensions.specs().iter().any(|spec| spec.name == name)
                    && !self.extensions.is_read_only(name))
            {
                return ToolOutput::err(
                    Phase::Orient,
                    "WRITE_SCOPE_CONFLICT: unbounded mutation unavailable in scoped worker",
                );
            }
            if matches!(
                name,
                "edit" | "write" | "replace_symbol" | "insert_after_symbol"
            ) {
                let key = if name.ends_with("symbol") {
                    s("id")
                } else {
                    s("path")
                };
                let file = key.split("::").next().unwrap_or_default();
                let rel = Path::new(file.strip_prefix("./").unwrap_or(file));
                if rel.is_absolute()
                    || rel.components().any(|c| !matches!(c, Component::Normal(_)))
                    || !allowed.contains(rel)
                {
                    return ToolOutput::err(
                        Phase::Orient,
                        format!("WRITE_SCOPE_CONFLICT: `{file}` is outside declared write_files"),
                    );
                }
            }
        }
        let mut guidance = String::new();
        if matches!(
            name,
            "read" | "edit" | "write" | "rename_symbol" | "replace_symbol" | "insert_after_symbol"
        ) {
            let key = if name.ends_with("symbol") {
                s("id")
            } else {
                s("path")
            };
            let path = key.split("::").next().unwrap_or_default();
            // Absolute paths under the user's workspace are accepted by the
            // file tools. Scope the instructions to that same relative path.
            let scoped = if Path::new(path).is_absolute() {
                match self.resolve(path) {
                    Ok((_, relative)) => relative,
                    Err(error) => return ToolOutput::err(Phase::Orient, error),
                }
            } else {
                path.to_owned()
            };
            match self.instructions.for_path(Path::new(&scoped)) {
                Ok(text) => guidance = text,
                Err(error) => return ToolOutput::err(Phase::Orient, error.to_string()),
            }
            if !guidance.is_empty()
                && matches!(
                    name,
                    "edit" | "write" | "rename_symbol" | "replace_symbol" | "insert_after_symbol"
                )
            {
                return ToolOutput::err(
                    Phase::Orient,
                    format!(
                        "Read these scoped instructions before editing, then retry:\n{guidance}"
                    ),
                );
            }
        }
        let mut result = match name {
            "map" => self.map(s("path"), args["budget"].as_u64()),
            "plan_split" => self.plan_split(&args["ids"]),
            "symbols" => self.symbols(
                s("query"),
                s("path"),
                s("kind"),
                args["test"].as_bool(),
                args["limit"].as_u64(),
            ),
            "symbol_context" => self.symbol_context(s("id"), args["siblings"].as_u64()),
            "inspect_symbol" => self.inspect_symbol(s("query"), s("path")),
            "file_context" => self.file_context(s("path")),
            "symbol_impact" => self.symbol_impact(s("id")),
            "ast_search" => self.ast_search(
                s("pattern"),
                s("path"),
                s("language"),
                args["limit"].as_u64(),
            ),
            "find" => self.find(
                s("pattern"),
                args["path"].as_str(),
                args["literal"].as_bool().unwrap_or(false),
                s("kind"),
                args,
            ),
            "glob" => self.glob(s("pattern"), args["path"].as_str(), args["limit"].as_u64()),
            "outline" => self.outline(s("path")),
            "read" => self.read_tool(s("path"), args["start"].as_u64(), args["end"].as_u64()),
            "read_many" => self.read_many(&args["items"]),
            "read_artifact" => {
                self.read_artifact(s("path"), args["start"].as_u64(), args["end"].as_u64())
            }
            "git_status" => self.git_status(),
            "git_diff" => self.git_diff(
                args["path"].as_str(),
                args["staged"].as_bool().unwrap_or(false),
            ),
            "git_log" => self.git_log(args["path"].as_str(), args["limit"].as_u64()),
            "git_show" => self.git_show(args["rev"].as_str(), args["path"].as_str()),
            "git_blame" => self.git_blame(s("path"), args["start"].as_u64(), args["end"].as_u64()),
            "refs" => self.refs(s("id")),
            "edit" if !self.read_only => self.edit(
                s("path"),
                s("old"),
                s("new"),
                args["replace_all"].as_bool().unwrap_or(false),
            ),
            "write" if !self.read_only => self.write(s("path"), s("content")),
            "check" if !self.read_only => self.check(),
            "diagnostics" if !self.read_only => self.diagnostics(args["path"].as_str()),
            "exec_command" if self.running.contains_key(s("command").trim()) => {
                // The same command started again while it still runs (weak models do this
                // when they see no output yet): point at the live session instead.
                let session = self.running[s("command").trim()];
                ToolOutput::ok(
                    Phase::Orient,
                    format!(
                        "`{}` is already running as session {session}. Wait for it with write_stdin {{\"session_id\":{session},\"yield_ms\":30000}} and no chars; do not start it again.",
                        s("command").trim()
                    ),
                )
            }
            "exec_command" => self.terminal_output(
                Some(s("command").trim().to_owned()),
                None,
                self.terminals.exec(
                    s("command"),
                    // Most commands finish within seconds; returning after 100 ms showed an
                    // empty "running" result that models took for failure and retried.
                    args["yield_ms"]
                        .as_u64()
                        .unwrap_or(10_000)
                        .min(self.remaining_time().as_millis().min(u64::MAX as u128) as u64),
                    args["timeout_seconds"]
                        .as_u64()
                        .map(Duration::from_secs)
                        .unwrap_or_else(|| self.remaining_time())
                        .min(self.remaining_time()),
                    self.cancel,
                ),
            ),
            "write_stdin" => self.terminal_output(
                None,
                args["session_id"].as_u64(),
                self.terminals.write_stdin(
                    args["session_id"].as_u64().unwrap_or(0),
                    s("chars"),
                    args["yield_ms"]
                        .as_u64()
                        .unwrap_or(if s("chars").is_empty() { 10_000 } else { 500 })
                        .min(self.remaining_time().as_millis().min(u64::MAX as u128) as u64),
                    self.cancel,
                ),
            ),
            "use_skill" => {
                match crate::instructions::discover(self.overlay.workspace()).and_then(|skills| {
                    let skill = skills
                        .into_iter()
                        .find(|skill| skill.name == s("name"))
                        .ok_or_else(|| anyhow::anyhow!("unknown skill"))?;
                    crate::instructions::load_skill(&skill.path)
                }) {
                    Ok(text) => ToolOutput::ok(Phase::Orient, text),
                    Err(error) => ToolOutput::err(Phase::Orient, error.to_string()),
                }
            }
            "read_skill_resource" => {
                let result = (|| -> anyhow::Result<String> {
                    let skill = self
                        .skills
                        .iter()
                        .find(|skill| skill.name == s("skill"))
                        .ok_or_else(|| anyhow::anyhow!("unknown skill"))?;
                    let directory = skill.path.parent().unwrap().canonicalize()?;
                    let path = directory.join(s("path")).canonicalize()?;
                    anyhow::ensure!(
                        path.starts_with(&directory),
                        "skill resource escapes its directory"
                    );
                    let text = std::fs::read_to_string(path)?;
                    anyhow::ensure!(text.len() <= 64_000, "skill resource exceeds 64KB");
                    Ok(text)
                })();
                match result {
                    Ok(text) => ToolOutput::ok(Phase::Orient, text),
                    Err(error) => ToolOutput::err(Phase::Orient, error.to_string()),
                }
            }
            "rename_symbol" if !self.read_only => {
                match crate::world::World::open(self.overlay.dir())
                    .and_then(|world| self.overlay.rename(s("id"), s("new_name"), &world))
                {
                    Ok(result) => ToolOutput::ok(Phase::Edit, result.summary),
                    Err(error) => ToolOutput::err(Phase::Edit, error.to_string()),
                }
            }
            "replace_symbol" | "insert_after_symbol" if !self.read_only => {
                self.symbol_edit(s("id"), s("content"), name == "insert_after_symbol")
            }
            "finish" => ToolOutput {
                text: "ok".into(),
                is_error: false,
                phase: Phase::Message,
                finish: Some(s("summary").to_owned()),
            },
            other
                if matches!(other, "web_fetch" | "web_search")
                    || self
                        .extensions
                        .specs()
                        .iter()
                        .any(|spec| spec.name == other) =>
            {
                if self.read_only && !self.extensions.is_read_only(other) {
                    ToolOutput::err(
                        Phase::Orient,
                        "read-only worker cannot call a mutating extension",
                    )
                } else if !self.extensions_network_allowed()
                    && self.extensions.requires_network(other)
                {
                    ToolOutput::err(
                        Phase::Orient,
                        "network access is disabled by execution policy",
                    )
                } else {
                    match self
                        .extensions
                        .call(other, args, self.cancel, self.remaining_time())
                    {
                        Ok(text) => ToolOutput::ok(Phase::Orient, text),
                        Err(error) => ToolOutput::err(Phase::Orient, error.to_string()),
                    }
                }
            }
            other => ToolOutput::err(Phase::Orient, format!("unknown tool `{other}`")),
        };
        if !guidance.is_empty() {
            result.text.push_str(&guidance);
        }
        self.remember_structural(name, args, &result);
        result
    }
    fn terminal_output(
        &mut self,
        command: Option<String>,
        session: Option<u64>,
        result: anyhow::Result<crate::effects::TerminalOutput>,
    ) -> ToolOutput {
        match result {
            Ok(output) => {
                let failed = output.exit_code.is_some_and(|exit| exit != 0);
                self.artifacts
                    .insert(output.artifact.id.clone(), output.artifact.clone());
                let mut value = serde_json::to_value(&output).unwrap();
                match (output.exit_code, output.session_id) {
                    (None, Some(session)) => {
                        if let Some(command) = command {
                            self.running.insert(command, session);
                        }
                        value["note"] = format!(
                            "still running. Wait with write_stdin {{\"session_id\":{session},\"yield_ms\":30000}} and no chars; send chars only to answer a prompt. Do not start the command again."
                        ).into();
                    }
                    _ => {
                        // finished: forget it so the same command may run again later
                        let done = output.session_id.or(session);
                        self.running.retain(|_, live| Some(*live) != done);
                        if let Some(command) = command {
                            self.running.remove(&command);
                        }
                    }
                }
                // Clip the output field, never serialized JSON. Raw PTY bytes
                // stay complete in the artifact even when the model's view is bounded.
                let mut text = value.to_string();
                if text.len() > OUTPUT_MAX {
                    value["output_truncated"] = true.into();
                    let mut cap = output.output.len().min(OUTPUT_MAX);
                    loop {
                        while !output.output.is_char_boundary(cap) {
                            cap -= 1;
                        }
                        value["output"] = output.output[..cap].into();
                        text = value.to_string();
                        if text.len() <= OUTPUT_MAX {
                            break;
                        }
                        cap /= 2;
                    }
                }
                ToolOutput {
                    text,
                    is_error: failed,
                    phase: Phase::Orient,
                    finish: None,
                }
            }
            Err(error) => ToolOutput::err(Phase::Orient, error.to_string()),
        }
    }
    fn symbol_edit(&mut self, id: &str, content: &str, insert: bool) -> ToolOutput {
        let Some(graph) = self.graph() else {
            return ToolOutput::err(Phase::Orient, "graph unavailable");
        };
        let Some(symbol) = graph.get(id) else {
            return ToolOutput::err(Phase::Orient, "unknown symbol");
        };
        let result = (|| -> anyhow::Result<_> {
            let path = Path::new(&symbol.file);
            let text = std::fs::read_to_string(self.overlay.resolve(path)?)?;
            let lines = text.lines().collect::<Vec<_>>();
            let old = lines[symbol.start.saturating_sub(1)..symbol.end.min(lines.len())].join("\n");
            let replacement = if insert {
                format!("{old}\n{content}")
            } else {
                content.to_owned()
            };
            self.overlay.edit(path, &old, &replacement)
        })();
        match result {
            Ok(output) => ToolOutput::ok(Phase::Edit, output.summary),
            Err(error) => ToolOutput::err(Phase::Edit, error.to_string()),
        }
    }

    // ------------------------------------------------------------------ read side

    fn find(&self, pattern: &str, path: Option<&str>, literal: bool, kind: &str, options: &Value) -> ToolOutput {
        if pattern.trim().len() < 2 {
            return ToolOutput::err(Phase::Orient, "pattern too short");
        }
        if kind == "path" {
            return self.find_paths(pattern, path);
        }
        let target = if path == Some(".") {
            self.root.clone()
        } else {
            match path.map(|p| self.resolve(p)) {
                Some(Err(e)) => return ToolOutput::err(Phase::Orient, e),
                Some(Ok((abs, _))) => abs,
                None => self
                    .default_find_scope
                    .as_ref()
                    .map_or_else(|| self.root.clone(), |scope| self.root.join(scope)),
            }
        };
        let mut cmd = Command::new("rg");
        if target.is_file() && let Some(include) = options["include"].as_str() {
            match file_glob_matches(include, &self.rel(target.to_string_lossy().as_ref())) {
                Ok(false) => return ToolOutput::ok(Phase::Orient, "No text matches within the file include filter."),
                Ok(true) => {},
                Err(error) => return ToolOutput::err(Phase::Orient, format!("invalid include glob: {error}")),
            }
        }
        let limit = options["limit"].as_u64().unwrap_or(FIND_MAX_LINES as u64).clamp(1, FIND_MAX_LINES as u64) as usize;
        cmd.args([
            "--no-config",
            "-n",
            "--with-filename",
            "--no-heading",
            "--color=never",
            "--max-columns=200",
            "--max-columns-preview",
        ]);
        cmd.arg(format!("--max-count={}", limit + 1));
        if literal {
            cmd.arg("-F");
        }
        if let Some(include) = options["include"].as_str() { cmd.args(["-g", include]); }
        cmd.arg(if options["case_sensitive"] == false { "-i" } else { "--case-sensitive" });
        cmd.arg("--").arg(pattern).arg(&target).current_dir(&self.root);
        let (out, capture_truncated) = match crate::base::cancellable_output_bounded(&mut cmd, self.cancel, SEARCH_CAPTURE_MAX_BYTES, self.remaining_time()) {
            Ok(out) => out,
            Err(e) => return ToolOutput::err(Phase::Orient, format!("search failed: {e}")),
        };
        if !out.status.success() && out.status.code() != Some(1) {
            return ToolOutput::err(
                Phase::Orient,
                clip(format!(
                    "ripgrep failed (exit {}):\n{}\nUse literal=true for an exact source fragment, or repair the regex/path.",
                    out.status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&out.stderr)
                )),
            );
        }
        let mut text = String::from_utf8_lossy(&out.stdout);
        if capture_truncated && !text.ends_with('\n') {
            text = std::borrow::Cow::Owned(text.rsplit_once('\n').map_or(String::new(), |(prefix, _)| prefix.to_owned()));
        }
        let lines: Vec<&str> = text.lines().collect();
        if lines.is_empty() {
            // A phrase with no exact hit can still identify a useful group of
            // files. Compute the intersection locally instead of making the
            // model guess several new patterns in separate paid requests.
            let words: Vec<_> = pattern.split_whitespace().collect();
            if (2..=4).contains(&words.len())
                && words
                    .iter()
                    .all(|word| word.chars().all(|c| c.is_alphanumeric() || c == '_'))
            {
                let mut common: Option<BTreeSet<String>> = None;
                for word in words {
                    let mut command = Command::new("rg");
                    command.args(["-l", "-i", "-F"]);
                    if let Some(include) = options["include"].as_str() { command.args(["-g", include]); }
                    command.arg("--").arg(word).arg(&target).current_dir(&self.root);
                    let files = crate::base::cancellable_output_bounded(&mut command, self.cancel, SEARCH_CAPTURE_MAX_BYTES, self.remaining_time());
                    let Ok((files, false)) = files else {
                        // A partial intersection could falsely suggest exhaustive candidates.
                        common = None;
                        break;
                    };
                    if !files.status.success() && files.status.code() != Some(1) {
                        break;
                    }
                    let found: BTreeSet<String> = String::from_utf8_lossy(&files.stdout)
                        .lines()
                        .map(|line| self.rel(line))
                        .collect();
                    common = Some(match common {
                        Some(previous) => previous.intersection(&found).cloned().collect(),
                        None => found,
                    });
                }
                if let Some(files) = common.filter(|files| !files.is_empty()) {
                    return ToolOutput::ok(
                        Phase::Orient,
                        format!(
                            "no exact match for `{pattern}`; all words occur in {} files within {}. Candidate files (read or narrow by path):\n{}",
                            files.len(),
                            self.rel(target.to_string_lossy().as_ref()),
                            files.into_iter().take(24).collect::<Vec<_>>().join("\n")
                        ),
                    );
                }
            }
            return ToolOutput::ok(Phase::Orient, format!(
                "no text matches for `{pattern}`; this does not mean the directory is missing. \
                 To list the repository use outline {{\"path\":\".\"}}; to find a filename \
                 use find {{\"pattern\":\"{pattern}\",\"kind\":\"path\"}}."
            ));
        }
        let truncated = capture_truncated || lines.len() > limit;
        let graph = self.graph();
        let mut out = if truncated {
            format!("truncated search preview: first {limit} matching lines or fewer; narrow path/include/pattern for complete results.\n")
        } else { String::new() };
        for line in lines.into_iter().take(limit) {
            let Some((file, no, body)) = split_rg_line(line) else {
                continue;
            };
            let rel = self.rel(file);
            let symbol = no.parse::<usize>().ok().and_then(|n| {
                graph
                    .as_ref()?
                    .symbol_at(&rel, n)
                    .map(|s| s.id.split("::").skip(1).collect::<Vec<_>>().join("::"))
            });
            match symbol {
                Some(sym) => out.push_str(&format!("{rel}:{no} [{sym}] {}\n", body.trim())),
                None => out.push_str(&format!("{rel}:{no} {}\n", body.trim())),
            }
        }
        ToolOutput::ok(Phase::Orient, out)
    }

    fn glob(&self, pattern: &str, path: Option<&str>, limit: Option<u64>) -> ToolOutput {
        if pattern.trim().is_empty() {
            return ToolOutput::err(Phase::Orient, "glob needs a file pattern, e.g. **/*.rs");
        }
        let target = match path {
            None | Some(".") => self.root.clone(),
            Some(path) => match self.resolve(path) {
                Ok((absolute, _)) => absolute,
                Err(error) => return ToolOutput::err(Phase::Orient, error),
            },
        };
        let mut command = Command::new("rg");
        if target.is_file() {
            match file_glob_matches(pattern, &self.rel(target.to_string_lossy().as_ref())) {
                Ok(false) => return ToolOutput::ok(Phase::Orient, "No files match this glob within the scope."),
                Ok(true) => {},
                Err(error) => return ToolOutput::err(Phase::Orient, format!("invalid file glob: {error}")),
            }
        }
        command.args(["--no-config", "--files", "--sort=path", "-g", pattern, "--"]).arg(target).current_dir(&self.root);
        let (output, capture_truncated) = match crate::base::cancellable_output_bounded(&mut command, self.cancel, SEARCH_CAPTURE_MAX_BYTES, self.remaining_time()) {
            Ok(output) => output,
            Err(error) => return ToolOutput::err(Phase::Orient, error.to_string()),
        };
        if !output.status.success() && output.status.code() != Some(1) {
            return ToolOutput::err(Phase::Orient, format!("glob failed: {}", String::from_utf8_lossy(&output.stderr)));
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let mut paths = text.lines().map(|path| self.rel(path)).collect::<Vec<_>>();
        if capture_truncated && !text.ends_with('\n') { paths.pop(); }
        let limit = limit.unwrap_or(40).clamp(1, 80) as usize;
        let truncated = capture_truncated || paths.len() > limit;
        paths.truncate(limit);
        ToolOutput::ok(Phase::Orient, if paths.is_empty() {
            "No files match this glob within the scope and ignore rules.".into()
        } else {
            format!("{}{}\n{}", if truncated { "truncated file preview · " } else { "files · " }, paths.len(), paths.join("\n"))
        })
    }

    fn find_paths(&self, pattern: &str, path: Option<&str>) -> ToolOutput {
        let target = match path {
            Some(".") | None => self.root.clone(),
            Some(path) => match self.resolve(path) {
                Ok((absolute, _)) => absolute,
                Err(error) => return ToolOutput::err(Phase::Orient, error),
            },
        };
        if !target.exists() {
            return ToolOutput::err(Phase::Orient, format!(
                "search scope does not exist: {}. Use outline with path '.' to see the repository root.",
                self.rel(target.to_string_lossy().as_ref())
            ));
        }
        let needle = pattern.trim().trim_matches('/').to_lowercase();
        if needle.is_empty() {
            return ToolOutput::err(Phase::Orient, "find kind=path needs part of a file or directory name; \
                to list the repository root use outline {\"path\":\".\"}");
        }
        if self.root.file_name().is_some_and(|name| name.to_string_lossy().to_lowercase().contains(&needle))
            && target == self.root
        {
            return self.outline(".");
        }
        let output = match Command::new("rg").args(["--files", "--"]).arg(&target)
            .current_dir(&self.root).output()
        {
            Ok(output) => output,
            Err(error) => return ToolOutput::err(Phase::Orient, format!("ripgrep unavailable: {error}")),
        };
        if !output.status.success() && output.status.code() != Some(1) {
            return ToolOutput::err(Phase::Orient, format!(
                "cannot list repository files: {}", String::from_utf8_lossy(&output.stderr)
            ));
        }
        let mut found = BTreeSet::new();
        for file in String::from_utf8_lossy(&output.stdout).lines() {
            let relative = self.rel(file);
            let path = Path::new(&relative);
            // Report the shallowest path where the name matches: a matching directory
            // stands for its contents instead of listing every file beneath it.
            let mut prefix = PathBuf::new();
            for component in path.components() {
                prefix.push(component.as_os_str());
                let candidate = prefix.to_string_lossy().replace('\\', "/");
                if candidate.to_lowercase().contains(&needle) {
                    let directory = prefix != path;
                    found.insert(format!("{}{}", candidate, if directory { "/" } else { "" }));
                    break;
                }
            }
        }
        if found.is_empty() {
            return ToolOutput::ok(Phase::Orient, format!(
                "no file or directory names match `{pattern}`; inspect the root using outline {{\"path\":\".\"}}"
            ));
        }
        let count = if found.len() > 40 { format!("first 40 of {}", found.len()) } else { found.len().to_string() };
        ToolOutput::ok(Phase::Orient, format!(
            "path matches for `{pattern}` ({count}):\n{}\nUse outline on a directory or read on a file.",
            found.into_iter().take(40).collect::<Vec<_>>().join("\n")
        ))
    }

    pub fn find_anchor_candidates(&self, task: &str) -> String {
        let graph = match self.graph() {
            Some(g) => g,
            None => return String::new(),
        };

        let mut words = Vec::new();
        // Extract quoted strings
        let mut in_quotes = false;
        let mut quote_start = 0;
        for (i, c) in task.char_indices() {
            if c == '`' || c == '"' || c == '\'' {
                if in_quotes {
                    let s = &task[quote_start..i];
                    if s.len() >= 3 && !s.contains(' ') {
                        words.push(s.to_string());
                    }
                    in_quotes = false;
                } else {
                    in_quotes = true;
                    quote_start = i + c.len_utf8();
                }
            }
        }

        // Extract identifier-like words
        for part in task.split(|c: char| !c.is_alphanumeric() && c != '_') {
            if part.len() >= 3 && !words.contains(&part.to_string()) {
                words.push(part.to_string());
            }
        }

        let mut matches = Vec::new();
        for word in &words {
            for sym in graph.named(word) {
                let entry = format!("{}:{}..{} ({})", sym.file, sym.start, sym.end, sym.id);
                if !matches.contains(&entry) {
                    matches.push(entry);
                }
            }
        }

        let mut result = String::new();
        for m in matches {
            if result.len() + m.len() + 1 > 1500 {
                break;
            }
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(&m);
        }
        result
    }

    fn rel(&self, path: &str) -> String {
        Path::new(path)
            .strip_prefix(&self.root)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| path.trim_start_matches("./").to_owned())
            .replace(std::path::MAIN_SEPARATOR, "/")
    }

    fn map(&self, path: &str, budget: Option<u64>) -> ToolOutput {
        let scope = if path.trim().is_empty() || path.trim() == "." {
            String::new()
        } else {
            match self.resolve(path) {
                Ok((_, relative)) => relative,
                Err(error) => return ToolOutput::err(Phase::Orient, error),
            }
        };
        let snapshot = match self.explorer.snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => return ToolOutput::err(Phase::Orient, error.to_string()),
        };
        let graph = match self.graph.graph(&snapshot) {
            Ok(graph) => graph,
            Err(error) => return ToolOutput::err(Phase::Orient, error.to_string()),
        };
        let paths = snapshot
            .paths()
            .map(|path| path.to_string_lossy().replace('\\', "/"))
            .collect::<Vec<_>>();
        let budget = budget.unwrap_or(3000).clamp(256, OUTPUT_MAX as u64) as usize;
        ToolOutput::ok(Phase::Orient, map::render(&graph, &paths, &scope, budget))
    }

    fn plan_split(&self, ids: &Value) -> ToolOutput {
        let Some(ids) = ids.as_array() else {
            return ToolOutput::err(Phase::Orient, "ids must be an array of symbol IDs");
        };
        let ids: Vec<String> = match ids
            .iter()
            .map(|id| id.as_str().map(ToOwned::to_owned))
            .collect::<Option<_>>()
        {
            Some(ids) => ids,
            None => return ToolOutput::err(Phase::Orient, "ids must contain only strings"),
        };
        let Some(graph) = self.graph() else {
            return ToolOutput::err(Phase::Orient, "graph unavailable");
        };
        match split::candidates(&graph, &ids) {
            Ok(groups) => ToolOutput::ok(Phase::Orient, json!({
                "revision":graph.revision,
                "advisory":true,
                "reason":"name-based references are approximate; explicit file scopes and patch validation are required",
                "groups":groups,
            }).to_string()),
            Err(error) => ToolOutput::err(Phase::Orient, error),
        }
    }

    fn ast_search(
        &self,
        pattern: &str,
        path: &str,
        language: &str,
        limit: Option<u64>,
    ) -> ToolOutput {
        let lang = match language {
            "" => None,
            "rust" => Some(Language::Rust),
            "python" => Some(Language::Python),
            "javascript" => Some(Language::JavaScript),
            "typescript" => Some(Language::TypeScript),
            "tsx" => Some(Language::Tsx),
            "go" => Some(Language::Go),
            "java" => Some(Language::Java),
            other => {
                return ToolOutput::err(Phase::Orient, format!("unsupported language: {other}"));
            }
        };
        let scope = if path.trim().is_empty() || path.trim() == "." {
            String::new()
        } else {
            match self.resolve(path) {
                Ok((_, relative)) => relative,
                Err(error) => return ToolOutput::err(Phase::Orient, error),
            }
        };
        let snapshot = match self.explorer.snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => return ToolOutput::err(Phase::Orient, error.to_string()),
        };
        let graph = match self.graph.graph(&snapshot) {
            Ok(graph) => graph,
            Err(error) => return ToolOutput::err(Phase::Orient, error.to_string()),
        };
        match structural::search(
            &snapshot,
            &graph,
            pattern,
            &scope,
            lang,
            limit.unwrap_or(30).clamp(1, 80) as usize,
        ) {
            Ok(text) => ToolOutput::ok(Phase::Orient, text),
            Err(error) => ToolOutput::err(Phase::Orient, error),
        }
    }

    fn symbols(
        &self,
        query: &str,
        path: &str,
        kind: &str,
        test: Option<bool>,
        limit: Option<u64>,
    ) -> ToolOutput {
        let query = query.trim();
        if query.is_empty() {
            return ToolOutput::err(Phase::Orient, "symbol query must not be empty");
        }
        let scope = if path.trim().is_empty() || path.trim() == "." {
            String::new()
        } else {
            match self.resolve(path) {
                Ok((_, relative)) => relative,
                Err(error) => return ToolOutput::err(Phase::Orient, error),
            }
        };
        let Some(graph) = self.graph() else {
            return ToolOutput::err(Phase::Orient, "no code graph available");
        };
        let q = query.to_ascii_lowercase();
        let kind = kind.trim().to_ascii_lowercase();
        let mut matches = graph
            .symbols()
            .iter()
            .filter(|symbol| !symbol.is_pseudo())
            .filter(|symbol| {
                kind.is_empty() || format!("{:?}", symbol.kind).to_ascii_lowercase() == kind
            })
            .filter(|symbol| test.is_none_or(|test| symbol.test == test))
            .filter(|symbol| {
                scope.is_empty()
                    || symbol.file == scope
                    || symbol.file.starts_with(&format!("{scope}/"))
            })
            .filter_map(|symbol| {
                let name = symbol.name.to_ascii_lowercase();
                let id = symbol.id.to_ascii_lowercase();
                let score = if name == q {
                    0
                } else if name.starts_with(&q) {
                    1
                } else if name.contains(&q) {
                    2
                } else if id.contains(&q) {
                    3
                } else {
                    return None;
                };
                Some((score, symbol))
            })
            .collect::<Vec<_>>();
        matches.sort_by(|(score_a, a), (score_b, b)| {
            score_a
                .cmp(score_b)
                .then_with(|| a.file.cmp(&b.file))
                .then_with(|| a.start.cmp(&b.start))
                .then_with(|| a.id.cmp(&b.id))
        });
        let limit = limit.unwrap_or(30).clamp(1, 80) as usize;
        matches.truncate(limit);
        if matches.is_empty() {
            return ToolOutput::ok(Phase::Orient, "No AST symbols matched.");
        }
        let mut out = String::new();
        for (_, symbol) in matches {
            out.push_str(&format!(
                "{} · {:?} · {}:{}-{}{}\n",
                symbol.id,
                symbol.kind,
                symbol.file,
                symbol.start,
                symbol.end,
                if symbol.test { " · test" } else { "" }
            ));
        }
        ToolOutput::ok(Phase::Orient, out)
    }

    fn symbol_context(&self, id: &str, siblings: Option<u64>) -> ToolOutput {
        let Some(graph) = self.graph() else {
            return ToolOutput::err(Phase::Orient, "no code graph available");
        };
        let Some(target) = graph.get(id) else {
            return ToolOutput::err(Phase::Orient, format!("unknown symbol '{id}'"));
        };
        let sibling_count = siblings.unwrap_or(2).min(5) as usize;
        let mut out = format!(
            "target: {} · {:?} · {}:{}-{}{}\n",
            target.id,
            target.kind,
            target.file,
            target.start,
            target.end,
            if target.test { " · test" } else { "" }
        );

        let mut owner = id;
        let mut owner_symbol = None;
        while let Some((prefix, _)) = owner.rsplit_once("::") {
            owner = prefix;
            if let Some(symbol) = graph.get(owner) {
                owner_symbol = Some(symbol);
                break;
            }
        }
        if let Some(owner) = owner_symbol {
            out.push_str(&format!(
                "owner: {} · {:?} · {}:{}-{}\n",
                owner.id, owner.kind, owner.file, owner.start, owner.end
            ));
        }

        let mut same_file = graph
            .in_file(&target.file)
            .filter(|symbol| !symbol.is_pseudo())
            .collect::<Vec<_>>();
        same_file.sort_by(|a, b| {
            a.start
                .cmp(&b.start)
                .then_with(|| a.end.cmp(&b.end))
                .then_with(|| a.id.cmp(&b.id))
        });
        if sibling_count > 0
            && let Some(index) = same_file.iter().position(|symbol| symbol.id == target.id)
        {
            let start = index.saturating_sub(sibling_count);
            let end = (index + sibling_count + 1).min(same_file.len());
            out.push_str("nearby:\n");
            for symbol in &same_file[start..end] {
                if symbol.id == target.id {
                    continue;
                }
                out.push_str(&format!(
                    "  {} · {:?} · {}:{}-{}\n",
                    symbol.id, symbol.kind, symbol.file, symbol.start, symbol.end
                ));
            }
        }

        let mut callers = graph.referrers(id);
        callers.sort_by(|a, b| a.id.cmp(&b.id));
        callers.dedup_by(|a, b| a.id == b.id);
        out.push_str("callers:\n");
        if callers.is_empty() {
            out.push_str("  (none)\n");
        } else {
            for symbol in callers.into_iter().take(20) {
                out.push_str(&format!(
                    "  {} · {:?} · {}:{}-{}\n",
                    symbol.id, symbol.kind, symbol.file, symbol.start, symbol.end
                ));
            }
        }

        let mut callees = graph
            .resolve(id)
            .into_iter()
            .filter(|symbol| symbol.id != id)
            .collect::<Vec<_>>();
        callees.sort_by(|a, b| a.id.cmp(&b.id));
        callees.dedup_by(|a, b| a.id == b.id);
        out.push_str("callees:\n");
        if callees.is_empty() {
            out.push_str("  (none)\n");
        } else {
            for symbol in callees.into_iter().take(20) {
                out.push_str(&format!(
                    "  {} · {:?} · {}:{}-{}\n",
                    symbol.id, symbol.kind, symbol.file, symbol.start, symbol.end
                ));
            }
        }
        ToolOutput::ok(Phase::Orient, out)
    }

    fn inspect_symbol(&mut self, query: &str, path: &str) -> ToolOutput {
        let query = query.trim();
        if query.is_empty() {
            return ToolOutput::err(Phase::Orient, "symbol query must not be empty");
        }
        let scope = if path.trim().is_empty() || path.trim() == "." {
            String::new()
        } else {
            match self.resolve(path) {
                Ok((_, relative)) => relative,
                Err(error) => return ToolOutput::err(Phase::Orient, error),
            }
        };
        let Some(graph) = self.graph() else {
            return ToolOutput::err(Phase::Orient, "no code graph available");
        };
        let exact_id = graph.get(query).map(|symbol| symbol.id.clone());
        let mut candidates = if let Some(id) = exact_id {
            vec![id]
        } else {
            graph
                .symbols()
                .iter()
                .filter(|symbol| !symbol.is_pseudo())
                .filter(|symbol| {
                    scope.is_empty()
                        || symbol.file == scope
                        || symbol.file.starts_with(&format!("{scope}/"))
                })
                .filter(|symbol| symbol.name == query)
                .map(|symbol| symbol.id.clone())
                .collect::<Vec<_>>()
        };
        candidates.sort();
        candidates.dedup();
        if candidates.len() != 1 {
            if candidates.is_empty() {
                let q = query.to_ascii_lowercase();
                candidates = graph
                    .symbols()
                    .iter()
                    .filter(|symbol| !symbol.is_pseudo())
                    .filter(|symbol| {
                        scope.is_empty()
                            || symbol.file == scope
                            || symbol.file.starts_with(&format!("{scope}/"))
                    })
                    .filter(|symbol| {
                        symbol.name.to_ascii_lowercase().contains(&q)
                            || symbol.id.to_ascii_lowercase().contains(&q)
                    })
                    .take(12)
                    .map(|symbol| symbol.id.clone())
                    .collect();
            }
            return ToolOutput::ok(
                Phase::Orient,
                if candidates.is_empty() {
                    format!("No AST symbol matched `{query}`.")
                } else {
                    format!(
                        "`{query}` is ambiguous; choose one stable symbol id:\n{}",
                        candidates
                            .into_iter()
                            .map(|id| format!("  {id}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    )
                },
            );
        }
        let id = candidates.pop().unwrap();
        drop(graph);
        let context = self.symbol_context(&id, Some(1));
        if context.is_error {
            return context;
        }
        let source = self.read_tool(&id, None, None);
        if source.is_error {
            return source;
        }
        ToolOutput::ok(
            Phase::Orient,
            format!("{}\nsource:\n{}", context.text, source.text),
        )
    }

    fn file_context(&self, path: &str) -> ToolOutput {
        let (absolute, rel) = match self.resolve(path) {
            Ok(value) => value,
            Err(error) => return ToolOutput::err(Phase::Orient, error),
        };
        if !absolute.is_file() {
            return ToolOutput::err(Phase::Orient, format!("{rel} is not a file"));
        }
        let Some(graph) = self.graph() else {
            return ToolOutput::err(Phase::Orient, "no code graph available");
        };
        let symbols = graph.in_file(&rel).collect::<Vec<_>>();
        if symbols.is_empty() {
            return ToolOutput::ok(
                Phase::Orient,
                format!("file: {rel}\nNo indexed AST symbols for this file."),
            );
        }
        let mut outgoing = BTreeSet::new();
        let mut incoming = BTreeSet::new();
        for symbol in &symbols {
            for target in graph.resolve(&symbol.id) {
                if target.file != rel {
                    outgoing.insert(target.file.clone());
                }
            }
            for source in graph.referrers(&symbol.id) {
                if source.file != rel {
                    incoming.insert(source.file.clone());
                }
            }
        }
        let mut visible = symbols
            .iter()
            .filter(|symbol| !symbol.is_pseudo())
            .collect::<Vec<_>>();
        visible.sort_by_key(|symbol| symbol.start);
        let mut out = format!("file: {rel}\nsymbols:\n");
        for symbol in visible.into_iter().take(40) {
            out.push_str(&format!(
                "  {} · {:?} · {}-{}\n",
                symbol.id, symbol.kind, symbol.start, symbol.end
            ));
        }
        out.push_str("references files:\n");
        if outgoing.is_empty() {
            out.push_str("  (none)\n");
        } else {
            for file in outgoing.into_iter().take(32) {
                out.push_str(&format!("  {file}\n"));
            }
        }
        out.push_str("referenced by files:\n");
        if incoming.is_empty() {
            out.push_str("  (none)\n");
        } else {
            for file in incoming.into_iter().take(32) {
                out.push_str(&format!("  {file}\n"));
            }
        }
        ToolOutput::ok(Phase::Orient, out)
    }

    fn symbol_impact(&self, id: &str) -> ToolOutput {
        let Some(graph) = self.graph() else {
            return ToolOutput::err(Phase::Orient, "no code graph available");
        };
        let Some(target) = graph.get(id) else {
            return ToolOutput::err(Phase::Orient, format!("unknown symbol `{id}`"));
        };

        let mut affected_files = BTreeSet::from([target.file.clone()]);
        let mut reachable_tests = BTreeSet::new();
        let mut seen = BTreeSet::from([target.id.clone()]);
        let mut queue = VecDeque::from([target.id.clone()]);
        while let Some(current) = queue.pop_front() {
            for caller in graph.referrers(&current) {
                affected_files.insert(caller.file.clone());
                if caller.test {
                    reachable_tests.insert(caller.id.clone());
                }
                if seen.insert(caller.id.clone()) {
                    queue.push_back(caller.id.clone());
                }
            }
        }

        let mut direct_callers = graph
            .referrers(id)
            .into_iter()
            .map(|symbol| symbol.id.clone())
            .collect::<BTreeSet<_>>();
        let mut direct_callees = graph
            .resolve(id)
            .into_iter()
            .map(|symbol| symbol.id.clone())
            .collect::<BTreeSet<_>>();
        direct_callers.remove(id);
        direct_callees.remove(id);

        let mut out = format!(
            "target: {} · {:?} · {}:{}-{}\nreverse-reachable symbols: {}\naffected files: {}\n",
            target.id,
            target.kind,
            target.file,
            target.start,
            target.end,
            seen.len().saturating_sub(1),
            affected_files.len()
        );
        out.push_str("direct callers:\n");
        if direct_callers.is_empty() {
            out.push_str("  (none)\n");
        } else {
            for caller in direct_callers {
                out.push_str(&format!("  {caller}\n"));
            }
        }
        out.push_str("direct dependencies:\n");
        if direct_callees.is_empty() {
            out.push_str("  (none)\n");
        } else {
            for callee in direct_callees {
                out.push_str(&format!("  {callee}\n"));
            }
        }
        out.push_str("affected files:\n");
        for file in affected_files {
            out.push_str(&format!("  {file}\n"));
        }
        out.push_str("reachable tests:\n");
        if reachable_tests.is_empty() {
            out.push_str("  (none found in the AST graph)\n");
        } else {
            for test in reachable_tests {
                out.push_str(&format!("  {test}\n"));
            }
        }
        ToolOutput::ok(Phase::Orient, out)
    }

    fn outline(&self, path: &str) -> ToolOutput {
        // The root is a valid directory to inspect, but Overlay::resolve
        // intentionally refuses `.` and empty paths for file effects.
        let (abs, rel) = match path.trim() {
            "." | "./" => (self.root.clone(), ".".to_owned()),
            _ => match self.resolve(path) {
                Ok(p) => p,
                Err(e) => return ToolOutput::err(Phase::Orient, e),
            },
        };
        if abs.is_dir() {
            let mut entries: Vec<String> = fs::read_dir(&abs)
                .map(|d| {
                    d.filter_map(Result::ok)
                        .filter(|e| {
                            !e.file_name().to_string_lossy().starts_with('.')
                                && e.file_name() != "target"
                        })
                        .map(|e| {
                            format!(
                                "{}{}",
                                e.file_name().to_string_lossy(),
                                if e.path().is_dir() { "/" } else { "" }
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            entries.sort();
            return ToolOutput::ok(Phase::Orient, format!("{rel}/\n  {}", entries.join("\n  ")));
        }
        let Ok(text) = fs::read_to_string(&abs) else {
            return ToolOutput::err(Phase::Orient, format!("cannot read {rel}"));
        };
        ToolOutput::ok(Phase::Orient, self.outline_text(&rel, &text))
    }

    fn outline_text(&self, rel: &str, text: &str) -> String {
        let lines: Vec<&str> = text.lines().collect();
        let mut out = format!("{rel}: {} lines\n", lines.len());
        if Language::of(rel).is_some()
            && let Some(graph) = self.graph()
        {
            let mut symbols: Vec<_> = graph.in_file(rel).filter(|s| !s.is_pseudo()).collect();
            symbols.sort_by_key(|s| s.start);
            for s in symbols {
                let head = lines
                    .get(s.start.saturating_sub(1)..s.end.min(lines.len()))
                    .and_then(|ls| {
                        ls.iter().find(|l| {
                            !l.trim_start().starts_with("///") && !l.trim_start().starts_with("#[")
                        })
                    })
                    .map(|l| l.trim())
                    .unwrap_or("");
                let path = s.id.split("::").skip(1).collect::<Vec<_>>().join("::");
                out.push_str(&format!(
                    "  {:>5}-{:<5} {path}  {}\n",
                    s.start,
                    s.end,
                    head.chars().take(110).collect::<String>()
                ));
            }
            return out;
        }
        for (i, line) in lines.iter().enumerate() {
            let t = line.trim_start();
            if t.starts_with('#')
                || t.starts_with("def ")
                || t.starts_with("class ")
                || t.starts_with("function ")
                || t.starts_with("export ")
            {
                out.push_str(&format!("  {:>5} {}\n", i + 1, line.trim()));
            }
        }
        out
    }

    fn read_tool(&mut self, path: &str, start: Option<u64>, end: Option<u64>) -> ToolOutput {
        // Symbol id: `src/lib.rs::Type::name`
        if let Some((file, _)) = path.split_once("::")
            && start.is_none()
            && let Some(graph) = self.graph()
            && let Some(symbol) = graph.get(path)
        {
            let (a, b) = (symbol.start as u64, symbol.end as u64);
            return self.read_lines(file, Some(a), Some(b));
        }
        self.read_lines(path, start, end)
    }

    fn read_many(&mut self, items: &Value) -> ToolOutput {
        let Some(items) = items.as_array() else {
            return ToolOutput::err(Phase::Orient, "items must be an array");
        };
        if items.is_empty() || items.len() > 8 {
            return ToolOutput::err(Phase::Orient, "read_many requires 1..=8 items");
        }

        let per_item = (OUTPUT_MAX / items.len()).max(900);
        let mut out = String::new();
        let mut guidance = BTreeSet::new();
        for (index, item) in items.iter().enumerate() {
            let Some(path) = item["path"].as_str().filter(|path| !path.trim().is_empty()) else {
                return ToolOutput::err(
                    Phase::Orient,
                    format!("read_many item {} needs a non-empty path", index + 1),
                );
            };
            let start = item["start"].as_u64();
            let end = item["end"].as_u64();
            if start.is_some() != end.is_some() {
                return ToolOutput::err(
                    Phase::Orient,
                    format!(
                        "read_many item {} must provide both start and end",
                        index + 1
                    ),
                );
            }
            if let (Some(start), Some(end)) = (start, end)
                && (start == 0 || end < start)
            {
                return ToolOutput::err(
                    Phase::Orient,
                    format!("read_many item {} has an invalid line range", index + 1),
                );
            }
            let result = self.read_tool(path, start, end);
            if result.is_error {
                return ToolOutput::err(
                    Phase::Orient,
                    format!(
                        "read_many item {} ({path}) failed: {}",
                        index + 1,
                        result.text
                    ),
                );
            }
            let instruction_path = path.split("::").next().unwrap_or(path);
            let relative_path = if Path::new(instruction_path).is_absolute() {
                match self.resolve(instruction_path) {
                    Ok((_, relative)) => relative,
                    Err(error) => return ToolOutput::err(Phase::Orient, error),
                }
            } else {
                instruction_path.to_owned()
            };
            match self.instructions.for_path(Path::new(&relative_path)) {
                Ok(text) if !text.is_empty() => {
                    guidance.insert(text);
                }
                Ok(_) => {}
                Err(error) => return ToolOutput::err(Phase::Orient, error.to_string()),
            }
            if index > 0 {
                out.push_str("\n\n");
            }
            out.push_str(&format!("=== {} ===\n", path));
            out.push_str(&clip_limit(&result.text, per_item));
        }
        let mut result = ToolOutput::ok(Phase::Orient, out);
        for text in guidance {
            result.text.push_str("\n\n");
            result.text.push_str(&text);
        }
        result
    }

    fn refs(&self, id: &str) -> ToolOutput {
        let Some(graph) = self.graph() else {
            return ToolOutput::err(Phase::Orient, "no code graph available");
        };
        if graph.get(id).is_none() {
            return ToolOutput::err(Phase::Orient, format!("unknown symbol `{id}`"));
        }

        let referrers = graph.referrers(id);
        let callees = graph.resolve(id);

        let mut out = format!("references for `{id}` (approximate, name-resolved):\n");

        out.push_str("callers:\n");
        if referrers.is_empty() {
            out.push_str("  (none)\n");
        } else {
            let mut by_file: std::collections::BTreeMap<&str, Vec<&crate::world::graph::Symbol>> =
                std::collections::BTreeMap::new();
            for sym in &referrers {
                by_file.entry(&sym.file).or_default().push(sym);
            }
            let mut count = 0;
            const MAX_ITEMS: usize = 30;
            for (file, syms) in by_file {
                out.push_str(&format!("  {file}:\n"));
                for s in syms {
                    out.push_str(&format!("    {}\n", s.id));
                    count += 1;
                    if count >= MAX_ITEMS {
                        break;
                    }
                }
                if count >= MAX_ITEMS {
                    out.push_str("    ... (truncated)\n");
                    break;
                }
            }
        }

        out.push_str("callees:\n");
        let non_self_callees: Vec<_> = callees.into_iter().filter(|s| s.id != id).collect();
        if non_self_callees.is_empty() {
            out.push_str("  (none)\n");
        } else {
            let mut by_file: std::collections::BTreeMap<&str, Vec<&crate::world::graph::Symbol>> =
                std::collections::BTreeMap::new();
            for sym in &non_self_callees {
                by_file.entry(&sym.file).or_default().push(sym);
            }
            let mut count = 0;
            const MAX_ITEMS: usize = 30;
            for (file, syms) in by_file {
                out.push_str(&format!("  {file}:\n"));
                for s in syms {
                    out.push_str(&format!("    {}\n", s.id));
                    count += 1;
                    if count >= MAX_ITEMS {
                        break;
                    }
                }
                if count >= MAX_ITEMS {
                    out.push_str("    ... (truncated)\n");
                    break;
                }
            }
        }

        ToolOutput::ok(Phase::Orient, out)
    }

    fn read_artifact(&mut self, path: &str, start: Option<u64>, end: Option<u64>) -> ToolOutput {
        let Some(reference) = self.artifacts.get(path) else {
            return ToolOutput::err(Phase::Orient, "unknown artifact reference");
        };
        let (Some(a), Some(b)) = (start, end) else {
            return ToolOutput::err(Phase::Orient, "start/end lines required");
        };
        if a == 0 || b < a || b - a >= 400 {
            return ToolOutput::err(Phase::Orient, "invalid artifact line range");
        }
        match self.overlay.artifacts().get(reference, None) {
            Ok(bytes) => ToolOutput::ok(
                Phase::Orient,
                String::from_utf8_lossy(&bytes)
                    .lines()
                    .enumerate()
                    .skip(a as usize - 1)
                    .take((b - a + 1) as usize)
                    .map(|(i, l)| format!("{:>5}| {l}", i + 1))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            Err(e) => ToolOutput::err(Phase::Orient, e.to_string()),
        }
    }

    fn read_lines(&mut self, path: &str, start: Option<u64>, end: Option<u64>) -> ToolOutput {
        let (abs, rel) = match self.resolve(path) {
            Ok(p) => p,
            Err(e) => return ToolOutput::err(Phase::Orient, e),
        };
        let Ok(bytes) = fs::read(&abs) else {
            return ToolOutput::err(
                Phase::Orient,
                format!("cannot read {rel} (use outline on a directory)"),
            );
        };
        let text = String::from_utf8_lossy(&bytes).into_owned();
        if let Err(e) = self.overlay.read(Path::new(&rel), Some(0..0)) {
            return ToolOutput::err(Phase::Orient, e.to_string());
        }
        let lines: Vec<&str> = text.lines().collect();
        if start.is_none() && end.is_none() && lines.len() > READ_MAX_LINES {
            return ToolOutput::ok(
                Phase::Orient,
                format!(
                    "{}\n(file is long; read a range with start/end, or a symbol by id)",
                    self.outline_text(&rel, &text)
                ),
            );
        }
        let a = start.unwrap_or(1).max(1) as usize;
        let b = (end.map(|e| e as usize).unwrap_or(lines.len()))
            .min(lines.len())
            .min(a + READ_MAX_LINES - 1);
        if a > lines.len().max(1) {
            return ToolOutput::err(Phase::Orient, format!("{rel} has {} lines", lines.len()));
        }
        let mut out = format!("{rel} lines {a}-{b} of {}\n", lines.len());
        for (i, line) in lines.iter().enumerate().take(b).skip(a - 1) {
            out.push_str(&format!("{:>5}| {line}\n", i + 1));
        }
        ToolOutput::ok(Phase::Orient, out)
    }

    fn git_status(&self) -> ToolOutput {
        self.git_read_at(&self.root, &["status", "--short", "--branch"], None)
    }

    fn git_diff(&self, path: Option<&str>, staged: bool) -> ToolOutput {
        let mut args = vec!["diff", "--no-ext-diff", "--no-color"];
        if staged {
            args.push("--cached");
        }
        self.git_read_at(&self.root, &args, path)
    }

    fn git_log(&self, path: Option<&str>, limit: Option<u64>) -> ToolOutput {
        let count = limit.unwrap_or(12).clamp(1, 50).to_string();
        let args = vec![
            "log",
            "--oneline",
            "--decorate",
            "--no-color",
            "-n",
            count.as_str(),
        ];
        self.git_read_at(self.overlay.workspace(), &args, path)
    }

    fn git_show(&self, rev: Option<&str>, path: Option<&str>) -> ToolOutput {
        let rev = rev.unwrap_or("HEAD").trim();
        if rev.is_empty() || rev.starts_with('-') || rev.chars().any(char::is_whitespace) {
            return ToolOutput::err(Phase::Orient, "invalid Git revision");
        }
        let args = ["show", "--no-ext-diff", "--no-color", rev];
        self.git_read_at(self.overlay.workspace(), &args, path)
    }

    fn git_blame(&self, path: &str, start: Option<u64>, end: Option<u64>) -> ToolOutput {
        let path = path.trim().trim_start_matches("./");
        let rel = Path::new(path);
        if path.is_empty()
            || rel.is_absolute()
            || rel
                .components()
                .any(|component| matches!(component, Component::ParentDir))
        {
            return ToolOutput::err(Phase::Orient, "path must stay inside the workspace");
        }
        if start.is_some() != end.is_some() || start.zip(end).is_some_and(|(a, b)| a == 0 || b < a)
        {
            return ToolOutput::err(Phase::Orient, "invalid blame line range");
        }
        let mut command = Command::new("git");
        command
            .current_dir(self.overlay.workspace())
            .args(["blame", "--date=short"]);
        let range = start.zip(end).map(|(a, b)| format!("{a},{b}"));
        if let Some(range) = &range {
            command.args(["-L", range]);
        }
        command.arg("--").arg(path);
        match command.output() {
            Ok(output) if output.status.success() => ToolOutput::ok(
                Phase::Orient,
                String::from_utf8_lossy(&output.stdout)
                    .trim_end()
                    .to_owned(),
            ),
            Ok(output) => ToolOutput::err(
                Phase::Orient,
                format!(
                    "git blame failed (exit {}): {}",
                    output.status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            ),
            Err(error) => ToolOutput::err(Phase::Orient, format!("git unavailable: {error}")),
        }
    }

    fn git_read_at(&self, directory: &Path, args: &[&str], path: Option<&str>) -> ToolOutput {
        let mut command = Command::new("git");
        command.current_dir(directory).args(args);
        if let Some(path) = path.filter(|path| !path.trim().is_empty()) {
            let path = path.trim().trim_start_matches("./");
            let rel = Path::new(path);
            if rel.is_absolute()
                || rel
                    .components()
                    .any(|component| matches!(component, Component::ParentDir))
            {
                return ToolOutput::err(
                    Phase::Orient,
                    format!("{path}: path must stay inside the workspace"),
                );
            }
            command.arg("--").arg(path);
        }
        match command.output() {
            Ok(output) if output.status.success() => {
                let text = String::from_utf8_lossy(&output.stdout)
                    .trim_end()
                    .to_owned();
                ToolOutput::ok(
                    Phase::Orient,
                    if text.is_empty() {
                        "No matching Git output.".into()
                    } else {
                        clip(text)
                    },
                )
            }
            Ok(output) => ToolOutput::err(
                Phase::Orient,
                clip(format!(
                    "git failed (exit {}): {}",
                    output.status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&output.stderr).trim()
                )),
            ),
            Err(error) => ToolOutput::err(Phase::Orient, format!("git unavailable: {error}")),
        }
    }

    // ------------------------------------------------------------------ write side

    fn edit(&mut self, path: &str, old: &str, new: &str, replace_all: bool) -> ToolOutput {
        let mut result = self
            .overlay
            .edit_all(Path::new(path), old, new, replace_all);
        // Some models HTML-escape tool arguments (`&` → `&amp;`): when the text is not
        // found as given, try it unescaped once, before reporting a mismatch.
        if result.is_err() && [old, new].iter().any(|t| has_html_entities(t)) {
            let retry = self.overlay.edit_all(
                Path::new(path),
                &unescape_html(old),
                &unescape_html(new),
                replace_all,
            );
            if retry.is_ok() {
                result = retry;
            }
        }
        match result {
            Ok(output) => ToolOutput::ok(
                Phase::Edit,
                format!("{}\n{}", output.summary, output.structured),
            ),
            Err(e) => ToolOutput::err(Phase::Edit, e.to_string()),
        }
    }
    fn write(&mut self, path: &str, content: &str) -> ToolOutput {
        match self.overlay.write(Path::new(path), content) {
            Ok(output) => ToolOutput::ok(Phase::Edit, output.summary),
            Err(e) => ToolOutput::err(Phase::Edit, e.to_string()),
        }
    }
    fn check(&mut self) -> ToolOutput {
        let diff = self.overlay.patch().map(|p| p.diff).unwrap_or_default();
        if let Some((last, output)) = &self.last_check
            && *last == diff
        {
            let previous: Value = serde_json::from_str(&output.text).unwrap_or_default();
            let artifacts: Vec<_> = previous["commands"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|command| command["artifact_path"].as_str())
                .collect();
            return ToolOutput {
                text: json!({
                    "unchanged": true,
                    "status": previous["status"].as_str().unwrap_or(if output.is_error { "failed" } else { "passed" }),
                    "artifacts": artifacts,
                    "message": "Validation already ran for this exact patch. Edit the code before checking again; earlier details remain in the original check result."
                }).to_string(),
                ..(*output).clone()
            };
        }
        let output = self.run_checks();
        self.last_check = Some((diff, output.clone()));
        output
    }

    fn diagnostics(&mut self, path: Option<&str>) -> ToolOutput {
        let mut directory = match path.filter(|path| !path.trim().is_empty()) {
            Some(path) => match self.resolve(path) {
                Ok((absolute, _)) => {
                    if absolute.is_dir() {
                        absolute
                    } else {
                        absolute.parent().unwrap_or(&self.root).to_path_buf()
                    }
                }
                Err(error) => return ToolOutput::err(Phase::Orient, error),
            },
            None => self.root.clone(),
        };
        let project = loop {
            if directory.join("Cargo.toml").exists()
                || directory.join("package.json").exists()
                || directory.join("pyproject.toml").exists()
                || directory.join("setup.py").exists()
            {
                break Some(directory.clone());
            }
            if directory == self.root || !directory.pop() || !directory.starts_with(&self.root) {
                break None;
            }
        };
        let Some(project) = project else {
            return ToolOutput::err(
                Phase::Validate,
                "no project diagnostics are known for this path",
            );
        };
        let rel = project.strip_prefix(&self.root).unwrap_or(Path::new(""));
        let prefix = if rel.as_os_str().is_empty() {
            String::new()
        } else {
            let quoted = rel.to_string_lossy().replace('\'', "'\\''");
            format!("cd '{quoted}' && ")
        };
        let command = if project.join("Cargo.toml").exists() {
            format!("{prefix}cargo check --quiet")
        } else if project.join("package.json").exists() {
            let package = fs::read(project.join("package.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
            let script = package.as_ref().and_then(|package| {
                ["typecheck", "check"]
                    .into_iter()
                    .find(|name| package["scripts"][name].as_str().is_some())
            });
            let Some(script) = script else {
                return ToolOutput::err(
                    Phase::Validate,
                    "package.json has no typecheck/check script for fast diagnostics",
                );
            };
            format!("{prefix}npm run --silent {script}")
        } else {
            format!("{prefix}python3 -m compileall -q .")
        };
        let result = self.terminals.exec(
            &command,
            10_000.min(self.remaining_time().as_millis().min(u64::MAX as u128) as u64),
            self.remaining_time().min(Duration::from_secs(60)),
            self.cancel,
        );
        self.terminal_output(Some(command), None, result)
    }
    fn run_checks(&mut self) -> ToolOutput {
        // Validate the projects of the changed files; before any change, those of the
        // files read so far (the root may have no build file of its own).
        let mut basis = self.overlay.touched().unwrap_or_default();
        if basis.is_empty() {
            basis = self.overlay.read_paths();
        }
        let checks = super::effective_checks(self.overlay.dir(), &self.checks, &basis);
        if checks.is_empty() {
            return ToolOutput::err(
                Phase::Validate,
                "no validation command known yet: check runs the build and tests of the project \
                 (Cargo.toml, package.json with tests, pyproject.toml) containing the files you \
                 edit. Make your edit first, then call check.",
            );
        }
        let runner = Runner::new(self.overlay.artifacts().clone());
        let mut reports = Vec::new();
        for command in &checks {
            let remaining = self.remaining_time();
            if remaining.is_zero() {
                return ToolOutput::err(Phase::Validate, json!({
                    "status":"timed_out", "commands":reports,
                    "message":"Worker deadline reached; no additional validation command was started."
                }).to_string());
            }
            match runner.run_with_policy(
                self.overlay.dir(),
                command,
                remaining,
                self.cancel,
                Some(&self.overlay.target_dir()),
                self.policy,
            ) {
                Ok(result) => {
                    let passed = result.status == crate::effects::RunStatus::Passed;
                    self.artifacts
                        .insert(result.artifact.id.clone(), result.artifact.clone());
                    let mut report = serde_json::to_value(result).unwrap();
                    report["artifact_path"] = report["artifact"]["id"].clone();
                    reports.push(report);
                    if !passed {
                        return ToolOutput::err(
                            Phase::Validate,
                            json!({"status":"failed","commands":reports}).to_string(),
                        );
                    }
                }
                Err(e) => return ToolOutput::err(Phase::Validate, e.to_string()),
            }
        }
        ToolOutput::ok(
            Phase::Validate,
            json!({"status":"passed","commands":reports}).to_string(),
        )
    }
}

fn split_rg_line(line: &str) -> Option<(&str, &str, &str)> {
    let bytes = line.as_bytes();
    let start = if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/')
    {
        3
    } else if bytes.len() >= 7
        && &bytes[..4] == b"\\\\?\\"
        && bytes[4].is_ascii_alphabetic()
        && bytes[5] == b':'
        && matches!(bytes[6], b'\\' | b'/')
    {
        7
    } else {
        0
    };
    let (rel_file, rest) = line[start..].split_once(':')?;
    let file = &line[..start + rel_file.len()];
    let (no, body) = rest.split_once(':')?;
    Some((file, no, body))
}

fn spec(name: &str, description: &str, parameters: Value) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: description.into(),
        parameters,
    }
}

fn has_html_entities(text: &str) -> bool {
    ["&amp;", "&lt;", "&gt;", "&quot;", "&#39;", "&#x27;"]
        .iter()
        .any(|entity| text.contains(entity))
}

fn unescape_html(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod map_tests {
    use super::*;
    use crate::{
        base::Revision,
        effects::{ExecutionPolicy, Overlay, SandboxMode},
    };

    #[test]
    fn map_and_outline_expose_multilanguage_symbols_to_read_only_workers() {
        assert_eq!(
            split_rg_line(r"C:\work\src\lib.py:12:def helper():"),
            Some((r"C:\work\src\lib.py", "12", "def helper():"))
        );
        assert_eq!(
            split_rg_line(r"\\?\C:\work\src\lib.py:12:def helper():"),
            Some((r"\\?\C:\work\src\lib.py", "12", "def helper():"))
        );
        let directory = tempfile::tempdir().unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(directory.path())
                .status()
                .unwrap()
                .success()
        );
        fs::create_dir_all(directory.path().join("src")).unwrap();
        fs::write(
            directory.path().join("src/lib.py"),
            "def helper():\n    return 1\ndef entry():\n    return helper()\n",
        )
        .unwrap();
        let revision = Revision::capture(directory.path()).unwrap();
        let mut overlay = Overlay::create(directory.path(), &revision).unwrap();
        let cancel = AtomicBool::new(false);
        let mut worker = Workspace::new(
            &mut overlay,
            true,
            vec![],
            &cancel,
            Duration::from_secs(10),
            ExecutionPolicy {
                sandbox: SandboxMode::ReadOnly,
                network: false,
            },
            &[],
            None,
        )
        .unwrap();
        assert!(worker.specs().iter().any(|s| s.name == "map"));
        assert!(worker.specs().iter().any(|s| s.name == "glob"));
        let files = worker.call("glob", &json!({"pattern":"**/*.py"}));
        assert!(!files.is_error && files.text.contains("src/lib.py"), "{}", files.text);
        let scoped = worker.call("glob", &json!({"pattern":"*.py", "path":"src"}));
        assert!(!scoped.is_error && scoped.text.contains("src/lib.py"), "{}", scoped.text);
        assert!(worker.call("glob", &json!({"pattern":"*.py", "path":"../"})).is_error);
        assert!(worker.call("glob", &json!({"pattern":"["})).is_error);
        assert!(worker.call("glob", &json!({"pattern":""})).is_error);
        let excluded = worker.call("find", &json!({"pattern":"helper", "include":"*.rs"}));
        assert!(!excluded.is_error && !excluded.text.contains("src/lib.py:1"), "{}", excluded.text);
        let folded = worker.call("find", &json!({"pattern":"HELPER", "case_sensitive":false, "limit":1}));
        assert!(!folded.is_error && folded.text.contains("src/lib.py:1") && folded.text.contains("truncated"), "{}", folded.text);
        let single = worker.call("find", &json!({"pattern":"entry", "path":"src/lib.py"}));
        assert!(!single.is_error && single.text.contains("src/lib.py:3"), "{}", single.text);
        let excluded_file = worker.call("find", &json!({"pattern":"entry", "path":"src/lib.py", "include":"*.rs"}));
        assert!(!excluded_file.is_error && !excluded_file.text.contains("src/lib.py:3"), "{}", excluded_file.text);
        let excluded_glob = worker.call("glob", &json!({"pattern":"*.rs", "path":"src/lib.py"}));
        assert!(!excluded_glob.is_error && !excluded_glob.text.contains("src/lib.py"), "{}", excluded_glob.text);
        let split = worker.call(
            "plan_split",
            &json!({"ids":["src/lib.py::helper","src/lib.py::entry"]}),
        );
        assert!(!split.is_error, "{}", split.text);
        let split: Value = serde_json::from_str(&split.text).unwrap();
        assert_eq!(split["advisory"], true);
        assert_eq!(split["groups"].as_array().unwrap().len(), 1);
        let result = worker.call("map", &json!({"path":"src","budget":2000}));
        assert!(!result.is_error, "{}", result.text);
        assert!(result.text.contains("lib.py (python)"), "{}", result.text);
        assert!(result.text.contains("fn entry"), "{}", result.text);
        assert!(!result.text.contains("other files:"), "{}", result.text);
        let outline = worker.call("outline", &json!({"path":"src/lib.py"}));
        assert!(
            !outline.is_error && outline.text.contains("entry"),
            "{}",
            outline.text
        );
        for root in [".", "./"] {
            let listing = worker.call("outline", &json!({"path":root}));
            assert!(
                !listing.is_error && listing.text.contains("src/"),
                "outline {root} failed: {}",
                listing.text
            );
        }
        let path_search = worker.call("find", &json!({"pattern":"src", "kind":"path"}));
        assert!(
            !path_search.is_error && path_search.text.contains("src/"),
            "path search must find directories: {}", path_search.text
        );
        assert!(
            !path_search.text.contains("src/lib.py"),
            "a matching directory must not also list every file beneath it: {}", path_search.text
        );
        for pattern in ["lib", "src/lib", "SRC/LIB.PY"] {
            let files = worker.call("find", &json!({"pattern":pattern, "kind":"path"}));
            assert!(
                !files.is_error && files.text.contains("src/lib.py") && !files.text.contains("src/\n"),
                "path search for {pattern} must name the file: {}", files.text
            );
        }
        let none = worker.call("find", &json!({"pattern":"  ", "kind":"path"}));
        assert!(none.is_error, "an empty name must not list the whole repository: {}", none.text);
        let absolute = directory.path().join("src/lib.py");
        let absolute_read = worker.call("read", &json!({"path":absolute.to_string_lossy()}));
        assert!(
            !absolute_read.is_error && absolute_read.text.contains("def helper"),
            "absolute in-workspace reads must work: {}", absolute_read.text
        );
        let absolute_batch = worker.call("read_many", &json!({"items":[{"path":absolute.to_string_lossy()}]}));
        assert!(
            !absolute_batch.is_error && absolute_batch.text.contains("def helper"),
            "absolute in-workspace batch reads must work: {}", absolute_batch.text
        );
        let unrelated = tempfile::tempdir().unwrap();
        let unrelated_read = worker.call("read", &json!({"path":unrelated.path().join("secret.py").to_string_lossy()}));
        assert!(unrelated_read.is_error && unrelated_read.text.contains("outside the workspace"));
        assert!(worker.call("outline", &json!({"path":"../"})).is_error);
        let ast = worker.call(
            "ast_search",
            &json!({
                "pattern":"helper()", "path":"src", "language":"python",
            }),
        );
        assert!(
            !ast.is_error && ast.text.contains("src/lib.py:4 [src/lib.py::entry]"),
            "{}",
            ast.text
        );
        let refs = worker.call("refs", &json!({"id":"src/lib.py::helper"}));
        assert!(
            !refs.is_error && refs.text.contains("src/lib.py::entry"),
            "{}",
            refs.text
        );
        let inspect = worker.call("inspect_symbol", &json!({"query":"entry"}));
        assert!(
            !inspect.is_error
                && inspect.text.contains("target: src/lib.py::entry")
                && inspect.text.contains("return helper()"),
            "{}",
            inspect.text
        );
        let file = worker.call("file_context", &json!({"path":"src/lib.py"}));
        assert!(
            !file.is_error && file.text.contains("src/lib.py::entry"),
            "{}",
            file.text
        );
        assert!(worker.call("map", &json!({"path":"../"})).is_error);
    }

    #[test]
    fn native_git_tools_report_overlay_status_diff_and_history() {
        let directory = tempfile::tempdir().unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(directory.path())
                .status()
                .unwrap()
                .success()
        );
        fs::create_dir_all(directory.path().join("src")).unwrap();
        fs::write(
            directory.path().join("src/lib.rs"),
            "pub fn value() -> u32 { 1 }\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n    #[test]\n    fn value_is_one() { assert_eq!(value(), 1); }\n}\n",
        )
        .unwrap();
        fs::write(
            directory.path().join("Cargo.toml"),
            "[package]\nname = \"native-tools-test\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        assert!(
            Command::new("git")
                .args(["add", "."])
                .current_dir(directory.path())
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args([
                    "-c",
                    "user.name=Loom",
                    "-c",
                    "user.email=loom@localhost",
                    "commit",
                    "-qm",
                    "initial",
                ])
                .current_dir(directory.path())
                .status()
                .unwrap()
                .success()
        );
        let revision = Revision::capture(directory.path()).unwrap();
        let mut overlay = Overlay::create(directory.path(), &revision).unwrap();
        let cancel = AtomicBool::new(false);
        let mut worker = Workspace::new(
            &mut overlay,
            false,
            vec![],
            &cancel,
            Duration::from_secs(10),
            ExecutionPolicy {
                sandbox: SandboxMode::WorkspaceWrite,
                network: false,
            },
            &[],
            None,
        )
        .unwrap();

        for name in [
            "git_status",
            "git_diff",
            "git_log",
            "git_show",
            "git_blame",
            "diagnostics",
            "symbol_impact",
        ] {
            assert!(
                worker.specs().iter().any(|spec| spec.name == name),
                "missing native {name} tool"
            );
        }
        let history = worker.call("git_log", &json!({"limit":5}));
        assert!(!history.is_error, "{}", history.text);
        assert!(history.text.contains("initial"), "{}", history.text);
        let shown = worker.call("git_show", &json!({"rev":"HEAD","path":"src/lib.rs"}));
        assert!(
            !shown.is_error && shown.text.contains("value"),
            "{}",
            shown.text
        );
        let blame = worker.call("git_blame", &json!({"path":"src/lib.rs","start":1,"end":1}));
        assert!(
            !blame.is_error && blame.text.contains("value"),
            "{}",
            blame.text
        );
        let diagnostics = worker.call("diagnostics", &json!({"path":"src/lib.rs"}));
        #[cfg(unix)]
        assert!(!diagnostics.is_error, "{}", diagnostics.text);
        #[cfg(not(unix))]
        assert!(diagnostics.is_error && diagnostics.text.contains("PTY execution is only supported on Unix"), "{}", diagnostics.text);
        let impact = worker.call("symbol_impact", &json!({"id":"src/lib.rs::value"}));
        assert!(
            !impact.is_error
                && impact.text.contains("reachable tests:")
                && impact.text.contains("src/lib.rs::tests::value_is_one"),
            "{}",
            impact.text
        );

        assert!(!worker.call("read", &json!({"path":"src/lib.rs"})).is_error);
        let edit = worker.call(
            "edit",
            &json!({"path":"src/lib.rs","old":"{ 1 }","new":"{ 2 }"}),
        );
        assert!(!edit.is_error, "{}", edit.text);

        let status = worker.call("git_status", &json!({}));
        assert!(!status.is_error, "{}", status.text);
        assert!(status.text.contains("src/lib.rs"), "{}", status.text);

        let diff = worker.call("git_diff", &json!({"path":"src/lib.rs"}));
        assert!(!diff.is_error, "{}", diff.text);
        assert!(
            diff.text.contains("-pub fn value() -> u32 { 1 }")
                && diff.text.contains("+pub fn value() -> u32 { 2 }"),
            "{}",
            diff.text
        );
        assert!(worker.call("git_diff", &json!({"path":"../"})).is_error);
    }

    #[test]
    fn v4_indexes_survive_workers_and_reparse_only_changed_files() {
        fn wait_for_cleanup(slot: &Path) {
            use fs2::FileExt;
            let lease = fs::OpenOptions::new().read(true).write(true)
                .open(slot.with_extension("lock")).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match lease.try_lock_exclusive() {
                    Ok(()) => break,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
                        assert!(Instant::now() < deadline, "overlay cleanup did not release {}", slot.display());
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("overlay lease check failed: {error}"),
                }
            }
        }
        let directory = tempfile::tempdir().unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(directory.path())
                .status()
                .unwrap()
                .success()
        );
        fs::create_dir_all(directory.path().join("src")).unwrap();
        fs::write(
            directory.path().join("src/a.rs"),
            "pub fn a() -> u32 { 1 }\n",
        )
        .unwrap();
        fs::write(
            directory.path().join("src/b.rs"),
            "pub fn b() -> u32 { a() }\n",
        )
        .unwrap();
        let cancel = AtomicBool::new(false);
        let (first_graph, first_cache, first_revision, first_slot) = {
            let revision = Revision::capture(directory.path()).unwrap();
            let mut overlay = Overlay::create(directory.path(), &revision).unwrap();
            let mut worker = Workspace::new(
                &mut overlay,
                true,
                vec![],
                &cancel,
                Duration::from_secs(10),
                ExecutionPolicy {
                    sandbox: SandboxMode::ReadOnly,
                    network: false,
                },
                &[],
                None,
            )
            .unwrap();
            assert!(!worker.call("map", &json!({"path":"src"})).is_error);
            assert_eq!(worker.graph.parses(), 2);
            assert!(!worker.structural_cache.lock().unwrap().entries.is_empty());
            (
                Arc::clone(&worker.graph),
                Arc::clone(&worker.structural_cache),
                revision,
                worker.root.clone(),
            )
        };
        // Drop removes the overlay asynchronously while keeping the lane leased.
        // This test checks sequential reuse, so wait for that actual precondition.
        wait_for_cleanup(&first_slot);

        {
            let mut overlay = Overlay::create(directory.path(), &first_revision).unwrap();
            let mut worker = Workspace::new(
                &mut overlay,
                true,
                vec![],
                &cancel,
                Duration::from_secs(10),
                ExecutionPolicy {
                    sandbox: SandboxMode::ReadOnly,
                    network: false,
                },
                &[],
                None,
            )
            .unwrap();
            assert!(Arc::ptr_eq(&first_cache, &worker.structural_cache));
            assert!(!worker.call("map", &json!({"path":"src"})).is_error);
            assert!(!worker.structural_cache.lock().unwrap().entries.is_empty());
        }
        wait_for_cleanup(&first_slot);

        fs::write(
            directory.path().join("src/a.rs"),
            "pub fn a() -> u32 { 2 }\n",
        )
        .unwrap();
        let revision = Revision::capture(directory.path()).unwrap();
        let mut overlay = Overlay::create(directory.path(), &revision).unwrap();
        let mut worker = Workspace::new(
            &mut overlay,
            true,
            vec![],
            &cancel,
            Duration::from_secs(10),
            ExecutionPolicy {
                sandbox: SandboxMode::ReadOnly,
                network: false,
            },
            &[],
            None,
        )
        .unwrap();
        assert!(Arc::ptr_eq(&first_graph, &worker.graph));
        assert!(!worker.call("map", &json!({"path":"src"})).is_error);
        assert_eq!(
            worker.graph.parses(),
            3,
            "only the changed file should be reparsed"
        );
    }

    #[test]
    fn concurrent_overlay_slots_use_independent_graph_lanes() {
        let directory = tempfile::tempdir().unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(directory.path())
                .status()
                .unwrap()
                .success()
        );
        fs::write(
            directory.path().join("lib.rs"),
            "pub fn value() -> u32 { 1 }\n",
        )
        .unwrap();
        let base = Revision::capture(directory.path()).unwrap();
        let cancel = AtomicBool::new(false);
        let first_graph = {
            let mut first_overlay = Overlay::create(directory.path(), &base).unwrap();
            let mut first = Workspace::new(
                &mut first_overlay,
                true,
                vec![],
                &cancel,
                Duration::from_secs(10),
                ExecutionPolicy {
                    sandbox: SandboxMode::ReadOnly,
                    network: false,
                },
                &[],
                None,
            )
            .unwrap();
            assert!(!first.call("map", &json!({"path":"."})).is_error);
            let first_graph = Arc::clone(&first.graph);

            let mut second_overlay = Overlay::create(directory.path(), &base).unwrap();
            let mut second = Workspace::new(
                &mut second_overlay,
                true,
                vec![],
                &cancel,
                Duration::from_secs(10),
                ExecutionPolicy {
                    sandbox: SandboxMode::ReadOnly,
                    network: false,
                },
                &[],
                None,
            )
            .unwrap();
            assert!(!second.call("map", &json!({"path":"."})).is_error);
            assert!(
                !Arc::ptr_eq(&first.graph, &second.graph),
                "concurrent overlay slots must not share one current graph"
            );
            first_graph
        };

        fs::write(
            directory.path().join("lib.rs"),
            "pub fn value() -> u32 { 2 }\n",
        )
        .unwrap();
        let next = Revision::capture(directory.path()).unwrap();
        let mut overlay = Overlay::create(directory.path(), &next).unwrap();
        let mut worker = Workspace::new(
            &mut overlay,
            true,
            vec![],
            &cancel,
            Duration::from_secs(10),
            ExecutionPolicy {
                sandbox: SandboxMode::ReadOnly,
                network: false,
            },
            &[],
            None,
        )
        .unwrap();
        assert!(
            Arc::ptr_eq(&first_graph, &worker.graph),
            "the sequential slot lane should retain its graph across revisions"
        );
        assert!(!worker.call("map", &json!({"path":"."})).is_error);
        assert_eq!(
            worker.graph.parses(),
            2,
            "slot 0 should only parse the changed file on the next revision"
        );
    }
}
