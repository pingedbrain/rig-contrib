//! The `Host` allowlist of the HTTP endpoint: by default loopback names only,
//! and an explicit list replaces that default.

use anyhow::ensure;
use reqwest::header::HOST;
use rig_mcp_server::McpServer;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Binds first, then serves the MCP router at the bound address.
async fn serve(server: McpServer) -> anyhow::Result<String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let app = server.tool(super::mcp::Add).into_router()?;
    tokio::spawn(async move { rig_mcp_server::axum::serve(listener, app).await });
    Ok(address.to_string())
}

/// Status of an `initialize` request sent with the given `Host` header.
async fn status_with_host(address: &str, host: &str) -> anyhow::Result<reqwest::StatusCode> {
    let response = reqwest::Client::new()
        .post(format!("http://{address}/mcp"))
        .header(HOST, host)
        .header("accept", "application/json, text/event-stream")
        .json(
            &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "test", "version": "0"}}}),
        )
        .send()
        .await?;
    Ok(response.status())
}

#[tokio::test]
async fn by_default_loopback_names_pass_and_another_host_gets_403() -> anyhow::Result<()> {
    let address = serve(McpServer::new("test")).await?;
    let port = address.rsplit(':').next().unwrap_or_default();

    ensure!(
        status_with_host(&address, &format!("localhost:{port}"))
            .await?
            .is_success()
    );
    ensure!(
        status_with_host(&address, &format!("127.0.0.1:{port}"))
            .await?
            .is_success()
    );
    ensure!(status_with_host(&address, "evil.example").await? == 403);
    Ok(())
}

#[tokio::test]
async fn an_explicit_list_replaces_the_default() -> anyhow::Result<()> {
    let server = McpServer::new("test").allowed_hosts(["mcp.example.com"]);
    let address = serve(server).await?;
    let port = address.rsplit(':').next().unwrap_or_default();

    ensure!(
        status_with_host(&address, "mcp.example.com")
            .await?
            .is_success()
    );
    ensure!(status_with_host(&address, &format!("localhost:{port}")).await? == 403);
    Ok(())
}

#[tokio::test]
async fn a_request_without_a_host_header_gets_400() -> anyhow::Result<()> {
    let address = serve(McpServer::new("test")).await?;
    let mut stream = tokio::net::TcpStream::connect(&address).await?;
    stream
        .write_all(b"GET /mcp HTTP/1.1\r\nConnection: close\r\n\r\n")
        .await?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply).await?;
    ensure!(reply.starts_with("HTTP/1.1 400"), "{reply}");
    Ok(())
}
