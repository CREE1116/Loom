use crate::{
    agent::{AgentBackend, AgentUpdate},
    runtime,
    transport::{Client, Event as RpcEvent},
};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

pub fn probe(cwd: &std::path::Path, codex_bin: &str, endpoint: Option<&str>) -> Result<()> {
    let info = match endpoint {
        Some(endpoint) => endpoint.to_owned(),
        None => runtime::ensure(cwd, codex_bin)?.endpoint,
    };
    let mut first = Client::connect(&info)?;
    rpc(
        &mut first,
        "initialize",
        json!({"clientInfo":{"name":"custom_tui_probe","version":"0.1.0"},"capabilities":{"experimentalApi":true}}),
    )?;
    first.send(json!({"method":"initialized"}))?;
    let result = rpc(
        &mut first,
        "thread/start",
        json!({"cwd":cwd.to_string_lossy(),"historyMode":"legacy"}),
    )?;
    let id = result["thread"]["id"]
        .as_str()
        .context("No thread ID returned")?
        .to_owned();
    let mut second = Client::connect(&info)?;
    rpc(
        &mut second,
        "initialize",
        json!({"clientInfo":{"name":"custom_tui_probe_viewer","version":"0.1.0"},"capabilities":{"experimentalApi":true}}),
    )?;
    second.send(json!({"method":"initialized"}))?;
    let read = rpc(
        &mut second,
        "thread/read",
        json!({"threadId":id,"includeTurns":false}),
    )?;
    if read["thread"]["id"] != id {
        bail!("Two clients read different threads");
    }
    drop(first);
    drop(second);
    let mut third = Client::connect(&info)?;
    rpc(
        &mut third,
        "initialize",
        json!({"clientInfo":{"name":"custom_tui_probe_reconnect","version":"0.1.0"},"capabilities":{"experimentalApi":true}}),
    )?;
    third.send(json!({"method":"initialized"}))?;
    let read = rpc(
        &mut third,
        "thread/read",
        json!({"threadId":id,"includeTurns":false}),
    )?;
    if read["thread"]["id"] != id {
        bail!("Reconnection changed the thread ID");
    }
    let mut backend = super::codex::CodexBackend::connect(
        &info,
        super::codex::Options {
            cwd: cwd.into(),
            model: None,
            initial_session: Some(id.clone()),
            readonly: true,
            auto_restore: false,
        },
    )?;
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        if let Some(event) = backend.poll()?
            && let AgentUpdate::SessionLoaded { snapshot, .. } = event.update
        {
            if snapshot.id != id {
                bail!("Domain backend changed the session ID");
            }
            break;
        }
        if Instant::now() >= deadline {
            bail!("Domain backend handshake timed out");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    println!(
        "Real Codex handshake OK; shared two-client metadata OK; connection reopen OK; AgentEvent backend OK. No inference or tool execution.\nRuntime: {info}\nThread: {id}"
    );
    Ok(())
}
fn rpc(client: &mut Client, method: &str, params: Value) -> Result<Value> {
    let id = client.request(method, params)?;
    let deadline = Instant::now() + Duration::from_secs(25);
    while Instant::now() < deadline {
        match client.incoming.recv_timeout(Duration::from_millis(200)) {
            Ok(RpcEvent::Message(message)) if message["id"].as_u64() == Some(id) => {
                if !message["error"].is_null() {
                    bail!("{method}: {}", message["error"]);
                }
                return Ok(message["result"].clone());
            }
            Ok(RpcEvent::Disconnected(message)) => bail!("{message}"),
            _ => {}
        }
    }
    bail!("{method} timed out")
}
pub fn list_models(cwd: &std::path::Path, codex_bin: &str, endpoint: Option<&str>) -> Result<()> {
    let endpoint = match endpoint {
        Some(endpoint) => endpoint.to_owned(),
        None => runtime::ensure(cwd, codex_bin)?.endpoint,
    };
    let mut client = Client::connect(&endpoint)?;
    rpc(
        &mut client,
        "initialize",
        json!({"clientInfo":{"name":"custom_tui_models","version":"0.1.0"}}),
    )?;
    client.send(json!({"method":"initialized"}))?;
    let mut cursor = Value::Null;
    loop {
        let result = rpc(
            &mut client,
            "model/list",
            json!({"cursor":cursor,"limit":100}),
        )?;
        if let Some(models) = result["data"].as_array() {
            for model in models {
                println!(
                    "{}{}",
                    model["model"]
                        .as_str()
                        .or_else(|| model["id"].as_str())
                        .unwrap_or("unknown"),
                    if model["isDefault"] == true {
                        " (default)"
                    } else {
                        ""
                    }
                );
            }
        }
        cursor = result["nextCursor"].clone();
        if cursor.is_null() {
            break;
        }
    }
    Ok(())
}

pub fn list_skills(cwd: &std::path::Path, codex_bin: &str, endpoint: Option<&str>) -> Result<()> {
    let endpoint = match endpoint {
        Some(endpoint) => endpoint.to_owned(),
        None => runtime::ensure(cwd, codex_bin)?.endpoint,
    };
    let mut client = Client::connect(&endpoint)?;
    rpc(
        &mut client,
        "initialize",
        json!({"clientInfo":{"name":"custom_tui_skills","version":"0.1.0"}}),
    )?;
    client.send(json!({"method":"initialized"}))?;
    let result = rpc(
        &mut client,
        "skills/list",
        json!({"cwds":[cwd],"forceReload":true}),
    )?;
    let (skills, errors) = super::codec::skills(&result);
    for skill in skills {
        println!("${}\t{}", skill.name, skill.path);
    }
    for error in errors {
        eprintln!("{error}");
    }
    Ok(())
}
