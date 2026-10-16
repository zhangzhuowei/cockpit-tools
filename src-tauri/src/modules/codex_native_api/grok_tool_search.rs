//! Request-local lowering of client tool discovery; hosted search stays hosted.
use super::*;

fn key() -> ToolKey {
    ToolKey {
        namespace: None,
        name: "tool_search".into(),
        custom: false,
        search: true,
    }
}

pub(super) fn client_declaration(tool: &Value) -> bool {
    text(tool, "type") == Some("tool_search")
        && matches!(text(tool, "execution"), None | Some("client"))
}

pub(super) fn lower_declaration(
    tool: &Value,
    context: &mut Context,
) -> Result<Value, ServiceError> {
    let mut lowered = tool.clone();
    lowered["type"] = json!("function");
    lowered["name"] = json!(context.bind(key())?);
    lowered.as_object_mut().unwrap().remove("execution");
    if lowered.get("parameters").is_none_or(Value::is_null) {
        lowered["parameters"] = json!({"type":"object","properties":{"query":{"type":"string"},"limit":{"type":"integer"}},"required":["query"]});
    }
    if lowered.get("description").is_none_or(Value::is_null) {
        lowered["description"] = json!("Search and load client tools for the current task.");
    }
    Ok(lowered)
}

fn search_item(item: &Value, declared: bool) -> bool {
    matches!(
        text(item, "type"),
        Some("tool_search_call" | "tool_search_output")
    ) && (text(item, "execution") == Some("client")
        || (text(item, "execution").is_none() && declared))
}

fn declaration_identity(tool: &Value, parent: &str) -> String {
    json!([
        text(tool, "type").unwrap_or(""),
        namespace(tool).unwrap_or_else(|| parent.to_owned()),
        text(tool, "name").unwrap_or("")
    ])
    .to_string()
}

fn declaration_keys(
    tools: &[Value],
    parent: &str,
    keys: &mut HashSet<String>,
    depth: usize,
) -> Result<(), ServiceError> {
    if depth > 16 {
        return Err(invalid("Tool namespaces exceed their nesting limit"));
    }
    for tool in tools {
        let name = text(tool, "name").unwrap_or("");
        let name = if parent.is_empty() {
            name.to_owned()
        } else {
            format!("{parent}.{name}")
        };
        if text(tool, "type") == Some("namespace") {
            if let Some(children) = tool.get("tools").and_then(Value::as_array) {
                declaration_keys(children, &name, keys, depth + 1)?;
            }
        } else {
            keys.insert(declaration_identity(tool, parent));
            if keys.len() > MAX_BINDINGS {
                return Err(invalid("Too many Grok tool identities"));
            }
        }
    }
    Ok(())
}

fn novel_tool(
    tool: &Value,
    parent: &str,
    keys: &mut HashSet<String>,
    depth: usize,
) -> Result<Option<Value>, ServiceError> {
    if depth > 16 {
        return Err(invalid("Tool namespaces exceed their nesting limit"));
    }
    let short = text(tool, "name").unwrap_or("");
    let name = if parent.is_empty() {
        short.to_owned()
    } else {
        format!("{parent}.{short}")
    };
    if text(tool, "type") == Some("namespace") {
        let children = tool
            .get("tools")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("Discovered namespace requires a tools array"))?;
        let mut kept = vec![];
        for child in children {
            if let Some(child) = novel_tool(child, &name, keys, depth + 1)? {
                kept.push(child);
            }
        }
        if kept.is_empty() {
            return Ok(None);
        }
        let mut result = tool.clone();
        result["tools"] = json!(kept);
        return Ok(Some(result));
    }
    if !keys.insert(declaration_identity(tool, parent)) {
        return Ok(None);
    }
    if keys.len() > MAX_BINDINGS {
        return Err(invalid("Too many Grok tool identities"));
    }
    Ok(Some(tool.clone()))
}

fn completed_discoveries(item: &Value) -> Result<Option<Vec<Value>>, ServiceError> {
    if text(item, "type") != Some("tool_search_output")
        || text(item, "execution") == Some("server")
        || text(item, "status").is_some_and(|status| status != "completed")
    {
        return Ok(None);
    }
    if let Some(tools) = item.get("tools") {
        return tools
            .as_array()
            .cloned()
            .map(Some)
            .ok_or_else(|| invalid("Discovered tools must be an array"));
    }
    let output = match item.get("output") {
        Some(Value::String(raw)) => serde_json::from_str::<Value>(raw).ok(),
        output => output.cloned(),
    };
    Ok(output.and_then(|output| {
        output
            .as_array()
            .cloned()
            .or_else(|| output.get("tools").and_then(Value::as_array).cloned())
    }))
}

pub(super) fn prepare(body: &mut Value, context: &mut Context) -> Result<(), ServiceError> {
    let mut declared = body
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| tools.iter().any(client_declaration));
    let input = body
        .get("input")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    declared |= input.iter().any(|item| {
        text(item, "type") == Some("additional_tools")
            && item
                .get("tools")
                .and_then(Value::as_array)
                .is_some_and(|tools| tools.iter().any(client_declaration))
    });
    let mut client_ids = HashSet::new();
    for item in &input {
        if search_item(item, declared) {
            if let Some(id) = text(item, "call_id").or_else(|| text(item, "id")) {
                client_ids.insert(id.to_owned());
            }
        }
    }
    let additional_tools = input
        .iter()
        .any(|item| text(item, "type") == Some("additional_tools"));
    if !declared && client_ids.is_empty() && !additional_tools {
        return Ok(());
    }
    // An explicit empty list disables tools. Missing declarations can occur on
    // follow-up requests, whose completed discoveries still define callable tools.
    let may_promote = body.get("tools").is_none()
        || body
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| !tools.is_empty());
    if may_promote {
        let mut tools = body
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut known = HashSet::new();
        declaration_keys(&tools, "", &mut known, 0)?;
        for item in &input {
            if text(item, "type") == Some("additional_tools") {
                let additional = item
                    .get("tools")
                    .and_then(Value::as_array)
                    .ok_or_else(|| invalid("Additional tools must be an array"))?;
                for tool in additional {
                    if let Some(tool) = novel_tool(tool, "", &mut known, 0)? {
                        tools.push(tool);
                    }
                }
            }
        }
        // Current declarations win; among historical discoveries the latest
        // completed definition wins, so a stale schema cannot replace it.
        for item in input.iter().rev() {
            if !search_item(item, declared)
                && !text(item, "call_id").is_some_and(|id| client_ids.contains(id))
            {
                continue;
            }
            if let Some(discovered) = completed_discoveries(item)? {
                for tool in &discovered {
                    if let Some(tool) = novel_tool(tool, "", &mut known, 0)? {
                        tools.push(tool);
                    }
                }
            }
        }
        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }
    }
    collect_names(body, &mut context.reserved, 0)?;
    let wire = if declared || !client_ids.is_empty() {
        context.bind(key())?
    } else {
        String::new()
    };
    if let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) {
        // Declarations remain in tools; Grok does not accept additional_tools
        // as a message item. Explicit tools=[] still disables new tools.
        items.retain(|item| text(item, "type") != Some("additional_tools"));
        for item in items {
            let paired = text(item, "execution") != Some("server")
                && text(item, "call_id").is_some_and(|id| client_ids.contains(id));
            if !search_item(item, declared) && !paired {
                continue;
            }
            match text(item, "type") {
                Some("tool_search_call") => {
                    let id = text(item, "call_id")
                        .filter(|id| !id.is_empty())
                        .ok_or_else(|| invalid("Client tool search requires call_id"))?
                        .to_owned();
                    let arguments = item.get("arguments").cloned().unwrap_or_else(|| json!({}));
                    let arguments = if let Some(raw) = arguments.as_str() {
                        decode_arguments(raw, false)
                            .map_err(|_| invalid("Invalid client tool search arguments"))?
                    } else {
                        arguments
                    };
                    if !arguments.is_object() {
                        return Err(invalid("Client tool search arguments must be an object"));
                    }
                    item["type"] = json!("function_call");
                    item["name"] = json!(wire);
                    item["call_id"] = json!(id);
                    item["arguments"] = json!(arguments.to_string());
                    if let Some(id) = text(item, "id").map(str::to_owned) {
                        item["id"] = json!(wire_item_id(&id));
                    }
                    item.as_object_mut().unwrap().remove("execution");
                    item.as_object_mut().unwrap().remove("status");
                }
                Some("tool_search_output") => {
                    if text(item, "call_id").is_none_or(str::is_empty) {
                        return Err(invalid("Client tool search output requires call_id"));
                    }
                    let output = item
                        .get("output")
                        .or_else(|| item.get("tools"))
                        .cloned()
                        .ok_or_else(|| {
                            invalid("Client tool search output requires output or tools")
                        })?;
                    let output = match output {
                        Value::String(s) => s,
                        Value::Null => String::new(),
                        other => other.to_string(),
                    };
                    item["type"] = json!("function_call_output");
                    item["output"] = json!(output);
                    let object = item.as_object_mut().unwrap();
                    for field in ["tools", "execution", "status", "id"] {
                        object.remove(field);
                    }
                }
                _ => {}
            }
        }
    }
    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        if !tools.iter().any(|t| {
            text(t, "type") == Some("tool_search") && text(t, "execution") == Some("server")
        }) {
            remove_deferred(tools, 0)?;
        }
    }
    Ok(())
}

fn remove_deferred(tools: &mut [Value], depth: usize) -> Result<(), ServiceError> {
    if depth > 16 {
        return Err(invalid("Tool namespaces exceed their nesting limit"));
    }
    for tool in tools {
        if let Some(object) = tool.as_object_mut() {
            object.remove("defer_loading");
        }
        if let Some(children) = tool.get_mut("tools").and_then(Value::as_array_mut) {
            remove_deferred(children, depth + 1)?;
        }
    }
    Ok(())
}

pub(super) fn rewrite_choice(choice: &mut Value, context: &Context) {
    if text(choice, "type") == Some("tool_search") && text(choice, "execution") != Some("server") {
        if let Some(wire) = context.by_key.get(&key()) {
            choice["type"] = json!("function");
            choice["name"] = json!(wire);
            choice.as_object_mut().unwrap().remove("execution");
        }
    }
}

pub(super) fn wire_item_id(id: &str) -> String {
    id.strip_prefix("tsc_")
        .map(|suffix| format!("fc_{suffix}"))
        .unwrap_or_else(|| id.to_owned())
}
fn client_item_id(id: &str) -> String {
    id.strip_prefix("fc_")
        .map(|suffix| format!("tsc_{suffix}"))
        .unwrap_or_else(|| id.to_owned())
}
fn decode_arguments(raw: &str, partial: bool) -> Result<Value, ServiceError> {
    if raw.is_empty() && partial {
        return Ok(json!({}));
    }
    let arguments: Value = serde_json::from_str(raw)
        .map_err(|_| upstream("Grok returned invalid tool search arguments"))?;
    if !arguments.is_object() {
        return Err(upstream("Grok tool search arguments must be an object"));
    }
    Ok(arguments)
}
pub(super) fn restore_item(item: &mut Value, partial: bool) -> Result<(), ServiceError> {
    let raw = text(item, "arguments").unwrap_or("");
    let arguments = decode_arguments(raw, partial)?;
    item["type"] = json!("tool_search_call");
    item["execution"] = json!("client");
    item["arguments"] = arguments;
    if let Some(id) = text(item, "id").map(str::to_owned) {
        item["id"] = json!(client_item_id(&id));
    }
    let object = item.as_object_mut().unwrap();
    object.remove("name");
    object.remove("namespace");
    Ok(())
}
pub(super) fn stream_key(context: &Context, event: &Value) -> Option<String> {
    let keys = [
        call_key(event),
        text(event, "call_id").map(|id| format!("call:{id}")),
        event
            .get("output_index")
            .and_then(Value::as_u64)
            .map(|index| format!("index:{index}")),
    ];
    keys.into_iter().flatten().find_map(|key| {
        context
            .call_aliases
            .get(&key)
            .cloned()
            .or_else(|| context.calls.contains_key(&key).then_some(key))
    })
}
pub(super) fn arguments_event(
    context: &mut Context,
    key: &str,
    event: &Value,
) -> Result<bool, ServiceError> {
    if !context
        .calls
        .get(key)
        .and_then(|call| call.binding.as_ref())
        .is_some_and(|binding| binding.key.search)
    {
        return Ok(false);
    }
    if text(event, "type") == Some("response.function_call_arguments.delta") {
        let delta = text(event, "delta")
            .ok_or_else(|| upstream("Grok tool argument delta must be text"))?;
        context.account(delta.len())?;
        context
            .calls
            .get_mut(key)
            .unwrap()
            .arguments
            .push_str(delta);
    } else {
        let arguments = text(event, "arguments")
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| context.calls[key].arguments.clone());
        decode_arguments(&arguments, false)?;
        if arguments.len() > MAX_STATE_BYTES {
            return Err(upstream("Grok tool arguments exceed the stream limit"));
        }
        context.calls.get_mut(key).unwrap().arguments = arguments;
    }
    // Client tool search is dispatched from completed output items, not from
    // function-argument events (which would expose the private proxy function).
    Ok(true)
}
pub(super) fn complete_item(context: &Context, event: &mut Value) -> Result<(), ServiceError> {
    let mut lookup = event.clone();
    if let Some(id) = event.pointer("/item/id").cloned() {
        lookup["item_id"] = id;
    }
    if let Some(key) = stream_key(context, &lookup) {
        if let Some(call) = context.calls.get(&key).filter(|call| {
            call.binding
                .as_ref()
                .is_some_and(|binding| binding.key.search)
        }) {
            if event
                .pointer("/item/arguments")
                .is_none_or(|value| value.as_str().is_some_and(str::is_empty))
                && !call.arguments.is_empty()
            {
                event["item"]["arguments"] = json!(call.arguments);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "grok_tool_search_tests.rs"]
mod tests;
