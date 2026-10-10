// Path import keeps this gate runnable before the main agent exports the module.
mod models {
    pub use loom_core::models::ToolSpec;
}
mod effects {
    pub use loom_core::effects::{ExecutionPolicy, SandboxMode};
}
#[path = "../src/extensions.rs"]
mod extensions;

use effects::{ExecutionPolicy, SandboxMode};
use extensions::Extensions;
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

const TIMEOUT: Duration = Duration::from_secs(3);

fn config(text: &str) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join(".loom")).unwrap();
    fs::write(dir.path().join(".loom/config.toml"), text).unwrap();
    dir
}

const SERVER: &str = r#"
import json, os, sys, subprocess, time
open('started', 'w').write(str(os.getpid()))
for line in sys.stdin:
    q = json.loads(line)
    method = q.get('method')
    if 'id' not in q: continue
    if method == 'initialize':
        assert q['params']['protocolVersion'] == '2025-03-26'
        r = {'protocolVersion':'2025-03-26','capabilities':{'tools':{},'resources':{}},'serverInfo':{'name':'fixture','version':'1'}}
    elif method == 'tools/list':
        if os.getenv('BAD_LIST'):
            print(json.dumps({'jsonrpc':'2.0','id':q['id'],'error':{'code':-32603,'message':'list failed explicitly'}}), flush=True)
            continue
        if q.get('params', {}).get('cursor') == 'page2':
            r = {'tools':[{'name':'echo?','inputSchema':{'type':'object'},'annotations':{'readOnlyHint':True}}]}
        else:
            r = {'tools':[{'name':'echo!','description':'fixture echo','inputSchema':{'type':'object','properties':{'text':{'type':'string'}}}}], 'nextCursor':'page2'}
    elif method == 'tools/call':
        if q['params']['arguments'].get('hang'):
            child = subprocess.Popen(['sleep','30'])
            open('child.pid','w').write(str(child.pid))
            time.sleep(30)
        if q['params']['arguments'].get('huge'):
            sys.stdout.write('x' * (2 * 1024 * 1024)); sys.stdout.flush(); time.sleep(30)
        if q['params']['arguments'].get('fail'):
            r = {'isError':True,'content':[{'type':'text','text':'tool failure'}]}
        else:
            r = {'content':[{'type':'text','text':q['params']['name'] + ':' + os.getenv('SUPPLIED','missing') + ':' + q['params']['arguments'].get('text','')}]}
    elif method == 'resources/list': r = {'resources':[{'uri':'fixture://one','name':'One'}]}
    elif method == 'resources/read': r = {'contents':[{'uri':q['params']['uri'],'text':'resource text'}]}
    else: r = {}
    print(json.dumps({'jsonrpc':'2.0','method':'notifications/progress','params':{}}), flush=True)
    print(json.dumps({'jsonrpc':'2.0','id':q['id'],'result':r}), flush=True)
"#;

fn stdio(extra: &str) -> TempDir {
    let dir = config(&format!(
        "[mcp_servers.fixture]\ncommand = 'python3'\nargs = ['-u', 'server.py']\n[mcp_servers.fixture.env]\nSUPPLIED = 'configured'\n{extra}\n"
    ));
    fs::write(dir.path().join("server.py"), SERVER).unwrap();
    dir
}

#[cfg(unix)]
#[test]
fn open_is_lazy_and_stdio_discovery_calls_and_resources_are_real() {
    let dir = stdio("");
    let ext = Extensions::open(dir.path()).unwrap();
    assert!(!dir.path().join("started").exists());
    let specs = ext.specs();
    let echoes: Vec<_> = specs
        .iter()
        .filter(|s| s.name.starts_with("mcp__fixture__echo"))
        .collect();
    assert_eq!(echoes.len(), 2);
    assert_ne!(echoes[0].name, echoes[1].name);
    assert_eq!(echoes[0].parameters["type"], "object");
    let cancel = AtomicBool::new(false);
    for spec in echoes {
        let out = ext
            .call(&spec.name, &json!({"text":"hello"}), &cancel, TIMEOUT)
            .unwrap();
        assert!(out.contains(":configured:hello"), "{out}");
    }
    let list = specs
        .iter()
        .find(|s| s.name.ends_with("__list_resources"))
        .unwrap();
    assert!(
        ext.call(&list.name, &json!({}), &cancel, TIMEOUT)
            .unwrap()
            .contains("fixture://one")
    );
    let read = specs
        .iter()
        .find(|s| s.name.ends_with("__read_resource"))
        .unwrap();
    assert!(
        ext.call(
            &read.name,
            &json!({"uri":"fixture://one"}),
            &cancel,
            TIMEOUT
        )
        .unwrap()
        .contains("resource text")
    );
    assert!(
        ext.call("unknown", &json!({}), &cancel, TIMEOUT)
            .unwrap_err()
            .to_string()
            .contains("unknown")
    );
}

#[cfg(unix)]
#[test]
fn discovery_failure_is_visible_and_tool_errors_are_errors() {
    let dir = stdio("BAD_LIST = '1'");
    let ext = Extensions::open(dir.path()).unwrap();
    let specs = ext.specs();
    let diagnostics = specs
        .iter()
        .find(|s| s.name == "extensions_diagnostics")
        .unwrap();
    assert!(diagnostics.description.contains("fixture"));
    let out = ext
        .call(
            &diagnostics.name,
            &json!({}),
            &AtomicBool::new(false),
            TIMEOUT,
        )
        .unwrap();
    assert!(out.contains("list failed explicitly"), "{out}");
    let good = stdio("");
    let ext = Extensions::open(good.path()).unwrap();
    let name = ext
        .specs()
        .into_iter()
        .find(|s| s.name.starts_with("mcp__fixture__echo"))
        .unwrap()
        .name;
    assert!(
        ext.call(
            &name,
            &json!({"fail":true}),
            &AtomicBool::new(false),
            TIMEOUT
        )
        .unwrap_err()
        .to_string()
        .contains("tool failure")
    );
}

#[test]
fn invalid_config_and_pre_cancelled_calls_do_not_start_processes() {
    let invalid = config("[mcp_servers.bad]\ncommand='sh'\nurl='http://localhost/mcp'\n");
    assert!(Extensions::open(invalid.path()).is_err());
    let dir = stdio("");
    let ext = Extensions::open(dir.path()).unwrap();
    assert!(
        ext.call(
            "mcp__fixture__echo_",
            &json!({}),
            &AtomicBool::new(true),
            TIMEOUT
        )
        .is_err()
    );
    assert!(
        ext.call(
            "mcp__fixture__echo_",
            &json!({}),
            &AtomicBool::new(false),
            Duration::ZERO
        )
        .is_err()
    );
    assert!(!dir.path().join("started").exists());
}

#[cfg(unix)]
#[test]
fn readonly_classification_requires_explicit_mcp_annotation() {
    let dir = stdio("");
    let ext = Extensions::open(dir.path()).unwrap();
    assert!(ext.is_read_only("web_search"));
    assert!(ext.is_read_only("web_fetch"));
    assert!(!ext.is_read_only("unknown"));
    let specs = ext.specs();
    let echoes: Vec<_> = specs
        .iter()
        .filter(|s| s.name.starts_with("mcp__fixture__echo"))
        .collect();
    assert_eq!(
        echoes.iter().filter(|s| ext.is_read_only(&s.name)).count(),
        1
    );
    assert!(!ext.is_read_only("mcp__fixture__read_resource"));
    assert!(ext.requires_network("web_search"));
    assert!(ext.requires_network("web_fetch"));
    assert!(!ext.requires_network(&echoes[0].name));
}

#[cfg(unix)]
fn stopped(pid: i32) -> bool {
    let out = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    out.stdout.is_empty() || String::from_utf8_lossy(&out.stdout).trim().starts_with('Z')
}

#[cfg(unix)]
#[test]
fn cancellation_deadline_and_drop_reap_process_groups() {
    for cancelled in [true, false] {
        let dir = stdio("");
        let ext = Extensions::open(dir.path()).unwrap();
        let name = ext
            .specs()
            .into_iter()
            .find(|s| s.name.starts_with("mcp__fixture__echo"))
            .unwrap()
            .name;
        let cancel = Arc::new(AtomicBool::new(false));
        let trigger = cancel.clone();
        let child_file = dir.path().join("child.pid");
        let watched = child_file.clone();
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + TIMEOUT;
            while !watched.exists() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            if cancelled {
                trigger.store(true, Ordering::SeqCst);
            }
        });
        let start = Instant::now();
        let err = ext
            .call(
                &name,
                &json!({"hang":true}),
                &cancel,
                Duration::from_millis(350),
            )
            .unwrap_err();
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(
            err.to_string()
                .contains(if cancelled { "cancel" } else { "deadline" }),
            "{err:#}"
        );
        worker.join().unwrap();
        let child: i32 = fs::read_to_string(child_file).unwrap().parse().unwrap();
        let parent: i32 = fs::read_to_string(dir.path().join("started"))
            .unwrap()
            .parse()
            .unwrap();
        assert!(stopped(child));
        assert!(stopped(parent));
    }
    let dir = stdio("");
    let ext = Extensions::open(dir.path()).unwrap();
    ext.specs();
    let pid: i32 = fs::read_to_string(dir.path().join("started"))
        .unwrap()
        .parse()
        .unwrap();
    drop(ext);
    assert!(stopped(pid));
}

#[cfg(unix)]
#[test]
fn oversized_stdio_message_is_rejected_promptly() {
    let dir = stdio("");
    let ext = Extensions::open(dir.path()).unwrap();
    let name = ext
        .specs()
        .into_iter()
        .find(|s| s.name.starts_with("mcp__fixture__echo"))
        .unwrap()
        .name;
    let err = ext
        .call(
            &name,
            &json!({"huge":true}),
            &AtomicBool::new(false),
            TIMEOUT,
        )
        .unwrap_err();
    assert!(err.to_string().contains("limit"), "{err:#}");
}

type Handler = dyn Fn(&str, &str, &str) -> (String, String, String) + Send + Sync;
struct Http {
    url: String,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Http {
    fn new(
        handler: impl Fn(&str, &str, &str) -> (String, String, String) + Send + Sync + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let done = stop.clone();
        let handler: Arc<Handler> = Arc::new(handler);
        let worker = thread::spawn(move || {
            let mut connections = Vec::new();
            while !done.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let handler = handler.clone();
                        let seen = seen.clone();
                        connections.push(thread::spawn(move || serve(stream, handler, seen)));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(e) => panic!("{e}"),
                }
            }
            for connection in connections {
                connection.join().unwrap();
            }
        });
        Self {
            url,
            requests,
            stop,
            worker: Some(worker),
        }
    }
}
impl Drop for Http {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.worker.take().unwrap().join().unwrap();
    }
}
fn serve(mut stream: TcpStream, handler: Arc<Handler>, seen: Arc<Mutex<Vec<String>>>) {
    // On BSD/macOS an accepted socket inherits the listener's non-blocking mode, which
    // made reads fail with WouldBlock depending on timing.
    stream.set_nonblocking(false).unwrap();
    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
    let mut bytes = Vec::new();
    let mut buf = [0; 4096];
    let split = loop {
        let n = stream.read(&mut buf).unwrap();
        if n == 0 {
            return;
        }
        bytes.extend_from_slice(&buf[..n]);
        if let Some(pos) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let header = String::from_utf8(bytes[..split].to_vec()).unwrap();
    let len: usize = header
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|s| s.trim().parse().unwrap())
        })
        .unwrap_or(0);
    while bytes.len() < split + len {
        let n = stream.read(&mut buf).unwrap();
        if n == 0 {
            return;
        }
        bytes.extend_from_slice(&buf[..n]);
    }
    let body = String::from_utf8(bytes[split..split + len].to_vec()).unwrap();
    seen.lock().unwrap().push(format!("{header}{body}"));
    let path = header.split_whitespace().nth(1).unwrap();
    let (status, extra, body) = handler(path, &header, &body);
    let response = format!(
        "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {}\r\n{extra}\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

#[test]
fn streamable_http_initializes_preserves_session_headers_and_reads_sse() {
    let http = Http::new(|_, header, body| {
        let q: Value = serde_json::from_str(body).unwrap();
        let method = q["method"].as_str().unwrap_or("");
        assert!(
            header
                .to_ascii_lowercase()
                .contains("x-configured: supplied")
        );
        if method == "initialize" {
            let r = json!({"jsonrpc":"2.0","id":q["id"],"result":{"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"http","version":"1"}}});
            return (
                "200 OK".into(),
                "Content-Type: application/json\r\nMcp-Session-Id: session-one\r\n".into(),
                r.to_string(),
            );
        }
        assert!(
            header
                .to_ascii_lowercase()
                .contains("mcp-session-id: session-one")
        );
        assert!(
            header
                .to_ascii_lowercase()
                .contains("mcp-protocol-version: 2025-03-26")
        );
        if method.is_empty() {
            assert_eq!(q["id"], 999);
            assert_eq!(q["result"], json!({}));
            return ("202 Accepted".into(), String::new(), String::new());
        }
        if method == "notifications/initialized" {
            return ("202 Accepted".into(), String::new(), String::new());
        }
        let result = if method == "tools/list" {
            json!({"tools":[{"name":"remote","inputSchema":{"type":"object"}}]})
        } else {
            json!({"content":[{"type":"text","text":"http real result"}]})
        };
        let r = json!({"jsonrpc":"2.0","id":q["id"],"result":result});
        (
            "200 OK".into(),
            "Content-Type: text/event-stream\r\n".into(),
            format!(
                "event: message\ndata: {{\"jsonrpc\":\"2.0\",\"id\":999,\"method\":\"ping\"}}\n\nevent: message\ndata: {r}\n\n"
            ),
        )
    });
    let dir = config(&format!(
        "[mcp_servers.remote]\nurl = '{}/mcp'\n[mcp_servers.remote.headers]\nX-Configured = 'supplied'\n",
        http.url
    ));
    let ext = Extensions::open(dir.path()).unwrap();
    assert!(http.requests.lock().unwrap().is_empty());
    assert!(
        !ext.specs_with_network(false)
            .iter()
            .any(|s| s.name.starts_with("mcp__remote"))
    );
    assert!(http.requests.lock().unwrap().is_empty());
    let spec = ext
        .specs()
        .into_iter()
        .find(|s| s.name == "mcp__remote__remote")
        .unwrap();
    assert!(ext.requires_network(&spec.name));
    assert!(
        ext.call(&spec.name, &json!({}), &AtomicBool::new(false), TIMEOUT)
            .unwrap()
            .contains("http real result")
    );
}

#[test]
fn search_and_fetch_return_bounded_attributed_text_and_strip_active_html() {
    let http = Http::new(|path, _, _| {
        if path.starts_with("/search?") {
            assert!(path.contains("q=rust%20mcp"), "{path}");
            ("200 OK".into(), "Content-Type: application/json\r\n".into(), json!({"results":[{"title":"MCP & Rust","url":"https://example.org/reference","content":"actual indexed snippet"}]}).to_string())
        } else {
            (
                "200 OK".into(),
                "Content-Type: text/html\r\n".into(),
                format!(
                    "<html><style>secret-style</style><script>secret-script</script><h1>Hello &amp; world</h1><p>{}</p></html>",
                    "visible ".repeat(20000)
                ),
            )
        }
    });
    let dir = config(&format!(
        "[web_search]\nprovider='searxng'\nendpoint='{}/search'\n",
        http.url
    ));
    let ext = Extensions::open(dir.path()).unwrap();
    let cancel = AtomicBool::new(false);
    let out = ext
        .call("web_search", &json!({"query":"rust mcp"}), &cancel, TIMEOUT)
        .unwrap();
    assert!(
        out.contains("actual indexed snippet") && out.contains("https://example.org/reference"),
        "{out}"
    );
    let out = ext
        .call(
            "web_fetch",
            &json!({"url":format!("{}/page", http.url)}),
            &cancel,
            TIMEOUT,
        )
        .unwrap();
    assert!(out.contains("Hello & world") && out.contains(&http.url));
    assert!(!out.contains("secret-script") && !out.contains("secret-style"));
    assert!(out.len() <= 32 * 1024);
}

#[test]
fn brave_uses_only_configured_credentials_and_missing_search_is_explicit() {
    let empty = tempfile::tempdir().unwrap();
    let ext = Extensions::open(empty.path()).unwrap();
    let err = ext
        .call(
            "web_search",
            &json!({"query":"test"}),
            &AtomicBool::new(false),
            TIMEOUT,
        )
        .unwrap_err();
    assert!(err.to_string().contains("config"), "{err:#}");
    let http = Http::new(|_, header, _| {
        assert!(
            header
                .to_ascii_lowercase()
                .contains("x-subscription-token: explicit-key")
        );
        ("200 OK".into(), "Content-Type: application/json\r\n".into(), json!({"web":{"results":[{"title":"Brave result","url":"https://example.com/brave","description":"real result"}]}}).to_string())
    });
    let dir = config(&format!(
        "[web_search]\nprovider='brave'\nendpoint='{}/search'\napi_key='explicit-key'\n",
        http.url
    ));
    let ext = Extensions::open(dir.path()).unwrap();
    assert!(
        ext.call(
            "web_search",
            &json!({"query":"test"}),
            &AtomicBool::new(false),
            TIMEOUT
        )
        .unwrap()
        .contains("https://example.com/brave")
    );
}

#[test]
fn searxng_html_extracts_real_result_links_and_rejects_challenge_pages() {
    let http = Http::new(|path, _, _| {
        assert!(path.contains("format=html"));
        let body = if path.contains("q=blocked") {
            "<html><h1>Please verify you are human</h1></html>"
        } else {
            r#"<html><a href="https://navigation.invalid">Navigation</a>
            <article class="result default"><h3><a href="https://example.org/html?q=1&amp;x=2">Actual &#x1F980; result</a></h3>
            <p class="content">Indexed &amp; useful snippet.</p></article></html>"#
        };
        (
            "200 OK".into(),
            "Content-Type: text/html\r\n".into(),
            body.into(),
        )
    });
    let dir = config(&format!(
        "[web_search]\nprovider='searxng'\nendpoint='{}/search'\nformat='html'\n",
        http.url
    ));
    let ext = Extensions::open(dir.path()).unwrap();
    let out = ext
        .call(
            "web_search",
            &json!({"query":"html"}),
            &AtomicBool::new(false),
            TIMEOUT,
        )
        .unwrap();
    assert!(out.contains("Actual 🦀 result"));
    assert!(out.contains("https://example.org/html?q=1&x=2"));
    assert!(!out.contains("navigation.invalid"));
    assert!(
        ext.call(
            "web_search",
            &json!({"query":"blocked"}),
            &AtomicBool::new(false),
            TIMEOUT
        )
        .is_err()
    );
}

#[cfg(unix)]
#[test]
fn server_name_collisions_are_disambiguated_without_misrouting() {
    let dir = config(
        "[mcp_servers.'a-b']\ncommand='python3'\nargs=['-u','server.py']\n[mcp_servers.'a-b'.env]\nSUPPLIED='first'\n[mcp_servers.a_b]\ncommand='python3'\nargs=['-u','server.py']\n[mcp_servers.a_b.env]\nSUPPLIED='second'\n",
    );
    fs::write(dir.path().join("server.py"), SERVER).unwrap();
    let ext = Extensions::open(dir.path()).unwrap();
    let specs = ext.specs();
    let names: std::collections::BTreeSet<_> = specs.iter().map(|s| &s.name).collect();
    assert_eq!(names.len(), specs.len());
    let outputs: Vec<_> = specs
        .iter()
        .filter(|s| s.name.starts_with("mcp__a_b__echo"))
        .map(|s| {
            ext.call(&s.name, &json!({}), &AtomicBool::new(false), TIMEOUT)
                .unwrap()
        })
        .collect();
    assert_eq!(outputs.len(), 4);
    assert_eq!(outputs.iter().filter(|s| s.contains(":first:")).count(), 2);
    assert_eq!(outputs.iter().filter(|s| s.contains(":second:")).count(), 2);
}

#[cfg(unix)]
#[test]
fn network_denied_discovery_and_stdio_call_never_contact_http_servers() {
    let http = Http::new(|_, _, _| ("503 Unavailable".into(), String::new(), String::new()));
    let dir = config(&format!(
        "[mcp_servers.remote]\nurl='{}/mcp'\n[mcp_servers.local]\ncommand='python3'\nargs=['-u','server.py']\n",
        http.url
    ));
    fs::write(dir.path().join("server.py"), SERVER).unwrap();
    let ext = Extensions::open(dir.path()).unwrap();
    let specs = ext.specs_with_network(false);
    assert!(specs.iter().all(|s| !ext.requires_network(&s.name)));
    let name = &specs
        .iter()
        .find(|s| s.name.starts_with("mcp__local__echo"))
        .unwrap()
        .name;
    assert!(
        ext.call(
            name,
            &json!({"text":"network denied"}),
            &AtomicBool::new(false),
            TIMEOUT
        )
        .unwrap()
        .contains("network denied")
    );
    assert!(http.requests.lock().unwrap().is_empty());
}

#[cfg(not(unix))]
#[test]
fn non_unix_stdio_surfaces_diagnostic_and_network_denied_skips_http() {
    let http = Http::new(|_, _, _| ("503 Unavailable".into(), String::new(), String::new()));
    let dir = config(&format!(
        "[mcp_servers.remote]\nurl='{}/mcp'\n[mcp_servers.local]\ncommand='python3'\nargs=['-u','server.py']\n",
        http.url
    ));
    let ext = Extensions::open(dir.path()).unwrap();
    assert!(ext.is_read_only("web_search"));
    assert!(ext.is_read_only("web_fetch"));
    assert!(ext.requires_network("web_search"));
    assert!(ext.requires_network("web_fetch"));
    let specs = ext.specs_with_network(false);
    assert!(specs.iter().all(|s| !ext.requires_network(&s.name)));
    assert!(http.requests.lock().unwrap().is_empty());
    let diagnostics = specs
        .iter()
        .find(|s| s.name == "extensions_diagnostics")
        .unwrap();
    let out = ext
        .call(
            &diagnostics.name,
            &json!({}),
            &AtomicBool::new(false),
            TIMEOUT,
        )
        .unwrap();
    assert!(
        out.contains("MCP stdio process-group supervision requires Unix"),
        "{out}"
    );
}

#[test]
fn http_cancellation_and_oversized_body_are_bounded() {
    let http = Http::new(|_, _, _| {
        thread::sleep(Duration::from_millis(500));
        (
            "200 OK".into(),
            "Content-Type: text/plain\r\n".into(),
            "x".repeat(2 * 1024 * 1024),
        )
    });
    let dir = tempfile::tempdir().unwrap();
    let ext = Extensions::open(dir.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let trigger = cancel.clone();
    let worker = thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        trigger.store(true, Ordering::SeqCst);
    });
    let start = Instant::now();
    assert!(
        ext.call(
            "web_fetch",
            &json!({"url":http.url}),
            &cancel,
            Duration::from_secs(1)
        )
        .unwrap_err()
        .to_string()
        .contains("cancel")
    );
    assert!(start.elapsed() < Duration::from_millis(400));
    worker.join().unwrap();
    let err = ext
        .call(
            "web_fetch",
            &json!({"url":http.url}),
            &AtomicBool::new(false),
            TIMEOUT,
        )
        .unwrap_err();
    assert!(err.to_string().contains("limit"), "{err:#}");
}

const ISOLATED_SERVER: &str = r#"
import json, os, socket, sys, tempfile
def attempt(path):
    try:
        with open(path, 'w') as f: f.write('mcp edited')
        return 'written'
    except OSError: return 'denied'
for line in sys.stdin:
    q = json.loads(line)
    if 'id' not in q: continue
    method = q['method']
    if method == 'initialize':
        r = {'protocolVersion':'2025-03-26','capabilities':{'tools':{}},'serverInfo':{'name':'isolated','version':'1'}}
    elif method == 'tools/list':
        r = {'tools':[{'name':'probe','inputSchema':{'type':'object'},'annotations':{'readOnlyHint':True}}, {'name':'write','inputSchema':{'type':'object'}}]}
    elif method == 'tools/call':
        args = q['params']['arguments']
        if 'port' in args:
            try:
                s = socket.create_connection(('127.0.0.1', args['port']), timeout=0.3)
                s.sendall(b'probe'); s.close(); network = 'connected'
            except OSError: network = 'denied'
            value = {'network':network}
        else:
            fd, temp = tempfile.mkstemp(); os.write(fd, b'private'); os.close(fd)
            value = {'cwd':os.getcwd(), 'temp':temp, 'local':attempt('local.txt'),
                     'original':attempt(args['original']), 'outside':attempt(args['outside']),
                     'target':attempt(os.path.join(os.environ['CARGO_TARGET_DIR'], 'built')),
                     'artifact':attempt(args['artifact'])}
        r = {'content':[{'type':'text','text':json.dumps(value)}]}
    print(json.dumps({'jsonrpc':'2.0','id':q['id'],'result':r}), flush=True)
"#;

fn isolated_config() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("isolated.py"), ISOLATED_SERVER).unwrap();
    fs::create_dir(dir.path().join(".loom")).unwrap();
    let script =
        toml::Value::String(dir.path().join("isolated.py").to_str().unwrap().into()).to_string();
    fs::write(
        dir.path().join(".loom/config.toml"),
        format!("[mcp_servers.fixture]\ncommand='python3'\nargs=['-u', {script}]\n"),
    )
    .unwrap();
    dir
}

fn restricted_specs(ext: &Extensions) -> Option<Vec<loom_core::models::ToolSpec>> {
    let specs = ext.specs();
    if specs.iter().any(|s| s.name.starts_with("mcp__fixture__")) {
        return Some(specs);
    }
    let diagnostic = ext
        .call(
            "extensions_diagnostics",
            &json!({}),
            &AtomicBool::new(false),
            TIMEOUT,
        )
        .unwrap();
    // A missing sandbox backend may only fail closed. Do not skip a broken
    // available sandbox or a stdio-hook/initialization regression.
    #[cfg(target_os = "linux")]
    if !std::path::Path::new("/usr/bin/bwrap").is_file()
        && !std::path::Path::new("/bin/bwrap").is_file()
    {
        assert!(diagnostic.contains("bwrap unavailable"), "{diagnostic}");
        return None;
    }
    panic!("sandboxed MCP discovery failed: {diagnostic}");
}

#[cfg(unix)]
#[test]
fn workspace_config_is_original_but_stdio_writes_only_execution_root_and_exceptions() {
    let original = isolated_config();
    let execution = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::create_dir(execution.path().join(".loom")).unwrap();
    fs::write(
        execution.path().join(".loom/config.toml"),
        "invalid overlay config, never load",
    )
    .unwrap();
    fs::write(original.path().join("local.txt"), "original unchanged").unwrap();
    let ext = Extensions::open_for_workspace(
        original.path(),
        execution.path(),
        ExecutionPolicy::default(),
    )
    .unwrap();
    assert!(!execution.path().join("local.txt").exists());
    if restricted_specs(&ext).is_some() {
        let result = ext.call("mcp__fixture__write", &json!({
            "original":original.path().join("local.txt"), "outside":outside.path().join("denied"),
            "artifact":original.path().join(".loom/artifacts/shared")
        }), &AtomicBool::new(false), TIMEOUT).unwrap();
        let result: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(
            result["cwd"],
            execution.path().canonicalize().unwrap().to_str().unwrap()
        );
        assert_eq!(result["local"], "written");
        assert_eq!(result["original"], "denied");
        assert_eq!(result["outside"], "denied");
        assert_eq!(result["target"], "written");
        assert_eq!(result["artifact"], "written");
        let temp = std::path::PathBuf::from(result["temp"].as_str().unwrap());
        assert!(temp.starts_with(execution.path().canonicalize().unwrap().join(".loom/tmp")));
        assert_eq!(
            fs::read_to_string(execution.path().join("local.txt")).unwrap(),
            "mcp edited"
        );
    }
    assert_eq!(
        fs::read_to_string(original.path().join("local.txt")).unwrap(),
        "original unchanged"
    );
    assert!(!outside.path().join("denied").exists());
}

#[cfg(unix)]
#[test]
fn readonly_policy_filters_and_rejects_mutating_metadata_and_sandboxes_annotated_tools() {
    let original = isolated_config();
    let execution = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let ext = Extensions::open_for_workspace(
        original.path(),
        execution.path(),
        ExecutionPolicy {
            sandbox: SandboxMode::ReadOnly,
            network: false,
        },
    )
    .unwrap();
    if let Some(specs) = restricted_specs(&ext) {
        assert!(specs.iter().any(|s| s.name == "mcp__fixture__probe"));
        assert!(!specs.iter().any(|s| s.name == "mcp__fixture__write"));
        let error = ext
            .call(
                "mcp__fixture__write",
                &json!({}),
                &AtomicBool::new(false),
                TIMEOUT,
            )
            .unwrap_err();
        assert!(error.to_string().contains("read-only"), "{error:#}");
        let result = ext.call("mcp__fixture__probe", &json!({
            "original":original.path().join("source"), "outside":outside.path().join("denied"),
            "artifact":original.path().join(".loom/artifacts/shared")
        }), &AtomicBool::new(false), TIMEOUT).unwrap();
        let result: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(result["local"], "denied");
        assert_eq!(result["original"], "denied");
        assert_eq!(result["outside"], "denied");
        assert_eq!(result["target"], "written");
        assert_eq!(result["artifact"], "written");
    }
    assert!(!execution.path().join("local.txt").exists());
    assert!(!original.path().join("source").exists());
    assert!(!outside.path().join("denied").exists());
}

#[cfg(unix)]
#[test]
fn workspace_network_policy_denies_stdio_os_network_and_builtin_http_calls() {
    let original = isolated_config();
    let execution = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    for network in [false, true] {
        let ext = Extensions::open_for_workspace(
            original.path(),
            execution.path(),
            ExecutionPolicy {
                sandbox: SandboxMode::WorkspaceWrite,
                network,
            },
        )
        .unwrap();
        if restricted_specs(&ext).is_some() {
            let out = ext
                .call(
                    "mcp__fixture__probe",
                    &json!({"port":port}),
                    &AtomicBool::new(false),
                    TIMEOUT,
                )
                .unwrap();
            let result: Value = serde_json::from_str(&out).unwrap();
            assert_eq!(
                result["network"],
                if network { "connected" } else { "denied" }
            );
        }
        if !network {
            assert!(listener.accept().is_err());
            assert!(
                !ext.specs_with_network(true)
                    .iter()
                    .any(|s| s.name == "web_fetch")
            );
            for name in ["web_fetch", "web_search"] {
                let error = ext
                    .call(
                        name,
                        &json!({"url":format!("http://127.0.0.1:{port}"), "query":"test"}),
                        &AtomicBool::new(false),
                        TIMEOUT,
                    )
                    .unwrap_err();
                assert!(error.to_string().contains("network"), "{error:#}");
            }
        }
    }
}
