use crate::modules::codex_native_api::{
    contract::{Caller, Route, RouteRequest, WireApi},
    identity::IdentityStore,
};
use axum::http::HeaderMap;

fn prepared_history() -> Prepared {
    let directory = tempfile::tempdir().unwrap();
    let store = IdentityStore::open(directory.path().join("identity.sqlite")).unwrap();
    let mut headers = HeaderMap::new();
    let source_body = json!({"model":"m","client_metadata":{"session_id":"session","thread_id":"thread"},"input":[{"role":"user","content":"original"}]});
    let mut body = source_body.clone();
    let context = store.inspect("caller", &headers, &body).unwrap();
    let binding = store
        .rewrite(&context, "a", "owner", &mut headers, &mut body)
        .unwrap();
    Prepared {
        execution: crate::modules::codex_native_api::execution::Execution::from_deadline(
            Default::default(),
            tokio::time::Instant::now() + std::time::Duration::from_secs(60),
        ),
        route: Route {
            installation_profile: None,
            transport_identity: None,
            account_id: "a".into(),
            owner_id: "owner".into(),
            base_url: "http://127.0.0.1:1/v1".into(),
            model: "m".into(),
            headers: HeaderMap::new(),
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
            proxy_url: None,
            proxy_route: None,
            chatgpt: false,
            wire_api: WireApi::Responses,
            compact_endpoint: None,
            websocket_endpoint: None,
            default_service_tier: None,
            permit: None,
        },
        headers,
        body,
        binding,
        caller: Caller {
            id: "caller".into(),
            label: "test".into(),
        },
        request: RouteRequest {
            model: "m".into(),
            session_id: Some("session".into()),
            thread_id: Some("thread".into()),
            parent_thread_id: None,
            preferred_account_id: None,
            soft_preferred_account_id: None,
            target_account_id: None,
            force_refresh: false,
            image_generation_requested: false,
            image_input: false,
            excluded_account_ids: Vec::new(),
            requires_permit: true,
            auxiliary: Some("responses.websocket".into()),
        },
        source_body,
        source_headers: HeaderMap::new(),
        image_route_request: None,
        compact: false,
        grok_context: None,
        raw_chat: None,
        rebuild_plan: None,
        rebuild_applied: false,
        bridge_body: None,
    }
}

#[test]
fn mapped_thread_change_clears_private_tool_arguments_and_checkpoints() {
    let prepared = prepared_history();
    let mut history = History::default();
    history.begin(&prepared);
    let call = json!({"type":"function_call","call_id":"same-call","name":"private-tool","arguments":"private arguments"});
    history.remember_call(&call);
    history.observe(&prepared, &json!({"type":"response.completed","response":{"id":"response-one","status":"completed","output":[call]}}));
    assert!(history.checkpoint.is_some());
    assert!(!history.calls.is_empty());
    history.begin_scope(
        prepared.route.owner_id.clone(),
        mapped_header(&prepared, "session-id"),
        Some("different-mapped-thread".into()),
    );
    assert!(history.checkpoint.is_none());
    assert!(history.calls.is_empty());
    assert_eq!(history.call_bytes, 0);
    let output = json!({"type":"function_call_output","call_id":"same-call","output":"new result"});
    assert_eq!(
        history.repair_tools(&[output.clone()]).unwrap(),
        vec![output]
    );
}

#[test]
fn real_response_done_completed_establishes_replay_cache() {
    let prepared = prepared_history();
    let mut history = History::default();
    history.begin(&prepared);
    let call = json!({"type":"function_call","call_id":"done-call","name":"tool","arguments":"{}"});
    history.observe(
        &prepared,
        &json!({"type":"response.output_item.done","output_index":3,"item":call}),
    );
    history.observe(&prepared, &json!({"type":"response.done","response":{"id":"done-response","status":"completed","output":[]}}));
    let checkpoint = history.checkpoint.as_ref().unwrap();
    assert_eq!(checkpoint.id, "done-response");
    assert_eq!(checkpoint.output[0]["call_id"], "done-call");
    assert!(!checkpoint.connection_local);
    let expanded = history.logical_input(&json!({"previous_response_id":"done-response","input":[{"type":"function_call_output","call_id":"done-call","output":"result"}]})).unwrap();
    assert_eq!(expanded.len(), 3);
    assert_eq!(expanded[0]["content"], "original");
    assert_eq!(expanded[1]["call_id"], "done-call");
}

#[test]
fn accumulated_output_overflow_never_builds_a_partial_checkpoint() {
    let prepared = prepared_history();
    let mut history = History::default();
    history.begin(&prepared);
    for index in 0..3 {
        history.observe(&prepared, &json!({"type":"response.output_item.done","output_index":index,"item":{"type":"message","id":format!("item-{index}"),"content":"x".repeat(6*1024*1024)}}));
    }
    assert!(history.output_overflow);
    assert!(history.output_bytes <= MAX_CACHE_BYTES);
    assert_eq!(
        history.interrupted(&prepared, "overflow").unwrap_err().code,
        "upstream_history_too_large"
    );
    assert!(history.checkpoint.is_none());
    history.observe(&prepared, &json!({"type":"response.completed","response":{"id":"overflow","status":"completed","output":[]}}));
    assert!(history.checkpoint.is_none());
    assert!(history
        .logical_input(&json!({"previous_response_id":"overflow","input":[]}))
        .is_none());
}

#[test]
fn failed_or_empty_natural_incomplete_is_not_a_local_success_checkpoint() {
    let prepared = prepared_history();
    let mut history = History::default();
    history.begin(&prepared);
    for event in [
        json!({"type":"response.failed","response":{"id":"failed","status":"failed","output":[]}}),
        json!({"type":"response.incomplete","response":{"id":"empty","status":"incomplete","output":[]}}),
        json!({"type":"response.incomplete","response":{"id":"false-complete","status":"completed","output":[{"type":"message"}]}}),
    ] {
        assert!(history.incomplete_anchor(&prepared, &event).is_none());
        assert!(history.checkpoint.is_none());
    }
}

#[test]
fn completed_socket_history_replays_known_cipher_and_tool_pairs_without_mutating_input() {
    let prepared = prepared_history();
    let mut history = History::default();
    history.begin(&prepared);
    let reasoning =
        json!({"type":"reasoning","id":"reasoning-a","encrypted_content":"opaque-a","summary":[]});
    let call = json!({"type":"function_call","namespace":"collaboration","name":"spawn_agent","id":"item-call","call_id":"call-1","arguments":"{\"message\":\"task-cipher\"}","encrypted_function_args":["message"]});
    let output = vec![reasoning.clone(), call.clone()];
    for (index, item) in output.iter().enumerate() {
        history.observe(
            &prepared,
            &json!({"type":"response.output_item.done","output_index":index,"item":item}),
        );
    }
    history.observe(&prepared, &json!({"type":"response.completed","response":{"id":"completed-a","status":"completed","output":output}}));
    let result = json!({"type":"function_call_output","id":"result-1","call_id":"call-1","output":"child-id"});
    let delta = json!({"model":"m","previous_response_id":"completed-a","input":[result]});
    let original = delta.clone();
    let full = history
        .completed_replay(&prepared, &delta)
        .unwrap()
        .unwrap();
    assert!(full.get("previous_response_id").is_none());
    assert_eq!(
        full["input"],
        json!([prepared.source_body["input"][0], reasoning, call, result])
    );
    assert_eq!(delta, original);
    let mut other = prepared.clone();
    other.route.account_id = "other".into();
    assert!(history.completed_replay(&other, &delta).unwrap().is_none());
}

#[test]
fn partial_or_unproven_socket_history_cannot_authorize_cross_account_replay() {
    for mode in ["unknown-root", "missing-output", "conflict", "partial"] {
        let mut prepared = prepared_history();
        if mode == "unknown-root" {
            prepared.source_body["previous_response_id"] = json!("unseen");
        }
        let mut history = History::default();
        history.begin(&prepared);
        let first = json!({"type":"message","role":"assistant","content":"first"});
        let second = json!({"type":"message","role":"assistant","content":"second"});
        history.observe(
            &prepared,
            &json!({"type":"response.output_item.done","output_index":0,"item":first}),
        );
        if mode == "conflict" {
            history.observe(
                &prepared,
                &json!({"type":"response.output_item.done","output_index":0,"item":second}),
            );
        }
        let mut terminal = json!({"type":"response.completed","response":{"id":"candidate","status":"completed","output":[if mode=="conflict" {second} else {first}]}});
        if mode == "missing-output" {
            terminal["response"]
                .as_object_mut()
                .unwrap()
                .remove("output");
        }
        if mode == "partial" {
            terminal["type"] = json!("response.incomplete");
            terminal["response"]["status"] = json!("incomplete");
            history.incomplete_anchor(&prepared, &terminal).unwrap();
        } else {
            history.observe(&prepared, &terminal);
        }
        assert!(
            history
                .completed_replay(
                    &prepared,
                    &json!({"previous_response_id":"candidate","input":[]})
                )
                .unwrap()
                .is_none(),
            "{mode}"
        );
    }
}

#[test]
fn socket_replay_proof_survives_a_completed_delta_chain() {
    let mut prepared = prepared_history();
    let mut history = History::default();
    history.begin(&prepared);
    let first = json!({"type":"reasoning","encrypted_content":"root-cipher","summary":[]});
    history.observe(&prepared, &json!({"type":"response.completed","response":{"id":"root-response","status":"completed","output":[first]}}));
    prepared.source_body = json!({"model":"m","previous_response_id":"root-response","input":[{"role":"user","content":"next"}]});
    history.begin(&prepared);
    let second = json!({"type":"message","role":"assistant","content":"next result"});
    history.observe(&prepared, &json!({"type":"response.completed","response":{"id":"next-response","status":"completed","output":[second]}}));
    let full = history.completed_replay(&prepared, &json!({"model":"m","previous_response_id":"next-response","input":[{"role":"user","content":"third"}]})).unwrap().unwrap();
    assert_eq!(
        full["input"],
        json!([{"role":"user","content":"original"},first,{"role":"user","content":"next"},second,{"role":"user","content":"third"}])
    );
}

#[test]
fn repaired_completion_keeps_local_history_without_widening_cross_account_replay() {
    let prepared = prepared_history();
    let mut history = History::default();
    history.begin(&prepared);
    let call = json!({"type":"custom_tool_call","call_id":"c","name":"tool","input":"complete"});
    history.observe(&prepared, &json!({"type":"response.output_item.done","output_index":0,"item":call}));
    history.invalidate_rebuild_proof();
    history.observe(&prepared, &json!({"type":"response.completed","response":{"id":"repaired","status":"completed","output":[call]}}));
    let next = json!({"previous_response_id":"repaired","input":[{"type":"custom_tool_call_output","call_id":"c","output":"result"}]});
    assert_eq!(history.logical_input(&next).unwrap()[1], call);
    assert!(history.completed_replay(&prepared, &next).unwrap().is_none());
    history.remember_call(&json!({"type":"function_call","call_id":"c","name":"tool"}));
    assert_eq!(history.calls["c"], call);
}


#[test]
fn local_interruption_never_returns_or_replays_an_incomplete_collected_tool_call() {
    let prepared = prepared_history();
    let mut history = History::default();
    history.begin(&prepared);
    let call = json!({"type":"function_call","call_id":"unfinished","name":"tool"});
    let reasoning = json!({"type":"reasoning","encrypted_content":"retained-cipher"});
    history.observe(&prepared, &json!({"type":"response.output_item.done","output_index":0,"item":reasoning}));
    history.observe(&prepared, &json!({"type":"response.output_item.done","output_index":1,"item":call}));
    let interrupted = history.interrupted(&prepared, "partial").unwrap();
    assert_eq!(interrupted["response"]["output"], json!([reasoning]));
    let (body, owner) = history.resolve_local(&prepared, json!({"previous_response_id":"partial","input":[{"role":"user","content":"continue"}]})).unwrap();
    assert!(owner.is_some());
    assert_eq!(body["input"].as_array().unwrap().len(), 3);
    assert_eq!(body["input"][1], reasoning);
    assert!(history.calls.is_empty());
}


#[tokio::test]
async fn http_sse_bridge_repairs_output_without_granting_cross_account_history_proof() {
    use axum::body::Bytes;
    use futures_util::{stream, StreamExt};
    use crate::modules::codex_native_api::{events::SseDecoder, output_items::OutputCollector, upstream::ByteStream};
    for output in [json!([]), Value::Null, json!([{"type":"function_call","call_id":"c","name":"tool"}])] {
        let prepared = prepared_history();
        let mut history = History::default();
        history.begin(&prepared);
        let call = json!({"type":"custom_tool_call","call_id":"c","name":"tool","input":"complete"});
        let item = json!({"type":"response.output_item.done","output_index":0,"item":call});
        let terminal = json!({"type":"response.completed","response":{"id":"http-repaired","status":"completed","output":output}});
        let wire = format!("data: {item}\n\ndata: {terminal}\n\n");
        let mut ordinary_sse = SseDecoder::new(4096);
        assert_eq!(ordinary_sse.push(wire.as_bytes()).unwrap()[1]["response"]["output"], json!([call]));
        let source: ByteStream = stream::iter(vec![Ok(Bytes::from(wire))]).boxed();
        let mut messages = super::super::transport_bridge::sse_messages(source, 4096);
        let mut collector = OutputCollector::new(4096);
        let mut repaired_terminal = false;
        while let Some(message) = messages.next().await {
            let tokio_tungstenite::tungstenite::Message::Text(text) = message.unwrap() else { panic!() };
            let mut event: Value = serde_json::from_str(text.as_str()).unwrap();
            if collector.observe(&mut event).unwrap() {
                history.invalidate_rebuild_proof();
                repaired_terminal = true;
            }
            history.observe(&prepared, &event);
        }
        assert!(repaired_terminal, "the WS collector must see the unmodified terminal output");
        let next = json!({"previous_response_id":"http-repaired","input":[{"type":"custom_tool_call_output","call_id":"c","output":"result"}]});
        assert_eq!(history.logical_input(&next).unwrap()[1], call);
        assert!(history.completed_replay(&prepared, &next).unwrap().is_none());
    }
}


#[test]
fn incomplete_client_output_is_preserved_but_partial_tools_do_not_enter_replay_history() {
    let prepared = prepared_history();
    let mut history = History::default();
    history.begin(&prepared);
    let partial = json!({"type":"function_call","call_id":"partial","name":"tool","extension":"retained"});
    let reasoning = json!({"type":"reasoning","encrypted_content":"retained-cipher"});
    let event = json!({"type":"response.incomplete","response":{"id":"incomplete","status":"incomplete","output":[reasoning,partial]}});
    history.observe(&prepared, &event);
    assert_eq!(history.incomplete_anchor(&prepared, &event).as_deref(), Some("incomplete"));
    assert_eq!(event["response"]["output"], json!([reasoning,partial]));
    let (next, _) = history.resolve_local(&prepared, json!({"previous_response_id":"incomplete","input":[{"role":"user","content":"continue"}]})).unwrap();
    assert_eq!(next["input"], json!([prepared.source_body["input"][0],reasoning,{"role":"user","content":"continue"}]));
    assert!(history.calls.is_empty());
}
