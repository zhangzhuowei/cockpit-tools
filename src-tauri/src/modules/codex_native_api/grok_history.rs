//! Project Codex history onto xAI's Responses input without changing the
//! caller's saved transcript or treating opaque content as decrypted text.
use super::{invalid, ServiceError};
use serde_json::{json, Map, Value};

#[path = "grok_arguments.rs"]
mod arguments;

pub(super) fn prepare(body: &mut Value) -> Result<(), ServiceError> {
    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return Ok(());
    };
    for item in items {
        match item.get("type").and_then(Value::as_str) {
            Some("agent_message") => agent_message(item)?,
            Some("web_search_call") => search_action(item),
            Some("reasoning") => {
                if item.get("content") == Some(&Value::Null) {
                    item.as_object_mut().unwrap().remove("content");
                }
            }
            Some("function_call") => {
                if item["arguments"]
                    .as_str()
                    .is_some_and(|raw| raw.trim().is_empty())
                {
                    item["arguments"] = json!("{}");
                } else {
                    normalize_arguments(item);
                }
            }
            // This is an alias in the official client's ResponseItem enum.
            Some("compaction_summary") => item["type"] = json!("compaction"),
            _ => {}
        }
    }
    Ok(())
}

pub(super) fn normalize_arguments(item: &mut Value) {
    if let Some(raw) = item.get("arguments").and_then(Value::as_str) {
        let normalized = arguments::normalize(raw);
        if normalized != raw {
            item["arguments"] = json!(normalized);
        }
    }
}

/// Cached checkpoints retain the full source history for local continuation.
/// On the Grok wire, a valid compact item replaces the prefix it summarizes.
pub(super) fn compact_prefix(body: &mut Value) {
    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };
    let last = items.iter().rposition(|item| {
        item["type"] == "compaction"
            && item["encrypted_content"]
                .as_str()
                .is_some_and(super::encrypted::valid)
    });
    if let Some(index) = last {
        items.drain(..index);
    }
}

pub(super) fn merge_summaries(body: &mut Value) {
    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };
    let mut merged: Vec<Value> = Vec::with_capacity(items.len());
    for item in std::mem::take(items) {
        let summary_only = item["type"] == "reasoning"
            && item.as_object().is_some_and(|object| {
                object
                    .keys()
                    .all(|key| matches!(key.as_str(), "type" | "summary"))
            })
            && item["summary"]
                .as_array()
                .is_some_and(|summary| !summary.is_empty());
        if summary_only {
            if let Some(previous) = merged
                .last_mut()
                .filter(|previous| previous["type"] == "reasoning")
            {
                if let Some(summary) = previous.get_mut("summary").and_then(Value::as_array_mut) {
                    summary.extend(item["summary"].as_array().unwrap().iter().cloned());
                    continue;
                }
            }
        }
        merged.push(item);
    }
    *items = merged;
}

fn agent_message(item: &mut Value) -> Result<(), ServiceError> {
    let Some(parts) = item.get_mut("content").and_then(Value::as_array_mut) else {
        return Err(invalid("Agent message content must be an array"));
    };
    // Match CPA's adapter for legacy third-party messages. The literal bytes
    // remain available to the model; this is not decryption or ownership proof.
    // Newly generated Grok collaboration calls explicitly select plaintext at
    // the response boundary, so the client normally supplies input_text here.
    for part in parts.iter_mut() {
        if part.get("type").and_then(Value::as_str) == Some("encrypted_content") {
            let content = part
                .get("encrypted_content")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("Agent message encrypted content must be text"))?
                .to_owned();
            part["type"] = json!("input_text");
            part["text"] = json!(content);
            part.as_object_mut().unwrap().remove("encrypted_content");
        }
    }
    let object = item.as_object_mut().unwrap();
    let mut metadata = Map::new();
    for key in [
        "author",
        "recipient",
        "id",
        "internal_chat_message_metadata_passthrough",
    ] {
        if let Some(value) = object.remove(key) {
            metadata.insert(key.to_owned(), value);
        }
    }
    if !metadata.is_empty() {
        object.get_mut("content").unwrap().as_array_mut().unwrap().push(json!({
            "type":"input_text", "text":format!("Agent routing metadata: {}", Value::Object(metadata))
        }));
    }
    object.insert("type".into(), json!("message"));
    object.insert("role".into(), json!("user"));
    Ok(())
}

fn search_action(item: &mut Value) {
    let Some(action) = item.get_mut("action").and_then(Value::as_object_mut) else {
        return;
    };
    if action.get("type").and_then(Value::as_str) != Some("search") {
        return;
    }
    let query = action
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    let queries: Vec<String> = action
        .get("queries")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|query| !query.is_empty())
        .map(str::to_owned)
        .collect();
    if queries.is_empty() && !query.is_empty() {
        action.insert("queries".into(), json!([query]));
    } else if query.is_empty() && !queries.is_empty() {
        action.insert("query".into(), json!(queries[0]));
    }
}

#[cfg(test)]
#[path = "grok_history_tests.rs"]
mod tests;
