//! Rig agents and tools served over MCP, driven by the official MCP client.

use std::sync::{Arc, Mutex};

use anyhow::{Context, ensure};
use rig_agent::{Agent, AgentBuilder};
use rig_core::completion::ToolAnnotations;
use rig_core::test_utils::{MockCompletionModel, MockStreamEvent, MockTurn};
use rig_core::tool::{Tool, ToolContext, ToolExecutionError};
use rig_mcp_server::McpServer;
use rmcp::model::{CallToolRequestParams, ClientRequest, ProgressNotificationParam, Request};
use rmcp::service::{NotificationContext, PeerRequestOptions, RunningService};
use rmcp::{ClientHandler, RoleClient, ServiceExt};
use serde_json::{Value, json};

/// A plain Rig tool: adds two numbers, read-only.
struct Add;

impl Tool for Add {
    const NAME: &'static str = "add";
    type Args = Value;
    type Output = Value;
    type Error = ToolExecutionError;

    fn description(&self) -> String {
        "Add two numbers".into()
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {"a": {"type": "number"}, "b": {"type": "number"}},
            "required": ["a", "b"]})
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only_hint: Some(true),
            ..Default::default()
        }
    }

    async fn call(&self, _: &mut ToolContext, args: Value) -> Result<Value, Self::Error> {
        let n = |key: &str| {
            args[key]
                .as_f64()
                .ok_or_else(|| ToolExecutionError::other("not a number"))
        };
        Ok(json!({"sum": n("a")? + n("b")?}))
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct Task {
    title: String,
    priority: String,
}

fn streaming_agent(name: &str, replies: &[&[&str]]) -> (Agent, MockCompletionModel) {
    let model = MockCompletionModel::from_stream_turns(replies.iter().map(|chunks| {
        chunks
            .iter()
            .map(|chunk| MockStreamEvent::text(*chunk))
            .chain([MockStreamEvent::final_response_with_default_usage()])
            .collect::<Vec<_>>()
    }));
    let agent = AgentBuilder::new(model.clone())
        .name(name)
        .description("Answers questions.")
        .build();
    (agent, model)
}

/// Collects the progress messages it is sent.
#[derive(Clone, Default)]
struct Client {
    progress: Arc<Mutex<Vec<String>>>,
}

impl ClientHandler for Client {
    async fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _: NotificationContext<RoleClient>,
    ) {
        if let Some(message) = params.message {
            self.progress.lock().unwrap().push(message);
        }
    }
}

async fn connect(
    server: McpServer,
) -> anyhow::Result<(RunningService<RoleClient, Client>, Client)> {
    let (server_side, client_side) = tokio::io::duplex(64 * 1024);
    tokio::spawn(server.serve(server_side));
    let client = Client::default();
    Ok((client.clone().serve(client_side).await?, client))
}

fn call(name: &str, args: Value) -> CallToolRequestParams {
    let mut params = CallToolRequestParams::new(name.to_owned());
    params.arguments = args.as_object().cloned();
    params
}

#[tokio::test]
async fn tools_are_listed_with_their_schemas_and_annotations() -> anyhow::Result<()> {
    let (agent, _) = streaming_agent("Rig helper", &[]);
    let (typed, _) = streaming_agent("Task extractor", &[]);
    let server = McpServer::new("test")
        .agent(agent)
        .typed_agent::<Task>(typed)
        .tool(Add);
    let (client, _) = connect(server).await?;

    let tools = client.list_all_tools().await?;
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_ref()).collect();
    ensure!(
        names == ["rig_helper", "task_extractor", "add"],
        "{names:?}"
    );

    let helper = &tools[0];
    ensure!(helper.input_schema["required"] == json!(["message"]));
    ensure!(
        helper.output_schema.as_ref().context("output schema")?["properties"]["session_id"]
            .is_object()
    );
    let extractor = &tools[1];
    ensure!(
        extractor.output_schema.as_ref().context("schema of T")?["properties"]["priority"]
            .is_object()
    );
    let add = &tools[2];
    ensure!(
        add.annotations.as_ref().and_then(|a| a.read_only_hint) == Some(true),
        "{add:?}"
    );
    Ok(())
}

#[tokio::test]
async fn a_plain_tool_runs_without_a_model() -> anyhow::Result<()> {
    let (client, _) = connect(McpServer::new("test").tool(Add)).await?;
    let result = client
        .call_tool(call("add", json!({"a": 2, "b": 40})))
        .await?;
    ensure!(
        result.structured_content == Some(json!({"sum": 42.0})),
        "{result:?}"
    );
    ensure!(result.is_error != Some(true));

    // A tool's own failure is an error result, not a protocol error.
    let failed = client
        .call_tool(call("add", json!({"a": "x", "b": 1})))
        .await?;
    ensure!(failed.is_error == Some(true), "{failed:?}");
    Ok(())
}

#[tokio::test]
async fn an_agent_answers_keeps_its_session_and_streams_progress() -> anyhow::Result<()> {
    let (agent, model) = streaming_agent("Helper", &[&["I am ", "Rig."], &["You asked who I am."]]);
    let (client, handler) = connect(McpServer::new("test").agent(agent)).await?;

    // First call, with a progress token: the text arrives as progress too.
    let handle = client
        .send_cancellable_request(
            ClientRequest::CallToolRequest(Request::new(call(
                "helper",
                json!({"message": "Who are you?"}),
            ))),
            PeerRequestOptions::no_options(),
        )
        .await?;
    let response = handle.await_response().await?;
    let first: rmcp::model::CallToolResult =
        serde_json::from_value(serde_json::to_value(response)?)?;
    let structured = first.structured_content.context("structured content")?;
    ensure!(structured["answer"] == "I am Rig.", "{structured}");
    let session = structured["session_id"]
        .as_str()
        .context("a session id")?
        .to_owned();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    ensure!(
        *handler.progress.lock().unwrap() == ["I am ", "Rig."],
        "{:?}",
        handler.progress
    );

    // Second call in the same session: the model sees the first turn.
    let second = client
        .call_tool(call(
            "helper",
            json!({"message": "What did I ask?", "session_id": session}),
        ))
        .await?;
    ensure!(second.structured_content.context("structured")?["session_id"] == session.as_str());
    let seen = format!("{:?}", model.requests()[1].chat_history);
    ensure!(
        seen.contains("Who are you?") && seen.contains("I am Rig."),
        "{seen}"
    );
    Ok(())
}

#[tokio::test]
async fn a_typed_agent_answers_with_structured_content() -> anyhow::Result<()> {
    let model = MockCompletionModel::from_turns([MockTurn::text(
        r#"{"title": "Migrate payments", "priority": "high"}"#,
    )]);
    let agent = AgentBuilder::new(model).name("Task extractor").build();
    let (client, _) = connect(McpServer::new("test").typed_agent::<Task>(agent)).await?;
    let result = client
        .call_tool(call(
            "task_extractor",
            json!({"message": "Urgent: migrate payments"}),
        ))
        .await?;
    ensure!(
        result.structured_content == Some(json!({"title": "Migrate payments", "priority": "high"})),
        "{result:?}"
    );
    Ok(())
}

#[tokio::test]
async fn session_ids_that_could_escape_a_file_store_are_refused() -> anyhow::Result<()> {
    let (agent, _) = streaming_agent("Helper", &[&["unused"]]);
    let (client, _) = connect(McpServer::new("test").agent(agent)).await?;
    let refused = client
        .call_tool(call(
            "helper",
            json!({"message": "hi", "session_id": "../../etc/passwd"}),
        ))
        .await;
    ensure!(refused.is_err(), "{refused:?}");
    Ok(())
}

#[tokio::test]
async fn http_needs_the_bearer_token() -> anyhow::Result<()> {
    use rmcp::transport::StreamableHttpClientTransport;
    use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/mcp", listener.local_addr()?);
    let app = McpServer::new("test")
        .tool(Add)
        .bearer_token("s3cret")
        .into_router();
    tokio::spawn(async move { rig_mcp_server::axum::serve(listener, app).await });

    let anonymous = reqwest::Client::new()
        .post(&url)
        .header("accept", "application/json, text/event-stream")
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
        .send()
        .await?;
    ensure!(anonymous.status() == 401, "{}", anonymous.status());

    let config = StreamableHttpClientTransportConfig::with_uri(url).auth_header("s3cret");
    let client = ().serve(StreamableHttpClientTransport::from_config(config)).await?;
    let result = client
        .call_tool(call("add", json!({"a": 1, "b": 2})))
        .await?;
    ensure!(
        result.structured_content == Some(json!({"sum": 3.0})),
        "{result:?}"
    );
    Ok(())
}

#[tokio::test]
async fn a_rig_agent_uses_the_served_tools_through_rig_rmcp() -> anyhow::Result<()> {
    // Rig -> MCP -> Rig: a local agent calls the served `add` through
    // rig-rmcp, Rig's own MCP client.
    let (server_side, client_side) = tokio::io::duplex(64 * 1024);
    tokio::spawn(McpServer::new("test").tool(Add).serve(server_side));
    let connection = rig_rmcp::McpConnection::new(().serve(client_side).await?);
    let tools = connection.tools().await?;

    let model = MockCompletionModel::from_turns([
        MockTurn::tool_call("call-1", "add", json!({"a": 20, "b": 22})),
        MockTurn::text("It is 42."),
    ]);
    let mut builder = AgentBuilder::new(model).default_max_turns(3);
    for tool in tools {
        builder = builder.dynamic_tool(rig_core::tool::DynamicTool::try_from(tool)?);
    }
    let response = builder.build().prompt("What is 20 + 22?").await?;
    ensure!(response.output() == "It is 42.");
    let seen = format!("{:?}", response.messages);
    ensure!(
        seen.contains("42"),
        "the tool result came back through MCP: {seen}"
    );
    Ok(())
}

/// A tool that never returns, so an agent run using it holds its slot.
struct Hang;

impl Tool for Hang {
    const NAME: &'static str = "hang";
    type Args = Value;
    type Output = String;
    type Error = ToolExecutionError;

    fn description(&self) -> String {
        "Never returns".into()
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    async fn call(&self, _: &mut ToolContext, _: Value) -> Result<String, Self::Error> {
        std::future::pending().await
    }
}

fn hanging_agent(runs: usize) -> Agent {
    let model = MockCompletionModel::from_stream_turns((0..runs).map(|_| {
        vec![
            MockStreamEvent::tool_call("call-1", "hang", json!({})),
            MockStreamEvent::final_response_with_default_usage(),
        ]
    }));
    AgentBuilder::new(model)
        .name("Slow")
        .tool(Hang)
        .default_max_turns(2)
        .build()
}

#[tokio::test]
async fn a_cancelled_call_stops_its_run_and_frees_its_slot() -> anyhow::Result<()> {
    let server = McpServer::new("test")
        .agent(hanging_agent(2))
        .max_concurrent_calls(1);
    let (client, _) = connect(server).await?;

    let first = client
        .send_cancellable_request(
            ClientRequest::CallToolRequest(Request::new(call("slow", json!({"message": "one"})))),
            PeerRequestOptions::no_options(),
        )
        .await?;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    // While the first call holds the only slot, another is busy.
    let busy = client
        .call_tool(call("slow", json!({"message": "two"})))
        .await?;
    ensure!(busy.is_error == Some(true), "{busy:?}");
    ensure!(format!("{busy:?}").contains("busy"), "{busy:?}");

    first.cancel(None).await?;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    // The cancelled run let go of its slot: this call is not busy (it runs,
    // and is stopped by the timeout below instead).
    let third = tokio::time::timeout(
        std::time::Duration::from_millis(300),
        client.call_tool(call("slow", json!({"message": "three"}))),
    )
    .await;
    ensure!(
        third.is_err(),
        "the third call got a slot and is running: {third:?}"
    );
    Ok(())
}

#[tokio::test]
async fn a_run_past_its_time_is_stopped() -> anyhow::Result<()> {
    let server = McpServer::new("test")
        .agent(hanging_agent(1))
        .max_run_duration(std::time::Duration::from_millis(300));
    let (client, _) = connect(server).await?;
    let result = client
        .call_tool(call("slow", json!({"message": "hi"})))
        .await?;
    ensure!(result.is_error == Some(true), "{result:?}");
    ensure!(
        format!("{result:?}").contains("longer than allowed"),
        "{result:?}"
    );
    Ok(())
}

#[tokio::test]
async fn a_rate_limited_provider_is_reported_so_the_caller_can_retry() -> anyhow::Result<()> {
    // Every attempt (the first and two retries) is rate limited.
    let model = MockCompletionModel::from_turns((0..3).map(|attempt| {
        MockTurn::provider_response_error(
            http::StatusCode::TOO_MANY_REQUESTS,
            r#"{"error":{"message":"upstream rate limited","internal":"do-not-leak"}}"#,
            format!("req-{attempt}"),
        )
    }));
    // A typed agent awaits its run, so the failure arrives as the provider's
    // own error (a streamed run reports it instead; see the unit tests).
    let agent = AgentBuilder::new(model).name("Helper").build();
    let (client, _) = connect(McpServer::new("test").typed_agent::<Task>(agent)).await?;
    let result = client
        .call_tool(call("helper", json!({"message": "hi"})))
        .await?;
    let text = format!("{:?}", result.content);
    ensure!(result.is_error == Some(true), "{result:?}");
    ensure!(
        text.contains("rate limiting"),
        "the caller learns it may retry: {text}"
    );
    ensure!(
        !text.contains("do-not-leak"),
        "the provider's own text stays out: {text}"
    );
    Ok(())
}
