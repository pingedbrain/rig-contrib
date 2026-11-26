//! A Rig agent as an A2A agent executor.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use a2a::{
    A2AError, Artifact, Message, Part, Role, StreamResponse, Task, TaskArtifactUpdateEvent, TaskId,
    TaskState, TaskStatus, TaskStatusUpdateEvent, new_message_id,
};
use a2a_server::{AgentExecutor, ExecutorContext};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures::StreamExt;
use futures::future::BoxFuture;
use futures::stream::{AbortHandle, Abortable, BoxStream};
use rig_agent::Agent;
use rig_agent::agent::MultiTurnStreamItem;
use rig_agent::completion::{PromptError, StructuredOutputError};
use rig_core::message::{AssistantContent, DocumentSourceKind, MimeType};
use rig_core::streaming::{Item, StreamEvent};
use rig_memory::AsyncStore;
use rig_memory::recent::{Recent, TokenBudget, view};

use crate::history::{
    DEFAULT_HISTORY_TOKENS, DEFAULT_MAX_CONTEXTS, HistoryStore, InMemory, Persistent,
    valid_context_id,
};
use crate::parts::user_message;

/// The id of the text artifact each task's reply streams into.
const RESPONSE_ARTIFACT: &str = "response";

/// How many tasks run at once by default; more are rejected as busy.
pub const DEFAULT_MAX_CONCURRENT_TASKS: usize = 32;
/// How long one run may take by default before it is stopped.
pub const DEFAULT_MAX_RUN_DURATION: Duration = Duration::from_secs(600);

type Running = Arc<Mutex<HashMap<TaskId, AbortHandle>>>;

/// A typed run, erased: the agent, the prompt and the history in; the answer
/// as JSON and the run's transcript out, or what to tell the caller.
type TypedAnswer = Arc<
    dyn Fn(
            Agent,
            rig_core::message::Message,
            Vec<rig_core::message::Message>,
        ) -> BoxFuture<
            'static,
            Result<(serde_json::Value, Vec<rig_core::message::Message>), String>,
        > + Send
        + Sync,
>;

/// A Rig [`Agent`] as an A2A [`AgentExecutor`].
///
/// Each execution prompts the agent with the incoming message and the history
/// of its `contextId`, streams the reply's text into one artifact, and ends the
/// task completed, failed, or rejected when a part has no Rig content kind or
/// the `contextId` is not 1 to 128 ASCII letters, digits, `-` or `_`. A
/// completed run's messages join the context's history; see
/// [`history_store`](Self::history_store) and
/// [`history_window`](Self::history_window) for where it lives and how much
/// of it the model sees. Cancelling a task drops its run.
///
/// Every task spends model tokens, so an executor bounds them: at most
/// [`max_concurrent_tasks`](Self::max_concurrent_tasks) run at once (more are
/// rejected as busy, before any model call) and each stops after
/// [`max_run_duration`](Self::max_run_duration).
#[derive(Clone)]
pub struct A2aExecutor {
    agent: Agent,
    history: Arc<dyn HistoryStore>,
    window: Arc<dyn Recent>,
    running: Running,
    slots: Arc<tokio::sync::Semaphore>,
    max_run: Duration,
    typed: Option<TypedAnswer>,
}

impl A2aExecutor {
    /// An executor that answers every task with `agent`, keeping
    /// conversations in memory.
    pub fn new(agent: Agent) -> Self {
        Self {
            agent,
            history: Arc::new(InMemory::new(DEFAULT_MAX_CONTEXTS)),
            window: Arc::new(TokenBudget::new(DEFAULT_HISTORY_TOKENS)),
            running: Arc::default(),
            slots: Arc::new(tokio::sync::Semaphore::new(DEFAULT_MAX_CONCURRENT_TASKS)),
            max_run: DEFAULT_MAX_RUN_DURATION,
            typed: None,
        }
    }

    /// Answer every task with a `T`, as one JSON data part: the agent runs
    /// as [`Agent::prompt_typed`] does (with the conversation's history, and
    /// two retries when the model's JSON does not parse as `T`). Use it to
    /// serve an extractor or any agent whose callers want data, not prose.
    #[must_use]
    pub fn typed<T>(mut self) -> Self
    where
        T: schemars::JsonSchema
            + serde::de::DeserializeOwned
            + serde::Serialize
            + Send
            + Sync
            + 'static,
    {
        self.typed = Some(Arc::new(|agent, prompt, history| {
            Box::pin(async move {
                let response = agent
                    .prompt_typed::<T>(prompt)
                    .history(history)
                    .retries(2)
                    .await
                    .map_err(|error| {
                        tracing::warn!(%error, "A2A typed task failed");
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
        }));
        self
    }

    /// Run at most `max` tasks at once (at least one); a task beyond that is
    /// rejected as busy before it reaches the model. Defaults to
    /// [`DEFAULT_MAX_CONCURRENT_TASKS`].
    #[must_use]
    pub fn max_concurrent_tasks(mut self, max: usize) -> Self {
        self.slots = Arc::new(tokio::sync::Semaphore::new(max.max(1)));
        self
    }

    /// Stop a run that takes longer than `max`, failing its task. Defaults to
    /// [`DEFAULT_MAX_RUN_DURATION`].
    #[must_use]
    pub fn max_run_duration(mut self, max: Duration) -> Self {
        self.max_run = max;
        self
    }

    /// Keep conversations in memory, at most `max` of them (at least one);
    /// the oldest one started is forgotten first. This is the default, with
    /// [`DEFAULT_MAX_CONTEXTS`]; it replaces a [`history_store`](Self::history_store).
    #[must_use]
    pub fn max_contexts(mut self, max: usize) -> Self {
        self.history = Arc::new(InMemory::new(max));
        self
    }

    /// Keep conversations in `store`, one session log per `contextId`, so
    /// they survive restarts and several servers can share them: any
    /// [`rig_memory`] store, such as `rig_memory::DirStore` (feature `fs`)
    /// or the `rig-sqlite` and `rig-postgres` stores. Two servers saving one
    /// conversation at once do not lose either turn.
    #[must_use]
    pub fn history_store<S>(mut self, store: S) -> Self
    where
        S: AsyncStore + Send + Sync + 'static,
    {
        self.history = Arc::new(Persistent(store));
        self
    }

    /// Which newest messages of a conversation each request sends the
    /// model, such as `rig_memory::recent::TokenBudget::new(8_000)` or
    /// `LastMessages(20)`. Defaults to [`DEFAULT_HISTORY_TOKENS`] tokens. A
    /// tool result is never sent without its call.
    #[must_use]
    pub fn history_window(mut self, window: impl Recent + 'static) -> Self {
        self.window = Arc::new(window);
        self
    }
}

impl AgentExecutor for A2aExecutor {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let (handle, registration) = AbortHandle::new_pair();
        lock(&self.running).insert(ctx.task_id.clone(), handle);
        let registered = Registered {
            running: Arc::clone(&self.running),
            task_id: ctx.task_id.clone(),
        };
        let run = run(self.clone(), ctx, registered);
        Abortable::new(run, registration).boxed()
    }

    fn cancel(&self, ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        if let Some(handle) = lock(&self.running).remove(&ctx.task_id) {
            handle.abort();
        }
        let ids = Ids {
            task: ctx.task_id,
            context: ctx.context_id,
        };
        futures::stream::iter([Ok(ids.status(TaskState::Canceled, None))]).boxed()
    }
}

/// Unregisters a task's abort handle when its run is dropped.
struct Registered {
    running: Running,
    task_id: TaskId,
}

impl Drop for Registered {
    fn drop(&mut self) {
        lock(&self.running).remove(&self.task_id);
    }
}

fn run(
    executor: A2aExecutor,
    ctx: ExecutorContext,
    registered: Registered,
) -> impl futures::Stream<Item = Result<StreamResponse, A2AError>> + Send + 'static {
    async_stream::stream! {
        let _registered = registered;
        let ids = Ids {
            task: ctx.task_id,
            context: ctx.context_id,
        };
        // The task's span: the agent's own spans (model calls, tools) nest
        // under it, and it records how the task ended.
        let span = tracing::info_span!(
            "a2a.task",
            a2a.task_id = %ids.task,
            a2a.context_id = %ids.context,
            a2a.state = tracing::field::Empty,
        );
        let end = |state: &'static str| {
            span.record("a2a.state", state);
        };
        let Some(message) = ctx.message else {
            end("failed");
            yield Ok(ids.status(TaskState::Failed, Some(ids.agent_message("No message to answer."))));
            return;
        };
        let mut history = ctx
            .stored_task
            .and_then(|task| task.history)
            .unwrap_or_default();
        if history.last().map(|last| &last.message_id) != Some(&message.message_id) {
            history.push(message.clone());
        }
        if !valid_context_id(&ids.context) {
            let reason = ids.agent_message(
                "contextId must be 1 to 128 ASCII letters, digits, `-` or `_`.",
            );
            end("rejected");
            yield Ok(StreamResponse::Task(ids.task(TaskState::Rejected, Some(reason), history)));
            return;
        }
        let prompt = match user_message(&message) {
            Ok(prompt) => prompt,
            Err(unsupported) => {
                let reason = ids.agent_message(unsupported.to_string());
                end("rejected");
                yield Ok(StreamResponse::Task(ids.task(TaskState::Rejected, Some(reason), history)));
                return;
            }
        };
        // Held until the run ends, however it ends (an abort drops it too).
        let Ok(_slot) = Arc::clone(&executor.slots).try_acquire_owned() else {
            let reason = ids.agent_message("The agent is busy; try again shortly.");
            end("rejected_busy");
            yield Ok(StreamResponse::Task(ids.task(TaskState::Rejected, Some(reason), history)));
            return;
        };
        yield Ok(StreamResponse::Task(ids.task(TaskState::Working, None, history)));

        let prior = match executor.history.load(&ids.context).await {
            Ok(messages) => view(&messages, executor.window.as_ref())
                .iter()
                .map(|message| (**message).clone())
                .collect::<Vec<_>>(),
            Err(error) => {
                tracing::warn!(parent: &span, %error, "could not load the conversation");
                end("failed");
                yield Ok(ids.status(TaskState::Failed, Some(ids.agent_message("The conversation could not be loaded."))));
                return;
            }
        };
        let deadline = tokio::time::Instant::now() + executor.max_run;
        if let Some(typed) = &executor.typed {
            let answer = tracing::Instrument::instrument(typed(executor.agent.clone(), prompt, prior), span.clone());
            match tokio::time::timeout_at(deadline, answer).await {
                Err(_) => {
                    end("failed_timeout");
                    yield Ok(ids.status(TaskState::Failed, Some(ids.agent_message("The agent run took longer than allowed."))));
                }
                Ok(Err(reason)) => {
                    end("failed");
                    yield Ok(ids.status(TaskState::Failed, Some(ids.agent_message(reason))));
                }
                Ok(Ok((value, messages))) => {
                    if let Err(error) = executor.history.append(&ids.context, messages).await {
                        tracing::warn!(parent: &span, %error, "could not save the conversation");
                    }
                    let mut data = Part::text("");
                    data.content = a2a::PartContent::Data(value);
                    data.media_type = Some("application/json".to_owned());
                    end("completed");
                    yield Ok(ids.artifact(vec![data], false, true));
                    yield Ok(ids.status(TaskState::Completed, None));
                }
            }
            return;
        }
        let mut stream = span.in_scope(|| executor.agent.prompt(prompt).history(prior).stream());
        let mut streamed = false;
        loop {
            let item = match tokio::time::timeout_at(deadline, stream.next()).await {
                Ok(Some(item)) => item,
                Ok(None) => break,
                Err(_) => {
                    tracing::warn!(parent: &span, max = ?executor.max_run, "A2A run took too long");
                    end("failed_timeout");
                    yield Ok(ids.status(TaskState::Failed, Some(ids.agent_message("The agent run took longer than allowed."))));
                    return;
                }
            };
            match item {
                Ok(MultiTurnStreamItem::StreamAssistantItem(Item::Event(StreamEvent::Text {
                    text,
                    ..
                }))) => {
                    yield Ok(ids.chunk(text, streamed, false));
                    streamed = true;
                }
                Ok(MultiTurnStreamItem::FinalResponse(response)) => {
                    // The answer stands even if the history cannot be saved;
                    // the next turn then starts without this one.
                    if let Err(error) = executor.history.append(&ids.context, response.messages.clone()).await {
                        tracing::warn!(parent: &span, %error, "could not save the conversation");
                    }
                    // The final answer replaces the streamed text, which can
                    // include retried turns and text written before tool calls.
                    end("completed");
                    yield Ok(ids.artifact(answer_parts(&response.output(), &response.content), false, true));
                    yield Ok(ids.status(TaskState::Completed, None));
                    return;
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(parent: &span, %error, "A2A task failed");
                    end("failed");
                    yield Ok(ids.status(TaskState::Failed, Some(ids.agent_message(failure_message(&error)))));
                    return;
                }
            }
        }
        end("failed");
        yield Ok(ids.status(TaskState::Failed, Some(ids.agent_message("The agent run ended without an answer."))));
    }
}

/// The task and context one execution reports on.
struct Ids {
    task: TaskId,
    context: String,
}

impl Ids {
    fn agent_message(&self, text: impl Into<String>) -> Message {
        Message {
            message_id: new_message_id(),
            context_id: Some(self.context.clone()),
            task_id: Some(self.task.clone()),
            role: Role::Agent,
            parts: vec![Part::text(text)],
            metadata: None,
            extensions: None,
            reference_task_ids: None,
        }
    }

    fn task(&self, state: TaskState, message: Option<Message>, history: Vec<Message>) -> Task {
        Task {
            id: self.task.clone(),
            context_id: self.context.clone(),
            status: status(state, message),
            artifacts: None,
            history: Some(history),
            metadata: None,
        }
    }

    fn status(&self, state: TaskState, message: Option<Message>) -> StreamResponse {
        StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: self.task.clone(),
            context_id: self.context.clone(),
            status: status(state, message),
            metadata: None,
        })
    }

    /// A piece of text of the response artifact; see [`Self::artifact`].
    fn chunk(&self, text: String, append: bool, last_chunk: bool) -> StreamResponse {
        self.artifact(vec![Part::text(text)], append, last_chunk)
    }

    /// Parts of the response artifact: appended to the parts before them, or
    /// replacing them when `append` is false.
    fn artifact(&self, parts: Vec<Part>, append: bool, last_chunk: bool) -> StreamResponse {
        StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
            task_id: self.task.clone(),
            context_id: self.context.clone(),
            artifact: Artifact {
                artifact_id: RESPONSE_ARTIFACT.to_owned(),
                name: Some(RESPONSE_ARTIFACT.to_owned()),
                description: None,
                parts,
                metadata: None,
                extensions: None,
            },
            append: Some(append),
            last_chunk: Some(last_chunk),
            metadata: None,
        })
    }
}

/// The final answer's parts: its text, then any image the model produced.
fn answer_parts(text: &str, content: &[AssistantContent]) -> Vec<Part> {
    let mut parts = vec![Part::text(text)];
    for item in content {
        let AssistantContent::Image(image) = item else {
            continue;
        };
        let content = match &image.data {
            DocumentSourceKind::Url(url) => a2a::PartContent::Url(url.clone()),
            DocumentSourceKind::Raw(bytes) => a2a::PartContent::Raw(bytes.clone()),
            DocumentSourceKind::Base64(data) => match BASE64.decode(data) {
                Ok(bytes) => a2a::PartContent::Raw(bytes),
                Err(_) => continue,
            },
            // A provider-side file id or nothing: no bytes to send.
            _ => continue,
        };
        let mut part = Part::text("");
        part.content = content;
        part.media_type = image
            .media_type
            .as_ref()
            .map(|kind| kind.to_mime_type().to_owned());
        parts.push(part);
    }
    parts
}

fn status(state: TaskState, message: Option<Message>) -> TaskStatus {
    TaskStatus {
        state,
        message,
        timestamp: Some(chrono::Utc::now()),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the caller is told when a run fails: enough to decide whether to
/// retry, without the provider's own error text (which can name internals).
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
        Some(429) => "The model provider is rate limiting this agent; try again in a moment.",
        Some(401 | 403) => "The model provider rejected this agent's credentials.",
        Some(500..=599) => "The model provider is unavailable right now; try again shortly.",
        Some(_) => "The model provider rejected the request.",
        None if retryable => "The model provider could not be reached; try again shortly.",
        None => "The agent run failed.",
    }
}

#[cfg(test)]
mod tests {
    use rig_core::message::{AssistantContent, DocumentSourceKind, Image, ImageMediaType};

    use super::*;

    #[test]
    fn failures_are_classified_by_status_and_retryability() {
        use rig_core::error::{ErrorKind, ErrorReport};
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
    fn model_images_become_file_parts_after_the_text() {
        let image = |data| {
            AssistantContent::Image(Image {
                data,
                media_type: Some(ImageMediaType::PNG),
                ..Default::default()
            })
        };
        let content = [
            AssistantContent::text("here"),
            image(DocumentSourceKind::base64("aGk=")),
            image(DocumentSourceKind::url("https://x/a.png")),
            image(DocumentSourceKind::file_id("file-1")),
        ];
        let parts = answer_parts("here", &content);
        assert_eq!(
            parts.len(),
            3,
            "text, the decoded image, the URL image; no file id"
        );
        assert_eq!(parts[0].as_text(), Some("here"));
        assert_eq!(parts[1].content, a2a::PartContent::Raw(b"hi".to_vec()));
        assert_eq!(parts[1].media_type.as_deref(), Some("image/png"));
        assert_eq!(
            parts[2].content,
            a2a::PartContent::Url("https://x/a.png".into())
        );
    }
}
