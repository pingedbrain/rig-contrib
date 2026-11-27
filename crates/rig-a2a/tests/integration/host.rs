//! The `Host` allowlist: a request for another host gets 403, over loopback.
#![cfg(not(target_family = "wasm"))]

use anyhow::ensure;
use rig_a2a::{A2aServer, RemoteAgent};
use rig_agent::{Agent, AgentBuilder};
use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};

/// Bind first, then build the server at the bound URL.
async fn serve_agent(agent: Agent, hosts: Option<&[&str]>) -> anyhow::Result<String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let mut server = A2aServer::new(agent, &url);
    if let Some(hosts) = hosts {
        server = server.allowed_hosts(hosts.iter().copied());
    }
    let app = server.into_router();
    tokio::spawn(async move { rig_a2a::axum::serve(listener, app).await });
    Ok(url)
}

fn echo(text: &str) -> Agent {
    let turns = [vec![
        MockStreamEvent::text(text),
        MockStreamEvent::final_response_with_default_usage(),
    ]];
    AgentBuilder::new(MockCompletionModel::from_stream_turns(turns))
        .name("Echo")
        .description("Says things back.")
        .build()
}

/// Status of a card request sent with the given `Host` header.
async fn card_status(url: &str, host: &str) -> anyhow::Result<reqwest::StatusCode> {
    let response = reqwest::Client::new()
        .get(format!("{url}/.well-known/agent-card.json"))
        .header(reqwest::header::HOST, host)
        .send()
        .await?;
    Ok(response.status())
}

#[tokio::test]
async fn unset_allows_every_host() -> anyhow::Result<()> {
    let url = serve_agent(echo("Hi"), None).await?;
    ensure!(card_status(&url, "evil.example").await? == 200);
    let agent = RemoteAgent::builder(url.clone()).connect().await?;
    ensure!(agent.send("Hi", None).await?.text == "Hi");
    Ok(())
}

#[tokio::test]
async fn an_allowed_host_passes_and_another_gets_403() -> anyhow::Result<()> {
    let url = serve_agent(echo("Hello"), Some(&["localhost", "127.0.0.1"])).await?;
    let port = url.rsplit(':').next().unwrap_or_default();

    // Host names compare without case, and an entry without a port matches any port.
    ensure!(card_status(&url, &format!("LOCALHOST:{port}")).await? == 200);
    ensure!(card_status(&url, "evil.example").await? == 403);
    ensure!(card_status(&url, &format!("127.0.0.1:{port}")).await? == 200);

    // A call with a foreign Host is refused before the agent runs.
    let refused = reqwest::Client::new()
        .post(format!("{url}/rest/message:send"))
        .header(reqwest::header::HOST, "evil.example")
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body("{}")
        .send()
        .await?;
    ensure!(refused.status() == 403, "{}", refused.status());
    let body = refused.text().await?;
    ensure!(body.contains("Host not allowed"), "{body}");

    // A call with an allowed host still works.
    let agent = RemoteAgent::connect(&url).await?;
    ensure!(agent.send("Hi", None).await?.text == "Hello");
    Ok(())
}
