//! OpenAI Chat Completions protocol (`POST {base}/chat/completions`).
use super::*;
use serde_json::{Value, json};

pub struct ChatProvider {
    endpoint: Endpoint,
}

impl ChatProvider {
    pub fn new(endpoint: Endpoint) -> Self {
        Self { endpoint }
    }
}

pub(crate) fn body(request: &Request) -> Value {
    let mut messages = Vec::new();
    if let Some(system) = &request.system {
        messages.push(json!({"role": "system", "content": system}));
    }
    for message in &request.messages {
        match message.role {
            Role::User => {
                let text = message.text();
                let has_images = message
                    .parts
                    .iter()
                    .any(|p| matches!(p, Part::Image { .. }));
                if has_images {
                    let content: Vec<Value> = message.parts.iter().filter_map(|p| match p {
                        Part::Text(t) => Some(json!({"type":"text","text":t})),
                        Part::Image { mime_type, data } => Some(json!({"type":"image_url","image_url":{"url":format!("data:{mime_type};base64,{data}")}})),
                        _ => None,
                    }).collect();
                    messages.push(json!({"role":"user","content":content}));
                } else if !text.is_empty() {
                    messages.push(json!({"role":"user","content":text}));
                }
                for part in &message.parts {
                    if let Part::ToolResult {
                        call_id, output, ..
                    } = part
                    {
                        messages.push(
                            json!({"role": "tool", "tool_call_id": call_id, "content": output}),
                        );
                    }
                }
            }
            Role::Assistant => {
                let text = message.text();
                let calls: Vec<Value> = message
                    .parts
                    .iter()
                    .filter_map(|p| match p {
                        Part::ToolCall {
                            id,
                            name,
                            arguments,
                        } => Some(json!({
                            "id": id, "type": "function",
                            "function": {"name": name, "arguments": arguments_string(arguments)},
                        })),
                        _ => None,
                    })
                    .collect();
                let mut item = json!({"role": "assistant", "content": if text.is_empty() { Value::Null } else { Value::String(text) }});
                if !calls.is_empty() {
                    item["tool_calls"] = Value::Array(calls);
                }
                messages.push(item);
            }
        }
    }
    let mut body = json!({"model": request.model, "messages": messages, "stream": false});
    if !request.tools.is_empty() {
        body["tools"] = request
            .tools
            .iter()
            .map(|t| json!({"type": "function", "function": {"name": t.name, "description": t.description, "parameters": t.parameters}}))
            .collect();
    }
    if let Some(max) = request.max_output_tokens {
        body["max_tokens"] = json!(max);
    }
    if let Some(t) = request.temperature {
        body["temperature"] = json!(t);
    }
    if let Some(effort) = &request.reasoning_effort {
        body["reasoning_effort"] = json!(effort);
    }
    body
}

pub(crate) fn parse(
    value: &Value,
    latency: Duration,
    requested: &str,
) -> Result<Response, ProviderError> {
    if let Some(error) = value.get("error").filter(|e| !e.is_null()) {
        return Err(ProviderError::new(ErrorKind::Transient, error.to_string()));
    }
    let choice = value["choices"].get(0).ok_or_else(|| {
        ProviderError::new(
            ErrorKind::Transient,
            format!("no choices in response: {value}"),
        )
    })?;
    let message = &choice["message"];
    let mut parts = Vec::new();
    if let Some(text) = message["content"].as_str().filter(|t| !t.is_empty()) {
        parts.push(Part::Text(text.to_owned()));
    }
    for call in message["tool_calls"].as_array().into_iter().flatten() {
        parts.push(Part::ToolCall {
            id: call["id"].as_str().unwrap_or_default().to_owned(),
            name: call["function"]["name"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            arguments: parse_arguments(&call["function"]["arguments"]),
        });
    }
    let has_calls = parts.iter().any(|p| matches!(p, Part::ToolCall { .. }));
    let stop = match choice["finish_reason"].as_str() {
        Some("tool_calls") | Some("function_call") => Stop::ToolUse,
        _ if has_calls => Stop::ToolUse,
        Some("length") => Stop::MaxTokens,
        Some("stop") | None => Stop::EndTurn,
        Some(other) => Stop::Other(other.to_owned()),
    };
    let usage = &value["usage"];
    Ok(Response {
        parts,
        usage: Usage {
            input: usage["prompt_tokens"].as_u64(),
            cached_input: usage["prompt_tokens_details"]["cached_tokens"].as_u64(),
            output: usage["completion_tokens"].as_u64(),
        },
        stop,
        model: value["model"].as_str().unwrap_or(requested).to_owned(),
        latency,
        route: value["_loom_route"].as_str().map(str::to_owned),
        gateway_connection: value["_loom_connection"].as_str().map(str::to_owned),
        gateway_upstream_ms: value["_loom_upstream_ms"].as_u64(),
    })
}

impl Provider for ChatProvider {
    fn name(&self) -> &str {
        &self.endpoint.name
    }

    fn complete(&self, request: &Request) -> Result<Response, ProviderError> {
        let headers = auth_headers(&self.endpoint);
        let (value, latency) = super::post_json_cancellable(
            request,
            &url(&self.endpoint.base_url, "chat/completions"),
            &headers,
            &body(request),
        )?;
        parse(&value, latency, &request.model)
    }

    fn complete_stream(
        &self,
        request: &Request,
        observe: &mut dyn FnMut(StreamEvent),
    ) -> Result<Response, ProviderError> {
        stream::chat(
            &url(&self.endpoint.base_url, "chat/completions"),
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

    fn tool_request() -> Request {
        Request {
            model: "m".into(),
            system: Some("be brief".into()),
            messages: vec![
                Message::user("weather in Seoul?"),
                Message {
                    role: Role::Assistant,
                    parts: vec![Part::ToolCall {
                        id: "c1".into(),
                        name: "get_weather".into(),
                        arguments: json!({"city": "Seoul"}),
                    }],
                },
                Message {
                    role: Role::User,
                    parts: vec![Part::ToolResult {
                        call_id: "c1".into(),
                        output: "맑음".into(),
                        is_error: false,
                    }],
                },
            ],
            tools: vec![ToolSpec {
                name: "get_weather".into(),
                description: "weather".into(),
                parameters: json!({"type": "object", "properties": {"city": {"type": "string"}}}),
            }],
            max_output_tokens: Some(100),
            temperature: None,
            reasoning_effort: None,
            cancel: None,
            timeout: None,
            gateway_connection: None,
        }
    }

    #[test]
    fn round_trip_with_tool_calls_and_usage() {
        let (base, rx) = mock::serve(
            200,
            r#"{"model":"served-model","choices":[{"finish_reason":"tool_calls","message":{"content":null,"tool_calls":[{"id":"c2","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"Busan\"}"}}]}}],"usage":{"prompt_tokens":120,"completion_tokens":9,"prompt_tokens_details":{"cached_tokens":100}}}"#,
        );
        let provider = ChatProvider::new(Endpoint {
            name: "test".into(),
            protocol: Protocol::Chat,
            base_url: format!("{base}/v1"),
            api_key: Some("k".into()),
            headers: vec![],
        });
        let response = provider.complete(&tool_request()).unwrap();
        let sent = rx.recv().unwrap();
        assert_eq!(sent.path, "/v1/chat/completions");
        assert!(
            sent.headers
                .iter()
                .any(|(k, v)| k == "authorization" && v == "Bearer k")
        );
        let messages = sent.body["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(
            messages[2]["tool_calls"][0]["function"]["arguments"],
            r#"{"city":"Seoul"}"#
        );
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_call_id"], "c1");
        assert_eq!(sent.body["tools"][0]["function"]["name"], "get_weather");
        assert_eq!(response.stop, Stop::ToolUse);
        assert_eq!(response.model, "served-model");
        let (id, name, args) = response.tool_calls().next().unwrap();
        assert_eq!(
            (id, name, args),
            ("c2", "get_weather", &json!({"city": "Busan"}))
        );
        assert_eq!(
            response.usage,
            Usage {
                input: Some(120),
                cached_input: Some(100),
                output: Some(9)
            }
        );
    }
}
