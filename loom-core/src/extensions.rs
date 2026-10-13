//! Standalone Native extension tools. `open` only reads `.loom/config.toml`;
//! `specs` lazily negotiates MCP and caches discovery. Export this module from
//! the Core library and pass its specs/results through the ordinary tool loop.
//!
//! Configuration (only explicit credentials/headers are used):
//! ```toml
//! [mcp_servers.example]
//! command = "my-mcp-server"
//! args = ["--stdio"]
//! # env = { API_KEY = "explicit-key" }
//! # Alternatively: url = "https://host/mcp", headers = { Authorization = "..." }
//! [web_search]
//! provider = "searxng" # or "brave" (requires api_key or explicit headers)
//! endpoint = "https://search.example/search"
//! # format = "html" # SearxNG installations without the JSON API
//! ```
//!
//! Discovery failures are exposed through `extensions_diagnostics` because the
//! ToolSpec API has no error channel. Calling it returns the full diagnostics.
//! Stdio I/O is nonblocking on Unix, including writes, and failure/cancellation
//! destroys and reaps the entire server process group. HTTP uses a bounded
//! worker: cancellation returns immediately; an in-flight socket closes by the
//! original request deadline (at most 30 seconds), without replaying requests.
//!
//! Native workers use `open_for_workspace(original, overlay, policy)`: config
//! stays in the original workspace, while stdio cwd and filesystem authority
//! follow the overlay's OS execution policy. Private temporary files and shared
//! build/artifact directories are explicit writable exceptions. ReadOnly policy
//! filters and rejects unannotated MCP tools; network policy applies to stdio
//! sandboxing, HTTP discovery, and calls. Remote mutations still need the
//! caller's approval flow. `open(root)` retains standalone full-access behavior.

use crate::effects::{ExecutionPolicy, SandboxMode};
use crate::models::ToolSpec;
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{
        Mutex, MutexGuard, TryLockError,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

const PROTOCOL: &str = "2025-03-26";
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_HTTP_TIME: Duration = Duration::from_secs(30);
const MAX_WIRE: usize = 1024 * 1024;
const MAX_OUTPUT: usize = 32 * 1024;
const MAX_TOOLS: usize = 1024;
const DIAGNOSTICS: &str = "extensions_diagnostics";

#[derive(Default, Deserialize)]
struct Config {
    #[serde(default)]
    mcp_servers: BTreeMap<String, ServerConfig>,
    #[serde(default, alias = "web")]
    web_search: Option<SearchConfig>,
}

#[derive(Clone, Deserialize)]
struct ServerConfig {
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    url: Option<String>,
    #[serde(default, alias = "http_headers")]
    headers: BTreeMap<String, String>,
    /// Sign in with `loom mcp login <name>`; see `mcp_oauth`.
    #[serde(default)]
    oauth: bool,
    /// Set for servers from the user's global config: they get network even when the
    /// task sandbox denies it (the user chose them; a cloned repo's config cannot).
    #[serde(skip)]
    trusted: bool,
    /// Global config only: argv whose stdout is the bearer token (e.g. `gh auth token`).
    #[serde(default)]
    bearer_command: Vec<String>,
    #[serde(default = "enabled")]
    enabled: bool,
}
fn enabled() -> bool {
    true
}

#[derive(Clone, Deserialize)]
struct SearchConfig {
    #[serde(default = "searxng")]
    provider: String,
    endpoint: Option<String>,
    api_key: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default = "json_format")]
    format: String,
}
fn searxng() -> String {
    "searxng".into()
}
fn json_format() -> String {
    "json".into()
}

pub struct Extensions {
    execution: Execution,
    web: Option<SearchConfig>,
    state: Mutex<State>,
}
struct Execution {
    config_root: PathBuf,
    root: PathBuf,
    policy: ExecutionPolicy,
}
struct State {
    servers: BTreeMap<String, Server>,
    routes: BTreeMap<String, Route>,
    specs: Vec<ToolSpec>,
    diagnostics: Vec<String>,
    discovered: bool,
}
struct Server {
    config: ServerConfig,
    transport: Option<Transport>,
    next_id: u64,
    protocol: String,
    capabilities: Value,
    discovered: bool,
}
enum Transport {
    Stdio(Process),
    Http { session: Option<String> },
}
struct Process {
    child: Child,
    input: ChildStdin,
    output: ChildStdout,
    buffered: Vec<u8>,
    _temporary: tempfile::TempDir,
}
#[derive(Clone)]
struct Route {
    server: String,
    method: &'static str,
    tool: Option<String>,
    read_only: bool,
}

/// MCP server of a published tool name (`mcp__<server>__<tool>`).
pub fn mcp_server_of(tool: &str) -> Option<&str> {
    tool.strip_prefix("mcp__")?.split("__").next()
}

/// Servers a request names directly (Jira/Confluence count as Atlassian), so their
/// tools can be listed up front instead of waiting for the model to ask.
pub fn servers_mentioned<'a>(
    goal: &str,
    servers: impl IntoIterator<Item = &'a str>,
) -> Vec<&'a str> {
    let goal = goal.to_lowercase();
    servers
        .into_iter()
        .filter(|server| {
            goal.contains(&server.to_lowercase())
                || (*server == "atlassian"
                    && ["jira", "confluence", "지라", "컨플루언스"]
                        .iter()
                        .any(|w| goal.contains(w)))
        })
        .collect()
}

/// User-wide config, merged under every workspace's `.loom/config.toml`.
pub fn global_config_path() -> Option<PathBuf> {
    crate::models::state_dir().map(|dir| dir.join("config.toml"))
}

fn read_config(path: &Path) -> Result<Option<Config>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(
            toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// Global config overlaid by the workspace one (same-named servers replace).
fn load_config(root: &Path) -> Result<Config> {
    let workspace_path = root.join(".loom/config.toml");
    let global_path = global_config_path();
    let same = global_path.as_ref().is_some_and(|g| {
        g.canonicalize()
            .ok()
            .is_some_and(|g| workspace_path.canonicalize().ok() == Some(g))
    });
    let mut config = match &global_path {
        Some(path) => read_config(path)?.unwrap_or_default(),
        None => Config::default(),
    };
    for server in config.mcp_servers.values_mut() {
        server.trusted = true;
    }
    if !same && let Some(workspace) = read_config(&workspace_path)? {
        for (name, server) in &workspace.mcp_servers {
            // Commands run unsandboxed in Loom's process; a cloned repo must not supply one.
            ensure!(
                server.bearer_command.is_empty(),
                "MCP server {name}: bearer_command is only allowed in the global config"
            );
        }
        config.mcp_servers.extend(workspace.mcp_servers);
        if workspace.web_search.is_some() {
            config.web_search = workspace.web_search;
        }
    }
    Ok(config)
}

/// Built-in servers offered by `/mcp`: (name, url, oauth, bearer_command).
pub const PRESETS: &[(&str, &str, bool, &[&str])] = &[
    ("atlassian", "https://mcp.atlassian.com/v1/mcp", true, &[]),
    (
        "github",
        "https://api.githubcopilot.com/mcp/",
        false,
        &["gh", "auth", "token"],
    ),
    ("gitlab", "https://gitlab.com/api/v4/mcp", true, &[]),
];

/// Append a preset to the global config. `url` overrides the preset URL
/// (e.g. a self-hosted GitLab's `https://host/api/v4/mcp`).
pub fn add_preset(name: &str, url: Option<&str>) -> Result<()> {
    let (_, default_url, oauth, command) = PRESETS
        .iter()
        .find(|(preset, ..)| *preset == name)
        .ok_or_else(|| anyhow!("unknown MCP preset {name}"))?;
    let url = url.unwrap_or(default_url);
    validate_url(url)?;
    let path = global_config_path().context("Loom state directory unavailable")?;
    let existing = fs::read_to_string(&path).unwrap_or_default();
    let config: Config =
        toml::from_str(&existing).with_context(|| format!("parse {}", path.display()))?;
    ensure!(
        !config.mcp_servers.contains_key(name),
        "MCP server {name} is already configured"
    );
    let q = |s: &str| toml::Value::String(s.into()).to_string();
    let mut block = format!("\n\n[mcp_servers.{name}]\nurl = {}\n", q(url));
    if *oauth {
        block.push_str("oauth = true\n");
    }
    if !command.is_empty() {
        let args: Vec<_> = command.iter().map(|a| q(a)).collect();
        block.push_str(&format!("bearer_command = [{}]\n", args.join(", ")));
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(
        &path,
        format!("{}{block}", existing.trim_end_matches('\n')).trim_start(),
    )
    .with_context(|| format!("write {}", path.display()))
}

/// Configured MCP servers (global + workspace): (name, url or command, oauth).
pub fn configured_servers(root: &Path) -> Result<Vec<(String, String, bool)>> {
    Ok(load_config(root)?
        .mcp_servers
        .into_iter()
        .filter(|(_, s)| s.enabled)
        .map(|(name, s)| (name, s.url.or(s.command).unwrap_or_default(), s.oauth))
        .collect())
}

/// URL of an `oauth = true` server (global + workspace config).
pub fn oauth_url(root: &Path, name: &str) -> Result<String> {
    let config = load_config(root)?;
    let server = config
        .mcp_servers
        .get(name)
        .ok_or_else(|| anyhow!("MCP server {name} is not configured"))?;
    ensure!(server.oauth, "MCP server {name} does not set oauth = true");
    let url = server
        .url
        .clone()
        .ok_or_else(|| anyhow!("MCP server {name}: oauth requires HTTP url"))?;
    validate_url(&url)?;
    Ok(url)
}

impl Extensions {
    pub fn open(root: &Path) -> Result<Self> {
        Self::open_for_workspace(
            root,
            root,
            ExecutionPolicy {
                sandbox: SandboxMode::FullAccess,
                network: true,
            },
        )
    }

    /// Reads only the original workspace's config. All child execution uses
    /// execution_root and the supplied OS policy; opening never launches tools.
    pub fn open_for_workspace(
        config_root: &Path,
        execution_root: &Path,
        policy: ExecutionPolicy,
    ) -> Result<Self> {
        let config_root = config_root
            .canonicalize()
            .context("opening MCP config workspace")?;
        let execution_root = execution_root
            .canonicalize()
            .context("opening MCP execution workspace")?;
        let config = load_config(&config_root)?;
        for (name, server) in &config.mcp_servers {
            if !server.enabled {
                continue;
            }
            ensure!(!name.is_empty(), "MCP server name must not be empty");
            ensure!(
                server.command.is_some() ^ server.url.is_some(),
                "MCP server {name}: configure exactly one of command or url"
            );
            if let Some(command) = &server.command {
                ensure!(
                    !command.trim().is_empty(),
                    "MCP server {name}: empty command"
                );
                ensure!(
                    server.headers.is_empty(),
                    "MCP server {name}: headers require HTTP url"
                );
            }
            if let Some(url) = &server.url {
                validate_url(url).with_context(|| format!("MCP server {name}: invalid URL"))?;
                ensure!(
                    server.args.is_empty() && server.env.is_empty(),
                    "MCP server {name}: args/env require stdio command"
                );
            }
            ensure!(
                !server.oauth || server.url.is_some(),
                "MCP server {name}: oauth requires HTTP url"
            );
            ensure!(
                server.bearer_command.is_empty() || (server.url.is_some() && !server.oauth),
                "MCP server {name}: bearer_command requires HTTP url and excludes oauth"
            );
            validate_headers(&server.headers)?;
            for (key, value) in &server.env {
                ensure!(
                    !key.is_empty() && !key.contains(['=', '\0']) && !value.contains('\0'),
                    "MCP server {name}: invalid environment variable"
                );
            }
        }
        if let Some(web) = &config.web_search {
            ensure!(
                matches!(web.provider.as_str(), "searxng" | "brave"),
                "web_search.provider must be searxng or brave"
            );
            ensure!(
                matches!(web.format.as_str(), "json" | "html"),
                "web_search.format must be json or html"
            );
            ensure!(
                web.provider != "brave" || web.format == "json",
                "Brave requires JSON format"
            );
            if let Some(endpoint) = &web.endpoint {
                validate_url(endpoint).context("invalid web_search endpoint")?;
            }
            ensure!(
                web.provider != "searxng" || web.endpoint.is_some(),
                "configure web_search.endpoint for SearxNG"
            );
            validate_headers(&web.headers)?;
            if let Some(key) = &web.api_key {
                ensure!(!key.contains(['\r', '\n']), "invalid web_search.api_key");
            }
        }
        let servers = config
            .mcp_servers
            .into_iter()
            .filter(|(_, c)| c.enabled)
            .map(|(name, config)| {
                (
                    name,
                    Server {
                        config,
                        transport: None,
                        next_id: 1,
                        protocol: PROTOCOL.into(),
                        capabilities: Value::Null,
                        discovered: false,
                    },
                )
            })
            .collect();
        Ok(Self {
            execution: Execution {
                config_root,
                root: execution_root,
                policy,
            },
            web: config.web_search,
            state: Mutex::new(State {
                servers,
                routes: BTreeMap::new(),
                specs: Vec::new(),
                diagnostics: Vec::new(),
                discovered: false,
            }),
        })
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.specs_with_network(true)
    }

    /// Per-server discovery outcome for status screens: tool count or failure reason.
    pub fn server_status(&self) -> Vec<(String, std::result::Result<usize, String>)> {
        self.specs();
        let cancel = AtomicBool::new(false);
        let Ok(state) = self.lock(&cancel, Instant::now() + DISCOVERY_TIMEOUT) else {
            return Vec::new();
        };
        state
            .servers
            .keys()
            .map(|name| {
                let prefix = format!("MCP server {name} discovery failed: ");
                let failure = state
                    .diagnostics
                    .iter()
                    .find_map(|d| d.strip_prefix(&prefix));
                let tools = state.routes.values().filter(|r| &r.server == name).count();
                let status = match failure {
                    Some(reason) if tools == 0 => Err(reason.to_owned()),
                    _ => Ok(tools),
                };
                (name.clone(), status)
            })
            .collect()
    }

    /// Adds a per-discovery network restriction. It cannot override the stored
    /// execution policy; stdio servers always inherit the stored OS policy.
    pub fn specs_with_network(&self, allow_network: bool) -> Vec<ToolSpec> {
        let requested = allow_network;
        let allow_network = allow_network && self.execution.policy.network;
        let cancel = AtomicBool::new(false);
        let deadline = Instant::now() + DISCOVERY_TIMEOUT;
        match self.lock(&cancel, deadline) {
            Ok(mut state) => {
                if let Err(e) = self.discover(&mut state, &cancel, deadline, requested) {
                    state.diagnostics.push(format!("MCP discovery: {e:#}"));
                }
                let mut specs = builtin_specs();
                specs.extend(
                    state
                        .specs
                        .iter()
                        .filter(|spec| {
                            (self.execution.policy.sandbox != SandboxMode::ReadOnly
                                || state.routes.get(&spec.name).is_some_and(|r| r.read_only))
                                && (allow_network
                                    || state.routes.get(&spec.name).is_some_and(|r| {
                                        let config = &state.servers[&r.server].config;
                                        config.url.is_none() || (requested && config.trusted)
                                    }))
                        })
                        .cloned(),
                );
                if !allow_network {
                    specs.retain(|s| s.name != "web_search" && s.name != "web_fetch");
                }
                if !state.diagnostics.is_empty() {
                    specs.push(ToolSpec {
                        name: DIAGNOSTICS.into(),
                        description: bounded(&format!(
                            "Extension discovery failures:\n{}",
                            state.diagnostics.join("\n")
                        )),
                        parameters: object_schema(json!({}), &[]),
                    });
                }
                specs
            }
            Err(e) => {
                let mut specs = builtin_specs();
                if !allow_network {
                    specs.clear();
                }
                specs.push(ToolSpec {
                    name: DIAGNOSTICS.into(),
                    description: format!("Extension discovery failed: {e:#}"),
                    parameters: object_schema(json!({}), &[]),
                });
                specs
            }
        }
    }

    /// Cached metadata only: never starts a process or network request. Unknown
    /// tools, resources, and unannotated MCP tools conservatively return false.
    pub fn is_read_only(&self, name: &str) -> bool {
        if matches!(name, "web_search" | "web_fetch") {
            return true;
        }
        self.state
            .try_lock()
            .ok()
            .and_then(|s| s.routes.get(name).map(|r| r.read_only))
            .unwrap_or(false)
    }

    /// Cached metadata only. Unknown tools conservatively require network.
    pub fn requires_network(&self, name: &str) -> bool {
        if matches!(name, "web_search" | "web_fetch") {
            return true;
        }
        if name == DIAGNOSTICS {
            return false;
        }
        self.state
            .try_lock()
            .ok()
            .and_then(|s| {
                s.routes.get(name).map(|r| {
                    let config = &s.servers[&r.server].config;
                    config.url.is_some() && !config.trusted
                })
            })
            .unwrap_or(true)
    }

    pub fn call(
        &self,
        name: &str,
        args: &Value,
        cancel: &AtomicBool,
        timeout: Duration,
    ) -> Result<String> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| anyhow!("invalid timeout"))?;
        check(cancel, deadline)?;
        ensure!(args.is_object(), "tool arguments must be a JSON object");
        if matches!(name, "web_search" | "web_fetch") {
            ensure!(
                self.execution.policy.network,
                "extension network access is disabled by execution policy"
            );
        }
        match name {
            "web_search" => return self.search(args, cancel, deadline),
            "web_fetch" => return fetch(args, cancel, deadline),
            _ => {}
        }
        let mut state = self.lock(cancel, deadline)?;
        if name == DIAGNOSTICS {
            return Ok(bounded(&state.diagnostics.join("\n")));
        }
        if !state.routes.contains_key(name) {
            self.discover(&mut state, cancel, deadline, true)?;
        }
        let route = state.routes.get(name).cloned().ok_or_else(|| {
            anyhow!(
                "unknown extension tool {name}; {}",
                state.diagnostics.join("; ")
            )
        })?;
        ensure!(
            self.execution.policy.sandbox != SandboxMode::ReadOnly || route.read_only,
            "read-only execution policy rejects mutating or unannotated MCP tool {name}"
        );
        let server = state
            .servers
            .get_mut(&route.server)
            .ok_or_else(|| anyhow!("missing MCP server {}", route.server))?;
        ensure!(
            server.config.url.is_none() || server.config.trusted || self.execution.policy.network,
            "MCP HTTP network access is disabled by execution policy"
        );
        let params = match route.method {
            "tools/call" => json!({"name":route.tool, "arguments":args}),
            "resources/read" => json!({"uri":required_string(args, "uri")?}),
            _ => args.clone(),
        };
        // A failed/cancelled connection is never reused with an outstanding RPC.
        let result = server
            .start(&self.execution, cancel, deadline)
            .and_then(|()| server.rpc(route.method, params, cancel, deadline));
        let result = match result {
            Ok(result) => result,
            Err(e) => {
                server.transport.take();
                let diagnostic = format!("MCP {} {}: {e:#}", route.server, route.method);
                state.diagnostics.push(diagnostic.clone());
                return Err(anyhow!(diagnostic));
            }
        };
        if route.method == "tools/call" {
            let output = tool_output(&result);
            ensure!(
                result.get("isError").and_then(Value::as_bool) != Some(true),
                "MCP {} tool failed: {}",
                route.server,
                output
            );
            Ok(output)
        } else {
            Ok(bounded(&serde_json::to_string(&result)?))
        }
    }

    fn lock(&self, cancel: &AtomicBool, deadline: Instant) -> Result<MutexGuard<'_, State>> {
        loop {
            check(cancel, deadline)?;
            match self.state.try_lock() {
                Ok(state) => return Ok(state),
                Err(TryLockError::Poisoned(_)) => bail!("extensions state lock poisoned"),
                Err(TryLockError::WouldBlock) => pause(cancel, deadline)?,
            }
        }
    }

    fn discover(
        &self,
        state: &mut State,
        cancel: &AtomicBool,
        deadline: Instant,
        allow_network: bool,
    ) -> Result<()> {
        let policy_network = self.execution.policy.network;
        if state.discovered {
            return Ok(());
        }
        // Discovery is transactional per server; partially listed tools are never published.
        let names: Vec<_> = state.servers.keys().cloned().collect();
        let mut used = BTreeSet::from([
            "web_search".to_string(),
            "web_fetch".to_string(),
            DIAGNOSTICS.to_string(),
        ]);
        used.extend(state.routes.keys().cloned());
        check(cancel, deadline)?;
        // Servers are independent, so connect them concurrently (a slow one no longer
        // adds to the others), then publish routes in name order as before.
        let ready: Vec<String> = names
            .into_iter()
            .filter(|name| {
                let server = &state.servers[name];
                let network = allow_network && (policy_network || server.config.trusted);
                !server.discovered && (network || server.config.url.is_none())
            })
            .collect();
        let pending: Vec<(String, Server)> = ready
            .into_iter()
            .map(|name| {
                let server = state.servers.remove(&name).unwrap();
                (name, server)
            })
            .collect();
        let execution = &self.execution;
        let discover = |name: &str, server: &mut Server| {
            let key = cache_key(name, &server.config);
            match cache_load(&key) {
                Some(tools) => Ok(tools),
                None => server
                    .discover(execution, cancel, deadline)
                    .inspect(|tools| cache_save(&key, tools)),
            }
        };
        // HTTP servers are connected concurrently (a slow one no longer adds to the
        // others). Stdio servers stay on this thread: the sandbox's die-with-parent
        // signal fires when the spawning *thread* exits, which would kill them.
        let (http, stdio): (Vec<_>, Vec<_>) = pending
            .into_iter()
            .partition(|(_, server)| server.config.url.is_some());
        let mut results: Vec<_> = thread::scope(|scope| {
            let workers: Vec<_> = http
                .into_iter()
                .map(|(name, mut server)| {
                    scope.spawn(move || {
                        let discovered = discover(&name, &mut server);
                        (name, server, discovered)
                    })
                })
                .collect();
            let mut results: Vec<_> = stdio
                .into_iter()
                .map(|(name, mut server)| {
                    let discovered = discover(&name, &mut server);
                    (name, server, discovered)
                })
                .collect();
            results.extend(
                workers
                    .into_iter()
                    .map(|worker| worker.join().expect("MCP discovery worker panicked")),
            );
            results
        });
        results.sort_by(|a, b| a.0.cmp(&b.0));
        let mut cancelled = None;
        for (name, mut server, discovered) in results {
            server.discovered = true;
            match discovered {
                Ok(tools) => {
                    for (raw, description, parameters, method, tool, read_only) in tools {
                        let full = format!("mcp__{}__{}", sanitize(&name), sanitize(&raw));
                        let tool_name = unique_name(&full, &mut used);
                        state.routes.insert(
                            tool_name.clone(),
                            Route {
                                server: name.clone(),
                                method,
                                tool,
                                read_only,
                            },
                        );
                        state.specs.push(ToolSpec {
                            name: tool_name,
                            description,
                            parameters,
                        });
                    }
                }
                Err(e) => {
                    server.transport.take();
                    state
                        .diagnostics
                        .push(format!("MCP server {name} discovery failed: {e:#}"));
                    if cancel.load(Ordering::Relaxed) {
                        cancelled.get_or_insert(e);
                    }
                }
            }
            state.servers.insert(name, server);
        }
        if let Some(e) = cancelled {
            return Err(e);
        }
        state.discovered = state.servers.values().all(|s| s.discovered);
        Ok(())
    }

    fn search(&self, args: &Value, cancel: &AtomicBool, deadline: Instant) -> Result<String> {
        let query = required_string(args, "query")?;
        let config = self.web.as_ref().ok_or_else(|| anyhow!("web_search is not configured; set [web_search] provider and endpoint in .loom/config.toml"))?;
        let endpoint = config
            .endpoint
            .as_deref()
            .unwrap_or("https://api.search.brave.com/res/v1/web/search");
        let count = args
            .get("count")
            .and_then(Value::as_u64)
            .unwrap_or(5)
            .clamp(1, 20);
        let mut headers = config.headers.clone();
        if config.provider == "brave" {
            if let Some(key) = &config.api_key {
                headers.insert("X-Subscription-Token".into(), key.clone());
            }
            ensure!(
                headers
                    .keys()
                    .any(|k| k.eq_ignore_ascii_case("X-Subscription-Token")),
                "configure web_search.api_key or X-Subscription-Token for Brave"
            );
        }
        let mut url = format!(
            "{endpoint}{}q={}&count={count}",
            if endpoint.contains('?') { '&' } else { '?' },
            encode(query)
        );
        if config.provider == "searxng" {
            url.push_str(&format!("&format={}", config.format));
        }
        let response = http(
            HttpRequest {
                url,
                headers,
                method: "GET",
                body: None,
                rpc_id: None,
            },
            cancel,
            deadline,
        )?;
        let body = response.body;
        let results = if config.format == "html" {
            html_search(&String::from_utf8(body).context("search HTML is not UTF-8")?)?
        } else {
            let value: Value = serde_json::from_slice(&body).context(
                "search endpoint did not return JSON; configure format='html' for SearxNG HTML",
            )?;
            let rows = if config.provider == "brave" {
                value.pointer("/web/results")
            } else {
                value.get("results")
            };
            rows.and_then(Value::as_array)
                .ok_or_else(|| anyhow!("search endpoint returned no results array"))?
                .iter()
                .filter_map(|row| {
                    let url = row.get("url")?.as_str()?;
                    if validate_url(url).is_err() {
                        return None;
                    }
                    Some((
                        plain_html(row.get("title").and_then(Value::as_str).unwrap_or("")),
                        url.to_string(),
                        plain_html(
                            row.get("content")
                                .or_else(|| row.get("description"))
                                .and_then(Value::as_str)
                                .unwrap_or(""),
                        ),
                    ))
                })
                .collect()
        };
        let mut output = format!("Search: {query}\nSource: {endpoint}\n");
        if results.is_empty() {
            output.push_str("No results returned by the search endpoint.\n");
        }
        for (title, url, snippet) in results.into_iter().take(count as usize) {
            output.push_str(&format!(
                "\n{}\nURL: {}\n{}\n",
                limited(&title, 512),
                url,
                limited(&snippet, 2000)
            ));
            if output.len() > MAX_OUTPUT {
                break;
            }
        }
        Ok(bounded(&output))
    }
}

type DiscoveredTool = (String, String, Value, &'static str, Option<String>, bool);

/// Tool lists are cached so a task does not wait on every server's round trips
/// (GitHub's list alone is ~2.5s); calls still connect on demand. `/mcp` refresh clears it.
const CACHE_TTL: Duration = Duration::from_secs(24 * 3600);
fn cache_dir() -> Option<PathBuf> {
    crate::models::state_dir().map(|dir| dir.join("mcp-cache"))
}
fn cache_key(name: &str, c: &ServerConfig) -> String {
    use sha2::{Digest, Sha256};
    let identity = format!(
        "{name}\n{:?}\n{:?}\n{:?}\n{:?}\n{}\n{:?}",
        c.url, c.command, c.args, c.env, c.oauth, c.bearer_command
    );
    Sha256::digest(identity.as_bytes())[..12]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn cache_load(key: &str) -> Option<Vec<DiscoveredTool>> {
    let path = cache_dir()?.join(format!("{key}.json"));
    let age = fs::metadata(&path).ok()?.modified().ok()?.elapsed().ok()?;
    if age > CACHE_TTL {
        return None;
    }
    let entries: Vec<Value> = serde_json::from_slice(&fs::read(&path).ok()?).ok()?;
    entries
        .into_iter()
        .map(|e| {
            let method = match e["method"].as_str()? {
                "tools/call" => "tools/call",
                "resources/list" => "resources/list",
                "resources/read" => "resources/read",
                "resources/templates/list" => "resources/templates/list",
                "prompts/list" => "prompts/list",
                "prompts/get" => "prompts/get",
                _ => return None,
            };
            Some((
                e["name"].as_str()?.to_owned(),
                e["description"].as_str()?.to_owned(),
                e["parameters"].clone(),
                method,
                e["tool"].as_str().map(str::to_owned),
                e["read_only"].as_bool()?,
            ))
        })
        .collect()
}
fn cache_save(key: &str, tools: &[DiscoveredTool]) {
    let Some(dir) = cache_dir() else { return };
    let entries: Vec<Value> = tools
        .iter()
        .map(|(name, description, parameters, method, tool, read_only)| {
            json!({"name": name, "description": description, "parameters": parameters,
                   "method": method, "tool": tool, "read_only": read_only})
        })
        .collect();
    // Best effort: a cache that cannot be written only costs the next discovery.
    let _ = fs::create_dir_all(&dir).and_then(|()| {
        fs::write(
            dir.join(format!("{key}.json")),
            serde_json::to_vec(&entries).unwrap_or_default(),
        )
    });
}
/// Forget cached tool lists (after config, sign-in, or an explicit refresh).
pub fn clear_discovery_cache() {
    if let Some(dir) = cache_dir() {
        let _ = fs::remove_dir_all(dir);
    }
}
impl Server {
    fn start(
        &mut self,
        execution: &Execution,
        cancel: &AtomicBool,
        deadline: Instant,
    ) -> Result<()> {
        if self.transport.is_some() {
            return Ok(());
        }
        check(cancel, deadline)?;
        self.transport = Some(if self.config.url.is_some() {
            Transport::Http { session: None }
        } else {
            Transport::Stdio(Process::spawn(&self.config, execution)?)
        });
        let init = self.rpc("initialize", json!({"protocolVersion":PROTOCOL,"capabilities":{},"clientInfo":{"name":"loom-core","version":env!("CARGO_PKG_VERSION")}}), cancel, deadline)?;
        let version = init
            .get("protocolVersion")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("initialize missing protocolVersion"))?;
        ensure!(
            matches!(
                version,
                "2024-11-05" | "2025-03-26" | "2025-06-18" | "2025-11-25"
            ),
            "unsupported MCP protocol version {version}"
        );
        self.protocol = version.into();
        self.capabilities = init
            .get("capabilities")
            .filter(|c| c.is_object())
            .cloned()
            .ok_or_else(|| anyhow!("initialize missing capabilities"))?;
        self.send(
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            None,
            cancel,
            deadline,
        )?;
        Ok(())
    }

    fn discover(
        &mut self,
        execution: &Execution,
        cancel: &AtomicBool,
        deadline: Instant,
    ) -> Result<Vec<DiscoveredTool>> {
        self.start(execution, cancel, deadline)?;
        let mut result = Vec::new();
        let mut raw_names = BTreeSet::new();
        if self.capabilities.get("resources").is_some() {
            result.push((
                "list_resources".into(),
                "List this MCP server's resources; optional cursor for pagination.".into(),
                object_schema(json!({"cursor":{"type":"string"}}), &[]),
                "resources/list",
                None,
                false,
            ));
            result.push((
                "read_resource".into(),
                "Read an MCP resource by its exact URI.".into(),
                object_schema(json!({"uri":{"type":"string"}}), &["uri"]),
                "resources/read",
                None,
                false,
            ));
        }
        if self.capabilities.get("tools").is_some() {
            let mut params = json!({});
            let mut cursors = BTreeSet::new();
            loop {
                let page = self.rpc("tools/list", params, cancel, deadline)?;
                let tools = page
                    .get("tools")
                    .and_then(Value::as_array)
                    .ok_or_else(|| anyhow!("tools/list missing tools array"))?;
                ensure!(
                    result.len() + tools.len() <= MAX_TOOLS,
                    "MCP discovery tool count limit exceeded"
                );
                for tool in tools {
                    let raw = tool
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|n| !n.is_empty())
                        .ok_or_else(|| anyhow!("tools/list tool missing name"))?;
                    ensure!(
                        raw_names.insert(raw.to_string()),
                        "duplicate MCP tool name {raw}"
                    );
                    let schema = tool
                        .get("inputSchema")
                        .filter(|s| s.is_object())
                        .cloned()
                        .ok_or_else(|| anyhow!("MCP tool {raw} missing inputSchema object"))?;
                    result.push((
                        raw.into(),
                        tool.get("description")
                            .and_then(Value::as_str)
                            .unwrap_or("MCP external tool")
                            .into(),
                        schema,
                        "tools/call",
                        Some(raw.into()),
                        tool.pointer("/annotations/readOnlyHint")
                            .and_then(Value::as_bool)
                            == Some(true),
                    ));
                }
                match page.get("nextCursor") {
                    None | Some(Value::Null) => break,
                    Some(Value::String(cursor)) if !cursor.is_empty() => {
                        ensure!(
                            cursors.len() < MAX_TOOLS && cursors.insert(cursor.clone()),
                            "MCP pagination cursor loop/limit"
                        );
                        params = json!({"cursor":cursor});
                    }
                    _ => bail!("invalid MCP nextCursor"),
                }
            }
        }
        Ok(result)
    }

    fn rpc(
        &mut self,
        method: &str,
        params: Value,
        cancel: &AtomicBool,
        deadline: Instant,
    ) -> Result<Value> {
        ensure!(
            self.transport.is_some(),
            "MCP connection closed; call specs on a new Extensions instance to rediscover"
        );
        let id = self.next_id;
        self.next_id += 1;
        let response = self.send(
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
            Some(id),
            cancel,
            deadline,
        )?;
        if let Some(error) = response.get("error") {
            bail!("JSON-RPC {method} error: {}", bounded(&error.to_string()));
        }
        response
            .get("result")
            .cloned()
            .ok_or_else(|| anyhow!("JSON-RPC {method} missing result"))
    }

    fn send(
        &mut self,
        message: Value,
        id: Option<u64>,
        cancel: &AtomicBool,
        deadline: Instant,
    ) -> Result<Value> {
        check(cancel, deadline)?;
        match self
            .transport
            .as_mut()
            .ok_or_else(|| anyhow!("MCP connection closed"))?
        {
            Transport::Stdio(process) => {
                process.write(&message, cancel, deadline)?;
                let Some(id) = id else {
                    return Ok(Value::Null);
                };
                let mut total = 0;
                loop {
                    let line = process.line(cancel, deadline)?;
                    total += line.len();
                    ensure!(total <= MAX_WIRE, "MCP RPC read limit exceeded");
                    let value: Value =
                        serde_json::from_slice(&line).context("invalid MCP JSON-RPC line")?;
                    let messages = match &value {
                        Value::Array(batch) => batch.clone(),
                        _ => vec![value],
                    };
                    for value in messages {
                        ensure!(
                            value.get("jsonrpc").and_then(Value::as_str) == Some("2.0"),
                            "invalid MCP JSON-RPC version"
                        );
                        if value.get("method").is_some() {
                            if let Some(request_id) = value.get("id") {
                                let reply = server_request_reply(&value, request_id);
                                process.write(&reply, cancel, deadline)?;
                            }
                        } else if value.get("id") == Some(&json!(id)) {
                            return Ok(value);
                        } else {
                            bail!("unexpected MCP response id");
                        }
                    }
                }
            }
            Transport::Http { session } => {
                let mut headers = self.config.headers.clone();
                set_header(
                    &mut headers,
                    "Accept",
                    "application/json, text/event-stream",
                );
                set_header(&mut headers, "Content-Type", "application/json");
                if let Some(session) = session.as_deref() {
                    set_header(&mut headers, "Mcp-Session-Id", session);
                }
                if self.config.oauth {
                    // ponytail: refresh is a blocking request outside cancel/deadline (bounded by its own 15s timeout).
                    let token = crate::mcp_oauth::bearer(self.config.url.as_deref().unwrap())?;
                    set_header(&mut headers, "Authorization", &format!("Bearer {token}"));
                }
                if let Some((program, args)) = self.config.bearer_command.split_first() {
                    // ponytail: runs per request (~20ms for gh); cache if a slow command shows up.
                    let output = Command::new(program)
                        .args(args)
                        .stdin(Stdio::null())
                        .stderr(Stdio::null())
                        .output()
                        .with_context(|| format!("run bearer_command {program}"))?;
                    let token = String::from_utf8(output.stdout).unwrap_or_default();
                    let token = token.trim();
                    ensure!(
                        output.status.success()
                            && !token.is_empty()
                            && token.bytes().all(|b| (0x21..=0x7e).contains(&b)),
                        "bearer_command {program} did not print a token"
                    );
                    set_header(&mut headers, "Authorization", &format!("Bearer {token}"));
                }
                if message.get("method").and_then(Value::as_str) != Some("initialize") {
                    set_header(&mut headers, "MCP-Protocol-Version", &self.protocol);
                }
                let response = http(
                    HttpRequest {
                        url: self.config.url.clone().unwrap(),
                        headers,
                        method: "POST",
                        body: Some(message.to_string()),
                        rpc_id: id,
                    },
                    cancel,
                    deadline,
                )?;
                if message.get("method").and_then(Value::as_str) == Some("initialize")
                    && let Some(value) = response.session
                {
                    ensure!(
                        !value.is_empty() && value.bytes().all(|b| (0x21..=0x7e).contains(&b)),
                        "invalid MCP session ID"
                    );
                    *session = Some(value);
                }
                if id.is_none() {
                    return Ok(Value::Null);
                }
                response
                    .rpc
                    .ok_or_else(|| anyhow!("missing HTTP JSON-RPC response"))
            }
        }
    }
}

impl Process {
    #[cfg(unix)]
    fn spawn(config: &ServerConfig, execution: &Execution) -> Result<Self> {
        use std::os::unix::process::CommandExt;
        let mut command = Command::new(config.command.as_deref().unwrap());
        // Keep only operational environment, never implicitly inherit API keys.
        command.env_clear();
        // configure_command rebuilds Command and cannot observe env_clear.
        // Explicit removals survive that wrapper and prevent ambient keys from
        // leaking back into the configured server's environment.
        for (name, _) in std::env::vars_os() {
            command.env_remove(name);
        }
        for key in ["PATH", "HOME", "SystemRoot", "LANG", "LC_ALL"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        command
            .args(&config.args)
            .envs(&config.env)
            .current_dir(&execution.root);
        let temporary_root = controlled_directory(&execution.root, "tmp")?;
        let temporary = tempfile::Builder::new()
            .prefix("mcp-")
            .tempdir_in(&temporary_root)?;
        let target = controlled_directory(&execution.config_root, "target")?;
        let artifacts = controlled_directory(&execution.config_root, "artifacts")?;
        command
            .env("TMPDIR", temporary.path())
            .env("TMP", temporary.path())
            .env("TEMP", temporary.path())
            .env("CARGO_TARGET_DIR", &target)
            .env("PYTHONDONTWRITEBYTECODE", "1");
        let mut policy = execution.policy;
        policy.network |= config.trusted;
        policy
            .configure_command(
                &mut command,
                &execution.root,
                &[temporary.path().to_owned(), target, artifacts],
            )
            .context("configure MCP execution sandbox (refusing unsandboxed fallback)")?;
        // The sandbox wrapper cannot transfer these settings; install them last.
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0);
        let mut child = command.spawn().context("start MCP stdio command")?;
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let process = Self {
            child,
            input,
            output,
            buffered: Vec::new(),
            _temporary: temporary,
        };
        nonblocking(&process.input)?;
        nonblocking(&process.output)?;
        Ok(process)
    }
    #[cfg(not(unix))]
    fn spawn(_: &ServerConfig, _: &Execution) -> Result<Self> {
        bail!(
            "MCP stdio process-group supervision requires Unix; use a streamable HTTP server on this platform"
        )
    }
    fn write(&mut self, message: &Value, cancel: &AtomicBool, deadline: Instant) -> Result<()> {
        let mut bytes = serde_json::to_vec(message)?;
        ensure!(bytes.len() <= MAX_WIRE, "MCP request size limit exceeded");
        bytes.push(b'\n');
        let mut offset = 0;
        while offset < bytes.len() {
            check(cancel, deadline)?;
            match self.input.write(&bytes[offset..]) {
                Ok(0) => bail!("MCP stdin closed"),
                Ok(n) => offset += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => pause(cancel, deadline)?,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e).context("write MCP stdin"),
            }
        }
        Ok(())
    }
    fn line(&mut self, cancel: &AtomicBool, deadline: Instant) -> Result<Vec<u8>> {
        let mut bytes = [0; 8192];
        loop {
            check(cancel, deadline)?;
            if let Some(end) = self.buffered.iter().position(|b| *b == b'\n') {
                ensure!(end <= MAX_WIRE, "MCP message size limit exceeded");
                return Ok(self.buffered.drain(..=end).collect());
            }
            ensure!(
                self.buffered.len() <= MAX_WIRE,
                "MCP message size limit exceeded"
            );
            match self.output.read(&mut bytes) {
                Ok(0) => bail!("MCP stdout closed before RPC response"),
                Ok(n) => self.buffered.extend_from_slice(&bytes[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => pause(cancel, deadline)?,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e).context("read MCP stdout"),
            }
        }
    }
}
#[cfg(unix)]
fn controlled_directory(root: &Path, name: &str) -> Result<PathBuf> {
    let mut path = root.to_owned();
    for component in [".loom", name] {
        path.push(component);
        match fs::symlink_metadata(&path) {
            Ok(meta) => ensure!(
                meta.is_dir() && !meta.file_type().is_symlink(),
                "MCP writable directory cannot be a symlink or file: {}",
                path.display()
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&path)?;
            }
            Err(e) => return Err(e).context("inspect MCP writable directory"),
        }
        ensure!(
            path.canonicalize()? == path,
            "MCP writable directory escaped its workspace: {}",
            path.display()
        );
    }
    Ok(path)
}
impl Drop for Process {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
#[cfg(unix)]
fn nonblocking<T: std::os::fd::AsRawFd>(pipe: &T) -> Result<()> {
    let fd = pipe.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    ensure!(
        flags >= 0 && unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } >= 0,
        "cannot make MCP pipe nonblocking"
    );
    Ok(())
}

struct HttpRequest {
    url: String,
    headers: BTreeMap<String, String>,
    method: &'static str,
    body: Option<String>,
    rpc_id: Option<u64>,
}
struct HttpResponse {
    body: Vec<u8>,
    content_type: String,
    session: Option<String>,
    rpc: Option<Value>,
}

fn http(request: HttpRequest, cancel: &AtomicBool, deadline: Instant) -> Result<HttpResponse> {
    validate_url(&request.url)?;
    check(cancel, deadline)?;
    let timeout = deadline
        .saturating_duration_since(Instant::now())
        .min(MAX_HTTP_TIME);
    let (send, recv) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("loom-extension-http".into())
        .spawn(move || {
            let result = http_blocking(request, timeout);
            let _ = send.send(result);
        })
        .context("start bounded HTTP worker")?;
    loop {
        check(cancel, deadline)?;
        match recv.recv_timeout(
            Duration::from_millis(10).min(deadline.saturating_duration_since(Instant::now())),
        ) {
            Ok(result) => return result,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => bail!("HTTP worker disconnected"),
        }
    }
}

fn http_blocking(request: HttpRequest, timeout: Duration) -> Result<HttpResponse> {
    let deadline = Instant::now() + timeout;
    // No ambient proxy credentials, cookies, OAuth, netrc, or cross-origin redirects.
    let agent = ureq::AgentBuilder::new()
        .try_proxy_from_env(false)
        .redirects(0)
        .timeout(timeout)
        .build();
    let mut req = agent
        .request(request.method, &request.url)
        .set("Accept", "application/json, text/html, text/plain")
        .set("User-Agent", "Loom-Core/0.1");
    for (key, value) in &request.headers {
        req = req.set(key, value);
    }
    let response = match request.body.as_deref() {
        Some(body) => req.send_string(body),
        None => req.call(),
    }
    .map_err(|e| match e {
        ureq::Error::Status(code, _) => anyhow!("HTTP status {code}"),
        ureq::Error::Transport(t) => anyhow!("HTTP transport failed: {}", t.kind()),
    })?;
    ensure!(
        (200..300).contains(&response.status()),
        "HTTP status {} (redirects are not followed)",
        response.status()
    );
    if let Some(length) = response
        .header("Content-Length")
        .and_then(|s| s.parse::<u64>().ok())
    {
        ensure!(
            length <= MAX_WIRE as u64,
            "HTTP response size limit exceeded"
        );
    }
    let content_type = response
        .header("Content-Type")
        .unwrap_or("")
        .to_ascii_lowercase();
    let session = response.header("Mcp-Session-Id").map(str::to_owned);
    let mut result = HttpResponse {
        body: Vec::new(),
        content_type: content_type.clone(),
        session,
        rpc: None,
    };
    if request.method == "POST" && request.rpc_id.is_none() {
        ensure!(
            response.status() == 202 || response.status() == 204,
            "MCP notification expected HTTP 202/204, got {}",
            response.status()
        );
        return Ok(result);
    }
    if let Some(id) = request.rpc_id {
        if content_type.starts_with("text/event-stream") {
            let mut reader = BufReader::new(response.into_reader().take((MAX_WIRE + 1) as u64));
            let mut data = String::new();
            let mut total = 0;
            loop {
                let mut line = String::new();
                let n = reader.read_line(&mut line).context("read MCP SSE")?;
                total += n;
                ensure!(total <= MAX_WIRE, "MCP SSE read limit exceeded");
                ensure!(n > 0, "MCP SSE closed before response");
                let line = line.trim_end_matches(['\r', '\n']);
                if line.is_empty() {
                    if !data.is_empty() {
                        let value: Value =
                            serde_json::from_str(&data).context("invalid MCP SSE JSON")?;
                        if let Some(value) = matching_response(value, id, |value| {
                            http_server_reply(
                                &agent,
                                &request,
                                result.session.as_deref(),
                                value,
                                deadline,
                            )
                        })? {
                            result.rpc = Some(value);
                            return Ok(result);
                        }
                        data.clear();
                    }
                } else if let Some(value) = line.strip_prefix("data:") {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(value.strip_prefix(' ').unwrap_or(value));
                }
            }
        }
        ensure!(
            content_type.starts_with("application/json"),
            "MCP HTTP response must be JSON or SSE"
        );
    }
    response
        .into_reader()
        .take((MAX_WIRE + 1) as u64)
        .read_to_end(&mut result.body)
        .context("read HTTP response")?;
    ensure!(
        result.body.len() <= MAX_WIRE,
        "HTTP response size limit exceeded"
    );
    if let Some(id) = request.rpc_id {
        let value =
            serde_json::from_slice(&result.body).context("invalid HTTP JSON-RPC response")?;
        result.rpc = matching_response(value, id, |value| {
            http_server_reply(&agent, &request, result.session.as_deref(), value, deadline)
        })?;
        ensure!(result.rpc.is_some(), "HTTP JSON-RPC response id mismatch");
    }
    Ok(result)
}

fn matching_response(
    value: Value,
    id: u64,
    mut reply: impl FnMut(&Value) -> Result<()>,
) -> Result<Option<Value>> {
    let values = match value {
        Value::Array(batch) => batch,
        value => vec![value],
    };
    let mut response = None;
    for value in values {
        ensure!(
            value.get("jsonrpc").and_then(Value::as_str) == Some("2.0"),
            "invalid JSON-RPC version"
        );
        if value.get("method").is_some() {
            if value.get("id").is_some() {
                reply(&value)?;
            }
        } else if value.get("id") == Some(&json!(id)) {
            ensure!(response.is_none(), "duplicate HTTP JSON-RPC response");
            response = Some(value);
        } else {
            bail!("unexpected HTTP JSON-RPC response id");
        }
    }
    Ok(response)
}
fn http_server_reply(
    agent: &ureq::Agent,
    request: &HttpRequest,
    session: Option<&str>,
    value: &Value,
    deadline: Instant,
) -> Result<()> {
    ensure!(
        Instant::now() < deadline,
        "HTTP MCP reply deadline exceeded"
    );
    let mut req = agent
        .post(&request.url)
        .timeout(deadline.saturating_duration_since(Instant::now()));
    for (key, value) in &request.headers {
        req = req.set(key, value);
    }
    if let Some(session) = session {
        req = req.set("Mcp-Session-Id", session);
    }
    let response = req
        .send_string(&server_request_reply(value, &value["id"]).to_string())
        .map_err(|_| anyhow!("HTTP MCP server-request reply failed"))?;
    ensure!(
        response.status() == 202 || response.status() == 204,
        "HTTP MCP reply expected 202/204, got {}",
        response.status()
    );
    Ok(())
}
fn server_request_reply(value: &Value, id: &Value) -> Value {
    if value.get("method").and_then(Value::as_str) == Some("ping") {
        json!({"jsonrpc":"2.0","id":id,"result":{}})
    } else {
        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Loom does not support this server-to-client method"}})
    }
}

fn fetch(args: &Value, cancel: &AtomicBool, deadline: Instant) -> Result<String> {
    let url = required_string(args, "url")?;
    let response = http(
        HttpRequest {
            url: url.into(),
            headers: BTreeMap::new(),
            method: "GET",
            body: None,
            rpc_id: None,
        },
        cancel,
        deadline,
    )?;
    ensure!(
        response.content_type.is_empty()
            || response.content_type.starts_with("text/")
            || response.content_type.starts_with("application/json")
            || response.content_type.starts_with("application/xhtml+xml"),
        "web_fetch only supports text/HTML/JSON, received {}",
        response.content_type
    );
    let text = String::from_utf8(response.body).context("web_fetch response is not UTF-8 text")?;
    let text = if response.content_type.contains("html") || text.trim_start().starts_with('<') {
        plain_html(&text)
    } else {
        text
    };
    Ok(bounded(&format!("URL: {url}\n\n{text}")))
}
fn tool_output(result: &Value) -> String {
    let mut output = String::new();
    if let Some(parts) = result.get("content").and_then(Value::as_array) {
        for part in parts {
            if !output.is_empty() {
                output.push('\n');
            }
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                output.push_str(text);
            } else if let Some(resource) = part.get("resource") {
                output.push_str(&resource.to_string());
            } else if part.get("type").and_then(Value::as_str) == Some("image")
                || part.get("type").and_then(Value::as_str) == Some("audio")
            {
                output.push_str(&format!(
                    "[{} content omitted; MIME: {}]",
                    part["type"],
                    part.get("mimeType")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                ));
            } else {
                output.push_str(&part.to_string());
            }
            if output.len() > MAX_OUTPUT {
                break;
            }
        }
    }
    if let Some(value) = result.get("structuredContent") {
        output.push_str(&format!("\n{value}"));
    }
    if output.is_empty() {
        output = result.to_string();
    }
    bounded(&output)
}

fn builtin_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec { name: "web_search".into(), description: "Search the configured SearxNG or Brave endpoint. Returns real result snippets with source URLs; requires [web_search] config.".into(), parameters: object_schema(json!({"query":{"type":"string"},"count":{"type":"integer","minimum":1,"maximum":20}}), &["query"]) },
        ToolSpec { name: "web_fetch".into(), description: "Fetch an HTTP(S) page as bounded text with URL attribution. HTML scripts/styles are omitted.".into(), parameters: object_schema(json!({"url":{"type":"string"}}), &["url"]) },
    ]
}
fn object_schema(properties: Value, required: &[&str]) -> Value {
    json!({"type":"object","properties":properties,"required":required})
}
fn required_string<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty() && s.len() <= 8192)
        .ok_or_else(|| anyhow!("{key} must be a nonempty string of at most 8192 bytes"))
}
fn check(cancel: &AtomicBool, deadline: Instant) -> Result<()> {
    ensure!(!cancel.load(Ordering::Relaxed), "extension call cancelled");
    ensure!(Instant::now() < deadline, "extension deadline exceeded");
    Ok(())
}
fn pause(cancel: &AtomicBool, deadline: Instant) -> Result<()> {
    check(cancel, deadline)?;
    thread::sleep(Duration::from_millis(5).min(deadline.saturating_duration_since(Instant::now())));
    Ok(())
}
fn sanitize(name: &str) -> String {
    let value: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if value.is_empty() {
        "tool".into()
    } else {
        value
    }
}
fn unique_name(full: &str, used: &mut BTreeSet<String>) -> String {
    let base = limited(full, 60);
    let mut value = base.clone();
    let mut suffix = 2;
    while !used.insert(value.clone()) {
        let tail = format!("_{suffix}");
        value = format!("{}{}", limited(&base, 64 - tail.len()), tail);
        suffix += 1;
    }
    value
}
fn limited(text: &str, max: usize) -> String {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].into()
}
fn bounded(text: &str) -> String {
    if text.len() <= MAX_OUTPUT {
        return text.into();
    }
    format!("{}\n[output truncated]", limited(text, MAX_OUTPUT - 20))
}
fn validate_url(url: &str) -> Result<()> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or_else(|| anyhow!("URL must use http:// or https://"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    ensure!(
        !authority.is_empty()
            && !authority.contains('@')
            && !url.chars().any(char::is_whitespace)
            && !url.chars().any(char::is_control)
            && !url.contains(['\\', '#']),
        "URL has invalid authority, embedded credentials, whitespace, or fragment"
    );
    Ok(())
}
fn validate_headers(headers: &BTreeMap<String, String>) -> Result<()> {
    let mut seen = BTreeSet::new();
    for (key, value) in headers {
        ensure!(
            !key.is_empty()
                && key
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
                && !value.contains(['\r', '\n', '\0']),
            "invalid configured HTTP header"
        );
        let lower = key.to_ascii_lowercase();
        ensure!(
            seen.insert(lower.clone()),
            "duplicate configured HTTP header {key}"
        );
        ensure!(
            !matches!(
                lower.as_str(),
                "host"
                    | "content-length"
                    | "transfer-encoding"
                    | "connection"
                    | "mcp-session-id"
                    | "mcp-protocol-version"
            ),
            "reserved HTTP header {key}"
        );
    }
    Ok(())
}
fn set_header(headers: &mut BTreeMap<String, String>, key: &str, value: &str) {
    headers.retain(|name, _| !name.eq_ignore_ascii_case(key));
    headers.insert(key.into(), value.into());
}
fn encode(text: &str) -> String {
    let mut result = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            result.push(byte as char);
        } else {
            result.push_str(&format!("%{byte:02X}"));
        }
    }
    result
}

// Small conservative HTML text reader, not a browser. Strip active/raw-text
// elements, decode common/numeric entities, and collapse markup whitespace.
fn plain_html(html: &str) -> String {
    let mut output = String::new();
    let lower = html.to_ascii_lowercase();
    let mut at = 0;
    while at < html.len() {
        if html[at..].starts_with("<!--") {
            at = html[at + 4..]
                .find("-->")
                .map(|n| at + 4 + n + 3)
                .unwrap_or(html.len());
        } else if html.as_bytes()[at] == b'<' {
            let Some(end) = tag_end(html, at) else {
                break;
            };
            let tag = lower[at + 1..end].trim();
            let name = tag
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim_end_matches('/');
            if matches!(name, "script" | "style" | "noscript" | "template") {
                let close = format!("</{name}");
                at = lower[end + 1..]
                    .find(&close)
                    .and_then(|n| tag_end(html, end + 1 + n))
                    .map(|n| n + 1)
                    .unwrap_or(html.len());
            } else {
                output.push(' ');
                at = end + 1;
            }
        } else if html.as_bytes()[at] == b'&' {
            if let Some(end) = html[at..].find(';').filter(|n| *n <= 16)
                && let Some(value) = entity(&html[at + 1..at + end])
            {
                output.push(value);
                at += end + 1;
                continue;
            }
            output.push('&');
            at += 1;
        } else {
            let c = html[at..].chars().next().unwrap();
            output.push(c);
            at += c.len_utf8();
        }
    }
    output.split_whitespace().collect::<Vec<_>>().join(" ")
}
fn tag_end(html: &str, at: usize) -> Option<usize> {
    let mut quote = None;
    for (offset, c) in html[at..].char_indices() {
        match (quote, c) {
            (Some(q), c) if q == c => quote = None,
            (None, '\'' | '"') => quote = Some(c),
            (None, '>') => return Some(at + offset),
            _ => {}
        }
    }
    None
}
fn entity(text: &str) -> Option<char> {
    match text {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" | "#39" => Some('\''),
        "nbsp" => Some(' '),
        _ => {
            let n = if let Some(hex) = text.strip_prefix("#x").or_else(|| text.strip_prefix("#X")) {
                u32::from_str_radix(hex, 16).ok()
            } else {
                text.strip_prefix('#').and_then(|n| n.parse().ok())
            };
            n.and_then(char::from_u32)
        }
    }
}
fn attribute(tag: &str, key: &str) -> Option<String> {
    let bytes = tag.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        while at < bytes.len() && (bytes[at].is_ascii_whitespace() || b"</>".contains(&bytes[at])) {
            at += 1;
        }
        let start = at;
        while at < bytes.len() && !bytes[at].is_ascii_whitespace() && !b"=/>".contains(&bytes[at]) {
            at += 1;
        }
        let name = &tag[start..at];
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        if at >= bytes.len() || bytes[at] != b'=' {
            if at == start {
                at += 1;
            }
            continue;
        }
        at += 1;
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        if at >= bytes.len() {
            break;
        }
        let quote = if matches!(bytes[at], b'\'' | b'"') {
            let q = bytes[at];
            at += 1;
            Some(q)
        } else {
            None
        };
        let start = at;
        while at < bytes.len()
            && match quote {
                Some(q) => bytes[at] != q,
                None => !bytes[at].is_ascii_whitespace() && bytes[at] != b'>',
            }
        {
            at += 1;
        }
        if name.eq_ignore_ascii_case(key) {
            return Some(plain_html(&tag[start..at]));
        }
        if quote.is_some() && at < bytes.len() {
            at += 1;
        }
    }
    None
}
fn html_search(html: &str) -> Result<Vec<(String, String, String)>> {
    // SearxNG result articles have a result class and an h3 link. Restrict to
    // those so navigation links can never masquerade as search results.
    let lower = html.to_ascii_lowercase();
    let mut results = Vec::new();
    let mut at = 0;
    while let Some(n) = lower[at..].find("<article") {
        let start = at + n;
        let end_tag =
            tag_end(html, start).ok_or_else(|| anyhow!("malformed search HTML article"))?;
        let end = lower[end_tag..]
            .find("</article>")
            .map(|n| end_tag + n)
            .ok_or_else(|| anyhow!("malformed search HTML result"))?;
        at = end + 10;
        if !attribute(&html[start..=end_tag], "class")
            .unwrap_or_default()
            .split_whitespace()
            .any(|c| c == "result")
        {
            continue;
        }
        let section = &html[end_tag + 1..end];
        let section_lower = section.to_ascii_lowercase();
        let Some(h3) = section_lower.find("<h3") else {
            continue;
        };
        let Some(h3_end) = section_lower[h3..].find("</h3>").map(|n| h3 + n) else {
            continue;
        };
        let Some(link) = section_lower[h3..h3_end].find("<a ").map(|n| h3 + n) else {
            continue;
        };
        let Some(link_end) = tag_end(section, link) else {
            continue;
        };
        let Some(url) =
            attribute(&section[link..=link_end], "href").filter(|u| validate_url(u).is_ok())
        else {
            continue;
        };
        results.push((
            plain_html(&section[h3..h3_end]),
            url,
            limited(&plain_html(&section[h3_end + 5..]), 2000),
        ));
    }
    ensure!(
        !results.is_empty() || lower.contains("no results") || lower.contains("no result"),
        "search HTML contained no recognizable SearxNG results (possible challenge/error page)"
    );
    Ok(results)
}
