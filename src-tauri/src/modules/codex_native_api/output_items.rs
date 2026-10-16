//! Restore terminal output only from complete items observed in this response.
use std::collections::BTreeMap;

use serde_json::Value;

use super::contract::ServiceError;

pub(super) struct OutputCollector {
    limit: usize,
    indexed: BTreeMap<u64, (Value, usize)>,
    unindexed: Vec<Value>,
    bytes: usize,
}

impl OutputCollector {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            limit,
            indexed: BTreeMap::new(),
            unindexed: Vec::new(),
            bytes: 0,
        }
    }

    pub(super) fn observe(&mut self, event: &mut Value) -> Result<bool, ServiceError> {
        match event.get("type").and_then(Value::as_str) {
            Some("response.output_item.done") => {
                let Some(item) = event.get("item").filter(|item| {
                    item.is_object()
                        && item
                            .get("type")
                            .and_then(Value::as_str)
                            .is_some_and(|v| !v.is_empty())
                }) else {
                    return Ok(false);
                };
                let size = item.to_string().len();
                let index = event.get("output_index").and_then(Value::as_u64);
                let replaced = index
                    .and_then(|index| self.indexed.get(&index))
                    .map_or(0, |(_, size)| *size);
                let bytes = self.bytes.saturating_sub(replaced).saturating_add(size);
                if bytes > self.limit {
                    return Err(too_large());
                }
                self.bytes = bytes;
                if let Some(index) = index {
                    self.indexed.insert(index, (item.clone(), size));
                } else {
                    self.unindexed.push(item.clone());
                }
                Ok(false)
            }
            Some("response.completed" | "response.done" | "response.incomplete") => {
                let reconcile_calls = event["type"] != "response.incomplete";
                let Some(response) = event.get_mut("response").and_then(Value::as_object_mut)
                else {
                    return Ok(false);
                };
                let collected = || {
                    self.indexed
                        .values()
                        .map(|(item, _)| item)
                        .chain(self.unindexed.iter())
                };
                let changed = match response.get_mut("output") {
                    Some(Value::Array(output)) if !output.is_empty() => {
                        let calls: BTreeMap<_, _> = collected()
                            .filter(|item| complete_tool_call(item))
                            .map(|item| (item["call_id"].as_str().unwrap().trim(), item))
                            .collect();
                        let mut changed = false;
                        for (index, item) in output.iter_mut().enumerate() {
                            let missing_id = item.get("id").is_none_or(|id| {
                                id.is_null() || id.as_str().is_some_and(|id| id.trim().is_empty())
                            });
                            if missing_id {
                                if let Some(id) = self
                                    .indexed
                                    .get(&(index as u64))
                                    .and_then(|(item, _)| item.get("id"))
                                    .and_then(Value::as_str)
                                    .filter(|id| !id.trim().is_empty())
                                {
                                    if let Some(object) = item.as_object_mut() {
                                        object.insert("id".into(), Value::String(id.into()));
                                        changed = true;
                                    }
                                }
                            }
                            if reconcile_calls && is_tool_call(item) {
                                if let Some(call) =
                                    item["call_id"].as_str().and_then(|id| calls.get(id.trim()))
                                {
                                    if &*item != *call {
                                        *item = (*call).clone();
                                        changed = true;
                                    }
                                }
                            }
                        }
                        changed
                    }
                    _ if !self.indexed.is_empty() || !self.unindexed.is_empty() => {
                        let output = collected()
                            .filter(|item| {
                                !reconcile_calls || !is_tool_call(item) || complete_tool_call(item)
                            })
                            .cloned()
                            .collect();
                        let restored = Value::Array(output);
                        let changed = response.get("output") != Some(&restored);
                        response.insert("output".into(), restored);
                        changed
                    }
                    _ => false,
                };
                if changed && event.to_string().len() > self.limit {
                    return Err(too_large());
                }
                Ok(changed)
            }
            _ => Ok(false),
        }
    }
}

fn is_tool_call(item: &Value) -> bool {
    matches!(
        item.get("type").and_then(Value::as_str),
        Some("function_call" | "custom_tool_call")
    )
}

pub(super) fn complete_tool_call(item: &Value) -> bool {
    if !["call_id", "name"].into_iter().all(|field| {
        item.get(field)
            .and_then(Value::as_str)
            .is_some_and(|value| !value.trim().is_empty())
    }) {
        return false;
    }
    let field = match item.get("type").and_then(Value::as_str) {
        Some("function_call") => "arguments",
        Some("custom_tool_call") => "input",
        _ => return false,
    };
    item.get(field).is_some_and(Value::is_string)
}

fn too_large() -> ServiceError {
    ServiceError::new(
        502,
        "upstream_event_too_large",
        "Upstream event exceeds the configured size limit",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn completion_restores_sorted_output_without_incomplete_tools_or_losing_ciphertext() {
        for kind in ["response.completed", "response.done"] {
            let mut collector = OutputCollector::new(4096);
            let reasoning = json!({"type":"reasoning","encrypted_content":"cipher","summary":[]});
            let custom = json!({"type":"custom_tool_call","call_id":"c","name":"exec","input":"pwd","extension":true});
            for mut event in [
                json!({"type":"response.output_item.done","output_index":2,"item":custom}),
                json!({"type":"response.output_item.done","output_index":0,"item":reasoning}),
                json!({"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","call_id":"incomplete","name":"exec"}}),
                json!({"type":"response.output_item.done","item":{"type":"message","content":[]}}),
            ] {
                assert!(!collector.observe(&mut event).unwrap());
            }
            let mut terminal = json!({"type":kind,"extension":true,"response":{"id":"r","status":"completed","output":[],"usage":{"input_tokens":3,"output_tokens":5}}});
            assert!(collector.observe(&mut terminal).unwrap());
            assert_eq!(
                terminal["response"]["output"],
                json!([reasoning,custom,{"type":"message","content":[]}])
            );
            assert_eq!(terminal["response"]["usage"]["output_tokens"], 5);
            assert_eq!(terminal["extension"], true);
        }
    }

    #[test]
    fn completion_reconciles_tool_by_call_id_without_replacing_unrelated_output() {
        let mut collector = OutputCollector::new(4096);
        let custom = json!({"type":"custom_tool_call","call_id":"c","name":"exec","input":"pwd","encrypted_function_args":["input"]});
        collector
            .observe(&mut json!({"type":"response.output_item.done","item":custom}))
            .unwrap();
        let message = json!({"type":"message","id":"m","content":[]});
        let other =
            json!({"type":"function_call","call_id":"other","name":"exec","arguments":"{}"});
        let mut terminal = json!({"type":"response.completed","response":{"status":"completed","id":"r","output":[message,{"type":"function_call","call_id":"c","name":"exec"},other]}});
        assert!(collector.observe(&mut terminal).unwrap());
        assert_eq!(
            terminal["response"]["output"],
            json!([message, custom, other])
        );
    }

    #[test]
    fn failed_output_and_unknown_fields_are_preserved_without_inventing_success() {
        let mut collector = OutputCollector::new(4096);
        collector.observe(&mut json!({"type":"response.output_item.done","item":{"type":"function_call","call_id":"c","name":"tool"}})).unwrap();
        for mut event in [
            json!({"type":"response.failed","response":{"status":"failed","output":[]},"extension":true}),
            json!({"type":"response.cancelled","response":{"status":"cancelled","output":[]}}),
            json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"custom_tool_call","call_id":"c","name":"exec","input":"real"}]}}),
        ] {
            let original = event.clone();
            assert!(!collector.observe(&mut event).unwrap());
            assert_eq!(event, original);
        }
    }

    #[test]
    fn collector_bounds_aggregate_output_and_accounts_for_replaced_indexes() {
        let mut collector = OutputCollector::new(160);
        let mut event = json!({"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","encrypted_content":"x".repeat(64)}});
        collector.observe(&mut event).unwrap();
        collector.observe(&mut event).unwrap();
        event["output_index"] = json!(1);
        assert_eq!(
            collector.observe(&mut event).unwrap_err().code,
            "upstream_event_too_large"
        );
    }
    #[test]
    fn output_ids_and_incomplete_output_are_restored_without_changing_status() {
        for kind in ["response.completed", "response.incomplete"] {
            let mut collector = OutputCollector::new(4096);
            let message = json!({"type":"message","id":"observed-message","content":[]});
            collector.observe(&mut json!({"type":"response.output_item.done","output_index":0,"item":message})).unwrap();
            let status = if kind == "response.incomplete" {
                "incomplete"
            } else {
                "completed"
            };
            for id in [Value::Null, json!(" ")] {
                let mut event = json!({"type":kind,"response":{"id":"r","status":status,"output":[{"type":"message","id":id,"content":[],"extension":true}]}});
                assert!(collector.observe(&mut event).unwrap());
                assert_eq!(event["response"]["output"][0]["id"], "observed-message");
                assert_eq!(event["response"]["output"][0]["extension"], true);
                assert_eq!(event["response"]["status"], status);
            }
            let mut original = json!({"type":kind,"response":{"id":"r","status":status,"output":[{"type":"message","id":"final-id"}]}});
            assert!(!collector.observe(&mut original).unwrap());
            assert_eq!(original["response"]["output"][0]["id"], "final-id");
            let mut empty = json!({"type":kind,"response":{"id":"r","status":status,"output":[]}});
            assert!(collector.observe(&mut empty).unwrap());
            assert_eq!(empty["response"]["output"], json!([message]));
            assert_eq!(empty["type"], kind);
        }
    }
    #[test]
    fn incomplete_preserves_its_nonempty_terminal_tool_fields() {
        let mut collector = OutputCollector::new(4096);
        collector.observe(&mut json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"c","name":"tool","arguments":"old"}})).unwrap();
        let mut terminal = json!({"type":"response.incomplete","response":{"id":"r","status":"incomplete","output":[{"type":"function_call","call_id":"c","name":"tool","arguments":"new","extension":"terminal"}],"incomplete_details":{"reason":"max_output_tokens"}}});
        let original = terminal.clone();
        assert!(!collector.observe(&mut terminal).unwrap());
        assert_eq!(terminal, original);
    }

    #[test]
    fn null_and_nonarray_terminal_output_are_restored_from_observed_items() {
        for kind in ["response.completed", "response.done", "response.incomplete"] {
            for output in [Value::Null, json!(false), json!("invalid"), json!({})] {
                let mut collector = OutputCollector::new(4096);
                let reasoning =
                    json!({"type":"reasoning","id":"reasoning","encrypted_content":"retained"});
                collector.observe(&mut json!({"type":"response.output_item.done","output_index":0,"item":reasoning})).unwrap();
                let status = if kind == "response.incomplete" {
                    "incomplete"
                } else {
                    "completed"
                };
                let mut terminal =
                    json!({"type":kind,"response":{"id":"r","status":status,"output":output}});
                assert!(collector.observe(&mut terminal).unwrap());
                assert_eq!(terminal["response"]["output"], json!([reasoning]));
                assert_eq!(terminal["response"]["status"], status);
            }
        }
    }
    #[test]
    fn tool_reconciliation_runs_after_id_hydration_without_borrowing_another_items_id() {
        let mut collector = OutputCollector::new(4096);
        collector.observe(&mut json!({"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"message-id","content":[]}})).unwrap();
        let call = json!({"type":"function_call","call_id":"c","name":"tool","arguments":"{}"});
        collector
            .observe(&mut json!({"type":"response.output_item.done","output_index":1,"item":call}))
            .unwrap();
        let mut terminal = json!({"type":"response.completed","response":{"id":"r","status":"completed","output":[call]}});
        collector.observe(&mut terminal).unwrap();
        assert_eq!(terminal["response"]["output"], json!([call]));
        assert!(terminal["response"]["output"][0].get("id").is_none());
    }

    #[test]
    fn incomplete_fallback_keeps_observed_partial_tool_fields_for_the_client() {
        let mut collector = OutputCollector::new(4096);
        let partial = json!({"type":"function_call","call_id":"partial","name":"tool","extension":"partial-output"});
        collector
            .observe(
                &mut json!({"type":"response.output_item.done","output_index":0,"item":partial}),
            )
            .unwrap();
        let mut terminal = json!({"type":"response.incomplete","response":{"id":"r","status":"incomplete","output":[]}});
        assert!(collector.observe(&mut terminal).unwrap());
        assert_eq!(terminal["response"]["output"], json!([partial]));
        assert_eq!(terminal["response"]["status"], "incomplete");
    }
}
