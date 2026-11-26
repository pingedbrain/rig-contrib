use super::*;

#[test]
fn messages_round_trip_through_v03_json() {
    let mut message = Message::new(Role::User, vec![Part::text("hi")]);
    message.context_id = Some("ctx".into());
    let json = message_json(&message);
    assert_eq!(
        json.pointer("/parts/0"),
        Some(&json!({"kind": "text", "text": "hi"}))
    );
    assert_eq!(json.get("role"), Some(&json!("user")));
    let back = message_from_json(&json).unwrap();
    assert_eq!(back.parts.first().and_then(Part::as_text), Some("hi"));
    assert_eq!(back.context_id.as_deref(), Some("ctx"));
    assert_eq!(back.message_id, message.message_id);
}

#[test]
fn file_parts_keep_bytes_and_media_type() {
    let json = json!({"kind": "file", "file": {"bytes": "aGk=", "mimeType": "text/plain", "name": "a.txt"}});
    let part = part_from_json(&json).unwrap();
    assert_eq!(part.content, PartContent::Raw(b"hi".to_vec()));
    assert_eq!(part_json(&part), json);
}

#[test]
fn v03_results_become_v1_events() {
    let done = json!({"kind": "task", "id": "t", "contextId": "c", "status": {"state": "completed"},
        "artifacts": [{"artifactId": "a", "parts": [{"kind": "text", "text": "42"},
            {"kind": "data", "data": {"n": 42}}]}]});
    let StreamResponse::Task(task) = response_from_json(&done).unwrap() else {
        panic!()
    };
    assert_eq!(task.status.state, TaskState::Completed);
    let artifact = task.artifacts.unwrap().into_iter().next().unwrap();
    assert_eq!(
        artifact.parts.get(1).map(|part| &part.content),
        Some(&PartContent::Data(json!({"n": 42})))
    );

    let update = json!({"kind": "artifact-update", "taskId": "t", "contextId": "c", "append": true,
        "artifact": {"artifactId": "a", "parts": [{"kind": "text", "text": "lo"}]}});
    let StreamResponse::ArtifactUpdate(update) = response_from_json(&update).unwrap() else {
        panic!()
    };
    assert_eq!(update.append, Some(true));

    let error = json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32001, "message": "no task"}});
    assert_eq!(response_from_frame(&error).unwrap_err().code, -32001);
}

#[test]
fn sse_blocks_yield_their_data() {
    assert_eq!(
        sse_data(b"event: x\ndata: {\"a\":1}\n\n").as_deref(),
        Some("{\"a\":1}")
    );
    assert_eq!(sse_data(b": keep-alive\n\n"), None);
}
