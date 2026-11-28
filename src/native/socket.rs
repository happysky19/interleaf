//! Legacy Overleaf Socket.IO transport, reused from the preserved MIT Rust implementation.
//! One connection per call; mutations are emitted once and never reconnected/replayed.
use super::{Client, LIMIT, id};
use anyhow::{Result, anyhow, bail, ensure};
use serde_json::{Value, json};
use std::{
    net::{TcpStream, ToSocketAddrs},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tungstenite::{Message, WebSocket, client::IntoClientRequest, stream::MaybeTlsStream};

const TIMEOUT: Duration = Duration::from_secs(8);
pub(super) struct Snapshot {
    pub text: String,
    pub version: u64,
    pub ranges: Value,
}
pub(super) struct Socket {
    ws: WebSocket<MaybeTlsStream<TcpStream>>,
    next: u64,
    pub project: Value,
    pub permissions: String,
    pub concurrent: bool,
    own_op: Option<Value>,
    joined: Option<String>,
    deadline: Instant,
    heartbeat: Duration,
    last_heartbeat: Instant,
}
impl Client {
    pub(super) fn socket(&mut self) -> Result<Socket> {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let body = self.get(
            &format!("/socket.io/1/?projectId={}&t={stamp}", self.project),
            "text/plain",
        )?;
        let parts: Vec<_> = body.trim().split(':').collect();
        ensure!(
            parts.len() == 4 && parts[3].split(',').any(|s| s == "websocket"),
            "unsupported native Socket.IO handshake"
        );
        let sid = parts[0];
        ensure!(
            !sid.is_empty() && sid.len() < 256 && sid.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
            "invalid native socket identifier"
        );
        let interval = parts[1]
            .parse::<u64>()
            .ok()
            .filter(|n| (1..=300).contains(n))
            .ok_or_else(|| anyhow!("invalid native heartbeat interval"))?;
        let origin = self.origin.replacen("https:", "wss:", 1).replacen("http:", "ws:", 1);
        let mut request = format!("{origin}/socket.io/1/websocket/{sid}")
            .into_client_request()
            .map_err(|_| anyhow!("invalid native socket target"))?;
        request.headers_mut().insert(
            "Cookie",
            self.cookie.parse().map_err(|_| anyhow!("invalid session header"))?,
        );
        request.headers_mut().insert(
            "Origin",
            self.origin.parse().map_err(|_| anyhow!("invalid native origin"))?,
        );
        let host = request
            .uri()
            .host()
            .ok_or_else(|| anyhow!("native socket host missing"))?;
        let port = request
            .uri()
            .port_u16()
            .unwrap_or(if request.uri().scheme_str() == Some("ws") {
                80
            } else {
                443
            });
        let addresses = (host, port)
            .to_socket_addrs()
            .map_err(|_| anyhow!("native socket DNS failed"))?;
        let deadline = Instant::now() + TIMEOUT;
        let mut stream = None;
        for address in addresses {
            let remaining = deadline.saturating_duration_since(Instant::now());
            ensure!(!remaining.is_zero(), "native socket connect timed out");
            if let Ok(tcp) = TcpStream::connect_timeout(&address, remaining) {
                stream = Some(tcp);
                break;
            }
        }
        let tcp = stream.ok_or_else(|| anyhow!("native socket connect failed"))?;
        tcp.set_read_timeout(Some(TIMEOUT))?;
        tcp.set_write_timeout(Some(TIMEOUT))?;
        let config = tungstenite::protocol::WebSocketConfig::default()
            .max_message_size(Some(LIMIT as usize))
            .max_frame_size(Some(LIMIT as usize));
        let (ws, _) = tungstenite::client_tls_with_config(request, tcp, Some(config), None)
            .map_err(|_| anyhow!("native WebSocket TLS/handshake failed"))?;
        let mut socket = Socket {
            ws,
            next: 0,
            project: Value::Null,
            permissions: String::new(),
            concurrent: false,
            own_op: None,
            joined: None,
            deadline: Instant::now() + TIMEOUT,
            heartbeat: Duration::from_millis(interval * 500),
            last_heartbeat: Instant::now(),
        };
        loop {
            if socket.frame()? == "1::" {
                break;
            }
        }
        let sequence = socket.emit("joinProject", json!([{"project_id":self.project}]))?;
        loop {
            let frame = socket.frame()?;
            if let Some(raw) = frame.strip_prefix(&format!("6:::{sequence}+")) {
                let values: Vec<Value> =
                    serde_json::from_str(raw).map_err(|_| anyhow!("invalid native project acknowledgement"))?;
                ensure!(
                    values.first().is_some_and(Value::is_null),
                    "native project join rejected"
                );
                socket.project = values.get(1).cloned().unwrap_or(Value::Null);
                socket.permissions = values.get(2).and_then(Value::as_str).unwrap_or("").into();
            } else if let Some(raw) = frame.strip_prefix("5:::") {
                let event: Value = serde_json::from_str(raw).map_err(|_| anyhow!("invalid native project event"))?;
                ensure!(
                    event["name"] != "connectionRejected",
                    "native socket authorization rejected"
                );
                if event["name"] != "joinProjectResponse" {
                    continue;
                }
                socket.project = event["args"][0]["project"].clone();
                socket.permissions = event["args"][0]["permissionsLevel"].as_str().unwrap_or("").into();
            } else {
                continue;
            }
            ensure!(
                socket.project["_id"].as_str() == Some(self.project.as_str()),
                "native socket project mismatch"
            );
            return Ok(socket);
        }
    }
}
impl Socket {
    fn send(&mut self, value: String) -> Result<()> {
        self.ws
            .send(Message::Text(value.into()))
            .map_err(|_| anyhow!("native socket write failed; outcome may be uncertain"))
    }
    fn frame(&mut self) -> Result<String> {
        loop {
            let remaining = self.deadline.saturating_duration_since(Instant::now());
            ensure!(!remaining.is_zero(), "native socket acknowledgement timed out");
            if self.last_heartbeat.elapsed() >= self.heartbeat {
                self.send("2::".into())?;
                self.last_heartbeat = Instant::now();
            }
            let wait = remaining.min(
                self.heartbeat
                    .saturating_sub(self.last_heartbeat.elapsed())
                    .max(Duration::from_millis(1)),
            );
            match self.ws.get_mut() {
                MaybeTlsStream::Plain(tcp) => tcp.set_read_timeout(Some(wait))?,
                MaybeTlsStream::Rustls(tls) => tls.sock.set_read_timeout(Some(wait))?,
                _ => bail!("unsupported native TLS transport"),
            }
            let message = match self.ws.read() {
                Ok(message) => message,
                Err(tungstenite::Error::Io(e))
                    if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) =>
                {
                    continue;
                }
                Err(_) => bail!("native socket disconnected or acknowledgement timed out"),
            };
            match message {
                Message::Text(text) if text == "2::" => self.send("2::".into())?,
                Message::Text(text) if text.starts_with("0:") || text.starts_with("7:") => {
                    bail!("native Socket.IO session closed or rejected")
                }
                Message::Text(text) => return Ok(text.to_string()),
                Message::Close(_) => bail!("native socket closed"),
                Message::Ping(_) | Message::Pong(_) => {}
                _ => bail!("unsupported native socket frame"),
            }
        }
    }
    fn emit(&mut self, name: &str, args: Value) -> Result<u64> {
        self.next += 1;
        self.deadline = Instant::now() + TIMEOUT;
        self.send(format!("5:{}+::{}", self.next, json!({"name":name,"args":args})))?;
        Ok(self.next)
    }
    fn ack(&mut self, name: &str, args: Value) -> Result<Vec<Value>> {
        let sequence = self.emit(name, args)?;
        loop {
            let frame = self.frame()?;
            if let Some(raw) = frame.strip_prefix(&format!("6:::{sequence}+")) {
                let values: Vec<Value> =
                    serde_json::from_str(raw).map_err(|_| anyhow!("invalid native acknowledgement"))?;
                ensure!(
                    values.first().is_none_or(Value::is_null),
                    "native operation rejected; inspect its receipt"
                );
                return Ok(values);
            }
            if frame == format!("6:::{sequence}") {
                return Ok(Vec::new());
            }
            if let Some(raw) = frame.strip_prefix("5:::") {
                let event: Value =
                    serde_json::from_str(raw).map_err(|_| anyhow!("invalid native collaboration event"))?;
                ensure!(
                    event["name"] != "otUpdateError" && event["name"] != "connectionRejected",
                    "native collaboration rejected operation"
                );
                if event["name"] == "otUpdateApplied"
                    && event["args"][0]["doc"].as_str() == self.joined.as_deref()
                    && self.own_op.as_ref() != Some(&event["args"][0]["op"])
                {
                    self.concurrent = true;
                }
                if event["name"].as_str().is_some_and(|name| {
                    matches!(
                        name,
                        "reciveNewDoc"
                            | "reciveNewFolder"
                            | "reciveEntityRename"
                            | "reciveEntityMove"
                            | "removeEntity"
                            | "renameEntity"
                            | "moveEntity"
                            | "toggle-track-changes"
                    )
                }) {
                    self.concurrent = true;
                }
            }
        }
    }
    pub fn join(&mut self, doc: &str) -> Result<Snapshot> {
        id(doc)?;
        self.joined = Some(doc.into());
        // Reply: [error, lines, version, ops, ranges, type, options]. Servers without
        // history-ot omit the type; the official client then assumes ShareJS text.
        let mut values = self.ack("joinDoc", json!([doc,{"encodeRanges":true}]))?;
        ensure!(values.len() >= 5, "incomplete native document reply");
        if let Some(kind) = values.get(5) {
            ensure!(kind == "sharejs-text-ot", "unsupported native document type {kind}");
        }
        let text = values[1]
            .as_array()
            .ok_or_else(|| anyhow!("native document text missing"))?
            .iter()
            .map(|v| packed(v.as_str().ok_or_else(|| anyhow!("invalid native document line"))?))
            .collect::<Result<Vec<_>>>()?
            .join("\n");
        ensure!(text.len() <= LIMIT as usize, "native text document too large");
        let version = values[2]
            .as_u64()
            .filter(|v| *v <= 9_007_199_254_740_991)
            .ok_or_else(|| anyhow!("invalid native document version"))?;
        // encodeRanges packs tracked and commented text the same way as the lines.
        let mut ranges = values[4].take();
        for (key, field) in [("changes", "i"), ("changes", "d"), ("comments", "c")] {
            for range in ranges.get_mut(key).and_then(Value::as_array_mut).into_iter().flatten() {
                if let Some(slot) = range.get_mut("op").and_then(|op| op.get_mut(field))
                    && let Some(text) = slot.as_str()
                {
                    *slot = packed(text)?.into();
                }
            }
        }
        Ok(Snapshot { text, version, ranges })
    }
    pub fn refresh(&mut self, doc: &str) -> Result<Snapshot> {
        self.leave()?;
        self.join(doc)
    }
    pub fn leave(&mut self) -> Result<()> {
        if let Some(doc) = self.joined.take() {
            self.ack("leaveDoc", json!([doc]))?;
        }
        Ok(())
    }
    pub fn apply(&mut self, doc: &str, ops: Value, version: u64, seed: Option<&str>) -> Result<()> {
        ensure!(
            !self.concurrent,
            "concurrent native change detected; no mutation submitted"
        );
        let mut update = json!({"doc":doc,"op":ops,"v":version});
        if let Some(seed) = seed {
            update["meta"] = json!({"tc":seed});
        }
        self.own_op = Some(update["op"].clone());
        self.ack("applyOtUpdate", json!([doc, update]))?;
        Ok(())
    }
}
/// The legacy wire packs UTF-8 bytes into Latin-1 JSON code points.
fn packed(value: &str) -> Result<String> {
    let bytes = value
        .chars()
        .map(|c| u8::try_from(c as u32).map_err(|_| anyhow!("unsupported native text encoding")))
        .collect::<Result<Vec<_>>>()?;
    String::from_utf8(bytes).map_err(|_| anyhow!("invalid packed native UTF-8"))
}
impl Drop for Socket {
    fn drop(&mut self) {
        let _ = self.send("0::".into());
        let _ = self.ws.close(None);
    }
}
