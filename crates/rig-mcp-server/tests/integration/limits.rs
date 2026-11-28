//! The session and window caps: the oldest session is forgotten past
//! `max_sessions`, and `history_window` bounds what a call sends the model.

use anyhow::{Context, ensure};
use rig_mcp_server::McpServer;
use rig_mcp_server::rig_memory::recent::LastMessages;
use serde_json::{Value, json};

use super::mcp::{call, connect, streaming_agent};

#[tokio::test]
async fn max_sessions_forgets_the_oldest_session_first() -> anyhow::Result<()> {
    let (agent, model) = streaming_agent(
        "Helper",
        &[&["alpha reply"], &["beta reply"], &["alpha again"]],
    );
    let (client, _) = connect(McpServer::new("test").max_sessions(1).agent(agent)).await?;
    let ask = |session: &str, message: &str| {
        client.call_tool(call(
            "helper",
            json!({"message": message, "session_id": session}),
        ))
    };
    ask("alpha", "alpha first").await?;
    // A second session evicts "alpha", the only one kept.
    ask("beta", "beta first").await?;
    ask("alpha", "alpha second").await?;

    let requests = model.requests();
    let third = requests.get(2).context("a third request")?;
    let seen = format!("{:?}", third.chat_history);
    ensure!(
        !seen.contains("alpha first") && !seen.contains("alpha reply"),
        "the evicted session kept its history: {seen}"
    );
    ensure!(seen.contains("alpha second"), "{seen}");
    Ok(())
}

#[tokio::test]
async fn history_window_bounds_what_the_model_sees() -> anyhow::Result<()> {
    let (agent, model) = streaming_agent("Helper", &[&["one"], &["two"], &["three"]]);
    let (client, _) = connect(
        McpServer::new("test")
            .history_window(LastMessages(2))
            .agent(agent),
    )
    .await?;
    let first = client
        .call_tool(call("helper", json!({"message": "first"})))
        .await?;
    let session = first
        .structured_content
        .as_ref()
        .and_then(|content| content.get("session_id"))
        .and_then(Value::as_str)
        .context("a session id")?
        .to_owned();
    for message in ["second", "third"] {
        client
            .call_tool(call(
                "helper",
                json!({"message": message, "session_id": session}),
            ))
            .await?;
    }

    let requests = model.requests();
    let third = requests.get(2).context("a third request")?;
    let seen = format!("{:?}", third.chat_history);
    ensure!(
        !seen.contains("first"),
        "the oldest turn is out of the window: {seen}"
    );
    ensure!(seen.contains("second") && seen.contains("two"), "{seen}");
    Ok(())
}
