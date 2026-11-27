//! Remote A2A agents as Rig tools.
//!
//! [`RemoteAgent::connect`] reads an agent's card and talks to it in the
//! protocol version the card declares: v1.0 through the official client, or
//! v0.3 JSON-RPC for agents that have not upgraded (see [`crate::v03`]).
//! [`RemoteAgent::stream`] follows an answer as it is written;
//! [`RemoteAgent::send`] waits for the whole of it. [`RemoteAgent::into_tool`]
//! turns the agent into a [`DynamicTool`] a Rig agent can call, so a local
//! agent can delegate to any A2A agent, whatever framework it is written in.

use std::sync::Arc;
use std::time::Duration;

use a2a::{
    A2AError, AgentCard, Artifact, Message, Part, PartContent, Role, SendMessageRequest,
    SendMessageResponse, StreamResponse, TaskState,
};
use a2a_client::auth::AuthInterceptor;
use a2a_client::{A2AClient, A2AClientFactory, Transport};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures::stream::BoxStream;
use futures::{Stream, StreamExt};
use rig_core::message::{
    DocumentSourceKind, EmptyToolName, Image, ImageMediaType, MimeType, ToolName, ToolResultContent,
};
use rig_core::tool::{DynamicTool, ToolExecutionError, ToolOutput};
use rig_core::wasm_compat::WasmBoxedStream;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::v03;

/// How long a remote agent may stay silent before a call gives up, unless
/// told otherwise: the whole wait without streaming, the gap between two
/// events with it.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// Longest tool name most providers accept.
const MAX_TOOL_NAME: usize = 64;
/// Largest image a tool result hands the model.
const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
/// Largest text file a tool result inlines.
const MAX_INLINE_TEXT: usize = 20_000;
/// Largest agent card read from a remote agent.
const MAX_CARD_BYTES: usize = 1024 * 1024;

/// Why a remote agent could not be reached or did not answer.
#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    /// The agent answered with an A2A error.
    #[error("A2A error {}: {}", .0.code, .0.message)]
    A2a(#[from] A2AError),
    /// The HTTP request itself failed.
    #[error("HTTP: {0}")]
    Http(#[from] reqwest::Error),
    /// The agent card is missing or in neither v1.0 nor v0.3 shape.
    #[error("agent card: {0}")]
    Card(String),
    /// The agent stayed silent for longer than the timeout.
    #[error("no answer within {0:?}")]
    Timeout(Duration),
}

/// An A2A agent reached over the network.
#[derive(Clone)]
pub struct RemoteAgent {
    card: AgentCard,
    link: Link,
    timeout: Duration,
}

#[derive(Clone)]
enum Link {
    V1(Arc<A2AClient<Box<dyn Transport>>>),
    V03 {
        http: reqwest::Client,
        url: String,
        bearer: Option<String>,
    },
}

/// Options for [`RemoteAgent`]: a bearer token and a timeout.
#[derive(Debug, Clone)]
pub struct RemoteAgentBuilder {
    base_url: String,
    bearer: Option<String>,
    timeout: Duration,
}

/// One answer from a remote agent.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Reply {
    /// The answer's text parts, one per line. A finished task answers in its
    /// artifacts; a task in any other state, in its status message.
    pub text: String,
    /// The answer's structured data parts.
    pub data: Vec<Value>,
    /// The answer's file parts.
    pub files: Vec<RemoteFile>,
    /// The conversation to send follow-ups in, when the agent gave one.
    pub context_id: Option<String>,
    /// The task's state, when the agent answered with a task.
    pub state: Option<TaskState>,
}

/// A file in an answer.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteFile {
    /// The file's name, when the agent gave one.
    pub name: Option<String>,
    /// The file's media type, when the agent gave one.
    pub media_type: Option<String>,
    /// The file itself, or where to fetch it.
    pub content: FileContent,
}

/// Where a [`RemoteFile`]'s bytes are.
#[derive(Debug, Clone, PartialEq)]
pub enum FileContent {
    /// Sent inline.
    Bytes(Vec<u8>),
    /// To be fetched from this URL.
    Url(String),
}

/// What [`RemoteAgent::stream`] reports while an answer is written.
#[derive(Debug, Clone, PartialEq)]
pub enum ReplyEvent {
    /// The task moved to this state (working, input required, ...).
    Status(TaskState),
    /// More answer text, to append to what came before.
    Text(String),
    /// The agent rewrote its answer so far; this replaces all text before it.
    Rewrite(String),
    /// The whole answer. Always the last event of a stream that did not fail.
    Done(Reply),
}

impl RemoteAgent {
    /// Connect to the agent whose card is under `base_url`, with no
    /// credentials and the [`DEFAULT_TIMEOUT`]. The card is read up to 1 MiB;
    /// a larger one is a [`RemoteError::Card`].
    pub async fn connect(base_url: &str) -> Result<Self, RemoteError> {
        Self::builder(base_url).connect().await
    }

    /// Options for connecting to the agent whose card is under `base_url`.
    pub fn builder(base_url: impl Into<String>) -> RemoteAgentBuilder {
        RemoteAgentBuilder {
            base_url: base_url.into(),
            bearer: None,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// The agent card, in v1.0 shape whatever version the agent speaks.
    pub fn card(&self) -> &AgentCard {
        &self.card
    }

    /// Whether the agent's card says it streams its answers.
    pub fn streams(&self) -> bool {
        self.card.capabilities.streaming == Some(true)
    }

    /// Send `text`, continuing `context_id`'s conversation when given, and
    /// wait for the whole answer. An agent that streams is followed event by
    /// event, so a long task that keeps reporting is not cut short; one that
    /// does not must answer within the timeout.
    pub async fn send(&self, text: &str, context_id: Option<&str>) -> Result<Reply, RemoteError> {
        if self.streams() {
            let mut events = self.stream(text, context_id).await?;
            while let Some(event) = events.next().await {
                if let ReplyEvent::Done(reply) = event? {
                    return Ok(reply);
                }
            }
            return Err(A2AError::new(
                a2a::error_code::INVALID_AGENT_RESPONSE,
                "the stream ended without an answer",
            )
            .into());
        }
        self.whole(text, context_id).await
    }

    /// One request, one response, within the timeout: for agents that do not stream.
    async fn whole(&self, text: &str, context_id: Option<&str>) -> Result<Reply, RemoteError> {
        let message = user_message(text, context_id);
        let answer = async {
            Ok::<_, RemoteError>(match &self.link {
                Link::V1(client) => match client.send_message(&request(message)).await? {
                    SendMessageResponse::Task(task) => StreamResponse::Task(task),
                    SendMessageResponse::Message(message) => StreamResponse::Message(message),
                },
                Link::V03 { http, url, bearer } => {
                    v03::send(http, url, bearer.as_deref(), &message).await?
                }
            })
        };
        let answer = tracing::Instrument::instrument(answer, self.call_span());
        let response = tokio::time::timeout(self.timeout, answer)
            .await
            .map_err(|_| RemoteError::Timeout(self.timeout))??;
        let mut assembly = Assembly::default();
        assembly.apply(response);
        Ok(assembly.finish())
    }

    /// Send `text` and follow the answer as it is written: state changes,
    /// text as it grows, then [`ReplyEvent::Done`] with the whole answer.
    /// Each event must arrive within the timeout of the one before. An agent
    /// that does not stream yields its whole answer as one `Done`.
    ///
    /// Over A2A v0.3, one event may be 16 MiB before its blank line; a longer
    /// one ends the stream with an error. A v0.3 `message/send` reply, which
    /// an agent that does not stream sends instead, is capped at 16 MiB too.
    pub async fn stream(
        &self,
        text: &str,
        context_id: Option<&str>,
    ) -> Result<WasmBoxedStream<'static, Result<ReplyEvent, RemoteError>>, RemoteError> {
        if !self.streams() {
            let reply = self.whole(text, context_id).await?;
            return Ok(Box::pin(futures::stream::iter([Ok(ReplyEvent::Done(
                reply,
            ))])));
        }
        let message = user_message(text, context_id);
        let open = async {
            Ok::<_, RemoteError>(match &self.link {
                Link::V1(client) => client
                    .send_streaming_message(&request(message))
                    .await?
                    .map(|event| event.map_err(RemoteError::from))
                    .boxed(),
                Link::V03 { http, url, bearer } => {
                    v03::stream(http, url, bearer.as_deref(), &message).await?
                }
            })
        };
        let timeout = self.timeout;
        let span = self.call_span();
        let open = tracing::Instrument::instrument(open, span.clone());
        let events = tokio::time::timeout(timeout, open)
            .await
            .map_err(|_| RemoteError::Timeout(timeout))??;
        Ok(Box::pin(follow(events, timeout, span)))
    }

    /// The span one call runs in, from the request until the answer is
    /// whole. With the `otel` feature, the call carries its trace context.
    fn call_span(&self) -> tracing::Span {
        tracing::info_span!("a2a.call", otel.kind = "client", a2a.agent = %self.card.name)
    }

    /// This agent as a tool named after its card, taking a `message` and an
    /// optional `context_id` to continue a conversation the tool started.
    ///
    /// The model gets the answer as JSON (`reply`, `context_id`, `state`,
    /// and any `data` and `files`), followed by any image the agent sent.
    ///
    /// Returns [`EmptyToolName`] when the tool name is empty, as
    /// [`Agent::into_tool`](rig_agent::Agent::into_tool) does; a card whose
    /// name has no usable characters is named `remote_agent` instead.
    pub fn into_tool(self) -> Result<DynamicTool, EmptyToolName> {
        let name = ToolName::new(tool_name(&self.card.name))?;
        let description = tool_description(&self.card);
        let parameters = json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "What to ask or tell the agent, self-contained."
                },
                "context_id": {
                    "type": "string",
                    "description": "The context_id of an earlier reply, to continue that conversation."
                }
            },
            "required": ["message"],
            "additionalProperties": false
        });
        let tool =
            DynamicTool::new_with_context(name, description, parameters, move |_context, args| {
                let agent = self.clone();
                Box::pin(async move {
                    let args = Args::deserialize(args).map_err(|error| {
                        ToolExecutionError::other(format!("invalid arguments: {error}"))
                    })?;
                    let reply = agent
                        .send(&args.message, args.context_id.as_deref())
                        .await
                        .map_err(|error| {
                            ToolExecutionError::other(format!("remote agent: {error}"))
                        })?;
                    tool_output(&reply)
                })
            });
        Ok(tool)
    }
}

impl RemoteAgentBuilder {
    /// Send `Authorization: Bearer <token>` with every call.
    #[must_use]
    pub fn bearer_token(mut self, token: impl Into<String>) -> Self {
        self.bearer = Some(token.into());
        self
    }

    /// How long the agent may stay silent; see [`DEFAULT_TIMEOUT`].
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Read the agent card and connect in the version it declares.
    pub async fn connect(self) -> Result<RemoteAgent, RemoteError> {
        // No overall request timeout: a stream may rightly last longer. The
        // agent's silence is bounded per event instead.
        let http = reqwest::Client::builder()
            .connect_timeout(self.timeout)
            .build()?;
        let card_url = format!(
            "{}{}",
            self.base_url.trim_end_matches('/'),
            a2a_server::WELL_KNOWN_AGENT_CARD_PATH
        );
        let body = tokio::time::timeout(self.timeout, async {
            let response = http.get(&card_url).send().await?.error_for_status()?;
            read_capped(response, MAX_CARD_BYTES).await
        })
        .await
        .map_err(|_| RemoteError::Timeout(self.timeout))??
        .ok_or_else(|| RemoteError::Card(format!("larger than {MAX_CARD_BYTES} bytes")))?;
        let card: Value = serde_json::from_slice(&body)
            .map_err(|e| RemoteError::Card(format!("not JSON: {e}")))?;

        if card.get("supportedInterfaces").is_some() {
            let card: AgentCard = serde_json::from_value(card)
                .map_err(|e| RemoteError::Card(format!("not a v1.0 card: {e}")))?;
            let mut factory = A2AClientFactory::builder();
            if let Some(token) = &self.bearer {
                factory = factory.with_interceptor(Arc::new(AuthInterceptor::bearer(token)));
            }
            #[cfg(feature = "otel")]
            {
                factory = factory.with_interceptor(Arc::new(crate::otel::TraceContext));
            }
            let client = factory.build().create_from_card(&card).await?;
            return Ok(RemoteAgent {
                card,
                link: Link::V1(Arc::new(client)),
                timeout: self.timeout,
            });
        }

        let Some(url) = card.get("url").and_then(Value::as_str).map(str::to_owned) else {
            return Err(RemoteError::Card(
                "neither `supportedInterfaces` (v1.0) nor `url` (v0.3)".into(),
            ));
        };
        Ok(RemoteAgent {
            card: v1_card_from_v03(card, &url)?,
            link: Link::V03 {
                http,
                url,
                bearer: self.bearer,
            },
            timeout: self.timeout,
        })
    }
}

/// Assemble `events` into reply events, giving up when one takes longer than
/// `timeout`. The call's `span` lasts as long as the stream.
fn follow(
    mut events: BoxStream<'static, Result<StreamResponse, RemoteError>>,
    timeout: Duration,
    span: tracing::Span,
) -> impl Stream<Item = Result<ReplyEvent, RemoteError>> + Send + 'static {
    async_stream::try_stream! {
        let _span = span;
        let mut assembly = Assembly::default();
        loop {
            let next = tokio::time::timeout(timeout, events.next())
                .await
                .map_err(|_| RemoteError::Timeout(timeout))?;
            let Some(event) = next else { break };
            for reply_event in assembly.apply(event?) {
                yield reply_event;
            }
        }
        // A stream that stops before the answer is settled is a failure, not
        // an empty answer: a refused call (401) or a dropped connection ends
        // the official client's stream the same way.
        if !assembly.settled() {
            Err(A2AError::new(
                a2a::error_code::INVALID_AGENT_RESPONSE,
                "the stream ended before the agent answered (refused, or the connection dropped)",
            ))?;
        }
        yield ReplyEvent::Done(assembly.finish());
    }
}

/// An answer put together from the events that describe it.
#[derive(Default)]
struct Assembly {
    context_id: Option<String>,
    state: Option<TaskState>,
    /// The status message of a task that is not working: why it failed, what
    /// input it needs.
    status_message: Option<Message>,
    /// The text of status messages sent while working, in order. Some agents
    /// (Strands, for one) stream their answer this way before sending it
    /// whole as an artifact.
    progress: String,
    artifacts: Vec<Artifact>,
    message: Option<Message>,
    /// The text reported so far, to report only what changed.
    shown: String,
}

impl Assembly {
    /// Take in one event; returns what it changed.
    fn apply(&mut self, event: StreamResponse) -> Vec<ReplyEvent> {
        let mut changes = Vec::new();
        let mut set_state = |state: TaskState, assembly: &mut Self| {
            if assembly.state.as_ref() != Some(&state) {
                assembly.state = Some(state.clone());
                changes.push(ReplyEvent::Status(state));
            }
        };
        match event {
            StreamResponse::Task(task) => {
                self.context_id = Some(task.context_id);
                self.artifacts = task.artifacts.unwrap_or_default();
                self.note(&task.status.state, task.status.message);
                set_state(task.status.state, self);
            }
            StreamResponse::Message(message) => {
                if message.context_id.is_some() {
                    self.context_id = message.context_id.clone();
                }
                self.message = Some(message);
            }
            StreamResponse::StatusUpdate(update) => {
                self.context_id = Some(update.context_id);
                self.note(&update.status.state, update.status.message);
                set_state(update.status.state, self);
            }
            StreamResponse::ArtifactUpdate(update) => {
                self.context_id = Some(update.context_id);
                self.merge(update.artifact, update.append.unwrap_or(false));
            }
        }
        let text = self.current().text;
        if text != self.shown {
            changes.push(match text.strip_prefix(self.shown.as_str()) {
                Some(more) => ReplyEvent::Text(more.to_owned()),
                None => ReplyEvent::Rewrite(text.clone()),
            });
            self.shown = text;
        }
        changes
    }

    /// Keep a status message: as progress while the task works, as the
    /// task's explanation otherwise.
    fn note(&mut self, state: &TaskState, message: Option<Message>) {
        let Some(message) = message else { return };
        if matches!(state, TaskState::Working | TaskState::Submitted) {
            let text: Vec<&str> = message.parts.iter().filter_map(Part::as_text).collect();
            self.progress.push_str(&text.concat());
        } else {
            self.status_message = Some(message);
        }
    }

    /// Append `artifact`'s parts to the artifact with its id, or replace it.
    /// Appended text continues the text part before it.
    fn merge(&mut self, artifact: Artifact, append: bool) {
        let Some(existing) = self
            .artifacts
            .iter_mut()
            .find(|known| known.artifact_id == artifact.artifact_id)
        else {
            self.artifacts.push(artifact);
            return;
        };
        if !append {
            *existing = artifact;
            return;
        }
        for part in artifact.parts {
            match (existing.parts.last_mut(), &part.content) {
                (Some(last), PartContent::Text(more)) => match &mut last.content {
                    PartContent::Text(text) => text.push_str(more),
                    _ => existing.parts.push(part),
                },
                _ => existing.parts.push(part),
            }
        }
    }

    /// The answer as the events so far describe it.
    fn current(&self) -> Reply {
        if let Some(message) = &self.message {
            return reply_from_parts(message.parts.iter(), self.context_id.clone(), None);
        }
        let (context_id, state) = (self.context_id.clone(), self.state.clone());
        let artifact_parts = self.artifacts.iter().flat_map(|artifact| &artifact.parts);
        // A finished task answers in its artifacts; a task that failed or
        // waits on its caller explains itself in its status message; a task
        // still working shows its artifacts so far, or else its progress.
        let finished = self.state == Some(TaskState::Completed);
        if (finished || self.status_message.is_none()) && artifact_parts.clone().next().is_some() {
            return reply_from_parts(artifact_parts, context_id, state);
        }
        if let Some(message) = &self.status_message {
            return reply_from_parts(message.parts.iter(), context_id, state);
        }
        Reply {
            text: self.progress.clone(),
            context_id,
            state,
            ..Reply::default()
        }
    }

    /// Whether the events so far hold an answer: a direct message, or a task
    /// that finished or is waiting on its caller.
    fn settled(&self) -> bool {
        self.message.is_some()
            || self.state.as_ref().is_some_and(|state| {
                state.is_terminal()
                    || matches!(state, TaskState::InputRequired | TaskState::AuthRequired)
            })
    }

    fn finish(self) -> Reply {
        self.current()
    }
}

fn reply_from_parts<'a>(
    parts: impl Iterator<Item = &'a Part>,
    context_id: Option<String>,
    state: Option<TaskState>,
) -> Reply {
    let mut reply = Reply {
        context_id,
        state,
        ..Reply::default()
    };
    let mut lines = Vec::new();
    for part in parts {
        let file = |content| RemoteFile {
            name: part.filename.clone(),
            media_type: part.media_type.clone(),
            content,
        };
        match &part.content {
            PartContent::Text(text) => lines.push(text.as_str()),
            PartContent::Data(data) => reply.data.push(data.clone()),
            PartContent::Raw(bytes) => reply.files.push(file(FileContent::Bytes(bytes.clone()))),
            PartContent::Url(url) => reply.files.push(file(FileContent::Url(url.clone()))),
        }
    }
    reply.text = lines.join("\n");
    reply
}

/// What the model sees of a reply: one JSON block, then the images.
fn tool_output(reply: &Reply) -> Result<ToolOutput, ToolExecutionError> {
    let mut images = Vec::new();
    let files: Vec<Value> = reply
        .files
        .iter()
        .map(|file| describe_file(file, &mut images))
        .collect();
    let mut summary = Map::new();
    summary.insert("reply".into(), json!(reply.text));
    summary.insert("context_id".into(), json!(reply.context_id));
    summary.insert(
        "state".into(),
        json!(reply.state.as_ref().map(v03::state_name)),
    );
    if !reply.data.is_empty() {
        summary.insert("data".into(), json!(reply.data));
    }
    if !files.is_empty() {
        summary.insert("files".into(), json!(files));
    }
    let mut content = vec![ToolResultContent::json(Value::Object(summary))];
    content.extend(images);
    ToolOutput::content(content)
}

/// A file as the model reads it: its name and type, its text when it is a
/// small text file, its URL when it has one. Images also go into `images`,
/// for the model to look at.
fn describe_file(file: &RemoteFile, images: &mut Vec<ToolResultContent>) -> Value {
    let mut described = Map::new();
    described.insert("name".into(), json!(file.name));
    described.insert("media_type".into(), json!(file.media_type));
    let image_kind = |bytes: Option<&[u8]>| {
        file.media_type
            .as_deref()
            .and_then(ImageMediaType::from_mime_type)
            .or_else(|| bytes.and_then(ImageMediaType::sniff))
    };
    let image = |data, kind| {
        ToolResultContent::Image(Image {
            data,
            media_type: Some(kind),
            ..Default::default()
        })
    };
    match &file.content {
        FileContent::Url(url) => {
            described.insert("url".into(), json!(url));
            if let Some(kind) = image_kind(None) {
                images.push(image(DocumentSourceKind::url(url), kind));
                described.insert("shown".into(), json!("as an image below"));
            }
        }
        FileContent::Bytes(bytes) => {
            described.insert("size_bytes".into(), json!(bytes.len()));
            let texty = file
                .media_type
                .as_deref()
                .is_none_or(|media| media.starts_with("text/") || media == "application/json");
            match (image_kind(Some(bytes)), std::str::from_utf8(bytes)) {
                (Some(kind), _) if bytes.len() <= MAX_IMAGE_BYTES => {
                    images.push(image(
                        DocumentSourceKind::base64(BASE64.encode(bytes)),
                        kind,
                    ));
                    described.insert("shown".into(), json!("as an image below"));
                }
                (None, Ok(text)) if texty && bytes.len() <= MAX_INLINE_TEXT => {
                    described.insert("text".into(), json!(text));
                }
                _ => {
                    described.insert("shown".into(), json!("not shown: binary or too large"));
                }
            }
        }
    }
    Value::Object(described)
}

/// The body of a response, read chunk by chunk; `None` once it passes
/// `limit` bytes, so a long body is never held whole.
pub(crate) async fn read_capped(
    response: reqwest::Response,
    limit: usize,
) -> Result<Option<Vec<u8>>, reqwest::Error> {
    let mut body = Vec::new();
    let mut chunks = response.bytes_stream();
    while let Some(chunk) = chunks.next().await {
        body.extend_from_slice(&chunk?);
        if body.len() > limit {
            return Ok(None);
        }
    }
    Ok(Some(body))
}

/// A v0.3 card read as a v1.0 one, for [`RemoteAgent::card`]: one JSON-RPC
/// interface at its `url`, and its security fields (shaped differently in
/// v1.0) left out.
fn v1_card_from_v03(mut card: Value, url: &str) -> Result<AgentCard, RemoteError> {
    if let Some(fields) = card.as_object_mut() {
        fields.remove("securitySchemes");
        fields.remove("security");
        fields.insert(
            "supportedInterfaces".into(),
            json!([{"url": url, "protocolBinding": "JSONRPC", "protocolVersion": "0.3"}]),
        );
    }
    serde_json::from_value(card).map_err(|e| RemoteError::Card(format!("not a v0.3 card: {e}")))
}

fn user_message(text: &str, context_id: Option<&str>) -> Message {
    let mut message = Message::new(Role::User, vec![Part::text(text)]);
    message.context_id = context_id.map(str::to_owned);
    message
}

fn request(message: Message) -> SendMessageRequest {
    SendMessageRequest {
        message,
        configuration: None,
        metadata: None,
        tenant: None,
    }
}

#[derive(Deserialize)]
struct Args {
    message: String,
    context_id: Option<String>,
}

/// A provider-safe tool name from an agent's name: ASCII letters, digits and
/// `_`, at most 64 characters, never empty.
fn tool_name(agent_name: &str) -> String {
    let mut name: String = agent_name
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
    name.truncate(MAX_TOOL_NAME);
    let name = name.trim_matches('_');
    if name.is_empty() {
        "remote_agent".to_owned()
    } else {
        name.to_owned()
    }
}

fn tool_description(card: &AgentCard) -> String {
    let skills: Vec<String> = card
        .skills
        .iter()
        .map(|skill| format!("- {}: {}", skill.name, skill.description))
        .collect();
    format!(
        "Delegate to the remote agent \"{}\" (A2A). {}\nSkills:\n{}\n\
         Pass the context_id of an earlier reply to continue the same conversation.",
        card.name,
        card.description,
        skills.join("\n")
    )
}

#[cfg(test)]
mod tests;
