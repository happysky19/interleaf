mod build;
mod cloud;
mod edits;
mod files;
mod manage;
mod socket;
// Native Overleaf collaboration. Login and project authorization belong to the launcher.
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
    identity: Option<(String, String)>,
    upload_roots: Vec<PathBuf>,
    projects: Option<Vec<String>>,
    operations: Option<Vec<String>>,
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
        let origin = crate::login::normalize_base_url(&envelope.base_url)?;
        cookie(&envelope.cookie)?;
        Ok(Self {
            origin,
            cookie: envelope.cookie,
            receipts,
            identity: None,
            upload_roots: Vec::new(),
            projects: None,
            operations: None,
        })
    }

    /// Bind the session to the launcher-verified account, checked on every call.
    pub fn with_identity(mut self, account_id: &str, email: &str) -> Result<Self> {
        id(account_id)?;
        ensure!(!email.is_empty() && email.len() <= 254, "invalid expected account");
        self.identity = Some((account_id.into(), email.to_ascii_lowercase()));
        Ok(self)
    }

    /// The trusted host supplies filesystem roots; MCP arguments cannot widen them.
    pub fn with_upload_roots(mut self, roots: Vec<PathBuf>) -> Result<Self> {
        for root in &roots {
            ensure!(
                root.is_absolute() && root.is_dir() && root.canonicalize()? == *root,
                "upload roots must be resolved absolute directories"
            );
        }
        self.upload_roots = roots;
        Ok(self)
    }

    pub(super) fn upload_roots(&self) -> &[PathBuf] {
        &self.upload_roots
    }

    /// Restrict the native protocol independently of the MCP authorization callback.
    pub fn with_capabilities(mut self, projects: Option<Vec<String>>, operations: Vec<String>) -> Result<Self> {
        if let Some(ids) = &projects {
            ensure!(ids.len() <= 500, "too many native project grants");
            for project in ids {
                id(project)?;
            }
        }
        let valid = native_operations();
        let unique: BTreeSet<_> = operations.iter().collect();
        ensure!(
            unique.len() == operations.len() && operations.iter().all(|op| op == "direct_edit" || valid.contains(op)),
            "invalid native capability"
        );
        self.projects = projects;
        self.operations = Some(operations);
        Ok(self)
    }

    pub(super) fn project_scope(&self) -> Option<&[String]> {
        self.projects.as_deref()
    }

    fn permits_tool(&self, name: &str) -> bool {
        native_operation(name).is_some_and(|op| self.operations.as_ref().is_none_or(|ops| ops.contains(&op)))
    }

    pub(crate) fn tool_definitions(&self) -> Vec<Value> {
        definitions()
            .into_iter()
            .filter(|d| d["name"].as_str().is_some_and(|n| self.permits_tool(n)))
            .collect()
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
        let definition = self
            .tool_definitions()
            .into_iter()
            .find(|d| d["name"] == name)
            .ok_or_else(|| anyhow!("unknown native tool"))?;
        validate_arguments(&definition["inputSchema"], &Value::Object(args.clone()), 0)?;
        if name == "native_ping" {
            return Ok(json!({"pong":true,"version":crate::VERSION}).to_string());
        }
        let account_tool = matches!(name, "native_list_projects" | "native_create_project");
        let project = if account_tool { "" } else { need(args, "project_id")? };
        if !project.is_empty() {
            id(project)?;
            ensure!(
                self.projects
                    .as_ref()
                    .is_none_or(|ids| ids.iter().any(|p| p == project)),
                "project is outside the native grant"
            );
        }
        if name == "native_create_project" {
            ensure!(
                self.projects.is_none(),
                "project creation requires an account-wide grant"
            );
        }
        if matches!(name, "native_edit_file" | "native_find_and_replace") {
            let track = args.get("track").and_then(Value::as_str).unwrap_or("on");
            ensure!(
                track == "on"
                    || self
                        .operations
                        .as_ref()
                        .is_none_or(|ops| ops.iter().any(|op| op == "direct_edit")),
                "direct edits require an explicit direct_edit capability"
            );
        }
        if name == "native_list_projects" && self.projects.as_ref().is_some_and(Vec::is_empty) {
            return Ok(json!({"projects":[],"total_visible":0,"total_account":null,"complete":true}).to_string());
        }
        if is_mutation(name) {
            ensure!(
                crate::text::re!(r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$")
                    .is_match(need(args, "operation_id")?),
                "a stable operation_id UUID is required"
            );
        }
        let bootstrap = if name == "native_list_projects" {
            self.projects
                .as_ref()
                .and_then(|ids| ids.first())
                .map(String::as_str)
                .unwrap_or("")
        } else {
            project
        };
        let mut client = Client::new(self, bootstrap)?;
        // Read the durable outcome before domain preflights: successful deletion or
        // an old expected_version must not turn the same UUID into another attempt.
        if is_mutation(name) {
            let binding = if name == "native_reply_comment" {
                format!(
                    "{:x}",
                    Sha256::digest(serde_json::to_vec(&json!([
                        client.origin,
                        client.account,
                        client.project,
                        need(args, "thread_id")?,
                        need(args, "content")?
                    ]))?)
                )
            } else {
                format!(
                    "{:x}",
                    Sha256::digest(serde_json::to_vec(&json!([
                        name,
                        client.origin,
                        client.account,
                        client.project,
                        args
                    ]))?)
                )
            };
            if let Some(prior) = existing(&self.receipts, need(args, "operation_id")?, &binding, &client.project)? {
                return Ok(client.redact(&outcome(&prior, true).to_string()));
            }
        }
        let value = match name {
            "native_list_projects"
            | "native_create_project"
            | "native_open_project"
            | "native_list_files"
            | "native_read_file"
            | "native_search_project"
            | "native_create_doc"
            | "native_create_folder"
            | "native_upload_file"
            | "native_rename_entity"
            | "native_move_entity"
            | "native_delete_entity" => files::call(&mut client, self, name, args)?,
            "native_compile"
            | "native_read_log"
            | "native_label_pages"
            | "native_list_history"
            | "native_diff_file" => cloud::call(&mut client, self, name, args)?,
            "native_resolve_comment"
            | "native_reopen_comment"
            | "native_delete_comment"
            | "native_edit_comment_message"
            | "native_delete_comment_message"
            | "native_set_track_changes"
            | "native_accept_changes"
            | "native_reject_changes" => manage::call(&mut client, self, name, args)?,
            "native_compile_local" => build::compile(&mut client, self, args)?,
            "native_list_comments" => {
                let threads = client.threads()?;
                client.list(args, &threads)?
            }
            "native_read_comment_thread" => {
                let threads = client.threads()?;
                let thread = need(args, "thread_id")?;
                id(thread)?;
                ensure!(
                    threads.get(thread).is_some(),
                    "comment thread not found in this project"
                );
                json!({"project_id":project,"thread_id":thread,"thread":threads[thread],"complete":true})
            }
            "native_reply_comment" => {
                let thread = need(args, "thread_id")?;
                id(thread)?;
                let content = need(args, "content")?;
                ensure!(
                    !content.trim().is_empty() && content.len() <= 32768,
                    "comment text exceeds limit"
                );
                let threads = client.threads()?;
                client.reply(self, args, &threads)?
            }
            _ => edits::call(&mut client, self, name, args)?,
        };
        let encoded = client.redact(&serde_json::to_string(&value)?);
        ensure!(
            encoded.len() <= 2 * 1024 * 1024,
            "native result exceeds the MCP frame limit; select a document or use smaller binary pages"
        );
        Ok(encoded)
    }
}

pub(crate) fn definitions() -> Vec<Value> {
    let project = json!({"type":"string","pattern":"^[0-9a-f]{24}$","description":"Explicit Overleaf project ID from its URL; required on every call"});
    let thread = json!({"type":"string","pattern":"^[0-9a-f]{24}$"});
    let path = json!({"type":"string","description":"Project-relative text document path, e.g. main.tex or sections/introduction.tex"});
    let version = json!({"type":"integer","minimum":0,"description":"The exact version returned by native_read_file"});
    let occurrence = json!({"type":"integer","minimum":1,"description":"1-based exact match to use when text occurs more than once"});
    let operation = json!({"type":"string","format":"uuid","description":"Stable write UUID; reuse the same ID and arguments when checking an outcome"});
    let mut result: Vec<Value> = [
        ("native_list_comments","Read native review threads and quoted anchors. Open, anchored threads by default.",json!({"project_id":project,"include_resolved":{"type":"boolean"},"include_orphaned":{"type":"boolean"},"full":{"type":"boolean"}}),vec!["project_id"],true),
        ("native_read_comment_thread","Read all messages, authors and timestamps in one native review thread.",json!({"project_id":project,"thread_id":thread}),vec!["project_id","thread_id"],true),
        ("native_reply_comment","Reply once to an existing native thread. Uncertain writes are never resubmitted.",json!({"project_id":project,"thread_id":thread,"content":{"type":"string"},"operation_id":operation}),vec!["project_id","thread_id","content","operation_id"],false),
        ("native_compile_local","Compile a fresh native project snapshot with live text versions and source hashes; never publish changes. Requires local latexmk and a TeX engine.",json!({"project_id":project,"path":path,"engine":{"type":"string","enum":["pdflatex","xelatex","lualatex"]}}),vec!["project_id","path"],true),
        ("native_read_file","Read current live text and native document version before proposing an edit or anchor.",json!({"project_id":project,"path":path}),vec!["project_id","path"],true),
        ("native_list_tracked_changes","Read the complete pending Track Changes inventory. Optional path selects one document; no changes are accepted or rejected.",json!({"project_id":project,"path":path}),vec!["project_id"],true),
        ("native_suggest_edit","Suggest one exact replacement as pending Track Changes. Checks expected_version, preserves existing suggestions and verifies readback. Uncertain writes are never replayed.",json!({"project_id":project,"path":path,"old_string":{"type":"string","minLength":1},"new_string":{"type":"string"},"occurrence":occurrence,"expected_version":version,"operation_id":operation,"track":{"type":"string","const":"on"},"strict_version":{"type":"boolean","const":true}}),vec!["project_id","path","old_string","new_string","expected_version","operation_id"],false),
        ("native_add_comment","Create a native comment anchored to exact quoted text at expected_version. On a race, a partial thread remains for reconciliation; no destructive rollback or retry.",json!({"project_id":project,"path":path,"quoted_text":{"type":"string","minLength":1},"content":{"type":"string"},"occurrence":occurrence,"expected_version":version,"operation_id":operation}),vec!["project_id","path","quoted_text","content","expected_version","operation_id"],false),
    ].into_iter().filter(|(name, ..)| !matches!(*name, "native_read_file" | "native_list_tracked_changes")).map(|(name,description,properties,required,read_only)|json!({"name":name,"description":description,
        "inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false},
        "annotations":{"readOnlyHint":read_only,"destructiveHint":false,"idempotentHint":true}})).collect();
    if let Some(comments) = result.iter_mut().find(|d| d["name"] == "native_list_comments") {
        comments["inputSchema"]["properties"]["path_contains"] = json!({"type":"string"});
    }
    result.push(
        json!({"name":"native_ping","description":"Health check without account or network access.",
        "inputSchema":{"type":"object","properties":{},"required":[],"additionalProperties":false},
        "annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true}}),
    );
    result.extend(files::definitions());
    result.extend(edits::definitions());
    result.extend(manage::definitions());
    result.extend(cloud::definitions());
    result
}

pub(crate) fn is_mutation(name: &str) -> bool {
    definitions()
        .into_iter()
        .find(|d| d["name"] == name)
        .is_some_and(|d| d["annotations"]["readOnlyHint"] == false)
}

/// Canonical capabilities, independent of helper aliases.
pub fn native_operations() -> Vec<String> {
    definitions()
        .into_iter()
        .filter_map(|d| {
            let name = d["name"].as_str()?;
            (!matches!(name, "native_suggest_edit" | "native_compile_local"))
                .then(|| name.trim_start_matches("native_").to_owned())
        })
        .collect()
}

/// Map compatibility helpers to the same host-owned capability.
pub fn native_operation(name: &str) -> Option<String> {
    let op = match name {
        "native_suggest_edit" => "find_and_replace",
        "native_compile_local" => "read_file",
        n => n.strip_prefix("native_")?,
    };
    native_operations().iter().any(|n| n == op).then(|| op.to_owned())
}

pub fn native_tool_is_mutation(name: &str) -> bool {
    is_mutation(name)
}

/// Validate the advertised argument types before any domain code or network IO.
fn validate_arguments(schema: &Value, value: &Value, depth: usize) -> Result<()> {
    ensure!(depth < 24, "native arguments are nested too deeply");
    if let Some(kind) = schema["type"].as_str() {
        let valid = match kind {
            "string" => value.is_string(),
            "boolean" => value.is_boolean(),
            "integer" => value.is_u64() || value.is_i64(),
            "number" => value.is_number(),
            "array" => value.is_array(),
            "object" => value.is_object(),
            "null" => value.is_null(),
            _ => true,
        };
        ensure!(valid, "native argument type mismatch");
    }
    if let Some(options) = schema["enum"].as_array() {
        ensure!(options.contains(value), "native argument is outside allowed values");
    }
    if let Some(constant) = schema.get("const") {
        ensure!(constant == value, "native argument violates a fixed policy");
    }
    if let Some(text) = value.as_str() {
        if let Some(n) = schema["minLength"].as_u64() {
            ensure!(text.len() >= n as usize, "native text argument is empty");
        }
        if let Some(n) = schema["maxLength"].as_u64() {
            ensure!(text.len() <= n as usize, "native text argument exceeds limit");
        }
        if let Some(pattern) = schema["pattern"].as_str() {
            ensure!(
                regex::Regex::new(pattern)?.is_match(text),
                "native argument format mismatch"
            );
        }
    }
    if let Some(number) = value.as_f64() {
        if let Some(n) = schema["minimum"].as_f64() {
            ensure!(number >= n, "native number below minimum");
        }
        if let Some(n) = schema["maximum"].as_f64() {
            ensure!(number <= n, "native number above maximum");
        }
    }
    if let Some(object) = value.as_object() {
        if let Some(required) = schema["required"].as_array() {
            for key in required.iter().filter_map(Value::as_str) {
                ensure!(object.contains_key(key), "missing native argument {key}");
            }
        }
        let props = schema["properties"].as_object();
        for (key, field) in object {
            if let Some(spec) = props.and_then(|p| p.get(key)) {
                validate_arguments(spec, field, depth + 1)?;
            } else if schema["additionalProperties"] == false {
                bail!("unknown native argument {key}");
            } else if schema["additionalProperties"].is_object() {
                validate_arguments(&schema["additionalProperties"], field, depth + 1)?;
            }
        }
    }
    if let Some(array) = value.as_array() {
        ensure!(array.len() <= 4096, "native array exceeds limit");
        if let Some(items) = schema.get("items") {
            for item in array {
                validate_arguments(items, item, depth + 1)?;
            }
        }
    }
    Ok(())
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
    last_content_type: Option<String>,
}

impl Client {
    fn new(session: &NativeSession, project: &str) -> Result<Self> {
        let mut client = Self {
            http: ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(60)))
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
            last_content_type: None,
        };
        let page = client.get(
            &if project.is_empty() {
                "/project".into()
            } else {
                format!("/project/{project}")
            },
            "text/html",
        )?;
        if !project.is_empty() {
            ensure!(
                meta(&page, "ol-project_id")? == project,
                "native project verification failed"
            );
        }
        let user: Value =
            serde_json::from_str(&meta(&page, "ol-user")?).map_err(|_| anyhow!("invalid native account metadata"))?;
        client.account = user
            .get("_id")
            .or_else(|| user.get("id"))
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("native account missing"))?
            .to_owned();
        id(&client.account)?;
        if let Some((account, email)) = &session.identity {
            ensure!(
                client.account == *account
                    && user["email"]
                        .as_str()
                        .is_some_and(|actual| actual.eq_ignore_ascii_case(email)),
                "native account does not match the saved login"
            );
        }
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
        self.rotate(response.headers())?;
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
    fn rotate(&mut self, headers: &ureq::http::HeaderMap) -> Result<()> {
        for header in headers.get_all("set-cookie") {
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
        Ok(())
    }

    /// One bounded same-origin request. No redirect, retry or external cookie forwarding.
    fn bytes(
        &mut self,
        method: &str,
        path: &str,
        body: Option<Vec<u8>>,
        content_type: Option<&str>,
    ) -> Result<Vec<u8>> {
        ensure!(
            path.starts_with('/')
                && !path.starts_with("//")
                && !path.contains('\\')
                && !path.chars().any(char::is_control),
            "native request must stay on its configured origin"
        );
        ensure!(
            matches!(method, "GET" | "POST" | "PUT" | "DELETE" | "PATCH"),
            "unsupported native HTTP method"
        );
        let mut cookie =
            ureq::http::HeaderValue::from_str(&self.cookie).map_err(|_| anyhow!("invalid native cookie header"))?;
        cookie.set_sensitive(true);
        let mut csrf =
            ureq::http::HeaderValue::from_str(&self.csrf).map_err(|_| anyhow!("invalid native CSRF header"))?;
        csrf.set_sensitive(true);
        let mut request = ureq::http::Request::builder()
            .method(method)
            .uri(format!("{}{path}", self.origin))
            .header("Cookie", cookie)
            .header("X-Csrf-Token", csrf)
            .header("Connection", "close");
        if let Some(kind) = content_type {
            request = request.header("Content-Type", kind);
        }
        let body = body.unwrap_or_default();
        ensure!(body.len() <= 48 * 1024 * 1024, "native upload body exceeds limit");
        let request = request.body(body).map_err(|_| anyhow!("invalid native HTTP request"))?;
        let mut response = self
            .http
            .run(request)
            .map_err(|_| anyhow!("native HTTP outcome unavailable; do not replay mutations"))?;
        let status = response.status().as_u16();
        ensure!(
            (200..300).contains(&status),
            "native HTTP refused request (status {status})"
        );
        self.rotate(response.headers())?;
        self.last_content_type = response
            .headers()
            .get("content-type")
            .and_then(|h| h.to_str().ok())
            .map(|s| s.split(';').next().unwrap_or(s).to_owned());
        let mut data = Vec::new();
        response
            .body_mut()
            .as_reader()
            .take(32 * 1024 * 1024 + 1)
            .read_to_end(&mut data)
            .map_err(|_| anyhow!("native HTTP body unavailable"))?;
        ensure!(data.len() <= 32 * 1024 * 1024, "native HTTP body exceeds 32 MiB");
        Ok(data)
    }

    fn json(&mut self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
        let body = body.map(|v| serde_json::to_vec(&v)).transpose()?;
        let bytes = self.bytes(method, path, body, Some("application/json"))?;
        if bytes.is_empty() {
            return Ok(Value::Null);
        }
        ensure!(
            self.last_content_type
                .as_deref()
                .is_some_and(|s| s == "application/json" || s.ends_with("+json")),
            "native HTTP did not return JSON"
        );
        ensure!(bytes.len() <= LIMIT as usize, "native JSON response exceeds limit");
        serde_json::from_slice(&bytes).map_err(|_| anyhow!("invalid native JSON response"))
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
        for key in ["include_resolved", "include_orphaned", "full"] {
            ensure!(args.get(key).is_none_or(Value::is_boolean), "{key} must be boolean");
        }
        let needle = args
            .get("path_contains")
            .map(|v| {
                v.as_str()
                    .filter(|s| s.len() <= 1024)
                    .map(str::to_lowercase)
                    .ok_or_else(|| anyhow!("path_contains must be bounded text"))
            })
            .transpose()?
            .filter(|s| !s.is_empty());
        let socket = self.socket()?;
        let docs = edits::documents(&socket.project)?;
        let by_id: std::collections::BTreeMap<_, _> =
            docs.iter().map(|(path, id)| (id.as_str(), path.as_str())).collect();
        let mut users = std::collections::BTreeMap::new();
        for user in
            std::iter::once(&socket.project["owner"]).chain(socket.project["members"].as_array().into_iter().flatten())
        {
            if let Some(id) = user["_id"].as_str() {
                users.insert(id, user);
            }
        }
        let mut anchors: Map<String, Value> = Map::new();
        for doc in edits::ranges(self)? {
            let doc_id = doc["id"].as_str().unwrap();
            if let Some(comments) = doc["ranges"].get("comments") {
                for range in comments.as_array().ok_or_else(|| anyhow!("invalid native anchors"))? {
                    let op = &range["op"];
                    let thread = op["t"]
                        .as_str()
                        .ok_or_else(|| anyhow!("native anchor thread missing"))?;
                    id(thread)?;
                    ensure!(
                        op["p"].as_u64().is_some_and(|p| p <= 9_007_199_254_740_991) && op["c"].is_string(),
                        "invalid native anchor"
                    );
                    anchors.entry(thread.to_owned()).or_insert_with(||json!([])).as_array_mut().unwrap()
                        .push(json!({"doc_id":doc_id,"doc_path":by_id.get(doc_id),"path":by_id.get(doc_id),"position":op["p"],"quoted_text":op["c"]}));
                }
            }
        }
        let flag = |key: &str| args.get(key) == Some(&Value::Bool(true));
        let thread_map = threads
            .as_object()
            .ok_or_else(|| anyhow!("complete native thread map required"))?;
        let mut rows = Vec::new();
        let mut open = 0;
        let mut resolved_count = 0;
        let mut orphan_count = 0;
        let mut hidden_orphaned = 0;
        for (thread, value) in thread_map {
            let resolved = value["resolved"].as_bool().unwrap_or(false);
            if resolved {
                resolved_count += 1;
            } else {
                open += 1;
            }
            let anchored = anchors.get(thread).cloned().unwrap_or_else(|| json!([]));
            let list = anchored.as_array().unwrap();
            let orphaned = list.is_empty();
            if orphaned {
                orphan_count += 1;
            }
            if resolved && !flag("include_resolved") {
                continue;
            }
            if orphaned && !flag("include_orphaned") {
                hidden_orphaned += 1;
                continue;
            }
            if let Some(needle) = &needle
                && !list.iter().any(|a| {
                    a["doc_path"]
                        .as_str()
                        .is_some_and(|p| p.to_lowercase().contains(needle))
                })
            {
                continue;
            }
            let messages = value["messages"]
                .as_array()
                .ok_or_else(|| anyhow!("complete native thread messages required"))?;
            let first = messages.first();
            let last = messages.last();
            let first_id = first.and_then(author);
            let known = first_id.and_then(|id| users.get(id)).copied();
            let embedded = first.map(|m| &m["user"]).filter(|u| u.is_object());
            let person = embedded
                .filter(|u| {
                    ["first_name", "last_name", "email"]
                        .iter()
                        .any(|key| u[*key].as_str().is_some_and(|s| !s.is_empty()))
                })
                .or(known);
            let first_name = person.and_then(|u| u["first_name"].as_str()).unwrap_or("");
            let last_name = person.and_then(|u| u["last_name"].as_str()).unwrap_or("");
            let name = format!("{first_name} {last_name}").trim().to_owned();
            let email = person.and_then(|u| u["email"].as_str());
            let first_author = if name.is_empty() {
                email.map(str::to_owned)
            } else {
                Some(name)
            };
            let content = last.and_then(|m| m["content"].as_str()).unwrap_or("");
            let preview = content.chars().take(120).collect::<String>();
            let last_updated = last.or(first).map(|m| m["timestamp"].clone()).unwrap_or(Value::Null);
            let primary = list.last();
            let mut row = json!({"thread_id":thread,"resolved":resolved,"orphaned":orphaned,"anchors":anchored,"message_count":messages.len(),
                "doc_id":primary.map(|a|&a["doc_id"]),"doc_path":primary.map(|a|&a["doc_path"]),
                "position":primary.map(|a|&a["position"]),"quoted_text":primary.map(|a|&a["quoted_text"]),
                "first_author":first_author,"first_author_id":first_id,"first_author_email":email,"last_updated":last_updated,
                "preview":preview,"preview_truncated":content.chars().count()>120});
            if flag("full") {
                row["full_thread"] = value.clone();
            }
            rows.push(row);
        }
        rows.sort_by(|a, b| {
            let order = match (a["last_updated"].as_str(), b["last_updated"].as_str()) {
                (Some(a), Some(b)) => match (a.parse::<f64>(), b.parse::<f64>()) {
                    (Ok(a), Ok(b)) => b.total_cmp(&a),
                    _ => b.cmp(a), // Server Date timestamps use UTC ISO-8601.
                },
                _ => b["last_updated"]
                    .as_f64()
                    .unwrap_or(0.0)
                    .total_cmp(&a["last_updated"].as_f64().unwrap_or(0.0)),
            };
            order.then_with(|| a["thread_id"].as_str().cmp(&b["thread_id"].as_str()))
        });
        Ok(
            json!({"project_id":self.project,"threads":rows,"count":rows.len(),"complete":true,"truncated":false,
            "hidden_orphaned":hidden_orphaned,"summary":{"total_threads":thread_map.len(),"open_threads":open,"resolved_threads":resolved_count,"orphaned_threads":orphan_count}}),
        )
    }
    fn message(&mut self, thread: &str, content: &str) -> Result<Option<String>> {
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
    }
    fn posted_message_id(
        &mut self,
        thread: &str,
        content: &str,
        returned: Option<&str>,
        previous: &BTreeSet<&str>,
    ) -> Result<Option<String>> {
        let after = self.threads()?;
        let candidates: Vec<_> = after[thread]["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|m| m["content"] == content && author(m) == Some(self.account.as_str()))
            .filter_map(|m| m["id"].as_str())
            .filter(|id| !previous.contains(id))
            .collect();
        if let Some(id) = returned {
            return Ok(candidates
                .into_iter()
                .find(|candidate| *candidate == id)
                .map(str::to_owned));
        }
        Ok((candidates.len() == 1).then(|| candidates[0].to_owned()))
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
        let (file, mut receipt, fresh) = reserve(
            &session.receipts,
            operation,
            &binding,
            &self.project,
            Some(thread),
            Value::Null,
        )?;
        if !fresh {
            return Ok(outcome(&receipt, true));
        }
        let previous: BTreeSet<_> = threads[thread]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m["id"].as_str())
            .collect();
        // Only an acknowledged POST may use an id-less response fallback.
        // A lost acknowledgement stays uncertain even if a matching body appears.
        if let Ok(returned) = self.message(thread, content) {
            receipt.message_id = returned.clone();
            if save(&file, &receipt).is_err() {
                return Ok(outcome(&receipt, false));
            }
            if let Ok(Some(id)) = self.posted_message_id(thread, content, returned.as_deref(), &previous) {
                receipt.message_id = Some(id);
                receipt.verified = true;
                receipt.state = "confirmed".into();
            }
        }
        let persisted = save(&file, &receipt).is_ok();
        Ok(outcome(&receipt, persisted))
    }
}

/// All new mutation domains use the same durable intent/no-replay journal.
fn once(
    client: &mut Client,
    session: &NativeSession,
    name: &str,
    args: &Map<String, Value>,
    mut details: Value,
    action: impl FnOnce(&mut Client, &mut Receipt) -> Result<Value>,
) -> Result<Value> {
    let operation = need(args, "operation_id")?;
    ensure!(
        crate::text::re!(r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$").is_match(operation),
        "a stable operation_id UUID is required"
    );
    let binding = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&json!([
            name,
            client.origin,
            client.account,
            client.project,
            args
        ]))?)
    );
    if !details.is_object() {
        details = json!({});
    }
    details["operation"] = json!(name);
    let (file, mut receipt, fresh) = reserve(
        &session.receipts,
        operation,
        &binding,
        &client.project,
        args.get("thread_id").and_then(Value::as_str),
        details,
    )?;
    if !fresh {
        return Ok(outcome(&receipt, true));
    }
    let result = action(client, &mut receipt);
    receipt.state = if receipt.verified && result.is_ok() {
        "confirmed"
    } else {
        "uncertain"
    }
    .into();
    let mut value = match result {
        Ok(value) => value.as_object().cloned().unwrap_or_default(),
        Err(_) => {
            receipt.verified = false;
            receipt.details["readback_incomplete"] = json!(true);
            Map::new()
        }
    };
    let persisted = save(&file, &receipt).is_ok();
    let preserved_project = value.get("project_id").cloned();
    value.extend(outcome(&receipt, persisted).as_object().unwrap().clone());
    if receipt.project_id.is_empty()
        && let Some(project) = preserved_project
    {
        value.insert("project_id".into(), project);
    }
    Ok(Value::Object(value))
}

fn outcome(receipt: &Receipt, persisted: bool) -> Value {
    let complete = receipt.verified && persisted;
    let next = if complete {
        "complete"
    } else if receipt.details["operation"] == "native_suggest_edit" {
        "Read native_read_file and native_list_tracked_changes to reconcile; retain this operation_id and do not resubmit with a new ID."
    } else if receipt.details["operation"] == "native_compile" {
        "Use native_read_log or native_label_pages with this operation_id to inspect retained output; do not automatically repeat the compile."
    } else if receipt.details["operation"] == "native_add_comment" {
        "Read native_read_comment_thread for this thread_id and native_list_comments with include_orphaned=true; retain this operation_id and do not resubmit."
    } else if receipt.thread_id.is_some() {
        "Read native_read_comment_thread for this thread_id to reconcile; retain this operation_id and do not resubmit with a new ID."
    } else {
        "Read the affected project/file/history to reconcile; retain this operation_id and do not resubmit with a new ID."
    };
    let mut result = receipt.details.as_object().cloned().unwrap_or_default();
    result.extend(json!({"operation_id":receipt.operation_id,"project_id":receipt.project_id,"thread_id":receipt.thread_id,"message_id":receipt.message_id,
        "state":if persisted {receipt.state.as_str()}else{"uncertain"},"verified":complete,"receipt_persisted":persisted,"next":next}).as_object().unwrap().clone());
    Value::Object(result)
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
    thread_id: Option<String>,
    #[serde(default)]
    details: Value,
    message_id: Option<String>,
    state: String,
    verified: bool,
}

fn reserve(
    root: &Path,
    operation: &str,
    binding: &str,
    project: &str,
    thread: Option<&str>,
    details: Value,
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
    check_receipt_dir(root)?;
    let path = root.join(format!("{operation}.json"));
    let receipt = Receipt {
        schema: 1,
        operation_id: operation.into(),
        binding: binding.into(),
        project_id: project.into(),
        thread_id: thread.map(str::to_owned),
        details,
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
            let prior = existing(root, operation, binding, project)?
                .ok_or_else(|| anyhow!("native receipt disappeared; do not retry"))?;
            ensure!(prior.thread_id.as_deref() == thread, "operation_id target changed");
            Ok((path, prior, false))
        }
        Err(_) => bail!("cannot persist native attempt; no reply submitted"),
    }
}

fn existing(root: &Path, operation: &str, binding: &str, project: &str) -> Result<Option<Receipt>> {
    match fs::symlink_metadata(root) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
        Ok(_) => {}
    }
    check_receipt_dir(root)?;
    let path = root.join(format!("{operation}.json"));
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(nix::libc::O_NOFOLLOW);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.len() <= 512 * 1024,
        "invalid native receipt; do not retry"
    );
    let receipt: Receipt = serde_json::from_reader(file.take(512 * 1024 + 1))
        .map_err(|_| anyhow!("invalid native receipt; do not retry"))?;
    ensure!(
        receipt.schema == 1
            && receipt.operation_id == operation
            && receipt.binding == binding
            && receipt.project_id == project,
        "operation_id is already bound to another operation"
    );
    Ok(Some(receipt))
}
fn check_receipt_dir(root: &Path) -> Result<()> {
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
    Ok(())
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
        fn start(lost_response: bool, no_message_id: bool) -> Self {
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
                    stream.set_nonblocking(false).unwrap();
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
                    if url.starts_with("/socket.io/1/websocket/") {
                        let key = headers
                            .lines()
                            .find_map(|line| {
                                line.split_once(':')
                                    .filter(|(key, _)| key.eq_ignore_ascii_case("sec-websocket-key"))
                                    .map(|(_, value)| value.trim())
                            })
                            .unwrap();
                        let accepted = tungstenite::handshake::derive_accept_key(key.as_bytes());
                        write!(stream,"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accepted}\r\n\r\n").unwrap();
                        let mut ws =
                            tungstenite::WebSocket::from_raw_socket(stream, tungstenite::protocol::Role::Server, None);
                        ws.send(tungstenite::Message::Text("1::".into())).unwrap();
                        let frame = ws.read().unwrap().into_text().unwrap();
                        let (sequence, event) = frame.strip_prefix("5:").unwrap().split_once("+::").unwrap();
                        let event: Value = serde_json::from_str(event).unwrap();
                        assert_eq!(event["name"], "joinProject");
                        assert_eq!(event["args"][0]["project_id"], PROJECT);
                        let project = json!({"_id":PROJECT,"owner":{"_id":ACCOUNT,"first_name":"Ada","last_name":"Writer","email":"author@example.invalid"},"members":[],
                            "rootFolder":[{"_id":"777777777777777777777777","name":"rootFolder","docs":[{"_id":DOC,"name":"main.tex"}],"folders":[],"fileRefs":[]}],"rootDoc_id":DOC});
                        ws.send(tungstenite::Message::Text(
                            format!("6:::{sequence}+{}", json!([null, project, "readAndWrite"])).into(),
                        ))
                        .unwrap();
                        continue;
                    }
                    let (kind, body) = if url.starts_with("/socket.io/1/?") {
                        ("text/plain", "fixture-sid:15:60:websocket".into())
                    } else if first.starts_with("POST ") {
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
                        if no_message_id {
                            write!(stream, "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n").unwrap();
                            continue;
                        }
                        ("application/json", json!({"id":"message-new"}).to_string())
                    } else if url.ends_with("/threads") {
                        let mut messages = vec![
                            json!({"id":"message-original","content":"Please clarify.","timestamp":1,"user":{"_id":ACCOUNT}}),
                        ];
                        if count.load(Ordering::SeqCst) > 0 {
                            messages.push(json!({"id":"message-new","content":"A precise reply.","timestamp":2,"user":{"_id":ACCOUNT}}));
                        }
                        (
                            "application/json",
                            json!({THREAD:{"messages":messages,"resolved":false},
                                "555555555555555555555555":{"messages":[{"id":"orphan-message","content":"Unanchored question.","timestamp":3,"user":null}],"resolved":false},
                                "666666666666666666666666":{"messages":[{"id":"resolved-message","content":"Already addressed.","timestamp":2,"user":null}],"resolved":true}}).to_string(),
                        )
                    } else if url.ends_with("/ranges") {
                        ("application/json", json!([{"id":DOC,"ranges":{"comments":[{"id":"range-1","op":{"t":THREAD,"p":3,"c":"method"}},{"id":"range-2","op":{"t":"666666666666666666666666","p":10,"c":"resolved"}}]}}]).to_string())
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
                identity: None,
                upload_roots: Vec::new(),
                projects: None,
                operations: None,
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
        rpc_expect(native, name, args, false)
    }
    fn rpc_expect(native: NativeSession, name: &str, args: Value, expected_error: bool) -> Value {
        let input = format!(
            "{}\n",
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}})
        );
        let mut output = Vec::new();
        crate::serve_with_native(input.as_bytes(), &mut output, native).unwrap();
        let response: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(response["result"]["isError"], expected_error, "{response}");
        serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    }
    #[test]
    fn native_comments_read_and_reply_without_replay() {
        for (lost, idless) in [(false, false), (true, false), (false, true)] {
            let f = Fixture::start(lost, idless);
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
            assert_eq!(list["threads"][0]["doc_path"], "main.tex");
            assert_eq!(list["threads"][0]["first_author"], "Ada Writer");
            assert_eq!(list["threads"][0]["first_author_email"], "author@example.invalid");
            assert_eq!(list["hidden_orphaned"], 1);
            assert_eq!(list["summary"]["total_threads"], 3);
            let filtered = rpc(
                f.session(&receipts),
                "native_list_comments",
                json!({"project_id":PROJECT,"path_contains":"SECTIONS/"}),
            );
            assert_eq!(filtered["count"], 0);
            let full = rpc(
                f.session(&receipts),
                "native_list_comments",
                json!({"project_id":PROJECT,"path_contains":"MAIN.TEX","include_resolved":true,"include_orphaned":true,"full":true}),
            );
            assert_eq!(full["count"], 2);
            assert_eq!(full["threads"][0]["thread_id"], "666666666666666666666666");
            assert!(full["threads"][0]["full_thread"].is_object());
            let all = rpc(
                f.session(&receipts),
                "native_list_comments",
                json!({"project_id":PROJECT,"include_resolved":true,"include_orphaned":true}),
            );
            assert_eq!(all["count"], 3);
            assert_eq!(all["threads"][0]["thread_id"], "555555555555555555555555");
            assert_eq!(all["hidden_orphaned"], 0);
            let before = rpc(
                f.session(&receipts),
                "native_read_comment_thread",
                json!({"project_id":PROJECT,"thread_id":THREAD}),
            );
            assert_eq!(before["thread"]["messages"].as_array().unwrap().len(), 1);
            let args = json!({"project_id":PROJECT,"thread_id":THREAD,"content":"A precise reply.","operation_id":OP});
            let result = rpc_expect(f.session(&receipts), "native_reply_comment", args.clone(), lost);
            assert_eq!(result["state"], if lost { "uncertain" } else { "confirmed" });
            assert_eq!(result["verified"], !lost);
            let receipt_path = std::fs::read_dir(&receipts).unwrap().next().unwrap().unwrap().path();
            let prior_modified = std::fs::metadata(&receipt_path).unwrap().modified().unwrap();
            let again = rpc_expect(f.session(&receipts), "native_reply_comment", args, lost);
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
            json!({"baseUrl":"http://example.org","cookie":COOKIE}),
            json!({"baseUrl":"https://www.overleaf.com","cookie":"overleaf_session2=bad\nheader"}),
        ] {
            let (reader, mut writer) = UnixStream::pair().unwrap();
            writer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
            writer.write_all(payload.to_string().as_bytes()).unwrap();
            writer.shutdown(std::net::Shutdown::Write).unwrap();
            let fd = reader.into_raw_fd();
            let session = NativeSession::from_inherited_socket(fd, root.path().join("receipts"));
            assert_eq!(
                session.is_ok(),
                payload["baseUrl"] == "https://www.overleaf.com" && payload["cookie"] == COOKIE
            );
            let mut buffer = [0u8; 1];
            assert_eq!(writer.read(&mut buffer).unwrap(), 0, "session socket remained open");
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
        let f = Fixture::start(false, false);
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
        let wrong_account = f
            .session(&root.path().join("receipts"))
            .with_identity("555555555555555555555555", "owner@example.invalid")
            .unwrap();
        assert!(
            wrong_account
                .call(
                    "native_list_comments",
                    json!({"project_id":PROJECT}).as_object().unwrap()
                )
                .is_err()
        );
        let input = [
            json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"push","arguments":{}}}),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"native_list_comments","arguments":{"project_id":PROJECT}}}),
        ].iter().map(Value::to_string).collect::<Vec<_>>().join("\n") + "\n";
        let mut output = Vec::new();
        crate::serve_native(
            input.as_bytes(),
            &mut output,
            f.session(&root.path().join("receipts")),
            |_, _| bail!("fixture task expired"),
        )
        .unwrap();
        let responses: Vec<Value> = std::str::from_utf8(&output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(
            responses[0]["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .all(|tool| tool["name"].as_str().unwrap().starts_with("native_"))
        );
        assert_eq!(responses[1]["result"]["isError"], true);
        assert_eq!(responses[2]["result"]["isError"], true);
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

#[cfg(test)]
mod socket_tests;
