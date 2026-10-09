//! Call one model endpoint with a tool and print the round trip.
//! `cargo run --example model_probe -- <chat|responses|anthropic> <base_url> <model> [api_key_env]`
use loom_tui::core::model::*;
use serde_json::json;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let protocol = match args.first().map(String::as_str) {
        Some("responses") => Protocol::Responses,
        Some("anthropic") => Protocol::Anthropic,
        _ => Protocol::Chat,
    };
    let provider = connect(Endpoint {
        name: "probe".into(),
        protocol,
        base_url: args
            .get(1)
            .cloned()
            .unwrap_or_else(|| "http://127.0.0.1:11434/v1".into()),
        api_key: args.get(3).and_then(|name| std::env::var(name).ok()),
        headers: vec![],
    });
    let model = args.get(2).cloned().unwrap_or_default();
    println!(
        "models: {:?}",
        provider
            .list_models()
            .map(|m| m.into_iter().take(5).collect::<Vec<_>>())
    );
    let tool = ToolSpec {
        name: "get_weather".into(),
        description: "Current weather for a city".into(),
        parameters: json!({"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}),
    };
    let mut request = Request {
        model,
        system: Some("Use tools when they help. Answer in one short sentence.".into()),
        messages: vec![Message::user("What's the weather in Seoul right now?")],
        tools: vec![tool],
        max_output_tokens: Some(300),
        temperature: None,
        reasoning_effort: None,
        timeout: None,
        gateway_connection: None,
        cancel: None,
    };
    let first = provider.complete(&request)?;
    println!(
        "1) stop={:?} model={} usage={:?} latency={:?}\n   calls={:?}\n   text={:?}",
        first.stop,
        first.model,
        first.usage,
        first.latency,
        first.tool_calls().collect::<Vec<_>>(),
        first.text()
    );
    let Some((id, _, _)) = first.tool_calls().next() else {
        return Ok(());
    };
    let id = id.to_owned();
    request.messages.push(Message {
        role: Role::Assistant,
        parts: first.parts.clone(),
    });
    request.messages.push(Message {
        role: Role::User,
        parts: vec![Part::ToolResult {
            call_id: id,
            output: "{\"sky\":\"clear\",\"celsius\":21}".into(),
            is_error: false,
        }],
    });
    let second = provider.complete(&request)?;
    println!(
        "2) stop={:?} usage={:?} latency={:?}\n   text={:?}",
        second.stop,
        second.usage,
        second.latency,
        second.text()
    );
    Ok(())
}
