//! Remote control: a small web server that mirrors the open conversation and accepts
//! messages, interrupts and approval answers from a browser (a phone on the same
//! network, for example).
//!
//! Every request needs the session's random 128-bit token (in the page URL, then as a
//! bearer header). The server only runs while remote control is on; turning it off
//! closes the listener and every waiting request. Traffic is plain HTTP, so the token is
//! visible to anyone who can read the local network; the UI says so.
use anyhow::{Context, Result};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

const PAGE: &str = include_str!("remote.html");
const MAX_BODY: usize = 64 * 1024;
const LONG_POLL: Duration = Duration::from_secs(20);
const DEFAULT_PORT: u16 = 8787;

/// What a browser asked the open window to do.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    Send(String),
    Interrupt,
    Reply { id: String, choice: String },
}

struct Shared {
    /// (version, JSON snapshot of the window).
    state: Mutex<(u64, String)>,
    changed: Condvar,
    commands: Mutex<Vec<Command>>,
    stop: AtomicBool,
    clients: AtomicUsize,
}

pub struct Remote {
    shared: Arc<Shared>,
    token: String,
    pub port: u16,
    pub urls: Vec<String>,
    accept: Option<thread::JoinHandle<()>>,
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
            clients: AtomicUsize::new(0),
        });
        let mut urls = Vec::new();
        if let Some(ip) = lan_address() {
            urls.push(format!("http://{ip}:{port}/?t={token}"));
        }
        urls.push(format!("http://127.0.0.1:{port}/?t={token}"));
        let accept = {
            let shared = shared.clone();
            let token = token.clone();
            thread::Builder::new()
                .name("loom-remote".into())
                .spawn(move || accept_loop(listener, shared, token))?
        };
        Ok(Self { shared, token, port, urls, accept: Some(accept) })
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

    pub fn take_commands(&self) -> Vec<Command> {
        std::mem::take(&mut *self.shared.commands.lock().unwrap())
    }

    /// Browsers that asked for state within the last long-poll window.
    pub fn clients(&self) -> usize {
        self.shared.clients.load(Ordering::Relaxed)
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
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
        let _ = &self.token;
    }
}

fn token() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).map_err(|e| anyhow::anyhow!("random token: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
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
            Ok((stream, peer)) => {
                let shared = shared.clone();
                let token = token.clone();
                let _ = thread::Builder::new()
                    .name("loom-remote-client".into())
                    .spawn(move || {
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
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().context("request line")?.to_owned();
    let target = parts.next().context("request target")?.to_owned();
    let (path, query) = target.split_once('?').map_or((target.clone(), String::new()), |(p, q)| (p.into(), q.into()));
    let mut length = 0usize;
    let mut authorization = None;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 || header.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            match name.trim().to_ascii_lowercase().as_str() {
                "content-length" => length = value.trim().parse().unwrap_or(0),
                "authorization" => authorization = Some(value.trim().to_owned()),
                _ => {}
            }
        }
    }
    anyhow::ensure!(length <= MAX_BODY, "body too large");
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    Ok(Request { method, path, query, authorization, body })
}

fn query_value<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| pair.split_once('=').filter(|(k, _)| *k == key).map(|(_, v)| v))
}

/// Constant-time comparison, so response timing says nothing about the token.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
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

fn serve(mut stream: TcpStream, _peer: SocketAddr, shared: &Shared, token: &str) -> Result<()> {
    let request = match read_request(&mut stream) {
        Ok(request) => request,
        Err(_) => return respond(&mut stream, "400 Bad Request", "text/plain", b"bad request"),
    };
    let presented = query_value(&request.query, "t")
        .map(str::to_owned)
        .or_else(|| request.authorization.as_deref().and_then(|a| a.strip_prefix("Bearer ")).map(str::to_owned));
    if !presented.is_some_and(|t| same(&t, token)) {
        // Slow down guessing; a 128-bit token is out of reach anyway.
        thread::sleep(Duration::from_millis(300));
        return respond(&mut stream, "401 Unauthorized", "text/plain; charset=utf-8", "토큰이 맞지 않습니다. Loom에 표시된 주소로 접속하세요.".as_bytes());
    }
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/") => respond(&mut stream, "200 OK", "text/html; charset=utf-8", PAGE.as_bytes()),
        ("GET", "/api/state") => {
            let since: u64 = query_value(&request.query, "since").and_then(|v| v.parse().ok()).unwrap_or(0);
            shared.clients.fetch_add(1, Ordering::Relaxed);
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
            shared.clients.fetch_sub(1, Ordering::Relaxed);
            respond(&mut stream, "200 OK", "application/json", body.as_bytes())
        }
        ("POST", "/api/send" | "/api/interrupt" | "/api/reply") => {
            let value: serde_json::Value = serde_json::from_slice(&request.body).unwrap_or_default();
            let command = match request.path.as_str() {
                "/api/send" => value["text"]
                    .as_str()
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .map(|t| Command::Send(t.to_owned())),
                "/api/interrupt" => Some(Command::Interrupt),
                _ => match (value["id"].as_str(), value["choice"].as_str()) {
                    (Some(id), Some(choice)) => Some(Command::Reply { id: id.into(), choice: choice.into() }),
                    _ => None,
                },
            };
            match command {
                Some(command) => {
                    shared.commands.lock().unwrap().push(command);
                    respond(&mut stream, "202 Accepted", "application/json", b"{\"ok\":true}")
                }
                None => respond(&mut stream, "400 Bad Request", "application/json", b"{\"ok\":false}"),
            }
        }
        _ => respond(&mut stream, "404 Not Found", "text/plain", b"not found"),
    }
}

/// The QR code of `text` as rows of modules (true = dark), with a two-module quiet zone.
pub fn qr_modules(text: &str) -> Vec<Vec<bool>> {
    let Ok(code) = qrcode::QrCode::with_error_correction_level(text.as_bytes(), qrcode::EcLevel::L) else {
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
        let mut out = String::new();
        stream.read_to_string(&mut out).unwrap();
        out
    }

    #[test]
    fn requests_need_the_token_and_commands_reach_the_window() {
        let remote = Remote::start(Some(0)).unwrap();
        let port = remote.port;
        let token = remote.token().to_owned();
        assert!(http(port, "GET / HTTP/1.1\r\n\r\n").starts_with("HTTP/1.1 401"));
        assert!(http(port, "GET /?t=wrong HTTP/1.1\r\n\r\n").starts_with("HTTP/1.1 401"));
        let page = http(port, &format!("GET /?t={token} HTTP/1.1\r\n\r\n"));
        assert!(page.starts_with("HTTP/1.1 200") && page.contains("<html"));

        remote.publish(&serde_json::json!({"status":"대기 중"}));
        let state = http(port, &format!("GET /api/state?since=0 HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"));
        assert!(state.contains("\"version\":1") && state.contains("대기 중"), "{state}");

        let body = r#"{"text":"run the tests"}"#;
        let sent = http(port, &format!(
            "POST /api/send HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ));
        assert!(sent.starts_with("HTTP/1.1 202"), "{sent}");
        let body = r#"{"id":"7","choice":"accept"}"#;
        http(port, &format!(
            "POST /api/reply HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ));
        assert_eq!(
            remote.take_commands(),
            vec![Command::Send("run the tests".into()), Command::Reply { id: "7".into(), choice: "accept".into() }]
        );
        let forged = http(port, "POST /api/interrupt HTTP/1.1\r\nAuthorization: Bearer nope\r\nContent-Length: 0\r\n\r\n");
        assert!(forged.starts_with("HTTP/1.1 401"));
        assert!(remote.take_commands().is_empty());
    }

    #[test]
    fn stopping_closes_the_listener() {
        let remote = Remote::start(Some(0)).unwrap();
        let (port, token) = (remote.port, remote.token().to_owned());
        drop(remote);
        // Another test may take the freed port; what matters is that this server is gone.
        let answered = TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_millis(300))
            .is_ok_and(|_| http(port, &format!("GET /?t={token} HTTP/1.1\r\n\r\n")).starts_with("HTTP/1.1 200"));
        assert!(!answered);
    }

    #[test]
    fn qr_code_is_square_with_a_light_border() {
        let rows = qr_modules("http://192.168.0.2:8787/?t=00112233445566778899aabbccddeeff");
        assert!(rows.len() > 20);
        assert!(rows.iter().all(|r| r.len() == rows.len()));
        assert!(rows[0].iter().all(|dark| !dark) && rows[2][2], "quiet zone, then a finder pattern");
    }
}
