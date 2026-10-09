//! Ollama's native chat API (`POST {base}/api/chat`).
//!
//! Ollama's OpenAI-compatible endpoint ignores the context size and serves every model
//! at its 4096-token default, which is too small for agent work. The native API takes
//! `options.num_ctx`, so Loom talks to Ollama directly.
use super::*;
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Mutex};

/// Context used when the model allows it; KV cache memory grows with it.
const DEFAULT_CONTEXT: u64 = 32_768;

pub struct OllamaProvider {
    endpoint: Endpoint,
    contexts: Mutex<HashMap<String, u64>>,
    reasoning: Mutex<HashMap<String, ReasoningProfile>>,
}

impl OllamaProvider {
    pub fn new(endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            contexts: Mutex::new(HashMap::new()),
            reasoning: Mutex::new(HashMap::new()),
        }
    }

    fn root(&self) -> String {
        self.endpoint
            .base_url
            .trim_end_matches('/')
            .trim_end_matches("/v1")
            .to_owned()
    }

    /// min(model's trained context, LOOM_OLLAMA_CTX or 32k).
    fn context_for(&self, model: &str) -> u64 {
        self.context_for_timeout(model, None)
    }

    fn context_for_timeout(&self, model: &str, timeout: Option<Duration>) -> u64 {
        if let Some(n) = self.contexts.lock().unwrap().get(model) {
            return *n;
        }
        let wanted = std::env::var("LOOM_OLLAMA_CTX")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_CONTEXT);
        let shown = post_json(
            &url(&self.root(), "api/show"),
            &auth_headers(&self.endpoint),
            &json!({"model":model}),
            Some(
                timeout
                    .unwrap_or(Duration::from_secs(10))
                    .min(Duration::from_secs(10)),
            ),
        )
        .ok()
        .map(|(v, _)| v);
        let reasoning = shown.as_ref().map(reasoning_from_show).unwrap_or_default();
        self.reasoning
            .lock()
            .unwrap()
            .insert(model.to_owned(), reasoning);
        let trained = shown.and_then(|v| {
            v["model_info"]
                .as_object()?
                .iter()
                .find(|(k, _)| k.ends_with(".context_length"))
                .and_then(|(_, n)| n.as_u64())
        });
        let n = trained.map_or(wanted, |t| t.min(wanted));
        self.contexts.lock().unwrap().insert(model.to_owned(), n);
        n
    }

    fn validate_effort(&self, request: &Request) -> Result<(), ProviderError> {
        if let Some(effort) = &request.reasoning_effort
            && let Some(profile) = self.reasoning.lock().unwrap().get(&request.model)
            && !profile.supported_efforts.is_empty()
            && !profile.supported_efforts.contains(effort)
        {
            return Err(ProviderError::new(
                ErrorKind::BadRequest,
                format!(
                    "unsupported reasoning effort {effort:?}; model reports {:?}",
                    profile.supported_efforts
                ),
            ));
        }
        Ok(())
    }
}

fn reasoning_from_show(shown: &Value) -> ReasoningProfile {
    let Some(values) = shown["thinking"]["values"].as_array() else {
        // Older Ollama releases expose boolean thinking through capabilities only.
        // Explicit value metadata still takes precedence for level-only models.
        if shown["capabilities"].as_array().is_some_and(|caps|
            caps.iter().any(|cap| cap == "thinking")) {
            return ReasoningProfile {
                supported_efforts: vec!["none".into(), "on".into()],
                default_effort: None,
            };
        }
        return ReasoningProfile::default();
    };
    if values.iter().all(|v| v == false) {
        return ReasoningProfile::default();
    }
    let effort = |v: &Value| match v {
        Value::String(s) => Some(s.clone()),
        Value::Bool(false) => Some("none".into()),
        Value::Bool(true) => Some("on".into()),
        _ => None,
    };
    ReasoningProfile {
        supported_efforts: values.iter().filter_map(effort).collect(),
        default_effort: effort(&shown["thinking"]["default"]),
    }
}

pub(crate) fn body(request: &Request, num_ctx: u64) -> Value {
    let mut messages = Vec::new();
    if let Some(system) = &request.system {
        messages.push(json!({"role": "system", "content": system}));
    }
    for message in &request.messages {
        match message.role {
            Role::User => {
                let text = message.text();
                let images: Vec<&str> = message
                    .parts
                    .iter()
                    .filter_map(|p| match p {
                        Part::Image { data, .. } => Some(data.as_str()),
                        _ => None,
                    })
                    .collect();
                if !text.is_empty() || !images.is_empty() {
                    let mut item = json!({"role":"user", "content":text});
                    if !images.is_empty() {
                        item["images"] = json!(images);
                    }
                    messages.push(item);
                }
                for part in &message.parts {
                    if let Part::ToolResult { output, .. } = part {
                        messages.push(json!({"role": "tool", "content": output}));
                    }
                }
            }
            Role::Assistant => {
                let calls: Vec<Value> = message
                    .parts
                    .iter()
                    .filter_map(|p| match p {
                        Part::ToolCall {
                            name, arguments, ..
                        } => Some(json!({"function": {"name": name, "arguments": arguments}})),
                        _ => None,
                    })
                    .collect();
                let mut item = json!({"role": "assistant", "content": message.text()});
                if !calls.is_empty() {
                    item["tool_calls"] = Value::Array(calls);
                }
                messages.push(item);
            }
        }
    }
    let mut options = json!({"num_ctx": num_ctx});
    if let Some(max) = request.max_output_tokens {
        options["num_predict"] = json!(max);
    }
    if let Some(t) = request.temperature {
        options["temperature"] = json!(t);
    }
    let mut body =
        json!({"model": request.model, "messages": messages, "stream": false, "options": options});
    if !request.tools.is_empty() {
        body["tools"] = request
            .tools
            .iter()
            .map(|t| json!({"type": "function", "function": {"name": t.name, "description": t.description, "parameters": t.parameters}}))
            .collect();
    }
    if let Some(effort) = &request.reasoning_effort {
        body["think"] = match effort.as_str() {
            "none" => json!(false),
            "on" => json!(true),
            // Thinking level names are model-defined; let the server validate
            // them instead of silently changing the requested effort.
            _ => json!(effort),
        };
    }
    body
}

pub(crate) fn parse(
    value: &Value,
    latency: Duration,
    requested: &str,
) -> Result<Response, ProviderError> {
    if let Some(error) = value["error"].as_str() {
        return Err(ProviderError::new(ErrorKind::BadRequest, error));
    }
    let message = &value["message"];
    let mut parts = Vec::new();
    if let Some(text) = message["content"].as_str().filter(|t| !t.is_empty()) {
        parts.push(Part::Text(text.to_owned()));
    }
    for (i, call) in message["tool_calls"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        parts.push(Part::ToolCall {
            // Ollama does not number calls; results are matched by order.
            id: call["id"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| format!("call_{i}")),
            name: call["function"]["name"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            arguments: parse_arguments(&call["function"]["arguments"]),
        });
    }
    let stop = if parts.iter().any(|p| matches!(p, Part::ToolCall { .. })) {
        Stop::ToolUse
    } else if value["done_reason"] == "length" {
        Stop::MaxTokens
    } else {
        Stop::EndTurn
    };
    Ok(Response {
        parts,
        usage: Usage {
            input: value["prompt_eval_count"].as_u64(),
            // Ollama reports only newly evaluated prompt tokens; cache hits are not exposed.
            cached_input: None,
            output: value["eval_count"].as_u64(),
        },
        stop,
        model: value["model"].as_str().unwrap_or(requested).to_owned(),
        latency,
        route: value["_loom_route"].as_str().map(str::to_owned),
        gateway_connection: None,
        gateway_upstream_ms: None,
    })
}

impl Provider for OllamaProvider {
    fn name(&self) -> &str {
        &self.endpoint.name
    }

    fn complete(&self, request: &Request) -> Result<Response, ProviderError> {
        request.check_cancelled()?;
        let num_ctx = self.context_for_timeout(&request.model, request.timeout);
        request.check_cancelled()?;
        self.validate_effort(request)?;
        let (value, latency) = super::post_json_cancellable(
            request,
            &url(&self.root(), "api/chat"),
            &self.endpoint.headers,
            &body(request, num_ctx),
        )?;
        parse(&value, latency, &request.model)
    }

    fn complete_stream(
        &self,
        request: &Request,
        observe: &mut dyn FnMut(StreamEvent),
    ) -> Result<Response, ProviderError> {
        request.check_cancelled()?;
        let num_ctx = self.context_for_timeout(&request.model, request.timeout);
        request.check_cancelled()?;
        self.validate_effort(request)?;
        stream::ollama(
            &url(&self.root(), "api/chat"),
            &self.endpoint.headers,
            request,
            body(request, num_ctx),
            observe,
        )
    }

    fn list_models(&self) -> Result<Vec<String>, ProviderError> {
        let value = get_json(&url(&self.root(), "api/tags"), &self.endpoint.headers)?;
        Ok(value["models"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m["name"].as_str().map(str::to_owned))
            .collect())
    }

    fn reasoning_profile(&self, model: &str) -> ReasoningProfile {
        self.context_for(model);
        self.reasoning
            .lock()
            .unwrap()
            .get(model)
            .cloned()
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_profile_comes_from_show_metadata() {
        let (base, sent) = mock::serve(
            200,
            r#"{"thinking":{"values":[false,"low","medium","xhigh"],"default":"medium"}}"#,
        );
        let provider = OllamaProvider::new(Endpoint {
            name: "ollama".into(),
            protocol: Protocol::Ollama,
            base_url: base,
            api_key: None,
            headers: vec![],
        });
        let profile = provider.reasoning_profile("m");
        assert_eq!(sent.recv().unwrap().path, "/api/show");
        assert_eq!(
            profile.supported_efforts,
            ["none", "low", "medium", "xhigh"]
        );
        assert_eq!(profile.default_effort.as_deref(), Some("medium"));
        assert_eq!(
            provider.reasoning_profile("m").supported_efforts,
            profile.supported_efforts,
            "cached without another HTTP request"
        );
        let request = Request {
            model: "m".into(),
            reasoning_effort: Some("high".into()),
            ..Request::default()
        };
        assert_eq!(
            provider
                .complete_stream(&request, &mut |_| panic!("unsupported effort output"))
                .unwrap_err()
                .kind,
            ErrorKind::BadRequest
        );
        assert!(
            reasoning_from_show(&json!({"thinking":{"values":[false],"default":false}}))
                .supported_efforts
                .is_empty()
        );
        assert_eq!(reasoning_from_show(&json!({"capabilities":["thinking"]})).supported_efforts,
            ["none", "on"]);
    }

    #[test]
    fn legacy_thinking_capability_exposes_boolean_controls() {
        let profile = reasoning_from_show(&json!({"capabilities": ["completion", "thinking"]}));
        assert_eq!(profile.supported_efforts, ["none", "on"]);
        assert!(profile.default_effort.is_none());
    }
    #[test]
    fn native_body_carries_context_and_tool_history() {
        let request = Request {
            model: "m".into(),
            system: Some("s".into()),
            messages: vec![
                Message::user("go"),
                Message {
                    role: Role::Assistant,
                    parts: vec![Part::ToolCall {
                        id: "call_0".into(),
                        name: "read".into(),
                        arguments: json!({"path": "a"}),
                    }],
                },
                Message {
                    role: Role::User,
                    parts: vec![Part::ToolResult {
                        call_id: "call_0".into(),
                        output: "x".into(),
                        is_error: false,
                    }],
                },
            ],
            max_output_tokens: Some(64),
            ..Request::default()
        };
        let b = body(&request, 32768);
        assert_eq!(b["options"]["num_ctx"], 32768);
        assert_eq!(b["options"]["num_predict"], 64);
        assert_eq!(
            b["messages"][2]["tool_calls"][0]["function"]["arguments"]["path"],
            "a"
        );
        assert_eq!(b["messages"][3]["role"], "tool");
    }

    #[test]
    fn parses_tool_calls_without_ids() {
        let r = parse(
            &json!({"model": "m", "message": {"content": "", "tool_calls": [{"function": {"name": "find", "arguments": {"pattern": "x"}}}]}, "done_reason": "stop", "prompt_eval_count": 900, "eval_count": 12}),
            Duration::ZERO,
            "m",
        )
        .unwrap();
        assert_eq!(r.stop, Stop::ToolUse);
        assert_eq!(
            r.tool_calls().next().unwrap(),
            ("call_0", "find", &json!({"pattern": "x"}))
        );
        assert_eq!(r.usage.input, Some(900));
    }
}
