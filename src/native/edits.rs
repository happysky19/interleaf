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
fn ranges(client: &mut Client) -> Result<Vec<Value>> {
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
fn changes(client: &mut Client, doc: &str) -> Result<Vec<Value>> {
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
pub(super) fn call(
    client: &mut Client,
    session: &NativeSession,
    name: &str,
    args: &Map<String, Value>,
) -> Result<Value> {
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
        let selected = args.get("path").and_then(Value::as_str);
        if let Some(path) = selected {
            ensure!(docs.contains_key(path), "native text document path not found");
        }
        let by_id: BTreeMap<_, _> = docs.iter().map(|(path, id)| (id.as_str(), path.as_str())).collect();
        let mut rows = Vec::new();
        for doc in ranges(client)? {
            if let Some(pending) = doc["ranges"].get("changes") {
                for change in pending.as_array().unwrap() {
                    let doc_id = doc["id"].as_str().unwrap();
                    let path = by_id
                        .get(doc_id)
                        .ok_or_else(|| anyhow!("pending changes refer to a missing document; re-read project"))?;
                    if selected.is_some_and(|p| p != *path) {
                        continue;
                    }
                    rows.push(json!({"change_id":change["id"],"doc_id":doc_id,"path":path,"op":change["op"],"metadata":change["metadata"],"pending":true}));
                }
            }
        }
        return Ok(
            json!({"project_id":client.project,"changes":rows,"count":rows.len(),"complete":true,"truncated":false}),
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
