//! Run a [Rig](https://github.com/0xPlaygrounds/rig) agent on AWS Bedrock
//! AgentCore Runtime.
//!
//! AgentCore runs an agent as an ARM64 container that serves its HTTP
//! protocol contract on port 8080: `POST /invocations` with a JSON body,
//! answered with JSON or server-sent events, and `GET /ping`. Each runtime
//! session (the `X-Amzn-Bedrock-AgentCore-Runtime-Session-Id` header) is one
//! conversation. [`AgentCoreApp`] serves that contract for a Rig agent:
//!
//! - the session's conversation is kept between invocations, in memory or in
//!   any [`rig_memory`] store (see [`AgentCoreApp::history_store`]);
//! - the reply streams as server-sent events when the caller asks for
//!   `text/event-stream`, and is one JSON object otherwise;
//! - each invocation runs in a span that continues the caller's trace and
//!   carries the session as `session.id` (through [`rig_otel`]), which is what
//!   AgentCore Observability and Evaluations group a session's traces by;
//! - `/ping` answers `HealthyBusy` while an invocation or background work from
//!   [`AgentCoreApp::spawn_task`] is open, so AgentCore Runtime keeps the
//!   session running until that work ends.
//!
//! - prompts and replies go on the spans with [`AgentCoreApp::record_content`].
//!   Unset, the agent's own
//!   [`record_content_telemetry`](rig_agent::AgentBuilder::record_content_telemetry) setting applies to
//!   the agent's spans; the invocation span carries them only when
//!   `record_content(true)` is set. AgentCore Evaluations needs them to judge
//!   a session.
//!
//! ```no_run
//! async fn run(agent: rig_agent::Agent) -> Result<(), rig_agentcore::ServeError> {
//!     rig_agentcore::AgentCoreApp::new(agent)
//!         .record_content(true)
//!         .serve()
//!         .await
//! }
//! ```

#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::panic,
        clippy::unwrap_used,
        clippy::unreachable
    )
)]

mod busy;
mod history;

use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures::StreamExt;
use rig_agent::Agent;
use rig_agent::agent::MultiTurnStreamItem;
use rig_core::message::Message;
use rig_core::streaming::{Item, StreamEvent};
use rig_core::wasm_compat::{WasmCompatSend, WasmCompatSync};
use rig_memory::AsyncStore;
use rig_memory::recent::{Recent, TokenBudget, view};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::Instrument;

use crate::busy::Busy;
use crate::history::{
    HistoryError, HistoryStore, InMemory, Persistent, Retained, Summarizing, is_summary, local_id,
    summarize,
};

/// The header AgentCore Runtime names the session with.
pub const SESSION_HEADER: &str = "x-amzn-bedrock-agentcore-runtime-session-id";

/// The port AgentCore Runtime sends the HTTP protocol to.
pub const DEFAULT_PORT: u16 = 8080;

/// How many sessions an app keeps by default. AgentCore gives each session
/// its own microVM, so one process usually sees one; this bounds the memory
/// when the app runs elsewhere.
pub const DEFAULT_MAX_SESSIONS: usize = 1000;

/// How many tokens of history a session keeps by default.
pub const DEFAULT_HISTORY_TOKENS: usize = 32_000;

/// The largest `/invocations` body by default, in bytes: the limit Axum's JSON
/// extractor already applied. A larger body gets 413.
pub const DEFAULT_MAX_REQUEST_BYTES: usize = 2 * 1024 * 1024;

pub use busy::BusyGuard;
/// The session stores and history windows conversations are kept with.
pub use rig_memory;

/// Why the app stopped serving.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ServeError {
    /// The port could not be bound.
    #[error("could not listen on {address}: {source}")]
    Bind {
        /// The address.
        address: SocketAddr,
        /// The error.
        #[source]
        source: std::io::Error,
    },
    /// Serving failed.
    #[error("serving failed: {0}")]
    Serve(#[source] std::io::Error),
}

/// A Rig agent served on AgentCore Runtime's HTTP protocol contract.
#[derive(Clone)]
pub struct AgentCoreApp {
    inner: Arc<Inner>,
}

#[derive(Clone)]
struct Inner {
    agent: Agent,
    history: Arc<dyn HistoryStore>,
    window: Arc<dyn Recent>,
    summarizing: Option<Summarizing>,
    record_content: Option<bool>,
    slots: Option<Arc<Semaphore>>,
    max_run: Option<Duration>,
    max_request_bytes: usize,
    busy: Arc<Busy>,
}

impl Inner {
    /// Whether the invocation span carries prompts and answers. With no
    /// [`AgentCoreApp::record_content`] there is no agent setting to follow
    /// here, so it does not.
    fn records_invocation_content(&self) -> bool {
        self.record_content == Some(true)
    }
}

impl AgentCoreApp {
    /// Serve `agent`.
    pub fn new(agent: Agent) -> Self {
        Self {
            inner: Arc::new(Inner {
                agent,
                history: Arc::new(InMemory::new(DEFAULT_MAX_SESSIONS)),
                window: Arc::new(TokenBudget::new(DEFAULT_HISTORY_TOKENS)),
                summarizing: None,
                record_content: None,
                slots: None,
                max_run: None,
                max_request_bytes: DEFAULT_MAX_REQUEST_BYTES,
                busy: Busy::new(),
            }),
        }
    }

    /// Keep at most `max` sessions' conversations in memory, forgetting the
    /// oldest. This is the default, with [`DEFAULT_MAX_SESSIONS`]. It replaces
    /// a store set with [`history_store`](Self::history_store).
    #[must_use]
    pub fn max_sessions(self, max: usize) -> Self {
        self.rebuild(|inner| inner.history = Arc::new(InMemory::new(max)))
    }

    /// Keep sessions in `store`, one session log per session id, so they
    /// survive restarts and several apps can share them: any [`rig_memory`]
    /// store, such as `rig_memory::DirStore` (feature `fs`) or the
    /// `rig-sqlite` and `rig-postgres` stores. Any session id works: one that
    /// is 1 to 128 ASCII letters, digits, `-` or `_` names its log as it is,
    /// any other is hashed into a safe log name. Two apps, or concurrent
    /// invocations of one session, saving at once do not lose either turn: a
    /// save that loses a race rereads and retries up to 10 times with a short
    /// jittered backoff. If a save still fails, the invocation's answer is
    /// returned anyway, the failure is logged, and that run's turn is not
    /// persisted, so later invocations do not see it. A load that fails is
    /// reported to the caller as an error, before the agent runs. The store
    /// keeps each session's whole conversation;
    /// [`history_window`](Self::history_window) limits only what an invocation
    /// sends. Replaces the in-memory store set by [`max_sessions`](Self::max_sessions).
    #[must_use]
    pub fn history_store<S>(self, store: S) -> Self
    where
        S: AsyncStore + WasmCompatSend + WasmCompatSync + 'static,
    {
        self.rebuild(|inner| inner.history = Arc::new(Persistent(store)))
    }

    /// Which newest messages of a session each invocation sends the model,
    /// such as `rig_memory::recent::TokenBudget::new(8_000)` or
    /// `LastMessages(20)`. Defaults to [`DEFAULT_HISTORY_TOKENS`] tokens. A
    /// tool result is never sent without its call. An empty window
    /// (`TokenBudget::new(0)`, `LastMessages(0)`) sends no history.
    ///
    /// With [`summarize_history`](Self::summarize_history), the summary is
    /// sent first, and the window limits only the messages after it.
    ///
    /// What the window also does depends on the store. In memory, messages
    /// beyond it are forgotten when a run is saved, so memory stays bounded.
    /// With [`history_store`](Self::history_store), the store keeps the whole
    /// conversation and the window only limits what is sent, so a later,
    /// wider window sees older turns again.
    #[must_use]
    pub fn history_window(self, window: impl Recent + 'static) -> Self {
        self.rebuild(|inner| inner.window = Arc::new(window))
    }

    /// Summarize a session's older history with `summarizer` once the session
    /// holds more than `trigger_messages` messages. The messages before the
    /// newest `keep_recent` are replaced by one summary, which becomes the
    /// session's first message: a user message in rig-memory's compaction frame
    /// ("The conversation history before this point was compacted into the
    /// following summary: <summary>...</summary>"). The newest messages stay
    /// verbatim, and the cut never separates a tool call from its result.
    ///
    /// The summary is always sent first, before the messages the window keeps,
    /// even when the window is narrower than the summary and the kept messages.
    /// In a session this option summarizes, a first message that opens with
    /// rig-memory's summary frame text is treated as the summary, even if a
    /// caller wrote it.
    /// `summarizer` should be an agent without tools: a tool call in its reply
    /// is not used. `trigger_messages` is raised to `keep_recent + 1` when it is
    /// not larger, since a cut needs something older than the kept messages.
    ///
    /// Summarizing runs before the invocation answers. Two concurrent
    /// invocations of one session may both summarize. A summary whose session
    /// changed under it (a concurrent summary, or a refused save to the store)
    /// is skipped and the history is kept, not lost.
    ///
    /// If a summary fails (the summarizer errors or returns no text, or the
    /// store refuses the change), a warning is logged, the session keeps its
    /// history, and the invocation answers as usual. The next invocation tries
    /// again. Without a store, a session whose summaries keep failing keeps at
    /// least its newest `2 * trigger_messages` messages, so memory stays
    /// bounded; older messages are dropped. With
    /// [`history_store`](Self::history_store), the store keeps the whole
    /// conversation as it does without summaries.
    ///
    /// Without a store, the summary is kept in memory with the session. With
    /// [`history_store`](Self::history_store), the summary is written to the
    /// store's log in place of the messages it replaces, which the log keeps in
    /// its history.
    #[must_use]
    pub fn summarize_history(
        self,
        summarizer: Agent,
        keep_recent: usize,
        trigger_messages: usize,
    ) -> Self {
        let trigger_messages = trigger_messages.max(keep_recent.saturating_add(1));
        self.rebuild(|inner| {
            inner.summarizing = Some(Summarizing {
                summarizer,
                keep_recent,
                trigger_messages,
            });
        })
    }

    /// Record prompts and replies on the GenAI spans. AgentCore Evaluations
    /// needs them to judge a session; they put conversation content into your
    /// telemetry.
    ///
    /// Set, this overrides the agent's own
    /// [`record_content_telemetry`](rig_agent::AgentBuilder::record_content_telemetry)
    /// for every invocation, in both directions. Unset (the default), the
    /// agent's setting applies to its spans, which is off unless the agent turns
    /// it on. The invocation span (`gen_ai.task.input`, `gen_ai.task.output` and
    /// the `agentcore.invocation.*` fields) has no agent setting to follow, so it
    /// carries content only when this is set to `true`.
    #[must_use]
    pub fn record_content(self, record: bool) -> Self {
        self.rebuild(|inner| inner.record_content = Some(record))
    }

    /// Run at most `max` invocations at once (at least one). An invocation
    /// beyond that gets 429 Too Many Requests before the agent runs; 429 is the
    /// conventional throttle status. Without this there is no cap, which is the
    /// default: AgentCore Runtime gives each session its own microVM. Outside
    /// AgentCore Runtime, a cap around 32 suits most single-process apps.
    #[must_use]
    pub fn max_concurrent_tasks(self, max: usize) -> Self {
        self.rebuild(|inner| inner.slots = Some(Arc::new(Semaphore::new(max.max(1)))))
    }

    /// Stop an invocation that runs longer than `max`, counting the summarizing
    /// step that can run before it. A stopped invocation answers 504 Gateway
    /// Timeout (or an `{"error"}` event when streaming), and its turn is not
    /// added to the session. Without this there is no limit, which is the
    /// default. Outside AgentCore Runtime, ten minutes suits most agents.
    #[must_use]
    pub fn max_run_duration(self, max: Duration) -> Self {
        self.rebuild(|inner| inner.max_run = Some(max))
    }

    /// Refuse an `/invocations` body larger than `bytes` with 413 Payload Too
    /// Large, before it is parsed. Defaults to [`DEFAULT_MAX_REQUEST_BYTES`].
    #[must_use]
    pub fn max_request_bytes(self, bytes: usize) -> Self {
        self.rebuild(|inner| inner.max_request_bytes = bytes)
    }

    /// Mark background work as open until the returned guard drops. While any
    /// guard is alive, `/ping` answers `HealthyBusy`, so AgentCore Runtime keeps
    /// the session running; once they are all dropped it answers `Healthy`
    /// again. Use it for work that outlives an invocation, such as a task the
    /// app starts on its own.
    pub fn track_task(&self) -> BusyGuard {
        Busy::open(&self.inner.busy)
    }

    /// Run `future` on the Tokio runtime as background work. `/ping` answers
    /// `HealthyBusy` until it finishes, or until its task is aborted. Dropping
    /// the returned handle does not stop the task. Call it on a running Tokio
    /// runtime, as for [`tokio::spawn`].
    ///
    /// ```no_run
    /// # async fn run(app: &rig_agentcore::AgentCoreApp) {
    /// app.spawn_task(async {
    ///     // Work that outlives the invocation that started it.
    /// });
    /// # }
    /// ```
    pub fn spawn_task<F>(&self, future: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let guard = self.track_task();
        tokio::spawn(async move {
            let _guard = guard;
            future.await
        })
    }

    fn rebuild(self, change: impl FnOnce(&mut Inner)) -> Self {
        let mut inner = Arc::try_unwrap(self.inner).unwrap_or_else(|shared| (*shared).clone());
        change(&mut inner);
        Self {
            inner: Arc::new(inner),
        }
    }

    /// The contract's routes, `/invocations` and `/ping`, as an Axum router.
    pub fn router(self) -> axum::Router {
        let limit = self.inner.max_request_bytes;
        axum::Router::new()
            .route(
                "/invocations",
                post(invoke).layer(DefaultBodyLimit::max(limit)),
            )
            .route("/ping", get(ping))
            .with_state(self)
    }

    /// Serve on `0.0.0.0` at `PORT` (default 8080, what AgentCore sends to)
    /// until the process gets Ctrl-C or SIGTERM. A `PORT` that is set but is
    /// not a port number logs a warning and serves on the default.
    pub async fn serve(self) -> Result<(), ServeError> {
        let listener = listen(port_from_env(std::env::var("PORT"))).await?;
        axum::serve(listener, self.router())
            .with_graceful_shutdown(shutdown())
            .await
            .map_err(ServeError::Serve)
    }
}

/// Resolves when the process is asked to stop.
async fn shutdown() {
    let interrupt = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = interrupt => {}
        () = terminate => {}
    }
}

/// The `/invocations` body: AgentCore passes the caller's payload as is.
#[derive(Deserialize)]
struct Invocation {
    prompt: String,
}

/// `Healthy`, or `HealthyBusy` while work is open, with the Unix time (in
/// seconds) the status last changed. The time moves only on a change, as
/// AgentCore's contract asks: a time that advanced on every ping would keep the
/// idle timeout from firing.
async fn ping(State(app): State<AgentCoreApp>) -> Json<serde_json::Value> {
    let (busy, since) = app.inner.busy.status();
    let status = if busy { "HealthyBusy" } else { "Healthy" };
    Json(json!({"status": status, "time_of_last_update": since}))
}

async fn invoke(
    State(app): State<AgentCoreApp>,
    headers: HeaderMap,
    body: Result<Json<Invocation>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let invocation = match body {
        Ok(Json(invocation)) => invocation,
        Err(rejection) => {
            if rejection.into_response().status() == StatusCode::PAYLOAD_TOO_LARGE {
                return error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    &format!(
                        "the request body is larger than {} bytes",
                        app.inner.max_request_bytes
                    ),
                );
            }
            return error(
                StatusCode::BAD_REQUEST,
                "the body must be a JSON object with a \"prompt\" string",
            );
        }
    };
    // Held until the run ends, however it ends. Checked before the session is
    // loaded, so a refused invocation never touches the agent or the history.
    let slot = match &app.inner.slots {
        None => None,
        Some(slots) => match Arc::clone(slots).try_acquire_owned() {
            Ok(slot) => Some(slot),
            Err(_) => {
                return error(
                    StatusCode::TOO_MANY_REQUESTS,
                    "the agent is busy; try again shortly",
                );
            }
        },
    };
    // Open for the whole run, however it ends. Opened after the slot, so a
    // refused invocation does not flip the status.
    let busy = app.track_task();
    let session = headers
        .get(SESSION_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .map_or_else(local_id, str::to_owned);
    // The invocation is the agent run AgentCore Evaluations looks for: an
    // `invoke_agent` span (Rig adopts it as the run's span, so it makes none
    // of its own), with the prompt and the answer when content is recorded.
    let agent_name = app.inner.agent.name().unwrap_or("agent").to_owned();
    let span = tracing::info_span!(
        "agentcore.invocation",
        otel.name = %format!("invoke_agent {agent_name}"),
        otel.kind = "server",
        agentcore.session_id = %session,
        gen_ai.operation.name = "invoke_agent",
        gen_ai.agent.name = %agent_name,
        gen_ai.task.input = tracing::field::Empty,
        gen_ai.task.output = tracing::field::Empty,
        agentcore.invocation.user_prompt = tracing::field::Empty,
        agentcore.invocation.agent_response = tracing::field::Empty,
    );
    if app.inner.records_invocation_content() {
        span.record("gen_ai.task.input", invocation.prompt.as_str());
        span.record(
            "agentcore.invocation.user_prompt",
            invocation.prompt.as_str(),
        );
    }
    // Without an OpenTelemetry layer there is nothing to continue: fine.
    let _ = rig_otel::set_session(&span, session.clone(), Some(rig_otel::extract(&headers)));

    let streams = headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|accept| accept.contains("text/event-stream"));
    if streams {
        stream(app, session, invocation.prompt, span, slot, busy)
    } else {
        whole(app, session, invocation.prompt, slot, busy)
            .instrument(span)
            .await
    }
}

/// The session's conversation as a request sends it: the newest messages
/// the window keeps, after a summary when the history has grown past the
/// summarizing trigger.
async fn history_of(inner: &Inner, session: &str) -> Result<Vec<Message>, HistoryError> {
    let mut messages = inner.history.load(session).await?;
    if let Some(summarizing) = &inner.summarizing
        && messages.len() > summarizing.trigger_messages
        && let Some(summarized) =
            summarize(inner.history.as_ref(), summarizing, session, &messages).await
    {
        messages = summarized;
    }
    // The summary, when the session has one, goes first; the window applies to
    // the messages after it, so a narrow window cannot drop it.
    let mut sent = Vec::new();
    let rest = match messages.split_first() {
        Some((first, rest)) if inner.summarizing.is_some() && is_summary(first) => {
            sent.push((**first).clone());
            rest
        }
        _ => messages.as_slice(),
    };
    sent.extend(
        view(rest, inner.window.as_ref())
            .iter()
            .map(|message| (**message).clone()),
    );
    Ok(sent)
}

async fn whole(
    app: AgentCoreApp,
    session: String,
    prompt: String,
    _slot: Option<OwnedSemaphorePermit>,
    _busy: BusyGuard,
) -> Response {
    let inner = &app.inner;
    let deadline = deadline_of(inner.max_run);
    let history = match within(deadline, history_of(inner, &session)).await {
        Ok(Ok(history)) => history,
        Ok(Err(failure)) => {
            tracing::warn!(error = %failure, "could not load the conversation");
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "the conversation could not be loaded",
            );
        }
        Err(_) => {
            tracing::warn!(max = ?inner.max_run, "agent run took too long");
            return timed_out(inner.max_run);
        }
    };
    let request = inner.agent.prompt(prompt).history(history);
    let request = match inner.record_content {
        Some(record) => request.record_content_telemetry(record),
        None => request,
    };
    let run = within(deadline, request.run()).await;
    match run {
        // A stopped run is dropped before `save`, so its turn is not kept.
        Err(_) => {
            tracing::warn!(max = ?inner.max_run, "agent run took too long");
            timed_out(inner.max_run)
        }
        Ok(Ok(response)) => {
            save(inner, &session, response.messages.clone()).await;
            let answer = response.output();
            record_answer(inner.records_invocation_content(), &answer);
            Json(json!({"response": answer, "status": "success"})).into_response()
        }
        Ok(Err(failure)) => {
            tracing::warn!(error = %failure, "agent run failed");
            error(StatusCode::INTERNAL_SERVER_ERROR, "the agent run failed")
        }
    }
}

/// The deadline for an invocation's run, from the start of the invocation.
fn deadline_of(max: Option<Duration>) -> Option<tokio::time::Instant> {
    max.map(|max| tokio::time::Instant::now() + max)
}

/// `future`, stopped at `deadline` when there is one.
async fn within<F: std::future::Future>(
    deadline: Option<tokio::time::Instant>,
    future: F,
) -> Result<F::Output, tokio::time::error::Elapsed> {
    match deadline {
        Some(deadline) => tokio::time::timeout_at(deadline, future).await,
        None => Ok(future.await),
    }
}

fn timeout_text(max: Option<Duration>) -> String {
    match max {
        Some(max) => format!("the agent run took longer than {max:?}"),
        None => "the agent run took too long".to_owned(),
    }
}

fn timed_out(max: Option<Duration>) -> Response {
    error(StatusCode::GATEWAY_TIMEOUT, &timeout_text(max))
}

/// Add a finished run to its session. A save that fails is logged, not
/// reported: the answer was already produced.
async fn save(inner: &Inner, session: &str, messages: Vec<Message>) {
    // With a summarizer, the window trims the session only down to its newest
    // 2 * trigger messages, so a summary (kept first) is not dropped while it
    // is still needed, and a failing summarizer cannot grow the session forever.
    let floor = inner.summarizing.as_ref().map_or(0, |summarizing| {
        summarizing.trigger_messages.saturating_mul(2)
    });
    let retained = Retained {
        window: inner.window.as_ref(),
        floor,
    };
    let window: &dyn Recent = match inner.summarizing {
        Some(_) => &retained,
        None => inner.window.as_ref(),
    };
    if let Err(failure) = inner.history.append(session, messages, window).await {
        tracing::warn!(error = %failure, "could not save the conversation");
    }
}

/// Stream the reply as server-sent events: `{"text": ...}` as the model
/// writes, then `{"response": ...}` with the whole answer, or `{"error": ...}`.
fn stream(
    app: AgentCoreApp,
    session: String,
    prompt: String,
    span: tracing::Span,
    slot: Option<OwnedSemaphorePermit>,
    busy: BusyGuard,
) -> Response {
    let (sender, receiver) = mpsc::unbounded_channel::<serde_json::Value>();
    tokio::spawn(
        async move {
            let _slot = slot;
            let _busy = busy;
            let inner = &app.inner;
            let deadline = deadline_of(inner.max_run);
            let history = match within(deadline, history_of(inner, &session)).await {
                Ok(Ok(history)) => history,
                Ok(Err(failure)) => {
                    tracing::warn!(error = %failure, "could not load the conversation");
                    let _ = sender.send(json!({"error": "the conversation could not be loaded"}));
                    return;
                }
                Err(_) => {
                    tracing::warn!(max = ?inner.max_run, "agent run took too long");
                    let _ = sender.send(json!({"error": timeout_text(inner.max_run)}));
                    return;
                }
            };
            let request = inner.agent.prompt(prompt).history(history);
            let request = match inner.record_content {
                Some(record) => request.record_content_telemetry(record),
                None => request,
            };
            let mut run = request.stream();
            loop {
                let item = match within(deadline, run.next()).await {
                    Ok(Some(item)) => item,
                    Ok(None) => break,
                    Err(_) => {
                        tracing::warn!(max = ?inner.max_run, "agent run took too long");
                        let _ = sender.send(json!({"error": timeout_text(inner.max_run)}));
                        return;
                    }
                };
                match item {
                    Ok(MultiTurnStreamItem::StreamAssistantItem(Item::Event(
                        StreamEvent::Text { text, .. },
                    ))) => {
                        let _ = sender.send(json!({"text": text}));
                    }
                    Ok(MultiTurnStreamItem::FinalResponse(response)) => {
                        save(inner, &session, response.messages.clone()).await;
                        let answer = response.output();
                        record_answer(inner.records_invocation_content(), &answer);
                        let _ = sender.send(json!({"response": answer}));
                        return;
                    }
                    Ok(_) => {}
                    Err(failure) => {
                        tracing::warn!(error = %failure, "agent run failed");
                        let _ = sender.send(json!({"error": "the agent run failed"}));
                        return;
                    }
                }
            }
            let _ = sender.send(json!({"error": "the agent run ended without an answer"}));
        }
        .instrument(span),
    );
    let events = futures::stream::unfold(receiver, |mut receiver| async move {
        let data = receiver.recv().await?;
        Some((
            Ok::<_, Infallible>(Event::default().data(data.to_string())),
            receiver,
        ))
    });
    Sse::new(events).into_response()
}

/// Record the agent's answer on the invocation span, the current one.
fn record_answer(record_content: bool, answer: &str) {
    if record_content {
        let span = tracing::Span::current();
        span.record("gen_ai.task.output", answer);
        span.record("agentcore.invocation.agent_response", answer);
    }
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"error": message, "status": "error"}))).into_response()
}

/// The port `PORT` names, or [`DEFAULT_PORT`] when it is not set. A value
/// that is not a port number warns and also gets [`DEFAULT_PORT`].
fn port_from_env(value: Result<String, std::env::VarError>) -> u16 {
    let value = match value {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => return DEFAULT_PORT,
        Err(std::env::VarError::NotUnicode(value)) => value.to_string_lossy().into_owned(),
    };
    value.parse().unwrap_or_else(|_| {
        tracing::warn!(
            port = %value,
            "PORT is not a port number from 0 to 65535; serving on {DEFAULT_PORT} instead"
        );
        DEFAULT_PORT
    })
}

/// Listen on every interface at `port`.
async fn listen(port: u16) -> Result<tokio::net::TcpListener, ServeError> {
    let address = SocketAddr::from(([0, 0, 0, 0], port));
    tokio::net::TcpListener::bind(address)
        .await
        .map_err(|source| ServeError::Bind { address, source })
}

#[cfg(test)]
mod tests {
    use std::env::VarError;

    use super::*;

    #[test]
    fn the_port_comes_from_env_or_the_default() {
        assert_eq!(port_from_env(Ok("3200".into())), 3200);
        assert_eq!(port_from_env(Err(VarError::NotPresent)), DEFAULT_PORT);
    }

    #[test]
    fn a_bad_port_falls_back_to_the_default() {
        for bad in ["", "abc", "70000", "-1", " 8080"] {
            assert_eq!(port_from_env(Ok(bad.into())), DEFAULT_PORT, "{bad:?}");
        }
    }

    #[tokio::test]
    async fn a_busy_port_is_an_error_that_names_the_address() {
        let held = listen(0).await.unwrap();
        let port = held.local_addr().unwrap().port();
        let message = listen(port)
            .await
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(
            message.starts_with(&format!("could not listen on 0.0.0.0:{port}: ")),
            "{message}"
        );
    }
}

#[cfg(doctest)]
#[doc = include_str!("../README.md")]
mod readme_doctests {}
