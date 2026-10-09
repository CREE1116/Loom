//! Anthropic Messages protocol (`POST {base}/messages`), API-key authentication.
use super::*;
use serde_json::{Value, json};

const VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u32 = 8192;

pub struct AnthropicProvider {
    endpoint: Endpoint,
}

impl AnthropicProvider {
    pub fn new(endpoint: Endpoint) -> Self {
        Self { endpoint }
    }

    fn headers(&self) -> Vec<(String, String)> {
        let mut headers = self.endpoint.headers.clone();
        headers.push(("anthropic-version".into(), VERSION.into()));
        if let Some(key) = &self.endpoint.api_key {
            headers.push(("x-api-key".into(), key.clone()));
        }
        headers
    }
}

pub(crate) fn body(request: &Request) -> Value {
    let messages: Vec<Value> = request
        .messages
        .iter()
        .map(|message| {
            let content: Vec<Value> = message
                .parts
                .iter()
                .map(|part| match part {
                    Part::Text(text) => json!({"type": "text", "text": text}),
                    Part::Image { mime_type, data } => json!({"type":"image", "source":{"type":"base64","media_type":mime_type,"data":data}}),
                    Part::ToolCall { id, name, arguments } => json!({
                        "type": "tool_use", "id": id, "name": name,
                        "input": if arguments.is_object() { arguments.clone() } else { json!({}) },
                    }),
                    Part::ToolResult { call_id, output, is_error } => json!({
                        "type": "tool_result", "tool_use_id": call_id, "content": output, "is_error": is_error,
                    }),
                })
                .collect();
            json!({"role": if message.role == Role::User { "user" } else { "assistant" }, "content": content})
        })
        .collect();
    let mut body = json!({
        "model": request.model,
        "max_tokens": request.max_output_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        "messages": messages,
    });
    if let Some(system) = &request.system {
        body["system"] = json!(system);
    }
    if let Some(t) = request.temperature {
        body["temperature"] = json!(t);
    }
    if let Some(effort) = &request.reasoning_effort {
        body["output_config"] = json!({"effort":effort});
    }
    if !request.tools.is_empty() {
        body["tools"] = request
            .tools
            .iter()
            .map(|t| json!({"name": t.name, "description": t.description, "input_schema": t.parameters}))
            .collect();
    }
    body
}

pub(crate) fn parse(
    value: &Value,
    latency: Duration,
    requested: &str,
) -> Result<Response, ProviderError> {
    if value["type"] == "error" {
        let kind = match value["error"]["type"].as_str() {
            Some("rate_limit_error") => ErrorKind::RateLimited,
            Some("overloaded_error") | Some("api_error") => ErrorKind::Transient,
            Some("authentication_error") | Some("permission_error") => ErrorKind::Auth,
            Some("billing_error") => ErrorKind::Quota,
            _ => ErrorKind::BadRequest,
        };
        return Err(ProviderError::new(
            kind,
            value["error"]["message"].as_str().unwrap_or_default(),
        ));
    }
    let mut parts = Vec::new();
    for block in value["content"].as_array().into_iter().flatten() {
        match block["type"].as_str() {
            Some("text") => {
                if let Some(text) = block["text"].as_str().filter(|t| !t.is_empty()) {
                    parts.push(Part::Text(text.to_owned()));
                }
            }
            Some("tool_use") => parts.push(Part::ToolCall {
                id: block["id"].as_str().unwrap_or_default().to_owned(),
                name: block["name"].as_str().unwrap_or_default().to_owned(),
                arguments: block["input"].clone(),
            }),
            _ => {}
        }
    }
    let stop = match value["stop_reason"].as_str() {
        Some("tool_use") => Stop::ToolUse,
        Some("max_tokens") => Stop::MaxTokens,
        Some("end_turn") | Some("stop_sequence") | None => Stop::EndTurn,
        Some(other) => Stop::Other(other.to_owned()),
    };
    // Anthropic counts cache reads and writes outside `input_tokens`; fold them in so
    // `input` means "all input tokens" as for the other protocols.
    let usage = &value["usage"];
    let fresh = usage["input_tokens"].as_u64();
    let read = usage["cache_read_input_tokens"].as_u64().unwrap_or(0);
    let written = usage["cache_creation_input_tokens"].as_u64().unwrap_or(0);
    Ok(Response {
        parts,
        usage: Usage {
            input: fresh.map(|n| n + read + written),
            cached_input: usage["cache_read_input_tokens"].as_u64(),
            output: usage["output_tokens"].as_u64(),
        },
        stop,
        model: value["model"].as_str().unwrap_or(requested).to_owned(),
        latency,
        route: value["_loom_route"].as_str().map(str::to_owned),
        gateway_connection: None,
        gateway_upstream_ms: None,
    })
}

impl Provider for AnthropicProvider {
    fn name(&self) -> &str {
        &self.endpoint.name
    }

    fn complete(&self, request: &Request) -> Result<Response, ProviderError> {
        let (value, latency) = super::post_json_cancellable(
            request,
            &url(&self.endpoint.base_url, "messages"),
            &self.headers(),
            &body(request),
        )?;
        parse(&value, latency, &request.model)
    }

    fn complete_stream(
        &self,
        request: &Request,
        observe: &mut dyn FnMut(StreamEvent),
    ) -> Result<Response, ProviderError> {
        stream::anthropic(
            &url(&self.endpoint.base_url, "messages"),
            &self.headers(),
            request,
            body(request),
            observe,
        )
    }

    fn list_models(&self) -> Result<Vec<String>, ProviderError> {
        let value = get_json(&url(&self.endpoint.base_url, "models"), &self.headers())?;
        Ok(value["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m["id"].as_str().map(str::to_owned))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_with_tool_use_and_cache_usage() {
        let (base, rx) = mock::serve(
            200,
            r#"{"type":"message","model":"claude-x","stop_reason":"tool_use","content":[{"type":"text","text":"checking"},{"type":"tool_use","id":"t1","name":"read","input":{"path":"a.rs"}}],"usage":{"input_tokens":10,"cache_read_input_tokens":90,"cache_creation_input_tokens":0,"output_tokens":4}}"#,
        );
        let provider = AnthropicProvider::new(Endpoint {
            name: "anthropic".into(),
            protocol: Protocol::Anthropic,
            base_url: format!("{base}/v1"),
            api_key: Some("sk-test".into()),
            headers: vec![],
        });
        let request = Request {
            model: "claude-x".into(),
            system: Some("sys".into()),
            messages: vec![
                Message::user("read"),
                Message {
                    role: Role::User,
                    parts: vec![Part::ToolResult {
                        call_id: "t0".into(),
                        output: "err".into(),
                        is_error: true,
                    }],
                },
            ],
            tools: vec![ToolSpec {
                name: "read".into(),
                description: "d".into(),
                parameters: json!({"type": "object"}),
            }],
            ..Request::default()
        };
        let response = provider.complete(&request).unwrap();
        let sent = rx.recv().unwrap();
        assert_eq!(sent.path, "/v1/messages");
        assert!(
            sent.headers
                .iter()
                .any(|(k, v)| k == "x-api-key" && v == "sk-test")
        );
        assert!(sent.headers.iter().any(|(k, _)| k == "anthropic-version"));
        assert_eq!(sent.body["max_tokens"], 8192);
        assert_eq!(sent.body["tools"][0]["input_schema"]["type"], "object");
        assert_eq!(sent.body["messages"][1]["content"][0]["is_error"], true);
        assert_eq!(response.text(), "checking");
        assert_eq!(response.stop, Stop::ToolUse);
        assert_eq!(
            response.usage,
            Usage {
                input: Some(100),
                cached_input: Some(90),
                output: Some(4)
            }
        );
    }

    #[test]
    fn overload_is_transient_and_billing_is_quota() {
        let error = |t: &str| {
            parse(
                &json!({"type":"error","error":{"type":t,"message":"x"}}),
                Duration::ZERO,
                "m",
            )
            .unwrap_err()
            .kind
        };
        assert_eq!(error("overloaded_error"), ErrorKind::Transient);
        assert_eq!(error("billing_error"), ErrorKind::Quota);
        assert_eq!(error("rate_limit_error"), ErrorKind::RateLimited);
    }
}
