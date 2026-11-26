//! Streaming answers, and answers with data and files, as a client sees them.
#![cfg(not(target_family = "wasm"))]

use std::sync::Arc;

use anyhow::ensure;
use futures::StreamExt;
use futures::stream::BoxStream;
use rig_a2a::a2a::{
    A2AError, AgentCard, Artifact, Part, PartContent, StreamResponse, Task,
    TaskArtifactUpdateEvent, TaskState, TaskStatus, TaskStatusUpdateEvent,
};
use rig_a2a::a2a_server::agent_card::agent_card_router;
use rig_a2a::a2a_server::jsonrpc::jsonrpc_router;
use rig_a2a::a2a_server::{
    AgentExecutor, DefaultRequestHandler, ExecutorContext, InMemoryTaskStore, StaticAgentCard,
};
use rig_a2a::{A2aServer, FileContent, RemoteAgent, ReplyEvent, V03_PATH};
use rig_agent::AgentBuilder;
use rig_core::message::ToolResultContent;
use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};

/// The smallest PNG header `ImageMediaType::sniff` recognizes.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

#[tokio::test]
async fn stream_reports_text_as_it_grows_on_both_versions() -> anyhow::Result<()> {
    for prefix in ["", V03_PATH] {
        let model = MockCompletionModel::from_stream_turns([vec![
            MockStreamEvent::text("Hel"),
            MockStreamEvent::text("lo"),
            MockStreamEvent::final_response_with_default_usage(),
        ]]);
        let agent = AgentBuilder::new(model).name("Echo").build();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}", listener.local_addr()?);
        let app = A2aServer::new(agent, &url).into_router();
        tokio::spawn(async move { rig_a2a::axum::serve(listener, app).await });

        let remote = RemoteAgent::connect(&format!("{url}{prefix}")).await?;
        ensure!(remote.streams(), "{prefix}: the card declares streaming");
        let events: Vec<ReplyEvent> = remote
            .stream("Hi", None)
            .await?
            .map(|event| event.map_err(anyhow::Error::from))
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<anyhow::Result<_>>()?;
        let texts: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                ReplyEvent::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        ensure!(
            texts == ["Hel", "lo"],
            "{prefix}: deltas {texts:?} in {events:?}"
        );
        ensure!(
            events.contains(&ReplyEvent::Status(TaskState::Working)),
            "{prefix}: {events:?}"
        );
        let Some(ReplyEvent::Done(reply)) = events.last() else {
            anyhow::bail!("{prefix}: the stream ends with the answer: {events:?}");
        };
        ensure!(reply.text == "Hello" && reply.state == Some(TaskState::Completed));
    }
    Ok(())
}

/// A third-party A2A agent that answers with text, data, an image and a PDF link.
struct Reporter;

impl AgentExecutor for Reporter {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let (task_id, context_id) = (ctx.task_id, ctx.context_id);
        let status = |state| TaskStatus {
            state,
            message: None,
            timestamp: None,
        };
        let mut data = Part::text("");
        data.content = PartContent::Data(serde_json::json!({"rows": 3, "total": 42}));
        let mut image = Part::text("");
        image.content = PartContent::Raw(PNG.to_vec());
        image.media_type = Some("image/png".into());
        image.filename = Some("chart.png".into());
        let mut pdf = Part::text("");
        pdf.content = PartContent::Url("https://example.com/report.pdf".into());
        pdf.media_type = Some("application/pdf".into());
        let events = vec![
            Ok(StreamResponse::Task(Task {
                id: task_id.clone(),
                context_id: context_id.clone(),
                status: status(TaskState::Working),
                artifacts: None,
                history: None,
                metadata: None,
            })),
            Ok(StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
                task_id: task_id.clone(),
                context_id: context_id.clone(),
                artifact: Artifact {
                    artifact_id: "report".into(),
                    name: Some("report".into()),
                    description: None,
                    parts: vec![Part::text("Report ready."), data, image, pdf],
                    metadata: None,
                    extensions: None,
                },
                append: Some(false),
                last_chunk: Some(true),
                metadata: None,
            })),
            Ok(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                task_id,
                context_id,
                status: status(TaskState::Completed),
                metadata: None,
            })),
        ];
        futures::stream::iter(events).boxed()
    }

    fn cancel(&self, _: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        futures::stream::empty().boxed()
    }
}

async fn serve_reporter() -> anyhow::Result<String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let card: AgentCard = serde_json::from_value(serde_json::json!({
        "name": "Reporter",
        "description": "Builds reports.",
        "version": "1.0.0",
        "supportedInterfaces": [{"url": format!("{url}/jsonrpc"), "protocolBinding": "JSONRPC", "protocolVersion": "1.0"}],
        "capabilities": {"streaming": true, "pushNotifications": false},
        "defaultInputModes": ["text/plain"],
        "defaultOutputModes": ["text/plain", "application/json", "image/png", "application/pdf"],
        "skills": [{"id": "report", "name": "Report", "description": "Builds a report.", "tags": ["report"]}]
    }))?;
    let handler = Arc::new(
        DefaultRequestHandler::new(Reporter, InMemoryTaskStore::new())
            .with_capabilities(card.capabilities.clone()),
    );
    let app = rig_a2a::axum::Router::new()
        .nest("/jsonrpc", jsonrpc_router(handler))
        .merge(agent_card_router(Arc::new(StaticAgentCard::new(card))));
    tokio::spawn(async move { rig_a2a::axum::serve(listener, app).await });
    Ok(url)
}

#[tokio::test]
async fn data_and_files_reach_the_reply_and_the_model() -> anyhow::Result<()> {
    let remote = RemoteAgent::connect(&serve_reporter().await?).await?;

    let reply = remote.send("Build the report", None).await?;
    ensure!(reply.text == "Report ready.", "{reply:?}");
    // v1.0 carries data parts as a protobuf Struct, whose numbers are doubles:
    // 42 arrives as 42.0. Compare by value.
    ensure!(
        reply.data.len() == 1 && reply.data[0]["total"].as_f64() == Some(42.0),
        "{reply:?}"
    );
    ensure!(reply.files.len() == 2, "{reply:?}");
    ensure!(reply.files[0].content == FileContent::Bytes(PNG.to_vec()));
    ensure!(reply.files[0].name.as_deref() == Some("chart.png"));
    ensure!(reply.files[1].content == FileContent::Url("https://example.com/report.pdf".into()));

    // As a tool: the model gets the JSON summary and the image itself.
    let output = remote
        .into_tool()
        .run(serde_json::json!({"message": "Build the report"}))
        .await?;
    let content = output.as_content();
    ensure!(content.len() == 2, "JSON then the image: {content:?}");
    let summary = content[0].as_json().expect("a JSON block first");
    ensure!(
        summary["data"][0]["total"].as_f64() == Some(42.0),
        "{summary}"
    );
    ensure!(
        summary["files"][0]["shown"] == "as an image below",
        "{summary}"
    );
    ensure!(
        summary["files"][1]["url"] == "https://example.com/report.pdf",
        "{summary}"
    );
    ensure!(matches!(content[1], ToolResultContent::Image(_)));
    Ok(())
}
