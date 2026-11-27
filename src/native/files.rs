//! Native account discovery, project trees and bounded file operations.
use super::{Client, NativeSession, Receipt, edits, id, need, once};
use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use regex::{Regex, RegexBuilder};
use serde::Serialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::Read,
    path::{Component, Path},
};

const ENTITIES: usize = 4096;
const UPLOAD_LIMIT: usize = 2 * 1024 * 1024;
const CAPTURE_LIMIT: usize = 16 * 1024 * 1024;
const SCAN_DOCS: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct Entity {
    kind: &'static str,
    id: String,
    path: String,
    name: String,
    parent_folder_id: String,
}
struct Tree {
    root: String,
    entities: Vec<Entity>,
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn object_hash(value: &impl Serialize) -> Result<String> {
    Ok(hash(&serde_json::to_vec(value)?))
}
fn tree(project: &Value) -> Result<Tree> {
    fn walk(
        folder: &Value,
        prefix: &str,
        depth: usize,
        entries: &mut Vec<Entity>,
        ids: &mut BTreeSet<String>,
        paths: &mut BTreeSet<String>,
    ) -> Result<()> {
        ensure!(depth < 64, "native folder nesting exceeds 64 levels");
        let parent = need(
            folder.as_object().ok_or_else(|| anyhow!("invalid native folder"))?,
            "_id",
        )?;
        id(parent)?;
        for (key, kind) in [("docs", "doc"), ("fileRefs", "file"), ("folders", "folder")] {
            let children = folder[key]
                .as_array()
                .ok_or_else(|| anyhow!("incomplete native tree"))?;
            for child in children {
                ensure!(entries.len() < ENTITIES, "native project exceeds 4096 entities");
                let map = child.as_object().ok_or_else(|| anyhow!("invalid native entity"))?;
                let name = need(map, "name")?;
                edits::path(name)?;
                ensure!(!name.contains('/'), "native entity name contains a path separator");
                let path = format!("{prefix}{name}");
                edits::path(&path)?;
                let entity_id = need(map, "_id")?;
                id(entity_id)?;
                ensure!(
                    ids.insert(entity_id.into()) && paths.insert(path.clone()),
                    "ambiguous native entity path or identity"
                );
                entries.push(Entity {
                    kind,
                    id: entity_id.into(),
                    path: path.clone(),
                    name: name.into(),
                    parent_folder_id: parent.into(),
                });
                if kind == "folder" {
                    walk(child, &format!("{path}/"), depth + 1, entries, ids, paths)?;
                }
            }
        }
        Ok(())
    }
    let roots = project["rootFolder"]
        .as_array()
        .ok_or_else(|| anyhow!("native root folder missing"))?;
    ensure!(roots.len() == 1, "ambiguous native root folder");
    let root = roots[0]["_id"]
        .as_str()
        .ok_or_else(|| anyhow!("native root folder ID missing"))?;
    id(root)?;
    let mut out = Vec::new();
    walk(
        &roots[0],
        "",
        0,
        &mut out,
        &mut BTreeSet::from([root.into()]),
        &mut BTreeSet::new(),
    )?;
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(Tree {
        root: root.into(),
        entities: out,
    })
}
impl Tree {
    fn at(&self, path: &str) -> Option<&Entity> {
        self.entities.iter().find(|row| row.path == path)
    }
    fn folder(&self, path: &str) -> Result<String> {
        if path.is_empty() {
            return Ok(self.root.clone());
        }
        self.at(path)
            .filter(|e| e.kind == "folder")
            .map(|e| e.id.clone())
            .ok_or_else(|| anyhow!("native destination folder missing"))
    }
    fn scope(&self, path: &str) -> Vec<&Entity> {
        let prefix = format!("{path}/");
        self.entities
            .iter()
            .filter(|e| e.path == path || e.path.starts_with(&prefix))
            .collect()
    }
    fn digest(&self, path: Option<&str>) -> Result<String> {
        match path {
            Some(path) => object_hash(&self.scope(path)),
            None => object_hash(&json!({"root":self.root,"entities":self.entities})),
        }
    }
}
fn split(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}
fn bounded_string<'a>(args: &'a Map<String, Value>, key: &str, max: usize) -> Result<Option<&'a str>> {
    args.get(key)
        .map(|value| {
            let value = value.as_str().ok_or_else(|| anyhow!("{key} must be a string"))?;
            ensure!(value.len() <= max, "{key} exceeds its size limit");
            Ok(value)
        })
        .transpose()
}
fn flag(args: &Map<String, Value>, key: &str, default: bool) -> Result<bool> {
    args.get(key)
        .map(|value| value.as_bool().ok_or_else(|| anyhow!("{key} must be boolean")))
        .transpose()
        .map(|value| value.unwrap_or(default))
}
fn number(args: &Map<String, Value>, key: &str, default: u64, max: u64) -> Result<usize> {
    let n = args
        .get(key)
        .map(|v| v.as_u64().ok_or_else(|| anyhow!("{key} must be an integer")))
        .transpose()?
        .unwrap_or(default);
    ensure!((1..=max).contains(&n), "{key} is out of bounds");
    Ok(n as usize)
}
fn current(client: &mut Client, writable: bool) -> Result<(Value, Tree)> {
    let socket = client.socket()?;
    if writable {
        ensure!(
            matches!(socket.permissions.as_str(), "owner" | "readAndWrite"),
            "native project is not writable"
        );
    }
    let parsed = tree(&socket.project)?;
    Ok((socket.project.clone(), parsed))
}
fn require_same_tree(client: &mut Client, before: &Tree) -> Result<Tree> {
    let (_, after) = current(client, false)?;
    ensure!(
        before.digest(None)? == after.digest(None)?,
        "native project tree changed; no mutation submitted"
    );
    Ok(after)
}
fn mime(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "webp" => "image/webp",
        "tif" | "tiff" => "image/tiff",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}
fn binary(client: &mut Client, entity: &Entity) -> Result<Vec<u8>> {
    client.bytes(
        "GET",
        &format!("/project/{}/file/{}", client.project, entity.id),
        None,
        None,
    )
}
fn file_content(client: &mut Client, entity: &Entity, args: &Map<String, Value>) -> Result<Value> {
    if entity.kind == "doc" {
        let mut socket = client.socket()?;
        ensure!(
            tree(&socket.project)?.at(&entity.path) == Some(entity),
            "native document identity changed"
        );
        let snap = socket.join(&entity.id)?;
        let text_hash = hash(snap.text.as_bytes());
        ensure!(
            args.get("offset").and_then(Value::as_u64).unwrap_or(0) == 0,
            "byte paging applies only to binary files"
        );
        if args.contains_key("expected_sha256") {
            expected_hash(args, "expected_sha256", &text_hash)?;
        }
        let ranges = snap.ranges.clone();
        let changes = range_list(&ranges, "changes")?;
        let comments = range_list(&ranges, "comments")?;
        ensure!(!socket.concurrent, "native document changed while reading; read again");
        return Ok(json!({
            "kind":"doc","doc_id":entity.id,"entity_id":entity.id,"path":entity.path,
            "text":snap.text,"version":snap.version,"sha256":text_hash,
            "byte_count":snap.text.len(),"line_count":snap.text.split('\n').count(),
            "has_ranges":!ranges.is_null(),"tracked_change_count":changes.len(),
            "comment_count":comments.len(),"comments":comments,"recent_changes":changes.iter().rev().take(5).collect::<Vec<_>>(),
            "ranges":ranges,"review_snapshot":"native joinDoc text/version/ranges",
            "complete":true,"truncated":false
        }));
    }
    ensure!(entity.kind == "file", "a folder cannot be read as a file");
    let bytes = binary(client, entity)?;
    let sha = hash(&bytes);
    if args.contains_key("expected_sha256") {
        expected_hash(args, "expected_sha256", &sha)?;
    }
    let offset = args
        .get("offset")
        .map(|v| v.as_u64().ok_or_else(|| anyhow!("invalid binary offset")))
        .transpose()?
        .unwrap_or(0) as usize;
    ensure!(offset <= bytes.len(), "binary offset exceeds file size");
    let limit = number(args, "max_bytes", 1024 * 1024, 1024 * 1024)?;
    let end = offset.saturating_add(limit).min(bytes.len());
    let complete = offset == 0 && end == bytes.len();
    // MIME comes from the file name, never a server-provided executable content hint.
    Ok(json!({
        "kind":"file","file_id":entity.id,"entity_id":entity.id,"path":entity.path,
        "base64":STANDARD.encode(&bytes[offset..end]),"encoding":"base64","mime_type":mime(&entity.name),
        "sha256":sha,"byte_count":bytes.len(),"offset":offset,"returned_bytes":end-offset,
        "next_offset":if end < bytes.len() {Some(end)} else {None},
        "complete":complete,"truncated":!complete
    }))
}
fn range_list(value: &Value, key: &str) -> Result<Vec<Value>> {
    value
        .get(key)
        .map(|v| {
            v.as_array()
                .cloned()
                .ok_or_else(|| anyhow!("invalid native review ranges"))
        })
        .transpose()
        .map(|v| v.unwrap_or_default())
}
fn content_stamp(client: &mut Client, entity: &Entity) -> Result<Value> {
    if entity.kind == "doc" {
        let mut socket = client.socket()?;
        ensure!(
            tree(&socket.project)?.at(&entity.path) == Some(entity),
            "native document identity changed"
        );
        let snap = socket.join(&entity.id)?;
        return Ok(
            json!({"entity_id":entity.id,"sha256":hash(snap.text.as_bytes()),"version":snap.version,"byte_count":snap.text.len()}),
        );
    }
    ensure!(entity.kind == "file", "content hash requires a file or document");
    let bytes = binary(client, entity)?;
    Ok(json!({"entity_id":entity.id,"sha256":hash(&bytes),"byte_count":bytes.len()}))
}
fn manifest(client: &mut Client, tree: &Tree, path: Option<&str>) -> Result<BTreeMap<String, Value>> {
    let mut out = BTreeMap::new();
    let mut total = 0u64;
    for entity in &tree.entities {
        if path.is_some_and(|p| entity.path != p && !entity.path.starts_with(&format!("{p}/"))) {
            continue;
        }
        let mut row = serde_json::to_value(entity)?;
        if entity.kind != "folder" {
            let stamp = content_stamp(client, entity)?;
            total += stamp["byte_count"].as_u64().unwrap_or(0);
            ensure!(total <= CAPTURE_LIMIT as u64, "native content manifest exceeds 16 MiB");
            row.as_object_mut().unwrap().extend(stamp.as_object().unwrap().clone());
        }
        out.insert(entity.path.clone(), row);
    }
    require_same_tree(client, tree)?;
    Ok(out)
}
fn subtree_digest(manifest: &BTreeMap<String, Value>, path: &str) -> Result<String> {
    let prefix = format!("{path}/");
    object_hash(
        &manifest
            .iter()
            .filter(|(p, _)| p.as_str() == path || p.starts_with(&prefix))
            .collect::<BTreeMap<_, _>>(),
    )
}
fn expected_hash(args: &Map<String, Value>, key: &str, actual: &str) -> Result<()> {
    let expected = need(args, key)?;
    ensure!(
        expected.len() == 64
            && expected
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "{key} must be a lowercase SHA-256 hash from a fresh native read"
    );
    ensure!(
        expected == actual,
        "native {key} precondition changed; no mutation submitted"
    );
    Ok(())
}
fn precondition(
    client: &mut Client,
    args: &Map<String, Value>,
    tree: &Tree,
    entity: &Entity,
    deleting: bool,
) -> Result<()> {
    let expected_id = need(args, "expected_entity_id")?;
    id(expected_id)?;
    ensure!(
        entity.id == expected_id,
        "native entity identity changed; no mutation submitted"
    );
    if entity.kind == "folder" {
        expected_hash(args, "expected_tree_sha256", &tree.digest(Some(&entity.path))?)?;
        if deleting {
            let snapshot = manifest(client, tree, Some(&entity.path))?;
            expected_hash(
                args,
                "expected_subtree_sha256",
                &subtree_digest(&snapshot, &entity.path)?,
            )?;
        }
    } else {
        let stamp = content_stamp(client, entity)?;
        expected_hash(args, "expected_sha256", stamp["sha256"].as_str().unwrap())?;
        if entity.kind == "doc" {
            ensure!(
                args.get("expected_version").and_then(Value::as_u64) == stamp["version"].as_u64(),
                "fresh expected_version is required and must match before structural mutation"
            );
        }
    }
    require_same_tree(client, tree)?;
    Ok(())
}
fn parents(client: &mut Client, path: &str, create: bool, receipt: &mut Receipt) -> Result<String> {
    if path.is_empty() {
        return Ok(current(client, true)?.1.root);
    }
    let mut prefix = String::new();
    for part in path.split('/') {
        let parent_path = prefix.clone();
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(part);
        let (_, before) = current(client, true)?;
        if let Some(entity) = before.at(&prefix) {
            ensure!(entity.kind == "folder", "native parent path is occupied by a file");
            continue;
        }
        ensure!(create, "native parent folder missing; enable create_parents");
        let parent = before.folder(&parent_path)?;
        let response = client.json(
            "POST",
            &format!("/project/{}/folder", client.project),
            Some(json!({"name":part,"parent_folder_id":parent})),
        )?;
        let created = response["_id"]
            .as_str()
            .ok_or_else(|| anyhow!("native folder response has no ID"))?;
        id(created)?;
        receipt.details["created_ids"]
            .as_array_mut()
            .unwrap()
            .push(json!(created));
        let (_, after) = current(client, true)?;
        let folder = after
            .at(&prefix)
            .ok_or_else(|| anyhow!("created native folder absent on readback"))?;
        ensure!(
            folder.id == created && folder.kind == "folder" && folder.parent_folder_id == parent,
            "native folder readback mismatch"
        );
        ensure!(
            after.entities.len() == before.entities.len() + 1 && preserves(&before, &after, &BTreeSet::new()),
            "unrelated native tree changed during folder creation"
        );
    }
    current(client, true)?.1.folder(path)
}
fn preserves(before: &Tree, after: &Tree, excluded: &BTreeSet<String>) -> bool {
    before
        .entities
        .iter()
        .filter(|e| !excluded.contains(&e.id))
        .all(|e| after.entities.iter().any(|new| new == e))
}
fn postcheck(before: &Tree, after: &Tree, changed: &BTreeSet<String>) -> Result<()> {
    ensure!(
        preserves(before, after, changed),
        "native readback changed unrelated entities"
    );
    Ok(())
}
fn input_bytes(session: &NativeSession, args: &Map<String, Value>) -> Result<Vec<u8>> {
    let local = bounded_string(args, "local_path", 4096)?;
    let encoded = bounded_string(args, "content_base64", UPLOAD_LIMIT.div_ceil(3) * 4)?;
    ensure!(
        local.is_some() != encoded.is_some(),
        "provide exactly one of local_path or content_base64"
    );
    if let Some(raw) = encoded {
        let bytes = STANDARD.decode(raw).context("invalid strict base64 upload content")?;
        ensure!(bytes.len() <= UPLOAD_LIMIT, "native upload exceeds 2 MiB");
        if args.contains_key("content_sha256") {
            expected_hash(args, "content_sha256", &hash(&bytes))?;
        }
        return Ok(bytes);
    }
    need(args, "content_sha256")?;
    let path = Path::new(local.unwrap());
    ensure!(
        path.is_absolute()
            && !path.components().any(|c| match c {
                Component::Normal(name) => name.to_string_lossy().starts_with('.'),
                Component::CurDir | Component::ParentDir => true,
                _ => false,
            }),
        "local uploads require an absolute path without dot files or dot directories"
    );
    ensure!(
        !session.upload_roots().is_empty(),
        "local uploads are disabled without an assigned upload root"
    );
    let canonical = fs::canonicalize(path).context("cannot resolve local upload")?;
    ensure!(
        !canonical
            .components()
            .any(|c| matches!(c, Component::Normal(name) if name.to_string_lossy().starts_with('.'))),
        "local upload resolves into a dot directory or file"
    );
    let mut allowed = false;
    for root in session.upload_roots() {
        let root = fs::canonicalize(root).context("cannot resolve assigned upload root")?;
        ensure!(root.is_dir(), "assigned upload root is not a directory");
        if canonical.starts_with(&root) && canonical != root {
            allowed = true;
        }
    }
    ensure!(allowed, "local upload escapes assigned upload roots");
    // Reject every symbolic link in the requested path, including same-root links.
    let mut prefix = std::path::PathBuf::new();
    for part in path.components() {
        prefix.push(part);
        ensure!(
            !fs::symlink_metadata(&prefix)?.file_type().is_symlink(),
            "symbolic links are not allowed in local upload paths"
        );
    }
    let mut file = File::open(&canonical).context("cannot open local upload")?;
    let before = file.metadata()?;
    ensure!(
        before.is_file() && before.len() <= UPLOAD_LIMIT as u64,
        "local upload must be a regular file of at most 2 MiB"
    );
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(UPLOAD_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= UPLOAD_LIMIT && bytes.len() as u64 == before.len(),
        "local upload changed or exceeds 2 MiB"
    );
    ensure!(
        fs::canonicalize(path)? == canonical,
        "local upload path changed while reading"
    );
    let after = file.metadata()?;
    ensure!(
        before.len() == after.len() && before.modified()? == after.modified()?,
        "local upload changed while reading"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let current = fs::metadata(&canonical)?;
        ensure!(
            current.dev() == after.dev() && current.ino() == after.ino(),
            "local upload identity changed while reading"
        );
    }
    expected_hash(args, "content_sha256", &hash(&bytes))?;
    Ok(bytes)
}
fn multipart(name: &str, bytes: &[u8], operation: &str) -> Result<(Vec<u8>, String)> {
    ensure!(
        !name.contains(['"', '\\', '\r', '\n', '\0']) && !name.chars().any(char::is_control),
        "unsafe upload destination name"
    );
    let boundary = format!("interleaf-{}-{}", operation.replace('-', ""), &hash(bytes)[..16]);
    ensure!(
        !bytes.windows(boundary.len()).any(|w| w == boundary.as_bytes()),
        "upload boundary appears in content"
    );
    let mut body = format!("--{boundary}\r\nContent-Disposition: form-data; name=\"name\"\r\n\r\n{name}\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"type\"\r\n\r\napplication/octet-stream\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"qqfile\"; filename=\"upload.bin\"\r\nContent-Type: application/octet-stream\r\n\r\n").into_bytes();
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    Ok((body, format!("multipart/form-data; boundary={boundary}")))
}
fn matcher(args: &Map<String, Value>) -> Result<Regex> {
    let query = need(args, "query")?;
    ensure!(query.len() <= 4096, "native search query exceeds 4096 bytes");
    let query = if flag(args, "regex", false)? {
        query.to_owned()
    } else {
        regex::escape(query)
    };
    RegexBuilder::new(&query)
        .case_insensitive(!flag(args, "case_sensitive", false)?)
        .size_limit(1024 * 1024)
        .dfa_size_limit(2 * 1024 * 1024)
        .build()
        .context("invalid native search expression (Rust regex syntax)")
}
fn search_lines(text: &str, regex: &Regex, include_comments: bool, remaining: usize) -> (Vec<Value>, bool) {
    let mut out = Vec::new();
    for (line_no, line) in text.split('\n').enumerate() {
        if !include_comments && line.trim_start().starts_with('%') {
            continue;
        }
        for found in regex.find_iter(line) {
            if out.len() == remaining {
                return (out, true);
            }
            out.push(
                json!({"line":line_no+1,"col":line[..found.start()].encode_utf16().count()+1,
                "text":line.chars().take(300).collect::<String>(),"match":found.as_str()}),
            );
        }
    }
    (out, false)
}
fn list_projects(client: &mut Client, session: &NativeSession, args: &Map<String, Value>) -> Result<Value> {
    let limit = number(args, "limit", 30, 500)?;
    let archived = flag(args, "include_archived", false)?;
    let trashed = flag(args, "include_trashed", false)?;
    let filter = bounded_string(args, "name_contains", 1024)?
        .unwrap_or("")
        .to_lowercase();
    let restricted = session.project_scope().is_some();
    let response = if let Some(allowed) = session.project_scope() {
        ensure!(
            allowed.len() <= 500,
            "restricted native project inventory exceeds 500 allowed projects"
        );
        let mut projects = Vec::new();
        for project_id in allowed {
            id(project_id)?;
            // Read only the explicitly allowed project; never query the account dashboard.
            let mut scoped = Client::new(session, project_id)?;
            ensure!(
                scoped.account == client.account,
                "scoped native project account mismatch"
            );
            let socket = scoped.socket()?;
            let mut project = socket.project.clone();
            project["accessLevel"] = json!(socket.permissions);
            projects.push(project);
        }
        json!({"projects":projects})
    } else {
        match client.json("POST", "/api/project", Some(json!({}))) {
            Ok(value) => value,
            Err(error) if error.to_string().contains("status 404") || error.to_string().contains("status 405") => {
                client.json("GET", "/user/projects", None)?
            }
            Err(error) => return Err(error),
        }
    };
    let rows = response["projects"]
        .as_array()
        .ok_or_else(|| anyhow!("native account project inventory missing"))?;
    ensure!(rows.len() <= 10000, "native account inventory exceeds 10000 projects");
    let mut seen = BTreeSet::new();
    let mut projects = Vec::new();
    for project in rows {
        let project_id = project["id"]
            .as_str()
            .or_else(|| project["_id"].as_str())
            .ok_or_else(|| anyhow!("native account project ID missing"))?;
        id(project_id)?;
        ensure!(seen.insert(project_id), "duplicate native account project identity");
        let name = project["name"].as_str().unwrap_or("(unnamed)");
        ensure!(name.len() <= 1024, "native project name exceeds its size limit");
        if (!archived && project["archived"] == true)
            || (!trashed && project["trashed"] == true)
            || !name.to_lowercase().contains(&filter)
        {
            continue;
        }
        projects.push(json!({
            "id":project_id,"project_id":project_id,"name":name,"lastUpdated":project["lastUpdated"],
            "lastUpdatedBy":member(&project["lastUpdatedBy"]),"owner":member(&project["owner"]),
            "accessLevel":project["accessLevel"],"archived":project["archived"],"trashed":project["trashed"]
        }));
    }
    projects.sort_by(|a, b| {
        b["lastUpdated"]
            .as_str()
            .unwrap_or("")
            .cmp(a["lastUpdated"].as_str().unwrap_or(""))
            .then_with(|| a["id"].as_str().cmp(&b["id"].as_str()))
    });
    let matched = projects.len();
    projects.truncate(limit);
    Ok(
        json!({"projects":projects,"count":projects.len(),"total_matched":matched,"total_account":if restricted{Value::Null}else{json!(rows.len())},"total_visible":rows.len(),"restricted_scope":restricted,
        "truncated":matched>limit,"complete":matched<=limit}),
    )
}
fn member(value: &Value) -> Value {
    if value.is_null() {
        return Value::Null;
    }
    json!({"id":value["_id"].as_str().or_else(||value["id"].as_str()),"email":value["email"],
        "first_name":value["first_name"],"last_name":value["last_name"],"privileges":value["privileges"]})
}
fn open_project(client: &mut Client) -> Result<Value> {
    let socket = client.socket()?;
    let project = &socket.project;
    let tree = tree(project)?;
    let state = project
        .get("track_changes_state")
        .or_else(|| project.get("trackChangesState"))
        .unwrap_or(&Value::Null);
    let track =
        state == true || state[&client.account] == true || state["__everyone__"] == true || state["__guests__"] == true;
    let root_doc = project["rootDoc_id"].as_str();
    let root_path = root_doc
        .and_then(|id| tree.entities.iter().find(|e| e.id == id))
        .map(|e| &e.path);
    let members: Vec<_> = project["members"]
        .as_array()
        .into_iter()
        .flatten()
        .map(member)
        .collect();
    Ok(json!({
        "project_id":client.project,"name":project["name"],"root_folder_id":tree.root,
        "root_doc_id":root_doc,"root_doc_path":root_path,"entity_count":tree.entities.len(),
        "docs":tree.entities.iter().filter(|e| e.kind=="doc").count(),
        "files":tree.entities.iter().filter(|e| e.kind=="file").count(),
        "folders":tree.entities.iter().filter(|e| e.kind=="folder").count(),
        "entities":tree.entities,"tree_sha256":tree.digest(None)?,"permissions":socket.permissions,
        "track_changes_on_for_me":track,"track_changes_state":state,"features":project["features"],
        "compiler":project["compiler"],"spell_check_language":project["spellCheckLanguage"],
        "public_access_level":project["publicAccesLevel"],"owner":member(&project["owner"]),"members":members,
        "complete":true,"truncated":false
    }))
}
fn list_files(client: &mut Client, args: &Map<String, Value>) -> Result<Value> {
    let (_, tree) = current(client, false)?;
    let kind = bounded_string(args, "kind", 10)?.unwrap_or("all");
    ensure!(
        matches!(kind, "all" | "doc" | "file" | "folder"),
        "unknown native entity kind"
    );
    let filter = bounded_string(args, "path_contains", 1024)?
        .unwrap_or("")
        .to_lowercase();
    let snapshot = if flag(args, "include_content_hashes", false)? {
        Some(manifest(client, &tree, None)?)
    } else {
        None
    };
    let mut entities = Vec::new();
    for entity in &tree.entities {
        if (kind != "all" && entity.kind != kind) || !entity.path.to_lowercase().contains(&filter) {
            continue;
        }
        let mut row = serde_json::to_value(entity)?;
        if entity.kind == "folder" {
            row["tree_sha256"] = json!(tree.digest(Some(&entity.path))?);
            if let Some(snapshot) = &snapshot {
                row["subtree_sha256"] = json!(subtree_digest(snapshot, &entity.path)?);
            }
        } else if let Some(snapshot) = &snapshot {
            row = snapshot[&entity.path].clone();
        }
        entities.push(row);
    }
    Ok(
        json!({"project_id":client.project,"root_folder_id":tree.root,"entities":entities,"count":entities.len(),
        "total_entities":tree.entities.len(),"tree_sha256":tree.digest(None)?,
        "content_hashes_included":snapshot.is_some(),"complete":true,"truncated":false}),
    )
}

pub(super) fn call(
    client: &mut Client,
    session: &NativeSession,
    name: &str,
    args: &Map<String, Value>,
) -> Result<Value> {
    match name {
        "native_list_projects" => list_projects(client, session, args),
        "native_open_project" => open_project(client),
        "native_list_files" => list_files(client, args),
        "native_read_file" => {
            let (project, tree) = current(client, false)?;
            let selected = bounded_string(args, "path", 1024)?;
            let path = match selected {
                Some(path) => path.to_owned(),
                None => {
                    let root = project["rootDoc_id"]
                        .as_str()
                        .ok_or_else(|| anyhow!("native project has no root document; supply path"))?;
                    tree.entities
                        .iter()
                        .find(|e| e.id == root && e.kind == "doc")
                        .map(|e| e.path.clone())
                        .ok_or_else(|| anyhow!("native root document not found"))?
                }
            };
            edits::path(&path)?;
            let entity = tree
                .at(&path)
                .ok_or_else(|| anyhow!("native file path not found; use native_list_files"))?;
            let mut result = file_content(client, entity, args)?;
            result["project_id"] = json!(client.project);
            let (_, after) = current(client, false)?;
            ensure!(
                after.at(&path) == Some(entity),
                "native file path changed while reading"
            );
            Ok(result)
        }
        "native_search_project" => {
            let regex = matcher(args)?;
            let include_comments = flag(args, "include_comment_lines", true)?;
            let limit = number(args, "max_results", 200, 2000)?;
            let filter = bounded_string(args, "path_contains", 1024)?
                .unwrap_or("")
                .to_lowercase();
            let mut socket = client.socket()?;
            let before = tree(&socket.project)?;
            let docs: Vec<_> = before
                .entities
                .iter()
                .filter(|e| e.kind == "doc" && e.path.to_lowercase().contains(&filter))
                .collect();
            let mut results = Vec::new();
            let mut versions = Vec::new();
            let mut scanned = 0usize;
            let mut bytes = 0usize;
            let mut truncated = false;
            let mut reason = Value::Null;
            for entity in &docs {
                if scanned >= SCAN_DOCS || bytes >= CAPTURE_LIMIT {
                    truncated = true;
                    reason = json!("scan_budget");
                    break;
                }
                let snap = socket.join(&entity.id)?;
                bytes += snap.text.len();
                if bytes > CAPTURE_LIMIT {
                    truncated = true;
                    reason = json!("byte_budget");
                    break;
                }
                scanned += 1;
                let (hits, more) = search_lines(&snap.text, &regex, include_comments, limit - results.len());
                for mut hit in hits {
                    hit["path"] = json!(entity.path);
                    hit["doc_id"] = json!(entity.id);
                    hit["version"] = json!(snap.version);
                    results.push(hit);
                }
                versions.push(json!({"doc_id":entity.id,"path":entity.path,"version":snap.version,"sha256":hash(snap.text.as_bytes())}));
                socket.leave()?;
                if more {
                    truncated = true;
                    reason = json!("max_results");
                    break;
                }
            }
            ensure!(!socket.concurrent, "native documents changed during search; read again");
            Ok(
                json!({"project_id":client.project,"results":results,"count":results.len(),"docs_searched":scanned,
                "docs_matched_filter":docs.len(),"versions":versions,"truncated":truncated,"complete":!truncated,
                "truncation_reason":reason,"regex_dialect":"Rust regex","column_encoding":"UTF-16, one based"}),
            )
        }
        "native_create_project" => {
            ensure!(
                session.project_scope().is_none(),
                "project creation is disabled by a restricted native project grant"
            );
            let project_name = need(args, "name")?;
            ensure!(
                !project_name.trim().is_empty()
                    && project_name.len() <= 150
                    && !project_name.chars().any(char::is_control),
                "invalid native project name"
            );
            ensure!(
                client.redact(project_name) == project_name,
                "project name contains credentials"
            );
            let template = bounded_string(args, "template", 16)?.unwrap_or("none");
            ensure!(
                matches!(template, "none" | "example"),
                "native template must be none or example"
            );
            once(
                client,
                session,
                name,
                args,
                json!({"created_ids":[],"atomic_precondition":false}),
                |client, receipt| {
                    let response = client.json(
                        "POST",
                        "/project/new",
                        Some(json!({"projectName":project_name,"template":template})),
                    )?;
                    let created = response["project_id"]
                        .as_str()
                        .ok_or_else(|| anyhow!("native creation acknowledgement lacks project_id"))?;
                    id(created)?;
                    receipt.details["created_project_id"] = json!(created);
                    let mut created_client = Client::new(session, created)?;
                    ensure!(
                        created_client.account == client.account,
                        "created project account mismatch"
                    );
                    let (project, _) = current(&mut created_client, true)?;
                    ensure!(
                        project["name"] == project_name
                            && project["owner"]["_id"]
                                .as_str()
                                .or_else(|| project["owner"]["id"].as_str())
                                == Some(client.account.as_str()),
                        "created project name or owner mismatch"
                    );
                    receipt.verified = true;
                    Ok(json!({"project_id":created,"created_project_id":created,"name":project_name}))
                },
            )
        }
        "native_create_doc" | "native_create_folder" | "native_upload_file" => {
            let path = need(args, "path")?;
            edits::path(path)?;
            let create_parents = flag(args, "create_parents", true)?;
            let content = bounded_string(args, "content", 256 * 1024)?.unwrap_or("");
            ensure!(
                name == "native_create_doc" || !args.contains_key("content"),
                "content is only supported for document creation"
            );
            ensure!(client.redact(content) == content, "native content contains credentials");
            let content_hash = hash(content.as_bytes());
            once(
                client,
                session,
                name,
                args,
                json!({"created_ids":[],"atomic_precondition":false,"content_sha256":if name=="native_upload_file" {args.get("content_sha256").cloned().unwrap_or(Value::Null)}else{json!(content_hash)}}),
                |client, receipt| {
                    let bytes = if name == "native_upload_file" {
                        Some(input_bytes(session, args)?)
                    } else {
                        None
                    };
                    let target_hash = bytes.as_ref().map(|b| hash(b)).unwrap_or_else(|| content_hash.clone());
                    receipt.details["content_sha256"] = json!(target_hash);
                    let (_, initial) = current(client, true)?;
                    let original = initial.at(path).cloned();
                    if name == "native_create_folder"
                        && let Some(existing) = &original
                    {
                        ensure!(existing.kind == "folder", "native path already occupied by a file");
                        receipt.details["entity_id"] = json!(existing.id);
                        receipt.verified = true;
                        return Ok(json!({"entity_id":existing.id,"created":false,"path":path}));
                    }
                    if let Some(existing) = &original {
                        ensure!(
                            name == "native_upload_file" && existing.kind == "file",
                            "existing text documents require native_edit_file; upload replacement is binary-only"
                        );
                        precondition(client, args, &initial, existing, false)?;
                    } else {
                        ensure!(
                            !args.contains_key("expected_entity_id"),
                            "expected replacement entity is absent; no upload attempted"
                        );
                    }
                    let (parent_path, file_name) = split(path);
                    let parent_id = parents(client, parent_path, create_parents, receipt)?;
                    let (_, before) = current(client, true)?;
                    ensure!(
                        before.at(path).cloned() == original,
                        "native destination changed before creation"
                    );
                    let (created, kind) = if name == "native_upload_file" {
                        if let Some(existing) = &original {
                            precondition(client, args, &before, existing, false)?;
                        }
                        let (body, content_type) =
                            multipart(file_name, bytes.as_ref().unwrap(), need(args, "operation_id")?)?;
                        let raw = client.bytes(
                            "POST",
                            &format!("/project/{}/upload?folder_id={parent_id}", client.project),
                            Some(body),
                            Some(&content_type),
                        )?;
                        let response: Value =
                            serde_json::from_slice(&raw).context("invalid native upload acknowledgement")?;
                        ensure!(response["success"] == true, "native upload did not acknowledge success");
                        let created = response["entity_id"]
                            .as_str()
                            .ok_or_else(|| anyhow!("native upload entity ID missing"))?
                            .to_owned();
                        let kind = match response["entity_type"].as_str() {
                            Some("doc") => "doc",
                            Some("file") => "file",
                            _ => bail!("native upload entity kind missing or unsupported"),
                        };
                        (created, kind)
                    } else {
                        let kind = if name == "native_create_doc" { "doc" } else { "folder" };
                        let response = client.json(
                            "POST",
                            &format!("/project/{}/{kind}", client.project),
                            Some(json!({"name":file_name,"parent_folder_id":parent_id})),
                        )?;
                        (
                            response["_id"]
                                .as_str()
                                .ok_or_else(|| anyhow!("native create acknowledgement has no ID"))?
                                .to_owned(),
                            kind,
                        )
                    };
                    id(&created)?;
                    receipt.details["entity_id"] = json!(created);
                    let (_, after_create) = current(client, true)?;
                    let entity = after_create
                        .at(path)
                        .ok_or_else(|| anyhow!("created native entity missing on readback"))?
                        .clone();
                    ensure!(
                        entity.id == created && entity.kind == kind && entity.parent_folder_id == parent_id,
                        "native created entity readback mismatch"
                    );
                    let excluded = original
                        .as_ref()
                        .map(|e| BTreeSet::from([e.id.clone()]))
                        .unwrap_or_default();
                    postcheck(&before, &after_create, &excluded)?;
                    let expected_count = before.entities.len() + usize::from(original.is_none());
                    ensure!(
                        after_create.entities.len() == expected_count,
                        "native creation changed unexpected entity count"
                    );
                    if name == "native_create_doc" {
                        let mut socket = client.socket()?;
                        ensure!(
                            tree(&socket.project)?.at(path) == Some(&entity),
                            "created native document identity changed"
                        );
                        let empty = socket.join(&created)?;
                        ensure!(
                            empty.text.is_empty(),
                            "created native document is nonempty; initial content not applied"
                        );
                        let fresh = socket.refresh(&created)?;
                        ensure!(
                            fresh.version == empty.version && fresh.text.is_empty() && !socket.concurrent,
                            "created native document changed before initial content"
                        );
                        if !content.is_empty() {
                            socket.apply(&created, json!([{"p":0,"i":content}]), fresh.version, None)?;
                            let written = socket.refresh(&created)?;
                            ensure!(
                                written.text == content && written.version == fresh.version + 1 && !socket.concurrent,
                                "initial native content readback mismatch"
                            );
                            receipt.details["version"] = json!(written.version);
                        } else {
                            receipt.details["version"] = json!(fresh.version);
                        }
                        receipt.details["sha256"] = json!(hash(content.as_bytes()));
                    } else if name == "native_upload_file" {
                        let stamp = content_stamp(client, &entity)?;
                        ensure!(
                            stamp["sha256"] == target_hash,
                            "native upload byte hash readback mismatch"
                        );
                        receipt.details["sha256"] = stamp["sha256"].clone();
                        if stamp.get("version").is_some() {
                            receipt.details["version"] = stamp["version"].clone();
                        }
                    }
                    let (_, final_tree) = current(client, true)?;
                    ensure!(
                        final_tree.at(path) == Some(&entity),
                        "native created entity changed after content verification"
                    );
                    postcheck(&before, &final_tree, &excluded)?;
                    ensure!(
                        final_tree.entities.len() == after_create.entities.len(),
                        "native creation tree changed during content verification"
                    );
                    receipt.verified = true;
                    Ok(
                        json!({"path":path,"entity_id":created,"kind":kind,"created":original.is_none(),"sha256":receipt.details["sha256"]}),
                    )
                },
            )
        }
        "native_rename_entity" | "native_move_entity" | "native_delete_entity" => {
            let path = need(args, "path")?;
            edits::path(path)?;
            let new_name = bounded_string(args, "new_name", 1024)?;
            if name == "native_rename_entity" {
                let value = new_name.ok_or_else(|| anyhow!("new_name required"))?;
                edits::path(value)?;
                ensure!(
                    !value.contains('/') && client.redact(value) == value,
                    "new_name must be one safe path component"
                );
            }
            let to_folder = bounded_string(args, "to_folder", 1024)?;
            if name == "native_move_entity" {
                let value = to_folder.ok_or_else(|| anyhow!("to_folder required (empty for root)"))?;
                if !value.is_empty() {
                    edits::path(value)?;
                }
            }
            once(
                client,
                session,
                name,
                args,
                json!({"atomic_precondition":false}),
                |client, receipt| {
                    let (project, before) = current(client, true)?;
                    let entity = before
                        .at(path)
                        .ok_or_else(|| anyhow!("native entity path not found"))?
                        .clone();
                    precondition(client, args, &before, &entity, name == "native_delete_entity")?;
                    receipt.details["entity_id"] = json!(entity.id);
                    let descendants = before.scope(path);
                    let changed: BTreeSet<String> = descendants.iter().map(|e| e.id.clone()).collect();
                    if name == "native_delete_entity" {
                        let root = project["rootDoc_id"].as_str();
                        ensure!(
                            !descendants.iter().any(|e| Some(e.id.as_str()) == root),
                            "cannot delete the root document or its containing folder"
                        );
                        client.json(
                            "DELETE",
                            &format!("/project/{}/{}/{}", client.project, entity.kind, entity.id),
                            None,
                        )?;
                        let (_, after) = current(client, true)?;
                        ensure!(
                            !after.entities.iter().any(|e| changed.contains(&e.id)
                                || e.path == path
                                || e.path.starts_with(&format!("{path}/"))),
                            "native deletion readback is incomplete"
                        );
                        postcheck(&before, &after, &changed)?;
                        ensure!(
                            after.entities.len() + changed.len() == before.entities.len(),
                            "native deletion changed unexpected entity count"
                        );
                        receipt.verified = true;
                        return Ok(json!({"entity_id":entity.id,"deleted":true}));
                    }
                    let (destination, parent, new_entity_name, endpoint, body) = if name == "native_rename_entity" {
                        let new_name = new_name.unwrap();
                        let parent_path = split(path).0;
                        (
                            if parent_path.is_empty() {
                                new_name.into()
                            } else {
                                format!("{parent_path}/{new_name}")
                            },
                            entity.parent_folder_id.clone(),
                            new_name.to_owned(),
                            "rename",
                            json!({"name":new_name}),
                        )
                    } else {
                        let target = to_folder.unwrap();
                        ensure!(
                            entity.kind != "folder" || (target != path && !target.starts_with(&format!("{path}/"))),
                            "cannot move a folder into itself or its descendants"
                        );
                        let parent = before.folder(target)?;
                        (
                            if target.is_empty() {
                                entity.name.clone()
                            } else {
                                format!("{target}/{}", entity.name)
                            },
                            parent.clone(),
                            entity.name.clone(),
                            "move",
                            json!({"folder_id":parent}),
                        )
                    };
                    ensure!(
                        destination != path && before.at(&destination).is_none(),
                        "native destination already exists or makes no change"
                    );
                    require_same_tree(client, &before)?;
                    client.json(
                        "POST",
                        &format!("/project/{}/{}/{}/{endpoint}", client.project, entity.kind, entity.id),
                        Some(body),
                    )?;
                    let (_, after) = current(client, true)?;
                    ensure!(
                        after.at(path).is_none(),
                        "old native path still present after structural mutation"
                    );
                    for old in descendants {
                        let mut expected = old.clone();
                        expected.path = format!("{destination}{}", old.path.strip_prefix(path).unwrap());
                        if old.id == entity.id {
                            expected.parent_folder_id = parent.clone();
                            expected.name = new_entity_name.clone();
                        }
                        ensure!(
                            after.at(&expected.path) == Some(&expected),
                            "native structural readback identity or descendants mismatch"
                        );
                    }
                    postcheck(&before, &after, &changed)?;
                    if entity.kind != "folder" {
                        let moved = after
                            .at(&destination)
                            .ok_or_else(|| anyhow!("native structural target missing"))?;
                        precondition(client, args, &after, moved, false)?;
                    }
                    ensure!(
                        after.entities.len() == before.entities.len()
                            && after
                                .scope(&destination)
                                .iter()
                                .map(|e| e.id.clone())
                                .collect::<BTreeSet<_>>()
                                == changed,
                        "native move/rename changed unexpected descendants"
                    );
                    receipt.verified = true;
                    Ok(json!({"entity_id":entity.id,"path":destination,"kind":entity.kind}))
                },
            )
        }
        _ => bail!("unknown native project/file tool"),
    }
}

pub(super) fn definitions() -> Vec<Value> {
    let string = json!({"type":"string"});
    let project = json!({"type":"string","pattern":"^[0-9a-f]{24}$"});
    let operation = json!({"type":"string","format":"uuid","description":"Stable operation UUID; never replace it after an uncertain result."});
    let boolean = json!({"type":"boolean"});
    let hash = json!({"type":"string","pattern":"^[0-9a-f]{64}$"});
    let preconditions = json!({
        "expected_entity_id":project,"expected_sha256":hash,
        "expected_version":{"type":"integer","minimum":0},
        "expected_tree_sha256":hash,"expected_subtree_sha256":hash
    });
    let tools = vec![
        (
            "native_create_project",
            "Create one account-owned project with an unrestricted account grant. Stable receipt and fresh owner/name readback; no replay.",
            json!({"name":{"type":"string","minLength":1,"maxLength":150},"template":{"enum":["none","example"],"default":"none"},"operation_id":operation}),
            vec!["name", "operation_id"],
            false,
            false,
        ),
        (
            "native_list_projects",
            "List visible projects with owner, update time and bounded filters. Restricted grants read only each allowed project, never the account-wide project inventory.",
            json!({"include_archived":boolean,"include_trashed":boolean,"name_contains":string,"limit":{"type":"integer","minimum":1,"maximum":500,"default":30}}),
            vec![],
            true,
            false,
        ),
        (
            "native_open_project",
            "Read a fresh project tree, root document, collaborators, permissions and compiler metadata.",
            json!({"project_id":project}),
            vec!["project_id"],
            true,
            false,
        ),
        (
            "native_list_files",
            "Read a fresh complete tree. include_content_hashes reads bounded content/version manifests for structural preconditions and folder deletion.",
            json!({"project_id":project,"kind":{"enum":["all","doc","file","folder"]},"path_contains":string,"include_content_hashes":boolean}),
            vec!["project_id"],
            true,
            false,
        ),
        (
            "native_read_file",
            "Read live text/version/review summaries or paged binary base64/MIME with full-file SHA-256. Omitted path selects the root document. Binary pages are at most 1 MiB; use next_offset and expected_sha256 for subsequent pages.",
            json!({"project_id":project,"path":string,"offset":{"type":"integer","minimum":0},
                "max_bytes":{"type":"integer","minimum":1,"maximum":1048576},
                "expected_sha256":{"type":"string","pattern":"^[0-9a-f]{64}$"}}),
            vec!["project_id"],
            true,
            false,
        ),
        (
            "native_search_project",
            "Bounded project text search with Rust regex, case/path/comment filters, one-based UTF-16 columns and explicit completeness.",
            json!({"project_id":project,"query":{"type":"string","minLength":1,"maxLength":4096},"regex":boolean,"case_sensitive":boolean,"path_contains":string,"include_comment_lines":boolean,"max_results":{"type":"integer","minimum":1,"maximum":2000,"default":200}}),
            vec!["project_id", "query"],
            true,
            false,
        ),
        (
            "native_create_doc",
            "Create a new text document, optionally creating parents and initial untracked content. Never overwrites an existing path.",
            json!({"project_id":project,"path":string,"content":{"type":"string","maxLength":262144},"create_parents":boolean,"operation_id":operation}),
            vec!["project_id", "path", "operation_id"],
            false,
            false,
        ),
        (
            "native_create_folder",
            "Create a folder and missing parents, with stable operation receipt and exact identity readback.",
            json!({"project_id":project,"path":string,"create_parents":boolean,"operation_id":operation}),
            vec!["project_id", "path", "operation_id"],
            false,
            false,
        ),
        (
            "native_upload_file",
            "Upload at most 2 MiB from strict base64 or an assigned local root; canonical symlink/dot-directory guards. local_path requires content_sha256 to bind intended bytes. Replacement requires fresh expected_entity_id and SHA-256/version.",
            json!({"project_id":project,"path":string,"local_path":string,"content_base64":string,"content_sha256":hash,"create_parents":boolean,"operation_id":operation}),
            vec!["project_id", "path", "operation_id"],
            false,
            true,
        ),
        (
            "native_rename_entity",
            "Rename exactly expected_entity_id. Require expected_sha256/version for files/docs, expected_tree_sha256 for folders; obtain from native_read_file/list_files.",
            json!({"project_id":project,"path":string,"new_name":string,"operation_id":operation}),
            vec!["project_id", "path", "new_name", "operation_id", "expected_entity_id"],
            false,
            false,
        ),
        (
            "native_move_entity",
            "Move exactly expected_entity_id into an existing folder (empty to_folder means root). Require fresh content/version or folder tree hash.",
            json!({"project_id":project,"path":string,"to_folder":string,"operation_id":operation}),
            vec!["project_id", "path", "to_folder", "operation_id", "expected_entity_id"],
            false,
            false,
        ),
        (
            "native_delete_entity",
            "Delete exactly expected_entity_id after fresh SHA-256/version checks. Folders also need expected_tree_sha256 and expected_subtree_sha256 from list_files(include_content_hashes=true). Root document deletion is refused.",
            json!({"project_id":project,"path":string,"operation_id":operation}),
            vec!["project_id", "path", "operation_id", "expected_entity_id"],
            false,
            true,
        ),
    ];
    tools.into_iter().map(|(name,description,mut properties,required,read_only,destructive)| {
        if matches!(name,"native_upload_file"|"native_rename_entity"|"native_move_entity"|"native_delete_entity") {
            properties.as_object_mut().unwrap().extend(preconditions.as_object().unwrap().clone());
        }
        json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false},
            "annotations":{"readOnlyHint":read_only,"destructiveHint":destructive,"idempotentHint":true}})
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Value {
        json!({"rootFolder":[{"_id":"000000000000000000000001","docs":[{"_id":"000000000000000000000002","name":"main.tex"}],"fileRefs":[{"_id":"000000000000000000000003","name":"plot.pdf"}],"folders":[{"_id":"000000000000000000000004","name":"parts","docs":[{"_id":"000000000000000000000005","name":"a.tex"}],"fileRefs":[],"folders":[]}]}]})
    }
    #[test]
    fn tree_wire_identity_and_subtree_preconditions() {
        let project = fixture();
        let parsed = tree(&project).unwrap();
        assert_eq!(parsed.entities.len(), 4);
        assert_eq!(parsed.folder("parts").unwrap(), "000000000000000000000004");
        assert_eq!(parsed.scope("parts").len(), 2);
        let mut changed = project.clone();
        changed["rootFolder"][0]["folders"][0]["docs"][0]["name"] = json!("b.tex");
        let changed = tree(&changed).unwrap();
        assert_ne!(
            parsed.digest(Some("parts")).unwrap(),
            changed.digest(Some("parts")).unwrap()
        );
        let mut duplicate = project.clone();
        duplicate["rootFolder"][0]["fileRefs"][0]["name"] = json!("main.tex");
        assert!(tree(&duplicate).is_err());
        let mut traversal = project.clone();
        traversal["rootFolder"][0]["docs"][0]["name"] = json!("../bad.tex");
        assert!(tree(&traversal).is_err());
    }
    #[test]
    fn search_bounds_unicode_comments_and_exact_limit() {
        let regex = Regex::new("hit").unwrap();
        let (hits, more) = search_lines("% hit\n😀 hit hit\nhit", &regex, false, 2);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0]["line"], 2);
        assert_eq!(hits[0]["col"], 4);
        assert!(more);
        let (hits, more) = search_lines("hit hit", &regex, true, 2);
        assert_eq!(hits.len(), 2);
        assert!(!more);
        let (hits, more) = search_lines("é", &Regex::new("").unwrap(), true, 2);
        assert_eq!(hits.len(), 2);
        assert!(!more);
        assert!(matcher(json!({"query":"(?=unsupported)","regex":true}).as_object().unwrap()).is_err());
    }
    #[test]
    fn upload_confinement_strict_base64_and_multipart() {
        let dir = tempfile::Builder::new()
            .prefix("interleaf-upload-")
            .tempdir_in("/tmp")
            .unwrap();
        let root = dir.path().join("uploads");
        fs::create_dir(&root).unwrap();
        let path = root.join("image.pdf");
        fs::write(&path, b"%PDF fixture").unwrap();
        let session = NativeSession::from_reader(
            br#"{"baseUrl":"https://www.overleaf.com","cookie":"overleaf_session2=dummy"}"#.as_slice(),
            dir.path().join("receipts"),
        )
        .unwrap()
        .with_upload_roots(vec![root.clone()])
        .unwrap();
        let args = json!({"local_path":path,"content_sha256":hash(b"%PDF fixture")});
        assert_eq!(
            input_bytes(&session, args.as_object().unwrap()).unwrap(),
            b"%PDF fixture"
        );
        fs::write(root.join(".secret"), b"private").unwrap();
        assert!(
            input_bytes(
                &session,
                json!({"local_path":root.join(".secret"),"content_sha256":hash(b"private")})
                    .as_object()
                    .unwrap()
            )
            .is_err()
        );
        assert!(input_bytes(&session, json!({"content_base64":"%%%"}).as_object().unwrap()).is_err());
        let bytes = input_bytes(&session, json!({"content_base64":"AAEC/w=="}).as_object().unwrap()).unwrap();
        let (body, kind) = multipart("image.pdf", &bytes, "00000000-0000-0000-0000-000000000000").unwrap();
        assert!(kind.starts_with("multipart/form-data; boundary="));
        assert!(body.windows(bytes.len()).any(|part| part == bytes));
        assert!(multipart("bad\r\nheader", &bytes, "uuid").is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&path, root.join("link.pdf")).unwrap();
            assert!(
                input_bytes(
                    &session,
                    json!({"local_path":root.join("link.pdf"),"content_sha256":hash(b"%PDF fixture")})
                        .as_object()
                        .unwrap()
                )
                .is_err()
            );
            let outside = dir.path().join("outside.pdf");
            fs::write(&outside, b"x").unwrap();
            std::os::unix::fs::symlink(&outside, root.join("escape.pdf")).unwrap();
            assert!(
                input_bytes(
                    &session,
                    json!({"local_path":root.join("escape.pdf"),"content_sha256":hash(b"x")})
                        .as_object()
                        .unwrap()
                )
                .is_err()
            );
        }
    }
}
