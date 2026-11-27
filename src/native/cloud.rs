//! Server compile artifacts and native project history. No local TeX or Git fallback.
use super::{Client, NativeSession, check_receipt_dir, edits, need, once};
use anyhow::{Result, anyhow, bail, ensure};
use percent_encoding::{NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const RECORD_LIMIT: u64 = 128 * 1024;
const MAX_VERSION: u64 = 9_007_199_254_740_991;
const FRESHNESS: &str = "Retained server compile output; the current source has not been verified against this build.";

pub(super) fn definitions() -> Vec<Value> {
    let project = json!({"type":"string","pattern":"^[0-9a-f]{24}$"});
    let operation = json!({"type":"string","format":"uuid","description":"Stable compile UUID. Reuse the ID and arguments to inspect an uncertain attempt; never create a replacement ID automatically."});
    let optional_operation = json!({"type":"string","format":"uuid","description":"Select an exact retained compile. Omit to read the last started compile for this account/project."});
    let path = json!({"type":"string","minLength":1,"description":"Project-relative document path"});
    [
        ("native_compile", "Compile on the configured Overleaf server once. Reports actual server status and bounded log errors; a produced PDF can still contain LaTeX errors. Output does not certify unchanged current source.",
            json!({"project_id":project,"operation_id":operation,"root_doc":path,"path":path,"draft":{"type":"boolean"},"stop_on_first_error":{"type":"boolean"}}),
            vec!["project_id","operation_id"],false),
        ("native_read_log", "Read the full bounded output.log of an exact retained server compile, including first errors. Refuses missing or unknown build output instead of using an older compile.",
            json!({"project_id":project,"operation_id":optional_operation}),vec!["project_id"],true),
        ("native_label_pages", "Read label numbers and pages from output.aux of a retained server compile. Labels describe that build, not unverified current source.",
            json!({"project_id":project,"operation_id":optional_operation,"label_prefix":{"type":"string","maxLength":1024}}),vec!["project_id"],true),
        ("native_list_history", "Read the server's native project history, version ranges, authors, paths, operations and labels. This is Overleaf history, not Git history.",
            json!({"project_id":project,"limit":{"type":"integer","minimum":1,"maximum":100},"before":{"type":"integer","minimum":0},"path_contains":{"type":"string","maxLength":1024}}),vec!["project_id"],true),
        ("native_diff_file", "Read a native server history diff for one file and exact version range. Returns the original change chunks and a contextual unified text diff, or an explicit binary result.",
            json!({"project_id":project,"path":path,"from":{"type":"integer","minimum":0},"to":{"type":"integer","minimum":0},"context_lines":{"type":"integer","minimum":0,"maximum":20}}),vec!["project_id","path","from","to"],true),
    ].into_iter().map(|(name,description,properties,required,read_only)|json!({
        "name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false},
        "annotations":{"readOnlyHint":read_only,"destructiveHint":false,"idempotentHint":true}
    })).collect()
}

pub(super) fn call(
    client: &mut Client,
    session: &NativeSession,
    name: &str,
    args: &Map<String, Value>,
) -> Result<Value> {
    match name {
        "native_compile" => compile(client, session, args),
        "native_read_log" => read_log(client, session, args),
        "native_label_pages" => label_pages(client, session, args),
        "native_list_history" => history(client, args),
        "native_diff_file" => diff(client, args),
        _ => bail!("unknown native cloud tool"),
    }
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn uuid(value: &str) -> Result<()> {
    ensure!(
        crate::text::re!(r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$").is_match(value),
        "a stable operation_id UUID is required"
    );
    Ok(())
}

fn integer(args: &Map<String, Value>, name: &str, default: Option<u64>, max: u64) -> Result<u64> {
    let value = match args.get(name) {
        Some(value) => value.as_u64(),
        None => default,
    }
    .ok_or_else(|| anyhow!("{name} must be a nonnegative bounded integer"))?;
    ensure!(value <= max, "{name} exceeds the supported range");
    Ok(value)
}

fn optional_text<'a>(args: &'a Map<String, Value>, key: &str, limit: usize) -> Result<Option<&'a str>> {
    match args.get(key) {
        None => Ok(None),
        Some(value) => {
            let value = value.as_str().ok_or_else(|| anyhow!("{key} must be text"))?;
            ensure!(value.len() <= limit, "{key} exceeds its size limit");
            Ok(Some(value))
        }
    }
}

fn flag(args: &Map<String, Value>, key: &str) -> Result<bool> {
    match args.get(key) {
        None => Ok(false),
        Some(value) => value.as_bool().ok_or_else(|| anyhow!("{key} must be a boolean")),
    }
}

fn encode(value: &str) -> String {
    utf8_percent_encode(value, NON_ALPHANUMERIC).to_string()
}

fn token(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128 && value.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b))
}

// Only server output routes for this project are eligible to receive a cookie.
fn output_path(client: &Client, raw: &str, clsi: Option<&str>, group: Option<&str>) -> Result<String> {
    ensure!(
        raw.len() <= 2048 && client.redact(raw) == raw,
        "invalid compile output reference"
    );
    let raw = if raw.starts_with("https://") || raw.starts_with("http://") {
        raw.strip_prefix(&client.origin)
            .filter(|path| path.starts_with('/'))
            .ok_or_else(|| anyhow!("cross-origin compile output is refused"))?
    } else {
        ensure!(!raw.starts_with("//"), "protocol-relative output is refused");
        raw
    };
    ensure!(
        !raw.contains('#') && !raw.contains('\\') && !raw.contains(':'),
        "invalid output route"
    );
    let (path, query) = raw.split_once('?').unwrap_or((raw, ""));
    let path = format!("/{}", path.trim_start_matches('/'));
    let decoded = percent_decode_str(&path)
        .decode_utf8()
        .map_err(|_| anyhow!("invalid output route encoding"))?;
    ensure!(
        decoded == path && !path.split('/').any(|part| matches!(part, "." | "..")),
        "encoded or traversing output routes are refused"
    );
    let prefix = format!("/project/{}/", client.project);
    let rest = path
        .strip_prefix(&prefix)
        .ok_or_else(|| anyhow!("output belongs to another project"))?;
    let filename = if let Some(rest) = rest.strip_prefix("output/") {
        rest
    } else if let Some(rest) = rest.strip_prefix("build/") {
        let (build, rest) = rest
            .split_once('/')
            .ok_or_else(|| anyhow!("invalid build output route"))?;
        ensure!(token(build), "invalid build identifier");
        rest.strip_prefix("output/")
            .ok_or_else(|| anyhow!("invalid build output route"))?
    } else {
        bail!("only project compile output routes are allowed")
    };
    ensure!(
        token(filename) && !matches!(filename, "." | ".."),
        "invalid compile output name"
    );
    let mut routing = Map::new();
    if !query.is_empty() {
        for pair in query.split('&') {
            let (name, value) = pair.split_once('=').ok_or_else(|| anyhow!("invalid output routing"))?;
            ensure!(
                matches!(name, "clsiserverid" | "compileGroup") && token(value),
                "unsupported output query"
            );
            ensure!(
                routing.insert(name.into(), json!(value)).is_none(),
                "duplicate output routing"
            );
        }
    }
    for (key, value) in [("clsiserverid", clsi), ("compileGroup", group)] {
        if let Some(value) = value {
            ensure!(
                token(value) && client.redact(value) == value,
                "invalid compile routing identifier"
            );
            if let Some(old) = routing.get(key) {
                ensure!(old == value, "conflicting compile routing identifiers");
            }
            routing.insert(key.into(), json!(value));
        }
    }
    if routing.is_empty() {
        Ok(path)
    } else {
        let query = routing
            .iter()
            .map(|(key, value)| format!("{key}={}", value.as_str().unwrap()))
            .collect::<Vec<_>>()
            .join("&");
        Ok(format!("{path}?{query}"))
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Output {
    path: String,
    url: String,
    #[serde(default)]
    build: Option<String>,
    #[serde(default)]
    sha256: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Compile {
    schema: u32,
    scope: String,
    operation_id: String,
    acknowledged: bool,
    status: String,
    started_ms: u64,
    output_files: Vec<Output>,
    timings: Value,
}

fn directory(path: &Path) -> Result<()> {
    if !path.exists() {
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(path) {
            Ok(()) => File::open(path.parent().ok_or_else(|| anyhow!("missing cloud record parent"))?)?.sync_all()?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    check_receipt_dir(path)
}

fn scope(client: &Client) -> Result<String> {
    Ok(hash(&serde_json::to_vec(&json!([
        client.origin,
        client.account,
        client.project
    ]))?))
}

fn record_root(client: &Client, session: &NativeSession) -> Result<PathBuf> {
    directory(&session.receipts)?;
    let cloud = session.receipts.join("cloud");
    directory(&cloud)?;
    let root = cloud.join(scope(client)?);
    directory(&root)?;
    Ok(root)
}

fn write_private(root: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    ensure!(bytes.len() as u64 <= RECORD_LIMIT, "compile metadata exceeds its bound");
    check_receipt_dir(root)?;
    let mut file = tempfile::NamedTempFile::new_in(root)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(root.join(name))
        .map_err(|_| anyhow!("cannot persist compile metadata; do not repeat the compile"))?;
    File::open(root)?.sync_all()?;
    Ok(())
}

fn read_private(root: &Path, name: &str) -> Result<Vec<u8>> {
    check_receipt_dir(root)?;
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(nix::libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(root.join(name))
        .map_err(|_| anyhow!("retained compile not found; run a new explicitly requested compile"))?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= RECORD_LIMIT,
        "invalid retained compile metadata"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "compile metadata must be private"
        );
    }
    let mut bytes = Vec::new();
    (&mut file).take(RECORD_LIMIT + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= RECORD_LIMIT, "compile metadata exceeds its bound");
    Ok(bytes)
}

fn retain(root: &Path, record: &Compile) -> Result<String> {
    let bytes = serde_json::to_vec(record)?;
    write_private(root, &format!("{}.json", record.operation_id), &bytes)?;
    Ok(hash(&bytes))
}

fn load(client: &Client, session: &NativeSession, args: &Map<String, Value>) -> Result<Compile> {
    let root = record_root(client, session)?;
    let operation = if let Some(operation) = optional_text(args, "operation_id", 36)? {
        operation.to_owned()
    } else {
        String::from_utf8(read_private(&root, "latest")?).map_err(|_| anyhow!("invalid latest compile reference"))?
    };
    uuid(&operation)?;
    let record: Compile = serde_json::from_slice(&read_private(&root, &format!("{operation}.json"))?)
        .map_err(|_| anyhow!("invalid retained compile metadata"))?;
    ensure!(
        record.schema == 1 && record.scope == scope(client)? && record.operation_id == operation,
        "compile scope does not match this account/project"
    );
    ensure!(
        record.acknowledged,
        "last compile outcome is uncertain; no older output will be substituted"
    );
    ensure!(record.output_files.len() <= 128, "invalid retained output inventory");
    for file in &record.output_files {
        ensure!(token(&file.path), "invalid retained output name");
        output_path(client, &file.url, None, None)?;
        ensure!(
            file.url.split('?').next().and_then(|path| path.rsplit('/').next()) == Some(file.path.as_str()),
            "output name and route disagree"
        );
        ensure!(
            file.sha256
                .as_ref()
                .is_none_or(|hash| hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit())),
            "invalid artifact hash"
        );
    }
    Ok(record)
}

fn response_record(client: &Client, operation: &str, started_ms: u64, body: &Value) -> Result<Compile> {
    let status = body["status"]
        .as_str()
        .ok_or_else(|| anyhow!("compile response has no status"))?;
    ensure!(
        token(status) && client.redact(status) == status,
        "invalid compile response status"
    );
    let routing = |key: &str| -> Result<Option<&str>> {
        body.get(key)
            .map(|value| value.as_str().ok_or_else(|| anyhow!("invalid compile routing")))
            .transpose()
    };
    let clsi = routing("clsiServerId")?;
    let group = routing("compileGroup")?;
    let rows = match body.get("outputFiles") {
        None => &[][..],
        Some(value) => value
            .as_array()
            .ok_or_else(|| anyhow!("invalid compile output inventory"))?
            .as_slice(),
    };
    ensure!(rows.len() <= 128, "compile output inventory exceeds its bound");
    let mut names = BTreeSet::new();
    let mut files = Vec::new();
    for row in rows {
        let name = row["path"]
            .as_str()
            .ok_or_else(|| anyhow!("compile output has no name"))?;
        ensure!(
            token(name) && names.insert(name),
            "invalid or duplicate compile output name"
        );
        let url = output_path(
            client,
            row["url"]
                .as_str()
                .ok_or_else(|| anyhow!("compile output has no URL"))?,
            clsi,
            group,
        )?;
        let build = row
            .get("build")
            .map(|value| {
                value
                    .as_str()
                    .filter(|s| token(s))
                    .map(str::to_owned)
                    .ok_or_else(|| anyhow!("invalid output build ID"))
            })
            .transpose()?;
        ensure!(
            client.redact(name) == name && build.as_ref().is_none_or(|s| client.redact(s) == *s),
            "output metadata contains credentials"
        );
        ensure!(
            url.split('?').next().and_then(|path| path.rsplit('/').next()) == Some(name),
            "output name and route disagree"
        );
        files.push(Output {
            path: name.into(),
            url,
            build,
            sha256: None,
        });
    }
    let artifact_builds = files
        .iter()
        .filter(|file| matches!(file.path.as_str(), "output.pdf" | "output.log" | "output.aux"))
        .filter_map(output_build)
        .collect::<BTreeSet<_>>();
    ensure!(
        artifact_builds.len() <= 1,
        "compile artifacts refer to different builds"
    );
    let timings = body.get("timings").cloned().unwrap_or_else(|| json!({}));
    let timings_map = timings.as_object().ok_or_else(|| anyhow!("invalid compile timings"))?;
    ensure!(
        timings_map.len() <= 32
            && timings_map
                .iter()
                .all(|(key, value)| token(key) && value.as_f64().is_some_and(|v| v.is_finite() && v >= 0.0)),
        "invalid compile timings"
    );
    Ok(Compile {
        schema: 1,
        scope: scope(client)?,
        operation_id: operation.into(),
        acknowledged: true,
        status: status.into(),
        started_ms,
        output_files: files,
        timings,
    })
}

fn fetch_unpinned(client: &mut Client, record: &Compile, name: &str) -> Result<String> {
    let file = record
        .output_files
        .iter()
        .find(|file| file.path == name)
        .ok_or_else(|| anyhow!("this compile produced no {name}; older build output is not used"))?;
    let path = output_path(client, &file.url, None, None)?;
    let bytes = client.bytes("GET", &path, None, None)?;
    ensure!(
        client
            .last_content_type
            .as_deref()
            .is_some_and(|kind| kind == "application/octet-stream"
                || (kind.starts_with("text/") && !matches!(kind, "text/html" | "text/xml"))
                || matches!(kind, "application/x-tex" | "application/x-latex" | "application/x-log")),
        "compile output is not a text artifact"
    );
    ensure!(
        bytes.len() <= 4 * 1024 * 1024,
        "compile output exceeds the 4 MiB bound; no partial log is reported as complete"
    );
    String::from_utf8(bytes).map_err(|_| anyhow!("compile output is not valid UTF-8"))
}

fn fetch(client: &mut Client, record: &Compile, name: &str) -> Result<String> {
    let file = record
        .output_files
        .iter()
        .find(|file| file.path == name)
        .ok_or_else(|| anyhow!("this compile produced no {name}; older build output is not used"))?;
    ensure!(
        file.sha256.is_some() || file.url.starts_with(&format!("/project/{}/build/", client.project)),
        "unversioned output was not captured for this compile; no potentially replaced output is returned"
    );
    let text = fetch_unpinned(client, record, name)?;
    if let Some(expected) = &file.sha256 {
        ensure!(
            hash(text.as_bytes()) == *expected,
            "server artifact changed after this compile; retained output is stale"
        );
    }
    Ok(text)
}

fn log_summary(log: &str, cap: usize) -> Value {
    let mut errors = Vec::new();
    let mut count = 0;
    let mut warnings = 0;
    for line in log.lines() {
        if line.starts_with('!')
            || line.contains("LaTeX Error:")
            || crate::text::re!(r"^.+:[0-9]+: (?:Undefined control sequence|Missing |Extra |Runaway argument|Emergency stop|TeX capacity exceeded|Package .+ Error:)").is_match(line)
            || line.contains("Fatal error occurred")
            || line.contains("Emergency stop")
        {
            count += 1;
            if errors.len() < cap {
                errors.push(crate::text::head(line.trim(), 2048));
            }
        } else if line.to_ascii_lowercase().contains("warning") {
            warnings += 1;
        }
    }
    let compact = log.replace(['\r', '\n'], "");
    let pages = crate::text::re!(r"Output written on [^(]*\((\d+) pages?")
        .captures(&compact)
        .and_then(|captures| captures[1].parse::<u64>().ok());
    json!({"error_count":count,"warning_count":warnings,"first_errors":errors,"page_count":pages,"log_bytes":log.len(),"log_complete":true})
}

fn output_build(file: &Output) -> Option<&str> {
    file.url
        .split_once("/build/")
        .and_then(|(_, rest)| rest.split_once('/').map(|(build, _)| build))
}

fn summary(record: &Compile, log: Option<&str>) -> Value {
    let build_for = |name: &str| {
        record
            .output_files
            .iter()
            .find(|file| file.path == name)
            .and_then(output_build)
    };
    let artifact_build_verified = build_for("output.pdf")
        .zip(build_for("output.log"))
        .is_some_and(|(pdf, log)| pdf == log);
    let pdf = record.output_files.iter().any(|file| file.path == "output.pdf");
    let mut result = log.map(|log| log_summary(log,5)).unwrap_or_else(|| json!({
        "error_count":null,"warning_count":null,"first_errors":[],"page_count":null,"log_bytes":null,"log_complete":false
    }));
    let clean = record.status == "success"
        && artifact_build_verified
        && pdf
        && result["page_count"].as_u64().is_some_and(|pages| pages > 0)
        && result["error_count"] == 0;
    result.as_object_mut().unwrap().extend(json!({
        "operation_id":record.operation_id,"status":record.status,
        "clean":clean,
        "built_cleanly":clean,
        "pdf_available":pdf,"output_files":record.output_files.iter().map(|file|file.path.as_str()).collect::<Vec<_>>(),
        "build_ids":record.output_files.iter().filter_map(output_build).collect::<BTreeSet<_>>(),
        "artifact_build_verified":artifact_build_verified,
        "timings":record.timings,"compile_time_ms":record.timings["compile"],"total_time_ms":record.timings["compileE2E"],
        "started_ms":record.started_ms,"current_source_verified":false,"freshness":FRESHNESS
    }).as_object().unwrap().clone());
    result
}

fn compile(client: &mut Client, session: &NativeSession, args: &Map<String, Value>) -> Result<Value> {
    let operation = need(args, "operation_id")?;
    uuid(operation)?;
    let root_doc = optional_text(args, "root_doc", 1024)?;
    let path = optional_text(args, "path", 1024)?;
    ensure!(
        root_doc.is_none() || path.is_none() || root_doc == path,
        "root_doc and path disagree"
    );
    let root_doc = root_doc.or(path);
    if let Some(path) = root_doc {
        edits::path(path)?;
    }
    let mut body = json!({"check":"silent","draft":flag(args,"draft")?,"incrementalCompilesEnabled":true,"stopOnFirstError":flag(args,"stop_on_first_error")?});
    if let Some(path) = root_doc {
        body["rootResourcePath"] = json!(path);
    }
    let root = record_root(client, session)?;
    once(
        client,
        session,
        "native_compile",
        args,
        json!({"compile_record_id":operation}),
        |client, receipt| {
            let started_ms = u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())?;
            let mut record = Compile {
                schema: 1,
                scope: scope(client)?,
                operation_id: operation.into(),
                acknowledged: false,
                status: "uncertain".into(),
                started_ms,
                output_files: Vec::new(),
                timings: json!({}),
            };
            retain(&root, &record)?;
            // Point at intent before POST: a failed/lost new compile cannot expose an older successful output.
            write_private(&root, "latest", operation.as_bytes())?;
            let response = client.json(
                "POST",
                &format!("/project/{}/compile?auto_compile=true", client.project),
                Some(body),
            )?;
            record = response_record(client, operation, started_ms, &response)?;
            // Pin mutable CE output routes as well as build-specific cloud routes.
            let log = fetch_unpinned(client, &record, "output.log");
            let aux = fetch_unpinned(client, &record, "output.aux");
            for (name, text) in [("output.log", log.as_ref().ok()), ("output.aux", aux.as_ref().ok())] {
                if let Some(text) = text {
                    record
                        .output_files
                        .iter_mut()
                        .find(|file| file.path == name)
                        .unwrap()
                        .sha256 = Some(hash(text.as_bytes()));
                }
            }
            let digest = retain(&root, &record)?;
            receipt.details["compile_record_sha256"] = json!(digest);
            receipt.verified = true;
            let mut result = summary(&record, log.as_deref().ok());
            result["project_id"] = json!(client.project);
            if let Err(error) = log {
                result["log_error"] = json!(client.redact(&error.to_string()));
            }
            if let Err(error) = aux {
                result["aux_error"] = json!(client.redact(&error.to_string()));
            }
            Ok(result)
        },
    )
}

fn read_log(client: &mut Client, session: &NativeSession, args: &Map<String, Value>) -> Result<Value> {
    let record = load(client, session, args)?;
    let log = fetch(client, &record, "output.log")?;
    let mut result = summary(&record, Some(&log));
    result
        .as_object_mut()
        .unwrap()
        .extend(log_summary(&log, 200).as_object().unwrap().clone());
    result["project_id"] = json!(client.project);
    result["log"] = json!(log);
    Ok(result)
}

fn group(input: &str) -> Option<(&str, &str)> {
    let mut level = 0usize;
    for (index, ch) in input.char_indices() {
        match ch {
            '{' => level += 1,
            '}' => {
                level = level.checked_sub(1)?;
                if level == 0 {
                    return Some((&input[1..index], &input[index + 1..]));
                }
            }
            _ if index == 0 => return None,
            _ => {}
        }
    }
    None
}

fn labels(aux: &str, prefix: &str) -> Result<Vec<Value>> {
    let mut rows = Vec::new();
    for entry in aux.split("\\newlabel").skip(1) {
        let Some((label, rest)) = group(entry) else { continue };
        let Some((fields, _)) = group(rest) else { continue };
        let Some((number, rest)) = group(fields) else { continue };
        let Some((page, rest)) = group(rest) else { continue };
        if label.starts_with(prefix) {
            ensure!(rows.len() < 10000, "label inventory exceeds 10000 entries");
            rows.push(json!({"label":label,"number":number,"page":page,"title":group(rest).map(|(title,_)|title).filter(|title|!title.is_empty())}));
        }
    }
    Ok(rows)
}

fn label_pages(client: &mut Client, session: &NativeSession, args: &Map<String, Value>) -> Result<Value> {
    let prefix = optional_text(args, "label_prefix", 1024)?.unwrap_or("");
    let record = load(client, session, args)?;
    let aux = fetch(client, &record, "output.aux")?;
    let rows = labels(&aux, prefix)?;
    let log = fetch(client, &record, "output.log");
    let mut result = summary(&record, log.as_deref().ok());
    result["project_id"] = json!(client.project);
    result["labels"] = json!(rows);
    result["count"] = json!(rows.len());
    result["aux_complete"] = json!(true);
    if let Err(error) = log {
        result["log_error"] = json!(client.redact(&error.to_string()));
    }
    Ok(result)
}

fn author(user: &Value) -> String {
    let names = ["first_name", "last_name"]
        .iter()
        .filter_map(|key| user[key].as_str())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if !names.is_empty() {
        names
    } else {
        ["email", "id", "_id"]
            .iter()
            .find_map(|key| user[key].as_str())
            .unwrap_or("unknown")
            .into()
    }
}

fn authors(users: &Value) -> Vec<String> {
    users
        .as_array()
        .map(|users| users.iter().map(author).collect())
        .unwrap_or_default()
}

fn history(client: &mut Client, args: &Map<String, Value>) -> Result<Value> {
    let limit = integer(args, "limit", Some(20), 100)?;
    ensure!(limit >= 1, "history limit must be at least one");
    let mut path = format!("/project/{}/updates?min_count={limit}", client.project);
    if args.contains_key("before") {
        path.push_str(&format!("&before={}", integer(args, "before", None, MAX_VERSION)?));
    }
    let needle = optional_text(args, "path_contains", 1024)?.unwrap_or("").to_lowercase();
    let response = client.json("GET", &path, None)?;
    let updates = response["updates"]
        .as_array()
        .ok_or_else(|| anyhow!("incomplete native history response"))?;
    ensure!(updates.len() <= 1000, "history response exceeds 1000 entries");
    let mut entries = Vec::new();
    for update in updates {
        let from = update["fromV"]
            .as_u64()
            .filter(|v| *v <= MAX_VERSION)
            .ok_or_else(|| anyhow!("invalid history from version"))?;
        let to = update["toV"]
            .as_u64()
            .filter(|v| *v <= MAX_VERSION)
            .ok_or_else(|| anyhow!("invalid history to version"))?;
        ensure!(from <= to, "reversed native history version range");
        let paths = update.get("pathnames").cloned().unwrap_or_else(|| json!([]));
        let ops = update.get("project_ops").cloned().unwrap_or_else(|| json!([]));
        let path_rows = paths
            .as_array()
            .ok_or_else(|| anyhow!("invalid native history paths"))?;
        let op_rows = ops
            .as_array()
            .ok_or_else(|| anyhow!("invalid native history operations"))?;
        let matches = needle.is_empty()
            || path_rows
                .iter()
                .filter_map(Value::as_str)
                .any(|p| p.to_lowercase().contains(&needle))
            || op_rows.iter().any(|op| {
                ["add", "rename", "remove"].iter().any(|kind| {
                    ["pathname", "newPathname"].iter().any(|key| {
                        op[kind][key]
                            .as_str()
                            .is_some_and(|p| p.to_lowercase().contains(&needle))
                    })
                })
            });
        if !matches {
            continue;
        }
        // min_count may return a whole batch. Preserve it before advancing the server cursor.
        entries.push(json!({"from_version":from,"to_version":to,"time_ms":update["meta"]["end_ts"],"start_time_ms":update["meta"]["start_ts"],"authors":authors(&update["meta"]["users"]),"files":paths,"file_ops":ops,"labels":update.get("labels").cloned().unwrap_or_else(||json!([])),"origin":update["meta"]["origin"],"native_record":update}));
    }
    let next = response.get("nextBeforeTimestamp").cloned().unwrap_or(Value::Null);
    ensure!(
        next.is_null() || next.as_u64().is_some_and(|v| v <= MAX_VERSION),
        "invalid native history cursor"
    );
    Ok(
        json!({"project_id":client.project,"count":entries.len(),"entries":entries,"next_before":next,"page_complete":true,"requested_limit":limit,"entries_exceed_requested_limit":entries.len()>limit as usize,"server_records_received":updates.len(),"source":"overleaf-native-history"}),
    )
}

fn diff(client: &mut Client, args: &Map<String, Value>) -> Result<Value> {
    let path = need(args, "path")?;
    edits::path(path)?;
    let from = integer(args, "from", None, MAX_VERSION)?;
    let to = integer(args, "to", None, MAX_VERSION)?;
    ensure!(from <= to, "from must not exceed to");
    let context = integer(args, "context_lines", Some(2), 20)?;
    let response = client.json(
        "GET",
        &format!(
            "/project/{}/diff?from={from}&to={to}&pathname={}",
            client.project,
            encode(path)
        ),
        None,
    )?;
    let chunks = &response["diff"];
    if chunks["binary"] == true {
        return Ok(
            json!({"project_id":client.project,"path":path,"from":from,"to":to,"binary":true,"source":"overleaf-native-history"}),
        );
    }
    let chunks = chunks
        .as_array()
        .ok_or_else(|| anyhow!("invalid native history diff"))?;
    ensure!(chunks.len() <= 100000, "native diff exceeds its chunk bound");
    let mut before = String::new();
    let mut after = String::new();
    let mut inserted = 0usize;
    let mut deleted = 0usize;
    let mut people = BTreeSet::new();
    for chunk in chunks {
        let mut fields = 0;
        for key in ["u", "i", "d"] {
            if let Some(value) = chunk.get(key) {
                let text = value.as_str().ok_or_else(|| anyhow!("invalid native diff text"))?;
                fields += 1;
                match key {
                    "i" => {
                        after.push_str(text);
                        inserted += text.chars().count();
                    }
                    "d" => {
                        before.push_str(text);
                        deleted += text.chars().count();
                    }
                    _ => {
                        before.push_str(text);
                        after.push_str(text);
                    }
                }
            }
        }
        ensure!(
            fields == 1,
            "native diff chunks must contain exactly one unchanged/inserted/deleted text field"
        );
        people.extend(authors(&chunk["meta"]["users"]));
        ensure!(
            before.len() <= 4 * 1024 * 1024 && after.len() <= 4 * 1024 * 1024,
            "native diff text exceeds its bound"
        );
    }
    let rendered = if before == after {
        String::new()
    } else {
        similar::TextDiff::from_lines(&before, &after)
            .unified_diff()
            .context_radius(context as usize)
            .header(&format!("{path}@{from}"), &format!("{path}@{to}"))
            .to_string()
    };
    Ok(
        json!({"project_id":client.project,"path":path,"from":from,"to":to,"binary":false,"inserted_chars":inserted,"deleted_chars":deleted,"authors":people,"diff":rendered,"chunks":chunks,"context_lines":context,"source":"overleaf-native-history"}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader, Read, Write},
        net::TcpListener,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        thread,
    };
    const PROJECT: &str = "111111111111111111111111";
    const ACCOUNT: &str = "222222222222222222222222";
    const OP: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    const OP2: &str = "ffffffff-bbbb-cccc-dddd-eeeeeeeeeeee";
    struct Fixture {
        origin: String,
        posts: Arc<AtomicUsize>,
        stop: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
    }
    impl Fixture {
        fn start(lost: bool, no_log: bool, html_log: bool) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let origin = format!("http://{}", listener.local_addr().unwrap());
            let posts = Arc::new(AtomicUsize::new(0));
            let stop = Arc::new(AtomicBool::new(false));
            let (count, stopping) = (posts.clone(), stop.clone());
            let task = thread::spawn(move || {
                while !stopping.load(Ordering::SeqCst) {
                    let (mut stream, _) = match listener.accept() {
                        Ok(pair) => pair,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(std::time::Duration::from_millis(2));
                            continue;
                        }
                        Err(error) => panic!("{error}"),
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
                    assert!(headers.contains("overleaf_session2=fixture-cookie"));
                    let route = first.split_whitespace().nth(1).unwrap();
                    let (kind, body) = if first.starts_with("POST ") {
                        assert_eq!(route, format!("/project/{PROJECT}/compile?auto_compile=true"));
                        let len = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .and_then(|n| n.trim().parse::<usize>().ok())
                            })
                            .unwrap();
                        let mut body = vec![0; len];
                        reader.read_exact(&mut body).unwrap();
                        let body: Value = serde_json::from_slice(&body).unwrap();
                        assert_eq!(body["check"], "silent");
                        assert_eq!(body["rootResourcePath"], "main.tex");
                        assert_eq!(body["draft"], false);
                        assert_eq!(body["stopOnFirstError"], true);
                        count.fetch_add(1, Ordering::SeqCst);
                        if lost {
                            continue;
                        }
                        let mut files = vec![
                            json!({"path":"output.pdf","url":format!("/project/{PROJECT}/build/build-1/output/output.pdf"),"build":"build-1"}),
                            json!({"path":"output.aux","url":format!("/project/{PROJECT}/build/build-1/output/output.aux"),"build":"build-1"}),
                        ];
                        if !no_log {
                            files.push(json!({"path":"output.log","url":format!("/project/{PROJECT}/build/build-1/output/output.log"),"build":"build-1"}))
                        }
                        ("application/json",json!({"status":"success","clsiServerId":"clsi-1","compileGroup":"standard","outputFiles":files,"timings":{"compile":12,"compileE2E":34}}).to_string())
                    } else if route.contains("/output/output.log") {
                        assert!(route.contains("clsiserverid=clsi-1") && route.contains("compileGroup=standard"));
                        if html_log {
                            ("text/html", "<html><body>Login required</body></html>".into())
                        } else {
                            ("text/plain","! Undefined control sequence.\nLaTeX Warning: unresolved reference\nOutput written on output.pdf (2 pages, 7 bytes).\n".into())
                        }
                    } else if route.contains("/output/output.aux") {
                        assert!(route.contains("clsiserverid=clsi-1") && route.contains("compileGroup=standard"));
                        (
                            "text/plain",
                            r"\newlabel{sec:one}{{1}{2}{Nested {title}}{section.1}{}}\newlabel{tab:one}{{3}{4}}".into(),
                        )
                    } else if route.contains("/updates?") {
                        assert!(route.contains("min_count=1") && route.contains("before=999"));
                        ("application/json",json!({"updates":[{"fromV":3,"toV":7,"meta":{"users":[{"first_name":"Ada","last_name":"Lovelace"}],"start_ts":100,"end_ts":200,"origin":{"kind":"editor"}},"pathnames":["main.tex"],"project_ops":[{"rename":{"pathname":"old.tex","newPathname":"main.tex"},"atV":4}],"labels":[{"comment":"submitted","version":7}]}],"nextBeforeTimestamp":99}).to_string())
                    } else if route.contains("/diff?") {
                        assert!(route.contains("pathname=sections%2Fmain%2Etex"));
                        ("application/json",json!({"diff":[{"u":"a\nb\nc "},{"d":"old","meta":{"users":[{"email":"author@example.test"}]}},{"i":"new"},{"u":"\nd\ne\n"}]}).to_string())
                    } else {
                        assert_eq!(route, format!("/project/{PROJECT}"));
                        (
                            "text/html",
                            format!(
                                "<meta name='ol-project_id' content='{PROJECT}'><meta name='ol-user' content='{{&quot;_id&quot;:&quot;{ACCOUNT}&quot;}}'><meta name='ol-csrfToken' content='fixture-csrf'>"
                            ),
                        )
                    };
                    write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
                }
            });
            Self {
                origin,
                posts,
                stop,
                thread: Some(task),
            }
        }
        fn session(&self, path: &Path) -> NativeSession {
            NativeSession {
                origin: self.origin.clone(),
                cookie: "overleaf_session2=fixture-cookie".into(),
                receipts: path.into(),
                identity: None,
                upload_roots: Vec::new(),
                projects: None,
                operations: None,
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            self.thread.take().unwrap().join().unwrap();
        }
    }

    #[test]
    fn cloud_compile_retains_routed_outputs_without_replay_or_stale_success() {
        for (lost, no_log, html_log) in [
            (false, false, false),
            (true, false, false),
            (false, true, false),
            (false, false, true),
        ] {
            let fixture = Fixture::start(lost, no_log, html_log);
            let dir = tempfile::tempdir().unwrap();
            let session = fixture.session(&dir.path().join("receipts"));
            let mut client = Client::new(&session, PROJECT).unwrap();
            let args = json!({"project_id":PROJECT,"operation_id":OP,"root_doc":"main.tex","stop_on_first_error":true});
            let result = call(&mut client, &session, "native_compile", args.as_object().unwrap()).unwrap();
            assert_eq!(result["verified"], !lost, "{result}");
            if !lost {
                assert_eq!(result["clean"], false, "a partial PDF or missing log must not be clean");
                assert_eq!(result["log_complete"], !no_log && !html_log);
                if !no_log && !html_log {
                    assert_eq!(result["error_count"], 1);
                    assert_eq!(result["warning_count"], 1);
                    assert_eq!(result["page_count"], 2);
                }
            }
            let mut restarted = Client::new(&session, PROJECT).unwrap();
            let again = call(&mut restarted, &session, "native_compile", args.as_object().unwrap()).unwrap();
            assert_eq!(again["verified"], result["verified"]);
            assert_eq!(fixture.posts.load(Ordering::SeqCst), 1, "a stable UUID repeated POST");
            let mut changed = args.clone();
            changed["root_doc"] = json!("other.Rnw");
            assert!(
                call(&mut restarted, &session, "native_compile", changed.as_object().unwrap()).is_err(),
                "changed root reused an operation UUID"
            );
            assert_eq!(fixture.posts.load(Ordering::SeqCst), 1);
            assert_eq!(
                log_summary(
                    "main.tex:42: Undefined control sequence.\nOutput written on output.pdf (1 page).",
                    5
                )["error_count"],
                1
            );
            let reads = json!({"project_id":PROJECT,"operation_id":OP});
            let log = call(&mut restarted, &session, "native_read_log", reads.as_object().unwrap());
            if lost || no_log || html_log {
                assert!(log.is_err());
            } else {
                let log = log.unwrap();
                assert!(log["log"].as_str().unwrap().contains("Undefined control"));
                assert_eq!(log["current_source_verified"], false);
                let labels = call(
                    &mut restarted,
                    &session,
                    "native_label_pages",
                    json!({"project_id":PROJECT,"label_prefix":"sec:"}).as_object().unwrap(),
                )
                .unwrap();
                assert_eq!(labels["labels"][0]["title"], "Nested {title}");
                assert_eq!(labels["count"], 1);
                let root = record_root(&restarted, &session).unwrap();
                let mut captured = load(&restarted, &session, reads.as_object().unwrap()).unwrap();
                let output = captured
                    .output_files
                    .iter_mut()
                    .find(|file| file.path == "output.log")
                    .unwrap();
                let original_hash = output.sha256.replace("0".repeat(64));
                retain(&root, &captured).unwrap();
                assert!(
                    call(&mut restarted, &session, "native_read_log", reads.as_object().unwrap()).is_err(),
                    "replaced server output was returned as the retained compile"
                );
                captured
                    .output_files
                    .iter_mut()
                    .find(|file| file.path == "output.log")
                    .unwrap()
                    .sha256 = original_hash;
                retain(&root, &captured).unwrap();
                let unknown = Compile {
                    schema: 1,
                    scope: scope(&restarted).unwrap(),
                    operation_id: OP2.into(),
                    acknowledged: false,
                    status: "uncertain".into(),
                    started_ms: 2,
                    output_files: vec![],
                    timings: json!({}),
                };
                retain(&root, &unknown).unwrap();
                write_private(&root, "latest", OP2.as_bytes()).unwrap();
                assert!(
                    call(
                        &mut restarted,
                        &session,
                        "native_read_log",
                        json!({"project_id":PROJECT}).as_object().unwrap()
                    )
                    .is_err(),
                    "a newer unknown compile exposed old success"
                );
                assert!(call(&mut restarted, &session, "native_read_log", reads.as_object().unwrap()).is_ok());
            }
            for route in [
                "https://evil.example/project/x/output/output.log",
                "//evil.example/output.log",
                "/project/999999999999999999999999/output/output.log",
                "/project/111111111111111111111111/build/../output/output.log",
                "/project/111111111111111111111111/output/%2e%2e",
                "/project/111111111111111111111111/output/output.log?token=secret",
            ] {
                assert!(output_path(&client, route, None, None).is_err(), "{route}");
            }
            assert_eq!(
                summary(
                    &Compile {
                        schema: 1,
                        scope: String::new(),
                        operation_id: OP.into(),
                        acknowledged: true,
                        status: "failure".into(),
                        started_ms: 0,
                        output_files: vec![Output {
                            path: "output.pdf".into(),
                            url: String::new(),
                            build: None,
                            sha256: None
                        }],
                        timings: json!({})
                    },
                    Some("No errors\n")
                )["clean"],
                false
            );
            let files = fs::read_dir(record_root(&client, &session).unwrap()).unwrap();
            for file in files {
                let text = fs::read_to_string(file.unwrap().path()).unwrap();
                assert!(
                    !text.contains("fixture-cookie")
                        && !text.contains("fixture-csrf")
                        && !text.contains("Undefined control")
                );
            }
        }
    }

    #[test]
    fn history_uses_native_versions_paths_and_contextual_diff() {
        let fixture = Fixture::start(false, false, false);
        let dir = tempfile::tempdir().unwrap();
        let session = fixture.session(&dir.path().join("receipts"));
        let mut client = Client::new(&session, PROJECT).unwrap();
        let history = call(
            &mut client,
            &session,
            "native_list_history",
            json!({"project_id":PROJECT,"limit":1,"before":999,"path_contains":"MAIN"})
                .as_object()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(history["entries"][0]["from_version"], 3);
        assert_eq!(history["entries"][0]["to_version"], 7);
        assert_eq!(history["entries"][0]["authors"][0], "Ada Lovelace");
        assert_eq!(
            history["entries"][0]["file_ops"][0]["rename"]["newPathname"],
            "main.tex"
        );
        assert_eq!(history["next_before"], 99);
        let diff = call(
            &mut client,
            &session,
            "native_diff_file",
            json!({"project_id":PROJECT,"path":"sections/main.tex","from":3,"to":7,"context_lines":1})
                .as_object()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(diff["inserted_chars"], 3);
        assert_eq!(diff["deleted_chars"], 3);
        assert!(diff["diff"].as_str().unwrap().contains("-c old"));
        assert!(diff["diff"].as_str().unwrap().contains("+c new"));
        assert_eq!(diff["authors"][0], "author@example.test");
        assert_eq!(diff["chunks"][1]["d"], "old");
        assert!(
            call(
                &mut client,
                &session,
                "native_diff_file",
                json!({"project_id":PROJECT,"path":"../escape","from":3,"to":7})
                    .as_object()
                    .unwrap()
            )
            .is_err()
        );
        assert_eq!(fixture.posts.load(Ordering::SeqCst), 0);
    }
}
