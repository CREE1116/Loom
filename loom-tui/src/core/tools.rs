//! Core-owned code tools for the native worker (docs/loom-core-tools.md).
//!
//! The tools decide how much of the repository reaches the model: large files come back
//! as an outline unless a range is asked for, searches are grouped by symbol and capped,
//! edits require a fresh read and are rejected if they introduce a Rust syntax error.
use super::graph::{CodeGraph, GraphIndex};
use super::model::ToolSpec;
use super::repository::RepositoryExplorer;
use super::telemetry::Phase;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeSet, HashMap},
    fs,
    io::{self, Write},
    path::{Component, Path, PathBuf},
    process::Command,
    sync::Arc,
    time::Duration,
};

const READ_MAX_LINES: usize = 400;
const OUTPUT_MAX: usize = 12_000;
const FIND_MAX_LINES: usize = 80;

pub struct ToolOutput {
    pub text: String,
    pub is_error: bool,
    pub phase: Phase,
    /// The worker should stop: `finish` was called.
    pub finish: Option<String>,
}

impl ToolOutput {
    fn ok(phase: Phase, text: impl Into<String>) -> Self {
        Self {
            text: clip(text.into()),
            is_error: false,
            phase,
            finish: None,
        }
    }
    fn err(phase: Phase, text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
            phase,
            finish: None,
        }
    }
}

fn clip(text: String) -> String {
    if text.len() <= OUTPUT_MAX {
        return text;
    }
    let mut cut = OUTPUT_MAX;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!(
        "{}\n… output truncated ({} bytes total); narrow the request",
        &text[..cut],
        text.len()
    )
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub struct Workspace {
    root: PathBuf,
    explorer: RepositoryExplorer,
    graph: GraphIndex,
    read: HashMap<PathBuf, String>,
    pub read_only: bool,
    /// Validation commands run by `check`, e.g. `cargo test --quiet`.
    pub checks: Vec<String>,
    pub edits: Vec<String>,
}

impl Workspace {
    pub fn open(root: &Path) -> anyhow::Result<Self> {
        let root = dunce::canonicalize(root)?;
        let checks = if root.join("Cargo.toml").exists() {
            vec![
                "cargo build --quiet".into(),
                "cargo test --quiet -- --test-threads=1".into(),
            ]
        } else if root.join("package.json").exists() {
            vec!["npm test --silent".into()]
        } else if root.join("pyproject.toml").exists() || root.join("setup.py").exists() {
            vec!["python3 -m pytest -q".into()]
        } else {
            vec![]
        };
        Ok(Self {
            explorer: RepositoryExplorer::new(&root)?,
            graph: GraphIndex::default(),
            root,
            read: HashMap::new(),
            read_only: false,
            checks,
            edits: Vec::new(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn graph(&self) -> Option<Arc<CodeGraph>> {
        let snapshot = self.explorer.snapshot().ok()?;
        self.graph.graph(&snapshot).ok()
    }

    /// Resolve a workspace-relative path, refusing anything outside the root.
    fn resolve(&self, path: &str) -> Result<(PathBuf, String), String> {
        let rel = Path::new(path.trim().trim_start_matches("./"));
        if rel.is_absolute() && !rel.starts_with(&self.root) {
            return Err(format!("{path} is outside the workspace"));
        }
        let rel = rel.strip_prefix(&self.root).unwrap_or(rel);
        if rel.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err(format!("{path}: `..` is not allowed"));
        }
        let shown = rel.to_string_lossy().replace('\\', "/");
        Ok((self.root.join(rel), shown))
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = vec![
            spec(
                "find",
                "Search file contents (ripgrep). Results are grouped by the enclosing symbol. Use a specific pattern; broad searches are refused.",
                json!({
                    "type": "object", "required": ["pattern"],
                    "properties": {
                        "pattern": {"type": "string", "description": "regex, or literal text when literal=true"},
                        "path": {"type": "string", "description": "file or directory to search in (default: whole workspace)"},
                        "literal": {"type": "boolean"}
                    }
                }),
            ),
            spec(
                "outline",
                "List the symbols of a file (or the files of a directory) without bodies. Use this before reading a large file.",
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
                "read_artifact",
                "Read numbered lines of a raw check log returned as an artifact path. Use when the check summary lacks needed detail (max 400 lines per call).",
                json!({
                    "type": "object", "required": ["path", "start", "end"],
                    "properties": {
                        "path": {"type": "string", "description": ".loom/artifacts/<n>.log from check"},
                        "start": {"type": "integer", "description": "first line, 1-based, inclusive"},
                        "end": {"type": "integer", "description": "last line, 1-based, inclusive"}
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
        }
        specs.push(spec("finish", "Stop and report. Call when the task is done, or when you cannot make progress (say why).", json!({
            "type": "object", "required": ["summary"], "properties": {"summary": {"type": "string"}}
        })));
        specs
    }

    pub fn call(&mut self, name: &str, args: &Value) -> ToolOutput {
        let s = |k: &str| args[k].as_str().unwrap_or_default();
        match name {
            "find" => self.find(
                s("pattern"),
                args["path"].as_str(),
                args["literal"].as_bool().unwrap_or(false),
            ),
            "outline" => self.outline(s("path")),
            "read" => self.read_tool(s("path"), args["start"].as_u64(), args["end"].as_u64()),
            "read_artifact" => {
                self.read_artifact(s("path"), args["start"].as_u64(), args["end"].as_u64())
            }
            "refs" => self.refs(s("id")),
            "edit" if !self.read_only => self.edit(
                s("path"),
                s("old"),
                s("new"),
                args["replace_all"].as_bool().unwrap_or(false),
            ),
            "write" if !self.read_only => self.write(s("path"), s("content")),
            "check" if !self.read_only => self.check(),
            "finish" => ToolOutput {
                text: "ok".into(),
                is_error: false,
                phase: Phase::Message,
                finish: Some(s("summary").to_owned()),
            },
            other => ToolOutput::err(Phase::Orient, format!("unknown tool `{other}`")),
        }
    }

    // ------------------------------------------------------------------ read side

    fn find(&self, pattern: &str, path: Option<&str>, literal: bool) -> ToolOutput {
        if pattern.trim().len() < 2 {
            return ToolOutput::err(Phase::Orient, "pattern too short");
        }
        let target = match path.map(|p| self.resolve(p)) {
            Some(Err(e)) => return ToolOutput::err(Phase::Orient, e),
            Some(Ok((abs, _))) => abs,
            None => self.root.clone(),
        };
        let mut cmd = Command::new("rg");
        cmd.args([
            "-n",
            "--no-heading",
            "--color=never",
            "--max-columns=200",
            "--max-count=30",
        ]);
        if literal {
            cmd.arg("-F");
        }
        let out = match cmd
            .arg("--")
            .arg(pattern)
            .arg(&target)
            .current_dir(&self.root)
            .output()
        {
            Ok(out) => out,
            Err(e) => return ToolOutput::err(Phase::Orient, format!("ripgrep unavailable: {e}")),
        };
        let text = String::from_utf8_lossy(&out.stdout);
        let lines: Vec<&str> = text.lines().collect();
        if lines.is_empty() {
            return ToolOutput::ok(Phase::Orient, format!("no matches for `{pattern}`"));
        }
        let files: std::collections::BTreeSet<&str> = lines
            .iter()
            .filter_map(|l| split_rg_line(l).map(|(file, _, _)| file))
            .collect();
        if lines.len() > FIND_MAX_LINES {
            let mut msg = format!(
                "{} matches in {} files — too broad; narrow the pattern or path. Files:\n",
                lines.len(),
                files.len()
            );
            for f in files.iter().take(40) {
                msg.push_str(&format!("  {}\n", self.rel(f)));
            }
            return ToolOutput::ok(Phase::Orient, msg);
        }
        let graph = self.graph();
        let mut out = String::new();
        for line in lines {
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

    fn outline(&self, path: &str) -> ToolOutput {
        let (abs, rel) = match self.resolve(path) {
            Ok(p) => p,
            Err(e) => return ToolOutput::err(Phase::Orient, e),
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
        if rel.ends_with(".rs")
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
            let mut by_file: std::collections::BTreeMap<&str, Vec<&crate::core::graph::Symbol>> =
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
            let mut by_file: std::collections::BTreeMap<&str, Vec<&crate::core::graph::Symbol>> =
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
        let (abs, rel) = match self.resolve(path) {
            Ok(p) => p,
            Err(e) => return ToolOutput::err(Phase::Orient, e),
        };
        let parts: Vec<_> = Path::new(&rel).components().collect();
        let filename = parts
            .last()
            .map(|c| c.as_os_str().to_string_lossy())
            .unwrap_or_default();
        let number = filename.strip_suffix(".log").unwrap_or("");
        if parts.len() != 3
            || parts[0] != Component::Normal(".loom".as_ref())
            || parts[1] != Component::Normal("artifacts".as_ref())
            || number.is_empty()
            || !number.chars().all(|c| c.is_ascii_digit())
        {
            return ToolOutput::err(
                Phase::Orient,
                "read_artifact requires a .loom/artifacts/<n>.log path",
            );
        }
        // Guard against a symlinked artifacts directory or log escaping the workspace.
        let artifacts = self.root.join(".loom/artifacts");
        let real_artifacts = dunce::canonicalize(&artifacts);
        if !real_artifacts
            .as_ref()
            .is_ok_and(|p| p.starts_with(&self.root))
            || !dunce::canonicalize(&abs)
                .is_ok_and(|p| p.starts_with(real_artifacts.as_ref().unwrap()))
        {
            return ToolOutput::err(Phase::Orient, "artifact not found inside workspace");
        }
        let (Some(a), Some(b)) = (start, end) else {
            return ToolOutput::err(Phase::Orient, "read_artifact needs start and end lines");
        };
        if a == 0 || b < a || b - a >= READ_MAX_LINES as u64 {
            return ToolOutput::err(
                Phase::Orient,
                "invalid artifact range (1-based, max 400 lines)",
            );
        }
        self.read_lines(&rel, Some(a), Some(b))
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
        self.read.insert(abs.clone(), digest(&bytes));
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

    // ------------------------------------------------------------------ write side

    fn fresh(&self, abs: &Path, rel: &str) -> Result<String, String> {
        let bytes =
            fs::read(abs).map_err(|_| format!("{rel} does not exist; use write to create it"))?;
        match self.read.get(abs) {
            None => Err(format!("read {rel} before editing it")),
            Some(d) if *d != digest(&bytes) => {
                Err(format!("{rel} changed since you read it; read it again"))
            }
            Some(_) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
        }
    }

    fn edit(&mut self, path: &str, old: &str, new: &str, replace_all: bool) -> ToolOutput {
        let (abs, rel) = match self.resolve(path) {
            Ok(p) => p,
            Err(e) => return ToolOutput::err(Phase::Edit, e),
        };
        let text = match self.fresh(&abs, &rel) {
            Ok(t) => t,
            Err(e) => return ToolOutput::err(Phase::Edit, e),
        };
        if old.is_empty() {
            return ToolOutput::err(Phase::Edit, "old must not be empty");
        }
        let (ranges, how) = match find_matches(&text, old) {
            Some(found) => found,
            None => {
                return ToolOutput::err(
                    Phase::Edit,
                    format!("`old` not found in {rel}; read the exact lines again"),
                );
            }
        };
        if ranges.len() > 1 && !replace_all {
            let at: Vec<usize> = ranges
                .iter()
                .map(|(s, _)| text[..*s].lines().count() + 1)
                .collect();
            return ToolOutput::err(
                Phase::Edit,
                format!(
                    "`old` matches {} places (lines {at:?}); add context or set replace_all",
                    ranges.len()
                ),
            );
        }
        let mut updated = String::with_capacity(text.len() + new.len());
        let mut last = 0;
        for (s, e) in &ranges {
            updated.push_str(&text[last..*s]);
            updated.push_str(new);
            last = *e;
        }
        updated.push_str(&text[last..]);
        if let Some(problem) = new_syntax_error(&rel, &text, &updated) {
            return ToolOutput::err(
                Phase::Edit,
                format!("edit rejected, it would break the syntax of {rel}: {problem}"),
            );
        }
        if let Err(e) = fs::write(&abs, &updated) {
            return ToolOutput::err(Phase::Edit, format!("cannot write {rel}: {e}"));
        }
        self.read.insert(abs, digest(updated.as_bytes()));
        self.edits.push(rel.clone());
        let line = updated[..ranges[0].0].lines().count().max(1);
        let lines: Vec<&str> = updated.lines().collect();
        let (a, b) = (
            line.saturating_sub(3).max(1),
            (line + new.lines().count() + 2).min(lines.len()),
        );
        let mut out = format!(
            "edited {rel} ({} place{}, matched {how})\n",
            ranges.len(),
            if ranges.len() == 1 { "" } else { "s" }
        );
        for (i, l) in lines.iter().enumerate().take(b).skip(a - 1) {
            out.push_str(&format!("{:>5}| {l}\n", i + 1));
        }
        ToolOutput::ok(Phase::Edit, out)
    }

    fn write(&mut self, path: &str, content: &str) -> ToolOutput {
        let (abs, rel) = match self.resolve(path) {
            Ok(p) => p,
            Err(e) => return ToolOutput::err(Phase::Edit, e),
        };
        if abs.exists() {
            return ToolOutput::err(Phase::Edit, format!("{rel} exists; use edit"));
        }
        if rel.ends_with(".rs")
            && let Err(e) = syn::parse_file(content)
        {
            return ToolOutput::err(Phase::Edit, format!("not valid Rust: {e}"));
        }
        if let Some(parent) = abs.parent() {
            let _ = fs::create_dir_all(parent);
        }
        match fs::write(&abs, content) {
            Ok(()) => {
                self.read.insert(abs, digest(content.as_bytes()));
                self.edits.push(rel.clone());
                ToolOutput::ok(
                    Phase::Edit,
                    format!("created {rel} ({} lines)", content.lines().count()),
                )
            }
            Err(e) => ToolOutput::err(Phase::Edit, format!("cannot write {rel}: {e}")),
        }
    }

    fn check(&self) -> ToolOutput {
        if self.checks.is_empty() {
            return ToolOutput::err(
                Phase::Validate,
                "no build/test command known for this workspace",
            );
        }
        let mut raw = Vec::new();
        let mut commands = Vec::new();
        let mut passed = 0;
        let mut failed_tests = 0;
        let mut failing_tests = BTreeSet::new();
        let mut first_compiler_error = None;
        let mut failed = false;
        for command in &self.checks {
            let started = std::time::Instant::now();
            let label = command
                .split_whitespace()
                .take(2)
                .collect::<Vec<_>>()
                .join(" ");
            #[cfg(unix)]
            let child = Command::new("sh")
                .arg("-c")
                .arg(command)
                .current_dir(&self.root)
                .output();
            #[cfg(windows)]
            let child = Command::new("cmd")
                .args(["/C", command])
                .current_dir(&self.root)
                .output();
            raw.extend_from_slice(format!("$ {command}\n").as_bytes());
            let out = match child {
                Ok(out) => out,
                Err(e) => {
                    failed = true;
                    commands.push(
                        json!({"command": label, "status": "failed", "error": e.to_string()}),
                    );
                    raw.extend_from_slice(format!("cannot run `{command}`: {e}\n").as_bytes());
                    break;
                }
            };
            raw.extend_from_slice(&out.stdout);
            raw.extend_from_slice(&out.stderr);
            if !raw.ends_with(b"\n") {
                raw.push(b'\n');
            }
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            parse_check_output(
                &text,
                &mut passed,
                &mut failed_tests,
                &mut failing_tests,
                &mut first_compiler_error,
            );
            let ok = out.status.success();
            commands.push(json!({
                "command": label,
                "status": if ok { "passed" } else { "failed" },
                "seconds": (started.elapsed().as_millis() as f64) / 1000.0
            }));
            if !ok {
                failed = true;
                break;
            }
        }
        let artifact = match self.save_check_artifact(&raw) {
            Ok(path) => path,
            Err(e) => {
                return ToolOutput::err(Phase::Validate, format!("cannot save check log: {e}"));
            }
        };
        let report = json!({
            "status": if failed { "failed" } else { "passed" },
            "commands": commands,
            "passed": passed,
            "failed": failed_tests,
            "failing_tests": failing_tests,
            "first_compiler_error": first_compiler_error,
            "artifact": artifact,
        })
        .to_string();
        if failed {
            ToolOutput::err(Phase::Validate, report)
        } else {
            ToolOutput::ok(Phase::Validate, report)
        }
    }

    fn save_check_artifact(&self, raw: &[u8]) -> io::Result<String> {
        let dir = self.root.join(".loom/artifacts");
        fs::create_dir_all(&dir)?;
        if !dunce::canonicalize(&dir)?.starts_with(&self.root) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "artifact directory outside workspace",
            ));
        }
        for n in 1.. {
            let rel = format!(".loom/artifacts/{n}.log");
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(self.root.join(&rel))
            {
                Ok(mut file) => {
                    file.write_all(raw)?;
                    return Ok(rel);
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        unreachable!()
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

/// Byte ranges of `needle` in `text`: exact first, then line-trimmed, then
/// whitespace-collapsed matching (as opencode, Aider and Cline do).
pub(crate) fn find_matches(
    text: &str,
    needle: &str,
) -> Option<(Vec<(usize, usize)>, &'static str)> {
    let exact: Vec<(usize, usize)> = text
        .match_indices(needle)
        .map(|(i, m)| (i, i + m.len()))
        .collect();
    if !exact.is_empty() {
        return Some((exact, "exactly"));
    }
    // Line-based fuzzy: compare trimmed lines, then whitespace-collapsed lines.
    let starts: Vec<usize> = std::iter::once(0)
        .chain(text.match_indices('\n').map(|(i, _)| i + 1))
        .collect();
    let lines: Vec<&str> = text.split('\n').collect();
    let want: Vec<&str> = needle.trim_matches('\n').split('\n').collect();
    let collapse = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    for (how, norm) in [
        (
            "ignoring indentation",
            &(|s: &str| s.trim().to_owned()) as &dyn Fn(&str) -> String,
        ),
        ("ignoring whitespace", &collapse as &dyn Fn(&str) -> String),
    ] {
        let want: Vec<String> = want.iter().map(|l| norm(l)).collect();
        let mut found = Vec::new();
        for i in 0..lines.len().saturating_sub(want.len() - 1) {
            if (0..want.len()).all(|k| norm(lines[i + k]) == want[k]) {
                let end_line = i + want.len() - 1;
                let end = starts[end_line] + lines[end_line].len();
                found.push((starts[i], end));
            }
        }
        if !found.is_empty() {
            return Some((found, how));
        }
    }
    None
}

/// For Rust files: an error the edit introduces (a file that was already broken may stay broken).
fn new_syntax_error(rel: &str, before: &str, after: &str) -> Option<String> {
    if !rel.ends_with(".rs") || syn::parse_file(before).is_err() {
        return None;
    }
    syn::parse_file(after)
        .err()
        .map(|e| format!("line {}: {e}", e.span().start().line))
}

/// Extract only actionable diagnostics; the full stdout/stderr lives in the artifact.
fn parse_check_output(
    output: &str,
    passed: &mut u64,
    failed: &mut u64,
    failing_tests: &mut BTreeSet<String>,
    first_compiler_error: &mut Option<Value>,
) {
    let lines: Vec<&str> = output.lines().collect();
    let mut in_failures = false;
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("test result:") {
            let parts: Vec<&str> = rest.split_whitespace().collect();
            for pair in parts.windows(2) {
                if pair[1] == "passed;" {
                    *passed += pair[0].parse::<u64>().unwrap_or(0);
                }
                if pair[1] == "failed;" {
                    *failed += pair[0].parse::<u64>().unwrap_or(0);
                }
            }
        }
        if let Some(name) = t
            .strip_prefix("---- ")
            .and_then(|s| s.strip_suffix(" stdout ----"))
        {
            failing_tests.insert(name.to_owned());
        }
        if t == "failures:" {
            in_failures = true;
            continue;
        }
        if in_failures {
            if t.is_empty() {
                in_failures = false;
            } else if !t.contains(char::is_whitespace) && !t.starts_with("----") {
                failing_tests.insert(t.to_owned());
            }
        }
        if first_compiler_error.is_none() && (t.starts_with("error[") || t.starts_with("error:")) {
            for loc in lines.iter().skip(i + 1).take(8) {
                let loc = loc.trim();
                if loc.starts_with("error") {
                    break;
                }
                if let Some(where_) = loc.strip_prefix("--> ") {
                    let mut pieces = where_.rsplitn(3, ':');
                    let _column = pieces.next();
                    let row = pieces.next().and_then(|n| n.parse::<u64>().ok());
                    let file = pieces.next();
                    if let (Some(file), Some(line)) = (file, row) {
                        *first_compiler_error =
                            Some(json!({ "message": t, "file": file, "line": line }));
                        break;
                    }
                }
            }
        }
    }
}

#[allow(dead_code)]
const _TIMEOUT: Duration = Duration::from_secs(900);

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/lib.rs"), "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\npub fn twice(x: i32) -> i32 {\n    add(x, x)\n}\n").unwrap();
        let ws = Workspace::open(dir.path()).unwrap();
        (dir, ws)
    }

    #[test]
    fn check_summarizes_failing_tests_and_stores_full_log() {
        let (dir, mut ws) = workspace();
        ws.checks = if cfg!(windows) {
            vec![
                "echo build succeeded".into(),
                "(echo running 5 tests & echo ---- tests::all stdout ---- & echo assertion details that should only live in the artifact & echo failures: & echo     tests::all & echo     tests::hours_minutes & echo. & echo test result: FAILED. 3 passed; 2 failed; 0 ignored; 0 measured) & exit /b 1".into(),
            ]
        } else {
            vec![
                "printf 'build succeeded\\n'".into(),
                "printf 'running 5 tests\\n---- tests::all stdout ----\\nassertion details that should only live in the artifact\\nfailures:\\n    tests::all\\n    tests::hours_minutes\\n\\ntest result: FAILED. 3 passed; 2 failed; 0 ignored; 0 measured\\n'; exit 1".into(),
            ]
        };
        let output = ws.call("check", &json!({}));
        assert!(output.is_error);
        let result: Value = serde_json::from_str(&output.text).unwrap();
        assert_eq!(result["status"], "failed");
        assert_eq!(result["passed"], 3);
        assert_eq!(result["failed"], 2);
        assert_eq!(
            result["failing_tests"],
            json!(["tests::all", "tests::hours_minutes"])
        );
        assert!(
            !output
                .text
                .contains("assertion details that should only live in the artifact")
        );
        let path = result["artifact"].as_str().unwrap();
        assert!(path.starts_with(".loom/artifacts/") && path.ends_with(".log"));
        let raw = fs::read_to_string(dir.path().join(path)).unwrap();
        assert!(raw.contains("build succeeded"));
        assert!(raw.contains("assertion details that should only live in the artifact"));
    }

    #[test]
    fn check_reports_first_compiler_error_location() {
        let (_dir, mut ws) = workspace();
        ws.checks = if cfg!(windows) {
            vec!["(echo error[E0308]: mismatched types & echo   --^> src/lib.rs:12:7 & echo error: could not compile package) 1>&2 & exit /b 1".into()]
        } else {
            vec!["printf 'error[E0308]: mismatched types\\n  --> src/lib.rs:12:7\\nerror: could not compile package\\n' >&2; exit 1".into()]
        };
        let output = ws.call("check", &json!({}));
        assert!(output.is_error);
        let result: Value = serde_json::from_str(&output.text).unwrap();
        assert_eq!(
            result["first_compiler_error"],
            json!({
                "message": "error[E0308]: mismatched types", "file": "src/lib.rs", "line": 12
            })
        );
    }

    #[test]
    fn check_sums_multiple_successful_test_suites() {
        let (dir, mut ws) = workspace();
        ws.checks = if cfg!(windows) {
            vec![
                "echo build ok".into(),
                "echo test result: ok. 4 passed; 0 failed; 0 ignored & echo test result: ok. 2 passed; 0 failed; 0 ignored".into(),
            ]
        } else {
            vec![
                "printf 'build ok\\n'".into(),
                "printf 'test result: ok. 4 passed; 0 failed; 0 ignored\\ntest result: ok. 2 passed; 0 failed; 0 ignored\\n'".into(),
            ]
        };
        let output = ws.call("check", &json!({}));
        assert!(!output.is_error, "{}", output.text);
        let result: Value = serde_json::from_str(&output.text).unwrap();
        assert_eq!(result["status"], "passed");
        assert_eq!(result["passed"], 6);
        assert_eq!(result["failed"], 0);
        assert_eq!(result["failing_tests"], json!([]));
        assert!(
            dir.path()
                .join(result["artifact"].as_str().unwrap())
                .exists()
        );
    }

    #[test]
    fn read_artifact_returns_requested_lines_only() {
        let (dir, mut ws) = workspace();
        let folder = dir.path().join(".loom/artifacts");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("1.log"), "first\nsecond\nthird\nfourth\n").unwrap();
        let output = ws.call(
            "read_artifact",
            &json!({
                "path": ".loom/artifacts/1.log", "start": 2, "end": 3
            }),
        );
        assert!(!output.is_error, "{}", output.text);
        assert!(output.text.contains("second") && output.text.contains("third"));
        assert!(!output.text.contains("first") && !output.text.contains("fourth"));
        assert!(
            ws.call("read_artifact", &json!({"path": "src/lib.rs"}))
                .is_error
        );
        assert!(
            ws.call("read_artifact", &json!({"path": "../outside.log"}))
                .is_error
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_artifact_refuses_symlinks_outside_the_workspace() {
        let (dir, mut ws) = workspace();
        let other = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".loom/artifacts")).unwrap();
        fs::write(other.path().join("secret.log"), "external data\n").unwrap();
        std::os::unix::fs::symlink(
            other.path().join("secret.log"),
            dir.path().join(".loom/artifacts/1.log"),
        )
        .unwrap();
        let output = ws.call(
            "read_artifact",
            &json!({
                "path": ".loom/artifacts/1.log", "start": 1, "end": 1
            }),
        );
        assert!(output.is_error);
        assert!(!output.text.contains("external data"));
    }

    #[test]
    fn edit_requires_a_fresh_read_and_reports_fuzzy_matches() {
        let (dir, mut ws) = workspace();
        let r = ws.call(
            "edit",
            &json!({"path": "src/lib.rs", "old": "a + b", "new": "b + a"}),
        );
        assert!(r.is_error && r.text.contains("read src/lib.rs before"));
        ws.call("read", &json!({"path": "src/lib.rs"}));
        let r = ws.call(
            "edit",
            &json!({"path": "src/lib.rs", "old": "  a + b", "new": "    a.wrapping_add(b)"}),
        );
        assert!(!r.is_error, "{}", r.text);
        let r = ws.call(
            "edit",
            &json!({"path": "src/lib.rs", "old": "add(x,   x)", "new": "    add(x, x) * 1"}),
        );
        assert!(r.text.contains("ignoring whitespace"), "{}", r.text);
        fs::write(dir.path().join("src/lib.rs"), "pub fn changed() {}\n").unwrap();
        let r = ws.call(
            "edit",
            &json!({"path": "src/lib.rs", "old": "changed", "new": "x"}),
        );
        assert!(r.text.contains("changed since you read it"));
    }

    #[test]
    fn syntax_breaking_edit_is_rejected_and_file_untouched() {
        let (dir, mut ws) = workspace();
        ws.call("read", &json!({"path": "src/lib.rs"}));
        let r = ws.call(
            "edit",
            &json!({"path": "src/lib.rs", "old": "    a + b\n}", "new": "    a + b"}),
        );
        assert!(
            r.is_error && r.text.contains("break the syntax"),
            "{}",
            r.text
        );
        assert!(
            fs::read_to_string(dir.path().join("src/lib.rs"))
                .unwrap()
                .contains("a + b\n}")
        );
    }

    #[test]
    fn workspace_boundary_and_symbol_reads() {
        let (_dir, mut ws) = workspace();
        assert!(ws.call("read", &json!({"path": "../etc/passwd"})).is_error);
        assert!(ws.call("read", &json!({"path": "/etc/passwd"})).is_error);
        let r = ws.call("read", &json!({"path": "src/lib.rs::twice"}));
        assert!(
            r.text.contains("add(x, x)") && !r.text.contains("a + b"),
            "{}",
            r.text
        );
        let r = ws.call("outline", &json!({"path": "src/lib.rs"}));
        assert!(
            r.text.contains("twice") && r.text.contains("pub fn add"),
            "{}",
            r.text
        );
        let r = ws.call("find", &json!({"pattern": "add\\(", "path": "src"}));
        assert!(r.text.contains("[twice]"), "grouped by symbol: {}", r.text);
    }

    #[test]
    fn read_only_workspace_offers_no_edit_tools() {
        let (_dir, mut ws) = workspace();
        ws.read_only = true;
        let names: Vec<_> = ws.specs().into_iter().map(|s| s.name).collect();
        assert_eq!(
            names,
            ["find", "outline", "read", "read_artifact", "refs", "finish"]
        );
        assert!(
            ws.call(
                "edit",
                &json!({"path": "src/lib.rs", "old": "a", "new": "b"})
            )
            .is_error
        );
    }

    #[test]
    fn refs_lists_callers_and_callees_and_errors_on_unknown() {
        let (_dir, ws) = workspace();
        let r = ws.refs("src/lib.rs::add");
        assert!(!r.is_error, "{}", r.text);
        assert!(r.text.contains("callers:"));
        assert!(
            r.text.contains("src/lib.rs::twice"),
            "add is called by twice: {}",
            r.text
        );
        assert!(r.text.contains("approximate, name-resolved"));

        let r = ws.refs("src/lib.rs::twice");
        assert!(!r.is_error, "{}", r.text);
        assert!(r.text.contains("callees:"));
        assert!(
            r.text.contains("src/lib.rs::add"),
            "twice calls add: {}",
            r.text
        );

        let r = ws.refs("src/lib.rs::nonexistent");
        assert!(r.is_error);
        assert!(r.text.contains("unknown symbol `src/lib.rs::nonexistent`"));
    }

    #[test]
    fn anchor_candidates_extracts_identifiers_and_caps_output() {
        let (_dir, ws) = workspace();
        let empty = ws.find_anchor_candidates("");
        assert!(empty.is_empty());

        let generic = ws.find_anchor_candidates("please fix this bug in the system");
        assert!(generic.is_empty());

        let candidate = ws.find_anchor_candidates("Please check the `twice` function");
        assert!(candidate.contains("twice"));
        assert!(candidate.contains("src/lib.rs"));

        // Test length limit
        let long_str = "twice ".repeat(500);
        let capped = ws.find_anchor_candidates(&long_str);
        assert!(capped.len() <= 1500);
    }
}
