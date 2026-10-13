//! Browser sign-in for HTTP MCP servers that require OAuth (e.g. mcp.atlassian.com).
//!
//! ```toml
//! [mcp_servers.atlassian]
//! url = "https://mcp.atlassian.com/v1/mcp"
//! oauth = true
//! ```
//! `loom mcp login atlassian` discovers the authorization server (RFC 9728, then
//! RFC 8414 at the MCP origin), registers a public client (RFC 7591), runs the PKCE
//! authorization-code flow through a loopback callback, and stores the grant in
//! `<state>/mcp-oauth/` (0600, keyed by server URL). Extensions attach the bearer
//! and refresh it shortly before expiry.
use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use url::Url;

const CALLBACK: &str = "/callback";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const REFRESH_MARGIN: u64 = 60;

#[derive(Serialize, Deserialize)]
struct Grant {
    url: String,
    token_endpoint: String,
    client_id: String,
    client_secret: Option<String>,
    access_token: String,
    refresh_token: Option<String>,
    expires_at: u64,
}

fn store_path(url: &str) -> Result<PathBuf> {
    let root = crate::models::state_dir().context("Loom state directory unavailable")?;
    let key = hex(&Sha256::digest(url.as_bytes())[..12]);
    Ok(root.join("mcp-oauth").join(format!("{key}.json")))
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
fn random() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|_| anyhow!("secure randomness unavailable"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn load(url: &str) -> Result<Option<Grant>> {
    let path = store_path(url)?;
    match std::fs::read(&path) {
        Ok(bytes) => {
            let grant: Grant = serde_json::from_slice(&bytes).context("corrupt MCP OAuth grant")?;
            ensure!(
                grant.url == url,
                "MCP OAuth grant belongs to a different URL"
            );
            Ok(Some(grant))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).context("read MCP OAuth grant"),
    }
}
fn save(grant: &Grant) -> Result<()> {
    let path = store_path(&grant.url)?;
    let dir = path.parent().unwrap();
    std::fs::create_dir_all(dir)?;
    // tempfile creates 0600 files on Unix; rename keeps the replacement atomic.
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    file.write_all(&serde_json::to_vec_pretty(grant)?)?;
    file.persist(&path).context("save MCP OAuth grant")?;
    Ok(())
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .try_proxy_from_env(false)
        .redirects(0)
        .timeout(HTTP_TIMEOUT)
        .build()
}
fn json(result: std::result::Result<ureq::Response, ureq::Error>, what: &str) -> Result<Value> {
    match result {
        Ok(response) => response
            .into_json()
            .with_context(|| format!("{what}: invalid JSON")),
        Err(ureq::Error::Status(code, response)) => {
            let body = response.into_string().unwrap_or_default();
            let detail: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let reason = detail["error"].as_str().unwrap_or("");
            bail!("{what}: HTTP {code} {reason}")
        }
        Err(ureq::Error::Transport(t)) => bail!("{what}: {}", t.kind()),
    }
}
fn https(endpoint: &str) -> Result<String> {
    let url = Url::parse(endpoint).with_context(|| format!("invalid endpoint {endpoint}"))?;
    let loopback = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    ensure!(
        url.scheme() == "https" || (url.scheme() == "http" && loopback),
        "OAuth endpoint must use https: {endpoint}"
    );
    Ok(url.into())
}

/// Authorization server metadata for an MCP server URL, plus the resource's own
/// `scopes_supported` (RFC 9728) — the authorization server lists every scope it has.
fn metadata(server: &str) -> Result<(Value, Vec<String>)> {
    let server = Url::parse(server)?;
    let origin = server.origin().ascii_serialization();
    let path = server.path().trim_end_matches('/');
    let mut issuer = origin.clone();
    let mut scopes = Vec::new();
    for candidate in [
        format!("{origin}/.well-known/oauth-protected-resource{path}"),
        format!("{origin}/.well-known/oauth-protected-resource"),
    ] {
        if let Ok(resource) = json(agent().get(&candidate).call(), "resource metadata")
            && let Some(found) = resource["authorization_servers"][0].as_str()
        {
            issuer = https(found)?.trim_end_matches('/').to_owned();
            scopes = resource["scopes_supported"]
                .as_array()
                .map(|s| {
                    s.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            break;
        }
    }
    let issuer = Url::parse(&issuer)?;
    let origin = issuer.origin().ascii_serialization();
    let path = issuer.path().trim_end_matches('/');
    for candidate in [
        format!("{origin}/.well-known/oauth-authorization-server{path}"),
        format!("{origin}/.well-known/openid-configuration{path}"),
    ] {
        if let Ok(meta) = json(agent().get(&candidate).call(), "authorization metadata") {
            return Ok((meta, scopes));
        }
    }
    bail!("{} does not publish OAuth authorization metadata", issuer)
}

fn token(endpoint: &str, grant: &Grant, form: &[(&str, &str)]) -> Result<Value> {
    let mut form = form.to_vec();
    form.extend([
        ("client_id", grant.client_id.as_str()),
        ("resource", grant.url.as_str()),
    ]);
    if let Some(secret) = &grant.client_secret {
        form.push(("client_secret", secret));
    }
    json(agent().post(endpoint).send_form(&form), "token request")
}
fn install(grant: &mut Grant, response: &Value) -> Result<()> {
    grant.access_token = response["access_token"]
        .as_str()
        .filter(|t| !t.is_empty() && t.bytes().all(|b| (0x21..=0x7e).contains(&b)))
        .context("token response missing access_token")?
        .to_owned();
    // Servers may rotate refresh tokens; keep the old one when none is returned.
    if let Some(refresh) = response["refresh_token"].as_str() {
        grant.refresh_token = Some(refresh.to_owned());
    }
    grant.expires_at = now() + response["expires_in"].as_u64().unwrap_or(3600);
    Ok(())
}

/// Current bearer token for `url`, refreshed when it is about to expire.
// ponytail: no cross-process lock; two workers refreshing at once can race on a
// rotating refresh token — add an fs2 lock around refresh if that shows up.
pub fn bearer(url: &str) -> Result<String> {
    let mut grant =
        load(url)?.ok_or_else(|| anyhow!("not signed in; run `loom mcp login <name>`"))?;
    if grant.expires_at > now() + REFRESH_MARGIN {
        return Ok(grant.access_token);
    }
    let refresh = grant
        .refresh_token
        .clone()
        .ok_or_else(|| anyhow!("MCP OAuth session expired; run `loom mcp login <name>`"))?;
    let endpoint = grant.token_endpoint.clone();
    let response = token(
        &endpoint,
        &grant,
        &[("grant_type", "refresh_token"), ("refresh_token", &refresh)],
    )
    .context("MCP OAuth refresh failed; run `loom mcp login <name>`")?;
    install(&mut grant, &response)?;
    save(&grant)?;
    Ok(grant.access_token)
}

/// Interactive sign-in. `open_browser` receives the authorization URL.
pub fn login(url: &str, open_browser: &mut dyn FnMut(&str) -> Result<()>) -> Result<()> {
    let (meta, scopes) = metadata(url)?;
    let authorize = https(
        meta["authorization_endpoint"]
            .as_str()
            .context("metadata missing authorization_endpoint")?,
    )?;
    let token_endpoint = https(
        meta["token_endpoint"]
            .as_str()
            .context("metadata missing token_endpoint")?,
    )?;
    let register = https(
        meta["registration_endpoint"]
            .as_str()
            .context("server does not support dynamic client registration")?,
    )?;

    let listener =
        TcpListener::bind("127.0.0.1:0").context("OAuth callback listener unavailable")?;
    listener.set_nonblocking(true)?;
    let redirect = format!(
        "http://127.0.0.1:{}{CALLBACK}",
        listener.local_addr()?.port()
    );

    let client = json(
        agent().post(&register).send_json(serde_json::json!({
            "client_name": "Loom",
            "redirect_uris": [redirect],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
        })),
        "client registration",
    )?;
    let mut grant = Grant {
        url: url.to_owned(),
        token_endpoint,
        client_id: client["client_id"]
            .as_str()
            .context("registration missing client_id")?
            .to_owned(),
        client_secret: client["client_secret"].as_str().map(str::to_owned),
        access_token: String::new(),
        refresh_token: None,
        expires_at: 0,
    };

    let state = random()?;
    let verifier = random()?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let mut target = Url::parse(&authorize)?;
    target.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("client_id", grant.client_id.as_str()),
        ("redirect_uri", redirect.as_str()),
        ("state", state.as_str()),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("resource", url),
    ]);
    if !scopes.is_empty() {
        target
            .query_pairs_mut()
            .append_pair("scope", &scopes.join(" "));
    }
    open_browser(target.as_str())?;

    let code = wait_for_code(&listener, &state)?;
    let response = token(
        &grant.token_endpoint.clone(),
        &grant,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("code_verifier", &verifier),
            ("redirect_uri", &redirect),
        ],
    )?;
    install(&mut grant, &response)?;
    save(&grant)
}

fn wait_for_code(listener: &TcpListener, state: &str) -> Result<String> {
    let deadline = Instant::now() + LOGIN_TIMEOUT;
    loop {
        ensure!(Instant::now() < deadline, "MCP sign-in timed out");
        let mut stream = match listener.accept() {
            Ok((stream, peer)) if peer.ip().is_loopback() => stream,
            Ok(_) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            Err(e) => return Err(e).context("OAuth callback listener failed"),
        };
        let Ok(target) = request_target(&mut stream) else {
            continue;
        };
        // Browsers also ask for /favicon.ico; only the callback path consumes the attempt.
        if !target.starts_with(CALLBACK) {
            respond(&mut stream, "404 Not Found", "");
            continue;
        }
        let url = Url::parse(&format!("http://127.0.0.1{target}"))?;
        let param = |name: &str| {
            url.query_pairs()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.into_owned())
        };
        let result = if param("state").as_deref() != Some(state) {
            Err(anyhow!("OAuth callback state did not match"))
        } else if let Some(error) = param("error") {
            Err(anyhow!("authorization was declined: {error}"))
        } else {
            param("code")
                .filter(|c| !c.is_empty())
                .ok_or_else(|| anyhow!("missing authorization code"))
        };
        respond(
            &mut stream,
            if result.is_ok() {
                "200 OK"
            } else {
                "400 Bad Request"
            },
            if result.is_ok() {
                "Loom is connected. You can close this window."
            } else {
                "Loom could not complete this sign-in. Return to Loom and try again."
            },
        );
        return result;
    }
}
fn request_target(stream: &mut TcpStream) -> Result<String> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 1024];
    while !bytes.windows(4).any(|w| w == b"\r\n\r\n") {
        ensure!(bytes.len() <= 16 * 1024, "OAuth callback too large");
        let n = stream.read(&mut chunk)?;
        ensure!(n != 0, "incomplete OAuth callback");
        bytes.extend_from_slice(&chunk[..n]);
    }
    let head = std::str::from_utf8(&bytes)?;
    let mut line = head.lines().next().unwrap_or("").split_whitespace();
    ensure!(
        line.next() == Some("GET"),
        "unexpected OAuth callback method"
    );
    Ok(line
        .next()
        .context("missing OAuth callback target")?
        .to_owned())
}
fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

pub fn signed_in(url: &str) -> bool {
    load(url).is_ok_and(|grant| grant.is_some())
}

pub fn logout(url: &str) -> Result<bool> {
    match std::fs::remove_file(store_path(url)?) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).context("remove MCP OAuth grant"),
    }
}

/// `loom mcp login|logout <name>` against the current directory's `.loom/config.toml`.
pub fn cli(
    args: &[String],
    root: &Path,
    open_browser: &mut dyn FnMut(&str) -> Result<()>,
) -> Result<()> {
    let usage = "usage: loom mcp add <preset> [url] | login <name> | logout <name>";
    crate::extensions::clear_discovery_cache();
    let (Some(action), Some(name)) = (args.first(), args.get(1)) else {
        bail!(usage)
    };
    if action == "add" {
        crate::extensions::add_preset(name, args.get(2).map(String::as_str))?;
        println!("Added {name} to the global config.");
        if let Ok(url) = crate::extensions::oauth_url(root, name) {
            login(&url, open_browser)?;
            println!("Signed in to MCP server {name}.");
        }
        return Ok(());
    }
    let url = crate::extensions::oauth_url(root, name)?;
    match action.as_str() {
        "login" => {
            login(&url, open_browser)?;
            println!("Signed in to MCP server {name}.");
        }
        "logout" => {
            let removed = logout(&url)?;
            println!(
                "{}",
                if removed {
                    format!("Signed out of {name}.")
                } else {
                    format!("{name} was not signed in.")
                }
            );
        }
        _ => bail!(usage),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn callback(state: &str, requests: &[&str]) -> Result<String> {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let requests: Vec<String> = requests.iter().map(|r| r.to_string()).collect();
        let browser = std::thread::spawn(move || {
            for target in requests {
                let mut stream = TcpStream::connect(address).unwrap();
                write!(stream, "GET {target} HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
                let mut reply = String::new();
                let _ = stream.read_to_string(&mut reply);
            }
        });
        let result = wait_for_code(&listener, state);
        browser.join().unwrap();
        result
    }

    #[test]
    fn callback_ignores_stray_requests_and_checks_state() {
        let code = callback("s1", &["/favicon.ico", "/callback?state=s1&code=abc"]).unwrap();
        assert_eq!(code, "abc");
        let forged = callback("s1", &["/callback?state=other&code=abc"]).unwrap_err();
        assert!(forged.to_string().contains("state"), "{forged}");
        let denied = callback("s1", &["/callback?state=s1&error=access_denied"]).unwrap_err();
        assert!(denied.to_string().contains("access_denied"), "{denied}");
    }
}
