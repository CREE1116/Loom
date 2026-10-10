//! Remote control: a small web server that mirrors the open conversation and accepts
//! messages, interrupts and approval answers from a browser (a phone on the same
//! network, for example).
//!
//! Every request needs the session's random 128-bit token (in the page URL, then as a
//! bearer header). The server only runs while remote control is on; turning it off
//! closes the listener, every waiting request and the tunnel. On the local network the
//! traffic is plain HTTP; with `cloudflared` installed a Cloudflare quick tunnel gives
//! an HTTPS address reachable from anywhere, which the window then shows instead.
use anyhow::{Context, Result};
use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Read, Write},
    net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket},
    process::{Child, Command as Process, Stdio},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

const PAGE: &str = include_str!("remote.html");
const MAX_BODY: usize = 64 * 1024;
const LONG_POLL: Duration = Duration::from_secs(20);
const DEFAULT_PORT: u16 = 8787;
const MAX_CONNECTIONS: usize = 32;
const MAX_HEADERS: usize = 16 * 1024;
const ACTION_TIMEOUT: Duration = Duration::from_secs(5);

/// What a browser asked the open window to do.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    Send(String),
    Interrupt,
    Retry,
    ResumeQueue,
    Queue { index: usize, revision: String, operation: String, model: Option<String>, effort: Option<String> },
    Reply {
        id: String,
        choice: String,
    },
    Model(String),
    Effort(String),
    NewConversation,
    Compact,
    Answer {
        id: String,
        answers: Vec<crate::agent::QuestionAnswer>,
    },
    CancelQuestions(String),
}

impl Command {
    fn parse(value: &serde_json::Value) -> Option<Self> {
        let text = |key: &str| {
            value[key]
                .as_str()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_owned)
        };
        Some(match value["action"].as_str()? {
            "send" => Self::Send(text("text")?),
            "interrupt" => Self::Interrupt,
            "retry" => Self::Retry,
            "queue_resume" => Self::ResumeQueue,
            operation @ ("queue_send" | "queue_remove" | "queue_update") => Self::Queue {
                index: usize::try_from(value["index"].as_u64()?).ok()?,
                revision: text("revision")?, operation: operation.into(),
                model: text("model"), effort: value["effort"].as_str().map(str::to_owned),
            },
            "reply" => Self::Reply {
                id: text("id")?,
                choice: text("choice")?,
            },
            "model" => Self::Model(text("model")?),
            "effort" => Self::Effort(value["effort"].as_str().unwrap_or_default().to_owned()),
            "new" => Self::NewConversation,
            "compact" => Self::Compact,
            "answer" => Self::Answer {
                id: text("id")?,
                answers: value["answers"]
                    .as_array()?
                    .iter()
                    .map(|answer| {
                        let question_id = answer["question_id"].as_str()?.to_owned();
                        let value = match (answer.get("option"), answer.get("text")) {
                            (Some(index), None) => crate::agent::AnswerValue::Option(
                                usize::try_from(index.as_u64()?).ok()?,
                            ),
                            (None, Some(text)) => {
                                crate::agent::AnswerValue::Text(text.as_str()?.to_owned())
                            }
                            _ => return None,
                        };
                        Some(crate::agent::QuestionAnswer { question_id, value })
                    })
                    .collect::<Option<Vec<_>>>()?,
            },
            "cancel_questions" => Self::CancelQuestions(text("id")?),
            _ => return None,
        })
    }
}

pub struct PendingCommand {
    pub command: Command,
    deadline: Instant,
    reply: mpsc::Sender<std::result::Result<(), String>>,
}

impl PendingCommand {
    pub fn expired(&self) -> bool {
        Instant::now() >= self.deadline
    }
    pub fn complete(self, result: std::result::Result<(), String>) {
        let _ = self.reply.send(result);
    }
}

struct Shared {
    /// (version, JSON snapshot of the window).
    state: Mutex<(u64, String)>,
    changed: Condvar,
    commands: Mutex<Vec<PendingCommand>>,
    stop: AtomicBool,
    clients: Mutex<HashMap<String, Instant>>,
    connections: Mutex<HashMap<u64, TcpStream>>,
    next_connection: AtomicU64,
    /// Recent action IDs prevent a lost HTTP reply from submitting the same task twice.
    receipts: Mutex<HashMap<String, (Instant, [u8; 32], Option<std::result::Result<(), String>>)>>,
}

pub struct Remote {
    shared: Arc<Shared>,
    token: String,
    pub port: u16,
    pub urls: Vec<String>,
    accept: Option<thread::JoinHandle<()>>,
    tunnel: Option<Child>,
    public: Arc<Mutex<Option<String>>>,
    tunnel_started: Option<Instant>,
}

impl Remote {
    /// Listens on every interface (so a phone on the same network can connect), on
    /// `port` or, when taken, any free port.
    pub fn start(port: Option<u16>) -> Result<Self> {
        let listener = TcpListener::bind(("0.0.0.0", port.unwrap_or(DEFAULT_PORT)))
            .or_else(|_| TcpListener::bind(("0.0.0.0", 0)))
            .context("원격 조작 서버를 열 포트가 없습니다")?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let token = token()?;
        let shared = Arc::new(Shared {
            state: Mutex::new((0, "{}".into())),
            changed: Condvar::new(),
            commands: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
            clients: Mutex::new(HashMap::new()),
            connections: Mutex::new(HashMap::new()),
            next_connection: AtomicU64::new(0),
            receipts: Mutex::new(HashMap::new()),
        });
        let mut urls = Vec::new();
        if let Some(ip) = lan_address() {
            urls.push(format!("http://{ip}:{port}/t/{token}"));
        }
        urls.push(format!("http://127.0.0.1:{port}/t/{token}"));
        let accept = {
            let shared = shared.clone();
            let token = token.clone();
            thread::Builder::new()
                .name("loom-remote".into())
                .spawn(move || accept_loop(listener, shared, token))?
        };
        Ok(Self {
            shared,
            token,
            port,
            urls,
            accept: Some(accept),
            tunnel: None,
            public: Arc::default(),
            tunnel_started: None,
        })
    }

    /// Opens a Cloudflare quick tunnel (no account needed) to this server. False when
    /// `cloudflared` is not installed; the HTTPS address appears in `public_url` once
    /// Cloudflare assigns it, usually within seconds.
    pub fn open_tunnel(&mut self) -> bool {
        if self.tunnel.is_some() {
            return true;
        }
        let Ok(mut child) = Process::new("cloudflared")
            .args([
                "tunnel",
                "--no-autoupdate",
                "--url",
                &format!("http://127.0.0.1:{}", self.port),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
        else {
            return false;
        };
        let stderr = child.stderr.take();
        let public = self.public.clone();
        let token = self.token.clone();
        let _ = thread::Builder::new()
            .name("loom-remote-tunnel".into())
            .spawn(move || {
                let Some(stderr) = stderr else { return };
                // cloudflared logs the assigned address; keep reading so its pipe never fills.
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    if public.lock().unwrap().is_none()
                        && let Some(url) = trycloudflare_url(&line)
                    {
                        *public.lock().unwrap() = Some(format!("{url}/t/{token}"));
                    }
                }
            });
        self.tunnel = Some(child);
        self.tunnel_started = Some(Instant::now());
        true
    }

    /// The tunnel's HTTPS address with the token, once known.
    pub fn public_url(&self) -> Option<String> {
        self.public.lock().unwrap().clone()
    }

    /// A failed or stalled tunnel leaves the local server available.
    pub fn tunnel_status(&mut self) -> std::result::Result<Option<String>, String> {
        let Some(child) = self.tunnel.as_mut() else {
            return Ok(None);
        };
        let failure = match child.try_wait() {
            Ok(Some(status)) => Some(format!("인터넷 연결이 종료됐습니다 ({status}). /remote off 후 /remote로 다시 시도하세요.")),
            Err(error) => Some(format!("인터넷 연결 상태 확인 실패: {error}")),
            Ok(None) if self.public_url().is_none() && self.tunnel_started.is_some_and(|at| at.elapsed() > Duration::from_secs(30)) =>
                Some("인터넷 주소 준비 시간이 초과됐습니다. /remote off 후 /remote로 다시 시도하세요.".into()),
            Ok(None) => None,
        };
        if let Some(error) = failure {
            if let Some(mut child) = self.tunnel.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            *self.public.lock().unwrap() = None;
            self.tunnel_started = None;
            return Err(error);
        }
        Ok(self.public_url())
    }

    /// Publishes the window's current state; waiting browsers wake only on a change.
    pub fn publish(&self, snapshot: &serde_json::Value) {
        let text = snapshot.to_string();
        let mut state = self.shared.state.lock().unwrap();
        if state.1 != text {
            state.0 += 1;
            state.1 = text;
            self.shared.changed.notify_all();
        }
    }

    pub fn take_commands(&self) -> Vec<PendingCommand> {
        std::mem::take(&mut *self.shared.commands.lock().unwrap())
    }

    /// Browsers that asked for state within the last long-poll window.
    pub fn clients(&self) -> usize {
        let mut clients = self.shared.clients.lock().unwrap();
        clients.retain(|_, seen| seen.elapsed() < LONG_POLL + Duration::from_secs(10));
        clients.len()
    }

    /// The address to show first: the network one when there is one.
    pub fn url(&self) -> &str {
        &self.urls[0]
    }

    #[cfg(test)]
    fn token(&self) -> &str {
        &self.token
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.shared.changed.notify_all();
        for stream in self.shared.connections.lock().unwrap().values() {
            let _ = stream.shutdown(Shutdown::Both);
        }
        self.shared.commands.lock().unwrap().clear();
        if let Some(mut tunnel) = self.tunnel.take() {
            let _ = tunnel.kill();
            let _ = tunnel.wait();
        }
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
    }
}

fn trycloudflare_url(line: &str) -> Option<&str> {
    let start = line.find("https://")?;
    let rest = &line[start..];
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '|')
        .unwrap_or(rest.len());
    let url = &rest[..end];
    url.ends_with(".trycloudflare.com").then_some(url)
}

/// 128 random bits in upper-case base32 (26 characters): with the token in the path,
/// an upper-cased address uses only QR alphanumeric characters, so its code stays
/// small enough for a narrow terminal.
fn token() -> Result<String> {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).map_err(|e| anyhow::anyhow!("random token: {e}"))?;
    let bits = u128::from_be_bytes(bytes);
    Ok((0..26)
        .map(|i| ALPHABET[((bits >> (i * 5)) & 31) as usize] as char)
        .collect())
}

/// The address other devices on the network reach this machine at (no packet is sent).
fn lan_address() -> Option<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

fn accept_loop(listener: TcpListener, shared: Arc<Shared>, token: String) {
    while !shared.stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((mut stream, peer)) => {
                let mut connections = shared.connections.lock().unwrap();
                if shared.stop.load(Ordering::SeqCst) || connections.len() >= MAX_CONNECTIONS {
                    drop(connections);
                    let _ = stream.set_write_timeout(Some(Duration::from_millis(100)));
                    let _ = respond(
                        &mut stream,
                        "503 Service Unavailable",
                        "text/plain",
                        b"too many connections",
                    );
                    continue;
                }
                let id = shared.next_connection.fetch_add(1, Ordering::Relaxed);
                let Ok(handle) = stream.try_clone() else {
                    continue;
                };
                connections.insert(id, handle);
                drop(connections);
                let shared = shared.clone();
                let token = token.clone();
                let guard = Connection {
                    shared: shared.clone(),
                    id,
                };
                let _ = thread::Builder::new()
                    .name("loom-remote-client".into())
                    .spawn(move || {
                        let _guard = guard;
                        let _ = serve(stream, peer, &shared, &token);
                    });
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(_) => thread::sleep(Duration::from_millis(200)),
        }
    }
}

struct Connection {
    shared: Arc<Shared>,
    id: u64,
}
impl Drop for Connection {
    fn drop(&mut self) {
        self.shared.connections.lock().unwrap().remove(&self.id);
    }
}

struct Request {
    method: String,
    path: String,
    query: String,
    authorization: Option<String>,
    body: Vec<u8>,
}

fn read_request(stream: &mut TcpStream) -> Result<Request> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut budget = MAX_HEADERS;
    let line = header_line(&mut reader, &mut budget)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().context("request line")?.to_owned();
    let target = parts.next().context("request target")?.to_owned();
    anyhow::ensure!(
        matches!(parts.next(), Some("HTTP/1.1" | "HTTP/1.0")) && parts.next().is_none(),
        "request version"
    );
    let (path, query) = target
        .split_once('?')
        .map_or((target.clone(), String::new()), |(p, q)| {
            (p.into(), q.into())
        });
    let mut length = 0usize;
    let mut has_length = false;
    let mut authorization = None;
    loop {
        let header = header_line(&mut reader, &mut budget)?;
        if header.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            match name.trim().to_ascii_lowercase().as_str() {
                "content-length" => {
                    anyhow::ensure!(!has_length, "duplicate content length");
                    has_length = true;
                    length = value.trim().parse().context("content length")?;
                }
                "transfer-encoding" => anyhow::bail!("chunked requests unsupported"),
                "authorization" => authorization = Some(value.trim().to_owned()),
                _ => {}
            }
        }
    }
    anyhow::ensure!(length <= MAX_BODY, "body too large");
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    Ok(Request {
        method,
        path,
        query,
        authorization,
        body,
    })
}

fn header_line(reader: &mut BufReader<TcpStream>, budget: &mut usize) -> Result<String> {
    let mut line = String::new();
    (&mut *reader)
        .take((*budget + 1) as u64)
        .read_line(&mut line)?;
    anyhow::ensure!(
        line.len() <= *budget && line.ends_with('\n'),
        "invalid or oversized headers"
    );
    *budget -= line.len();
    Ok(line)
}

fn query_value<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        pair.split_once('=')
            .filter(|(k, _)| *k == key)
            .map(|(_, v)| v)
    })
}

/// Constant-time comparison, so response timing says nothing about the token.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

fn respond(stream: &mut TcpStream, status: &str, kind: &str, body: &[u8]) -> Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\nX-Frame-Options: DENY\r\nReferrer-Policy: no-referrer\r\n\
         Content-Security-Policy: default-src 'self'; style-src 'unsafe-inline'; script-src 'unsafe-inline'\r\n\
         Connection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok(())
}

const MANIFEST: &str = r##"{"name":"Loom","short_name":"Loom","start_url":"/","scope":"/","display":"standalone",
"background_color":"#0f1115","theme_color":"#4f6bed",
"icons":[{"src":"/icon-192.png","sizes":"192x192","type":"image/png"},{"src":"/icon-512.png","sizes":"512x512","type":"image/png","purpose":"any maskable"}]}"##;

/// The app icon: an "L" on Loom's accent colour, drawn here so no binary asset ships.
fn icon(size: u32) -> Vec<u8> {
    let accent = image::Rgba([0x4f, 0x6b, 0xed, 0xff]);
    let white = image::Rgba([0xff, 0xff, 0xff, 0xff]);
    let unit = size as f32 / 16.0;
    let image = image::RgbaImage::from_fn(size, size, |x, y| {
        let (u, v) = (x as f32 / unit, y as f32 / unit);
        // Stem and foot of the L, inside the maskable safe zone.
        let stem = (5.5..7.5).contains(&u) && (4.0..12.0).contains(&v);
        let foot = (5.5..11.0).contains(&u) && (10.0..12.0).contains(&v);
        if stem || foot { white } else { accent }
    });
    let mut png = Vec::new();
    let _ = image::DynamicImage::ImageRgba8(image)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png);
    png
}

fn serve(mut stream: TcpStream, peer: SocketAddr, shared: &Shared, token: &str) -> Result<()> {
    // Accepted sockets can inherit the listener's nonblocking mode on BSD/macOS.
    // Each request has its own thread and needs blocking reads with a timeout.
    stream.set_nonblocking(false)?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let request = match read_request(&mut stream) {
        Ok(request) => request,
        Err(_) => return respond(&mut stream, "400 Bad Request", "text/plain", b"bad request"),
    };
    if shared.stop.load(Ordering::SeqCst) {
        return Ok(());
    }
    // Install metadata carries no conversation data; browsers fetch it without the token.
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/manifest.webmanifest") => {
            return respond(
                &mut stream,
                "200 OK",
                "application/manifest+json",
                MANIFEST.as_bytes(),
            );
        }
        ("GET", "/icon-192.png" | "/apple-touch-icon.png") => {
            return respond(&mut stream, "200 OK", "image/png", &icon(192));
        }
        ("GET", "/icon-512.png") => return respond(&mut stream, "200 OK", "image/png", &icon(512)),
        _ => {}
    }
    // The page is opened at /t/TOKEN (any case: QR scanners may keep the upper-cased
    // address); the page then sends the token as a bearer header.
    let page_token = request
        .path
        .strip_prefix("/t/")
        .or_else(|| request.path.strip_prefix("/T/"))
        .map(str::to_owned);
    let presented = page_token
        .clone()
        .or_else(|| query_value(&request.query, "t").map(str::to_owned))
        .or_else(|| {
            request
                .authorization
                .as_deref()
                .and_then(|a| a.strip_prefix("Bearer "))
                .map(str::to_owned)
        });
    // The installed app opens "/" and finds the token in its own storage.
    if request.method == "GET" && request.path == "/" && presented.is_none() {
        return respond(
            &mut stream,
            "200 OK",
            "text/html; charset=utf-8",
            PAGE.as_bytes(),
        );
    }
    if !presented.is_some_and(|t| same(&t.to_ascii_uppercase(), token)) {
        // Slow down guessing; a 128-bit token is out of reach anyway.
        thread::sleep(Duration::from_millis(300));
        return respond(
            &mut stream,
            "401 Unauthorized",
            "text/plain; charset=utf-8",
            "토큰이 맞지 않습니다. Loom에 표시된 주소로 접속하세요.".as_bytes(),
        );
    }
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", _) if page_token.is_some() || request.path == "/" => respond(
            &mut stream,
            "200 OK",
            "text/html; charset=utf-8",
            PAGE.as_bytes(),
        ),
        ("GET", "/api/state") => {
            let since: u64 = query_value(&request.query, "since")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let fallback = peer.ip().to_string();
            let client = query_value(&request.query, "client")
                .filter(|id| {
                    !id.is_empty()
                        && id.len() <= 64
                        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                })
                .unwrap_or(&fallback);
            {
                let mut clients = shared.clients.lock().unwrap();
                clients.retain(|_, seen| seen.elapsed() < LONG_POLL + Duration::from_secs(10));
                if clients.len() < MAX_CONNECTIONS || clients.contains_key(client) {
                    clients.insert(client.to_owned(), Instant::now());
                }
            }
            let deadline = Instant::now() + LONG_POLL;
            let mut state = shared.state.lock().unwrap();
            while state.0 <= since && !shared.stop.load(Ordering::SeqCst) {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                state = shared.changed.wait_timeout(state, left).unwrap().0;
            }
            let body = format!("{{\"version\":{},\"state\":{}}}", state.0, state.1);
            drop(state);
            if shared.stop.load(Ordering::SeqCst) {
                return Ok(());
            }
            respond(&mut stream, "200 OK", "application/json", body.as_bytes())
        }
        ("POST", "/api/action") => {
            let value: serde_json::Value =
                serde_json::from_slice(&request.body).unwrap_or_default();
            match Command::parse(&value) {
                Some(command) => {
                    let request_id = value.get("request_id").and_then(|id| id.as_str());
                    if value.get("request_id").is_some()
                        && !request_id.is_some_and(|id| {
                            !id.is_empty()
                                && id.len() <= 128
                                && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                        })
                    {
                        return respond(
                            &mut stream,
                            "400 Bad Request",
                            "application/json",
                            b"{\"ok\":false}",
                        );
                    }
                    if let Some(id) = request_id {
                        // Retain a digest, not message or secret-answer contents.
                        use sha2::{Digest, Sha256};
                        let signature: [u8; 32] = Sha256::digest(value.to_string().as_bytes()).into();
                        let mut receipts = shared.receipts.lock().unwrap();
                        receipts.retain(|_, (at, _, _)| at.elapsed() < Duration::from_secs(300));
                        if let Some((_, prior, result)) = receipts.get(id) {
                            let result = if *prior != signature {
                                Err("요청 식별자가 다른 작업에 사용됐습니다.".into())
                            } else {
                                result.clone().unwrap_or_else(|| {
                                    Err("이 요청을 처리 중입니다. 잠시 뒤 상태를 확인해주세요."
                                        .into())
                                })
                            };
                            drop(receipts);
                            return action_response(&mut stream, result);
                        }
                        if receipts.len() >= 128 {
                            if let Some(oldest) = receipts
                                .iter()
                                .filter(|(_, (_, _, result))| result.is_some())
                                .min_by_key(|(_, (at, _, _))| *at)
                                .map(|(id, _)| id.clone())
                            {
                                receipts.remove(&oldest);
                            }
                        }
                        if receipts.len() >= 128 {
                            drop(receipts);
                            return action_response(
                                &mut stream,
                                Err("처리 중인 요청이 많습니다. 잠시 뒤 다시 시도해주세요.".into()),
                            );
                        }
                        receipts.insert(id.to_owned(), (Instant::now(), signature, None));
                    }
                    let (reply, answer) = mpsc::channel();
                    {
                        let mut commands = shared.commands.lock().unwrap();
                        if shared.stop.load(Ordering::SeqCst) || commands.len() >= MAX_CONNECTIONS {
                            if let Some(id) = request_id
                                && let Some((_, _, cached)) =
                                    shared.receipts.lock().unwrap().get_mut(id)
                            {
                                *cached = Some(Err("Loom is unavailable".into()));
                            }
                            return respond(
                                &mut stream,
                                "503 Service Unavailable",
                                "application/json",
                                b"{\"ok\":false,\"error\":\"Loom is unavailable\"}",
                            );
                        }
                        commands.push(PendingCommand {
                            command,
                            deadline: Instant::now() + ACTION_TIMEOUT,
                            reply,
                        });
                    }
                    let result = answer.recv_timeout(ACTION_TIMEOUT).unwrap_or_else(|_| Err("Loom에서 요청을 확인하지 못했습니다. 상태를 확인한 뒤 다시 시도해주세요.".into()));
                    if let Some(id) = request_id
                        && let Some((_, _, cached)) = shared.receipts.lock().unwrap().get_mut(id)
                    {
                        *cached = Some(result.clone());
                    }
                    action_response(&mut stream, result)
                }
                None => respond(
                    &mut stream,
                    "400 Bad Request",
                    "application/json",
                    b"{\"ok\":false}",
                ),
            }
        }
        _ => respond(&mut stream, "404 Not Found", "text/plain", b"not found"),
    }
}

fn action_response(stream: &mut TcpStream, result: std::result::Result<(), String>) -> Result<()> {
    match result {
        Ok(()) => respond(stream, "200 OK", "application/json", b"{\"ok\":true}"),
        Err(error) => respond(
            stream,
            "409 Conflict",
            "application/json",
            serde_json::json!({"ok":false,"error":error})
                .to_string()
                .as_bytes(),
        ),
    }
}

/// The QR code of `text` as rows of modules (true = dark), with a two-module quiet zone.
pub fn qr_modules(text: &str) -> Vec<Vec<bool>> {
    // Upper case keeps an address in QR alphanumeric mode (hosts, schemes and the
    // token are case-insensitive), which needs fewer modules than byte mode.
    let text = text.to_ascii_uppercase();
    let Ok(code) = qrcode::QrCode::with_error_correction_level(text.as_bytes(), qrcode::EcLevel::L)
    else {
        return Vec::new();
    };
    let width = code.width();
    let quiet = 2;
    let size = width + 2 * quiet;
    (0..size)
        .map(|y| {
            (0..size)
                .map(|x| {
                    x >= quiet
                        && y >= quiet
                        && x < width + quiet
                        && y < width + quiet
                        && code[(x - quiet, y - quiet)] == qrcode::Color::Dark
                })
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http(port: u16, request: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut out = Vec::new();
        stream.read_to_end(&mut out).unwrap();
        String::from_utf8_lossy(&out).into_owned()
    }

    fn action(
        remote: &Remote,
        body: &str,
        verdict: std::result::Result<(), String>,
    ) -> (String, Command) {
        let request = format!(
            "POST /api/action HTTP/1.1\r\nAuthorization: Bearer {}\r\nContent-Length: {}\r\n\r\n{body}",
            remote.token(),
            body.len()
        );
        let port = remote.port;
        let response = thread::spawn(move || http(port, &request));
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(pending) = remote.take_commands().pop() {
                let command = pending.command.clone();
                pending.complete(verdict);
                return (response.join().unwrap(), command);
            }
            assert!(Instant::now() < deadline, "action did not reach the window");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn requests_need_the_token_and_commands_reach_the_window() {
        let remote = Remote::start(Some(0)).unwrap();
        let port = remote.port;
        let token = remote.token().to_owned();
        // The bare page (for the installed app) holds no data; the API needs the token.
        assert!(http(port, "GET /api/state HTTP/1.1\r\n\r\n").starts_with("HTTP/1.1 401"));
        assert!(http(port, "GET /manifest.webmanifest HTTP/1.1\r\n\r\n").contains("standalone"));
        let icon = http(port, "GET /icon-192.png HTTP/1.1\r\n\r\n");
        assert!(icon.starts_with("HTTP/1.1 200") && icon.contains("image/png"));
        assert!(http(port, "GET /?t=wrong HTTP/1.1\r\n\r\n").starts_with("HTTP/1.1 401"));
        let page = http(port, &format!("GET /t/{token} HTTP/1.1\r\n\r\n"));
        assert!(page.starts_with("HTTP/1.1 200") && page.contains("<html"));
        let scanned = http(
            port,
            &format!("GET /T/{} HTTP/1.1\r\n\r\n", token.to_ascii_lowercase()),
        );
        assert!(
            scanned.starts_with("HTTP/1.1 200"),
            "token case does not matter"
        );
        assert!(http(port, "GET /t/AAAA HTTP/1.1\r\n\r\n").starts_with("HTTP/1.1 401"));

        remote.publish(&serde_json::json!({"status":"대기 중"}));
        let state = http(
            port,
            &format!("GET /api/state?since=0 HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        assert!(
            state.contains("\"version\":1") && state.contains("대기 중"),
            "{state}"
        );

        let invalid_action = |body: &str| {
            http(
                port,
                &format!(
                    "POST /api/action HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                ),
            )
        };
        let (sent, send) = action(
            &remote,
            r#"{"action":"send","text":"run the tests"}"#,
            Ok(()),
        );
        assert!(sent.starts_with("HTTP/1.1 200"));
        let (_, reply) = action(
            &remote,
            r#"{"action":"reply","id":"7","choice":"accept"}"#,
            Ok(()),
        );
        let (_, model) = action(
            &remote,
            r#"{"action":"model","model":"lmx/native/a"}"#,
            Ok(()),
        );
        assert!(invalid_action(r#"{"action":"send","text":"  "}"#).starts_with("HTTP/1.1 400"));
        assert_eq!(
            vec![send, reply, model],
            vec![
                Command::Send("run the tests".into()),
                Command::Reply {
                    id: "7".into(),
                    choice: "accept".into()
                },
                Command::Model("lmx/native/a".into()),
            ]
        );
        let forged = http(
            port,
            "POST /api/action HTTP/1.1\r\nAuthorization: Bearer nope\r\nContent-Length: 22\r\n\r\n{\"action\":\"interrupt\"}",
        );
        assert!(forged.starts_with("HTTP/1.1 401"));
        assert!(remote.take_commands().is_empty());
        assert_eq!(
            remote.clients(),
            1,
            "an immediate poll remains a recently connected browser"
        );
    }

    #[test]
    fn actions_report_application_rejection_and_parse_explicit_question_answers() {
        let remote = Remote::start(Some(0)).unwrap();
        let (response, _) = action(
            &remote,
            r#"{"action":"model","model":"gone"}"#,
            Err("model unavailable".into()),
        );
        assert!(response.starts_with("HTTP/1.1 409") && response.contains("model unavailable"));
        let (_, command) = action(
            &remote,
            r#"{"action":"answer","id":"request","answers":[{"question_id":"q","option":1}]}"#,
            Ok(()),
        );
        assert_eq!(
            command,
            Command::Answer {
                id: "request".into(),
                answers: vec![crate::agent::QuestionAnswer {
                    question_id: "q".into(),
                    value: crate::agent::AnswerValue::Option(1)
                }]
            }
        );
        assert!(Command::parse(&serde_json::json!({"action":"answer","id":"request","answers":[{"question_id":"q","option":0,"text":"ambiguous"}]})).is_none());
    }

    #[test]
    fn retrying_a_received_action_does_not_submit_it_twice() {
        let remote = Remote::start(Some(0)).unwrap();
        let body = r#"{"action":"send","text":"one task","request_id":"retry-1"}"#;
        let (response, _) = action(&remote, body, Ok(()));
        assert!(response.starts_with("HTTP/1.1 200"));
        let duplicate = http(
            remote.port,
            &format!(
                "POST /api/action HTTP/1.1\r\nAuthorization: Bearer {}\r\nContent-Length: {}\r\n\r\n{body}",
                remote.token(),
                body.len()
            ),
        );
        assert!(duplicate.starts_with("HTTP/1.1 200"));
        assert!(remote.take_commands().is_empty());
    }

    #[test]
    fn split_request_body_waits_for_the_remaining_bytes() {
        let remote = Remote::start(Some(0)).unwrap();
        let body = r#"{"action":"send","text":"split packets"}"#;
        let mut stream = TcpStream::connect(("127.0.0.1", remote.port)).unwrap();
        write!(
            stream,
            "POST /api/action HTTP/1.1\r\nAuthorization: Bearer {}\r\nContent-Length: {}\r\n\r\n{}",
            remote.token(),
            body.len(),
            &body[..10]
        )
        .unwrap();
        thread::sleep(Duration::from_millis(100));
        assert!(remote.take_commands().is_empty());
        stream.write_all(body[10..].as_bytes()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(pending) = remote.take_commands().pop() {
                assert_eq!(pending.command, Command::Send("split packets".into()));
                pending.complete(Ok(()));
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
        let mut response = String::new();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200"));
    }

    #[test]
    #[cfg(unix)]
    fn failed_and_stalled_tunnels_keep_the_local_server_available() {
        let mut remote = Remote::start(Some(0)).unwrap();
        remote.tunnel = Some(Process::new("sh").args(["-c", "exit 7"]).spawn().unwrap());
        *remote.public.lock().unwrap() = Some("https://expired.trycloudflare.com".into());
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if remote.tunnel_status().is_err() {
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
        assert!(remote.public_url().is_none() && remote.tunnel.is_none());
        remote.tunnel = Some(Process::new("sleep").arg("5").spawn().unwrap());
        remote.tunnel_started = Some(Instant::now() - Duration::from_secs(31));
        assert!(remote.tunnel_status().is_err());
        assert!(http(remote.port, "GET / HTTP/1.1\r\n\r\n").starts_with("HTTP/1.1 200"));
    }

    #[test]
    fn malformed_length_and_large_headers_are_rejected() {
        let remote = Remote::start(Some(0)).unwrap();
        assert!(
            http(
                remote.port,
                "POST /api/action HTTP/1.1\r\nContent-Length: invalid\r\n\r\n"
            )
            .starts_with("HTTP/1.1 400")
        );
        assert!(
            http(
                remote.port,
                "POST /api/action HTTP/1.1\r\nContent-Length: 0\r\nContent-Length: 1\r\n\r\n"
            )
            .starts_with("HTTP/1.1 400")
        );
        assert!(
            http(
                remote.port,
                &format!("GET /{} HTTP/1.1\r\n\r\n", "x".repeat(MAX_HEADERS + 1))
            )
            .starts_with("HTTP/1.1 400")
        );
    }

    #[test]
    fn stopping_closes_long_polls_and_partially_written_requests() {
        let remote = Remote::start(Some(0)).unwrap();
        let mut poll = TcpStream::connect(("127.0.0.1", remote.port)).unwrap();
        write!(
            poll,
            "GET /api/state?since=0 HTTP/1.1\r\nAuthorization: Bearer {}\r\n\r\n",
            remote.token()
        )
        .unwrap();
        let mut partial = TcpStream::connect(("127.0.0.1", remote.port)).unwrap();
        partial.write_all(b"GET / HTTP/1.1\r\nX-Partial: ").unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let count = { remote.shared.connections.lock().unwrap().len() };
            if count >= 2 {
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
        let started = Instant::now();
        drop(remote);
        for mut stream in [poll, partial] {
            stream
                .set_read_timeout(Some(Duration::from_millis(500)))
                .unwrap();
            let mut byte = [0u8; 1];
            match stream.read(&mut byte) {
                Ok(0) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                    ) => {}
                other => panic!("request was not closed: {other:?}"),
            }
        }
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn tunnel_address_is_read_from_cloudflared_logs() {
        let line = "2026-10-10T05:00:00Z INF |  https://quiet-river-1234.trycloudflare.com                                 |";
        assert_eq!(
            trycloudflare_url(line),
            Some("https://quiet-river-1234.trycloudflare.com")
        );
        assert_eq!(
            trycloudflare_url("INF Requesting new quick Tunnel on trycloudflare.com..."),
            None
        );
        assert_eq!(
            trycloudflare_url("see https://developers.cloudflare.com/x"),
            None
        );
    }

    #[test]
    fn stopping_closes_the_listener() {
        let remote = Remote::start(Some(0)).unwrap();
        let (port, token) = (remote.port, remote.token().to_owned());
        drop(remote);
        // Another test may take the freed port; what matters is that this server is gone.
        let answered = TcpStream::connect_timeout(
            &SocketAddr::from(([127, 0, 0, 1], port)),
            Duration::from_millis(300),
        )
        .is_ok_and(|_| {
            http(port, &format!("GET /t/{token} HTTP/1.1\r\n\r\n")).starts_with("HTTP/1.1 200")
        });
        assert!(!answered);
    }

    #[test]
    fn qr_code_is_square_with_a_light_border() {
        let rows = qr_modules("http://192.168.0.2:8787/t/ABCDEFGHIJKLMNOPQRSTUVWXYZ");
        assert!(rows.len() > 20);
        // Alphanumeric mode keeps a local address at version 3 (29 modules + border).
        assert!(rows.len() <= 33, "{}", rows.len());
        let public = qr_modules(
            "https://quiet-river-sun-1234.trycloudflare.com/t/ABCDEFGHIJKLMNOPQRSTUVWXYZ",
        );
        assert!(public.len() <= 41, "{}", public.len());
        assert!(rows.iter().all(|r| r.len() == rows.len()));
        assert!(
            rows[0].iter().all(|dark| !dark) && rows[2][2],
            "quiet zone, then a finder pattern"
        );
    }
}
