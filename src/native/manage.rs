//! Native review mutations. Select exact current state, reserve durable intent, and verify readback.
use super::edits;
use super::socket::{Snapshot, Socket};
use super::{Client, NativeSession, author, id, need, valid_message_id};
use anyhow::{Result, anyhow, ensure};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn definitions() -> Vec<Value> {
    let base = json!({"project_id":{"type":"string"},"operation_id":{"type":"string","format":"uuid"}});
    let thread = json!({"thread_id":{"type":"string"},"doc_id":{"type":"string"},"expected_version":{"type":"integer","minimum":0},
        "expected_resolved":{"type":"boolean"},"any_author":{"type":"boolean"},
        "expected_messages":{"type":"object","additionalProperties":{"type":"string"},"description":"Exact map of every current message ID to original message text; required before deleting a thread."}});
    let message = json!({"thread_id":{"type":"string"},"message_id":{"type":"string"},"content":{"type":"string"},
        "expected_content":{"type":"string"},"expected_content_sha256":{"type":"string","pattern":"^[0-9a-f]{64}$"},"any_author":{"type":"boolean"}});
    let mut rows = Vec::new();
    for (name, description, props, required, destructive) in [
        (
            "native_resolve_comment",
            "Resolve one existing thread at an exact native document version, verifying messages and unrelated threads.",
            thread.clone(),
            vec!["thread_id", "expected_version"],
            false,
        ),
        (
            "native_reopen_comment",
            "Reopen one resolved native thread at an exact document version.",
            thread.clone(),
            vec!["thread_id", "expected_version"],
            false,
        ),
        (
            "native_delete_comment",
            "Delete exactly one thread after matching all original message bodies. Own messages only unless any_author is explicitly authorized for the project owner.",
            thread,
            vec!["thread_id", "expected_version", "expected_messages"],
            true,
        ),
        (
            "native_edit_comment_message",
            "Edit one original message after matching expected_content or its SHA256. Only the logged-in author can edit a message.",
            message.clone(),
            vec!["thread_id", "message_id", "content"],
            true,
        ),
        (
            "native_delete_comment_message",
            "Delete one message after matching its original content or SHA256, preserving all other messages.",
            message,
            vec!["thread_id", "message_id"],
            true,
        ),
        (
            "native_set_track_changes",
            "Change only the logged-in user's review switch, preserving every collaborator and guest entry and verifying a fresh state read.",
            json!({"on":{"type":"boolean"}}),
            vec!["on"],
            false,
        ),
        (
            "native_accept_changes",
            "Accept precise pending IDs or an explicit AND-filter/all selection. Every selected document requires its exact expected version. Partial outcomes remain uncertain.",
            edits::filter_properties(),
            vec![],
            true,
        ),
        (
            "native_reject_changes",
            "Reject precise pending IDs or explicit filters/all using versioned UTF16 inverse operations. No replay or destructive rollback; unrelated pending changes must be preserved.",
            edits::filter_properties(),
            vec![],
            true,
        ),
    ] {
        let mut properties = base.as_object().unwrap().clone();
        properties.extend(props.as_object().unwrap().clone());
        if name == "native_edit_comment_message" {
            properties.remove("any_author");
        }
        if matches!(name, "native_accept_changes" | "native_reject_changes") {
            properties.extend(json!({"change_ids":{"type":"array","items":{"type":"string"},"minItems":1,"uniqueItems":true},
                "all":{"type":"boolean"},"expected_version":{"type":"integer","minimum":0},
                "expected_versions":{"type":"object","additionalProperties":{"type":"integer","minimum":0},"description":"Exact document version map keyed by doc_id or project-relative path. A single expected_version is allowed only for a single document."}}).as_object().unwrap().clone());
        }
        let mut required = required;
        required.extend(["project_id", "operation_id"]);
        rows.push(json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false},
            "annotations":{"readOnlyHint":false,"destructiveHint":destructive,"idempotentHint":true}}));
    }
    rows
}
fn digest(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}
fn writable(socket: &Socket) -> Result<()> {
    ensure!(
        matches!(socket.permissions.as_str(), "owner" | "readAndWrite"),
        "native project is not writable"
    );
    Ok(())
}
fn owner(project: &Value, account: &str) -> bool {
    project["owner"]["_id"] == account || project["owner"].as_str() == Some(account)
}
fn expected(args: &Map<String, Value>) -> Result<u64> {
    args.get("expected_version")
        .and_then(Value::as_u64)
        .filter(|v| *v < 9_007_199_254_740_991)
        .ok_or_else(|| anyhow!("exact expected_version is required"))
}
fn same_baseline(socket: &mut Socket, doc: &str, before: &Snapshot) -> Result<()> {
    let now = socket.refresh(doc)?;
    ensure!(
        now.version == before.version && now.text == before.text && !socket.concurrent,
        "native document changed; no further mutation submitted"
    );
    Ok(())
}
fn bool_arg(args: &Map<String, Value>, key: &str) -> Result<bool> {
    ensure!(args.get(key).is_none_or(Value::is_boolean), "{key} must be boolean");
    Ok(args.get(key) == Some(&Value::Bool(true)))
}
pub(super) fn tracking_state(project: &Value) -> Result<Map<String, Value>> {
    let state = project
        .get("track_changes_state")
        .or_else(|| project.get("trackChangesState"))
        .unwrap_or(&Value::Null);
    match state {
        Value::Object(map) => {
            ensure!(map.values().all(Value::is_boolean), "invalid native tracking state");
            Ok(map.clone())
        }
        Value::Null | Value::Bool(false) => Ok(Map::new()),
        Value::Bool(true) => {
            let mut map = Map::new();
            for user in std::iter::once(&project["owner"]).chain(project["members"].as_array().into_iter().flatten()) {
                if let Some(id) = user["_id"].as_str() {
                    map.insert(id.into(), json!(true));
                }
            }
            map.insert("__guests__".into(), json!(true));
            Ok(map)
        }
        _ => Err(anyhow!("invalid native tracking state")),
    }
}
pub(super) fn tracking_on(project: &Value, account: &str) -> Result<bool> {
    let state = tracking_state(project)?;
    Ok([account, "__guests__", "__everyone__", "all"]
        .iter()
        .any(|id| state.get(*id) == Some(&Value::Bool(true))))
}
fn toggle(client: &mut Client, session: &NativeSession, name: &str, args: &Map<String, Value>) -> Result<Value> {
    let socket = client.socket()?;
    writable(&socket)?;
    let on = args
        .get("on")
        .and_then(Value::as_bool)
        .ok_or_else(|| anyhow!("on must be boolean"))?;
    ensure!(
        !on || socket.project["features"]["trackChanges"] == true,
        "Track Changes is unavailable for this project"
    );
    let original = tracking_state(&socket.project)?;
    let mut next = original.clone();
    next.insert(client.account.clone(), json!(on));
    let mut on_for = next.clone();
    let guests = on_for.remove("__guests__");
    let mut body = json!({"on_for":on_for});
    if let Some(guests) = guests {
        body["on_for_guests"] = guests;
    }
    let details = json!({"operation":name,"account_id":client.account,"on":on});
    super::once(client, session, name, args, details, |client, receipt| {
        let fresh = client.socket()?;
        writable(&fresh)?;
        ensure!(
            tracking_state(&fresh.project)? == original,
            "tracking state changed; no toggle submitted"
        );
        client.json(
            "POST",
            &format!("/project/{}/track_changes", client.project),
            Some(body),
        )?;
        let after = client.socket()?;
        let state = tracking_state(&after.project)?;
        receipt.verified = state == next;
        receipt.details["track_changes_on_for_me"] = json!(tracking_on(&after.project, &client.account)?);
        receipt.details["other_users_preserved"] = json!(
            state
                .iter()
                .filter(|(id, _)| *id != &client.account)
                .eq(original.iter().filter(|(id, _)| *id != &client.account))
        );
        Ok(json!({"state":state}))
    })
}
fn preserved_threads(before: &Value, after: &Value, target: &str) -> bool {
    let Some(old) = before.as_object() else { return false };
    let Some(new) = after.as_object() else { return false };
    old.iter()
        .filter(|(id, _)| id.as_str() != target)
        .all(|(id, v)| new.get(id) == Some(v))
        && new.keys().filter(|id| id.as_str() != target).count()
            == old.keys().filter(|id| id.as_str() != target).count()
}
fn messages(thread: &Value) -> Result<&Vec<Value>> {
    let rows = thread["messages"]
        .as_array()
        .ok_or_else(|| anyhow!("complete native thread messages required"))?;
    let mut ids = BTreeSet::new();
    ensure!(
        rows.iter()
            .all(|row| row["id"].as_str().is_some_and(|id| ids.insert(id))),
        "ambiguous native message identity"
    );
    Ok(rows)
}
fn message_content(args: &Map<String, Value>, text: &str) -> Result<()> {
    ensure!(
        args.contains_key("expected_content") || args.contains_key("expected_content_sha256"),
        "expected_content or expected_content_sha256 is required"
    );
    if let Some(expected) = args.get("expected_content") {
        ensure!(expected.as_str() == Some(text), "original message content changed");
    }
    if let Some(hash) = args.get("expected_content_sha256") {
        ensure!(
            hash.as_str()
                .is_some_and(|h| h.len() == 64 && h.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))),
            "expected_content_sha256 must be lowercase SHA256"
        );
        ensure!(
            hash.as_str() == Some(digest(text).as_str()),
            "original message hash changed"
        );
    }
    Ok(())
}
fn own_message(message: &Value, account: &str, any: bool, is_owner: bool) -> Result<()> {
    ensure!(
        author(message) == Some(account) || (any && is_owner),
        "message is not authored by the logged-in user; only an explicit project-owner any_author request may target others"
    );
    Ok(())
}
fn comment(client: &mut Client, session: &NativeSession, name: &str, args: &Map<String, Value>) -> Result<Value> {
    let thread = need(args, "thread_id")?;
    id(thread)?;
    let original = client.threads()?;
    let current = original
        .get(thread)
        .ok_or_else(|| anyhow!("thread does not exist in this project"))?;
    let original_messages = messages(current)?;
    let any = bool_arg(args, "any_author")?;
    let mut socket = client.socket()?;
    writable(&socket)?;
    let is_owner = owner(&socket.project, &client.account);
    ensure!(!any || is_owner, "any_author requires the logged-in project owner");
    ensure!(
        name != "native_edit_comment_message" || !any,
        "editing messages is limited to the logged-in author; any_author applies only to deletion"
    );
    if matches!(name, "native_edit_comment_message" | "native_delete_comment_message") {
        let message = need(args, "message_id")?;
        ensure!(valid_message_id(message), "invalid message ID");
        let target = original_messages
            .iter()
            .find(|m| m["id"] == message)
            .ok_or_else(|| anyhow!("message does not exist in this thread"))?;
        own_message(target, &client.account, any, is_owner)?;
        let text = target["content"].as_str().unwrap();
        message_content(args, text)?;
        let new_content = if name == "native_edit_comment_message" {
            let content = need(args, "content")?;
            ensure!(
                !content.trim().is_empty() && content.len() <= 32768 && content != text,
                "replacement message must be 1–32768 bytes and differ from original"
            );
            ensure!(
                client.redact(content) == content,
                "message must not contain credentials"
            );
            Some(content)
        } else {
            ensure!(!args.contains_key("content"), "delete message does not accept content");
            None
        };
        return super::once(
            client,
            session,
            name,
            args,
            json!({"operation":name,"thread_id":thread,"message_id":message,"original_sha256":digest(text)}),
            |client, receipt| {
                ensure!(
                    client.threads()? == original,
                    "thread state changed; no message mutation submitted"
                );
                if let Some(content) = new_content {
                    client.json(
                        "POST",
                        &format!("/project/{}/thread/{thread}/messages/{message}/edit", client.project),
                        Some(json!({"content":content})),
                    )?;
                } else {
                    let route = if any { "messages" } else { "own-messages" };
                    client.json(
                        "DELETE",
                        &format!("/project/{}/thread/{thread}/{route}/{message}", client.project),
                        None,
                    )?;
                }
                let after = client.threads()?;
                let after_messages = after
                    .get(thread)
                    .map(messages)
                    .transpose()?
                    .cloned()
                    .unwrap_or_default();
                let preserved = original_messages
                    .iter()
                    .filter(|m| m["id"] != message)
                    .all(|old| after_messages.iter().any(|new| new == old));
                let target_verified = if let Some(content) = new_content {
                    after_messages
                        .iter()
                        .any(|m| m["id"] == message && m["content"] == content && author(m) == author(target))
                        && after_messages.len() == original_messages.len()
                } else {
                    after_messages.iter().all(|m| m["id"] != message)
                        && after_messages.len() + 1 == original_messages.len()
                };
                receipt.verified = preserved_threads(&original, &after, thread) && preserved && target_verified;
                receipt.details["unrelated_messages_preserved"] = json!(preserved);
                Ok(json!({}))
            },
        );
    }
    let docs = edits::documents(&socket.project)?;
    let all_ranges = edits::ranges(client)?;
    let anchored: BTreeSet<_> = all_ranges
        .iter()
        .filter(|row| {
            row["ranges"]["comments"]
                .as_array()
                .is_some_and(|comments| comments.iter().any(|c| c["op"]["t"] == thread))
        })
        .filter_map(|row| row["id"].as_str())
        .collect();
    ensure!(anchored.len() <= 1, "thread has ambiguous document anchors");
    let fallback = socket.project["rootDoc_id"]
        .as_str()
        .or_else(|| socket.project["rootDocId"].as_str())
        .filter(|root| docs.values().any(|id| id == root))
        .or_else(|| docs.values().next().map(String::as_str));
    let addressed = anchored
        .iter()
        .next()
        .copied()
        .or(fallback)
        .ok_or_else(|| anyhow!("thread has no addressable document"))?;
    let doc = args
        .get("doc_id")
        .map(|v| v.as_str().ok_or_else(|| anyhow!("doc_id must be text")))
        .transpose()?
        .unwrap_or(addressed);
    let doc = doc.to_owned();
    id(&doc)?;
    ensure!(
        docs.values().any(|id| id == &doc) && (anchored.is_empty() || anchored.contains(doc.as_str())),
        "doc_id does not match the thread anchor in this project"
    );
    let before = socket.join(&doc)?;
    ensure!(
        before.version == expected(args)? && !socket.concurrent,
        "thread document version changed"
    );
    let deleting = name == "native_delete_comment";
    let resolved = current["resolved"]
        .as_bool()
        .ok_or_else(|| anyhow!("thread resolved state missing"))?;
    if let Some(expected) = args.get("expected_resolved") {
        ensure!(expected.as_bool() == Some(resolved), "thread resolved state changed");
    }
    if deleting {
        let wanted = args
            .get("expected_messages")
            .and_then(Value::as_object)
            .ok_or_else(|| anyhow!("expected_messages must contain all original message bodies"))?;
        ensure!(
            wanted.len() == original_messages.len()
                && original_messages
                    .iter()
                    .all(|m| wanted.get(m["id"].as_str().unwrap()) == Some(&m["content"])),
            "thread message bodies changed or expected_messages is incomplete"
        );
        for message in original_messages {
            own_message(message, &client.account, any, is_owner)?;
        }
    } else {
        ensure!(
            matches!(name, "native_resolve_comment" | "native_reopen_comment"),
            "unknown comment mutation"
        );
        ensure!(
            resolved != (name == "native_resolve_comment"),
            "thread is already in requested state; no mutation needed"
        );
    }
    super::once(
        client,
        session,
        name,
        args,
        json!({"operation":name,"thread_id":thread,"doc_id":doc,"version_before":before.version}),
        |client, receipt| {
            same_baseline(&mut socket, &doc, &before)?;
            ensure!(
                client.threads()? == original,
                "thread state changed; no thread mutation submitted"
            );
            ensure!(
                edits::ranges(client)? == all_ranges,
                "anchor or pending ranges changed; no thread mutation submitted"
            );
            socket.leave()?;
            ensure!(
                !socket.concurrent,
                "native project changed; no thread mutation submitted"
            );
            let suffix = if deleting {
                String::new()
            } else {
                format!(
                    "/{}",
                    if name == "native_resolve_comment" {
                        "resolve"
                    } else {
                        "reopen"
                    }
                )
            };
            client.json(
                if deleting { "DELETE" } else { "POST" },
                &format!("/project/{}/doc/{doc}/thread/{thread}{suffix}", client.project),
                (!deleting).then(|| json!({})),
            )?;
            let after = client.threads()?;
            let fresh = socket.refresh(&doc)?;
            let desired = if deleting {
                after.get(thread).is_none()
            } else {
                after.get(thread).is_some_and(|t| {
                    t["resolved"] == (name == "native_resolve_comment") && t["messages"] == current["messages"]
                })
            };
            let after_ranges = edits::ranges(client)?;
            let remaining_anchors = after_ranges
                .iter()
                .filter_map(|r| r["ranges"]["comments"].as_array())
                .flatten()
                .any(|c| c["op"]["t"] == thread);
            let changes_before: Vec<_> = all_ranges.iter().map(|r| (&r["id"], &r["ranges"]["changes"])).collect();
            let changes_after: Vec<_> = after_ranges
                .iter()
                .map(|r| (&r["id"], &r["ranges"]["changes"]))
                .collect();
            receipt.verified = desired
                && preserved_threads(&original, &after, thread)
                && (!deleting || !remaining_anchors)
                && changes_before == changes_after
                && fresh.text == before.text
                && (before.version..=before.version + 1).contains(&fresh.version)
                && !socket.concurrent;
            receipt.details["version_after"] = json!(fresh.version);
            Ok(json!({}))
        },
    )
}
fn select(all: &[Value], args: &Map<String, Value>) -> Result<Vec<Value>> {
    let filtered = [
        "path",
        "path_contains",
        "author_email",
        "author_id_endswith",
        "text_contains",
    ]
    .iter()
    .any(|key| args.contains_key(*key))
        || args.get("kind").is_some_and(|v| v != "any");
    let every = bool_arg(args, "all")?;
    if let Some(ids) = args.get("change_ids") {
        ensure!(!filtered && !every, "use change_ids or filters/all, never both");
        let ids = ids
            .as_array()
            .filter(|v| !v.is_empty() && v.len() <= 4096)
            .ok_or_else(|| anyhow!("change_ids must be a nonempty bounded array"))?;
        let mut wanted = BTreeSet::new();
        for id_value in ids {
            let change = id_value
                .as_str()
                .ok_or_else(|| anyhow!("change_ids must contain IDs"))?;
            id(change)?;
            ensure!(wanted.insert(change), "duplicate selected change ID");
        }
        let selected: Vec<_> = all
            .iter()
            .filter(|v| wanted.contains(v["change_id"].as_str().unwrap()))
            .cloned()
            .collect();
        ensure!(
            selected.len() == wanted.len(),
            "one or more selected changes are absent; no mutation submitted"
        );
        return Ok(selected);
    }
    ensure!(filtered || every, "explicit change_ids, filter or all=true is required");
    let mut selected = Vec::new();
    for row in all {
        if edits::matches_filter(row, args)? {
            selected.push(row.clone());
        }
    }
    ensure!(!selected.is_empty(), "no pending changes match the explicit selection");
    Ok(selected)
}
/// Convert an OT UTF16 position to a byte offset, rejecting split surrogate pairs.
fn byte_at(text: &str, position: u64) -> Result<usize> {
    let mut units = 0;
    for (offset, c) in text.char_indices() {
        if units == position {
            return Ok(offset);
        }
        units += c.len_utf16() as u64;
        ensure!(units <= position, "tracked change splits a UTF16 surrogate pair");
    }
    ensure!(units == position, "tracked change position is outside current text");
    Ok(text.len())
}
struct Inverse {
    ops: Vec<Value>,
    text: String,
    unrelated: Vec<Value>,
}
fn inverse(before: &str, selected: &[Value], others: &[Value]) -> Result<Inverse> {
    let mut ordered = selected.to_vec();
    ordered.sort_by(|a, b| {
        b["position"]
            .as_u64()
            .cmp(&a["position"].as_u64())
            .then_with(|| (b["kind"] == "insert").cmp(&(a["kind"] == "insert")))
    });
    let mut text = before.to_owned();
    let mut unrelated = others.to_vec();
    let mut ops = Vec::new();
    let mut last_delete_start = None;
    for change in ordered {
        let position = change["position"]
            .as_u64()
            .ok_or_else(|| anyhow!("invalid pending change position"))?;
        let content = change["text"]
            .as_str()
            .ok_or_else(|| anyhow!("invalid pending change text"))?;
        let length = content.encode_utf16().count() as u64;
        let offset = byte_at(&text, position)?;
        let removing = change["kind"] == "insert";
        if removing {
            if let Some(last) = last_delete_start {
                ensure!(
                    position + length <= last,
                    "selected tracked inserts overlap; cannot reject safely"
                );
            }
            let end = byte_at(&text, position + length)?;
            ensure!(
                &text[offset..end] == content,
                "pending insert text does not match current document"
            );
            for row in &unrelated {
                let p = row["position"].as_u64().unwrap();
                let end = p + if row["kind"] == "insert" {
                    row["text"].as_str().unwrap().encode_utf16().count() as u64
                } else {
                    0
                };
                ensure!(
                    end < position || p > position + length,
                    "rejection touches an unrelated pending change"
                );
            }
            text.replace_range(offset..end, "");
            ops.push(json!({"p":position,"d":content,"u":true}));
            last_delete_start = Some(position);
        } else {
            for row in &unrelated {
                let p = row["position"].as_u64().unwrap();
                let end = p + if row["kind"] == "insert" {
                    row["text"].as_str().unwrap().encode_utf16().count() as u64
                } else {
                    0
                };
                ensure!(
                    position < p || position > end,
                    "rejection touches an unrelated pending change"
                );
            }
            text.insert_str(offset, content);
            ops.push(json!({"p":position,"i":content,"u":true}));
        }
        for row in &mut unrelated {
            let p = row["position"].as_u64().unwrap();
            if p >= position {
                let next = if removing {
                    p.checked_sub(length)
                        .ok_or_else(|| anyhow!("invalid inverse position shift"))?
                } else {
                    p.checked_add(length)
                        .ok_or_else(|| anyhow!("inverse position overflow"))?
                };
                row["position"] = json!(next);
                row["op"]["p"] = json!(next);
            }
        }
        ensure!(
            text.len() <= super::LIMIT as usize,
            "rejected document exceeds native size limit"
        );
    }
    Ok(Inverse { ops, text, unrelated })
}
fn same_pending(a: &[Value], b: &[Value]) -> bool {
    a.len() == b.len()
        && a.iter().all(|old| {
            b.iter().any(|new| {
                old["change_id"] == new["change_id"]
                    && old["doc_id"] == new["doc_id"]
                    && old["op"] == new["op"]
                    && old["metadata"] == new["metadata"]
            })
        })
}
fn review(client: &mut Client, session: &NativeSession, name: &str, args: &Map<String, Value>) -> Result<Value> {
    let mut socket = client.socket()?;
    writable(&socket)?;
    let original = edits::inventory(client, &socket.project)?;
    let selected = select(&original, args)?;
    ensure!(selected.len() <= 4096, "selection exceeds native review limit");
    let selected_ids: BTreeSet<_> = selected
        .iter()
        .map(|v| v["change_id"].as_str().unwrap().to_owned())
        .collect();
    let mut grouped: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for row in &selected {
        grouped
            .entry(row["doc_id"].as_str().unwrap().into())
            .or_default()
            .push(row.clone());
    }
    let versions = args
        .get("expected_versions")
        .map(|v| {
            v.as_object()
                .ok_or_else(|| anyhow!("expected_versions must be an object"))
        })
        .transpose()?;
    ensure!(
        args.get("expected_version").is_none() || (grouped.len() == 1 && versions.is_none()),
        "single expected_version requires exactly one selected document and no map"
    );
    if let Some(versions) = versions {
        ensure!(
            versions
                .values()
                .all(|v| v.as_u64().is_some_and(|v| v < 9_007_199_254_740_991)),
            "invalid expected document version"
        );
        for key in versions.keys() {
            ensure!(
                grouped.keys().any(|doc| doc == key) || selected.iter().any(|row| row["path"] == key.as_str()),
                "expected_versions includes an unselected document"
            );
        }
    }
    let mut baselines = BTreeMap::new();
    for (doc, rows) in &grouped {
        let version = if let Some(versions) = versions {
            let by_id = versions.get(doc);
            let by_path = versions.get(rows[0]["path"].as_str().unwrap());
            ensure!(
                by_id.is_none() || by_path.is_none() || by_id == by_path,
                "conflicting expected versions for one document"
            );
            by_id
                .or(by_path)
                .and_then(Value::as_u64)
                .ok_or_else(|| anyhow!("expected_versions missing a selected document"))?
        } else {
            expected(args)?
        };
        socket.leave()?;
        let snap = socket.join(doc)?;
        ensure!(
            snap.version == version && !socket.concurrent,
            "selected document version changed; no review mutation submitted"
        );
        // Validate every inverse before starting a multi-document operation.
        if name == "native_reject_changes" {
            let others: Vec<_> = original
                .iter()
                .filter(|row| {
                    row["doc_id"] == doc.as_str() && !selected_ids.contains(row["change_id"].as_str().unwrap())
                })
                .cloned()
                .collect();
            inverse(&snap.text, rows, &others)?;
        }
        baselines.insert(doc.clone(), snap);
    }
    let seed = digest(need(args, "operation_id")?);
    super::once(
        client,
        session,
        name,
        args,
        json!({"operation":name,"selected_change_ids":selected_ids,"completed_doc_ids":[],"selected_count":selected.len()}),
        |client, receipt| {
            let mut current = original.clone();
            let mut completed = Vec::new();
            for (doc, rows) in &grouped {
                socket.leave()?;
                let baseline = &baselines[doc];
                let fresh = socket.join(doc)?;
                ensure!(
                    fresh.version == baseline.version && fresh.text == baseline.text && !socket.concurrent,
                    "review document changed; no further review mutation submitted"
                );
                ensure!(
                    same_pending(&edits::inventory(client, &socket.project)?, &current),
                    "pending inventory changed; no further review mutation submitted"
                );
                let ids: BTreeSet<_> = rows.iter().map(|v| v["change_id"].as_str().unwrap()).collect();
                let others: Vec<_> = current
                    .iter()
                    .filter(|v| v["doc_id"] == doc.as_str() && !ids.contains(v["change_id"].as_str().unwrap()))
                    .cloned()
                    .collect();
                let rejecting = name == "native_reject_changes";
                let planned = if rejecting {
                    Some(inverse(&baseline.text, rows, &others)?)
                } else {
                    None
                };
                same_baseline(&mut socket, doc, baseline)?;
                if let Some(inverse) = &planned {
                    socket.apply(doc, json!(inverse.ops), baseline.version, Some(&seed[..18]))?;
                } else {
                    socket.leave()?;
                    ensure!(!socket.concurrent, "native project changed; no acceptance submitted");
                    client.json(
                        "POST",
                        &format!("/project/{}/doc/{doc}/changes/accept", client.project),
                        Some(json!({"change_ids":ids})),
                    )?;
                }
                let after = socket.refresh(doc)?;
                let inventory = edits::inventory(client, &socket.project)?;
                let mut wanted: Vec<_> = current
                    .iter()
                    .filter(|v| v["doc_id"] != doc.as_str())
                    .cloned()
                    .collect();
                wanted.extend(planned.as_ref().map(|v| v.unrelated.clone()).unwrap_or(others));
                let expected_text = planned
                    .as_ref()
                    .map(|v| v.text.as_str())
                    .unwrap_or(baseline.text.as_str());
                let version_ok = if rejecting {
                    after.version == baseline.version + 1
                } else {
                    (baseline.version..=baseline.version + 1).contains(&after.version)
                };
                ensure!(
                    version_ok
                        && after.text == expected_text
                        && !socket.concurrent
                        && same_pending(&wanted, &inventory)
                        && inventory
                            .iter()
                            .all(|v| !ids.contains(v["change_id"].as_str().unwrap())),
                    "review mutation readback incomplete; no further mutation submitted"
                );
                let final_read = socket.refresh(doc)?;
                ensure!(
                    final_read.version == after.version && final_read.text == after.text && !socket.concurrent,
                    "review readback changed; no further mutation submitted"
                );
                completed.push(doc.clone());
                receipt.details["completed_doc_ids"] = json!(completed);
                current = inventory;
            }
            let final_inventory = edits::inventory(client, &socket.project)?;
            receipt.verified = same_pending(&current, &final_inventory)
                && final_inventory
                    .iter()
                    .all(|v| !selected_ids.contains(v["change_id"].as_str().unwrap()));
            receipt.details["unrelated_pending_preserved"] = json!(same_pending(&current, &final_inventory));
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
    match name {
        "native_set_track_changes" => toggle(client, session, name, args),
        "native_accept_changes" | "native_reject_changes" => review(client, session, name, args),
        _ => comment(client, session, name, args),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn change(id: &str, p: u64, text: &str, kind: &str) -> Value {
        json!({"change_id":id,"doc_id":"111111111111111111111111","path":"main.tex","position":p,"kind":kind,"text":text,
            "op":if kind=="insert" {json!({"p":p,"i":text})} else {json!({"p":p,"d":text})},"metadata":{"user_id":"author"}})
    }
    #[test]
    fn review_selection_and_utf16_inverse_preserve_unrelated_changes() {
        let selected = vec![
            change("222222222222222222222222", 3, "new", "insert"),
            change("333333333333333333333333", 3, "old", "delete"),
        ];
        let others = vec![change("444444444444444444444444", 8, "tail", "insert")];
        let undo = inverse("😀 new! tail", &selected, &others).unwrap();
        assert_eq!(undo.text, "😀 old! tail");
        assert_eq!(
            undo.ops,
            vec![json!({"p":3,"d":"new","u":true}), json!({"p":3,"i":"old","u":true})]
        );
        assert_eq!(undo.unrelated, others);
        assert!(byte_at("😀x", 1).is_err());
        assert!(
            inverse(
                "😀 new! tail",
                &selected,
                &[change("444444444444444444444444", 4, "e", "insert")]
            )
            .is_err()
        );
        let args = json!({"change_ids":["222222222222222222222222"],"all":true});
        assert!(select(&selected, args.as_object().unwrap()).is_err());
        assert!(select(&selected, json!({}).as_object().unwrap()).is_err());
        assert!(
            select(
                &selected,
                json!({"change_ids":["555555555555555555555555"]}).as_object().unwrap()
            )
            .is_err()
        );
        assert_eq!(
            select(&selected, json!({"kind":"delete"}).as_object().unwrap())
                .unwrap()
                .len(),
            1
        );
    }
    #[test]
    fn message_originals_and_tracking_map_are_exact() {
        assert!(message_content(json!({}).as_object().unwrap(), "original").is_err());
        assert!(message_content(json!({"expected_content":"different"}).as_object().unwrap(), "original").is_err());
        assert!(
            message_content(
                json!({"expected_content_sha256":digest("original")})
                    .as_object()
                    .unwrap(),
                "original"
            )
            .is_ok()
        );
        let project = json!({"trackChangesState":{"me":false,"other":true,"__guests__":false}});
        assert!(!tracking_on(&project, "me").unwrap());
        let state = tracking_state(&project).unwrap();
        assert_eq!(state["other"], true);
        assert_eq!(state["__guests__"], false);
        assert!(own_message(&json!({"user_id":"other"}), "me", true, false).is_err());
        assert!(own_message(&json!({"user_id":"other"}), "me", false, true).is_err());
        assert!(own_message(&json!({"user_id":"other"}), "me", true, true).is_ok());
    }
}
