//! Ordered JSON-RPC messages over a persistent local app-server WebSocket.
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{net::TcpStream, sync::mpsc, thread, time::Duration};
use tungstenite::{Message, WebSocket};

#[derive(Debug)]
pub enum Event {
    Message(Value),
    Disconnected(String),
}
pub struct Client {
    outgoing: mpsc::Sender<Value>,
    pub incoming: mpsc::Receiver<Event>,
    next_id: u64,
}

pub fn connect_socket(endpoint: &str) -> Result<WebSocket<TcpStream>> {
    let address = endpoint
        .strip_prefix("ws://")
        .context("Only local ws:// endpoints are supported")?;
    if !(address.starts_with("127.0.0.1:")
        || address.starts_with("localhost:")
        || address.starts_with("[::1]:"))
        || address.contains('/')
    {
        bail!("The shared runtime must listen on loopback");
    }
    let stream = TcpStream::connect(address).context("Connecting to Codex runtime")?;
    // Requests share the reader thread; a long read timeout also delays outgoing
    // keystroke-triggered requests when the server is otherwise quiet.
    stream.set_read_timeout(Some(Duration::from_millis(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    let (socket, _) = tungstenite::client(endpoint, stream)
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    Ok(socket)
}
impl Client {
    pub fn connect(endpoint: &str) -> Result<Self> {
        let mut socket = connect_socket(endpoint)?;
        let (outgoing, commands) = mpsc::channel::<Value>();
        let (events, incoming) = mpsc::channel();
        thread::Builder::new()
            .name("codex-rpc".into())
            .spawn(move || {
                loop {
                    loop {
                        match commands.try_recv() {
                            Ok(message) => {
                                if let Err(error) = socket.send(Message::Text(message.to_string()))
                                {
                                    let _ = events.send(Event::Disconnected(error.to_string()));
                                    return;
                                }
                            }
                            Err(mpsc::TryRecvError::Disconnected) => {
                                let _ = socket.close(None);
                                return;
                            }
                            Err(mpsc::TryRecvError::Empty) => break,
                        }
                    }
                    match socket.read() {
                        Ok(Message::Text(text)) => match serde_json::from_str::<Value>(&text) {
                            Ok(message) => {
                                if events.send(Event::Message(message)).is_err() {
                                    return;
                                }
                            }
                            Err(error) => {
                                let _ = events.send(Event::Disconnected(format!(
                                    "Invalid JSON-RPC: {error}"
                                )));
                                return;
                            }
                        },
                        Ok(Message::Close(_)) => {
                            let _ = events
                                .send(Event::Disconnected("Runtime closed the connection".into()));
                            return;
                        }
                        Ok(_) => {
                            let _ = socket.flush();
                        }
                        Err(tungstenite::Error::Io(ref error))
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) => {}
                        Err(error) => {
                            let _ = events.send(Event::Disconnected(error.to_string()));
                            return;
                        }
                    }
                }
            })?;
        Ok(Self {
            outgoing,
            incoming,
            next_id: 1,
        })
    }
    pub fn request(&mut self, method: &str, params: Value) -> Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"id":id,"method":method,"params":params}))?;
        Ok(id)
    }
    pub fn send(&self, value: Value) -> Result<()> {
        self.outgoing
            .send(value)
            .context("Runtime connection closed")
    }
    pub fn initialize(&mut self) -> Result<u64> {
        self.request("initialize",json!({"clientInfo":{"name":"custom_tui","title":"Custom CLI","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}}))
    }
    pub fn reply(&self, id: Value, result: Value) -> Result<()> {
        self.send(json!({"id":id,"result":result}))
    }
    pub fn reject(&self, id: Value, message: &str) -> Result<()> {
        self.send(json!({"id":id,"error":{"code":-32601,"message":message}}))
    }
}
