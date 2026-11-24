//! The MCP server: which Rig agents and tools it exposes, and how it runs them.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use futures::future::BoxFuture;
use rig_agent::Agent;
use rig_agent::agent::MultiTurnStreamItem;
use rig_agent::completion::{PromptError, StructuredOutputError};
use rig_core::message::Message;
use rig_core::serve::adapters::ToolAdapter;
use rig_core::streaming::{Item, StreamEvent};
use rig_core::tool::{DynamicTool, Tool, ToolExecutionError, ToolOutput, tool_definition};
use rig_memory::AsyncStore;
use rig_memory::recent::{Recent, TokenBudget, view};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    InitializeResult, JsonObject, ListToolsResult, PaginatedRequestParams,
    ProgressNotificationParam, ServerCapabilities, ServerConfig, ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::content::{image_blocks, tool_result};
use crate::history::{
    DEFAULT_HISTORY_TOKENS, DEFAULT_MAX_CONTEXTS, HistoryStore, InMemory, Persistent,
    valid_context_id,
};

/// How many calls run at once by default; more are answered as busy.
pub const DEFAULT_MAX_CONCURRENT_CALLS: usize = 32;
/// How long one agent run may take by default before it is stopped.
pub const DEFAULT_MAX_RUN_DURATION: Duration = Duration::from_secs(600);

/// Longest tool name MCP clients and model providers accept.
const MAX_TOOL_NAME: usize = 64;

type RunTool =
    Arc<dyn Fn(Value) -> BoxFuture<'static, Result<ToolOutput, ToolExecutionError>> + Send + Sync>;
type TypedAnswer = Arc<
    dyn Fn(
            Agent,
            Message,
            Vec<Message>,
        ) -> BoxFuture<'static, Result<(Value, Vec<Message>), String>>
        + Send
        + Sync,
>;

/// Rig agents and tools served over the Model Context Protocol.
///
/// Each [`agent`](Self::agent) becomes a tool taking a `message` and an
/// optional `session_id`: a call runs the agent with that session's history
/// and answers with its text (streamed meanwhile as progress notifications
/// when the client asks for progress) and any image it made. Each
/// [`tool`](Self::tool) is exposed as itself, run without a model.
///
/// Serve it to a local client (Claude Code, Cursor, Zed...) over stdio with
/// [`serve_stdio`](Self::serve_stdio), or over Streamable HTTP with
/// [`into_router`](Self::into_router).
pub struct McpServer {
    name: String,
    version: String,
    instructions: Option<String>,
    tools: Vec<Entry>,
    history: Arc<dyn HistoryStore>,
    window: Arc<dyn Recent>,
    slots: Arc<tokio::sync::Semaphore>,
    max_run: Duration,
    pub(crate) bearer: Option<String>,
    pub(crate) allowed_hosts: Option<Vec<String>>,
}

struct Entry {
    tool: rmcp::model::Tool,
    kind: Kind,
}

enum Kind {
    Tool(RunTool),
    Agent(Box<Agent>, Option<TypedAnswer>),
}

impl McpServer {
    /// A server introducing itself to clients as `name`, with nothing to
    /// offer yet.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            instructions: None,
            tools: Vec::new(),
            history: Arc::new(InMemory::new(DEFAULT_MAX_CONTEXTS)),
            window: Arc::new(TokenBudget::new(DEFAULT_HISTORY_TOKENS)),
            slots: Arc::new(tokio::sync::Semaphore::new(DEFAULT_MAX_CONCURRENT_CALLS)),
            max_run: DEFAULT_MAX_RUN_DURATION,
            bearer: None,
            allowed_hosts: None,
        }
    }

    /// The version clients see. Defaults to this crate's version.
    #[must_use]
    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }

    /// Instructions clients may pass to their model about using this server.
    #[must_use]
    pub fn instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = Some(instructions.into());
        self
    }

    /// Expose `agent` as a tool named after it (its name, made provider-safe),
    /// described by its description. It answers with text and any images,
    /// plus `{answer, session_id}` as structured content; passing that
    /// `session_id` back continues the conversation.
    ///
    /// # Panics
    ///
    /// If another tool already has that name.
    #[must_use]
    pub fn agent(self, agent: Agent) -> Self {
        let (name, description) = agent_identity(&agent);
        let output = json!({
            "type": "object",
            "properties": {
                "answer": {"type": "string"},
                "session_id": {"type": "string"}
            },
            "required": ["answer", "session_id"]
        });
        let tool = rmcp::model::Tool::new(name, description, Arc::new(agent_input()))
            .with_raw_output_schema(Arc::new(object(output)));
        self.add(Entry {
            tool,
            kind: Kind::Agent(Box::new(agent), None),
        })
    }

    /// Expose `agent` as a tool that answers with a `T`: each call runs as
    /// [`Agent::prompt_typed`] does (two retries when the JSON does not parse)
    /// and the `T` comes back as structured content matching the tool's
    /// output schema, the schema of `T`.
    ///
    /// # Panics
    ///
    /// If another tool already has the agent's name.
    #[must_use]
    pub fn typed_agent<T>(self, agent: Agent) -> Self
    where
        T: schemars::JsonSchema
            + serde::de::DeserializeOwned
            + serde::Serialize
            + Send
            + Sync
            + 'static,
    {
        let (name, description) = agent_identity(&agent);
        let schema = serde_json::to_value(schemars::schema_for!(T)).unwrap_or_default();
        let tool = rmcp::model::Tool::new(name, description, Arc::new(agent_input()))
            .with_raw_output_schema(Arc::new(object(schema)));
        let typed: TypedAnswer = Arc::new(|agent, prompt, history| {
            Box::pin(async move {
                let response = agent
                    .prompt_typed::<T>(prompt)
                    .history(history)
                    .retries(2)
                    .await
                    .map_err(|error| {
                        tracing::warn!(%error, "typed agent call failed");
                        match &error {
                            StructuredOutputError::Prompt(prompt) => failure_message(prompt),
                            _ => "The agent could not produce a structured answer.",
                        }
                        .to_owned()
                    })?;
                let value =
                    serde_json::to_value(&response.output).map_err(|error| error.to_string())?;
                Ok((value, response.messages))
            })
        });
        self.add(Entry {
            tool,
            kind: Kind::Agent(Box::new(agent), Some(typed)),
        })
    }

    /// Expose a Rig tool as itself: same name, description, parameters and
    /// annotations; called directly, with no model involved.
    ///
    /// # Panics
    ///
    /// If another tool already has that name.
    #[must_use]
    pub fn tool<T: Tool + 'static>(self, tool: T) -> Self {
        let definition = tool_definition(&tool);
        let adapter = Arc::new(ToolAdapter::new(tool));
        let run: RunTool = Arc::new(move |args| {
            let adapter = Arc::clone(&adapter);
            Box::pin(async move { adapter.run(args).await })
        });
        self.add_tool(definition, run)
    }

    /// Expose a [`DynamicTool`] as itself; see [`tool`](Self::tool).
    ///
    /// # Panics
    ///
    /// If another tool already has that name.
    #[must_use]
    pub fn dynamic_tool(self, tool: DynamicTool) -> Self {
        let definition = tool.definition();
        let tool = Arc::new(tool);
        let run: RunTool = Arc::new(move |args| {
            let tool = Arc::clone(&tool);
            Box::pin(async move { tool.run(args).await })
        });
        self.add_tool(definition, run)
    }

    /// Keep agent sessions in memory, at most `max` of them; the oldest one
    /// started is forgotten first. This is the default, with
    /// [`DEFAULT_MAX_CONTEXTS`](crate::DEFAULT_MAX_CONTEXTS).
    #[must_use]
    pub fn max_sessions(mut self, max: usize) -> Self {
        self.history = Arc::new(InMemory::new(max));
        self
    }

    /// Keep agent sessions in any [`rig_memory`] store (`DirStore` on disk,
    /// `rig-sqlite`, `rig-postgres`), so they survive restarts and several
    /// servers can share them.
    #[must_use]
    pub fn history_store<S>(mut self, store: S) -> Self
    where
        S: AsyncStore + Send + Sync + 'static,
    {
        self.history = Arc::new(Persistent(store));
        self
    }

    /// Which newest messages of a session each call sends the model.
    /// Defaults to [`DEFAULT_HISTORY_TOKENS`](crate::DEFAULT_HISTORY_TOKENS)
    /// tokens; a tool result is never sent without its call.
    #[must_use]
    pub fn history_window(mut self, window: impl Recent + 'static) -> Self {
        self.window = Arc::new(window);
        self
    }

    /// Run at most `max` calls at once (at least one); a call beyond that
    /// gets a "busy" error result before any model call.
    #[must_use]
    pub fn max_concurrent_calls(mut self, max: usize) -> Self {
        self.slots = Arc::new(tokio::sync::Semaphore::new(max.max(1)));
        self
    }

    /// Stop an agent run that takes longer than `max`.
    #[must_use]
    pub fn max_run_duration(mut self, max: Duration) -> Self {
        self.max_run = max;
        self
    }

    fn add_tool(self, definition: rig_core::completion::ToolDefinition, run: RunTool) -> Self {
        let mut tool = rmcp::model::Tool::new(
            definition.name.to_string(),
            definition.description,
            Arc::new(object(definition.parameters)),
        );
        // Rig's annotations and MCP's have the same wire names (readOnlyHint, ...).
        if let Some(annotations) = serde_json::to_value(&definition.annotations)
            .ok()
            .filter(|value| value.as_object().is_some_and(|fields| !fields.is_empty()))
            .and_then(|value| serde_json::from_value::<ToolAnnotations>(value).ok())
        {
            tool = tool.with_annotations(annotations);
        }
        self.add(Entry {
            tool,
            kind: Kind::Tool(run),
        })
    }

    fn add(mut self, entry: Entry) -> Self {
        assert!(
            self.tools
                .iter()
                .all(|known| known.tool.name != entry.tool.name),
            "rig-mcp-server: two tools are named `{}`",
            entry.tool.name
        );
        self.tools.push(entry);
        self
    }
}

/// The server as an MCP handler, shared by every connection.
#[derive(Clone)]
pub(crate) struct Handler(Arc<Inner>);

struct Inner {
    server: McpServer,
    by_name: HashMap<String, usize>,
}

impl Handler {
    pub(crate) fn new(server: McpServer) -> Self {
        let by_name = server
            .tools
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.tool.name.to_string(), index))
            .collect();
        Self(Arc::new(Inner { server, by_name }))
    }

    fn entry(&self, name: &str) -> Option<&Entry> {
        self.0
            .by_name
            .get(name)
            .map(|&index| &self.0.server.tools[index])
    }
}

impl ServerHandler for Handler {
    fn get_info(&self) -> ServerConfig {
        let server = &self.0.server;
        let mut info = InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                server.name.clone(),
                server.version.clone(),
            ));
        if let Some(instructions) = &server.instructions {
            info = info.with_instructions(instructions.clone());
        }
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult {
            tools: self
                .0
                .server
                .tools
                .iter()
                .map(|entry| entry.tool.clone())
                .collect(),
            ..Default::default()
        })
    }

    fn get_tool(&self, name: &str) -> Option<rmcp::model::Tool> {
        self.entry(name).map(|entry| entry.tool.clone())
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let entry = self.entry(&request.name).ok_or_else(|| {
            McpError::invalid_params(format!("no tool named `{}`", request.name), None)
        })?;
        let args = Value::Object(request.arguments.unwrap_or_default());
        let Ok(_slot) = Arc::clone(&self.0.server.slots).try_acquire_owned() else {
            return Ok(CallToolResponse::Complete(error(
                "The server is busy; try again shortly.",
            )));
        };
        let result = match &entry.kind {
            // A tool's own failure is a result the model reads, not a protocol error.
            Kind::Tool(run) => match run(args).await {
                Ok(output) => tool_result(output),
                Err(failure) => error(&failure.to_string()),
            },
            Kind::Agent(agent, typed) => {
                self.call_agent(agent, typed.as_ref(), args, &context)
                    .await?
            }
        };
        Ok(CallToolResponse::Complete(result))
    }
}

#[derive(Deserialize)]
struct AgentArgs {
    message: String,
    session_id: Option<String>,
}

impl Handler {
    async fn call_agent(
        &self,
        agent: &Agent,
        typed: Option<&TypedAnswer>,
        args: Value,
        context: &RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let server = &self.0.server;
        let AgentArgs {
            message,
            session_id,
        } = AgentArgs::deserialize(args)
            .map_err(|e| McpError::invalid_params(format!("invalid arguments: {e}"), None))?;
        let session = session_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        if !valid_context_id(&session) {
            return Err(McpError::invalid_params(
                "session_id must be 1 to 128 ASCII letters, digits, `-` or `_`",
                None,
            ));
        }
        let span = tracing::info_span!("mcp.agent_call", mcp.session_id = %session);
        let prior = match server.history.load(&session).await {
            Ok(messages) => view(&messages, server.window.as_ref())
                .iter()
                .map(|message| (**message).clone())
                .collect::<Vec<_>>(),
            Err(failure) => {
                tracing::warn!(parent: &span, %failure, "could not load the session");
                return Ok(error("The session could not be loaded."));
            }
        };
        let deadline = tokio::time::Instant::now() + server.max_run;
        let prompt = Message::user(message);

        if let Some(typed) = typed {
            let run =
                tracing::Instrument::instrument(typed(agent.clone(), prompt, prior), span.clone());
            let outcome = tokio::select! {
                _ = context.ct.cancelled() => return Ok(error("Cancelled.")),
                outcome = tokio::time::timeout_at(deadline, run) => outcome,
            };
            return Ok(match outcome {
                Err(_) => error("The agent run took longer than allowed."),
                Ok(Err(message)) => error(&message),
                Ok(Ok((value, messages))) => {
                    self.save(&session, messages, &span).await;
                    let mut result = CallToolResult::success(vec![
                        ContentBlock::text(value.to_string()),
                        ContentBlock::text(format!("session_id: {session}")),
                    ]);
                    result.structured_content = Some(value);
                    result
                }
            });
        }

        let progress = context.meta.get_progress_token();
        let mut stream = span.in_scope(|| agent.prompt(prompt).history(prior).stream());
        let mut chunks = 0_u32;
        loop {
            let item = tokio::select! {
                _ = context.ct.cancelled() => return Ok(error("Cancelled.")),
                item = tokio::time::timeout_at(deadline, stream.next()) => match item {
                    Err(_) => return Ok(error("The agent run took longer than allowed.")),
                    Ok(None) => return Ok(error("The agent run ended without an answer.")),
                    Ok(Some(item)) => item,
                },
            };
            match item {
                Ok(MultiTurnStreamItem::StreamAssistantItem(Item::Event(StreamEvent::Text {
                    text,
                    ..
                }))) => {
                    chunks += 1;
                    if let Some(token) = &progress {
                        // Progress is best effort: a client that went away
                        // still gets its answer if it comes back.
                        let _ = context
                            .peer
                            .notify_progress(
                                ProgressNotificationParam::new(token.clone(), f64::from(chunks))
                                    .with_message(text),
                            )
                            .await;
                    }
                }
                Ok(MultiTurnStreamItem::FinalResponse(response)) => {
                    self.save(&session, response.messages.clone(), &span).await;
                    let answer = response.output();
                    let mut blocks = vec![ContentBlock::text(answer.clone())];
                    blocks.extend(image_blocks(&response.content));
                    let mut result = CallToolResult::success(blocks);
                    result.structured_content =
                        Some(json!({"answer": answer, "session_id": session}));
                    return Ok(result);
                }
                Ok(_) => {}
                Err(failure) => {
                    tracing::warn!(parent: &span, %failure, "agent call failed");
                    return Ok(error(failure_message(&failure)));
                }
            }
        }
    }

    /// Add a finished run to its session. The answer stands if this fails;
    /// the next call then starts without this turn.
    async fn save(&self, session: &str, messages: Vec<Message>, span: &tracing::Span) {
        if let Err(failure) = self.0.server.history.append(session, messages).await {
            tracing::warn!(parent: span, %failure, "could not save the session");
        }
    }
}

/// What the calling model is told when a run fails: enough to decide whether
/// to retry, without the provider's own error text (which can name internals).
///
/// A failure arrives as the provider's error from an awaited run, or as its
/// report from a streamed one; both carry the status and whether to retry.
fn failure_message(failure: &PromptError) -> &'static str {
    let (status, retryable) = match failure {
        PromptError::Provider(provider) => (
            failure
                .provider_response_status()
                .map(|status| status.as_u16()),
            provider.is_retryable(),
        ),
        PromptError::Report(report) => (
            report.http_status.or(report
                .provider_response_status()
                .map(|status| status.as_u16())),
            report.retryable,
        ),
        PromptError::MaxTurns { .. } => {
            return "The agent used up its turns without finishing.";
        }
        _ => (None, false),
    };
    match status {
        Some(429) => "The model provider is rate limiting this server; try again in a moment.",
        Some(401 | 403) => "The model provider rejected this server's credentials.",
        Some(500..=599) => "The model provider is unavailable right now; try again shortly.",
        Some(_) => "The model provider rejected the request.",
        None if retryable => "The model provider could not be reached; try again shortly.",
        None => "The agent run failed.",
    }
}

fn error(text: &str) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(text)])
}

fn object(value: Value) -> JsonObject {
    match value {
        Value::Object(map) if !map.is_empty() => map,
        _ => serde_json::from_value(json!({"type": "object", "properties": {}}))
            .expect("a literal object"),
    }
}

fn agent_input() -> JsonObject {
    object(json!({
        "type": "object",
        "properties": {
            "message": {
                "type": "string",
                "description": "What to ask or tell the agent, self-contained."
            },
            "session_id": {
                "type": "string",
                "description": "The session_id of an earlier answer, to continue that conversation."
            }
        },
        "required": ["message"],
        "additionalProperties": false
    }))
}

fn agent_identity(agent: &Agent) -> (String, String) {
    let name = tool_name(agent.name().unwrap_or("agent"));
    let description = format!(
        "{} Ask it in plain language; pass the session_id of an earlier answer to continue \
         the same conversation.",
        agent.description().unwrap_or("A Rig agent.")
    );
    (name, description)
}

/// A provider-safe tool name: ASCII letters, digits and `_`, at most 64
/// characters, never empty.
pub(crate) fn tool_name(name: &str) -> String {
    let mut safe: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    safe.truncate(MAX_TOOL_NAME);
    let safe = safe.trim_matches('_');
    if safe.is_empty() {
        "agent".to_owned()
    } else {
        safe.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use rig_agent::completion::PromptError;
    use rig_core::error::{ErrorKind, ErrorReport};

    use super::{failure_message, tool_name};

    #[test]
    fn streamed_failures_are_classified_by_status_and_retryability() {
        let report = |status: Option<u16>, retryable: bool| {
            let mut report = ErrorReport::new(ErrorKind::ProviderResponse, "upstream said no")
                .with_retryable(retryable);
            if let Some(status) = status {
                report = report.with_http_status(status);
            }
            PromptError::Report(report)
        };
        assert!(failure_message(&report(Some(429), true)).contains("rate limiting"));
        assert!(failure_message(&report(Some(401), false)).contains("credentials"));
        assert!(failure_message(&report(Some(503), true)).contains("unavailable"));
        assert!(failure_message(&report(None, true)).contains("could not be reached"));
        assert_eq!(
            failure_message(&report(None, false)),
            "The agent run failed."
        );
    }

    #[test]
    fn agent_names_become_provider_safe_tool_names() {
        assert_eq!(tool_name("rig-lab assistant"), "rig_lab_assistant");
        assert_eq!(tool_name("  "), "agent");
        assert_eq!(tool_name(&"x".repeat(80)).len(), 64);
    }
}
