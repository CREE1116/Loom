//! Model provider layer: one request/response shape over every wire protocol.
//!
//! Three protocols cover the providers Loom uses:
//! - Chat Completions: Ollama, llama.cpp, LM Studio, OpenRouter, Gemini (OpenAI-compatible
//!   endpoint), Groq and any OpenAI-compatible server
//! - Responses: OpenAI, and gateways such as opencodex
//! - Anthropic Messages
//!
//! Usage is normalized so numbers compare across providers: `input` counts every input
//! token including cache hits, `cached_input` is the cached part of it. Missing numbers
//! stay `None`; they are never reported as zero.
use serde_json::Value;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

pub mod anthropic;
pub mod chat;
pub mod chatgpt;
pub mod cli;
pub mod discover;
pub mod freetier;
mod image;
mod http;
pub mod local;
pub mod import;
pub mod model_package;
pub mod lmx;
pub mod lmx_ir;
pub mod lmx_native;
pub mod lmx_native_file;
pub mod lmx_native_import;
pub mod lmx_native_gguf;
pub mod lmx_expert_cache;
pub mod lmx_engine;
pub mod lmx_native_runtime;
pub mod gguf_mlx;
pub mod lmx_runtime;
pub mod native_runtime;
#[cfg(loom_mlx)]
pub mod mlx_runtime;
pub mod checkpoints;
pub mod ollama;
pub mod responses;
pub mod router;
pub mod registry;
pub mod provider_cli;
pub mod runtime;
pub mod scripted;
mod stream;

pub use image::image_part;
pub use router::{CostClass, Need, Served, Tier};
pub use scripted::ScriptedModel;

/// Model caches and usage records can be isolated without replacing HOME or credentials.
pub(crate) fn state_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("LOOM_MODEL_STATE_DIR").filter(|path| !path.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(|home| std::path::PathBuf::from(home).join(".loom")))
}

/// Default cost limit before the user explicitly registers a connection or changes policy.
pub const COST_CEILING: CostClass = CostClass::Subscription;

/// Registering a direct connection explicitly permits its configured cost class.
/// With no registry, preserve the original free/subscription-only worker policy.
pub fn cost_ceiling() -> CostClass {
    registry::Registry::open().and_then(|registry| registry.config())
        .map(|config| config.max_cost).unwrap_or(COST_CEILING)
}

/// L4's only model entry point. Diagnostic CLI routing remains outside Worker access.
pub trait ModelPort: Send + Sync {
    fn complete(&self, need: Need, request: &Request) -> Result<Served, ProviderError>;

    /// Legacy ports complete normally and emit no synthetic progress.
    fn complete_stream(
        &self,
        need: Need,
        request: &Request,
        _observe: &mut dyn FnMut(StreamEvent),
    ) -> Result<Served, ProviderError> {
        request.check_cancelled()?;
        let served = self.complete(need, request)?;
        request.check_cancelled()?;
        Ok(served)
    }

    fn model_profiles(&self) -> Vec<ModelProfile> {
        Vec::new()
    }
    fn limits(&self) -> Vec<ModelLimits> {
        Vec::new()
    }
    /// Recorded calls, including the loaded ledger when supported.
    fn records(&self) -> Vec<router::CallRecord> {
        Vec::new()
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct ModelProfile {
    pub candidate: router::Candidate,
    pub unavailable_reason: Option<String>,
    /// Empty means that the provider has not reported supported reasoning controls.
    pub supported_efforts: Vec<String>,
    pub default_effort: Option<String>,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct ReasoningProfile {
    pub supported_efforts: Vec<String>,
    pub default_effort: Option<String>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct ModelLimits {
    pub endpoint: String,
    pub model: String,
    pub context: Option<u64>,
    pub tools: bool,
    pub disabled: Option<String>,
    pub cooldown_until: Option<std::time::SystemTime>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum StreamEvent {
    TextDelta(String),
    Usage(Usage),
    /// Local engines: tokens generated so far and the time spent generating them.
    Speed { tokens: u64, seconds: f64 },
}

impl ModelPort for router::Router {
    fn complete(&self, mut need: Need, request: &Request) -> Result<Served, ProviderError> {
        need.max_cost = need.max_cost.min(cost_ceiling());
        router::Router::complete(self, need, request)
    }
    fn complete_stream(
        &self,
        mut need: Need,
        request: &Request,
        observe: &mut dyn FnMut(StreamEvent),
    ) -> Result<Served, ProviderError> {
        need.max_cost = need.max_cost.min(cost_ceiling());
        router::Router::complete_stream(self, need, request, observe)
    }
    fn model_profiles(&self) -> Vec<ModelProfile> {
        router::Router::model_profiles(self)
    }
    fn limits(&self) -> Vec<ModelLimits> {
        router::Router::limits(self)
    }
    fn records(&self) -> Vec<router::CallRecord> {
        self.ledger()
    }
}

/// A general chat model, not a safety classifier, speech, image or embedding model a
/// gateway may also route to; only these are offered or kept for a coding worker.
pub fn is_chat_model(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    !["guard", "tts", "image", "embed", "whisper", "transcribe", "moderation", "veo"]
        .iter()
        .any(|word| id.contains(word))
}

/// Subscription OAuth routes must not be consumed through third-party gateways.
pub fn blocked_route(route: &str) -> bool {
    let key = route.trim().to_ascii_lowercase().replace(['_', '-'], "");
    // A subscription route the user explicitly accepted (its account risk included):
    // LOOM_ALLOW_ROUTES="antigravity,agy" or `allow = [...]` in ~/.loom/routes.toml.
    if allowed_routes().contains(&key) {
        return false;
    }
    blocked_by_default(&key)
}

/// The built-in block list, before any user allowance (see `blocked_route`).
pub fn blocked_by_default(route: &str) -> bool {
    let key = route.trim().to_ascii_lowercase().replace(['_', '-'], "");
    matches!(
        key.as_str(),
        // Claude, Codex/ChatGPT, Google Antigravity and Grok subscription logins, under
        // the provider ids and aliases gateways use for them
        "cc" | "claude" | "claudecode" | "antigravity" | "ag" | "agy" | "codex" | "codexoauth"
            | "chatgptoauth" | "gc" | "grokcli" | "grokweb" | "xaioauth" | "xao"
    )
}

fn allowed_routes() -> &'static [String] {
    static ALLOWED: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    ALLOWED.get_or_init(|| {
        let normalize = |s: &str| s.trim().to_ascii_lowercase().replace(['_', '-'], "");
        let mut out: Vec<String> = std::env::var("LOOM_ALLOW_ROUTES")
            .unwrap_or_default()
            .split(',')
            .map(normalize)
            .filter(|s| !s.is_empty())
            .collect();
        if let Some(home) = std::env::var_os("HOME")
            && let Ok(text) = std::fs::read_to_string(std::path::Path::new(&home).join(".loom/routes.toml"))
            && let Ok(table) = toml::from_str::<toml::Table>(&text)
        {
            out.extend(
                table.get("allow").and_then(|v| v.as_array()).into_iter().flatten()
                    .filter_map(|v| v.as_str()).map(normalize),
            );
        }
        out
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Part {
    Text(String),
    /// Raw base64 bytes, without a data URL prefix.
    Image {
        mime_type: String,
        data: String,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
    },
    ToolResult {
        call_id: String,
        output: String,
        is_error: bool,
    },
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Message {
    pub role: Role,
    pub parts: Vec<Part>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            parts: vec![Part::Text(text.into())],
        }
    }
    pub fn text(&self) -> String {
        self.parts
            .iter()
            .filter_map(|p| match p {
                Part::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema of the arguments object.
    pub parameters: Value,
}

#[derive(Clone, Debug, Default)]
pub struct Request {
    /// Empty selects automatically; otherwise selects this model (or endpoint/model).
    pub model: String,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub max_output_tokens: Option<u32>,
    /// Sampling temperature; `None` keeps the provider default.
    pub temperature: Option<f32>,
    pub reasoning_effort: Option<String>,
    /// Interrupts model HTTP, including idle headers and incomplete stream frames.
    pub cancel: Option<Arc<AtomicBool>>,
    pub timeout: Option<Duration>,
    /// A gateway connection confirmed by a previous response in this worker attempt.
    pub gateway_connection: Option<String>,
}

impl Request {
    pub(crate) fn check_cancelled(&self) -> Result<(), ProviderError> {
        if self
            .cancel
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire))
        {
            Err(ProviderError::new(
                ErrorKind::Cancelled,
                "model request cancelled",
            ))
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Usage {
    pub input: Option<u64>,
    pub cached_input: Option<u64>,
    pub output: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Stop {
    EndTurn,
    ToolUse,
    MaxTokens,
    Other(String),
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Response {
    pub parts: Vec<Part>,
    pub usage: Usage,
    pub stop: Stop,
    /// Model that actually answered, as the provider reports it.
    pub model: String,
    pub latency: Duration,
    /// Upstream provider when a gateway reports which one answered.
    pub route: Option<String>,
    #[serde(default)]
    pub gateway_connection: Option<String>,
    /// Upstream model latency reported by the gateway, when available.
    #[serde(default)]
    pub gateway_upstream_ms: Option<u64>,
}

impl Response {
    pub fn text(&self) -> String {
        Message {
            role: Role::Assistant,
            parts: self.parts.clone(),
        }
        .text()
    }
    pub fn tool_calls(&self) -> impl Iterator<Item = (&str, &str, &Value)> {
        self.parts.iter().filter_map(|p| match p {
            Part::ToolCall {
                id,
                name,
                arguments,
            } => Some((id.as_str(), name.as_str(), arguments)),
            _ => None,
        })
    }
}

/// What went wrong, in terms the router can act on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// Caller cancellation; never retry or penalize endpoint health.
    Cancelled,
    /// A caller/provider deadline elapsed. Retry is allowed while task time remains.
    TimedOut,
    /// Bad or missing credentials. Do not retry this endpoint.
    Auth,
    /// Allowance used up (daily/monthly quota, no credits). Retry much later.
    Quota,
    /// Too many requests right now. Retry after a short cooldown.
    RateLimited,
    /// Network failure, timeout or server error. Retry or try another endpoint.
    Transient,
    /// The request itself is wrong for this endpoint (unknown model, bad schema).
    BadRequest,
}

/// Local capacity/deadline admission failures describe contention, not provider
/// health or missing tool support. Retain the existing external error kinds.
pub(super) const LOCAL_RESOURCE_ADMISSION: &str = "local_resource_admission";

#[derive(Clone, Debug)]
pub struct ProviderError {
    pub kind: ErrorKind,
    pub message: String,
    pub retry_after: Option<Duration>,
    /// Provider machine code, retained before truncating the display message.
    pub code: Option<String>,
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}
impl std::error::Error for ProviderError {}

impl ProviderError {
    /// A generated call was rejected before execution, not missing tool support.
    pub(crate) fn is_tool_generation_rejected(&self) -> bool {
        self.kind == ErrorKind::BadRequest
            && (self.code.as_deref()==Some("tool_use_failed") || self.message.find('{')
                .and_then(|offset| serde_json::from_str::<serde_json::Value>(&self.message[offset..]).ok())
                .is_some_and(|body| body["error"]["code"] == "tool_use_failed"))
    }

    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            retry_after: None,
            code: None,
        }
    }
}

pub trait Provider: Send + Sync {
    /// Endpoint name, e.g. `ollama` or `openrouter`.
    fn name(&self) -> &str;
    fn complete(&self, request: &Request) -> Result<Response, ProviderError>;
    /// A gateway that must say which upstream answered (`Response::route`). A reply
    /// without a route cannot be checked against `blocked_route` and is refused.
    fn reports_route(&self) -> bool {
        false
    }
    fn complete_stream(
        &self,
        request: &Request,
        _observe: &mut dyn FnMut(StreamEvent),
    ) -> Result<Response, ProviderError> {
        request.check_cancelled()?;
        let response = self.complete(request)?;
        request.check_cancelled()?;
        Ok(response)
    }
    fn list_models(&self) -> Result<Vec<String>, ProviderError>;
    /// Model-reported or explicitly configured values only; no model-name guesses.
    fn reasoning_profile(&self, _model: &str) -> ReasoningProfile {
        ReasoningProfile::default()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    Chat,
    /// Ollama's native API (context size control).
    Ollama,
    Responses,
    Anthropic,
}

/// Where and how to reach one model endpoint.
#[derive(Clone)]
pub struct Endpoint {
    pub name: String,
    pub protocol: Protocol,
    /// Base URL including the API version path, e.g. `http://127.0.0.1:11434/v1`.
    pub base_url: String,
    pub api_key: Option<String>,
    pub headers: Vec<(String, String)>,
}

impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint").field("name", &self.name).field("protocol", &self.protocol)
            .field("base_url", &self.base_url).field("api_key", &self.api_key.as_ref().map(|_| "[redacted]"))
            .field("header_names", &self.headers.iter().map(|(name,_)| name).collect::<Vec<_>>()).finish()
    }
}

pub fn connect(endpoint: Endpoint) -> Box<dyn Provider> {
    match endpoint.protocol {
        Protocol::Chat => Box::new(chat::ChatProvider::new(endpoint)),
        Protocol::Ollama => Box::new(ollama::OllamaProvider::new(endpoint)),
        Protocol::Responses => Box::new(responses::ResponsesProvider::new(endpoint)),
        Protocol::Anthropic => Box::new(anthropic::AnthropicProvider::new(endpoint)),
    }
}

// ------------------------------------------------------------------ HTTP

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

pub(crate) fn url(base: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

pub(crate) fn post_json(
    url: &str,
    headers: &[(String, String)],
    body: &Value,
    timeout: Option<Duration>,
) -> Result<(Value, Duration), ProviderError> {
    let started = Instant::now();
    let mut request = ureq::post(url).timeout(timeout.unwrap_or(DEFAULT_TIMEOUT));
    for (name, value) in headers {
        request = request.set(name, value);
    }
    match request.send_json(body) {
        Ok(response) => {
            let value = response.into_json::<Value>().map_err(|e| {
                ProviderError::new(ErrorKind::Transient, format!("unreadable response: {e}"))
            })?;
            Ok((value, started.elapsed()))
        }
        Err(error) => Err(classify(error)),
    }
}

/// Interruptible HTTP; cancellation drops the connection instead of detaching it.
pub(crate) fn post_json_cancellable(
    request: &Request,
    url: &str,
    headers: &[(String, String)],
    body: &Value,
) -> Result<(Value, Duration), ProviderError> {
    http::json(request, url, headers, body)
}

pub(crate) fn get_json(url: &str, headers: &[(String, String)]) -> Result<Value, ProviderError> {
    let mut request = ureq::get(url).timeout(Duration::from_secs(10));
    for (name, value) in headers {
        request = request.set(name, value);
    }
    match request.call() {
        Ok(response) => response.into_json::<Value>().map_err(|e| {
            ProviderError::new(ErrorKind::Transient, format!("unreadable response: {e}"))
        }),
        Err(error) => Err(classify(error)),
    }
}

fn classify(error: ureq::Error) -> ProviderError {
    match error {
        ureq::Error::Status(code, response) => {
            let retry_after = response
                .header("retry-after")
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(Duration::from_secs);
            let body = response.into_string().unwrap_or_default();
            let kind = status_kind(code, &body);
            let provider_code=serde_json::from_str::<Value>(&body).ok()
                .and_then(|value|value["error"]["code"].as_str().map(ToOwned::to_owned));
            ProviderError {
                kind,
                message: format!(
                    "HTTP {code}: {}",
                    body.chars().take(500).collect::<String>()
                ),
                retry_after,
                code: provider_code,
            }
        }
        ureq::Error::Transport(transport) => {
            ProviderError::new(ErrorKind::Transient, transport.to_string())
        }
    }
}

pub(crate) fn status_kind(code: u16, body: &str) -> ErrorKind {
    let lower = body.to_ascii_lowercase();
    let quota = [
        "quota",
        "insufficient",
        "credit",
        "billing",
        "exceeded your",
        "limit reached",
    ]
    .iter()
    .any(|k| lower.contains(k));
    match code {
        401 | 403 => ErrorKind::Auth,
        402 => ErrorKind::Quota,
        // Providers often append a generic "rate limits" documentation URL
        // even when the account has exhausted its billing quota.
        429 if quota && (!lower.contains("rate")
            || lower.contains("billing")
            || lower.contains("current quota")) => ErrorKind::Quota,
        429 => ErrorKind::RateLimited,
        408 | 409 | 425 | 500..=599 => ErrorKind::Transient,
        _ => ErrorKind::BadRequest,
    }
}

/// Tool-call arguments arrive as a JSON string in OpenAI protocols; keep raw text if invalid.
pub(crate) fn parse_arguments(raw: &Value) -> Value {
    match raw {
        Value::String(text) => {
            serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.clone()))
        }
        other => other.clone(),
    }
}

pub(crate) fn arguments_string(arguments: &Value) -> String {
    match arguments {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

pub(crate) fn auth_headers(endpoint: &Endpoint) -> Vec<(String, String)> {
    let mut headers = endpoint.headers.clone();
    if let Some(key) = &endpoint.api_key {
        headers.push(("Authorization".into(), format!("Bearer {key}")));
    }
    headers
}

#[cfg(test)]
pub(crate) mod mock {
    //! One-shot HTTP server: records the request, answers with a canned response.
    use std::{
        io::{BufRead, BufReader, Read, Write},
        net::TcpListener,
        sync::mpsc,
        thread,
    };

    pub struct Recorded {
        pub path: String,
        pub headers: Vec<(String, String)>,
        pub body: serde_json::Value,
    }

    pub fn serve(status: u16, body: &str) -> (String, mpsc::Receiver<Recorded>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::channel();
        let body = body.to_owned();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let path = line.split_whitespace().nth(1).unwrap_or("").to_owned();
            let mut headers = Vec::new();
            let mut length = 0;
            loop {
                let mut h = String::new();
                reader.read_line(&mut h).unwrap();
                let h = h.trim_end();
                if h.is_empty() {
                    break;
                }
                if let Some((k, v)) = h.split_once(':') {
                    if k.eq_ignore_ascii_case("content-length") {
                        length = v.trim().parse().unwrap_or(0);
                    }
                    headers.push((k.trim().to_ascii_lowercase(), v.trim().to_owned()));
                }
            }
            let mut buf = vec![0; length];
            reader.read_exact(&mut buf).unwrap();
            let request = serde_json::from_slice(&buf).unwrap_or(serde_json::Value::Null);
            let _ = tx.send(Recorded {
                path,
                headers,
                body: request,
            });
            let reason = if status == 200 { "OK" } else { "ERR" };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nretry-after: 7\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        (base, rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_map_to_actions_the_router_can_take() {
        assert_eq!(status_kind(401, ""), ErrorKind::Auth);
        assert_eq!(
            status_kind(429, "Rate limit reached for requests"),
            ErrorKind::RateLimited
        );
        assert_eq!(
            status_kind(429, "You exceeded your current quota"),
            ErrorKind::Quota
        );
        assert_eq!(
            status_kind(429, "You exceeded your current quota, please check your plan and billing details. See our rate limits documentation."),
            ErrorKind::Quota,
            "a documentation link about rate limits must not turn exhausted credits into transient throttling"
        );
        assert_eq!(status_kind(402, ""), ErrorKind::Quota);
        assert_eq!(status_kind(503, ""), ErrorKind::Transient);
        assert_eq!(status_kind(400, "unknown model"), ErrorKind::BadRequest);
    }

    #[test]
    fn rate_limit_carries_retry_after() {
        let (base, _rx) = mock::serve(429, r#"{"error":"slow down, rate limited"}"#);
        let error = post_json(&url(&base, "x"), &[], &serde_json::json!({}), None).unwrap_err();
        assert_eq!(error.kind, ErrorKind::RateLimited);
        assert_eq!(error.retry_after, Some(Duration::from_secs(7)));
    }
    #[test]
    fn long_generation_error_keeps_its_code_when_the_display_excerpt_is_truncated() {
        let body=serde_json::json!({"error":{"message":"generated call rejected","code":"tool_use_failed","failed_generation":"x".repeat(2000)}}).to_string();
        let (base,_rx)=mock::serve(400,&body);
        let error=post_json(&url(&base,"x"),&[],&serde_json::json!({}),None).unwrap_err();
        assert!(error.message.len()<600,"keep the human-readable excerpt bounded");
        assert!(error.is_tool_generation_rejected(),"truncation discarded the machine-readable cause: {error}");
    }
}
