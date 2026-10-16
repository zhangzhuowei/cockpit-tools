use super::*;
use serde_json::json;

fn event(value: &Value) -> Vec<u8> {
    format!("data: {value}\n\n").into_bytes()
}
fn completion() -> Value {
    json!({"type":"response.completed","response":{"id":"r","status":"completed","output":[],"extension":{"keep":true}}})
}

#[test]
fn complete_wire_fields_and_unknown_values_are_preserved() {
    let value = json!({"type":"response.created","response":{"id":"r","status":"in_progress","extension":17}});
    let bytes = format!(": comment\r\nid: 9\r\nretry: 50\r\ncustom: value\r\nevent: response.created\r\ndata: {value}\r\n\r\n");
    let mut decoder = SseDecoder::new(4096);
    let frames = decoder.push_frames(bytes.as_bytes()).unwrap();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].value, Some(value));
    assert_eq!(frames[0].bytes.as_ref(), bytes.as_bytes());
}

#[test]
fn split_json_and_missing_field_newline_are_repaired_without_replay() {
    let mut decoder = SseDecoder::new(4096);
    assert!(decoder
        .push_frames(b"event: response.completed")
        .unwrap()
        .is_empty());
    assert!(decoder
        .push_frames(b"data: {\"response\":{\"id\":\"r\",")
        .unwrap()
        .is_empty());
    let frames = decoder
        .push_frames(b"\"status\":\"completed\",\"output\":[]},\"extension\":true}")
        .unwrap();
    assert_eq!(frames.len(), 1);
    assert_eq!(
        frames[0].value.as_ref().unwrap()["type"],
        "response.completed"
    );
    assert_eq!(frames[0].value.as_ref().unwrap()["extension"], true);
    let mut reader = SseDecoder::new(4096);
    assert_eq!(
        reader.push(&frames[0].bytes).unwrap()[0],
        *frames[0].value.as_ref().unwrap()
    );
    assert!(decoder.push_frames(b"\n\n").unwrap().is_empty());
}

#[test]
fn data_only_valid_eof_tail_is_flushed_and_truncated_json_is_rejected() {
    let bytes = format!("data: {}", completion());
    let mut decoder = SseDecoder::new(4096);
    assert!(decoder.push_frames(bytes.as_bytes()).unwrap().is_empty());
    let frames = decoder.finish_frames().unwrap();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].value, Some(completion()));
    assert!(frames[0].bytes.ends_with(b"\n\n"));
    assert!(!decoder.has_partial_event());
    assert!(decoder.finish_frames().unwrap().is_empty());
    let mut decoder = SseDecoder::new(4096);
    decoder
        .push_frames(b"data: {\"type\":\"response.completed\"")
        .unwrap();
    assert_eq!(
        decoder.finish_frames().unwrap_err().code,
        "incomplete_upstream_stream"
    );
}

#[test]
fn consecutive_data_chunks_missing_delimiters_remain_distinct_events() {
    let mut decoder = SseDecoder::new(4096);
    let first = json!({"type":"response.created","response":{"id":"r"}});
    assert!(decoder
        .push_frames(format!("data: {first}").as_bytes())
        .unwrap()
        .is_empty());
    let frames = decoder
        .push_frames(format!("data: {}", completion()).as_bytes())
        .unwrap();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].value, Some(first));
    assert_eq!(decoder.finish().unwrap(), vec![completion()]);
}

#[test]
fn many_small_events_in_one_transport_chunk_respect_per_event_limit() {
    let bytes = b"data: {\"extension\":1}\n\n".repeat(20);
    let mut decoder = SseDecoder::new(32);
    let frames = decoder.push_frames(&bytes).unwrap();
    assert_eq!(frames.len(), 20);
    let output: Vec<u8> = frames
        .into_iter()
        .flat_map(|frame| frame.bytes.to_vec())
        .collect();
    assert_eq!(output, bytes);
    let mut decoder = SseDecoder::new(32);
    assert_eq!(
        decoder
            .push_frames(b"data: {\"extension\":\"abcdefghijklmnopqrstuvwxyz\"}\n\n")
            .unwrap_err()
            .code,
        "upstream_event_too_large"
    );
}

#[test]
fn output_items_restore_completed_output_in_index_order_and_preserve_existing_output() {
    for existing in [false, true] {
        let mut decoder = SseDecoder::new(4096);
        for (index, text) in [(2, "later"), (0, "earlier")] {
            decoder.push(&event(&json!({"type":"response.output_item.done","output_index":index,"item":{"type":"message","text":text,"extension":index}}))).unwrap();
        }
        let mut completed = completion();
        if existing {
            completed["response"]["output"] = json!([{"type":"message","text":"canonical"}]);
        }
        let frames = decoder.push_frames(&event(&completed)).unwrap();
        let response = &frames[0].value.as_ref().unwrap()["response"];
        if existing {
            assert_eq!(response["output"][0]["text"], "canonical");
        } else {
            assert_eq!(response["output"][0]["text"], "earlier");
            assert_eq!(response["output"][1]["text"], "later");
            assert_eq!(response["output"][1]["extension"], 2);
        }
        assert_eq!(response["extension"]["keep"], true);
        let mut reader = SseDecoder::new(4096);
        assert_eq!(
            reader.push(&frames[0].bytes).unwrap()[0],
            *frames[0].value.as_ref().unwrap()
        );
    }
}

#[test]
fn output_cache_and_repaired_terminal_size_remain_bounded() {
    let mut decoder = SseDecoder::new(160);
    for index in 0..4 {
        let item = json!({"type":"response.output_item.done","output_index":index,"item":{"type":"message","content":"abcdefghijklmnopqrstuvwxyz"}});
        let result = decoder.push(&event(&item));
        if index < 2 {
            result.unwrap();
        } else {
            assert_eq!(result.unwrap_err().code, "upstream_event_too_large");
            break;
        }
    }
}

#[test]
fn explicit_failure_wins_over_success_event_and_later_completion() {
    for bytes in [
        format!("event: response.failed\ndata: {}\n\n", completion()),
        "event: response.completed\ndata: {\"type\":\"error\",\"error\":{\"code\":\"invalid_token\"}}\n\n".into(),
        "event: response.completed\ndata: {\"response\":{\"id\":\"r\",\"status\":\"completed\",\"error\":{\"code\":\"server_error\"}}}\n\n".into(),
    ] {
        let mut decoder = SseDecoder::new(4096);
        let mut payload = bytes.into_bytes(); payload.extend(event(&completion()));
        let frames = decoder.push_frames(&payload).unwrap();
        assert_eq!(frames.len(), 1);
        let mut observation = ResponseObservation::default(); observation.observe(frames[0].value.as_ref().unwrap());
        assert!(observation.terminal);
        assert!(!observation.succeeded);
        assert!(decoder.push(&event(&completion())).unwrap().is_empty());
    }
}

#[test]
fn completed_frames_require_real_id_and_completed_status() {
    for response in [
        json!({"status":"completed"}),
        json!({"id":"r"}),
        json!({"id":" ","status":"completed"}),
    ] {
        let value = json!({"type":"response.completed","response":response});
        let mut decoder = SseDecoder::new(4096);
        assert_eq!(
            decoder.push(&event(&value)).unwrap_err().code,
            "invalid_upstream_event"
        );
        let mut observation = ResponseObservation::default();
        observation.observe(&value);
        assert!(!observation.succeeded);
    }
    // A failure status is a valid terminal frame even when its event type says
    // completed. It must stop framing and checkpoint publication, never promote a checkpoint.
    for (status, expected_kind) in [
        ("failed", "response.failed"),
        ("incomplete", "response.incomplete"),
        ("cancelled", "response.cancelled"),
    ] {
        let value = json!({"type":"response.completed","response":{"id":"failed-response","status":status,"output":[]}});
        let mut payload = event(&value);
        payload.extend(event(&completion()));
        let mut decoder = SseDecoder::new(4096);
        let frames = decoder.push_frames(&payload).unwrap();
        assert_eq!(frames.len(), 1, "first failure must fence later completion");
        let failure = frames[0].value.as_ref().unwrap();
        assert_eq!(failure["type"], expected_kind);
        let mut observation = ResponseObservation::default();
        observation.observe(failure);
        observation.observe(&completion());
        assert!(observation.terminal);
        assert!(!observation.succeeded);
        assert!(decoder.push(&event(&completion())).unwrap().is_empty());

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("identity.sqlite");
        let store = super::super::identity::IdentityStore::open(path.clone()).unwrap();

        let mut headers = axum::http::HeaderMap::new();
        let mut body = json!({"model":"test","client_metadata":{"thread_id":"thread"},"input":[{"type":"reasoning","encrypted_content":"unaccepted-cipher"}]});
        let mut context = store
            .inspect_for_rebuild("caller", &headers, &body)
            .unwrap();
        store
            .configure_rebuild(&mut context, &headers, &body, true)
            .unwrap();
        let binding = store
            .rewrite(&context, "account", "owner", &mut headers, &mut body)
            .unwrap();
        store.observe_event(&binding, failure).unwrap();
        // A later transport-level signal cannot authorize an aborted history.
        store.observe_event(&binding, &completion()).unwrap();
        let connection = rusqlite::Connection::open(&path).unwrap();
        for table in ["native_rebuild_active_v1", "native_rebuild_checkpoints_v1"] {
            let rows: i64 = connection
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(
                rows, 0,
                "{status}: an aborted history must not publish {table}"
            );
        }
    }
}
