//! Official Codex request compatibility, separate from identity rewriting and routing.
use axum::http::HeaderMap;
use serde_json::{json, Value};

use super::contract::ServiceError;

#[path = "codex_wire_calls.rs"]
mod calls;
#[path = "codex_wire_lite.rs"]
mod lite;
#[path = "codex_wire_schema.rs"]
mod schema;

pub(super) const LITE_HEADER: &str = "x-openai-internal-codex-responses-lite";

/// Run after the host's image policy and identity mapping, for the selected route.
/// Third-party API-key payloads retain their generation settings and tool schemas.
pub(super) fn normalize(
    headers: &mut HeaderMap,
    body: &mut Value,
    chatgpt: bool,
    compact: bool,
) -> Result<(), ServiceError> {
    normalize_inner(headers, body, chatgpt, compact, false)
}

/// WebSocket preserves Go's parallel-tool default even when no tools are declared.
pub(super) fn normalize_websocket(
    headers: &mut HeaderMap,
    body: &mut Value,
    chatgpt: bool,
) -> Result<(), ServiceError> {
    normalize_inner(headers, body, chatgpt, false, true)
}

fn normalize_inner(
    headers: &mut HeaderMap,
    body: &mut Value,
    chatgpt: bool,
    compact: bool,
    websocket: bool,
) -> Result<(), ServiceError> {
    let streaming = body.get("stream").and_then(Value::as_bool) == Some(true);
    let Some(object) = body.as_object_mut() else {
        return Err(ServiceError::new(
            400,
            "invalid_json",
            "Request body must be a JSON object",
        ));
    };
    if compact {
        object.remove("stream");
    }
    if !chatgpt {
        headers.remove(LITE_HEADER);
        return Ok(());
    }
    if !compact {
        if let Some(Value::String(text)) = object.get("input") {
            object.insert("input".into(), json!([{"type":"message","role":"user","content":[{"type":"input_text","text":text}]}]));
        }
        object.insert("stream".into(), json!(true));
        object.insert("store".into(), json!(false));
        object.insert("parallel_tool_calls".into(), json!(true));
        let sources = object
            .get("include")
            .and_then(Value::as_array)
            .is_some_and(|items| {
                items
                    .iter()
                    .any(|item| item == "web_search_call.action.sources")
            });
        object.insert(
            "include".into(),
            if sources {
                json!([
                    "reasoning.encrypted_content",
                    "web_search_call.action.sources"
                ])
            } else {
                json!(["reasoning.encrypted_content"])
            },
        );
        for field in [
            "max_output_tokens",
            "max_completion_tokens",
            "temperature",
            "top_p",
            "truncation",
            "prompt_cache_options",
            "prompt_cache_retention",
            "context_management",
            "user",
            "safety_identifier",
        ] {
            object.remove(field);
        }
        if !websocket {
            object.remove("generate");
            let delivery = streaming
                .then(|| {
                    object
                        .get("stream_options")?
                        .get("reasoning_summary_delivery")
                        .cloned()
                })
                .flatten();
            object.remove("stream_options");
            if let Some(delivery) = delivery {
                object.insert(
                    "stream_options".into(),
                    json!({"reasoning_summary_delivery": delivery}),
                );
            }
        }
        if let Some(tier) = object.get("service_tier") {
            let tier = tier.as_str().map(|tier| tier.trim().to_ascii_lowercase());
            match tier.as_deref() {
                Some("priority" | "fast") => {
                    object.insert("service_tier".into(), json!("priority"));
                }
                Some("ultrafast") => {
                    object.insert("service_tier".into(), json!("ultrafast"));
                }
                _ => {
                    object.remove("service_tier");
                }
            }
        }
        if let Some(items) = object.get_mut("input").and_then(Value::as_array_mut) {
            for item in items {
                if item["role"] == "system" {
                    item["role"] = json!("developer");
                }
                if let Some(parts) = item.get_mut("content").and_then(Value::as_array_mut) {
                    for part in parts {
                        if let Some(part) = part.as_object_mut() {
                            part.remove("prompt_cache_breakpoint");
                        }
                    }
                }
            }
        }
        normalize_builtin_tools(body);
    }
    calls::normalize(body);
    let object = body.as_object_mut().expect("Object checked above");
    if object.get("instructions").is_none_or(|value| {
        value.is_null() || value.as_str().is_some_and(|text| text.trim().is_empty())
    }) {
        object.insert("instructions".into(), json!(""));
    }
    let metadata_lite = lite::metadata_enabled(body);
    let header_lite = headers.contains_key(LITE_HEADER);
    let explicit_lite = headers
        .get(LITE_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
        || metadata_lite;
    if !websocket {
        if explicit_lite {
            body["parallel_tool_calls"] = json!(false);
        } else if !body
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| !tools.is_empty())
        {
            body.as_object_mut().unwrap().remove("parallel_tool_calls");
        }
        schema::normalize_tools(body);
    }
    if !compact && (header_lite || metadata_lite) {
        body["parallel_tool_calls"] = json!(false);
        if lite::uses_image(body) {
            // Go promotes explicit image requests to full Responses rather than discarding them.
            headers.remove(LITE_HEADER);
        } else {
            lite::filter(body);
            if metadata_lite && !header_lite {
                headers.insert(LITE_HEADER, axum::http::HeaderValue::from_static("true"));
            }
        }
    }
    if websocket {
        if explicit_lite {
            body["parallel_tool_calls"] = json!(false);
        }
        schema::normalize_tools(body);
    }
    Ok(())
}

fn normalize_builtin_tools(body: &mut Value) {
    fn tool(tool: &mut Value) {
        if matches!(
            tool["type"].as_str(),
            Some("web_search_preview" | "web_search_preview_2025_03_11")
        ) {
            tool["type"] = json!("web_search");
        }
    }
    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        for entry in tools {
            tool(entry);
        }
    }
    if let Some(choice) = body.get_mut("tool_choice") {
        tool(choice);
        if let Some(tools) = choice.get_mut("tools").and_then(Value::as_array_mut) {
            for entry in tools {
                tool(entry);
            }
        }
    }
}

#[cfg(test)]
#[path = "codex_wire_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "codex_wire_runtime_parity_tests.rs"]
mod runtime_parity_tests;
