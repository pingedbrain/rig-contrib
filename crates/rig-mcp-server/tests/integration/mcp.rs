//! Rig agents and tools served over MCP, driven by the official MCP client.

use std::sync::{Arc, Mutex, PoisonError};

use anyhow::{Context, ensure};
use rig_agent::{Agent, AgentBuilder};
use rig_core::completion::ToolAnnotations;
use rig_core::test_utils::{MockCompletionModel, MockStreamEvent, MockTurn};
use rig_core::tool::{Tool, ToolContext, ToolExecutionError, args_schema};
use rig_mcp_server::McpServer;
use rmcp::model::{CallToolRequestParams, ClientRequest, ProgressNotificationParam, Request};
use rmcp::service::{NotificationContext, PeerRequestOptions, RunningService};
use rmcp::{ClientHandler, RoleClient, ServiceExt};
use schemars::JsonSchema;
use serde_json::{Value, json};
use tokio::sync::watch;

/// A plain Rig tool: adds two numbers, read-only.
pub(crate) struct Add;

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
            args.get(key)
                .and_then(Value::as_f64)
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

pub(crate) fn streaming_agent(name: &str, replies: &[&[&str]]) -> (Agent, MockCompletionModel) {
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
pub(crate) struct Client {
    progress: Arc<Mutex<Vec<String>>>,
}

impl ClientHandler for Client {
    async fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _: NotificationContext<RoleClient>,
    ) {
        if let Some(message) = params.message {
            self.progress
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(message);
        }
    }
}

pub(crate) async fn connect(
    server: McpServer,
) -> anyhow::Result<(RunningService<RoleClient, Client>, Client)> {
    let (server_side, client_side) = tokio::io::duplex(64 * 1024);
    tokio::spawn(server.serve(server_side));
    let client = Client::default();
    Ok((client.clone().serve(client_side).await?, client))
}

pub(crate) fn call(name: &str, args: Value) -> CallToolRequestParams {
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

    let [helper, extractor, add] = tools.as_slice() else {
        anyhow::bail!("three tools: {names:?}");
    };
    ensure!(helper.input_schema.get("required") == Some(&json!(["message"])));
    let output = serde_json::to_value(&helper.output_schema)?;
    ensure!(
        output
            .pointer("/properties/session_id")
            .is_some_and(Value::is_object),
        "{output}"
    );
    let output = serde_json::to_value(&extractor.output_schema)?;
    ensure!(
        output
            .pointer("/properties/priority")
            .is_some_and(Value::is_object),
        "schema of T: {output}"
    );
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

/// A typed Rig tool: its arguments are a struct, and its schema is derived
/// from that struct.
#[derive(serde::Deserialize, JsonSchema)]
struct Lookup {
    /// The city to look up.
    city: String,
    /// How many days ahead, if not today.
    days: Option<u8>,
}

struct LookupWeather;

impl Tool for LookupWeather {
    const NAME: &'static str = "lookup_weather";
    type Args = Lookup;
    type Output = Value;
    type Error = ToolExecutionError;

    fn description(&self) -> String {
        "Look up the weather in a city.".into()
    }

    fn parameters(&self) -> Value {
        args_schema::<Lookup>()
    }

    async fn call(&self, _: &mut ToolContext, args: Lookup) -> Result<Value, Self::Error> {
        Ok(json!({"city": args.city, "days": args.days}))
    }
}

#[tokio::test]
async fn a_typed_tool_lists_the_schema_of_its_arguments() -> anyhow::Result<()> {
    let (client, _) = connect(McpServer::new("test").tool(LookupWeather)).await?;
    let tools = client.list_all_tools().await?;
    let [lookup] = tools.as_slice() else {
        anyhow::bail!("one tool: {tools:?}");
    };
    ensure!(lookup.name.as_ref() == "lookup_weather", "{lookup:?}");
    let schema = Value::Object(lookup.input_schema.as_ref().clone());
    ensure!(
        schema.get("required") == Some(&json!(["city"])),
        "{schema:?}"
    );
    ensure!(
        schema.pointer("/properties/city/type") == Some(&json!("string")),
        "{schema:?}"
    );
    ensure!(
        schema.pointer("/properties/days/type") == Some(&json!("integer")),
        "{schema:?}"
    );
    ensure!(
        schema.pointer("/properties/city/description") == Some(&json!("The city to look up.")),
        "{schema:?}"
    );
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
    ensure!(
        structured.get("answer") == Some(&json!("I am Rig.")),
        "{structured}"
    );
    let session = structured
        .get("session_id")
        .and_then(Value::as_str)
        .context("a session id")?
        .to_owned();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    ensure!(
        *handler
            .progress
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            == ["I am ", "Rig."],
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
    let structured = second.structured_content.context("structured")?;
    ensure!(structured.get("session_id") == Some(&json!(session)));
    let requests = model.requests();
    let request = requests.get(1).context("a second request")?;
    let seen = format!("{:?}", request.chat_history);
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
        .into_router()?;
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
async fn http_refuses_a_body_over_the_limit_with_413() -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/mcp", listener.local_addr()?);
    let app = McpServer::new("test")
        .tool(Add)
        .max_request_bytes(1024)
        .into_router()?;
    tokio::spawn(async move { rig_mcp_server::axum::serve(listener, app).await });
    let post = |body: serde_json::Value| {
        reqwest::Client::new()
            .post(&url)
            .header("accept", "application/json, text/event-stream")
            .json(&body)
            .send()
    };

    let initialize = |padding: &str| {
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "test", "version": "0"}, "padding": padding}})
    };
    let small = post(initialize("")).await?;
    ensure!(small.status().is_success(), "{}", small.status());

    let large = post(initialize(&"x".repeat(2048))).await?;
    ensure!(large.status() == 413, "{}", large.status());
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

/// A tool that never returns, so an agent run using it holds its slot. Each
/// entry bumps the count the receiver from [`Hang::new`] watches.
#[derive(Clone)]
struct Hang {
    entered: Arc<watch::Sender<usize>>,
}

impl Hang {
    fn new() -> (Self, watch::Receiver<usize>) {
        let (entered, receiver) = watch::channel(0);
        (
            Self {
                entered: Arc::new(entered),
            },
            receiver,
        )
    }
}

/// Wait until the tool has been entered `count` times, or fail after 5 s.
async fn entered_times(entered: &mut watch::Receiver<usize>, count: usize) -> anyhow::Result<()> {
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        entered.wait_for(|entries| *entries >= count),
    )
    .await
    .context("the tool was not entered in time")?
    .map_err(|_| anyhow::anyhow!("the hang tool was dropped"))?;
    Ok(())
}

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
        self.entered.send_modify(|entries| *entries += 1);
        std::future::pending().await
    }
}

fn hanging_agent(runs: usize, hang: Hang) -> Agent {
    let model = MockCompletionModel::from_stream_turns((0..runs).map(|_| {
        vec![
            MockStreamEvent::tool_call("call-1", "hang", json!({})),
            MockStreamEvent::final_response_with_default_usage(),
        ]
    }));
    AgentBuilder::new(model)
        .name("Slow")
        .tool(hang)
        .default_max_turns(2)
        .build()
}

#[tokio::test]
async fn a_cancelled_call_stops_its_run_and_frees_its_slot() -> anyhow::Result<()> {
    let (hang, mut entered) = Hang::new();
    let server = McpServer::new("test")
        .agent(hanging_agent(2, hang))
        .max_concurrent_calls(1);
    let (client, _) = connect(server).await?;

    let first = client
        .send_cancellable_request(
            ClientRequest::CallToolRequest(Request::new(call("slow", json!({"message": "one"})))),
            PeerRequestOptions::no_options(),
        )
        .await?;
    entered_times(&mut entered, 1).await?;
    // While the first call holds the only slot, another is busy.
    let busy = client
        .call_tool(call("slow", json!({"message": "two"})))
        .await?;
    ensure!(busy.is_error == Some(true), "{busy:?}");
    ensure!(format!("{busy:?}").contains("busy"), "{busy:?}");

    first.cancel(None).await?;
    // The cancelled run let go of its slot: this call is not busy, so it
    // runs and enters the tool (it never returns, so it cannot finish first).
    let third = client.call_tool(call("slow", json!({"message": "three"})));
    tokio::pin!(third);
    tokio::select! {
        result = &mut third => {
            anyhow::bail!("the third call got no slot and returned: {result:?}");
        }
        entries = entered_times(&mut entered, 2) => entries?,
    }
    Ok(())
}

#[tokio::test]
async fn a_run_past_its_time_is_stopped() -> anyhow::Result<()> {
    let server = McpServer::new("test")
        .agent(hanging_agent(1, Hang::new().0))
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

#[tokio::test]
async fn a_tool_agent_without_a_turn_budget_tells_the_caller_how_to_raise_it() -> anyhow::Result<()>
{
    // No `default_max_turns`: the one-call budget ends the run after the tool call.
    let model = MockCompletionModel::from_stream_turns([vec![
        MockStreamEvent::tool_call("call-1", "add", json!({"a": 1, "b": 2})),
        MockStreamEvent::final_response_with_default_usage(),
    ]]);
    let agent = AgentBuilder::new(model).name("Helper").tool(Add).build();
    let (client, _) = connect(McpServer::new("test").agent(agent)).await?;
    let result = client
        .call_tool(call("helper", json!({"message": "hi"})))
        .await?;
    let text = format!("{:?}", result.content);
    ensure!(result.is_error == Some(true), "{result:?}");
    ensure!(
        text.contains("turn limit (1)") && text.contains("AgentBuilder::default_max_turns(n)"),
        "the hint names the limit and the fix: {text}"
    );
    Ok(())
}

#[tokio::test]
async fn a_run_that_finishes_gets_no_turn_hint() -> anyhow::Result<()> {
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::tool_call("call-1", "add", json!({"a": 1, "b": 2})),
            MockStreamEvent::final_response_with_default_usage(),
        ],
        vec![
            MockStreamEvent::text("Done."),
            MockStreamEvent::final_response_with_default_usage(),
        ],
    ]);
    let agent = AgentBuilder::new(model)
        .name("Helper")
        .tool(Add)
        .default_max_turns(2)
        .build();
    let (client, _) = connect(McpServer::new("test").agent(agent)).await?;
    let result = client
        .call_tool(call("helper", json!({"message": "hi"})))
        .await?;
    let text = format!("{:?}", result.content);
    ensure!(result.is_error != Some(true), "{result:?}");
    ensure!(text.contains("Done."), "{text}");
    ensure!(!text.contains("default_max_turns"), "{text}");
    Ok(())
}

#[tokio::test]
async fn a_budget_that_runs_out_says_to_raise_it() -> anyhow::Result<()> {
    // The budget is set, but the model keeps calling the tool past it.
    let model = MockCompletionModel::from_stream_turns((0..2).map(|attempt| {
        vec![
            MockStreamEvent::tool_call(format!("call-{attempt}"), "add", json!({"a": 1, "b": 2})),
            MockStreamEvent::final_response_with_default_usage(),
        ]
    }));
    let agent = AgentBuilder::new(model)
        .name("Helper")
        .tool(Add)
        .default_max_turns(2)
        .build();
    let (client, _) = connect(McpServer::new("test").agent(agent)).await?;
    let result = client
        .call_tool(call("helper", json!({"message": "keep going"})))
        .await?;
    let text = format!("{:?}", result.content);
    ensure!(result.is_error == Some(true), "{result:?}");
    ensure!(
        text.contains("turn limit (2)")
            && text.contains("raise AgentBuilder::default_max_turns above 2"),
        "the hint names the budget to raise: {text}"
    );
    Ok(())
}
