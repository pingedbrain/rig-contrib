//! Push notifications (off by default, opt-in on the server) and resubscribe.
#![cfg(not(target_family = "wasm"))]

use std::sync::Arc;
use std::time::Duration;

use a2a_client::agent_card::AgentCardResolver;
use anyhow::{Context, ensure};
use futures::StreamExt;
use rig_a2a::A2aServer;
use rig_a2a::a2a::{
    SendMessageConfiguration, StreamResponse, SubscribeToTaskRequest, TRANSPORT_PROTOCOL_JSONRPC,
    TaskPushNotificationConfig, TaskState, error_code,
};
use rig_a2a::axum::body::Bytes;
use rig_a2a::axum::extract::State;
use rig_a2a::axum::http::StatusCode;
use rig_a2a::axum::routing::post;
use rig_agent::{Agent, AgentBuilder};
use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};
use tokio::sync::{Notify, mpsc};

use super::serve::{MockControlledTool, connect, reply, request, serve};

/// Serve `agent` on a free loopback port with `configure` applied; returns its base URL.
async fn serve_with(
    agent: Agent,
    configure: impl FnOnce(A2aServer) -> A2aServer,
) -> anyhow::Result<String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let app = configure(A2aServer::new(agent, &url)).into_router();
    tokio::spawn(async move { rig_a2a::axum::serve(listener, app).await });
    Ok(url)
}

/// A local webhook: each POST body it receives is sent on the returned channel.
async fn webhook() -> anyhow::Result<(String, mpsc::UnboundedReceiver<StreamResponse>)> {
    async fn capture(
        State(sender): State<mpsc::UnboundedSender<StreamResponse>>,
        body: Bytes,
    ) -> StatusCode {
        if let Ok(event) = serde_json::from_slice::<StreamResponse>(&body) {
            let _ = sender.send(event);
        }
        StatusCode::OK
    }
    let (sender, receiver) = mpsc::unbounded_channel();
    let app = rig_a2a::axum::Router::new()
        .route("/hook", post(capture))
        .with_state(sender);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/hook", listener.local_addr()?);
    tokio::spawn(async move { rig_a2a::axum::serve(listener, app).await });
    Ok((url, receiver))
}

fn config(task_id: &str, url: &str) -> TaskPushNotificationConfig {
    TaskPushNotificationConfig {
        url: url.to_owned(),
        id: None,
        task_id: task_id.to_owned(),
        token: None,
        authentication: None,
        tenant: None,
    }
}

fn no_turns() -> MockCompletionModel {
    MockCompletionModel::from_stream_turns(Vec::<Vec<MockStreamEvent>>::new())
}

#[tokio::test]
async fn the_card_declares_push_only_when_it_is_on() -> anyhow::Result<()> {
    let off = serve(AgentBuilder::new(no_turns()).name("echo").build()).await?;
    let card = AgentCardResolver::new(None).resolve(&off).await?;
    ensure!(
        card.capabilities.push_notifications == Some(false),
        "{card:?}"
    );

    let on = serve_with(AgentBuilder::new(no_turns()).name("echo").build(), |s| {
        s.push_notifications(true)
    })
    .await?;
    let card = AgentCardResolver::new(None).resolve(&on).await?;
    ensure!(
        card.capabilities.push_notifications == Some(true),
        "{card:?}"
    );
    Ok(())
}

#[tokio::test]
async fn push_configs_deliver_task_updates_to_a_webhook() -> anyhow::Result<()> {
    let (hook, mut deliveries) = webhook().await?;
    let agent = AgentBuilder::new(MockCompletionModel::from_stream_turns([reply(&["Done"])]))
        .name("echo")
        .build();
    // The webhook is on loopback, so the test opts out of the address guard.
    let url = serve_with(agent, |s| {
        s.push_notifications(true).allow_insecure_push_urls()
    })
    .await?;
    let client = connect(&url, TRANSPORT_PROTOCOL_JSONRPC).await?;

    // The config rides on the send, so it is saved against the task it creates.
    let mut send = request("Hi", None);
    send.configuration = Some(SendMessageConfiguration {
        accepted_output_modes: None,
        task_push_notification_config: Some(config("", &hook)),
        history_length: None,
        return_immediately: None,
    });
    let mut stream = client.send_streaming_message(&send).await?;
    let mut task_id = None;
    while let Some(event) = stream.next().await {
        if let StreamResponse::Task(task) = event? {
            task_id = Some(task.id);
        }
    }
    let task_id = task_id.context("the stream starts with the task")?;

    // Updates are POSTed as they happen; the final one must arrive.
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), deliveries.recv())
            .await
            .context("the webhook got the final update in time")?
            .context("the webhook channel closed")?;
        if matches!(
            &event,
            StreamResponse::StatusUpdate(update)
                if update.task_id == task_id && update.status.state == TaskState::Completed
        ) {
            break;
        }
    }
    Ok(())
}

#[tokio::test]
async fn set_config_is_refused_when_push_is_off() -> anyhow::Result<()> {
    let url = serve(AgentBuilder::new(no_turns()).name("echo").build()).await?;
    let client = connect(&url, TRANSPORT_PROTOCOL_JSONRPC).await?;
    let error = client
        .create_push_config(&config("task-1", "https://hooks.example.com/a"))
        .await
        .err()
        .context("set-config is refused while push is off")?;
    ensure!(
        error.code == error_code::PUSH_NOTIFICATION_NOT_SUPPORTED,
        "{error:?}"
    );
    Ok(())
}

#[tokio::test]
async fn webhook_urls_are_checked_when_saved() -> anyhow::Result<()> {
    let url = serve_with(AgentBuilder::new(no_turns()).name("echo").build(), |s| {
        s.push_notifications(true)
            .push_allowed_hosts(["hooks.example.com"])
    })
    .await?;
    let client = connect(&url, TRANSPORT_PROTOCOL_JSONRPC).await?;

    for refused in [
        "http://hooks.example.com/a",
        "ftp://hooks.example.com/a",
        "https://evil.example.net/a",
    ] {
        let error = client
            .create_push_config(&config("task-1", refused))
            .await
            .err()
            .with_context(|| format!("{refused} is refused"))?;
        ensure!(
            error.code == error_code::INVALID_PARAMS,
            "{refused}: {error:?}"
        );
    }
    // Hosts compare without regard to case.
    client
        .create_push_config(&config("task-1", "https://Hooks.Example.com/a"))
        .await?;
    Ok(())
}

#[tokio::test]
async fn resubscribe_gets_the_rest_of_a_running_task() -> anyhow::Result<()> {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let tool = MockControlledTool::new(Arc::clone(&started), Arc::clone(&release));
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::tool_call("call-1", "controlled", serde_json::json!({})),
            MockStreamEvent::final_response_with_default_usage(),
        ],
        reply(&["Done"]),
    ]);
    let agent = AgentBuilder::new(model)
        .name("echo")
        .tool(tool)
        .default_max_turns(3)
        .build();
    let client = connect(&serve(agent).await?, TRANSPORT_PROTOCOL_JSONRPC).await?;

    let mut original = client
        .send_streaming_message(&request("Wait", None))
        .await?;
    let Some(Ok(StreamResponse::Task(task))) = original.next().await else {
        anyhow::bail!("the stream starts with the task");
    };
    started.notified().await;

    // A second client attaches while the task is still running.
    let mut resubscribed = client
        .subscribe_to_task(&SubscribeToTaskRequest {
            id: task.id.clone(),
            tenant: None,
        })
        .await?;
    release.notify_one();

    let drained = tokio::time::timeout(Duration::from_secs(10), async {
        let mut last = None;
        while let Some(event) = resubscribed.next().await {
            last = Some(event?);
        }
        Ok::<_, rig_a2a::a2a::A2AError>(last)
    })
    .await
    .context("the resubscribed stream ends")??;
    let Some(StreamResponse::StatusUpdate(done)) = drained else {
        anyhow::bail!("the resubscribed stream ends with a status update");
    };
    ensure!(done.status.state == TaskState::Completed, "{done:?}");

    let finished = tokio::time::timeout(Duration::from_secs(10), async {
        while original.next().await.is_some() {}
    })
    .await;
    ensure!(finished.is_ok(), "the original stream ends too");
    Ok(())
}
