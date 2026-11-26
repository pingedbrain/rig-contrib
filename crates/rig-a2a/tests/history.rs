//! Conversations kept in a rig-memory store: shared across servers (a
//! restart, or replicas), windowed before they reach the model, and keyed by
//! context ids that cannot escape a file store.
#![cfg(not(target_family = "wasm"))]

use anyhow::ensure;
use rig_a2a::a2a::TaskState;
use rig_a2a::rig_memory::MemoryStore;
use rig_a2a::rig_memory::recent::LastMessages;
use rig_a2a::{A2aServer, RemoteAgent};
use rig_agent::AgentBuilder;
use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};

fn model(replies: &[&str]) -> MockCompletionModel {
    MockCompletionModel::from_stream_turns(replies.iter().map(|text| {
        vec![
            MockStreamEvent::text(*text),
            MockStreamEvent::final_response_with_default_usage(),
        ]
    }))
}

async fn serve(server: impl FnOnce(String) -> A2aServer) -> anyhow::Result<String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let app = server(url.clone()).into_router();
    tokio::spawn(async move { rig_a2a::axum::serve(listener, app).await });
    Ok(url)
}

#[tokio::test]
async fn a_second_server_on_the_same_store_continues_the_conversation() -> anyhow::Result<()> {
    let store = MemoryStore::default();

    let first_model = model(&["Nice to meet you, Ada."]);
    let first = {
        let (model, store) = (first_model.clone(), store.clone());
        serve(move |url| {
            A2aServer::new(AgentBuilder::new(model).name("Echo").build(), url).history_store(store)
        })
        .await?
    };
    let reply = RemoteAgent::connect(&first)
        .await?
        .send("I am Ada.", None)
        .await?;
    let context = reply.context_id.expect("a context");

    // Another server (a restart, a replica) on the same store.
    let second_model = model(&["You are Ada."]);
    let second = {
        let model = second_model.clone();
        serve(move |url| {
            A2aServer::new(AgentBuilder::new(model).name("Echo").build(), url).history_store(store)
        })
        .await?
    };
    let reply = RemoteAgent::connect(&second)
        .await?
        .send("Who am I?", Some(&context))
        .await?;
    ensure!(reply.text == "You are Ada.");

    let requests = second_model.requests();
    let seen = format!("{:?}", requests[0].chat_history);
    ensure!(
        seen.contains("I am Ada.") && seen.contains("Nice to meet you, Ada."),
        "the second server's model saw the first turn: {seen}"
    );
    Ok(())
}

#[tokio::test]
async fn the_window_bounds_what_the_model_sees() -> anyhow::Result<()> {
    let windowed = model(&["one", "two", "three"]);
    let url = {
        let model = windowed.clone();
        serve(move |url| {
            A2aServer::new(AgentBuilder::new(model).name("Echo").build(), url)
                .history_window(LastMessages(2))
        })
        .await?
    };
    let remote = RemoteAgent::connect(&url).await?;
    let first = remote.send("first", None).await?;
    let context = first.context_id.as_deref();
    remote.send("second", context).await?;
    remote.send("third", context).await?;

    let third = &windowed.requests()[2];
    let seen = format!("{:?}", third.chat_history);
    ensure!(
        !seen.contains("first"),
        "the oldest turn is out of the window: {seen}"
    );
    ensure!(
        seen.contains("second") && seen.contains("two"),
        "the last turn is in: {seen}"
    );
    Ok(())
}

#[tokio::test]
async fn context_ids_that_could_escape_a_file_store_are_rejected() -> anyhow::Result<()> {
    let url = serve(|url| {
        A2aServer::new(
            AgentBuilder::new(model(&["unused"])).name("Echo").build(),
            url,
        )
        .history_store(MemoryStore::default())
    })
    .await?;
    let remote = RemoteAgent::connect(&url).await?;
    let reply = remote.send("hi", Some("../../etc/passwd")).await?;
    ensure!(reply.state == Some(TaskState::Rejected), "{reply:?}");
    ensure!(
        reply.text.contains("contextId"),
        "the reason is given: {reply:?}"
    );
    Ok(())
}
