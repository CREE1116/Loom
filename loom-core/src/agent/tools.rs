use crate::agent::{
    query::{Find, Operator, Relation, Sets},
    world::{Node, View, WorldHub},
};
use anyhow::{Context, Result, bail, ensure};
use crate::{
    base::{ArtifactRef, ArtifactStore},
    effects::{ExecutionPolicy, Overlay, Runner, TestResult},
    instructions::Instructions,
    models::ToolSpec,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs,
    path::{Component, Path, PathBuf},
    process::Command,
    sync::atomic::AtomicBool,
    time::Duration,
};

pub fn specs(delegate: bool, readonly: bool) -> Vec<ToolSpec> {
    let mut tools = vec![
        spec(
            "find",
            "Find targets by symbol name, path substring, or literal text. Set kind explicitly to symbol, path, or text. scope is an optional repository path or symbol target, never the search kind. Returns a complete set handle and a few representative names. No natural-language intent inference.",
            json!({"query":{"type":"string"},"kind":{"type":"string","enum":["symbol","path","text"]},"scope":{"type":"string"}}),
            &["query", "kind"],
        ),
        spec(
            "expand",
            "Follow one relation from a set handle returned by find or expand (for example S1). A repository path is not a set handle: first find it with kind=path. File and symbol membership are distinct: use files/symbols to convert before intersection. Reference edges are name-resolved candidates, not exact type analysis.",
            json!({"set":{"type":"string"},"relation":{"type":"string","enum":["owner","children","descendants","files","symbols","references","referenced_by","tests","package"]}}),
            &["set", "relation"],
        ),
        spec(
            "combine",
            "Compute exact set union, intersection or ordered difference. Uses complete sets, never just displayed representatives.",
            json!({"op":{"type":"string","enum":["union","intersect","difference"]},"sets":{"type":"array","minItems":2,"maxItems":32,"items":{"type":"string"}}}),
            &["op", "sets"],
        ),
        spec(
            "project",
            "Show more representative names from a set without changing membership. Page with offset/limit.",
            json!({"set":{"type":"string"},"offset":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":80}}),
            &["set"],
        ),
        spec(
            "open",
            "Read actual source for a target or small set. Directory targets show representative children. Command results return artifact_target; pass that value here unchanged to read durable output. artifact:<hash> and a returned raw artifact hash are both accepted. Large files require a zero-based line range. Handles are revision-pinned; refresh after changes.",
            json!({"target":{"type":"string"},"start":{"type":"integer","minimum":0},"end":{"type":"integer","minimum":0}}),
            &["target"],
        ),
        spec(
            "git",
            "Read isolated copy status, diff or history. No mutations.",
            json!({"op":{"type":"string","enum":["status","diff","log"]}}),
            &["op"],
        ),
        spec(
            "use_skill",
            "Load the full instructions of an installed skill named in the system prompt, before following it.",
            json!({"name":{"type":"string"}}),
            &["name"],
        ),
        spec(
            "finish",
            "Submit your result to Core. For a child this only submits its proposal. Core owns merged validation, approval and success.",
            json!({"summary":{"type":"string"},"unresolved":{"type":"boolean"}}),
            &["summary"],
        ),
        spec(
            "ask",
            "Ask the user a necessary question; wait for an answer. Child workers report blockers to the orchestrator instead.",
            json!({"question":{"type":"string"}}),
            &["question"],
        ),
    ];
    if !readonly {
        tools.push(spec("change", "Change one current target: edit exact old/new text; replace a whole symbol with content; create a new file; delete or move a previously opened file. Read before modifying. Writes remain inside your assigned scope.",json!({"op":{"type":"string","enum":["edit","replace","create","delete","move"]},"target":{"type":"string"},"old":{"type":"string"},"new":{"type":"string"},"content":{"type":"string"},"destination":{"type":"string"}}), &["op","target"]));
        tools.push(spec("run", "Run a bounded shell command. When command is supplied, omitted kind defaults to command. kind=check runs the supplied command, or configured validation when command is omitted. Commands run in an isolated copy of the repository with the repository root as working directory: use relative paths. Create or edit files with change (op=create/edit), not shell redirection; file changes are proposed for approval. Do not use shell ls/find/grep for repository navigation or code search; use find/open/expand/project instead. Successful output includes its first 8000 bytes; failures include the last 1500 bytes. When output_truncated is true, use open on artifact_target to read more instead of rerunning the command. Full validation of child proposals belongs to Core after merge.",json!({"kind":{"type":"string","enum":["command","check"]},"command":{"type":"string","minLength":1}}), &[]));
    }
    if delegate {
        tools.push(spec("agent", "Delegate a DAG of concrete goals. Calling this switches the main worker to orchestration. Each unit requires id, goal, write_files and depends_on. Children share the World parse index, use isolated revision branches, and cannot delegate. Core schedules, merges and validates. Dependencies start from merged predecessor code.",json!({"units":{"type":"array","minItems":1,"maxItems":16,"items":{"type":"object","additionalProperties":false,"required":["id","goal","write_files","depends_on"],"properties":{"id":{"type":"string"},"goal":{"type":"string"},"write_files":{"type":"array","items":{"type":"string"}},"depends_on":{"type":"array","items":{"type":"string"}}}}}}), &["units"]));
    }
    tools
}
fn spec(name: &str, description: &str, properties: Value, required: &[&str]) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: description.into(),
        parameters: json!({"type":"object","additionalProperties":false,"properties":properties,"required":required}),
    }
}

fn is_artifact_id(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn run_result_value(result: TestResult, store: &dyn ArtifactStore) -> Result<Value> {
    let artifact_target = format!("artifact:{}", result.artifact.id);
    // Return enough successful output to make progress without a second call.
    // For failures preserve the diagnostic tail; full output stays in the artifact.
    let length = result.artifact.bytes as usize;
    let success = result.status == crate::effects::RunStatus::Passed;
    let (start, end) = if success { (0, length.min(8000)) }
        else { (length.saturating_sub(1500), length) };
    let output = store.get(&result.artifact, Some(start..end))
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    let mut value = serde_json::to_value(result)?;
    if let Some(object) = value.as_object_mut() {
        object.insert("artifact_target".into(), json!(artifact_target));
        object.insert("output_truncated".into(), json!(start > 0 || end < length));
        if !output.trim().is_empty() {
            object.insert("output".into(), json!(output));
        }
    }
    Ok(value)
}

/// Commands run in the isolated copy of the workspace. A path into the real workspace
/// means the same file in the copy, so point it there instead of letting the sandbox
/// reject the write (the change is still proposed for approval like any other).
fn in_copy(command: &str, workspace: &Path, copy: &Path) -> String {
    let copy = copy.to_string_lossy();
    let workspace = workspace.to_string_lossy();
    let base = workspace.trim_end_matches(['/', '\\']);
    let mut roots = vec![base.to_owned()];
    if let Some(short) = base.strip_prefix("/private") {
        roots.push(short.trim_end_matches('/').to_owned());
    }
    if let Some(plain) = base.strip_prefix(r"\\?\") {
        let plain = plain.trim_end_matches(['/', '\\']);
        roots.push(plain.to_owned());
        if plain.contains('\\') {
            roots.push(plain.replace('\\', "/"));
        }
    } else if base.as_bytes().get(1) == Some(&b':') {
        roots.insert(0, format!(r"\\?\{base}"));
        if base.contains('\\') {
            roots.push(base.replace('\\', "/"));
        }
    }
    roots.retain(|root| !root.is_empty());
    // One pass, so the copy's own path (inside the workspace) is never rewritten again.
    let path_char = |c: char| c.is_alphanumeric() || "/\\._-~".contains(c);
    let mut out = String::with_capacity(command.len());
    let mut index = 0;
    while index < command.len() {
        let rest = &command[index..];
        let starts_path = command[..index].chars().next_back().is_none_or(|c| !path_char(c));
        let found = starts_path
            .then(|| {
                roots.iter().find(|root| {
                    rest.starts_with(root.as_str())
                        && rest[root.len()..]
                            .chars()
                            .next()
                            .is_none_or(|c| matches!(c, '/' | '\\') || !path_char(c))
                })
            })
            .flatten();
        if let Some(root) = found {
            out.push_str(&copy);
            index += root.len();
        } else {
            let c = rest.chars().next().unwrap();
            out.push(c);
            index += c.len_utf8();
        }
    }
    out
}

pub struct Host<'a> {
    pub overlay: &'a mut Overlay,
    pub view: View,
    pub sets: Sets,
    pub hub: WorldHub,
    pub scope: Option<BTreeSet<PathBuf>>,
    pub readonly: bool,
    pub orchestrating: bool,
    pub checks: Vec<String>,
    pub policy: ExecutionPolicy,
    pub instructions: Instructions,
    /// Tools from `.loom/config.toml` (MCP servers, web search), when configured.
    pub extensions: Option<crate::extensions::Extensions>,
}
impl Host<'_> {
    /// Extension tools this task may see (read-only tasks see read-only ones only).
    pub fn extension_tools(&self) -> Vec<ToolSpec> {
        let Some(extensions) = &self.extensions else { return vec![] };
        extensions
            .specs()
            .into_iter()
            .filter(|spec| !self.readonly || extensions.is_read_only(&spec.name))
            .collect()
    }
    pub fn refresh(&mut self, cancel: &AtomicBool) -> Result<()> {
        self.view = self.hub.view_cancellable(self.overlay.dir(), cancel)?;
        Ok(())
    }
    pub fn call(
        &mut self,
        name: &str,
        args: &Value,
        cancel: &AtomicBool,
        remaining: Duration,
    ) -> Result<Value> {
        ensure!(!cancel.load(std::sync::atomic::Ordering::Acquire), "tool cancelled");
        let string = |key: &str| -> Result<&str> {
            args[key].as_str().with_context(|| format!("missing {key}"))
        };
        match name {
            "find" => self
                .sets
                .find(&self.view, serde_json::from_value::<Find>(args.clone())?),
            "expand" => self.sets.expand(
                &self.view,
                string("set")?,
                serde_json::from_value::<Relation>(args["relation"].clone())?,
            ),
            "combine" => self.sets.combine(
                &self.view,
                serde_json::from_value::<Operator>(args["op"].clone())?,
                &serde_json::from_value::<Vec<String>>(args["sets"].clone())?,
            ),
            "project" => self.sets.project(
                &self.view,
                string("set")?,
                args["limit"].as_u64().unwrap_or(8) as usize,
                args["offset"].as_u64().unwrap_or(0) as usize,
            ),
            "open" => self.open(
                string("target")?,
                args["start"].as_u64(),
                args["end"].as_u64(),
            ),
            "use_skill" => {
                let name = string("name")?;
                let skills = crate::instructions::discover(self.overlay.workspace())?;
                let skill = skills
                    .iter()
                    .find(|s| s.name == name)
                    .with_context(|| format!("no installed skill named {name}; installed: {}", skills.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", ")))?;
                Ok(json!({"skill": skill.name, "instructions": crate::instructions::load_skill(&skill.path)?}))
            }
            "change" => self.change(args, cancel),
            "run" => {
                ensure!(!self.readonly, "read-only task cannot execute commands");
                let kind = match args.get("kind") {
                    Some(value) => value.as_str().context("kind must be a string")?,
                    None if args["command"].as_str().is_some() => "command",
                    None => bail!("supply command, or kind=check for configured validation"),
                };
                if self.scope.is_some() {
                    ensure!(
                        kind == "check",
                        "scoped worker cannot run commands with unknown write effects"
                    );
                    return Ok(
                        json!({"status":"deferred","reason":"Core validates the merged DAG; finish your scoped proposal"}),
                    );
                }
                let commands = match kind {
                    "check" if args.get("command").is_none() => self.checks.clone(),
                    "command" | "check" => {
                        let command = string("command")?;
                        ensure!(!command.trim().is_empty(), "command must not be empty");
                        vec![in_copy(command, self.overlay.workspace(), self.overlay.dir())]
                    }
                    _ => bail!("unknown run kind"),
                };
                ensure!(
                    !commands.is_empty(),
                    "no checks configured; supply a validation command"
                );
                let started = std::time::Instant::now();
                let runner = Runner::new(self.overlay.artifacts().clone());
                let mut results = Vec::new();
                for command in commands {
                    let time = remaining.saturating_sub(started.elapsed());
                    ensure!(!time.is_zero(), "command deadline reached");
                    let result = runner.run_with_policy(
                        self.overlay.dir(),
                        &command,
                        time,
                        cancel,
                        Some(&self.overlay.target_dir()),
                        self.policy,
                    )?;
                    let mut value = run_result_value(result, self.overlay.artifacts())?;
                    // Show paths as the model knows them, not the copy's location.
                    if let Some(output) = value["output"].as_str() {
                        let copy = self.overlay.dir().to_string_lossy().into_owned();
                        let workspace = self.overlay.workspace().to_string_lossy().into_owned();
                        value["output"] = json!(output.replace(&copy, &workspace));
                    }
                    results.push(value);
                }
                self.refresh(cancel)?;
                Ok(Value::Array(results))
            }
            "git" => {
                let argv = match string("op")? {
                    "status" => vec!["status", "--short"],
                    "diff" => vec!["diff", "--no-ext-diff", "--no-textconv"],
                    "log" => vec!["log", "-10", "--oneline"],
                    _ => bail!("unknown git operation"),
                };
                let result = crate::base::cancellable_output(Command::new("git")
                    .current_dir(self.overlay.dir())
                    .args(argv), cancel)?;
                ensure!(
                    result.status.success(),
                    "git failed: {}",
                    String::from_utf8_lossy(&result.stderr)
                );
                Ok(json!({"text":clip(&String::from_utf8_lossy(&result.stdout),12000)}))
            }
            other if self.extension_tools().iter().any(|spec| spec.name == other) => {
                let extensions = self.extensions.as_ref().expect("listed above");
                ensure!(!self.readonly || extensions.is_read_only(other), "read-only task cannot call a mutating extension");
                ensure!(self.policy.network || !extensions.requires_network(other), "network access is disabled by execution policy");
                Ok(json!({"result": extensions.call(other, args, cancel, remaining)?}))
            }
            _ => bail!("unknown or unavailable tool {name}"),
        }
    }
    fn open(&mut self, target: &str, start: Option<u64>, end: Option<u64>) -> Result<Value> {
        let explicit_artifact = target.strip_prefix("artifact:");
        let raw_artifact = is_artifact_id(target).then_some(target);
        if let Some(id) = explicit_artifact.or(raw_artifact) {
            let mut reference = ArtifactRef {
                id: id.into(),
                bytes: 0,
            };
            let artifact_path = self.overlay.artifacts().path(&reference)?;
            if explicit_artifact.is_none() && !artifact_path.is_file() {
                return self.open_target(target, start, end);
            }
            reference.bytes = fs::metadata(artifact_path)?.len();
            ensure!(
                reference.bytes <= 16 * 1024 * 1024,
                "artifact exceeds bounded reader"
            );
            let bytes = self.overlay.artifacts().get(&reference, None)?;
            let text = String::from_utf8_lossy(&bytes);
            let lines: Vec<_> = text.lines().collect();
            let a = start.unwrap_or(0) as usize;
            let b = end
                .map(|v| v as usize)
                .unwrap_or((a + 200).min(lines.len()));
            ensure!(
                a <= b && b <= lines.len() && b - a <= 400,
                "invalid artifact line range"
            );
            return Ok(
                json!({"target":target,"source":clip(&lines[a..b].join("\n"),8000),"start":a,"end":b,"lines":lines.len()}),
            );
        }
        self.open_target(target, start, end)
    }
    fn open_target(&mut self, target: &str, start: Option<u64>, end: Option<u64>) -> Result<Value> {
        let nodes = self.sets.targets(&self.view, target)?;
        ensure!(
            nodes.len() <= 8,
            "open accepts at most 8 targets; select representatives"
        );
        let mut results = Vec::new();
        let mut budget = 16000;
        for node in nodes {
            if matches!(node, Node::Directory(_)) {
                let children = self
                    .view
                    .nodes
                    .iter()
                    .filter(|n| self.view.owner(n).as_ref() == Some(&node))
                    .cloned()
                    .collect();
                results.push(self.sets.store(&self.view, children)?);
                continue;
            }
            let file = PathBuf::from(node.file().context("target has no source")?);
            let guidance = self.instructions.for_path(&file)?;
            let (a, b) = match &node {
                Node::Symbol(id) => {
                    let s = self.view.world.graph.get(id).context("symbol missing")?;
                    (Some(s.start.saturating_sub(1)), Some(s.end))
                }
                _ => (start.map(|n| n as usize), end.map(|n| n as usize)),
            };
            if let (Some(a), Some(b)) = (a, b) {
                ensure!(
                    b >= a && b - a <= 400,
                    "source exceeds 400 lines; request a narrower file range"
                );
            }
            let read = self.overlay.read(&file, a.zip(b).map(|(a, b)| a..b))?;
            if read.structured.get("outline").is_some() {
                let members=self.view.nodes.iter().filter(|n|matches!(n,Node::Symbol(id) if id.starts_with(&format!("{}::",file.display())))).cloned().collect();
                results.push(json!({"target":node.id(),"instructions":guidance,"select":self.sets.store(&self.view,members)?}));
            } else {
                let text = read.structured["text"].as_str().unwrap_or("");
                let displayed = clip(text, budget.min(8000));
                budget = budget.saturating_sub(displayed.len());
                results.push(json!({"target":node.id(),"instructions":guidance,"source":displayed,"start":a.unwrap_or(0),"revision":self.view.revision()}));
            }
        }
        Ok(json!(results))
    }
    fn writable(&self, path: &Path) -> Result<()> {
        ensure!(
            !self.readonly && !self.orchestrating,
            "writes unavailable in this role"
        );
        ensure!(
            !path.as_os_str().is_empty()
                && path.components().all(|p| matches!(p, Component::Normal(_))),
            "invalid write path"
        );
        ensure!(
            self.scope.as_ref().is_none_or(|scope| scope.contains(path)),
            "WRITE_SCOPE_CONFLICT: {}",
            path.display()
        );
        self.overlay.resolve(path)?;
        Ok(())
    }
    // The shared graph currently exposes line spans. Reject ambiguous boundaries
    // instead of guessing at byte ranges or replacing adjacent declarations.
    fn symbol_text(&self, id: &str, source: &str) -> Result<String> {
        let symbol = self.view.world.graph.get(id).context("symbol missing")?;
        for other in self.view.world.graph.in_file(&symbol.file) {
            if other.id == id || other.is_pseudo() {
                continue;
            }
            let descendant = other.id.starts_with(&format!("{id}::"));
            let wholly_inside = other.start > symbol.start && other.end < symbol.end;
            let enclosing = id.starts_with(&format!("{}::", other.id))
                && other.start < symbol.start
                && other.end > symbol.end;
            ensure!(
                enclosing
                    || descendant && wholly_inside
                    || other.end < symbol.start
                    || other.start > symbol.end,
                "symbol boundaries share lines or overlap; open the file and use an exact edit with surrounding context"
            );
        }
        let lines: Vec<_> = source.split_inclusive('\n').collect();
        let range = symbol.start.saturating_sub(1)..symbol.end;
        ensure!(
            range.start < range.end && range.end <= lines.len(),
            "invalid symbol span"
        );
        Ok(lines[range].concat())
    }
    fn change(&mut self, args: &Value, cancel: &AtomicBool) -> Result<Value> {
        let op = args["op"].as_str().context("missing operation")?;
        let target = args["target"].as_str().context("missing target")?;
        let node = if op == "create" {
            Node::File(target.into())
        } else {
            self.sets.single(&self.view, target)?
        };
        let path = PathBuf::from(node.file().context("cannot modify a directory")?);
        self.writable(&path)?;
        let guidance = self.instructions.for_path(&path)?;
        ensure!(
            guidance.is_empty(),
            "Read these scoped instructions before retrying:\n{guidance}"
        );
        let strarg = |key: &str| args[key].as_str().with_context(|| format!("missing {key}"));
        match op {
            "edit" => {
                let old = strarg("old")?;
                if let Node::Symbol(id) = &node {
                    let source = fs::read_to_string(self.overlay.resolve(&path)?)?;
                    let span = self.symbol_text(id, &source)?;
                    ensure!(
                        !old.is_empty() && span.contains(old),
                        "old text is outside the selected symbol; select the file explicitly if needed"
                    );
                    ensure!(
                        source.matches(old).count() == 1,
                        "symbol edit requires unique exact text; add context"
                    );
                }
                self.overlay.edit(&path, old, strarg("new")?)?;
            }
            "replace" => {
                let Node::Symbol(id) = node else {
                    bail!("replace requires a symbol target");
                };
                let source = fs::read_to_string(self.overlay.resolve(&path)?)?;
                let old = self.symbol_text(&id, &source)?;
                self.overlay.edit(&path, &old, strarg("content")?)?;
            }
            "create" => {
                self.overlay.write(&path, strarg("content")?)?;
            }
            "delete" | "move" => {
                ensure!(
                    matches!(node, Node::File(_)),
                    "delete/move requires a file target"
                );
                // Overlay edits enforce a current read before any destructive operation.
                let source = fs::read_to_string(self.overlay.resolve(&path)?)?;
                self.overlay.edit(&path, &source, &source)?;
                if op == "move" {
                    let destination = PathBuf::from(strarg("destination")?);
                    self.writable(&destination)?;
                    ensure!(
                        self.instructions.for_path(&destination)?.is_empty(),
                        "open destination scope instructions first"
                    );
                    self.overlay.write(&destination, &source)?;
                }
                fs::remove_file(self.overlay.resolve(&path)?)?;
            }
            _ => bail!("unknown change operation"),
        }
        self.refresh(cancel)?;
        Ok(json!({"changed":path,"revision":self.view.revision()}))
    }
}
pub fn clip(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.into();
    }
    let mut end = cap;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… truncated; request a narrower range", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        base::ArtifactRef,
        effects::{RunStatus, TestResult},
    };

    #[test]
    fn tool_contracts_make_navigation_explicit() {
        let tools = specs(false, false);
        let find = tools.iter().find(|tool| tool.name == "find").unwrap();
        let required = find.parameters["required"].as_array().unwrap();
        assert!(required.iter().any(|value| value == "query"));
        assert!(required.iter().any(|value| value == "kind"));
        assert!(find.description.contains("scope is an optional repository path"));

        let run = tools.iter().find(|tool| tool.name == "run").unwrap();
        assert!(run.description.contains("Do not use shell ls/find/grep"));
        assert_eq!(run.parameters["properties"]["command"]["minLength"], 1);
    }

    #[test]
    fn commands_point_real_workspace_paths_at_the_copy_only_at_path_boundaries() {
        let workspace = Path::new("/private/var/w/repo");
        let copy = Path::new("/private/var/w/repo/.loom/overlays/r/0");
        assert_eq!(
            in_copy("mkdir -p /private/var/w/repo/a && cat '/var/w/repo/b' /private/var/w/repo2/c", workspace, copy),
            "mkdir -p /private/var/w/repo/.loom/overlays/r/0/a && cat '/private/var/w/repo/.loom/overlays/r/0/b' /private/var/w/repo2/c"
        );
        assert_eq!(in_copy("ls src", workspace, copy), "ls src");
    }

    #[test]
    fn successful_readme_output_is_inline_and_large_output_is_bounded() {
        let root = tempfile::tempdir().unwrap();
        let store = crate::base::FileArtifactStore::open(root.path()).unwrap();
        for size in [7526, 20000] {
            let text = "R".repeat(size);
            let artifact = store.put(text.as_bytes()).unwrap();
            let value = run_result_value(TestResult {
                status: crate::effects::RunStatus::Passed, passed:0, failed:0,
                failing:vec![], first_error:None, artifact, ms:1,
            }, &store).unwrap();
            let output = value["output"].as_str().expect("successful commands need visible output");
            assert_eq!(output.len(), size.min(8000));
            assert_eq!(value["output_truncated"], size > 8000);
            assert!(value["artifact_target"].as_str().unwrap().starts_with("artifact:"));
        }
    }

    #[test]
    fn command_result_returns_reusable_artifact_target() {
        let id = "a".repeat(64);
        let value = run_result_value(TestResult {
            status: RunStatus::Passed,
            passed: 0,
            failed: 0,
            failing: vec![],
            first_error: None,
            artifact: ArtifactRef {
                id: id.clone(),
                bytes: 4,
            },
            ms: 1,
        }, &crate::base::FileArtifactStore::open(tempfile::tempdir().unwrap().path()).unwrap())
        .unwrap();
        assert_eq!(value["artifact_target"], format!("artifact:{id}"));
        assert!(is_artifact_id(&id));
        assert!(!is_artifact_id(&id.to_uppercase()));
    }
}
