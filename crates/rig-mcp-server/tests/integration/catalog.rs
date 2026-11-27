//! Resources and prompts: what a client lists, reads and gets, and that a
//! server without them does not say it has them.

use std::collections::HashMap;

use anyhow::ensure;
use rig_mcp_server::McpServer;
use rmcp::ServiceError;
use rmcp::model::{
    ErrorCode, GetPromptRequestParams, PromptArgument, PromptMessage, ReadResourceRequestParams,
    ResourceContents, Role,
};
use serde_json::{Value, json};

use crate::mcp::{Add, connect};

fn server() -> McpServer {
    McpServer::new("test")
        .resource("memo://intro", "intro", "text/markdown", "# Hello")
        .binary_resource(
            "blob://logo",
            "logo",
            "application/octet-stream",
            [1_u8, 2, 3],
        )
        .prompt(
            "explain",
            "Explain a topic.",
            vec![
                PromptArgument::new("topic").with_required(true),
                PromptArgument::new("tone"),
            ],
            |args: &HashMap<String, String>| {
                let topic = args.get("topic").map_or("", String::as_str);
                let tone = args.get("tone").map_or("plain", String::as_str);
                vec![PromptMessage::new_text(
                    Role::User,
                    format!("Explain {topic} in a {tone} tone."),
                )]
            },
        )
}

#[tokio::test]
async fn resources_are_listed_and_read_back() -> anyhow::Result<()> {
    let (client, _) = connect(server()).await?;

    let listed = client.list_all_resources().await?;
    let uris: Vec<&str> = listed.iter().map(|r| r.uri.as_str()).collect();
    ensure!(uris == ["memo://intro", "blob://logo"], "{uris:?}");
    let [intro, _] = listed.as_slice() else {
        anyhow::bail!("two resources: {listed:?}");
    };
    ensure!(
        intro.mime_type.as_deref() == Some("text/markdown"),
        "{listed:?}"
    );

    let text = client
        .read_resource(ReadResourceRequestParams::new("memo://intro"))
        .await?;
    let [
        ResourceContents::TextResourceContents {
            text, mime_type, ..
        },
    ] = text.contents.as_slice()
    else {
        anyhow::bail!("one text content: {:?}", text.contents);
    };
    ensure!(text == "# Hello", "{text}");
    ensure!(mime_type.as_deref() == Some("text/markdown"));

    let blob = client
        .read_resource(ReadResourceRequestParams::new("blob://logo"))
        .await?;
    let [ResourceContents::BlobResourceContents { blob, .. }] = blob.contents.as_slice() else {
        anyhow::bail!("one blob content: {:?}", blob.contents);
    };
    ensure!(blob == "AQID", "base64 of [1, 2, 3]: {blob}");

    let missing = client
        .read_resource(ReadResourceRequestParams::new("memo://nope"))
        .await
        .err();
    ensure!(
        matches!(&missing, Some(ServiceError::McpError(data)) if data.code == ErrorCode::RESOURCE_NOT_FOUND),
        "an unknown URI is -32002 (resource_not_found): {missing:?}"
    );
    Ok(())
}

#[tokio::test]
async fn prompts_are_listed_and_rendered_with_their_arguments() -> anyhow::Result<()> {
    let (client, _) = connect(server()).await?;

    let listed = client.list_all_prompts().await?;
    let [prompt] = listed.as_slice() else {
        anyhow::bail!("one prompt: {listed:?}");
    };
    ensure!(prompt.name == "explain", "{prompt:?}");
    let arguments = serde_json::to_value(&prompt.arguments)?;
    ensure!(
        arguments
            == json!([
                {"name": "topic", "required": true},
                {"name": "tone"}
            ]),
        "{arguments}"
    );

    let mut params = GetPromptRequestParams::new("explain");
    params.arguments = json!({"topic": "borrowing", "tone": "friendly"})
        .as_object()
        .cloned();
    let rendered = serde_json::to_value(client.get_prompt(params).await?)?;
    ensure!(
        rendered.pointer("/messages/0/content/text")
            == Some(&json!("Explain borrowing in a friendly tone.")),
        "{rendered}"
    );

    // An optional argument left out takes the template's own default.
    let mut params = GetPromptRequestParams::new("explain");
    params.arguments = json!({"topic": "lifetimes"}).as_object().cloned();
    let rendered = serde_json::to_value(client.get_prompt(params).await?)?;
    ensure!(
        rendered.pointer("/messages/0/content/text")
            == Some(&json!("Explain lifetimes in a plain tone.")),
        "{rendered}"
    );
    Ok(())
}

#[tokio::test]
async fn a_missing_required_prompt_argument_is_an_invalid_params_error() -> anyhow::Result<()> {
    let (client, _) = connect(server()).await?;

    let error = client
        .get_prompt(GetPromptRequestParams::new("explain"))
        .await
        .err()
        .ok_or_else(|| anyhow::anyhow!("a call without `topic` must fail"))?;
    let ServiceError::McpError(data) = error else {
        anyhow::bail!("an MCP error, not {error:?}");
    };
    ensure!(data.code == ErrorCode::INVALID_PARAMS, "{data:?}");
    ensure!(data.message.contains("topic"), "{data:?}");

    let mut params = GetPromptRequestParams::new("explain");
    params.arguments = json!({"topic": 3}).as_object().cloned();
    let error = client.get_prompt(params).await.err();
    ensure!(
        matches!(&error, Some(ServiceError::McpError(data)) if data.code == ErrorCode::INVALID_PARAMS),
        "{error:?}"
    );
    Ok(())
}

#[tokio::test]
async fn a_server_without_resources_or_prompts_does_not_advertise_them() -> anyhow::Result<()> {
    let (client, _) = connect(McpServer::new("test").tool(Add)).await?;
    let info = client
        .peer_info()
        .ok_or_else(|| anyhow::anyhow!("the server sent its info"))?;
    let capabilities = serde_json::to_value(&info.capabilities)?;
    ensure!(
        capabilities == json!({"tools": {}}),
        "tools only: {capabilities}"
    );
    Ok(())
}

#[tokio::test]
async fn a_server_with_a_resource_advertises_resources_and_not_prompts() -> anyhow::Result<()> {
    let (client, _) =
        connect(McpServer::new("test").resource("memo://a", "a", "text/plain", "a")).await?;
    let info = client
        .peer_info()
        .ok_or_else(|| anyhow::anyhow!("the server sent its info"))?;
    let capabilities: Value = serde_json::to_value(&info.capabilities)?;
    ensure!(
        capabilities == json!({"resources": {}, "tools": {}}),
        "resources, and tools as before, but no prompts: {capabilities}"
    );
    Ok(())
}
