//! Official public-client Sign in with ChatGPT; never reads another app's tokens.
//!
//! Protocol references (checked 2026-10-08):
//! https://developers.openai.com/siwc/token-sharing-open-source/sign-in
//! https://developers.openai.com/siwc/token-sharing-open-source/models-and-inference
//! https://developers.openai.com/siwc/token-sharing-open-source/profiles-and-sessions
//! https://developers.openai.com/siwc/token-sharing-open-source/token-reference
//! Unix storage fails closed on unsafe paths. Other platforms need a private ACL store.
use super::{ErrorKind, Provider, ProviderError, Request, Response, StreamEvent};
use anyhow::{Result, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use url::Url;

const ISSUER: &str = "https://auth.openai.com";
const RESOURCE: &str = "https://api.openai.com/v1";
const AUTHORIZE: &str = "https://auth.openai.com/api/accounts/authorize";
const TOKEN: &str = "https://auth.openai.com/api/accounts/oauth/token";
const DISCOVERY: &str = "https://auth.openai.com/.well-known/openid-configuration";
const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
const CALLBACK: &str = "/auth/callback";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(180);
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const REFRESH_MARGIN: u64 = 120;
const MAX_JSON: u64 = 512 * 1024;

// Deliberately no Debug: tokens, callback codes, and authorization URLs are secrets.
#[derive(Serialize, Deserialize)]
struct Credentials {
    version: u8,
    issuer: String,
    subject: String,
    client_id: String,
    ext_agent_host_id: String,
    email: Option<String>,
    #[serde(default)]
    id_token: String,
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    scopes: String,
    #[serde(default)]
    expires_at: u64,
    #[serde(default)]
    refresh_expires_at: u64,
    #[serde(default)]
    earliest_refresh_at: u64,
    #[serde(default)]
    nonce: String,
    signing_keys: JwkSet,
    #[serde(default = "empty_keys")]
    id_signing_keys: JwkSet,
}

fn empty_keys() -> JwkSet {
    JwkSet { keys: Vec::new() }
}

impl Credentials {
    fn check_registration(&self) -> Result<()> {
        ensure!(
            self.version == 1 && self.issuer == ISSUER,
            "invalid ChatGPT registration"
        );
        issued_client(&self.client_id)?;
        ensure!(
            !self.subject.is_empty() && !self.ext_agent_host_id.is_empty(),
            "invalid ChatGPT identity"
        );
        Ok(())
    }
    fn renewable(&self, now: u64) -> bool {
        self.check_registration().is_ok()
            && self.locally_verified()
            && plan_scope(&self.scopes)
            && bearer(&self.access_token)
            && !self.refresh_token.is_empty()
            && self.refresh_expires_at > now
            && (self.expires_at > now || self.earliest_refresh_at <= now)
    }
    fn locally_verified(&self) -> bool {
        let id = verified_at(
            &self.id_token,
            &self.client_id,
            if self.id_signing_keys.keys.is_empty() {
                &self.signing_keys
            } else {
                &self.id_signing_keys
            },
            false,
        );
        let access = verified_at(&self.access_token, RESOURCE, &self.signing_keys, false);
        match (id, access) {
            (Ok(id), Ok(access)) => {
                id["sub"] == self.subject
                    && !self.nonce.is_empty()
                    && id["nonce"].as_str().is_none_or(|n| equal(n, &self.nonce))
                    && access["client_id"] == self.client_id
                    && access["exp"]
                        .as_u64()
                        .is_some_and(|exp| self.expires_at <= exp)
                    && access["scope"].as_str().is_some_and(plan_scope)
            }
            _ => false,
        }
    }
    fn clear_tokens(&mut self) {
        self.id_token.clear();
        self.access_token.clear();
        self.refresh_token.clear();
        self.scopes.clear();
        self.expires_at = 0;
        self.refresh_expires_at = 0;
        self.earliest_refresh_at = 0;
        self.nonce.clear();
    }
}

fn now() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|n| n.as_secs())
        .map_err(|_| anyhow::anyhow!("system clock is invalid"))
}
fn random() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes)
        .map_err(|_| anyhow::anyhow!("secure randomness unavailable"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}
fn safe_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id != "host-id"
            && id.len() <= 96
            && id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
        "unsafe ChatGPT connection ID"
    );
    Ok(())
}
fn issued_client(id: &str) -> Result<()> {
    ensure!(
        id.starts_with("oaiapp_")
            && id.len() > 7
            && id.len() <= 256
            && id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-'),
        "missing or invalid issued ChatGPT client ID"
    );
    Ok(())
}
fn bearer(token: &str) -> bool {
    !token.is_empty() && token.len() <= 64 * 1024 && token.bytes().all(|c| c.is_ascii_graphic())
}
fn plan_scope(scopes: &str) -> bool {
    [
        "openid",
        "offline_access",
        "resource.invoke",
        "chatgpt.tokens.use.direct",
    ]
    .iter()
    .all(|required| scopes.split_ascii_whitespace().any(|s| s == *required))
}
fn equal(a: &str, b: &str) -> bool {
    // State and nonce are fixed-length, high-entropy values.
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |d, (a, b)| d | (a ^ b)) == 0
}

fn auth_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .redirects(0)
        .timeout(HTTP_TIMEOUT)
        .build()
}
fn json(response: ureq::Response) -> Result<Value> {
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(MAX_JSON + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow::anyhow!("unreadable OpenAI authentication response"))?;
    ensure!(
        bytes.len() as u64 <= MAX_JSON,
        "OpenAI authentication response too large"
    );
    serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("invalid OpenAI authentication response"))
}
fn auth_get(endpoint: &str) -> Result<Value> {
    let response = auth_agent()
        .get(endpoint)
        .call()
        .map_err(|_| anyhow::anyhow!("OpenAI authentication service unavailable"))?;
    ensure!(
        response.status() == 200,
        "OpenAI authentication service unavailable"
    );
    json(response)
}
fn auth_url(raw: &str) -> Result<String> {
    let parsed = Url::parse(raw)
        .map_err(|_| anyhow::anyhow!("invalid OpenAI authentication configuration"))?;
    ensure!(
        parsed.scheme() == "https"
            && parsed.host_str() == Some("auth.openai.com")
            && parsed.port_or_known_default() == Some(443)
            && parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed.query().is_none()
            && parsed.fragment().is_none(),
        "untrusted OpenAI authentication endpoint"
    );
    Ok(parsed.to_string())
}
fn discovery() -> Result<Value> {
    let config = auth_get(DISCOVERY)?;
    ensure!(config["issuer"] == ISSUER, "unexpected OpenAI token issuer");
    Ok(config)
}
fn keys() -> Result<JwkSet> {
    let config = discovery()?;
    let endpoint = auth_url(
        config["jwks_uri"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing OpenAI signing keys"))?,
    )?;
    serde_json::from_value(auth_get(&endpoint)?)
        .map_err(|_| anyhow::anyhow!("invalid OpenAI signing keys"))
}
fn verified(token: &str, audience: &str, jwks: &JwkSet) -> Result<Value> {
    verified_at(token, audience, jwks, true)
}
fn verified_at(token: &str, audience: &str, jwks: &JwkSet, check_expiry: bool) -> Result<Value> {
    let header =
        decode_header(token).map_err(|_| anyhow::anyhow!("invalid OpenAI signed token"))?;
    // Asymmetric signing only; never select algorithms from token-controlled keys/URLs.
    ensure!(
        matches!(
            header.alg,
            Algorithm::RS256
                | Algorithm::RS384
                | Algorithm::RS512
                | Algorithm::ES256
                | Algorithm::ES384
                | Algorithm::EdDSA
        ),
        "unsupported OpenAI signing algorithm"
    );
    let kid = header
        .kid
        .ok_or_else(|| anyhow::anyhow!("missing OpenAI signing key ID"))?;
    let jwk = jwks
        .find(&kid)
        .ok_or_else(|| anyhow::anyhow!("unknown OpenAI signing key"))?;
    ensure!(
        jwks.keys
            .iter()
            .filter(|key| key.common.key_id.as_deref() == Some(&kid))
            .count()
            == 1,
        "ambiguous OpenAI signing key"
    );
    let public =
        serde_json::to_value(jwk).map_err(|_| anyhow::anyhow!("invalid OpenAI signing key"))?;
    ensure!(
        public["use"].is_null() || public["use"] == "sig",
        "OpenAI key cannot verify signatures"
    );
    ensure!(
        public["alg"].is_null()
            || public["alg"] == serde_json::to_value(header.alg).unwrap_or(Value::Null),
        "OpenAI signing key algorithm did not match"
    );
    let key =
        DecodingKey::from_jwk(jwk).map_err(|_| anyhow::anyhow!("invalid OpenAI signing key"))?;
    let mut validation = Validation::new(header.alg);
    validation.leeway = 5;
    validation.validate_exp = check_expiry;
    validation.validate_nbf = true;
    validation.set_issuer(&[ISSUER]);
    validation.set_audience(&[audience]);
    validation.set_required_spec_claims(&["iss", "aud", "sub", "exp", "iat"]);
    let claims = decode::<Value>(token, &key, &validation)
        .map_err(|_| anyhow::anyhow!("OpenAI token signature or claims could not be verified"))?
        .claims;
    ensure!(
        claims["sub"].as_str().is_some_and(|s| !s.is_empty()),
        "missing OpenAI account identity"
    );
    ensure!(
        claims["iat"]
            .as_u64()
            .is_some_and(|iat| iat <= now().unwrap_or(0).saturating_add(5)),
        "invalid OpenAI token issue time"
    );
    if let Some(audiences) = claims["aud"].as_array().filter(|a| a.len() > 1) {
        ensure!(
            !audiences.is_empty() && claims["azp"] == audience,
            "unexpected authorized OpenAI client"
        );
    }
    Ok(claims)
}
fn identity(token: &str, client: &str, nonce: &str, jwks: &JwkSet) -> Result<Value> {
    let claims = verified(token, client, jwks)?;
    ensure!(
        claims["nonce"].as_str().is_some_and(|n| equal(n, nonce)),
        "OpenAI sign-in nonce did not match"
    );
    Ok(claims)
}

struct GrantError {
    terminal: bool,
    kind: ErrorKind,
}
fn grant(endpoint: &str, form: &[(&str, &str)]) -> std::result::Result<Value, GrantError> {
    match auth_agent().post(endpoint).send_form(form) {
        Ok(response) if response.status() == 200 => json(response).map_err(|_| GrantError {
            terminal: true,
            kind: ErrorKind::Auth,
        }),
        Ok(_) => Err(GrantError {
            terminal: false,
            kind: ErrorKind::Transient,
        }),
        Err(ureq::Error::Status(status, response)) => {
            let body = json(response).ok();
            let code = body.as_ref().and_then(|v| v["error"].as_str());
            Err(GrantError {
                kind: match code {
                    Some("invalid_client") => ErrorKind::BadRequest,
                    _ if status == 429 => ErrorKind::RateLimited,
                    _ if status >= 500 => ErrorKind::Transient,
                    _ => ErrorKind::Auth,
                },
                terminal: code.is_some_and(|code| {
                    matches!(
                        code,
                        "invalid_grant"
                            | "invalid_refresh_token"
                            | "token_expired"
                            | "refresh_token_expired"
                            | "refresh_token_invalidated"
                            | "refresh_token_reused"
                    )
                }),
            })
        }
        Err(_) => Err(GrantError {
            terminal: false,
            kind: ErrorKind::Transient,
        }),
    }
}

fn required<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value[field]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("incomplete OpenAI token response"))
}
fn install_tokens(c: &mut Credentials, token: &Value, jwks: &JwkSet, refresh: bool) -> Result<()> {
    ensure!(
        required(token, "token_type")?.eq_ignore_ascii_case("Bearer"),
        "unsupported OpenAI token type"
    );
    let access = required(token, "access_token")?;
    let replacement = required(token, "refresh_token")?;
    ensure!(
        bearer(access) && bearer(replacement),
        "invalid OpenAI credentials"
    );
    if refresh {
        ensure!(
            !equal(replacement, &c.refresh_token),
            "OpenAI refresh token was not rotated"
        );
    }
    let access_claims = verified(access, RESOURCE, jwks)?;
    ensure!(
        access_claims["client_id"] == c.client_id,
        "OpenAI access token client did not match"
    );
    // Access-token sub is opaque resource identity, not the OIDC account subject.
    let scope = token["scope"]
        .as_str()
        .or_else(|| refresh.then_some(c.scopes.as_str()))
        .ok_or_else(|| anyhow::anyhow!("missing granted ChatGPT permissions"))?;
    ensure!(
        plan_scope(scope) && access_claims["scope"].as_str().is_some_and(plan_scope),
        "ChatGPT plan usage permission was not granted"
    );
    let timestamp = now()?;
    let lifetime = token["expires_in"]
        .as_u64()
        .filter(|n| *n > 0 && *n <= 86400)
        .ok_or_else(|| anyhow::anyhow!("invalid OpenAI access-token lifetime"))?;
    let expires = timestamp
        .saturating_add(lifetime)
        .min(access_claims["exp"].as_u64().unwrap_or(0));
    ensure!(expires > timestamp, "OpenAI access token has expired");
    if refresh && let Some(id_token) = token["id_token"].as_str() {
        let claims = verified(id_token, &c.client_id, jwks)?;
        ensure!(
            claims["sub"] == c.subject,
            "OpenAI refreshed account did not match"
        );
        if let Some(nonce) = claims["nonce"].as_str() {
            ensure!(
                equal(nonce, &c.nonce),
                "OpenAI refreshed nonce did not match"
            );
        }
        c.id_token = id_token.to_owned();
        c.id_signing_keys = jwks.clone();
    }
    if c.id_signing_keys.keys.is_empty() {
        c.id_signing_keys = c.signing_keys.clone();
    }
    c.scopes = scope.to_owned();
    c.signing_keys = jwks.clone();
    c.access_token = access.to_owned();
    c.refresh_token = replacement.to_owned();
    c.expires_at = expires;
    c.refresh_expires_at = timestamp.saturating_add(30 * 86400);
    c.earliest_refresh_at = token["earliest_refresh_at"].as_u64().unwrap_or(0);
    Ok(())
}

struct Attempt {
    state: String,
    nonce: String,
    verifier: String,
    redirect: String,
    client: Option<String>,
}
impl Attempt {
    fn authorize(&self, host: &str, previous: Option<&Credentials>) -> Result<String> {
        if let Some(previous) = previous {
            previous.check_registration()?;
            ensure!(
                previous.ext_agent_host_id == host,
                "ChatGPT registration belongs to a different host; use a new connection to sign in while preserving this registration"
            );
            ensure!(
                self.client.as_deref() == Some(previous.client_id.as_str()),
                "ChatGPT registration belongs to a different client"
            );
        }
        let mut url =
            Url::parse(AUTHORIZE).map_err(|_| anyhow::anyhow!("invalid authorization endpoint"))?;
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(self.verifier.as_bytes()));
        let mut query = url.query_pairs_mut();
        query.extend_pairs([
            (
                "client_id",
                self.client.as_deref().unwrap_or("dynamic_agent_client"),
            ),
            ("ext_agent_host_id", host),
            ("response_type", "code"),
            ("redirect_uri", &self.redirect),
            ("scope", SCOPES),
            ("resource", RESOURCE),
            ("state", &self.state),
            ("nonce", &self.nonce),
            ("code_challenge_method", "S256"),
            ("code_challenge", &challenge),
        ]);
        if self.client.is_none() {
            query.append_pair("agent_name_hint", "Loom");
        }
        if let Some(previous) = previous {
            // login is an explicit user action. Ask for consent again only when a
            // previous verified sign-in recorded an incomplete plan grant.
            if !previous.scopes.is_empty() && !plan_scope(&previous.scopes) {
                query.append_pair("prompt", "consent");
            }
            if !previous.id_token.is_empty() {
                query.append_pair("id_token_hint", &previous.id_token);
            }
            if let Some(email) = &previous.email {
                query.append_pair("login_hint", email);
            }
        }
        drop(query);
        Ok(url.into())
    }
    fn callback(&self, target: &str) -> Result<(String, String)> {
        ensure!(
            target.starts_with("/auth/callback?"),
            "unexpected OAuth callback path"
        );
        let url = Url::parse(&format!("http://127.0.0.1{target}"))
            .map_err(|_| anyhow::anyhow!("invalid OAuth callback"))?;
        ensure!(
            url.path() == CALLBACK && url.fragment().is_none(),
            "unexpected OAuth callback path"
        );
        let mut values = BTreeMap::new();
        for (name, value) in url.query_pairs() {
            ensure!(
                values
                    .insert(name.into_owned(), value.into_owned())
                    .is_none(),
                "duplicate OAuth callback parameter"
            );
        }
        ensure!(
            values
                .get("state")
                .is_some_and(|state| equal(state, &self.state)),
            "OAuth callback state did not match"
        );
        ensure!(
            !values.contains_key("error"),
            "ChatGPT authorization was declined or failed"
        );
        let code = values
            .get("code")
            .filter(|code| !code.is_empty() && code.len() <= 8192)
            .ok_or_else(|| anyhow::anyhow!("missing OAuth authorization code"))?
            .clone();
        let client = match (&self.client, values.get("client_id")) {
            (Some(expected), Some(actual)) => {
                ensure!(
                    equal(expected, actual),
                    "OAuth callback client did not match"
                );
                expected.clone()
            }
            (Some(expected), None) => expected.clone(),
            (None, Some(actual)) => actual.clone(),
            (None, None) => bail!("ChatGPT registration did not return an issued client ID"),
        };
        issued_client(&client)?;
        Ok((code, client))
    }
}

fn callback_target(stream: &mut TcpStream, deadline: Instant) -> Result<String> {
    let mut bytes = Vec::new();
    // Read only headers, with a total deadline; a slow sender cannot extend login.
    while bytes.len() <= 16 * 1024 {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| anyhow::anyhow!("ChatGPT sign-in timed out"))?;
        stream
            .set_read_timeout(Some(remaining.min(Duration::from_secs(2))))
            .map_err(|_| anyhow::anyhow!("OAuth callback unavailable"))?;
        let mut chunk = [0u8; 1024];
        let count = stream
            .read(&mut chunk)
            .map_err(|_| anyhow::anyhow!("unreadable OAuth callback"))?;
        ensure!(count != 0, "incomplete OAuth callback");
        bytes.extend_from_slice(&chunk[..count]);
        if bytes.windows(4).any(|w| w == b"\r\n\r\n") {
            let header = std::str::from_utf8(&bytes)
                .map_err(|_| anyhow::anyhow!("invalid OAuth callback"))?;
            let mut line = header.lines().next().unwrap_or("").split_whitespace();
            ensure!(
                line.next() == Some("GET"),
                "unexpected OAuth callback method"
            );
            let target = line
                .next()
                .ok_or_else(|| anyhow::anyhow!("missing OAuth callback target"))?;
            ensure!(
                line.next() == Some("HTTP/1.1") && line.next().is_none(),
                "invalid OAuth callback request"
            );
            return Ok(target.to_owned());
        }
    }
    bail!("OAuth callback too large")
}
fn browser_result(stream: &mut TcpStream, success: bool) {
    let (status, body) = if success {
        (
            "200 OK",
            "Loom is connected to ChatGPT. You can close this window.",
        )
    } else {
        (
            "400 Bad Request",
            "Loom could not verify this sign-in. Return to Loom and try again.",
        )
    };
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nContent-Security-Policy: default-src 'none'\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

/// Explicit user-initiated login. The browser launcher must not log its URL.
pub fn login(
    root: &Path,
    id: &str,
    open_browser: &mut dyn FnMut(&str) -> Result<()>,
) -> Result<()> {
    safe_id(id)?;
    let store = Store::open(root, true)?;
    let host = store.host()?;
    let _lock = store.lock(id)?;
    let previous = store.load(id)?;
    if let Some(previous) = &previous {
        previous.check_registration()?;
    }
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|_| anyhow::anyhow!("OAuth callback listener unavailable"))?;
    listener
        .set_nonblocking(true)
        .map_err(|_| anyhow::anyhow!("OAuth callback listener unavailable"))?;
    let port = listener
        .local_addr()
        .map_err(|_| anyhow::anyhow!("OAuth callback listener unavailable"))?
        .port();
    let attempt = Attempt {
        state: random()?,
        nonce: random()?,
        verifier: random()?,
        redirect: format!("http://127.0.0.1:{port}{CALLBACK}"),
        client: previous.as_ref().map(|c| c.client_id.clone()),
    };
    // Check the stored host/client binding before any network request. A repaired
    // legacy installation ID must never silently rebind an issued registration.
    let authorize_url = attempt.authorize(&host, previous.as_ref())?;
    // Fetch public signing keys before the browser wait, not after consuming a code.
    let jwks = keys()?;
    let deadline = Instant::now() + LOGIN_TIMEOUT;
    open_browser(&authorize_url)
        .map_err(|_| anyhow::anyhow!("could not open ChatGPT sign-in browser"))?;
    loop {
        ensure!(Instant::now() < deadline, "ChatGPT sign-in timed out");
        match listener.accept() {
            Ok((mut stream, peer)) if peer.ip().is_loopback() => {
                let result = (|| -> Result<()> {
                    let target = callback_target(&mut stream, deadline)?;
                    // Ignore unrelated browser requests without consuming the attempt.
                    if !target.starts_with("/auth/callback?") {
                        bail!("unrelated OAuth callback request");
                    }
                    let (code, client) = attempt.callback(&target)?;
                    let token = grant(
                        TOKEN,
                        &[
                            ("grant_type", "authorization_code"),
                            ("client_id", &client),
                            ("code", &code),
                            ("code_verifier", &attempt.verifier),
                            ("redirect_uri", &attempt.redirect),
                            ("resource", RESOURCE),
                        ],
                    )
                    .map_err(|_| {
                        anyhow::anyhow!("ChatGPT code exchange failed; start a fresh sign-in")
                    })?;
                    let id_token = required(&token, "id_token")?;
                    // JWKS is fetched again if rotation took place during browser sign-in.
                    let mut identity_keys = jwks.clone();
                    let claims = match identity(id_token, &client, &attempt.nonce, &identity_keys) {
                        Ok(claims) => claims,
                        Err(_) => {
                            identity_keys = keys()?;
                            identity(id_token, &client, &attempt.nonce, &identity_keys)?
                        }
                    };
                    if let Some(previous) = &previous {
                        ensure!(
                            claims["sub"] == previous.subject,
                            "ChatGPT sign-in returned a different account"
                        );
                        ensure!(
                            previous.ext_agent_host_id == host,
                            "ChatGPT registration belongs to a different host"
                        );
                    }
                    let mut credentials = Credentials {
                        version: 1,
                        issuer: ISSUER.into(),
                        subject: required(&claims, "sub")?.into(),
                        client_id: client,
                        ext_agent_host_id: host.clone(),
                        email: claims["email"].as_str().map(str::to_owned),
                        id_token: id_token.into(),
                        access_token: String::new(),
                        refresh_token: String::new(),
                        scopes: String::new(),
                        expires_at: 0,
                        refresh_expires_at: 0,
                        earliest_refresh_at: 0,
                        nonce: attempt.nonce.clone(),
                        signing_keys: jwks.clone(),
                        id_signing_keys: identity_keys.clone(),
                    };
                    if !token["scope"].as_str().is_some_and(plan_scope) {
                        // Preserve a verified new registration without pretending that
                        // identity consent authorizes inference. Existing sessions stay intact.
                        if previous.is_none() {
                            credentials.scopes = token["scope"].as_str().unwrap_or("").into();
                            store.save(id, &credentials)?;
                        }
                        bail!("ChatGPT plan usage permission was not granted");
                    }
                    install_tokens(&mut credentials, &token, &identity_keys, false)
                        .or_else(|_| install_tokens(&mut credentials, &token, &keys()?, false))?;
                    store.save(id, &credentials)?;
                    Ok(())
                })();
                browser_result(&mut stream, result.is_ok());
                // State mismatch/denial consumes the attempt, never redeems a code.
                if result
                    .as_ref()
                    .err()
                    .is_some_and(|e| e.to_string() == "unrelated OAuth callback request")
                {
                    continue;
                }
                return result;
            }
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(25))
            }
            Err(_) => bail!("OAuth callback listener failed"),
        }
    }
}

/// Revoke the renewable session, then clear tokens but retain the account mapping.
/// An error after local clearing explicitly reports unconfirmed remote revocation.
pub fn logout(root: &Path, id: &str) -> Result<()> {
    safe_id(id)?;
    match std::fs::symlink_metadata(root.join("chatgpt-oauth")) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => bail!("ChatGPT credential directory unavailable"),
        Ok(_) => {}
    }
    let store = Store::open(root, false)?;
    let _lock = store.lock(id)?;
    #[cfg(unix)]
    let interrupted = store
        .read(&format!(".{id}.json.pending"))?
        .is_some_and(|bytes| serde_json::from_slice::<Value>(&bytes).is_err());
    #[cfg(not(unix))]
    let interrupted = false;
    let Some(mut c) = store.load(id)? else {
        return Ok(());
    };
    c.check_registration()?;
    let revoked = if c.refresh_token.is_empty() {
        !interrupted
    } else {
        (|| -> Result<()> {
            let config = discovery()?;
            let endpoint = auth_url(required(&config, "revocation_endpoint")?)?;
            let form = [
                ("token", c.refresh_token.as_str()),
                ("token_type_hint", "refresh_token"),
                ("client_id", c.client_id.as_str()),
            ];
            // One bounded retry with backoff on transport/5xx only.
            for attempt in 0..2 {
                match auth_agent().post(&endpoint).send_form(&form) {
                    Ok(response) if response.status() == 200 => return Ok(()),
                    Err(ureq::Error::Status(status, _)) if status < 500 => break,
                    _ if attempt == 0 => std::thread::sleep(Duration::from_millis(200)),
                    _ => break,
                }
            }
            bail!("revocation not confirmed")
        })()
        .is_ok()
    };
    c.clear_tokens();
    store.save(id, &c)?;
    ensure!(
        revoked,
        "signed out locally; remote revocation was not confirmed. Disconnect Loom in ChatGPT Settings"
    );
    Ok(())
}

/// Local verified-session status, not a claim of current remote admission or quota.
pub fn is_connected(root: &Path, id: &str) -> bool {
    (|| -> Result<bool> {
        safe_id(id)?;
        let store = Store::open(root, false)?;
        let _lock = store.lock(id)?;
        Ok(store
            .load(id)?
            .is_some_and(|c| c.renewable(now().unwrap_or(u64::MAX))))
    })()
    .unwrap_or(false)
}

pub fn provider(root: PathBuf, id: String) -> Box<dyn Provider> {
    Box::new(ChatGptProvider { root, id })
}
struct ChatGptProvider {
    root: PathBuf,
    id: String,
}
fn credential_error() -> ProviderError {
    ProviderError::new(
        ErrorKind::Auth,
        "ChatGPT credentials unavailable; sign in again",
    )
}
fn install_rotated(
    c: &mut Credentials,
    token: &Value,
    jwks: &JwkSet,
    reload: &mut dyn FnMut() -> Result<JwkSet>,
) -> Result<()> {
    match install_tokens(c, token, jwks, true) {
        Ok(()) => Ok(()),
        Err(error) => {
            let unknown = ["access_token", "id_token"]
                .iter()
                .filter_map(|field| token[*field].as_str())
                .filter_map(|token| decode_header(token).ok())
                .filter_map(|header| header.kid)
                .any(|kid| jwks.find(&kid).is_none());
            if unknown {
                install_tokens(c, token, &reload()?, true)
            } else {
                Err(error)
            }
        }
    }
}
fn refresh_session(
    store: &Store,
    id: &str,
    c: &mut Credentials,
    endpoint: &str,
    jwks: &JwkSet,
) -> std::result::Result<(), ProviderError> {
    match grant(
        endpoint,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", &c.client_id),
            ("refresh_token", &c.refresh_token),
            ("resource", RESOURCE),
        ],
    ) {
        Ok(token) => {
            if install_rotated(c, &token, jwks, &mut keys).is_err() {
                // A successful rotation with an unverifiable reply cannot safely
                // reuse the previous refresh token. Fail closed and reauthorize.
                c.clear_tokens();
                store.save(id, c).map_err(|_| credential_error())?;
                return Err(credential_error());
            }
            store.save(id, c).map_err(|_| credential_error())?;
            Ok(())
        }
        Err(error) => {
            if error.terminal {
                c.clear_tokens();
                store.save(id, c).map_err(|_| credential_error())?;
            }
            Err(ProviderError::new(
                error.kind,
                if error.terminal {
                    "ChatGPT session ended; sign in again"
                } else {
                    "ChatGPT refresh unavailable; retry later"
                },
            ))
        }
    }
}
impl ChatGptProvider {
    fn headers(&self) -> std::result::Result<Vec<(String, String)>, ProviderError> {
        let (store, _lock, mut c, timestamp) = (|| -> Result<_> {
            safe_id(&self.id)?;
            let store = Store::open(&self.root, false)?;
            let lock = store.lock(&self.id)?;
            let c = store
                .load(&self.id)?
                .ok_or_else(|| anyhow::anyhow!("ChatGPT sign-in required"))?;
            let timestamp = now()?;
            ensure!(c.renewable(timestamp), "ChatGPT sign-in required");
            Ok((store, lock, c, timestamp))
        })()
        .map_err(|_| credential_error())?;
        if c.expires_at <= timestamp.saturating_add(REFRESH_MARGIN) {
            if c.earliest_refresh_at <= timestamp {
                // Fetch before rotation so a JWKS outage does not consume a token.
                let jwks = keys().map_err(|_| {
                    ProviderError::new(
                        ErrorKind::Transient,
                        "OpenAI signing keys unavailable; retry later",
                    )
                })?;
                refresh_session(&store, &self.id, &mut c, TOKEN, &jwks)?;
            } else if c.expires_at <= timestamp.saturating_add(5) {
                return Err(ProviderError::new(
                    ErrorKind::Transient,
                    "ChatGPT token cannot be refreshed yet",
                ));
            }
        }
        Ok(vec![(
            "Authorization".into(),
            format!("Bearer {}", c.access_token),
        )])
    }
}

fn sanitized(mut error: ProviderError) -> ProviderError {
    // Shared transport diagnostics may contain reflected headers/response bodies.
    error.message = match error.kind {
        ErrorKind::Auth => "ChatGPT authorization was rejected",
        ErrorKind::Quota => "ChatGPT plan usage limit reached; check ChatGPT Settings → Usage",
        ErrorKind::RateLimited => "ChatGPT request rate limited",
        ErrorKind::Cancelled => "ChatGPT request cancelled",
        ErrorKind::TimedOut => "ChatGPT request timed out",
        ErrorKind::BadRequest => "ChatGPT rejected the requested model or capability",
        ErrorKind::Transient => "ChatGPT service or response unavailable",
    }
    .into();
    // Only documented, nonsecret machine codes cross the public error boundary.
    error.code = error.code.filter(|code| {
        matches!(
            code.as_str(),
            "subscription_sharing_user_not_eligible"
                | "subscription_sharing_usage_limit_exceeded"
                | "subscription_sharing_usage_unavailable"
                | "subscription_sharing_unsupported_capability"
                | "subscription_sharing_route_not_supported"
                | "subscription_sharing_invalid_user"
                | "subscription_sharing_user_unavailable"
                | "chatpass_v2_scope_not_authorized"
                | "chatpass_v2_invalid_authorization_context"
        )
    });
    if error.code.as_deref() == Some("subscription_sharing_usage_limit_exceeded") {
        error.kind = ErrorKind::Quota;
    }
    error
}
impl Provider for ChatGptProvider {
    fn name(&self) -> &str {
        &self.id
    }
    fn complete(&self, request: &Request) -> std::result::Result<Response, ProviderError> {
        // Official direct plan usage requires a stream even for a collected reply.
        self.complete_stream(request, &mut |_| {})
    }
    fn complete_stream(
        &self,
        request: &Request,
        observe: &mut dyn FnMut(StreamEvent),
    ) -> std::result::Result<Response, ProviderError> {
        request.check_cancelled()?;
        let headers = self.headers()?;
        request.check_cancelled()?;
        super::stream::responses(
            &super::url(RESOURCE, "responses"),
            &headers,
            request,
            super::responses::body(request),
            observe,
        )
        .map_err(sanitized)
    }
    fn list_models(&self) -> std::result::Result<Vec<String>, ProviderError> {
        let value = super::get_json(&super::url(RESOURCE, "models"), &self.headers()?)
            .map_err(sanitized)?;
        catalog(&value)
    }
}
fn catalog(value: &Value) -> std::result::Result<Vec<String>, ProviderError> {
    let models = value["models"].as_array().ok_or_else(|| {
        ProviderError::new(ErrorKind::Transient, "unreadable ChatGPT model catalog")
    })?;
    Ok(models
        .iter()
        .filter(|m| m["visibility"] == "list")
        .filter_map(|m| m["slug"].as_str())
        .filter(|id| !id.is_empty() && super::is_chat_model(id))
        .map(str::to_owned)
        .collect())
}

mod storage;
use storage::Store;

#[cfg(all(test, unix))]
#[path = "chatgpt/tests.rs"]
mod tests;
