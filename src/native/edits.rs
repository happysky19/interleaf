//! Pending-only editing and comment anchors. All writes use fresh versions and the shared journal.
use super::socket::{Snapshot, Socket};
use super::{Client, NativeSession, Receipt, author, existing, id, need, outcome, reserve, save};
use anyhow::{Result, anyhow, ensure};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn path(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 1024
            && !value.contains(['\\', '\0'])
            && value
                .split('/')
                .all(|p| !p.is_empty() && p != "." && p != ".." && !p.chars().any(char::is_control)),
        "native path must be a normalized project-relative text document path"
    );
    Ok(())
}
pub(super) fn documents(project: &Value) -> Result<BTreeMap<String, String>> {
    fn walk(
        folder: &Value,
        prefix: &str,
        depth: usize,
        out: &mut BTreeMap<String, String>,
        ids: &mut BTreeSet<String>,
    ) -> Result<()> {
        ensure!(depth < 64 && out.len() <= 1024, "native project tree too large");
        for doc in folder["docs"]
            .as_array()
            .ok_or_else(|| anyhow!("invalid native document tree"))?
        {
            let name = doc["name"]
                .as_str()
                .ok_or_else(|| anyhow!("native document name missing"))?;
            ensure!(!name.contains('/'), "invalid native document name");
            let full = format!("{prefix}{name}");
            path(&full)?;
            let doc_id = doc["_id"]
                .as_str()
                .ok_or_else(|| anyhow!("native document ID missing"))?;
            id(doc_id)?;
            ensure!(
                out.insert(full, doc_id.into()).is_none() && ids.insert(doc_id.into()),
                "ambiguous native document identity"
            );
        }
        for sub in folder["folders"]
            .as_array()
            .ok_or_else(|| anyhow!("invalid native folder tree"))?
        {
            let name = sub["name"]
                .as_str()
                .ok_or_else(|| anyhow!("native folder name missing"))?;
            path(name)?;
            ensure!(!name.contains('/'), "invalid native folder name");
            walk(sub, &format!("{prefix}{name}/"), depth + 1, out, ids)?;
        }
        Ok(())
    }
    let roots = project["rootFolder"]
        .as_array()
        .ok_or_else(|| anyhow!("native project root missing"))?;
    ensure!(roots.len() == 1, "ambiguous native project root");
    let mut out = BTreeMap::new();
    walk(&roots[0], "", 0, &mut out, &mut BTreeSet::new())?;
    ensure!(out.len() <= 1024, "native project tree too large");
    Ok(out)
}
pub(super) fn ranges(client: &mut Client) -> Result<Vec<Value>> {
    let raw = client.get(&format!("/project/{}/ranges", client.project), "application/json")?;
    let value: Value = serde_json::from_str(&raw).map_err(|_| anyhow!("invalid native ranges response"))?;
    let docs = value
        .as_array()
        .ok_or_else(|| anyhow!("complete native ranges list required"))?;
    let mut ids = BTreeSet::new();
    for doc in docs {
        let doc_id = doc["id"]
            .as_str()
            .ok_or_else(|| anyhow!("native range document missing"))?;
        id(doc_id)?;
        ensure!(ids.insert(doc_id), "duplicate native range document");
        if let Some(changes) = doc["ranges"].get("changes") {
            let mut seen = BTreeSet::new();
            for change in changes.as_array().ok_or_else(|| anyhow!("invalid pending changes"))? {
                let change_id = change["id"]
                    .as_str()
                    .ok_or_else(|| anyhow!("pending change ID missing"))?;
                id(change_id)?;
                ensure!(seen.insert(change_id), "duplicate pending change ID");
                let op = &change["op"];
                ensure!(
                    op["p"].as_u64().is_some_and(|p| p <= 9_007_199_254_740_991)
                        && (op["i"].is_string() != op["d"].is_string())
                        && op
                            .get("i")
                            .or_else(|| op.get("d"))
                            .and_then(Value::as_str)
                            .is_some_and(|s| !s.is_empty()),
                    "invalid pending text change"
                );
            }
        }
    }
    Ok(docs.clone())
}
pub(super) fn changes(client: &mut Client, doc: &str) -> Result<Vec<Value>> {
    Ok(ranges(client)?
        .into_iter()
        .find(|row| row["id"] == doc)
        .and_then(|row| row["ranges"]["changes"].as_array().cloned())
        .unwrap_or_default())
}
/// Byte offsets stay inside Rust strings; only OT positions are UTF-16 units.
fn locate(text: &str, quote: &str, occurrence: Option<u64>) -> Result<usize> {
    ensure!(
        !quote.is_empty() && quote.len() <= 32768,
        "matched text must contain 1–32768 bytes"
    );
    let mut matches = text
        .char_indices()
        .map(|(i, _)| i)
        .filter(|i| text[*i..].starts_with(quote));
    if let Some(n) = occurrence {
        ensure!(
            (1..=1_000_000).contains(&n),
            "occurrence must be a positive bounded index"
        );
        matches
            .nth((n - 1) as usize)
            .ok_or_else(|| anyhow!("requested text occurrence not found"))
    } else {
        let first = matches
            .next()
            .ok_or_else(|| anyhow!("matched text not found in current document"))?;
        ensure!(
            matches.next().is_none(),
            "matched text is ambiguous; supply occurrence or a longer quote"
        );
        Ok(first)
    }
}
struct Replacement {
    ops: Vec<Value>,
    text: String,
    position: usize,
    deleted: String,
    inserted: String,
}
fn replacement(before: &str, old: &str, new: &str, occurrence: Option<u64>) -> Result<Replacement> {
    ensure!(new.len() <= 32768, "replacement exceeds 32768 bytes");
    ensure!(old != new, "replacement makes no change");
    let offset = locate(before, old, occurrence)?;
    let prefix: usize = old
        .chars()
        .zip(new.chars())
        .take_while(|(a, b)| a == b)
        .map(|(c, _)| c.len_utf8())
        .sum();
    let suffix: usize = old[prefix..]
        .chars()
        .rev()
        .zip(new[prefix..].chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(c, _)| c.len_utf8())
        .sum();
    let deleted = old[prefix..old.len() - suffix].to_owned();
    let inserted = new[prefix..new.len() - suffix].to_owned();
    let position = before[..offset + prefix].encode_utf16().count();
    let mut ops = Vec::new();
    if !deleted.is_empty() {
        ops.push(json!({"p":position,"d":deleted}));
    }
    if !inserted.is_empty() {
        ops.push(json!({"p":position,"i":inserted}));
    }
    let text = format!("{}{}{}", &before[..offset], new, &before[offset + old.len()..]);
    Ok(Replacement {
        ops,
        text,
        position,
        deleted,
        inserted,
    })
}
fn no_overlap(before: &[Value], edit: &Replacement) -> Result<()> {
    let start = edit.position;
    let end = start + edit.deleted.encode_utf16().count();
    for range in before {
        let op = &range["op"];
        let p = op["p"].as_u64().unwrap() as usize;
        let range_end = p + op["i"].as_str().map(|s| s.encode_utf16().count()).unwrap_or(0);
        ensure!(
            end < p || start > range_end,
            "edit touches an existing pending change; choose unchanged surrounding text"
        );
    }
    Ok(())
}
fn preserved(before: &[Value], after: &[Value], edit: &Replacement) -> bool {
    let old_end = edit.position + edit.deleted.encode_utf16().count();
    let delta = edit.inserted.encode_utf16().count() as i64 - edit.deleted.encode_utf16().count() as i64;
    before.iter().all(|old| {
        after.iter().find(|new| new["id"] == old["id"]).is_some_and(|new| {
            let p = old["op"]["p"].as_u64().unwrap();
            let expected = if p as usize >= old_end {
                p as i64 + delta
            } else {
                p as i64
            };
            new["op"]["p"].as_i64() == Some(expected)
                && new["op"]["i"] == old["op"]["i"]
                && new["op"]["d"] == old["op"]["d"]
                && new["metadata"] == old["metadata"]
        })
    })
}
fn tracked(after: &[Value], before: &[Value], edit: &Replacement, seed: &str, account: &str) -> Option<Vec<String>> {
    if !preserved(before, after, edit) {
        return None;
    }
    let original: BTreeSet<_> = before.iter().map(|c| c["id"].as_str().unwrap()).collect();
    let added: Vec<_> = after
        .iter()
        .filter(|c| !original.contains(c["id"].as_str().unwrap()))
        .collect();
    if added.len() != edit.ops.len() {
        return None;
    }
    let mut expected: BTreeSet<_> = edit
        .ops
        .iter()
        .map(|op| {
            (
                op.get("i").is_some(),
                op.get("i").or_else(|| op.get("d")).unwrap().as_str().unwrap(),
            )
        })
        .collect();
    let mut ids = Vec::new();
    for change in added {
        let op = &change["op"];
        let change_id = change["id"].as_str().unwrap();
        let p = op["p"].as_u64().unwrap() as usize;
        let kind = op.get("i").is_some();
        let text = op.get("i").or_else(|| op.get("d")).unwrap().as_str().unwrap();
        if !change_id.starts_with(seed)
            || author(&change["metadata"]) != Some(account)
            || p < edit.position
            || p > edit.position + edit.inserted.encode_utf16().count()
            || !expected.remove(&(kind, text))
        {
            return None;
        }
        ids.push(change_id.into());
    }
    expected.is_empty().then_some(ids)
}
fn baseline(socket: &mut Socket, doc: &str, expected: u64) -> Result<Snapshot> {
    let snap = socket.join(doc)?;
    ensure!(
        snap.version == expected,
        "native document version changed; read the file again"
    );
    ensure!(!socket.concurrent, "concurrent native change detected");
    Ok(snap)
}
fn unchanged(socket: &mut Socket, doc: &str, before: &Snapshot) -> Result<()> {
    let fresh = socket.refresh(doc)?;
    ensure!(
        fresh.version == before.version && fresh.text == before.text && !socket.concurrent,
        "native baseline changed; no further mutation attempted"
    );
    Ok(())
}
/// The review inventory always keeps full text internally; output limits never select writes.
pub(super) fn inventory(client: &mut Client, project: &Value) -> Result<Vec<Value>> {
    let docs = documents(project)?;
    let by_id: BTreeMap<_, _> = docs.iter().map(|(p, d)| (d.as_str(), p.as_str())).collect();
    let mut users = BTreeMap::new();
    for user in std::iter::once(&project["owner"]).chain(project["members"].as_array().into_iter().flatten()) {
        if let Some(id) = user["_id"].as_str() {
            users.insert(id, user);
        }
    }
    let mut rows = Vec::new();
    let mut ids = BTreeSet::new();
    for doc in ranges(client)? {
        let doc_id = doc["id"].as_str().unwrap();
        let path = by_id
            .get(doc_id)
            .ok_or_else(|| anyhow!("pending changes refer to a missing document"))?;
        for change in doc["ranges"]["changes"].as_array().into_iter().flatten() {
            ensure!(
                ids.insert(change["id"].as_str().unwrap().to_owned()),
                "ambiguous pending change identity"
            );
            let user_id = author(&change["metadata"]);
            let user = user_id.and_then(|id| users.get(id)).copied().unwrap_or(&Value::Null);
            let author_name = [user["first_name"].as_str(), user["last_name"].as_str()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" ");
            let insert = change["op"]["i"].is_string();
            rows.push(
                json!({"change_id":change["id"],"doc_id":doc_id,"path":path,"doc_path":path,
                "op":change["op"],"metadata":change["metadata"],"pending":true,
                "kind":if insert {"insert"} else {"delete"},"position":change["op"]["p"],
                "text":change["op"][if insert {"i"} else {"d"}],"user_id":user_id,
                "author_name":author_name,"author_email":user["email"],"timestamp":change["metadata"]["ts"]}),
            );
        }
    }
    Ok(rows)
}
pub(super) fn matches_filter(row: &Value, args: &Map<String, Value>) -> Result<bool> {
    for key in [
        "path",
        "path_contains",
        "author_email",
        "author_id_endswith",
        "text_contains",
    ] {
        if let Some(value) = args.get(key) {
            ensure!(
                value.as_str().is_some_and(|s| !s.is_empty() && s.len() <= 32768),
                "invalid native change filter {key}"
            );
        }
    }
    let kind = args.get("kind").map(|v| v.as_str().unwrap_or("")).unwrap_or("any");
    ensure!(matches!(kind, "any" | "insert" | "delete"), "invalid change kind");
    Ok(args.get("path").is_none_or(|p| row["path"] == *p)
        && args.get("path_contains").is_none_or(|p| {
            row["path"]
                .as_str()
                .unwrap_or("")
                .to_lowercase()
                .contains(&p.as_str().unwrap().to_lowercase())
        })
        && args.get("author_email").is_none_or(|p| {
            row["author_email"]
                .as_str()
                .is_some_and(|email| email.eq_ignore_ascii_case(p.as_str().unwrap()))
        })
        && args
            .get("author_id_endswith")
            .is_none_or(|p| row["user_id"].as_str().unwrap_or("").ends_with(p.as_str().unwrap()))
        && (kind == "any" || row["kind"] == kind)
        && args.get("text_contains").is_none_or(|p| {
            row["text"]
                .as_str()
                .unwrap_or("")
                .to_lowercase()
                .contains(&p.as_str().unwrap().to_lowercase())
        }))
}
pub(super) fn filter_properties() -> Value {
    json!({"path":{"type":"string"},"path_contains":{"type":"string","minLength":1},"author_email":{"type":"string","minLength":1},
        "author_id_endswith":{"type":"string","minLength":1},"kind":{"type":"string","enum":["any","insert","delete"]},"text_contains":{"type":"string","minLength":1}})
}
pub(super) fn definitions() -> Vec<Value> {
    let mut filter = filter_properties().as_object().unwrap().clone();
    filter.extend(json!({"project_id":{"type":"string"},"limit":{"type":"integer","minimum":1,"maximum":2000},"full_text":{"type":"boolean"}}).as_object().unwrap().clone());
    let edit = json!({"project_id":{"type":"string"},"path":{"type":"string","description":"Defaults to the project root document."},
        "new_content":{"type":"string","description":"Desired complete file content; exclusive with old_string/new_string."},
        "content":{"type":"string","description":"Alias for new_content."},
        "old_string":{"type":"string","minLength":1},"new_string":{"type":"string"},
        "occurrence":{"type":"integer","minimum":1},"replace_all":{"type":"boolean"},
        "expected_version":{"type":"integer","minimum":0},"operation_id":{"type":"string","format":"uuid"},
        "track":{"type":"string","enum":["on","off","auto"],"default":"on"},"strict_version":{"type":"boolean","const":true}});
    [
        ("native_list_tracked_changes", "Read pending changes with author, document, kind and text filters. A supplied limit reports truncation explicitly.", Value::Object(filter), vec!["project_id"], true),
        ("native_edit_file", "Edit live text with a minimal versioned diff or exact replacement. Explicit tracking and pending-change preservation are verified; direct editing requires its own capability.", edit.clone(), vec!["project_id","expected_version","operation_id"], false),
        ("native_find_and_replace", "Replace an exact occurrence or all nonoverlapping matches in a live document at its exact version, with explicit tracking mode.", edit, vec!["project_id","old_string","new_string","expected_version","operation_id"], false),
    ].into_iter().map(|(name,description,properties,required,read)| json!({"name":name,"description":description,
        "inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false},
        "annotations":{"readOnlyHint":read,"destructiveHint":false,"idempotentHint":true}})).collect()
}
fn minimal(before: &str, after: &str) -> Result<Replacement> {
    ensure!(before != after, "edit makes no change");
    let prefix: usize = before
        .chars()
        .zip(after.chars())
        .take_while(|(a, b)| a == b)
        .map(|(c, _)| c.len_utf8())
        .sum();
    let suffix: usize = before[prefix..]
        .chars()
        .rev()
        .zip(after[prefix..].chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(c, _)| c.len_utf8())
        .sum();
    let deleted = before[prefix..before.len() - suffix].to_owned();
    let inserted = after[prefix..after.len() - suffix].to_owned();
    let position = before[..prefix].encode_utf16().count();
    let mut ops = Vec::new();
    if !deleted.is_empty() {
        ops.push(json!({"p":position,"d":deleted}));
    }
    if !inserted.is_empty() {
        ops.push(json!({"p":position,"i":inserted}));
    }
    Ok(Replacement {
        ops,
        text: after.into(),
        position,
        deleted,
        inserted,
    })
}
// Keep unchanged islands untouched so pending suggestions between separate edits survive.
fn components(before: &str, after: &str) -> Result<Vec<Replacement>> {
    let diff = similar::TextDiff::configure()
        .timeout(std::time::Duration::from_secs(1))
        .diff_chars(before, after);
    let mut out = Vec::new();
    let mut position = 0;
    let mut start = 0;
    let mut deleted = String::new();
    let mut inserted = String::new();
    fn flush(out: &mut Vec<Replacement>, start: usize, deleted: &mut String, inserted: &mut String) {
        if deleted.is_empty() && inserted.is_empty() {
            return;
        }
        let mut ops = Vec::new();
        if !deleted.is_empty() {
            ops.push(json!({"p":start,"d":deleted}));
        }
        if !inserted.is_empty() {
            ops.push(json!({"p":start,"i":inserted}));
        }
        out.push(Replacement {
            ops,
            text: String::new(),
            position: start,
            deleted: std::mem::take(deleted),
            inserted: std::mem::take(inserted),
        });
    }
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Equal => {
                flush(&mut out, start, &mut deleted, &mut inserted);
                position += change.value().encode_utf16().count();
                start = position;
            }
            similar::ChangeTag::Delete => {
                deleted.push_str(change.value());
                position += change.value().encode_utf16().count();
            }
            similar::ChangeTag::Insert => inserted.push_str(change.value()),
        }
    }
    flush(&mut out, start, &mut deleted, &mut inserted);
    ensure!(
        out.len() <= 4096,
        "too many disjoint edits; submit a smaller versioned edit"
    );
    Ok(out)
}
fn batch_preserved(before: &[Value], after: &[Value], edits: &[Replacement]) -> bool {
    before.iter().all(|old| {
        after.iter().find(|new| new["id"] == old["id"]).is_some_and(|new| {
            let p = old["op"]["p"].as_u64().unwrap() as i64;
            let shift: i64 = edits
                .iter()
                .filter(|edit| p >= (edit.position + edit.deleted.encode_utf16().count()) as i64)
                .map(|edit| edit.inserted.encode_utf16().count() as i64 - edit.deleted.encode_utf16().count() as i64)
                .sum();
            new["op"]["p"].as_i64() == Some(p + shift)
                && new["op"]["i"] == old["op"]["i"]
                && new["op"]["d"] == old["op"]["d"]
                && new["metadata"] == old["metadata"]
        })
    })
}
fn batch_tracked(
    after: &[Value],
    before: &[Value],
    edits: &[Replacement],
    seed: &str,
    account: &str,
) -> Option<Vec<String>> {
    if !batch_preserved(before, after, edits) {
        return None;
    }
    let old: BTreeSet<_> = before.iter().map(|v| v["id"].as_str().unwrap()).collect();
    let added: Vec<_> = after
        .iter()
        .filter(|v| !old.contains(v["id"].as_str().unwrap()))
        .collect();
    let mut expected = Vec::new();
    let mut shift = 0i64;
    for edit in edits {
        let start = (edit.position as i64 + shift) as u64;
        let end = start + edit.inserted.encode_utf16().count() as u64;
        for op in &edit.ops {
            expected.push((
                op.get("i").is_some(),
                op.get("i").or_else(|| op.get("d")).unwrap().as_str().unwrap(),
                start,
                end,
            ));
        }
        shift += edit.inserted.encode_utf16().count() as i64 - edit.deleted.encode_utf16().count() as i64;
    }
    if added.len() != expected.len() {
        return None;
    }
    let mut ids = Vec::new();
    for row in added {
        let op = &row["op"];
        let id = row["id"].as_str().unwrap();
        let p = op["p"].as_u64().unwrap();
        let inserted = op.get("i").is_some();
        let text = op.get("i").or_else(|| op.get("d")).unwrap().as_str().unwrap();
        if !id.starts_with(seed) || author(&row["metadata"]) != Some(account) {
            return None;
        }
        let found = expected
            .iter()
            .position(|(kind, value, start, end)| inserted == *kind && text == *value && p >= *start && p <= *end)?;
        expected.remove(found);
        ids.push(id.to_owned());
    }
    expected.is_empty().then_some(ids)
}
fn full_edit(client: &mut Client, session: &NativeSession, name: &str, args: &Map<String, Value>) -> Result<Value> {
    let mut socket = client.socket()?;
    ensure!(
        matches!(socket.permissions.as_str(), "readAndWrite" | "owner"),
        "native project is not writable"
    );
    let docs = documents(&socket.project)?;
    let selected = args
        .get("path")
        .map(|v| v.as_str().ok_or_else(|| anyhow!("path must be text")))
        .transpose()?;
    let (path, doc) = if let Some(path) = selected {
        self::path(path)?;
        docs.get_key_value(path)
            .ok_or_else(|| anyhow!("native text document path not found"))?
    } else {
        let root = socket.project["rootDoc_id"]
            .as_str()
            .or_else(|| socket.project["rootDocId"].as_str())
            .ok_or_else(|| anyhow!("project root document missing; supply path"))?;
        docs.iter()
            .find(|(_, id)| id.as_str() == root)
            .ok_or_else(|| anyhow!("project root document is not text"))?
    };
    let expected = args
        .get("expected_version")
        .and_then(Value::as_u64)
        .filter(|v| *v < 9_007_199_254_740_991)
        .ok_or_else(|| anyhow!("exact expected_version is required"))?;
    ensure!(
        args.get("strict_version").is_none_or(|v| v == true),
        "strict_version must remain true"
    );
    ensure!(
        args.get("replace_all").is_none_or(Value::is_boolean),
        "replace_all must be boolean"
    );
    let track = args.get("track").map(|v| v.as_str().unwrap_or("")).unwrap_or("on");
    ensure!(matches!(track, "on" | "off" | "auto"), "track must be on, off or auto");
    let forced = super::manage::tracking_on(&socket.project, &client.account)?;
    let tracked_mode = track == "on" || forced;
    if tracked_mode {
        ensure!(
            socket.project["features"]["trackChanges"] == true,
            "native Track Changes unavailable"
        );
    }
    let before = baseline(&mut socket, doc, expected)?;
    ensure!(
        !args.contains_key("content") || !args.contains_key("new_content"),
        "use new_content or content, never both"
    );
    let edit = if let Some(content) = args.get("new_content").or_else(|| args.get("content")) {
        ensure!(
            name == "native_edit_file"
                && !args.contains_key("old_string")
                && !args.contains_key("new_string")
                && !args.contains_key("occurrence")
                && !args.contains_key("replace_all"),
            "content cannot be combined with replacement selectors"
        );
        let content = content
            .as_str()
            .filter(|s| s.len() <= super::LIMIT as usize)
            .ok_or_else(|| anyhow!("content must be bounded text"))?;
        minimal(&before.text, content)?
    } else {
        let old = need(args, "old_string")?;
        ensure!(old.len() <= 32768, "old_string too large");
        let new = args
            .get("new_string")
            .and_then(Value::as_str)
            .filter(|s| s.len() <= 32768)
            .ok_or_else(|| anyhow!("new_string must be bounded text"))?;
        let occurrence = args
            .get("occurrence")
            .map(|v| v.as_u64().ok_or_else(|| anyhow!("occurrence must be integer")))
            .transpose()?;
        if args.get("replace_all") == Some(&Value::Bool(true)) {
            ensure!(occurrence.is_none(), "replace_all cannot be combined with occurrence");
            ensure!(before.text.contains(old), "matched text not found");
            let matches = before.text.match_indices(old).count();
            let projected = before.text.len().checked_sub(old.len() * matches).and_then(|size| {
                new.len()
                    .checked_mul(matches)
                    .and_then(|growth| size.checked_add(growth))
            });
            ensure!(
                projected.is_some_and(|size| size <= super::LIMIT as usize),
                "replacement result exceeds native size limit"
            );
            let after = before.text.replace(old, new);
            ensure!(after.len() <= super::LIMIT as usize, "result too large");
            minimal(&before.text, &after)?
        } else {
            replacement(&before.text, old, new, occurrence)?
        }
    };
    let pieces = components(&before.text, &edit.text)?;
    for piece in &pieces {
        ensure!(
            client.redact(&piece.deleted) == piece.deleted && client.redact(&piece.inserted) == piece.inserted,
            "edit must not contain credentials"
        );
    }
    let original = changes(client, doc)?;
    for piece in &pieces {
        no_overlap(&original, piece)?;
    }
    let ops: Vec<_> = pieces.iter().rev().flat_map(|piece| piece.ops.clone()).collect();
    unchanged(&mut socket, doc, &before)?;
    let seed = format!("{:x}", Sha256::digest(need(args, "operation_id")?.as_bytes()));
    let seed = &seed[..18];
    super::once(
        client,
        session,
        name,
        args,
        json!({"operation":name,"doc_id":doc,"path":path,"version_before":before.version,
        "before_sha256":format!("{:x}",Sha256::digest(before.text.as_bytes())),"after_sha256":format!("{:x}",Sha256::digest(edit.text.as_bytes())),"tracked":tracked_mode,"requested_track":track,"track_mode":track,"track_overridden":track=="off" && forced}),
        |client, receipt| {
            unchanged(&mut socket, doc, &before)?;
            ensure!(
                changes(client, doc)? == original,
                "pending changes changed; no edit submitted"
            );
            socket.apply(doc, json!(ops), before.version, tracked_mode.then_some(seed))?;
            let after = socket.refresh(doc)?;
            let pending = changes(client, doc)?;
            let final_read = socket.refresh(doc)?;
            let ids = if tracked_mode {
                batch_tracked(&pending, &original, &pieces, seed, &client.account)
            } else {
                Some(Vec::new())
            };
            let original_ids: BTreeSet<_> = original.iter().map(|v| v["id"].as_str().unwrap()).collect();
            let no_new = pending.iter().all(|v| original_ids.contains(v["id"].as_str().unwrap()));
            receipt.verified = after.version == before.version + 1
                && after.text == edit.text
                && final_read.version == after.version
                && final_read.text == edit.text
                && !socket.concurrent
                && batch_preserved(&original, &pending, &pieces)
                && ids.is_some()
                && (tracked_mode || no_new);
            receipt.details["change_ids"] = json!(ids.unwrap_or_default());
            receipt.details["version_after"] = json!(after.version);
            receipt.details["originals_preserved"] = json!(batch_preserved(&original, &pending, &pieces));
            Ok(json!({}))
        },
    )
}

pub(super) fn call(
    client: &mut Client,
    session: &NativeSession,
    name: &str,
    args: &Map<String, Value>,
) -> Result<Value> {
    if matches!(name, "native_edit_file" | "native_find_and_replace") {
        return full_edit(client, session, name, args);
    }
    let writing = matches!(name, "native_suggest_edit" | "native_add_comment");
    let operation = args.get("operation_id").and_then(Value::as_str).unwrap_or("");
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
    if writing && let Some(receipt) = existing(&session.receipts, operation, &binding, &client.project)? {
        return Ok(outcome(&receipt, true));
    }
    let mut socket = client.socket()?;
    let docs = documents(&socket.project)?;
    if name == "native_list_tracked_changes" {
        if let Some(path) = args.get("path") {
            ensure!(
                path.as_str().is_some_and(|p| docs.contains_key(p)),
                "native text document path not found"
            );
        }
        ensure!(
            args.get("full_text").is_none_or(Value::is_boolean),
            "full_text must be boolean"
        );
        let limit = args
            .get("limit")
            .map(|v| {
                v.as_u64()
                    .filter(|v| (1..=2000).contains(v))
                    .ok_or_else(|| anyhow!("limit must be 1–2000"))
            })
            .transpose()?;
        let mut rows = Vec::new();
        for row in inventory(client, &socket.project)? {
            if matches_filter(&row, args)? {
                rows.push(row);
            }
        }
        let total = rows.len();
        if let Some(limit) = limit {
            rows.truncate(limit as usize);
        }
        let truncated = rows.len() < total;
        let mut text_truncated = false;
        if args.get("full_text") == Some(&Value::Bool(false)) {
            for row in &mut rows {
                let text = row["text"].as_str().unwrap();
                if text.chars().count() > 200 {
                    let shortened = text.chars().take(200).collect::<String>();
                    row["text"] = json!(shortened);
                    row["text_truncated"] = json!(true);
                    row.as_object_mut().unwrap().remove("op");
                    text_truncated = true;
                }
            }
        }
        return Ok(
            json!({"project_id":client.project,"changes":rows,"count":rows.len(),"total":total,"complete":!truncated,"truncated":truncated,"text_truncated":text_truncated}),
        );
    }
    let path = need(args, "path")?;
    let doc = docs
        .get(path)
        .ok_or_else(|| anyhow!("native text document path not found"))?;
    if name == "native_read_file" {
        let snap = socket.join(doc)?;
        return Ok(
            json!({"project_id":client.project,"path":path,"doc_id":doc,"text":snap.text,"version":snap.version,"complete":true}),
        );
    }
    ensure!(
        matches!(socket.permissions.as_str(), "readAndWrite" | "owner"),
        "native project is not writable"
    );
    let expected = args["expected_version"].as_u64().unwrap();
    let before = baseline(&mut socket, doc, expected)?;
    let occurrence = args.get("occurrence").and_then(Value::as_u64);
    if name == "native_suggest_edit" {
        ensure!(
            socket.project["features"]["trackChanges"] == true,
            "native Track Changes unavailable for this project; no edit attempted"
        );
        let edit = replacement(
            &before.text,
            need(args, "old_string")?,
            args["new_string"].as_str().unwrap(),
            occurrence,
        )?;
        ensure!(
            client.redact(&edit.deleted) == edit.deleted && client.redact(&edit.inserted) == edit.inserted,
            "suggestion must not contain credentials"
        );
        let original = changes(client, doc)?;
        no_overlap(&original, &edit)?;
        unchanged(&mut socket, doc, &before)?;
        let seed = &binding[..18];
        let details = json!({"operation":name,"doc_id":doc,"path":path,"version_before":before.version,"tracked":false,"change_ids":[],"seed":seed});
        let (file, mut receipt, fresh) =
            reserve(&session.receipts, operation, &binding, &client.project, None, details)?;
        if !fresh {
            return Ok(outcome(&receipt, true));
        }
        let attempt = (|| -> Result<()> {
            socket.apply(doc, json!(edit.ops), before.version, Some(seed))?;
            let after = socket.refresh(doc)?;
            receipt.details["version_after"] = json!(after.version);
            let pending = changes(client, doc)?;
            let final_read = socket.refresh(doc)?;
            let ids = tracked(&pending, &original, &edit, seed, &client.account);
            let verified = after.version == before.version + 1
                && final_read.version == after.version
                && after.text == edit.text
                && final_read.text == edit.text
                && !socket.concurrent
                && ids.is_some();
            receipt.details["tracked"] = json!(ids.is_some());
            receipt.details["originals_preserved"] = json!(preserved(&original, &pending, &edit));
            receipt.details["change_ids"] = json!(ids.unwrap_or_default());
            receipt.details["pending_count"] = json!(pending.len());
            receipt.verified = verified;
            receipt.state = if verified { "confirmed" } else { "uncertain" }.into();
            Ok(())
        })();
        if attempt.is_err() {
            receipt.details["readback_incomplete"] = json!(true);
        }
        return Ok(finish(&file, &receipt));
    }
    let quote = need(args, "quoted_text")?;
    let content = need(args, "content")?;
    ensure!(
        client.redact(content) == content && client.redact(quote) == quote,
        "comment must not contain credentials"
    );
    let position = before.text[..locate(&before.text, quote, occurrence)?]
        .encode_utf16()
        .count();
    let thread = format!("{}000001", &binding[..18]);
    ensure!(
        client.threads()?.get(&thread).is_none(),
        "generated thread already exists; no comment attempted"
    );
    unchanged(&mut socket, doc, &before)?;
    let details = json!({"operation":name,"doc_id":doc,"path":path,"version_before":before.version,"anchored":false,"stage":"prepared"});
    let (file, mut receipt, fresh) = reserve(
        &session.receipts,
        operation,
        &binding,
        &client.project,
        Some(&thread),
        details,
    )?;
    if !fresh {
        return Ok(outcome(&receipt, true));
    }
    let attempt = (|| -> Result<()> {
        receipt.details["stage"] = json!("message_attempted");
        save(&file, &receipt)?;
        let returned = client.message(&thread, content)?;
        receipt.message_id = returned.clone();
        receipt.details["stage"] = json!("message_posted");
        save(&file, &receipt)?;
        let verified = client.posted_message_id(&thread, content, returned.as_deref(), &BTreeSet::new())?;
        ensure!(verified.is_some(), "native message unverified; no anchor attempted");
        receipt.message_id = verified;
        receipt.details["stage"] = json!("message_verified");
        save(&file, &receipt)?;
        unchanged(&mut socket, doc, &before)?;
        receipt.details["stage"] = json!("anchor_attempted");
        save(&file, &receipt)?;
        socket.apply(doc, json!([{"p":position,"c":quote,"t":thread}]), before.version, None)?;
        let after = socket.refresh(doc)?;
        receipt.details["version_after"] = json!(after.version);
        let messages = client.threads()?;
        let message_verified = receipt.message_id.as_ref().is_some_and(|id| {
            messages[&thread]["messages"].as_array().is_some_and(|rows| {
                rows.iter()
                    .any(|m| m["id"] == *id && m["content"] == content && author(m) == Some(client.account.as_str()))
            })
        });
        let anchored = ranges(client)?
            .iter()
            .filter(|row| row["id"] == *doc)
            .filter_map(|row| row["ranges"]["comments"].as_array())
            .flatten()
            .any(|range| {
                range["op"]["t"] == thread
                    && range["op"]["p"].as_u64() == Some(position as u64)
                    && range["op"]["c"] == quote
            });
        let final_read = socket.refresh(doc)?;
        receipt.details["anchored"] = json!(anchored);
        receipt.verified = message_verified
            && anchored
            && after.version == before.version + 1
            && final_read.version == after.version
            && after.text == before.text
            && final_read.text == before.text
            && !socket.concurrent;
        receipt.state = if receipt.verified { "confirmed" } else { "uncertain" }.into();
        receipt.details["stage"] = json!(if receipt.verified {
            "confirmed"
        } else {
            "verification_incomplete"
        });
        Ok(())
    })();
    if attempt.is_err() {
        receipt.details["readback_incomplete"] = json!(true);
    }
    Ok(finish(&file, &receipt))
}
fn finish(path: &std::path::Path, receipt: &Receipt) -> Value {
    outcome(receipt, save(path, receipt).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn full_edit_minimizes_unicode_diff_and_change_filters_stay_precise() {
        let edit = minimal("α😀 model!", "α😀 better model!").unwrap();
        assert_eq!(edit.ops, vec![json!({"p":4,"i":"better "})]);
        let edit = minimal("", "😀").unwrap();
        assert_eq!(edit.ops, vec![json!({"p":0,"i":"😀"})]);
        assert!(minimal("same", "same").is_err());
        let pieces = components("bad KEEP bad", "good KEEP good").unwrap();
        let pending = vec![json!({"id":"original","op":{"p":4,"i":"KEEP"},"metadata":{"user_id":"author"}})];
        assert_eq!(pieces.len(), 2);
        for piece in &pieces {
            assert!(no_overlap(&pending, piece).is_ok());
        }
        let shifted = vec![json!({"id":"original","op":{"p":5,"i":"KEEP"},"metadata":{"user_id":"author"}})];
        assert!(batch_preserved(&pending, &shifted, &pieces));
        let row = json!({"path":"Sections/Intro.tex","kind":"insert","user_id":"abcdef","author_email":"A@example.invalid","text":"New result"});
        assert!(matches_filter(&row,json!({"path_contains":"intro","author_email":"a@example.invalid","author_id_endswith":"def","kind":"insert","text_contains":"RESULT"}).as_object().unwrap()).unwrap());
        assert!(!matches_filter(&row, json!({"kind":"delete"}).as_object().unwrap()).unwrap());
        assert!(matches_filter(&row, json!({"kind":"unknown"}).as_object().unwrap()).is_err());
    }
    #[test]
    fn replacement_keeps_utf16_boundaries_and_rejects_overlapping_ambiguity() {
        assert!(replacement("α😀 aaa", "aa", "X", None).is_err());
        let edit = replacement("α😀 aaa", "aa", "X", Some(2)).unwrap();
        assert_eq!(edit.text, "α😀 aX");
        assert_eq!(edit.position, 5);
        assert_eq!(edit.ops, vec![json!({"p":5,"d":"aa"}), json!({"p":5,"i":"X"})]);
        let edit = replacement("α 😀a Tail", "😀a", "😀b", None).unwrap();
        assert_eq!(edit.position, 4);
        assert_eq!(edit.deleted, "a");
        assert_eq!(edit.inserted, "b");
        let edit = replacement("α😀 old!", "old", "", None).unwrap();
        assert_eq!(edit.text, "α😀 !");
        assert_eq!(edit.ops, vec![json!({"p":4,"d":"old"})]);
        let edit = replacement("α😀 model!", "model", "better model", None).unwrap();
        assert_eq!(edit.ops, vec![json!({"p":4,"i":"better "})]);
    }
}
