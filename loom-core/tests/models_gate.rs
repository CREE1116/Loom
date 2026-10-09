mod isolation;
use loom_core::models::*;
use serde_json::json;
use std::time::Duration;

fn response(parts: Vec<Part>, stop: Stop) -> Response {
    Response {
        parts,
        stop,
        usage: Usage::default(),
        model: "scripted".into(),
        latency: Duration::ZERO,
        route: None,
        gateway_connection: None,
        gateway_upstream_ms: None,
    }
}

#[test]
fn scripted_model_round_trips_tool_calls_and_results() {
    let call = Part::ToolCall {
        id: "read-1".into(),
        name: "read".into(),
        arguments: json!({"path":"lib.rs"}),
    };
    let model = ScriptedModel::new([
        response(vec![call], Stop::ToolUse),
        response(vec![Part::Text("proposal ready".into())], Stop::EndTurn),
    ]);
    let mut request = Request {
        messages: vec![Message::user("fix the bug")],
        ..Request::default()
    };
    let first = model.complete(Need::default(), &request).unwrap();
    request.messages.push(Message {
        role: Role::Assistant,
        parts: first.response.parts,
    });
    request.messages.push(Message {
        role: Role::User,
        parts: vec![Part::ToolResult {
            call_id: "read-1".into(),
            output: "source".into(),
            is_error: false,
        }],
    });
    assert_eq!(
        model
            .complete(Need::default(), &request)
            .unwrap()
            .response
            .text(),
        "proposal ready"
    );
    assert_eq!(model.requests()[1].messages, request.messages);
    assert_eq!(model.remaining(), 0);
    assert!(model.complete(Need::default(), &request).is_err());
}

#[test]
fn worker_port_cannot_override_cost_ceiling() {
    let mut router = router::Router::new(None);
    struct Paid;
    impl Provider for Paid {
        fn name(&self) -> &str {
            "paid"
        }
        fn complete(&self, _: &Request) -> Result<Response, ProviderError> {
            panic!("paid provider must never be invoked")
        }
        fn list_models(&self) -> Result<Vec<String>, ProviderError> {
            Ok(Vec::new())
        }
    }
    router.add(
        Box::new(Paid),
        vec![router::Candidate {
            endpoint: "paid".into(),
            model: "expensive".into(),
            tier: Tier::Flagship,
            cost: CostClass::Paid,
            price: Some((1.0, 1.0)),
            tools: true,
            context: None,
        }],
    );
    let port: &dyn ModelPort = &router;
    assert!(
        port.complete(
            Need {
                max_cost: CostClass::Unknown,
                ..Need::default()
            },
            &Request::default()
        )
        .is_err()
    );
    assert!(router.records().is_empty());
}

#[test]
fn loom_auto_routing_ignores_explicit_only_gateway_auto() {
    struct NamedProvider(&'static str);
    impl Provider for NamedProvider {
        fn name(&self) -> &str {
            self.0
        }
        fn complete(&self, request: &Request) -> Result<Response, ProviderError> {
            Ok(Response {
                parts: vec![Part::Text(self.0.into())],
                model: request.model.clone(),
                ..response(Vec::new(), Stop::EndTurn)
            })
        }
        fn list_models(&self) -> Result<Vec<String>, ProviderError> {
            Ok(Vec::new())
        }
    }

    let mut router = router::Router::new(None);
    router.add(
        Box::new(NamedProvider("local")),
        vec![router::Candidate {
            endpoint: "local".into(),
            model: "small".into(),
            tier: Tier::Small,
            cost: CostClass::Local,
            price: Some((0.0, 0.0)),
            tools: true,
            context: None,
        }],
    );
    router.add(Box::new(NamedProvider("gateway")), Vec::new());
    router.add_selectable(vec![router::Candidate {
        endpoint: "gateway".into(),
        model: "auto".into(),
        tier: Tier::Medium,
        cost: CostClass::Free,
        price: Some((0.0, 0.0)),
        tools: true,
        context: None,
    }]);

    let automatic = router
        .complete(
            Need {
                tools: true,
                max_cost: CostClass::Subscription,
                ..Need::default()
            },
            &Request::default(),
        )
        .unwrap();
    assert_eq!(automatic.candidate.endpoint, "local");
    assert_eq!(automatic.response.text(), "local");

    let explicit = router
        .complete(
            Need {
                tools: true,
                max_cost: CostClass::Subscription,
                ..Need::default()
            },
            &Request {
                model: "gateway/auto".into(),
                ..Request::default()
            },
        )
        .unwrap();
    assert_eq!(explicit.candidate.endpoint, "gateway");
    assert_eq!(explicit.response.text(), "gateway");
}

#[test]
fn forbidden_gateway_route_is_rejected_and_cooled_down() {
    struct Gateway;
    impl Provider for Gateway {
        fn name(&self) -> &str {
            "gateway"
        }
        fn complete(&self, _: &Request) -> Result<Response, ProviderError> {
            let mut reply = response(
                vec![Part::Text("reject this response".into())],
                Stop::EndTurn,
            );
            reply.route = Some("cc".into());
            Ok(reply)
        }
        fn list_models(&self) -> Result<Vec<String>, ProviderError> {
            Ok(Vec::new())
        }
    }
    let mut router = router::Router::new(None);
    router.add(
        Box::new(Gateway),
        vec![router::Candidate {
            endpoint: "gateway".into(),
            model: "auto".into(),
            tier: Tier::Medium,
            cost: CostClass::Free,
            price: Some((0.0, 0.0)),
            tools: true,
            context: None,
        }],
    );
    assert!(
        router
            .complete(Need::default(), &Request::default())
            .unwrap_err()
            .message
            .contains("blocked")
    );
    assert!(
        router.plan(Need::default())[0]
            .1
            .as_deref()
            .unwrap()
            .contains("cooling down")
    );
    assert!(!router.records()[0].ok);
    assert_eq!(router.records()[0].route.as_deref(), Some("cc"));
    for route in [
        "cc",
        "Claude-Code",
        "antigravity",
        "AG",
        "codex_oauth",
        "chatgpt-oauth",
        "agy",
        "grok-cli",
        "xai-oauth",
        "claude",
        "codex",
    ] {
        // the built-in list; a user may still allow a route in ~/.loom/routes.toml
        assert!(blocked_by_default(route), "{route}");
    }
    for route in ["gemini", "groq", "openrouter", "moonshot"] {
        assert!(!blocked_route(route) && !blocked_by_default(route), "{route}");
    }
}

#[test]
fn gateway_reply_without_a_reported_upstream_is_refused() {
    // A gateway that should name its upstream but does not could be relaying a
    // subscription login, so the router must not accept the reply.
    struct Silent;
    impl Provider for Silent {
        fn name(&self) -> &str {
            "gateway"
        }
        fn reports_route(&self) -> bool {
            true
        }
        fn complete(&self, _: &Request) -> Result<Response, ProviderError> {
            Ok(response(vec![Part::Text("who answered?".into())], Stop::EndTurn))
        }
        fn list_models(&self) -> Result<Vec<String>, ProviderError> {
            Ok(Vec::new())
        }
    }
    let mut router = router::Router::new(None);
    router.add(
        Box::new(Silent),
        vec![router::Candidate {
            endpoint: "gateway".into(),
            model: "auto".into(),
            tier: Tier::Medium,
            cost: CostClass::Free,
            price: Some((0.0, 0.0)),
            tools: true,
            context: None,
        }],
    );
    let error = router.complete(Need::default(), &Request::default()).unwrap_err();
    assert!(error.message.contains("unverified gateway route"), "{}", error.message);
    assert!(!router.records()[0].ok);
}

#[test]
fn a_blocking_model_call_stops_as_soon_as_the_user_cancels() {
    // Gateways are called without streaming (to learn the upstream route); such a call
    // must still end on Esc instead of waiting for the reply or the timeout.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        let (_socket, _) = listener.accept().unwrap();
        std::thread::sleep(Duration::from_secs(30)); // never answers in time
    });
    let provider = loom_core::models::chat::ChatProvider::new(Endpoint {
        name: "slow".into(),
        protocol: Protocol::Chat,
        base_url: base,
        api_key: None,
        headers: vec![],
    });
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    let started = std::time::Instant::now();
    let error = provider
        .complete(&Request {
            model: "m".into(),
            messages: vec![Message::user("hi")],
            timeout: Some(Duration::from_secs(60)),
            cancel: Some(cancel),
            ..Request::default()
        })
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Cancelled, "{error:?}");
    assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
}
