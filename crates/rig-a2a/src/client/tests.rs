use a2a::{TaskArtifactUpdateEvent, TaskStatus, TaskStatusUpdateEvent};

use super::*;

fn chunk(text: &str, append: bool) -> StreamResponse {
    StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
        task_id: "t".into(),
        context_id: "c".into(),
        artifact: Artifact {
            artifact_id: "response".into(),
            name: None,
            description: None,
            parts: vec![Part::text(text)],
            metadata: None,
            extensions: None,
        },
        append: Some(append),
        last_chunk: None,
        metadata: None,
    })
}

fn status(state: TaskState) -> StreamResponse {
    StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
        task_id: "t".into(),
        context_id: "c".into(),
        status: TaskStatus {
            state,
            message: None,
            timestamp: None,
        },
        metadata: None,
    })
}

#[test]
fn streamed_chunks_report_deltas_rewrites_and_the_whole_answer() {
    let mut assembly = Assembly::default();
    let events: Vec<ReplyEvent> = [
        status(TaskState::Working),
        chunk("Hel", false),
        chunk("lo", true),
        chunk("Hello", false), // the final answer replaces the chunks: same text, no event
        chunk("Bye", false),   // a different answer is a rewrite
        status(TaskState::Completed),
    ]
    .into_iter()
    .flat_map(|event| assembly.apply(event))
    .collect();
    assert_eq!(
        events,
        [
            ReplyEvent::Status(TaskState::Working),
            ReplyEvent::Text("Hel".into()),
            ReplyEvent::Text("lo".into()),
            ReplyEvent::Rewrite("Bye".into()),
            ReplyEvent::Status(TaskState::Completed),
        ]
    );
    let reply = assembly.finish();
    assert_eq!(reply.text, "Bye");
    assert_eq!(reply.context_id.as_deref(), Some("c"));
}

#[test]
fn progress_in_status_messages_streams_then_the_artifact_settles_it() {
    // The sequence Strands Agents sends: deltas as working status
    // messages, then the whole answer as one artifact.
    let working = |text: &str| {
        StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: "t".into(),
            context_id: "c".into(),
            status: TaskStatus {
                state: TaskState::Working,
                message: Some(Message::new(Role::Agent, vec![Part::text(text)])),
                timestamp: None,
            },
            metadata: None,
        })
    };
    let mut assembly = Assembly::default();
    let events: Vec<ReplyEvent> = [
        working("Ho"),
        working("la"),
        chunk("Hola", false),
        status(TaskState::Completed),
    ]
    .into_iter()
    .flat_map(|event| assembly.apply(event))
    .collect();
    assert_eq!(
        events,
        [
            ReplyEvent::Status(TaskState::Working),
            ReplyEvent::Text("Ho".into()),
            ReplyEvent::Text("la".into()),
            ReplyEvent::Status(TaskState::Completed),
        ],
        "no rewrite when the artifact matches the streamed text"
    );
    assert_eq!(assembly.finish().text, "Hola");
}

#[tokio::test]
async fn a_stream_that_stops_unanswered_is_an_error() {
    let unanswered = futures::stream::iter([Ok(status(TaskState::Working))]).boxed();
    let events: Vec<_> = follow(unanswered, Duration::from_secs(1), tracing::Span::none())
        .collect()
        .await;
    assert!(
        matches!(events.last(), Some(Err(RemoteError::A2a(_)))),
        "{events:?}"
    );

    let empty = futures::stream::empty().boxed();
    let events: Vec<_> = follow(empty, Duration::from_secs(1), tracing::Span::none())
        .collect()
        .await;
    assert!(
        matches!(events.as_slice(), [Err(RemoteError::A2a(_))]),
        "{events:?}"
    );
}

#[test]
fn replies_keep_data_and_files() {
    let mut data = Part::text("");
    data.content = PartContent::Data(json!({"total": 3}));
    let mut file = Part::text("");
    file.content = PartContent::Url("https://x/report.pdf".into());
    file.media_type = Some("application/pdf".into());
    let reply = reply_from_parts([Part::text("Done"), data, file].iter(), None, None);
    assert_eq!(reply.text, "Done");
    assert_eq!(reply.data, [json!({"total": 3})]);
    assert_eq!(
        reply.files.first().map(|file| &file.content),
        Some(&FileContent::Url("https://x/report.pdf".into()))
    );
}

#[test]
fn tool_output_shows_images_inlines_small_text_and_skips_binaries() {
    // The smallest PNG header ImageMediaType::sniff recognizes.
    let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    let file = |media: Option<&str>, content| RemoteFile {
        name: Some("f".into()),
        media_type: media.map(str::to_owned),
        content,
    };
    let reply = Reply {
        text: "here".into(),
        files: vec![
            file(Some("image/png"), FileContent::Bytes(png)),
            file(Some("text/csv"), FileContent::Bytes(b"a,b\n1,2".to_vec())),
            file(
                Some("application/zip"),
                FileContent::Bytes(vec![0xff, 0x00]),
            ),
        ],
        ..Reply::default()
    };
    let output = tool_output(&reply).unwrap();
    let content = output.as_content();
    assert_eq!(content.len(), 2, "the JSON block and one image");
    assert!(matches!(content.get(1), Some(ToolResultContent::Image(_))));
    let summary = content.first().and_then(|block| block.as_json()).unwrap();
    let field = |pointer: &str| summary.pointer(pointer).and_then(Value::as_str);
    assert_eq!(field("/files/0/shown"), Some("as an image below"));
    assert_eq!(field("/files/1/text"), Some("a,b\n1,2"));
    assert_eq!(
        field("/files/2/shown"),
        Some("not shown: binary or too large")
    );
}

#[test]
fn tool_names_are_provider_safe() {
    assert_eq!(tool_name("Code Reviewer v2!"), "code_reviewer_v2");
    assert_eq!(tool_name("   "), "remote_agent");
    assert_eq!(tool_name("ñandú"), "and");
    assert_eq!(tool_name(&"x".repeat(100)).len(), 64);
}

#[tokio::test]
async fn an_oversized_agent_card_is_refused() -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut request = [0u8; 4096];
        let _ = socket.read(&mut request).await;
        let body = vec![b' '; MAX_CARD_BYTES + 1];
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = socket.write_all(head.as_bytes()).await;
        let _ = socket.write_all(&body).await;
    });
    let err = RemoteAgent::builder(url).connect().await.err();
    anyhow::ensure!(
        matches!(err, Some(RemoteError::Card(ref msg)) if msg.contains("larger than")),
        "{err:?}"
    );
    Ok(())
}
