//! Each task gets an `a2a.task` span that records how it ended, and the
//! agent's own spans (model calls) nest under it.
#![cfg(not(target_family = "wasm"))]

use std::collections::HashMap;

use anyhow::ensure;
use rig_a2a::{A2aServer, RemoteAgent};
use rig_agent::AgentBuilder;
use rig_core::test_utils::{MockCompletionModel, MockStreamEvent, TraceCapture};

#[tokio::test] // current-thread: the thread-local subscriber sees the server too
async fn a_task_span_records_its_end_and_parents_the_agent_spans() -> anyhow::Result<()> {
    let capture = TraceCapture::default();
    let _guard = tracing::subscriber::set_default(capture.subscriber());

    let model = MockCompletionModel::from_stream_turns([vec![
        MockStreamEvent::text("pong"),
        MockStreamEvent::final_response_with_default_usage(),
    ]]);
    let agent = AgentBuilder::new(model).name("Echo").build();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let app = A2aServer::new(agent, &url).into_router();
    tokio::spawn(async move { rig_a2a::axum::serve(listener, app).await });
    let reply = RemoteAgent::connect(&url).await?.send("ping", None).await?;
    ensure!(reply.text == "pong");

    let spans = capture.spans();
    let task = spans
        .iter()
        .find(|span| span.name == "a2a.task")
        .ok_or_else(|| anyhow::anyhow!("no a2a.task span"))?;
    ensure!(
        task.text("a2a.state").as_deref() == Some("completed"),
        "{:?}",
        task.values()
    );
    ensure!(
        task.text("a2a.context_id") == reply.context_id,
        "{:?}",
        task.values()
    );

    let parents: HashMap<u64, Option<u64>> =
        spans.iter().map(|span| (span.id, span.parent)).collect();
    let under_task = |mut id: u64| {
        while let Some(Some(parent)) = parents.get(&id) {
            if *parent == task.id {
                return true;
            }
            id = *parent;
        }
        false
    };
    let nested: Vec<&str> = spans
        .iter()
        .filter(|span| under_task(span.id))
        .map(|span| span.name)
        .collect();
    ensure!(
        !nested.is_empty(),
        "the agent's spans nest under the task: {:?}",
        spans
            .iter()
            .map(|span| (span.name, span.parent_name))
            .collect::<Vec<_>>()
    );
    Ok(())
}
