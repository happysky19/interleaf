//! Loopback wire contract: real HTTP, WebSocket framing and durable receipts.
use super::*;
use std::io::{BufRead, BufReader};
use std::net::{TcpListener, TcpStream};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use tungstenite::{Message, accept_hdr};

const PROJECT: &str = "111111111111111111111111";
const DOC: &str = "222222222222222222222222";
const ACCOUNT: &str = "333333333333333333333333";
const ORIGINAL: &str = "444444444444444444444444";
const OP: &str = "8de99b00-0647-4092-b169-b16258e60e81";
const COMMENT: &str = "959a0d15-f177-43f7-904a-63f56eb75dd4";
const TEXT: &str = "α😀 The old model.\nTail";

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Normal,
    LostAck,
    NoOp,
    Race,
    CommentRace,
    NoMessage,
    IdlessMessage,
}
struct State {
    text: String,
    version: u64,
    changes: Vec<Value>,
    comments: Vec<Value>,
    threads: Map<String, Value>,
    writes: usize,
    posts: usize,
    mode: Mode,
    tracking: bool,
    doc_type: Option<&'static str>,
}
struct World {
    origin: String,
    state: Arc<Mutex<State>>,
    stopped: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    root: tempfile::TempDir,
}
impl World {
    fn new(mode: Mode) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(State {
            text: TEXT.into(),
            version: 95,
            changes: vec![
                json!({"id":ORIGINAL,"op":{"p":19,"i":"Tail"},"metadata":{"user_id":"555555555555555555555555","ts":"2026-11-26T00:00:00Z"}}),
            ],
            comments: vec![],
            threads: Map::new(),
            writes: 0,
            posts: 0,
            mode,
            tracking: true,
            doc_type: Some("sharejs-text-ot"),
        }));
        let stopped = Arc::new(AtomicBool::new(false));
        let (shared, stop) = (state.clone(), stopped.clone());
        let thread = thread::spawn(move || {
            let mut clients = Vec::new();
            while !stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        let state = shared.clone();
                        clients.push(thread::spawn(move || connection(stream, state)));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(2)),
                    Err(e) => panic!("{e}"),
                }
            }
            for client in clients {
                client.join().unwrap();
            }
        });
        Self {
            origin,
            state,
            stopped,
            thread: Some(thread),
            root: tempfile::tempdir().unwrap(),
        }
    }
    fn call(&self, tool: &str, args: Value) -> Result<Value> {
        let native = NativeSession {
            identity: None,
            upload_roots: Vec::new(),
            projects: None,
            operations: None,
            origin: self.origin.clone(),
            cookie: "overleaf_session2=fixture-session".into(),
            receipts: self.root.path().join("receipts"),
        };
        serde_json::from_str(&native.call(tool, args.as_object().unwrap())?).map_err(Into::into)
    }
}
impl Drop for World {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.thread.take().unwrap().join().unwrap();
    }
}
fn packed(value: &str) -> String {
    value.as_bytes().iter().map(|b| char::from(*b)).collect()
}
fn connection(mut stream: TcpStream, state: Arc<Mutex<State>>) {
    stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    stream.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
    let mut peek = [0; 4096];
    let size = stream.peek(&mut peek).unwrap();
    if std::str::from_utf8(&peek[..size])
        .unwrap()
        .starts_with("GET /socket.io/1/websocket/")
    {
        // tungstenite fixes the callback's error type in its public trait.
        #[allow(clippy::result_large_err)]
        let mut ws = accept_hdr(stream, |req: &tungstenite::handshake::server::Request, response| {
            let cookie = req.headers()["Cookie"].to_str().unwrap();
            assert!(cookie.contains("overleaf_session2=fixture-session") && cookie.contains("GCLB=fixture-sticky"));
            Ok(response)
        })
        .unwrap();
        ws.send(Message::text("1::")).unwrap();
        loop {
            let frame = match ws.read() {
                Ok(Message::Text(s)) => s,
                Ok(Message::Close(_)) | Err(_) => break,
                Ok(_) => continue,
            };
            if frame == "2::" {
                continue;
            }
            if frame.starts_with("0:") {
                break;
            }
            let Some((sequence, payload)) = frame.strip_prefix("5:").and_then(|s| s.split_once("+::")) else {
                panic!("bad frame {frame}")
            };
            let event: Value = serde_json::from_str(payload).unwrap();
            let mut s = state.lock().unwrap();
            let values = match event["name"].as_str().unwrap() {
                "joinProject" => {
                    assert_eq!(event["args"][0]["project_id"], PROJECT);
                    let project = json!({"_id":PROJECT,"features":{"trackChanges":s.tracking},"rootFolder":[{"_id":"777777777777777777777777","docs":[{"_id":DOC,"name":"main.tex"}],"folders":[],"fileRefs":[]} ]});
                    if s.mode == Mode::Normal {
                        ws.send(Message::text(format!("5:::{}",json!({"name":"joinProjectResponse","args":[{"project":project,"permissionsLevel":"readAndWrite"}]})))).unwrap();
                        continue;
                    }
                    json!([null, project, "readAndWrite", 2])
                }
                "joinDoc" => {
                    assert_eq!(event["args"][0], DOC);
                    // Unlike HTTP ranges, joinDoc packs lines and range text.
                    let mut ranges = json!({"changes":s.changes,"comments":s.comments});
                    for (key, field) in [("changes", "i"), ("changes", "d"), ("comments", "c")] {
                        for range in ranges[key].as_array_mut().unwrap() {
                            if let Some(text) = range["op"][field].as_str() {
                                range["op"][field] = packed(text).into();
                            }
                        }
                    }
                    let lines: Vec<_> = s.text.split('\n').map(packed).collect();
                    let mut reply = json!([null, lines, s.version, [], ranges]);
                    if let Some(kind) = s.doc_type {
                        reply
                            .as_array_mut()
                            .unwrap()
                            .extend([json!(kind), json!({"canSkipLeaveDoc":true})]);
                    }
                    reply
                }
                "leaveDoc" => json!([null]),
                "applyOtUpdate" => {
                    assert_eq!(event["args"][0], DOC);
                    let update = &event["args"][1];
                    assert_eq!(update["v"], s.version);
                    s.writes += 1;
                    let mode = s.mode;
                    if mode != Mode::NoOp {
                        let seed = update["meta"]["tc"].as_str();
                        for (index, op) in update["op"].as_array().unwrap().iter().enumerate() {
                            let p = op["p"].as_u64().unwrap() as usize;
                            if let Some(quote) = op["c"].as_str() {
                                assert!(seed.is_none(), "comment anchors must not fabricate text changes");
                                s.comments
                                    .push(json!({"id":"range-1","op":{"p":p,"c":quote,"t":op["t"]}}));
                                continue;
                            }
                            let seed = seed.expect("untracked edit submitted");
                            assert_eq!(seed.len(), 18);
                            assert_eq!(p, 8, "non-BMP prefix must use UTF-16 offsets");
                            let mut units: Vec<_> = s.text.encode_utf16().collect();
                            if let Some(old) = op["d"].as_str() {
                                let deleted: Vec<_> = old.encode_utf16().collect();
                                assert_eq!(&units[p..p + deleted.len()], &deleted);
                                units.drain(p..p + deleted.len());
                                for range in &mut s.changes {
                                    let start = range["op"]["p"].as_u64().unwrap() as usize;
                                    if start >= p + deleted.len() {
                                        range["op"]["p"] = json!(start - deleted.len());
                                    }
                                }
                            } else {
                                let inserted: Vec<_> = op["i"].as_str().unwrap().encode_utf16().collect();
                                units.splice(p..p, inserted.iter().copied());
                                for range in &mut s.changes {
                                    let start = range["op"]["p"].as_u64().unwrap() as usize;
                                    if start >= p {
                                        range["op"]["p"] = json!(start + inserted.len());
                                    }
                                }
                            }
                            s.text = String::from_utf16(&units).unwrap();
                            let mut change_op = op.clone();
                            if op.get("d").is_some() {
                                change_op["p"] = json!(p);
                            }
                            s.changes.push(json!({"id":format!("{seed}{:06x}",index+1),"op":change_op,"metadata":{"user_id":ACCOUNT,"ts":"2026-11-26T01:00:00Z"}}));
                        }
                    }
                    s.version += 1;
                    if mode == Mode::Race {
                        s.text.push('!');
                        s.version += 1;
                        ws.send(Message::text(format!(
                            "5:::{}",
                            json!({"name":"otUpdateApplied","args":[{"doc":DOC,"op":[{"p":25,"i":"!"}]}]})
                        )))
                        .unwrap();
                    }
                    if mode == Mode::LostAck {
                        drop(s);
                        let _ = ws.close(None);
                        break;
                    }
                    json!([null])
                }
                other => panic!("unexpected event {other}"),
            };
            drop(s);
            ws.send(Message::text(format!("6:::{sequence}+{values}"))).unwrap();
        }
        return;
    }
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut first = String::new();
    reader.read_line(&mut first).unwrap();
    let path = first.split_whitespace().nth(1).unwrap();
    let mut headers = String::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
        headers.push_str(&line);
    }
    assert!(headers.contains("overleaf_session2=fixture-session"));
    let mut s = state.lock().unwrap();
    let (media, body, extra) = if first.starts_with("POST ") {
        assert!(headers.to_lowercase().contains("x-csrf-token: fixture-csrf"));
        let len = headers
            .lines()
            .find_map(|l| {
                l.to_lowercase()
                    .strip_prefix("content-length: ")
                    .and_then(|n| n.trim().parse::<usize>().ok())
            })
            .unwrap();
        let mut body = vec![0; len];
        reader.read_exact(&mut body).unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        let thread = path.split('/').nth(4).unwrap();
        assert_eq!(thread.len(), 24);
        assert!(path.ends_with("/messages"));
        s.posts += 1;
        if s.mode == Mode::CommentRace {
            s.version += 1;
        }
        let message = format!("message-{}", s.posts);
        if s.mode != Mode::NoMessage {
            s.threads.insert(thread.into(),json!({"messages":[{"id":message,"content":body["content"],"timestamp":1,"user":{"_id":ACCOUNT}}],"resolved":false}));
        }
        if s.mode == Mode::IdlessMessage {
            drop(s);
            write!(stream, "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n").unwrap();
            return;
        }
        (
            "application/json",
            if s.mode == Mode::NoMessage {
                json!({}).to_string()
            } else {
                json!({"id":message}).to_string()
            },
            "",
        )
    } else if path.starts_with("/socket.io/1/?") {
        (
            "text/plain",
            "fixture:60:60:websocket".into(),
            "Set-Cookie: GCLB=fixture-sticky; Path=/\r\n",
        )
    } else if path.ends_with("/threads") {
        ("application/json", Value::Object(s.threads.clone()).to_string(), "")
    } else if path.ends_with("/ranges") {
        (
            "application/json",
            json!([{"id":DOC,"ranges":{"changes":s.changes,"comments":s.comments}}]).to_string(),
            "",
        )
    } else {
        assert_eq!(path, format!("/project/{PROJECT}"));
        (
            "text/html",
            format!(
                "<meta name='ol-project_id' content='{PROJECT}'><meta name='ol-user' content='{{&quot;_id&quot;:&quot;{ACCOUNT}&quot;}}'><meta name='ol-csrfToken' content='fixture-csrf'>"
            ),
            "",
        )
    };
    drop(s);
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: {media}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
}
fn args(version: u64) -> Value {
    json!({"project_id":PROJECT,"path":"main.tex","old_string":"The old model.","new_string":"The newer model.","expected_version":version,"operation_id":OP})
}
#[test]
fn pending_edits_and_anchors_are_verified_without_replay() {
    for mode in [Mode::Normal, Mode::LostAck, Mode::NoOp, Mode::Race] {
        let world = World::new(mode);
        let read = world
            .call("native_read_file", json!({"project_id":PROJECT,"path":"main.tex"}))
            .unwrap();
        assert_eq!(read["text"], TEXT);
        assert_eq!(read["version"], 95);
        let pending = world
            .call("native_list_tracked_changes", json!({"project_id":PROJECT}))
            .unwrap();
        assert_eq!(pending["count"], 1);
        assert_eq!(pending["truncated"], false);
        let result = world.call("native_suggest_edit", args(95)).unwrap();
        assert_eq!(result["verified"], mode == Mode::Normal);
        assert_eq!(
            result["state"],
            if mode == Mode::Normal { "confirmed" } else { "uncertain" }
        );
        let repeated = world.call("native_suggest_edit", args(95)).unwrap();
        assert_eq!(repeated["state"], result["state"]);
        assert_eq!(world.state.lock().unwrap().writes, 1);
        let read = world
            .call("native_read_file", json!({"project_id":PROJECT,"path":"main.tex"}))
            .unwrap();
        assert_eq!(
            read["text"],
            if mode == Mode::NoOp {
                TEXT
            } else if mode == Mode::Race {
                "α😀 The newer model.\nTail!"
            } else {
                "α😀 The newer model.\nTail"
            }
        );
        let pending = world
            .call("native_list_tracked_changes", json!({"project_id":PROJECT}))
            .unwrap();
        assert_eq!(pending["count"], if mode == Mode::NoOp { 1 } else { 3 });
        assert!(
            pending["changes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["change_id"] == ORIGINAL)
        );
        if mode == Mode::Normal {
            assert_eq!(result["tracked"], true);
            assert_eq!(result["version_after"], 96);
            let comment_args = json!({"project_id":PROJECT,"path":"main.tex","quoted_text":"newer model","content":"Clarify this approximation.","expected_version":96,"operation_id":COMMENT});
            let comment = world.call("native_add_comment", comment_args.clone()).unwrap();
            assert_eq!(comment["verified"], true);
            assert_eq!(comment["anchored"], true);
            assert_eq!(comment["version_after"], 97);
            let again = world.call("native_add_comment", comment_args).unwrap();
            assert_eq!(again["state"], "confirmed");
            let state = world.state.lock().unwrap();
            assert_eq!(state.posts, 1);
            assert_eq!(state.writes, 2);
            assert_eq!(state.changes.len(), 3);
        }
    }
    let idless = World::new(Mode::IdlessMessage);
    let comment=idless.call("native_add_comment",json!({"project_id":PROJECT,"path":"main.tex","quoted_text":"old model","content":"Clarify this approximation.","expected_version":95,"operation_id":COMMENT})).unwrap();
    assert_eq!(comment["verified"], true);
    assert_eq!(comment["anchored"], true);
    assert_eq!(idless.state.lock().unwrap().writes, 1);
    for mode in [Mode::LostAck, Mode::CommentRace, Mode::NoMessage] {
        let world = World::new(mode);
        let args = json!({"project_id":PROJECT,"path":"main.tex","quoted_text":"old model","content":"Clarify this approximation.","expected_version":95,"operation_id":COMMENT});
        let result = world.call("native_add_comment", args.clone()).unwrap();
        assert_eq!(result["state"], "uncertain");
        assert_eq!(result["verified"], false);
        let again = world.call("native_add_comment", args).unwrap();
        assert_eq!(again["state"], "uncertain");
        let state = world.state.lock().unwrap();
        assert_eq!(state.posts, 1);
        assert_eq!(state.writes, if mode == Mode::LostAck { 1 } else { 0 });
    }
}
#[test]
fn stale_overlapping_or_untracked_requests_never_mutate() {
    let world = World::new(Mode::Normal);
    assert_eq!(
        world
            .call("native_read_file", json!({"project_id":PROJECT,"path":"main.tex"}))
            .unwrap()["version"],
        95
    );
    assert!(world.call("native_suggest_edit", args(94)).is_err());
    let mut off = args(95);
    off["track"] = json!("off");
    assert!(world.call("native_suggest_edit", off).is_err());
    let mut loose = args(95);
    loose["strict_version"] = json!(false);
    assert!(world.call("native_suggest_edit", loose).is_err());
    {
        let mut state = world.state.lock().unwrap();
        state.changes[0]["op"] = json!({"p":8,"i":"old"});
    }
    assert!(world.call("native_suggest_edit", args(95)).is_err());
    {
        let mut state = world.state.lock().unwrap();
        state.changes.clear();
        state.tracking = false;
    }
    assert!(world.call("native_suggest_edit", args(95)).is_err());
    let state = world.state.lock().unwrap();
    assert_eq!(state.writes, 0);
    assert_eq!(state.posts, 0);
    assert_eq!(state.text, TEXT);
}
#[test]
fn joined_documents_follow_the_overleaf_reply() {
    let world = World::new(Mode::Normal);
    let read = || world.call("native_read_file", json!({"project_id":PROJECT,"path":"main.tex"}));
    world.state.lock().unwrap().changes[0]["op"] = json!({"p":0,"d":"α😀"});
    assert_eq!(read().unwrap()["ranges"]["changes"][0]["op"]["d"], "α😀");
    world.state.lock().unwrap().doc_type = None;
    assert_eq!(read().unwrap()["text"], TEXT);
    world.state.lock().unwrap().doc_type = Some("history-ot");
    assert!(format!("{:#}", read().unwrap_err()).contains("unsupported native document type \"history-ot\""));
    assert_eq!(world.state.lock().unwrap().writes, 0);
}
