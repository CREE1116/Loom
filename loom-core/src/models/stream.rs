//! Incremental wire decoding. Readers consume complete SSE events/NDJSON records,
//! independent of TCP chunk boundaries, and never turn a completed reply into deltas.
use super::*;
use serde_json::json;
use std::{
    collections::BTreeMap,
};

fn unreadable(error: impl std::fmt::Display) -> ProviderError {
    ProviderError::new(ErrorKind::Transient, format!("unreadable stream: {error}"))
}

fn exchange(
    endpoint: &str,
    headers: &[(String, String)],
    body: &Value,
    model_request: &Request,
    ndjson: bool,
    accept: &mut dyn FnMut(&str) -> Result<bool, ProviderError>,
) -> Result<(Duration, Option<String>), ProviderError> {
    super::http::run(model_request, async {
        let started = Instant::now();
        let mut response = super::http::send(model_request, endpoint, headers, body).await?;
        let route: Option<String> = None;
        let mut pending = Vec::new();
        let mut data = Vec::new();
        loop {
            model_request.check_cancelled()?;
            let chunk = response.chunk().await.map_err(super::http::error)?;
            let eof = chunk.is_none();
            if let Some(chunk) = chunk { pending.extend_from_slice(&chunk); }
            let mut consumed = 0;
            while let Some(length) = pending[consumed..].iter().position(|byte| *byte == b'\n')
                .map(|offset| offset + 1)
                .or_else(|| (eof && consumed < pending.len()).then_some(pending.len() - consumed))
            {
                model_request.check_cancelled()?;
                let line = std::str::from_utf8(&pending[consumed..consumed + length]).map_err(unreadable)?;
                consumed += length;
                let line = line.trim_end_matches(['\r', '\n']);
                let stop = if ndjson {
                    !line.is_empty() && accept(line)?
                } else if line.is_empty() && !data.is_empty() {
                    let event = data.join("\n");
                    data.clear();
                    accept(&event)?
                } else {
                    if let Some(value) = line.strip_prefix("data:") {
                        data.push(value.strip_prefix(' ').unwrap_or(value).to_owned());
                    }
                    false
                };
                model_request.check_cancelled()?;
                if stop { return Ok((started.elapsed(), route)); }
            }
            pending.drain(..consumed);
            if eof {
                if !ndjson && !data.is_empty() {
                    accept(&data.join("\n"))?;
                    model_request.check_cancelled()?;
                }
                break;
            }
        }
        Ok((started.elapsed(), route))
    })
}

fn value(data: &str) -> Result<Value, ProviderError> {
    serde_json::from_str(data).map_err(unreadable)
}

fn stream_error(error: &Value) -> ProviderError {
    let message = error["message"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| error.to_string());
    let detail = format!("{} {message}", error["code"]).to_ascii_lowercase();
    let kind = if detail.contains("quota")
        || detail.contains("usage limit")
        || detail.contains("insufficient")
        || detail.contains("billing")
    {
        ErrorKind::Quota
    } else if detail.contains("rate_limit") || detail.contains("rate limit") {
        ErrorKind::RateLimited
    } else if detail.contains("authentication") || detail.contains("invalid_api_key") {
        ErrorKind::Auth
    } else if detail.contains("invalid_request") || error["code"]=="tool_use_failed" {
        ErrorKind::BadRequest
    } else {
        ErrorKind::Transient
    };
    let mut failure=ProviderError::new(kind,message);
    failure.code=error["code"].as_str().map(ToOwned::to_owned);
    failure
}

fn delta(text: &str, observe: &mut dyn FnMut(StreamEvent)) {
    if !text.is_empty() {
        observe(StreamEvent::TextDelta(text.to_owned()));
    }
}

fn usage(usage: &Usage, observe: &mut dyn FnMut(StreamEvent)) {
    if *usage != Usage::default() {
        observe(StreamEvent::Usage(usage.clone()));
    }
}

pub(super) fn chat(
    endpoint: &str,
    headers: &[(String, String)],
    request: &Request,
    mut body: Value,
    observe: &mut dyn FnMut(StreamEvent),
) -> Result<Response, ProviderError> {
    body["stream"] = json!(true);
    body["stream_options"] = json!({"include_usage":true});
    let mut text = String::new();
    let mut calls: BTreeMap<u64, (String, String, String)> = BTreeMap::new();
    let mut model = request.model.clone();
    let mut finish = Value::Null;
    let mut reported_usage = Value::Null;
    let mut done = false;
    let (latency, route) = exchange(endpoint, headers, &body, request, false, &mut |data| {
        if data == "[DONE]" {
            done = true;
            return Ok(true);
        }
        let v = value(data)?;
        if v.get("error").is_some_and(|e| !e.is_null()) {
            return Err(stream_error(&v["error"]));
        }
        if let Some(m) = v["model"].as_str() {
            model = m.to_owned();
        }
        if !v["usage"].is_null() {
            reported_usage = v["usage"].clone();
        }
        for choice in v["choices"].as_array().into_iter().flatten() {
            if choice["index"].as_u64().unwrap_or(0) != 0 {
                continue;
            }
            let d = &choice["delta"];
            if let Some(t) = d["content"].as_str() {
                text.push_str(t);
                delta(t, observe);
            }
            for call in d["tool_calls"].as_array().into_iter().flatten() {
                let index = call["index"]
                    .as_u64()
                    .ok_or_else(|| unreadable("tool call missing index"))?;
                let entry = calls.entry(index).or_default();
                if let Some(id) = call["id"].as_str() {
                    entry.0.push_str(id);
                }
                if let Some(name) = call["function"]["name"].as_str() {
                    entry.1.push_str(name);
                }
                if let Some(args) = call["function"]["arguments"].as_str() {
                    entry.2.push_str(args);
                }
            }
            if !choice["finish_reason"].is_null() {
                finish = choice["finish_reason"].clone();
            }
        }
        Ok(false)
    })?;
    if !done {
        return Err(unreadable("ended before [DONE]"));
    }
    let calls: Vec<Value> = calls
        .into_values()
        .map(
            |(id, name, arguments)| json!({"id":id,"function":{"name":name,"arguments":arguments}}),
        )
        .collect();
    let response = super::chat::parse(
        &json!({"model":model,"choices":[{"message":{"content":text,"tool_calls":calls},"finish_reason":finish}],"usage":reported_usage,"_loom_route":route}),
        latency,
        &request.model,
    )?;
    usage(&response.usage, observe);
    Ok(response)
}

pub(super) fn responses(
    endpoint: &str,
    headers: &[(String, String)],
    request: &Request,
    mut body: Value,
    observe: &mut dyn FnMut(StreamEvent),
) -> Result<Response, ProviderError> {
    body["stream"] = json!(true);
    let mut terminal = None;
    let mut items = Vec::new();
    let (latency, route) = exchange(endpoint, headers, &body, request, false, &mut |data| {
        if data == "[DONE]" {
            return Ok(true);
        }
        let v = value(data)?;
        match v["type"].as_str() {
            Some("response.output_text.delta") => {
                if let Some(t) = v["delta"].as_str() {
                    delta(t, observe);
                }
            }
            Some("response.output_item.done") => items.push(v["item"].clone()),
            Some("response.completed" | "response.incomplete" | "response.failed") => {
                terminal = Some(v["response"].clone());
                return Ok(true);
            }
            Some("error") => {
                return Err(stream_error(&v));
            }
            _ => {} // Forward-compatible lifecycle/reasoning/tool argument events.
        }
        Ok(false)
    })?;
    // Terminal Responses events include the complete authoritative output, including
    // all tool arguments. Parsing it avoids duplicating streamed text or tool calls.
    let mut terminal = terminal.ok_or_else(|| unreadable("ended before terminal response"))?;
    // Stateless ChatGPT plan responses complete with an empty `output`; their items
    // arrived one by one as output_item.done. Without them the reply looks empty and
    // the worker asks again forever.
    let reported = terminal["output"].as_array().is_some_and(|output| !output.is_empty());
    if !reported && !items.is_empty() {
        terminal["output"] = json!(items);
    }
    terminal["_loom_route"] = json!(route);
    let response = super::responses::parse(&terminal, latency, &request.model)?;
    usage(&response.usage, observe);
    Ok(response)
}

pub(super) fn anthropic(
    endpoint: &str,
    headers: &[(String, String)],
    request: &Request,
    mut body: Value,
    observe: &mut dyn FnMut(StreamEvent),
) -> Result<Response, ProviderError> {
    body["stream"] = json!(true);
    let mut message = json!({"model":request.model,"usage":{},"content":[]});
    let mut blocks = BTreeMap::<u64, Value>::new();
    let mut arguments = BTreeMap::<u64, String>::new();
    let mut done = false;
    let (latency, route) = exchange(endpoint, headers, &body, request, false, &mut |data| {
        let v = value(data)?;
        match v["type"].as_str() {
            Some("error") => {
                super::anthropic::parse(&v, Duration::ZERO, &request.model)?;
            }
            Some("message_start") => {
                message = v["message"].clone();
            }
            Some("content_block_start") => {
                let i = v["index"]
                    .as_u64()
                    .ok_or_else(|| unreadable("block missing index"))?;
                let block = v["content_block"].clone();
                if let Some(t) = block["text"].as_str() {
                    delta(t, observe);
                }
                blocks.insert(i, block);
            }
            Some("content_block_delta") => {
                let i = v["index"]
                    .as_u64()
                    .ok_or_else(|| unreadable("delta missing index"))?;
                let d = &v["delta"];
                match d["type"].as_str() {
                    Some("text_delta") => {
                        let block = blocks
                            .get_mut(&i)
                            .ok_or_else(|| unreadable("delta before block"))?;
                        if let Some(t) = d["text"].as_str() {
                            let mut text = block["text"].as_str().unwrap_or_default().to_owned();
                            text.push_str(t);
                            block["text"] = json!(text);
                            delta(t, observe);
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(t) = d["partial_json"].as_str() {
                            arguments.entry(i).or_default().push_str(t);
                        }
                    }
                    _ => {}
                }
            }
            Some("message_delta") => {
                if !v["delta"]["stop_reason"].is_null() {
                    message["stop_reason"] = v["delta"]["stop_reason"].clone();
                }
                if let Some(updates) = v["usage"].as_object() {
                    for (key, val) in updates {
                        message["usage"][key] = val.clone();
                    }
                }
            }
            Some("message_stop") => {
                done = true;
                return Ok(true);
            }
            _ => {}
        }
        Ok(false)
    })?;
    if !done {
        return Err(unreadable("ended before message_stop"));
    }
    for (index, raw) in arguments {
        let block = blocks
            .get_mut(&index)
            .ok_or_else(|| unreadable("arguments before block"))?;
        block["input"] = serde_json::from_str(&raw).map_err(unreadable)?;
    }
    message["content"] = json!(blocks.into_values().collect::<Vec<_>>());
    message["_loom_route"] = json!(route);
    let response = super::anthropic::parse(&message, latency, &request.model)?;
    usage(&response.usage, observe);
    Ok(response)
}

pub(super) fn ollama(
    endpoint: &str,
    headers: &[(String, String)],
    request: &Request,
    mut body: Value,
    observe: &mut dyn FnMut(StreamEvent),
) -> Result<Response, ProviderError> {
    body["stream"] = json!(true);
    let mut aggregate = json!({"model":request.model});
    let mut text = String::new();
    let mut calls = Vec::new();
    let mut done = false;
    let (latency, route) = exchange(endpoint, headers, &body, request, true, &mut |data| {
        let v = value(data)?;
        if !v["error"].is_null() {
            super::ollama::parse(&v, Duration::ZERO, &request.model)?;
        }
        if let Some(m) = v["model"].as_str() {
            aggregate["model"] = json!(m);
        }
        if let Some(t) = v["message"]["content"].as_str() {
            text.push_str(t);
            delta(t, observe);
        }
        if let Some(c) = v["message"]["tool_calls"].as_array() {
            calls.extend(c.iter().cloned());
        }
        if v["done"] == true {
            done = true;
            for key in ["done_reason", "prompt_eval_count", "eval_count"] {
                aggregate[key] = v[key].clone();
            }
            return Ok(true);
        }
        Ok(false)
    })?;
    if !done {
        return Err(unreadable("ended before done"));
    }
    aggregate["message"] = json!({"content":text,"tool_calls":calls});
    aggregate["_loom_route"] = json!(route);
    let response = super::ollama::parse(&aggregate, latency, &request.model)?;
    usage(&response.usage, observe);
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sse(events: &[Value]) -> String {
        events.iter().map(|event| format!("data: {event}\n\n")).collect()
    }

    #[test]
    fn responses_items_streamed_before_an_empty_terminal_output_are_kept() {
        // The ChatGPT plan backend (store=false) sends each output item as
        // response.output_item.done and then completes with an empty `output`.
        let body = sse(&[
            json!({"type":"response.output_item.done","item":{"type":"reasoning","summary":[]}}),
            json!({"type":"response.output_text.delta","delta":"Looking."}),
            json!({"type":"response.output_item.done","item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Looking."}]}}),
            json!({"type":"response.output_item.done","item":{"type":"function_call","call_id":"c1","name":"read","arguments":"{\"path\":\"a.rs\"}"}}),
            json!({"type":"response.completed","response":{"model":"gpt-x","status":"completed","output":[],"usage":{"input_tokens":9,"output_tokens":12}}}),
        ]);
        let (base, _rx) = mock::serve(200, &body);
        let request = Request { model: "gpt-x".into(), ..Request::default() };
        let response = responses(&format!("{base}/responses"), &[], &request, json!({}), &mut |_| {}).unwrap();
        assert_eq!(response.text(), "Looking.");
        assert!(matches!(
            response.parts.last(),
            Some(Part::ToolCall { id, name, .. }) if id == "c1" && name == "read"
        ), "{:?}", response.parts);
        assert_eq!(response.stop, Stop::ToolUse);
    }

    #[test]
    fn a_complete_terminal_output_is_not_duplicated_by_streamed_items() {
        let call = json!({"type":"function_call","call_id":"c1","name":"read","arguments":"{}"});
        let body = sse(&[
            json!({"type":"response.output_item.done","item":call}),
            json!({"type":"response.completed","response":{"status":"completed","output":[call]}}),
        ]);
        let (base, _rx) = mock::serve(200, &body);
        let request = Request { model: "gpt-x".into(), ..Request::default() };
        let response = responses(&format!("{base}/responses"), &[], &request, json!({}), &mut |_| {}).unwrap();
        assert_eq!(response.parts.len(), 1, "{:?}", response.parts);
    }
}
