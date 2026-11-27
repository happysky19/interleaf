//! Native Overleaf comments. Login and project authorization belong to the launcher.
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

const LIMIT: u64 = 4 * 1024 * 1024;

/// A launcher-provided session, kept in memory. Never read browser/cookie stores.
pub struct NativeSession {
    origin: String,
    cookie: String,
    receipts: PathBuf,
}

fn need<'a>(args: &'a Map<String, Value>, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| anyhow!("missing native argument {key}"))
}

fn id(value: &str) -> Result<()> {
    ensure!(
        value.len() == 24 && value.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
        "native identifiers must be lowercase 24-character Overleaf IDs"
    );
    Ok(())
}

fn cookie(value: &str) -> Result<()> {
    let mut names = BTreeSet::new();
    for pair in value.split("; ") {
        let (name, token) = pair
            .split_once('=')
            .ok_or_else(|| anyhow!("invalid injected session cookie"))?;
        ensure!(
            matches!(name, "overleaf_session2" | "GCLB")
                && names.insert(name)
                && !token.is_empty()
                && token.len() <= 4096
                && token.bytes().all(|b| (33..127).contains(&b) && !b";\"\\,".contains(&b)),
            "invalid injected session cookie"
        );
    }
    ensure!(
        names.contains("overleaf_session2"),
        "injected Overleaf session cookie missing"
    );
    Ok(())
}

impl NativeSession {
    /// Read Fridica's existing {baseUrl,cookie} envelope, with a 16 KiB bound.
    /// The receipt directory must be private and outside the manuscript's tracked files.
    pub fn from_reader(reader: impl Read, receipts: PathBuf) -> Result<Self> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Envelope {
            base_url: String,
            cookie: String,
        }
        let mut raw = Vec::new();
        reader
            .take(16385)
            .read_to_end(&mut raw)
            .map_err(|_| anyhow!("cannot read injected session"))?;
        ensure!(raw.len() <= 16384, "injected session exceeds size limit");
        let envelope: Envelope =
            serde_json::from_slice(&raw).map_err(|_| anyhow!("invalid injected session envelope"))?;
        ensure!(
            envelope.base_url == "https://www.overleaf.com",
            "unsupported native session origin"
        );
        cookie(&envelope.cookie)?;
        Ok(Self {
            origin: envelope.base_url,
            cookie: envelope.cookie,
            receipts,
        })
    }

    /// Consume the anonymous Unix socket passed in OL_SESSION_FD. No unsafe code,
    /// credential files or environment-variable credential values are required.
    #[cfg(unix)]
    pub fn from_inherited_socket(fd: i32, receipts: PathBuf) -> Result<Self> {
        use nix::sys::socket::{MsgFlags, UnixAddr, getsockname, recv};
        use std::time::Instant;
        ensure!(fd >= 3, "invalid inherited session descriptor");
        struct Close(i32);
        impl Drop for Close {
            fn drop(&mut self) {
                let _ = nix::unistd::close(self.0);
            }
        }
        let _close = Close(fd);
        getsockname::<UnixAddr>(fd).map_err(|_| anyhow!("session descriptor must be a Unix socket"))?;
        let mut raw = vec![0; 16385];
        let mut used = 0;
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            ensure!(Instant::now() < deadline, "injected session read timed out");
            match recv(fd, &mut raw[used..], MsgFlags::MSG_DONTWAIT) {
                Ok(0) => break,
                Ok(n) => {
                    used += n;
                    ensure!(used <= 16384, "injected session exceeds size limit");
                }
                Err(nix::errno::Errno::EINTR) => continue,
                Err(nix::errno::Errno::EAGAIN) => std::thread::sleep(Duration::from_millis(2)),
                Err(_) => bail!("cannot read inherited session socket"),
            }
        }
        Self::from_reader(&raw[..used], receipts)
    }

    pub(crate) fn call(&self, name: &str, args: &Map<String, Value>) -> Result<String> {
        let allowed: &[&str] = match name {
            "native_list_comments" => &["project_id", "include_resolved", "include_orphaned", "full"],
            "native_read_comment_thread" => &["project_id", "thread_id"],
            "native_reply_comment" => &["project_id", "thread_id", "content", "operation_id"],
            _ => bail!("unknown native tool"),
        };
        ensure!(
            args.keys().all(|k| allowed.contains(&k.as_str())),
            "unknown native argument"
        );
        let project = need(args, "project_id")?;
        id(project)?;
        if name != "native_list_comments" {
            id(need(args, "thread_id")?)?;
        }
        for key in ["include_resolved", "include_orphaned", "full"] {
            ensure!(
                args.get(key).is_none_or(Value::is_boolean),
                "native filters must be booleans"
            );
        }
        if name == "native_reply_comment" {
            let content = need(args, "content")?;
            ensure!(
                !content.trim().is_empty() && content.len() <= 32768,
                "reply must contain 1–32768 bytes of text"
            );
            let op = need(args, "operation_id")?;
            ensure!(
                crate::text::re!(r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$").is_match(op),
                "a stable operation_id UUID is required; reuse it when checking an uncertain reply"
            );
        }
        let mut client = Client::new(self, project)?;
        let threads = client.threads()?;
        let value = match name {
            "native_list_comments" => client.list(args, &threads)?,
            "native_read_comment_thread" => {
                let thread = need(args, "thread_id")?;
                ensure!(
                    threads.get(thread).is_some(),
                    "comment thread not found in this project"
                );
                json!({"project_id":project,"thread_id":thread,"thread":threads[thread],"complete":true})
            }
            "native_reply_comment" => client.reply(self, args, &threads)?,
            _ => unreachable!(),
        };
        Ok(client.redact(&serde_json::to_string(&value)?))
    }
}

pub(crate) fn definitions() -> Vec<Value> {
    let project = json!({"type":"string","pattern":"^[0-9a-f]{24}$","description":"Explicit Overleaf project ID from its URL; required on every call"});
    let thread = json!({"type":"string","pattern":"^[0-9a-f]{24}$"});
    [
        ("native_list_comments", "Read native review threads and quoted anchors. Open, anchored threads by default.", json!({"project_id":project,"include_resolved":{"type":"boolean"},"include_orphaned":{"type":"boolean"},"full":{"type":"boolean"}}), vec!["project_id"], true),
        ("native_read_comment_thread", "Read all messages, authors and timestamps in one native review thread.", json!({"project_id":project,"thread_id":thread}), vec!["project_id","thread_id"], true),
        ("native_reply_comment", "Reply once to an existing native thread. Keep operation_id unchanged on retry; uncertain writes are never resubmitted.", json!({"project_id":project,"thread_id":thread,"content":{"type":"string"},"operation_id":{"type":"string","format":"uuid"}}), vec!["project_id","thread_id","content","operation_id"], false),
    ].into_iter().map(|(name, description, properties, required, read_only)| json!({
        "name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false},
        "annotations":{"readOnlyHint":read_only,"destructiveHint":false,"idempotentHint":true}
    })).collect()
}

fn meta(html: &str, name: &str) -> Result<String> {
    let mut found = Vec::new();
    for tag in crate::text::re!(r"(?is)<meta\s+[^>]*>").find_iter(html) {
        let mut attrs = Map::new();
        for c in crate::text::re!(r#"([\w-]+)\s*=\s*(?:"([^"]*)"|'([^']*)')"#).captures_iter(tag.as_str()) {
            attrs.insert(
                c[1].to_lowercase(),
                json!(html_escape::decode_html_entities(
                    c.get(2).or(c.get(3)).unwrap().as_str()
                )),
            );
        }
        if attrs.get("name").and_then(Value::as_str) == Some(name) {
            found.push(
                attrs
                    .get("content")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("native metadata missing"))?
                    .to_owned(),
            );
        }
    }
    ensure!(found.len() == 1, "native metadata missing or ambiguous");
    Ok(found.remove(0))
}

struct Client {
    http: ureq::Agent,
    origin: String,
    cookie: String,
    project: String,
    account: String,
    csrf: String,
    secrets: Vec<String>,
}

impl Client {
    fn new(session: &NativeSession, project: &str) -> Result<Self> {
        let mut client = Self {
            http: ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(8)))
                .max_redirects(0)
                .http_status_as_error(false)
                .proxy(None)
                .build()
                .into(),
            origin: session.origin.clone(),
            cookie: session.cookie.clone(),
            project: project.into(),
            account: String::new(),
            csrf: String::new(),
            secrets: vec![session.cookie.clone()],
        };
        let page = client.get(&format!("/project/{project}"), "text/html")?;
        ensure!(
            meta(&page, "ol-project_id")? == project,
            "native project verification failed"
        );
        let user: Value =
            serde_json::from_str(&meta(&page, "ol-user")?).map_err(|_| anyhow!("invalid native account metadata"))?;
        client.account = user
            .get("_id")
            .or_else(|| user.get("id"))
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("native account missing"))?
            .to_owned();
        id(&client.account)?;
        client.csrf = meta(&page, "ol-csrfToken")?;
        ensure!(
            !client.csrf.is_empty() && client.csrf.len() <= 1024 && client.csrf.bytes().all(|b| (33..127).contains(&b)),
            "invalid native CSRF token"
        );
        client.secrets.push(client.csrf.clone());
        Ok(client)
    }
    fn redact(&self, value: &str) -> String {
        let mut out = value.to_owned();
        for secret in &self.secrets {
            out = out.replace(secret, "[redacted]");
            for pair in secret.split("; ") {
                if let Some((_, token)) = pair.split_once('=') {
                    out = out.replace(token, "[redacted]");
                }
            }
        }
        out
    }
    fn body(&mut self, mut response: ureq::http::Response<ureq::Body>, expected: &str) -> Result<String> {
        let status = response.status().as_u16();
        ensure!(
            (200..300).contains(&status),
            "native HTTP refused request (status {status}); refresh through Fridica if login expired"
        );
        for header in response.headers().get_all("set-cookie") {
            let pair = header
                .to_str()
                .map_err(|_| anyhow!("invalid native session response"))?
                .split(';')
                .next()
                .unwrap_or("");
            if let Some((name, _)) = pair.split_once('=')
                && matches!(name, "overleaf_session2" | "GCLB")
            {
                let mut parts: Vec<_> = self
                    .cookie
                    .split("; ")
                    .filter(|p| !p.starts_with(&format!("{name}=")))
                    .map(str::to_owned)
                    .collect();
                parts.push(pair.to_owned());
                let next = parts.join("; ");
                cookie(&next)?;
                self.secrets.push(next.clone());
                self.cookie = next;
            }
        }
        if status == 204 {
            return Ok(String::new());
        }
        ensure!(
            response
                .headers()
                .get("content-type")
                .and_then(|h| h.to_str().ok())
                .and_then(|h| h.split(';').next())
                == Some(expected),
            "unsupported native response content type"
        );
        response
            .body_mut()
            .with_config()
            .limit(LIMIT)
            .read_to_string()
            .map_err(|_| anyhow!("native response invalid or too large"))
    }
    fn get(&mut self, path: &str, expected: &str) -> Result<String> {
        let response = self
            .http
            .get(format!("{}{path}", self.origin))
            .header("Cookie", &self.cookie)
            .header("Accept", expected)
            .call()
            .map_err(|_| anyhow!("native read failed; redirects and automatic login are disabled"))?;
        self.body(response, expected)
    }
    fn threads(&mut self) -> Result<Value> {
        let raw = self.get(&format!("/project/{}/threads", self.project), "application/json")?;
        let threads: Value = serde_json::from_str(&raw).map_err(|_| anyhow!("invalid native thread response"))?;
        for (thread, value) in threads
            .as_object()
            .ok_or_else(|| anyhow!("complete native thread map required"))?
        {
            id(thread)?;
            for message in value["messages"]
                .as_array()
                .ok_or_else(|| anyhow!("incomplete native thread"))?
            {
                ensure!(
                    message["id"].as_str().is_some_and(valid_message_id)
                        && message["content"].is_string()
                        && message["timestamp"].is_number(),
                    "incomplete native message"
                );
            }
        }
        Ok(threads)
    }
    fn list(&mut self, args: &Map<String, Value>, threads: &Value) -> Result<Value> {
        let ranges: Value =
            serde_json::from_str(&self.get(&format!("/project/{}/ranges", self.project), "application/json")?)
                .map_err(|_| anyhow!("invalid native anchor response"))?;
        let mut anchors: Map<String, Value> = Map::new();
        for doc in ranges
            .as_array()
            .ok_or_else(|| anyhow!("complete native range list required"))?
        {
            let doc_id = doc["id"]
                .as_str()
                .ok_or_else(|| anyhow!("native document ID missing"))?;
            id(doc_id)?;
            if let Some(comments) = doc["ranges"].get("comments") {
                for range in comments.as_array().ok_or_else(|| anyhow!("invalid native anchors"))? {
                    let op = &range["op"];
                    let thread = op["t"]
                        .as_str()
                        .ok_or_else(|| anyhow!("native anchor thread missing"))?;
                    id(thread)?;
                    ensure!(
                        op["p"].as_u64().is_some() && op["c"].is_string(),
                        "invalid native anchor"
                    );
                    anchors
                        .entry(thread.to_owned())
                        .or_insert_with(|| json!([]))
                        .as_array_mut()
                        .unwrap()
                        .push(json!({"doc_id":doc_id,"position":op["p"],"quoted_text":op["c"]}));
                }
            }
        }
        let flag = |key: &str| args.get(key).and_then(Value::as_bool).unwrap_or(false);
        let mut rows = Vec::new();
        for (thread, value) in threads.as_object().unwrap() {
            let resolved = value["resolved"].as_bool().unwrap_or(false);
            let anchor = anchors.get(thread).cloned().unwrap_or_else(|| json!([]));
            let orphaned = anchor.as_array().unwrap().is_empty();
            if (resolved && !flag("include_resolved")) || (orphaned && !flag("include_orphaned")) {
                continue;
            }
            let messages = value["messages"].as_array().unwrap();
            let mut row = json!({"thread_id":thread,"resolved":resolved,"orphaned":orphaned,"anchors":anchor,"message_count":messages.len(),"preview":messages.last().map(|m| &m["content"])});
            if flag("full") {
                row["full_thread"] = value.clone();
            }
            rows.push(row);
        }
        Ok(json!({"project_id":self.project,"threads":rows,"count":rows.len(),"complete":true}))
    }
    fn reply(&mut self, session: &NativeSession, args: &Map<String, Value>, threads: &Value) -> Result<Value> {
        let thread = need(args, "thread_id")?;
        ensure!(
            threads.get(thread).is_some(),
            "comment thread not found in this project"
        );
        let content = need(args, "content")?;
        ensure!(self.redact(content) == content, "reply must not contain credentials");
        let operation = need(args, "operation_id")?;
        let binding = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&json!([
                self.origin,
                self.account,
                self.project,
                thread,
                content
            ]))?)
        );
        let (path, mut receipt, fresh) = reserve(&session.receipts, operation, &binding, &self.project, thread)?;
        if !fresh {
            // An existing receipt is authoritative. Checking it is read-only,
            // including while another process is still completing the write.
            return Ok(outcome(&receipt, true));
        }
        // The durable attempt precedes the POST. A crash/lost ACK cannot resubmit it.
        let attempt = (|| -> Result<Option<String>> {
            let response = self
                .http
                .post(format!(
                    "{}/project/{}/thread/{thread}/messages",
                    self.origin, self.project
                ))
                .header("Cookie", &self.cookie)
                .header("X-Csrf-Token", &self.csrf)
                .header("Accept", "application/json")
                .send_json(json!({"content":content}))
                .map_err(|_| anyhow!("reply outcome uncertain"))?;
            let body = self.body(response, "application/json")?;
            if body.is_empty() {
                return Ok(None);
            }
            let response: Value = serde_json::from_str(&body).map_err(|_| anyhow!("reply outcome uncertain"))?;
            Ok(response["id"]
                .as_str()
                .filter(|v| valid_message_id(v) && self.redact(v) == *v)
                .map(str::to_owned))
        })();
        if let Ok(message) = attempt {
            receipt.message_id = message;
        }
        if let Some(message) = &receipt.message_id {
            receipt.verified = self.threads().ok().is_some_and(|all| {
                all[thread]["messages"].as_array().is_some_and(|messages| {
                    messages.iter().any(|m| {
                        m["id"] == *message && m["content"] == content && author(m) == Some(self.account.as_str())
                    })
                })
            });
            receipt.state = if receipt.verified { "confirmed" } else { "uncertain" }.into();
        }
        let persisted = save(&path, &receipt).is_ok();
        Ok(outcome(&receipt, persisted))
    }
}

fn outcome(receipt: &Receipt, persisted: bool) -> Value {
    json!({"operation_id":receipt.operation_id,"project_id":receipt.project_id,"thread_id":receipt.thread_id,"message_id":receipt.message_id,
        "state":if persisted {receipt.state.as_str()} else {"uncertain"},"verified":receipt.verified && persisted,"receipt_persisted":persisted,
        "next":if receipt.verified && persisted {"complete"} else {"Read the thread and reconcile; do not resubmit with a new operation_id."}})
}

fn author(message: &Value) -> Option<&str> {
    message["user_id"]
        .as_str()
        .or_else(|| message["user"]["_id"].as_str())
        .or_else(|| message["user"]["id"].as_str())
}

fn valid_message_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128 && value.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}

#[derive(Serialize, Deserialize)]
struct Receipt {
    schema: u32,
    operation_id: String,
    binding: String,
    project_id: String,
    thread_id: String,
    message_id: Option<String>,
    state: String,
    verified: bool,
}

fn reserve(
    root: &Path,
    operation: &str,
    binding: &str,
    project: &str,
    thread: &str,
) -> Result<(PathBuf, Receipt, bool)> {
    if !root.exists() {
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(root).context("cannot create native receipt directory")?;
        File::open(root.parent().ok_or_else(|| anyhow!("receipt parent missing"))?)?.sync_all()?;
    }
    let meta = fs::symlink_metadata(root)?;
    ensure!(
        meta.is_dir() && !meta.file_type().is_symlink(),
        "receipt directory must be a real private directory"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            meta.permissions().mode() & 0o077 == 0,
            "receipt directory must be private (0700)"
        );
    }
    let path = root.join(format!("{operation}.json"));
    let receipt = Receipt {
        schema: 1,
        operation_id: operation.into(),
        binding: binding.into(),
        project_id: project.into(),
        thread_id: thread.into(),
        message_id: None,
        state: "uncertain".into(),
        verified: false,
    };
    let mut file = tempfile::NamedTempFile::new_in(root)?;
    file.write_all(&serde_json::to_vec(&receipt)?)?;
    file.as_file().sync_all()?;
    match file.persist_noclobber(&path) {
        Ok(_) => {
            File::open(root)?.sync_all()?;
            Ok((path, receipt, true))
        }
        Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {
            let mut options = fs::OpenOptions::new();
            options.read(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.custom_flags(nix::libc::O_NOFOLLOW);
            }
            let file = options.open(&path)?;
            ensure!(
                file.metadata()?.is_file() && file.metadata()?.len() <= 4096,
                "invalid native receipt; do not retry"
            );
            let prior: Receipt = serde_json::from_reader(file.take(4097))
                .map_err(|_| anyhow!("invalid native receipt; do not retry"))?;
            ensure!(
                prior.schema == 1
                    && prior.operation_id == operation
                    && prior.binding == binding
                    && prior.project_id == project
                    && prior.thread_id == thread,
                "operation_id is already bound to another reply"
            );
            Ok((path, prior, false))
        }
        Err(_) => bail!("cannot persist native attempt; no reply submitted"),
    }
}

fn save(path: &Path, receipt: &Receipt) -> Result<()> {
    let root = path.parent().unwrap();
    let mut file = tempfile::NamedTempFile::new_in(root)?;
    file.write_all(&serde_json::to_vec(receipt)?)?;
    file.as_file().sync_all()?;
    file.persist(path)
        .map_err(|_| anyhow!("cannot persist native outcome; do not retry"))?;
    File::open(root)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use std::thread;
    const PROJECT: &str = "111111111111111111111111";
    const THREAD: &str = "222222222222222222222222";
    const ACCOUNT: &str = "333333333333333333333333";
    const DOC: &str = "444444444444444444444444";
    const COOKIE: &str = "overleaf_session2=fixture-session";
    const OP: &str = "da7a5d2a-cfb0-4ed9-9ec1-f7f63c34ee77";
    // The real HTTP client, MCP dispatcher and on-disk receipts run against a
    // loopback server. Losing a POST response must never trigger another POST.
    struct Fixture {
        origin: String,
        posts: Arc<AtomicUsize>,
        stopped: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
    }
    impl Fixture {
        fn start(lost_response: bool) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let origin = format!("http://{}", listener.local_addr().unwrap());
            let posts = Arc::new(AtomicUsize::new(0));
            let stopped = Arc::new(AtomicBool::new(false));
            let (count, stop) = (posts.clone(), stopped.clone());
            let thread = thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    let (mut stream, _) = match listener.accept() {
                        Ok(v) => v,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(std::time::Duration::from_millis(2));
                            continue;
                        }
                        Err(e) => panic!("{e}"),
                    };
                    stream
                        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                        .unwrap();
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut first = String::new();
                    reader.read_line(&mut first).unwrap();
                    let mut headers = String::new();
                    loop {
                        let mut line = String::new();
                        reader.read_line(&mut line).unwrap();
                        if line == "\r\n" || line.is_empty() {
                            break;
                        }
                        headers.push_str(&line);
                    }
                    assert!(headers.contains(COOKIE), "missing injected cookie");
                    let url = first.split_whitespace().nth(1).unwrap();
                    let (kind, body) = if first.starts_with("POST ") {
                        assert_eq!(url, format!("/project/{PROJECT}/thread/{THREAD}/messages"));
                        assert!(headers.to_lowercase().contains("x-csrf-token: fixture-csrf"));
                        let size: usize = headers
                            .lines()
                            .find_map(|l| {
                                l.to_lowercase()
                                    .strip_prefix("content-length: ")
                                    .and_then(|v| v.trim().parse().ok())
                            })
                            .unwrap();
                        let mut body = vec![0; size];
                        reader.read_exact(&mut body).unwrap();
                        assert_eq!(
                            serde_json::from_slice::<Value>(&body).unwrap()["content"],
                            "A precise reply."
                        );
                        count.fetch_add(1, Ordering::SeqCst);
                        if lost_response {
                            continue;
                        }
                        ("application/json", json!({"id":"message-new"}).to_string())
                    } else if url.ends_with("/threads") {
                        let mut messages = vec![
                            json!({"id":"message-original","content":"Please clarify.","timestamp":1,"user":null}),
                        ];
                        if count.load(Ordering::SeqCst) > 0 {
                            messages.push(json!({"id":"message-new","content":"A precise reply.","timestamp":2,"user":{"_id":ACCOUNT}}));
                        }
                        (
                            "application/json",
                            json!({THREAD:{"messages":messages,"resolved":false}}).to_string(),
                        )
                    } else if url.ends_with("/ranges") {
                        ("application/json", json!([{"id":DOC,"ranges":{"comments":[{"id":"range-1","op":{"t":THREAD,"p":3,"c":"method"}}]}}]).to_string())
                    } else {
                        assert_eq!(url, format!("/project/{PROJECT}"));
                        (
                            "text/html",
                            format!(
                                "<meta name='ol-project_id' content='{PROJECT}'><meta content='{{&quot;_id&quot;:&quot;{ACCOUNT}&quot;}}' name='ol-user'><meta name='ol-csrfToken' content='fixture-csrf'>"
                            ),
                        )
                    };
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                }
            });
            Self {
                origin,
                posts,
                stopped,
                thread: Some(thread),
            }
        }
        fn session(&self, receipts: &std::path::Path) -> NativeSession {
            NativeSession {
                origin: self.origin.clone(),
                cookie: COOKIE.into(),
                receipts: receipts.to_owned(),
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.stopped.store(true, Ordering::SeqCst);
            self.thread.take().unwrap().join().unwrap();
        }
    }
    fn rpc(native: NativeSession, name: &str, args: Value) -> Value {
        let input = format!(
            "{}\n",
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}})
        );
        let mut output = Vec::new();
        crate::serve_with_native(input.as_bytes(), &mut output, native).unwrap();
        let response: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(response["result"]["isError"], false, "{response}");
        serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    }
    #[test]
    fn native_comments_read_and_reply_without_replay() {
        for lost in [false, true] {
            let f = Fixture::start(lost);
            let root = tempfile::tempdir().unwrap();
            let receipts = root.path().join("receipts");
            let input = format!("{}\n", json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}));
            let mut output = Vec::new();
            crate::serve_with_native(input.as_bytes(), &mut output, f.session(&receipts)).unwrap();
            let tools: Value = serde_json::from_slice(&output).unwrap();
            assert!(
                tools["result"]["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|t| t["name"] == "native_reply_comment")
            );
            let list = rpc(
                f.session(&receipts),
                "native_list_comments",
                json!({"project_id":PROJECT}),
            );
            assert_eq!(list["threads"][0]["thread_id"], THREAD);
            assert_eq!(list["threads"][0]["anchors"][0]["doc_id"], DOC);
            let before = rpc(
                f.session(&receipts),
                "native_read_comment_thread",
                json!({"project_id":PROJECT,"thread_id":THREAD}),
            );
            assert_eq!(before["thread"]["messages"].as_array().unwrap().len(), 1);
            let args = json!({"project_id":PROJECT,"thread_id":THREAD,"content":"A precise reply.","operation_id":OP});
            let result = rpc(f.session(&receipts), "native_reply_comment", args.clone());
            assert_eq!(result["state"], if lost { "uncertain" } else { "confirmed" });
            assert_eq!(result["verified"], !lost);
            let receipt_path = std::fs::read_dir(&receipts).unwrap().next().unwrap().unwrap().path();
            let prior_modified = std::fs::metadata(&receipt_path).unwrap().modified().unwrap();
            let again = rpc(f.session(&receipts), "native_reply_comment", args);
            assert_eq!(again["state"], result["state"]);
            assert_eq!(
                std::fs::metadata(&receipt_path).unwrap().modified().unwrap(),
                prior_modified,
                "checking a completed/uncertain operation rewrote authoritative evidence"
            );

            assert_eq!(f.posts.load(Ordering::SeqCst), 1, "restarted session replayed a reply");
            let after = rpc(
                f.session(&receipts),
                "native_read_comment_thread",
                json!({"project_id":PROJECT,"thread_id":THREAD}),
            );
            assert_eq!(after["thread"]["messages"].as_array().unwrap().len(), 2);
            let disk =
                std::fs::read_to_string(std::fs::read_dir(&receipts).unwrap().next().unwrap().unwrap().path()).unwrap();
            assert!(
                !disk.contains("A precise reply.")
                    && !disk.contains("fixture-session")
                    && !disk.contains("fixture-csrf")
            );
        }
    }
    #[test]
    #[cfg(unix)]
    fn inherited_session_is_memory_only_and_bound_to_origin() {
        use std::os::fd::IntoRawFd;
        use std::os::unix::net::UnixStream;
        let root = tempfile::tempdir().unwrap();
        for payload in [
            json!({"baseUrl":"https://www.overleaf.com","cookie":COOKIE}),
            json!({"baseUrl":"https://example.org","cookie":COOKIE}),
            json!({"baseUrl":"https://www.overleaf.com","cookie":"overleaf_session2=bad\nheader"}),
        ] {
            let (reader, mut writer) = UnixStream::pair().unwrap();
            writer.write_all(payload.to_string().as_bytes()).unwrap();
            writer.shutdown(std::net::Shutdown::Write).unwrap();
            let fd = reader.into_raw_fd();
            let session = NativeSession::from_inherited_socket(fd, root.path().join("receipts"));
            assert_eq!(
                session.is_ok(),
                payload["baseUrl"] == "https://www.overleaf.com" && payload["cookie"] == COOKIE
            );
            let mut buffer = [0u8; 1];
            assert_eq!(
                nix::sys::socket::recv(fd, &mut buffer, nix::sys::socket::MsgFlags::MSG_DONTWAIT),
                Err(nix::errno::Errno::EBADF)
            );
        }
        assert_eq!(
            std::fs::read_dir(root.path()).unwrap().count(),
            0,
            "credentials persisted to disk"
        );
        assert!(NativeSession::from_reader(&vec![b'x'; 16385][..], root.path().join("receipts")).is_err());
    }

    #[test]
    fn malformed_targets_and_changed_operation_payload_never_post() {
        let f = Fixture::start(false);
        let root = tempfile::tempdir().unwrap();
        let native = f.session(&root.path().join("receipts"));
        for args in [
            json!({}),
            json!({"project_id":"../other"}),
            json!({"project_id":PROJECT,"include_resolved":"true"}),
            json!({"project_id":PROJECT,"thread_id":"bad","content":"x","operation_id":OP}),
            json!({"project_id":PROJECT,"thread_id":THREAD,"content":"x","operation_id":"bad"}),
            json!({"project_id":PROJECT,"thread_id":THREAD,"content":"x","operation_id":OP,"path":"../"}),
        ] {
            let name = if args.get("content").is_some() {
                "native_reply_comment"
            } else {
                "native_list_comments"
            };
            assert!(native.call(name, args.as_object().unwrap()).is_err());
        }
        assert_eq!(f.posts.load(Ordering::SeqCst), 0);
        rpc(
            f.session(&root.path().join("receipts")),
            "native_reply_comment",
            json!({"project_id":PROJECT,"thread_id":THREAD,"content":"A precise reply.","operation_id":OP}),
        );
        assert!(
            native
                .call(
                    "native_reply_comment",
                    json!({"project_id":PROJECT,"thread_id":THREAD,"content":"Different reply.","operation_id":OP})
                        .as_object()
                        .unwrap()
                )
                .is_err()
        );
        assert_eq!(f.posts.load(Ordering::SeqCst), 1);
    }
}
