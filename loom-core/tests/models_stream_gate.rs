mod isolation;
use loom_core::models::*;
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    sync::mpsc,
    thread,
    time::Duration,
};

// The server withholds the terminal frame until the observer acknowledges the first
// delta. This fails for implementations that buffer or simulate streaming at EOF.
fn server(
    protocol: Protocol,
    first: String,
    rest: String,
    route: Option<&str>,
) -> (Box<dyn Provider>, mpsc::Receiver<Value>, mpsc::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let (sent, received) = mpsc::channel();
    let (ack, wait) = mpsc::channel();
    let route = route.map(str::to_owned);
    thread::spawn(move || {
        loop {
            let (mut socket, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let show = line.contains("/api/show");
            let mut length = 0;
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some((k, v)) = line.split_once(':')
                    && k.eq_ignore_ascii_case("content-length")
                {
                    length = v.trim().parse().unwrap();
                }
            }
            let mut bytes = vec![0; length];
            reader.read_exact(&mut bytes).unwrap();
            if show {
                socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                    )
                    .unwrap();
                continue;
            }
            sent.send(serde_json::from_slice(&bytes).unwrap()).unwrap();
            let content_type = if protocol == Protocol::Ollama {
                "application/x-ndjson"
            } else {
                "text/event-stream"
            };
            let _ = &route;
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", first.len() + rest.len()).unwrap();
            for fragment in first.as_bytes().chunks(3) {
                if socket.write_all(fragment).is_err() {
                    return;
                }
            }
            socket.flush().unwrap();
            if wait.recv_timeout(Duration::from_secs(2)).is_err() {
                return;
            }
            for fragment in rest.as_bytes().chunks(5) {
                if socket.write_all(fragment).is_err() {
                    return;
                }
            }
            return;
        }
    });
    (
        connect(Endpoint {
            name: "test".into(),
            protocol,
            base_url,
            api_key: None,
            headers: vec![],
        }),
        received,
        ack,
    )
}

fn sse(value: Value) -> String {
    format!("data: {value}\r\n\r\n")
}

fn request() -> Request {
    Request {
        model: "m".into(),
        messages: vec![Message {
            role: Role::User,
            parts: vec![
                Part::Text("look".into()),
                Part::Image {
                    mime_type: "image/png".into(),
                    data: "aGVsbG8=".into(),
                },
            ],
        }],
        reasoning_effort: Some("high".into()),
        ..Request::default()
    }
}

#[test]
fn cancelled_gateway_wait_returns_without_waiting_for_the_socket() {
    use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
    use std::time::Instant;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (accepted, rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let (_socket, _) = listener.accept().unwrap();
        accepted.send(()).unwrap();
        thread::sleep(Duration::from_millis(450));
    });
    let provider = connect(Endpoint { name: "gateway".into(), protocol: Protocol::Chat,
        base_url: url, api_key: None, headers: vec![] });
    let cancel = Arc::new(AtomicBool::new(false));
    let mut request = request();
    request.cancel = Some(cancel.clone());
    request.timeout = Some(Duration::from_secs(2));
    let started = Instant::now();
    let waiter = thread::spawn(move || provider.complete(&request));
    rx.recv_timeout(Duration::from_secs(1)).unwrap();
    cancel.store(true, Ordering::SeqCst);
    let error = waiter.join().unwrap().unwrap_err();
    assert_eq!(error.kind, ErrorKind::Cancelled);
    assert!(started.elapsed() < Duration::from_millis(400));
    server.join().unwrap();
}

#[test]
fn unresponsive_gateway_is_bounded_by_model_call_timeout() {
    use std::sync::{Arc, atomic::AtomicBool};
    use std::time::Instant;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let (_socket, _) = listener.accept().unwrap();
        thread::sleep(Duration::from_millis(500));
    });
    let provider = connect(Endpoint { name: "gateway".into(), protocol: Protocol::Chat,
        base_url: url, api_key: None, headers: vec![] });
    let mut request = request();
    request.cancel = Some(Arc::new(AtomicBool::new(false)));
    request.timeout = Some(Duration::from_millis(90));
    let start = Instant::now();
    let error = provider.complete(&request).unwrap_err();
    assert_eq!(error.kind, ErrorKind::Transient);
    assert!(start.elapsed() < Duration::from_millis(400));
    server.join().unwrap();
}

#[test]
fn stream_preserves_a_terminal_event_at_eof_without_a_blank_line() {
    let first = sse(json!({"choices":[{"index":0,"delta":{"content":"hello"},"finish_reason":"stop"}]}));
    let (provider, _sent, ack) = server(Protocol::Chat, first, "data: [DONE]".into(), None);
    let reply = provider.complete_stream(&request(), &mut |event| {
        if matches!(event, StreamEvent::TextDelta(_)) { let _ = ack.send(()); }
    }).unwrap();
    assert_eq!(reply.text(), "hello");
}

#[test]
fn chat_streams_before_eof_and_joins_tool_arguments() {
    let first = sse(json!({"model":"served", "choices":[{"index":0,"delta":{"content":"한"}}]}));
    let rest = [
        sse(json!({"choices":[{"index":0,"delta":{"content":"글", "tool_calls":[{"index":0,"id":"c","function":{"name":"read","arguments":"{\"path\":"}}]}}]})),
        sse(json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a\"}"}}]},"finish_reason":"tool_calls"}]})),
        sse(json!({"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":3}})),
        "data: [DONE]\r\n\r\n".into(),
    ].concat();
    let (provider, sent, ack) = server(Protocol::Chat, first, rest, None);
    let mut events = Vec::new();
    let reply = provider
        .complete_stream(&request(), &mut |e| {
            if matches!(e, StreamEvent::TextDelta(_)) {
                let _ = ack.send(());
            }
            events.push(e);
        })
        .unwrap();
    assert_eq!(reply.text(), "한글");
    assert_eq!(reply.model, "served");
    assert_eq!(
        reply.tool_calls().next().unwrap(),
        ("c", "read", &json!({"path":"a"}))
    );
    assert_eq!(reply.usage.input, Some(12));
    assert_eq!(events[0], StreamEvent::TextDelta("한".into()));
    let body = sent.recv().unwrap();
    assert_eq!(body["stream"], true);
    assert_eq!(body["reasoning_effort"], "high");
    assert_eq!(
        body["messages"][0]["content"][1]["image_url"]["url"],
        "data:image/png;base64,aGVsbG8="
    );
}

#[test]
fn responses_streams_and_keeps_terminal_tool_calls() {
    let first = sse(json!({"type":"response.output_text.delta","delta":"hello"}));
    let rest = sse(
        json!({"type":"response.completed","response":{"status":"completed","model":"served","output":[{"type":"message","content":[{"type":"output_text","text":"hello"}]},{"type":"function_call","call_id":"c","name":"read","arguments":"{\"path\":\"a\"}"}],"usage":{"input_tokens":9,"output_tokens":2}}}),
    );
    let (provider, sent, ack) = server(Protocol::Responses, first, rest, None);
    let reply = provider
        .complete_stream(&request(), &mut |e| {
            if matches!(e, StreamEvent::TextDelta(_)) {
                ack.send(()).unwrap();
            }
        })
        .unwrap();
    assert_eq!(reply.text(), "hello");
    assert_eq!(reply.stop, Stop::ToolUse);
    assert_eq!(reply.usage.input, Some(9));
    let body = sent.recv().unwrap();
    assert_eq!(body["reasoning"]["effort"], "high");
    assert!(
        body["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["content"]
                .as_array()
                .is_some_and(|c| c.iter().any(|p| p["type"] == "input_image"
                    && p["image_url"] == "data:image/png;base64,aGVsbG8=")))
    );
}

#[test]
fn anthropic_streams_blocks_and_normalizes_cumulative_usage() {
    let first = [
        sse(json!({"type":"message_start","message":{"model":"served","usage":{"input_tokens":2,"cache_read_input_tokens":8,"output_tokens":1}}})),
        sse(json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})),
        sse(json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}})),
    ].concat();
    let rest = [
        sse(json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"c","name":"read","input":{}}})),
        sse(json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}})),
        sse(json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"a\"}"}})),
        sse(json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":4}})),
        sse(json!({"type":"message_stop"})),
    ].concat();
    let (provider, sent, ack) = server(Protocol::Anthropic, first, rest, None);
    let reply = provider
        .complete_stream(&request(), &mut |e| {
            if matches!(e, StreamEvent::TextDelta(_)) {
                ack.send(()).unwrap();
            }
        })
        .unwrap();
    assert_eq!(reply.text(), "hello");
    assert_eq!(reply.tool_calls().next().unwrap().2, &json!({"path":"a"}));
    assert_eq!(
        reply.usage,
        Usage {
            input: Some(10),
            cached_input: Some(8),
            output: Some(4)
        }
    );
    let body = sent.recv().unwrap();
    assert_eq!(body["output_config"]["effort"], "high");
    assert_eq!(
        body["messages"][0]["content"][1]["source"],
        json!({"type":"base64","media_type":"image/png","data":"aGVsbG8="})
    );
}

#[test]
fn ollama_streams_ndjson_and_keeps_calls_before_done() {
    let first = format!(
        "{}\n",
        json!({"model":"served","message":{"content":"hello"},"done":false})
    );
    let rest = format!(
        "{}\n{}\n",
        json!({"message":{"content":"", "tool_calls":[{"function":{"name":"read","arguments":{"path":"a"}}}]},"done":false}),
        json!({"done":true,"done_reason":"stop","prompt_eval_count":7,"eval_count":2})
    );
    let (provider, sent, ack) = server(Protocol::Ollama, first, rest, None);
    let reply = provider
        .complete_stream(&request(), &mut |e| {
            if matches!(e, StreamEvent::TextDelta(_)) {
                ack.send(()).unwrap();
            }
        })
        .unwrap();
    assert_eq!(reply.text(), "hello");
    assert_eq!(reply.tool_calls().next().unwrap().2, &json!({"path":"a"}));
    assert_eq!(reply.stop, Stop::ToolUse);
    assert_eq!(reply.usage.input, Some(7));
    let body = sent.recv().unwrap();
    assert_eq!(body["think"], "high");
    assert_eq!(body["messages"][0]["images"], json!(["aGVsbG8="]));
}

#[test]
fn message_images_are_serializable() {
    let message = request().messages.remove(0);
    assert_eq!(
        serde_json::from_value::<Message>(serde_json::to_value(&message).unwrap()).unwrap(),
        message
    );
}

#[test]
fn image_helper_detects_bytes_not_extensions_and_enforces_size() {
    use base64::Engine;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("misleading.txt");
    for (mime, bytes) in [
        ("image/png", b"\x89PNG\r\n\x1a\nbytes".as_slice()),
        ("image/jpeg", b"\xff\xd8\xffbytes".as_slice()),
        ("image/gif", b"GIF89abytes".as_slice()),
        ("image/webp", b"RIFF\x04\0\0\0WEBPbytes".as_slice()),
    ] {
        std::fs::write(&path, bytes).unwrap();
        let Part::Image { mime_type, data } = image_part(&path).unwrap() else {
            panic!("expected image");
        };
        assert_eq!(mime_type, mime);
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(data)
                .unwrap(),
            bytes
        );
    }
    std::fs::write(&path, b"not an image").unwrap();
    assert!(image_part(&path).is_err());
    assert!(image_part(dir.path()).is_err());
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(20 * 1024 * 1024 + 1).unwrap();
    assert!(
        image_part(&path)
            .unwrap_err()
            .to_string()
            .contains("20 MiB")
    );
}

#[test]
fn reasoning_profiles_are_explicit_and_unknown_models_stay_unknown() {
    let mut router = router::Router::new(None);
    router.add(
        Box::new(Fake {
            name: "test",
            error: None,
            emit: false,
        }),
        vec![candidate("test", "m", CostClass::Free)],
    );
    assert!(router.model_profiles()[0].supported_efforts.is_empty());
    assert!(router.model_profiles()[0].default_effort.is_none());
    router.set_reasoning_profile(
        "test",
        "m",
        ReasoningProfile {
            supported_efforts: vec!["low".into(), "high".into()],
            default_effort: Some("low".into()),
        },
    );
    let profile = router.model_profiles().remove(0);
    assert_eq!(profile.supported_efforts, ["low", "high"]);
    assert_eq!(profile.default_effort.as_deref(), Some("low"));
}

fn candidate(endpoint: &str, model: &str, cost: CostClass) -> router::Candidate {
    router::Candidate {
        endpoint: endpoint.into(),
        model: model.into(),
        tier: Tier::Small,
        cost,
        price: Some((2.0, 4.0)),
        tools: true,
        context: None,
    }
}

fn reply() -> Response {
    Response {
        parts: vec![Part::Text("answer".into())],
        usage: Usage {
            input: Some(10),
            cached_input: Some(2),
            output: Some(3),
        },
        stop: Stop::EndTurn,
        model: "served".into(),
        latency: Duration::ZERO,
        route: None,
        gateway_connection: None,
        gateway_upstream_ms: None,
    }
}

struct Fake {
    name: &'static str,
    error: Option<ErrorKind>,
    emit: bool,
}
impl Provider for Fake {
    fn name(&self) -> &str {
        self.name
    }
    fn list_models(&self) -> Result<Vec<String>, ProviderError> {
        Ok(vec!["m".into()])
    }
    fn complete(&self, _: &Request) -> Result<Response, ProviderError> {
        match self.error {
            Some(kind) => Err(ProviderError::new(kind, "failed")),
            None => Ok(reply()),
        }
    }
    fn complete_stream(
        &self,
        request: &Request,
        observe: &mut dyn FnMut(StreamEvent),
    ) -> Result<Response, ProviderError> {
        if self.emit {
            observe(StreamEvent::TextDelta("partial".into()));
        }
        self.complete(request)
    }
}

#[test]
fn legacy_port_fallback_does_not_fabricate_stream_events_or_quota() {
    let model = ScriptedModel::new([reply()]);
    let port: &dyn ModelPort = &model;
    let mut events = Vec::new();
    assert_eq!(
        port.complete_stream(Need::default(), &Request::default(), &mut |e| events
            .push(e))
            .unwrap()
            .response
            .text(),
        "answer"
    );
    assert!(events.is_empty());
    assert!(port.records().is_empty());
}

#[test]
fn stream_selection_enforces_cost_model_and_route_policy() {
    let mut router = router::Router::new(None);
    router.add(
        Box::new(Fake {
            name: "free",
            error: None,
            emit: false,
        }),
        vec![candidate("free", "cheap", CostClass::Free)],
    );
    router.add(
        Box::new(Fake {
            name: "paid",
            error: None,
            emit: false,
        }),
        vec![candidate("paid", "chosen", CostClass::Paid)],
    );
    router.add(
        Box::new(Fake {
            name: "cc",
            error: None,
            emit: false,
        }),
        vec![candidate("cc", "blocked", CostClass::Free)],
    );
    let port: &dyn ModelPort = &router;
    for model in ["chosen", "blocked", "missing"] {
        let request = Request {
            model: model.into(),
            ..Request::default()
        };
        assert!(
            port.complete_stream(Need::default(), &request, &mut |_| panic!(
                "disallowed output"
            ))
            .is_err()
        );
    }
    assert!(router.records().is_empty());
    let request = Request {
        model: "free/cheap".into(),
        ..Request::default()
    };
    assert_eq!(
        port.complete_stream(
            Need {
                min_tier: Tier::Flagship,
                ..Need::default()
            },
            &request,
            &mut |_| {}
        )
        .unwrap()
        .candidate
        .model,
        "cheap"
    );
    assert_eq!(port.model_profiles().len(), 3);
    assert!(
        port.model_profiles()
            .iter()
            .find(|p| p.candidate.endpoint == "paid")
            .unwrap()
            .unavailable_reason
            .as_ref()
            .unwrap()
            .contains("ceiling")
    );
    assert_eq!(port.records().len(), 1);
    // Diagnostic Router access keeps the existing explicit paid opt-in semantics.
    let paid = router
        .complete_stream(
            Need::default(),
            &Request {
                model: "chosen".into(),
                ..Request::default()
            },
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(paid.candidate.usd(&paid.response.usage), Some(0.000028));
}

#[test]
fn router_falls_back_only_before_events_and_exposes_real_health() {
    let mut router = router::Router::new(None);
    router.add(
        Box::new(Fake {
            name: "bad",
            error: Some(ErrorKind::Auth),
            emit: false,
        }),
        vec![candidate("bad", "m", CostClass::Free)],
    );
    router.add(
        Box::new(Fake {
            name: "ok",
            error: None,
            emit: false,
        }),
        vec![candidate("ok", "m", CostClass::Subscription)],
    );
    let port: &dyn ModelPort = &router;
    assert_eq!(
        port.complete_stream(Need::default(), &Request::default(), &mut |_| {})
            .unwrap()
            .candidate
            .endpoint,
        "ok"
    );
    assert_eq!(port.records().len(), 2);
    assert!(
        port.model_profiles()
            .iter()
            .find(|p| p.candidate.endpoint == "bad")
            .unwrap()
            .unavailable_reason
            .as_ref()
            .unwrap()
            .contains("disabled")
    );
    assert_eq!(
        port.limits()
            .iter()
            .find(|p| p.endpoint == "bad")
            .unwrap()
            .disabled
            .as_deref(),
        Some("failed")
    );

    let mut router = router::Router::new(None);
    router.add(
        Box::new(Fake {
            name: "partial",
            error: Some(ErrorKind::Transient),
            emit: true,
        }),
        vec![candidate("partial", "m", CostClass::Free)],
    );
    router.add(
        Box::new(Fake {
            name: "ok",
            error: None,
            emit: false,
        }),
        vec![candidate("ok", "m", CostClass::Subscription)],
    );
    let mut events = Vec::new();
    assert!(
        ModelPort::complete_stream(&router, Need::default(), &Request::default(), &mut |e| {
            events.push(e)
        })
        .is_err()
    );
    assert_eq!(events, vec![StreamEvent::TextDelta("partial".into())]);
    assert_eq!(router.records().len(), 1);
    assert!(
        router
            .limits()
            .iter()
            .find(|p| p.endpoint == "partial")
            .unwrap()
            .cooldown_until
            .is_some()
    );
}

#[test]
fn missing_terminal_frames_fail_instead_of_reporting_success() {
    for (protocol, first) in [
        (
            Protocol::Chat,
            sse(json!({"choices":[{"delta":{"content":"partial"}}]})),
        ),
        (
            Protocol::Responses,
            sse(json!({"type":"response.output_text.delta","delta":"partial"})),
        ),
        (
            Protocol::Anthropic,
            sse(
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"partial"}}),
            ),
        ),
        (
            Protocol::Ollama,
            format!(
                "{}\n",
                json!({"message":{"content":"partial"},"done":false})
            ),
        ),
    ] {
        let (provider, _sent, ack) = server(protocol, first, String::new(), None);
        ack.send(()).unwrap();
        let error = provider
            .complete_stream(&request(), &mut |_| {})
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Transient);
        assert!(error.message.contains("ended before"));
    }
}

#[test]
fn borrowed_model_port_supports_parallel_scoped_workers() {
    let model = ScriptedModel::new([reply(), reply()]);
    let port: &dyn ModelPort = &model;
    thread::scope(|scope| {
        let workers: Vec<_> = (0..2)
            .map(|_| {
                scope.spawn(move || {
                    port.complete_stream(Need::default(), &Request::default(), &mut |_| {})
                        .unwrap()
                })
            })
            .collect();
        for worker in workers {
            assert_eq!(worker.join().unwrap().response.text(), "answer");
        }
    });
    assert_eq!(model.remaining(), 0);
}

#[test]
fn real_stream_records_upstream_usage_and_reloads_the_ledger() {
    let first = sse(json!({"model":"served","choices":[{"index":0,"delta":{"content":"answer"}}]}));
    let rest = [
        sse(json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})),
        sse(json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":3,"prompt_tokens_details":{"cached_tokens":2}}})),
        "data: [DONE]\r\n\r\n".into(),
    ].concat();
    let (provider, _sent, ack) = server(Protocol::Chat, first, rest, None);
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("usage.jsonl");
    let mut router = router::Router::new(Some(log.clone()));
    router.add(provider, vec![candidate("test", "m", CostClass::Free)]);
    let port: &dyn ModelPort = &router;
    let served = port
        .complete_stream(Need::default(), &Request::default(), &mut |e| {
            if matches!(e, StreamEvent::TextDelta(_)) {
                ack.send(()).unwrap();
            }
        })
        .unwrap();
    assert_eq!(served.response.route, None);
    let records = port.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].served_model.as_deref(), Some("served"));
    assert_eq!(records[0].route, None);
    assert_eq!(
        (records[0].input, records[0].cached_input, records[0].output),
        (Some(10), Some(2), Some(3))
    );
    assert_eq!(records[0].usd, Some(0.0));
    assert_eq!(router::stats(&records)[0].endpoint, "test");
    let reloaded = router::Router::new(Some(log));
    assert_eq!(ModelPort::records(&reloaded).len(), 1);
}

#[test]
fn sse_ignores_comments_and_unknown_events_and_joins_data_lines() {
    let first = concat!(
        ": heartbeat\r\n\r\n",
        "event: ignored\r\ndata: {\"type\":\"future_event\"}\r\n\r\n",
        "event: response.output_text.delta\r\n",
        "data: {\"type\":\"response.output_text.delta\",\r\n",
        "data: \"delta\":\"hello\"}\r\n\r\n",
    )
    .to_owned();
    let rest = sse(
        json!({"type":"response.incomplete","response":{"status":"incomplete","output":[{"type":"message","content":[{"type":"output_text","text":"hello"}]}]}}),
    );
    let (provider, _sent, ack) = server(Protocol::Responses, first, rest, None);
    let mut events = Vec::new();
    let response = provider
        .complete_stream(&request(), &mut |e| {
            ack.send(()).unwrap();
            events.push(e);
        })
        .unwrap();
    assert_eq!(response.text(), "hello");
    assert_eq!(response.stop, Stop::MaxTokens);
    assert_eq!(response.usage, Usage::default());
    assert_eq!(events, vec![StreamEvent::TextDelta("hello".into())]);
}

#[test]
fn stream_errors_keep_provider_classification() {
    for (protocol, frame, kind) in [
        (
            Protocol::Chat,
            sse(json!({"error":{"code":"insufficient_quota","message":"no balance"}})),
            ErrorKind::Quota,
        ),
        (
            Protocol::Chat,
            sse(json!({"error":{"code":"server_error","message":"failed"}})),
            ErrorKind::Transient,
        ),
        (
            Protocol::Responses,
            sse(json!({"type":"error","code":"rate_limit_exceeded","message":"too fast"})),
            ErrorKind::RateLimited,
        ),
        (
            Protocol::Responses,
            sse(
                json!({"type":"response.failed","response":{"status":"failed","error":{"message":"usage limit reached"}}}),
            ),
            ErrorKind::Quota,
        ),
        (
            Protocol::Anthropic,
            sse(json!({"type":"error","error":{"type":"overloaded_error","message":"busy"}})),
            ErrorKind::Transient,
        ),
        (
            Protocol::Ollama,
            format!("{}\n", json!({"error":"unknown model"})),
            ErrorKind::BadRequest,
        ),
    ] {
        let (provider, _sent, _ack) = server(protocol, frame, String::new(), None);
        let error = provider
            .complete_stream(&request(), &mut |_| panic!("error is not a delta"))
            .unwrap_err();
        assert_eq!(error.kind, kind);
    }
}

#[test]
fn provider_default_stream_fallback_is_backward_compatible() {
    struct Legacy;
    impl Provider for Legacy {
        fn name(&self) -> &str {
            "legacy"
        }
        fn list_models(&self) -> Result<Vec<String>, ProviderError> {
            Ok(Vec::new())
        }
        fn complete(&self, _: &Request) -> Result<Response, ProviderError> {
            Ok(reply())
        }
    }
    assert_eq!(
        Legacy
            .complete_stream(&Request::default(), &mut |_| panic!("fake progress"))
            .unwrap()
            .text(),
        "answer"
    );
}

#[test]
fn paid_partial_stream_failure_has_unknown_cost() {
    for emit in [false, true] {
        let mut router = router::Router::new(None);
        router.add(
            Box::new(Fake {
                name: "paid",
                error: Some(ErrorKind::Transient),
                emit,
            }),
            vec![candidate("paid", "m", CostClass::Paid)],
        );
        assert!(
            router
                .complete_stream(Need::default(), &Request::default(), &mut |_| {})
                .is_err()
        );
        assert_eq!(
            router.records()[0].usd,
            None,
            "an interrupted paid generation can be billed without reported usage, even when it only generated tool arguments"
        );
    }
}

#[test]
fn cancelled_request_does_not_start_or_consume_legacy_port_reply() {
    use std::sync::{Arc, atomic::AtomicBool};
    let model = ScriptedModel::new([reply()]);
    let request = Request {
        cancel: Some(Arc::new(AtomicBool::new(true))),
        ..Request::default()
    };
    let error = model
        .complete_stream(Need::default(), &request, &mut |_| {
            panic!("cancelled output")
        })
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Cancelled);
    assert_eq!(model.remaining(), 1);
    assert!(model.requests().is_empty());
}

#[test]
fn cancelling_an_idle_stream_closes_the_socket_without_waiting_for_data() {
    use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
    for protocol in [Protocol::Chat, Protocol::Responses, Protocol::Anthropic, Protocol::Ollama] {
        for headers_sent in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let (ready, waiting) = mpsc::channel();
            let (closed, disconnected) = mpsc::channel();
            let server = thread::spawn(move || loop {
                let (mut socket, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(socket.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let show = line.contains("/api/show");
                let mut length = 0;
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" { break; }
                    if let Some((key, value)) = line.split_once(':') {
                        if key.eq_ignore_ascii_case("content-length") { length = value.trim().parse().unwrap(); }
                    }
                }
                reader.read_exact(&mut vec![0; length]).unwrap();
                if show {
                    socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").unwrap();
                    continue;
                }
                if headers_sent {
                    // An incomplete UTF-8/JSON line must also remain interruptible.
                    socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10000\r\nConnection: close\r\n\r\ndata: {\"partial\":\"").unwrap();
                }
                ready.send(()).unwrap();
                socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                closed.send(matches!(socket.read(&mut [0; 1]), Ok(0))).unwrap();
                break;
            });
            let provider = connect(Endpoint { name: "test".into(), protocol, base_url: endpoint, api_key: None, headers: vec![] });
            let flag = Arc::new(AtomicBool::new(false));
            let request = Request { cancel: Some(flag.clone()), timeout: Some(Duration::from_secs(3)), ..request() };
            let (done, result) = mpsc::channel();
            let worker = thread::spawn(move || { let _ = done.send(provider.complete_stream(&request, &mut |_| panic!("incomplete frame emitted"))); });
            waiting.recv_timeout(Duration::from_secs(1)).unwrap();
            thread::sleep(Duration::from_millis(30));
            flag.store(true, Ordering::Release);
            let error = result.recv_timeout(Duration::from_millis(350)).expect("Esc waited for an idle HTTP read").unwrap_err();
            assert_eq!(error.kind, ErrorKind::Cancelled);
            assert!(disconnected.recv_timeout(Duration::from_millis(350)).unwrap(), "cancelled inference connection remained open");
            worker.join().unwrap();
            server.join().unwrap();
        }
    }
}

#[test]
fn cancellation_stops_all_wire_streams_without_fallback_or_cooldown() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    for (protocol, first) in [
        (
            Protocol::Chat,
            sse(json!({"choices":[{"index":0,"delta":{"content":"partial"}}]})),
        ),
        (
            Protocol::Responses,
            sse(json!({"type":"response.output_text.delta","delta":"partial"})),
        ),
        (
            Protocol::Anthropic,
            sse(
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"partial"}}),
            ),
        ),
        (
            Protocol::Ollama,
            format!(
                "{}\n",
                json!({"message":{"content":"partial"},"done":false})
            ),
        ),
    ] {
        let (provider, _sent, _ack) = server(protocol, first, String::new(), None);
        let mut router = router::Router::new(None);
        router.add(provider, vec![candidate("test", "m", CostClass::Free)]);
        router.add(
            Box::new(Fake {
                name: "fallback",
                error: None,
                emit: false,
            }),
            vec![candidate("fallback", "m", CostClass::Subscription)],
        );
        let flag = Arc::new(AtomicBool::new(false));
        let request = Request {
            cancel: Some(flag.clone()),
            ..request()
        };
        let mut events = Vec::new();
        let error = ModelPort::complete_stream(&router, Need::default(), &request, &mut |e| {
            if matches!(e, StreamEvent::TextDelta(_)) {
                flag.store(true, Ordering::Release);
            }
            events.push(e);
        })
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Cancelled);
        assert_eq!(events, vec![StreamEvent::TextDelta("partial".into())]);
        assert_eq!(router.records().len(), 1);
        assert!(
            router
                .limits()
                .iter()
                .all(|l| l.cooldown_until.is_none() && l.disabled.is_none())
        );
    }
}
