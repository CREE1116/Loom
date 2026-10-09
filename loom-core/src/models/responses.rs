//! OpenAI Responses protocol (`POST {base}/responses`), stateless (`store: false`).
use super::*;
use serde_json::{Value, json};

pub struct ResponsesProvider {
    endpoint: Endpoint,
}

impl ResponsesProvider {
    pub fn new(endpoint: Endpoint) -> Self {
        Self { endpoint }
    }
}

pub(crate) fn body(request: &Request) -> Value {
    let mut input = Vec::new();
    for message in &request.messages {
        for part in &message.parts {
            match (message.role, part) {
                (Role::User, Part::Text(text)) => input.push(json!({
                    "type": "message", "role": "user", "content": [{"type": "input_text", "text": text}],
                })),
                (Role::Assistant, Part::Text(text)) => input.push(json!({
                    "type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}],
                })),
                (_, Part::Image { mime_type, data }) => input.push(json!({
                    "type":"message", "role":"user", "content":[{"type":"input_image","image_url":format!("data:{mime_type};base64,{data}")}],
                })),
                (_, Part::ToolCall { id, name, arguments }) => input.push(json!({
                    "type": "function_call", "call_id": id, "name": name, "arguments": arguments_string(arguments),
                })),
                (_, Part::ToolResult { call_id, output, .. }) => input.push(json!({
                    "type": "function_call_output", "call_id": call_id, "output": output,
                })),
            }
        }
    }
    let mut body = json!({"model": request.model, "input": input, "store": false, "stream": false});
    if let Some(system) = &request.system {
        body["instructions"] = json!(system);
    }
    if !request.tools.is_empty() {
        body["tools"] = request
            .tools
            .iter()
            .map(|t| json!({"type": "function", "name": t.name, "description": t.description, "parameters": t.parameters}))
            .collect();
    }
    if let Some(max) = request.max_output_tokens {
        body["max_output_tokens"] = json!(max);
    }
    if let Some(t) = request.temperature {
        body["temperature"] = json!(t);
    }
    if let Some(effort) = &request.reasoning_effort {
        body["reasoning"] = json!({"effort":effort});
    }
    body
}

pub(crate) fn parse(
    value: &Value,
    latency: Duration,
    requested: &str,
) -> Result<Response, ProviderError> {
    if value["status"] == "failed" || value.get("error").is_some_and(|e| !e.is_null()) {
        let message = value["error"]["message"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| value["error"].to_string());
        let lower = message.to_ascii_lowercase();
        let kind = if lower.contains("quota") || lower.contains("usage limit") {
            ErrorKind::Quota
        } else if lower.contains("rate limit") {
            ErrorKind::RateLimited
        } else {
            ErrorKind::Transient
        };
        return Err(ProviderError::new(kind, message));
    }
    let mut parts = Vec::new();
    for item in value["output"].as_array().into_iter().flatten() {
        match item["type"].as_str() {
            Some("message") => {
                for content in item["content"].as_array().into_iter().flatten() {
                    if let Some(text) = content["text"].as_str().filter(|t| !t.is_empty()) {
                        parts.push(Part::Text(text.to_owned()));
                    }
                }
            }
            Some("function_call") => parts.push(Part::ToolCall {
                id: item["call_id"].as_str().unwrap_or_default().to_owned(),
                name: item["name"].as_str().unwrap_or_default().to_owned(),
                arguments: parse_arguments(&item["arguments"]),
            }),
            _ => {}
        }
    }
    let stop = if parts.iter().any(|p| matches!(p, Part::ToolCall { .. })) {
        Stop::ToolUse
    } else if value["status"] == "incomplete" {
        Stop::MaxTokens
    } else {
        Stop::EndTurn
    };
    let usage = &value["usage"];
    Ok(Response {
        parts,
        usage: Usage {
            input: usage["input_tokens"].as_u64(),
            cached_input: usage["input_tokens_details"]["cached_tokens"].as_u64(),
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

impl Provider for ResponsesProvider {
    fn name(&self) -> &str {
        &self.endpoint.name
    }

    fn complete(&self, request: &Request) -> Result<Response, ProviderError> {
        let (value, latency) = super::post_json_cancellable(
            request,
            &url(&self.endpoint.base_url, "responses"),
            &auth_headers(&self.endpoint),
            &body(request),
        )?;
        parse(&value, latency, &request.model)
    }

    fn complete_stream(
        &self,
        request: &Request,
        observe: &mut dyn FnMut(StreamEvent),
    ) -> Result<Response, ProviderError> {
        stream::responses(
            &url(&self.endpoint.base_url, "responses"),
            &auth_headers(&self.endpoint),
            request,
            body(request),
            observe,
        )
    }

    fn list_models(&self) -> Result<Vec<String>, ProviderError> {
        let value = get_json(
            &url(&self.endpoint.base_url, "models"),
            &auth_headers(&self.endpoint),
        )?;
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
    fn round_trip_with_function_calls() {
        let (base, rx) = mock::serve(
            200,
            r#"{"model":"gpt-x","status":"completed","output":[{"type":"reasoning"},{"type":"function_call","call_id":"c9","name":"read","arguments":"{\"path\":\"a.rs\"}"}],"usage":{"input_tokens":50,"input_tokens_details":{"cached_tokens":40},"output_tokens":5}}"#,
        );
        let provider = ResponsesProvider::new(Endpoint {
            name: "openai".into(),
            protocol: Protocol::Responses,
            base_url: format!("{base}/v1"),
            api_key: None,
            headers: vec![],
        });
        let request = Request {
            model: "gpt-x".into(),
            system: Some("sys".into()),
            messages: vec![
                Message::user("read a.rs"),
                Message {
                    role: Role::Assistant,
                    parts: vec![Part::ToolCall {
                        id: "c1".into(),
                        name: "read".into(),
                        arguments: json!({"path": "b.rs"}),
                    }],
                },
                Message {
                    role: Role::User,
                    parts: vec![Part::ToolResult {
                        call_id: "c1".into(),
                        output: "fn b()".into(),
                        is_error: false,
                    }],
                },
            ],
            ..Request::default()
        };
        let response = provider.complete(&request).unwrap();
        let sent = rx.recv().unwrap();
        assert_eq!(sent.path, "/v1/responses");
        assert_eq!(sent.body["instructions"], "sys");
        assert_eq!(sent.body["store"], false);
        assert_eq!(sent.body["input"][1]["type"], "function_call");
        assert_eq!(sent.body["input"][2]["type"], "function_call_output");
        assert_eq!(response.stop, Stop::ToolUse);
        assert_eq!(
            response.tool_calls().next().unwrap().2,
            &json!({"path": "a.rs"})
        );
        assert_eq!(response.usage.cached_input, Some(40));
    }

    #[test]
    fn failed_status_with_usage_limit_is_quota() {
        let error = parse(
            &json!({"status":"failed","error":{"message":"You've hit your usage limit"}}),
            Duration::ZERO,
            "m",
        )
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Quota);
    }
}
