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
//!   AgentCore Observability and Evaluations group a session's traces by.
//!
//! ```no_run
//! # async fn run(agent: rig_agent::Agent) -> Result<(), rig_agentcore::ServeError> {
//! rig_agentcore::AgentCoreApp::new(agent).serve().await
//! # }
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

mod history;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Json;
use axum::extract::State;
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
use rig_memory::recent::{LastMessages, Recent, TokenBudget, view};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;
use tracing::Instrument;

use crate::history::{
    HistoryError, HistoryStore, InMemory, Persistent, Summarizing, local_id, summarize,
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
    record_content: bool,
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
                record_content: false,
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
    /// What the window also does depends on the store. In memory, messages
    /// beyond it are forgotten when a run is saved, so memory stays bounded.
    /// With [`history_store`](Self::history_store), the store keeps the whole
    /// conversation and the window only limits what is sent, so a later,
    /// wider window sees older turns again.
    #[must_use]
    pub fn history_window(self, window: impl Recent + 'static) -> Self {
        self.rebuild(|inner| inner.window = Arc::new(window))
    }

    /// Summarize a session's older history with `summarizer`, an agent
    /// without tools, once the session holds more than `trigger_messages`
    /// messages. The messages before the newest `keep_recent` are replaced by
    /// one summary message, which becomes the session's first message; the
    /// newest messages stay verbatim. The cut never separates a tool call from
    /// its result. Summarizing runs before the invocation answers, so the
    /// answer waits for it.
    ///
    /// Set `trigger_messages` above `keep_recent`. The summary is made from the
    /// messages it replaces, including an earlier summary, so it carries on
    /// from one summary to the next. Without a store, the summary is kept in
    /// memory with the session, and the window does not trim the session until
    /// it is summarized. With [`history_store`](Self::history_store), the
    /// summary is written to the store's log in place of the messages it
    /// replaces, which the log keeps in its history.
    ///
    /// If the summary fails (the summarizer errors or returns nothing, or the
    /// store refuses the change), a warning is logged, the session keeps its
    /// full history, and the invocation answers from it as usual. The next
    /// invocation tries again. The window still applies to what is sent, so a
    /// summary older than the window's budget is not sent: keep the window wide
    /// enough for the summary and the kept messages.
    #[must_use]
    pub fn summarize_history(
        self,
        summarizer: Agent,
        keep_recent: usize,
        trigger_messages: usize,
    ) -> Self {
        self.rebuild(|inner| {
            inner.summarizing = Some(Summarizing {
                summarizer,
                keep_recent,
                trigger_messages,
            });
        })
    }

    /// Record prompts and replies on the GenAI spans. AgentCore Evaluations
    /// needs them to judge a session; they are off by default, as in Rig,
    /// because they put conversation content into your telemetry.
    #[must_use]
    pub fn record_content(self, record: bool) -> Self {
        self.rebuild(|inner| inner.record_content = record)
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
        axum::Router::new()
            .route("/invocations", post(invoke))
            .route("/ping", get(ping))
            .with_state(self)
    }

    /// Serve on `0.0.0.0` at `PORT` (default 8080, what AgentCore sends to)
    /// until the process gets Ctrl-C or SIGTERM.
    pub async fn serve(self) -> Result<(), ServeError> {
        let port = std::env::var("PORT")
            .ok()
            .and_then(|port| port.parse().ok())
            .unwrap_or(DEFAULT_PORT);
        let address = SocketAddr::from(([0, 0, 0, 0], port));
        let listener = tokio::net::TcpListener::bind(address)
            .await
            .map_err(|source| ServeError::Bind { address, source })?;
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

async fn ping() -> Json<serde_json::Value> {
    Json(json!({"status": "Healthy"}))
}

async fn invoke(
    State(app): State<AgentCoreApp>,
    headers: HeaderMap,
    body: Result<Json<Invocation>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(invocation)) = body else {
        return error(
            StatusCode::BAD_REQUEST,
            "the body must be a JSON object with a \"prompt\" string",
        );
    };
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
    if app.inner.record_content {
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
        stream(app, session, invocation.prompt, span)
    } else {
        whole(app, session, invocation.prompt)
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
    Ok(view(&messages, inner.window.as_ref())
        .iter()
        .map(|message| (**message).clone())
        .collect())
}

async fn whole(app: AgentCoreApp, session: String, prompt: String) -> Response {
    let inner = &app.inner;
    let history = match history_of(inner, &session).await {
        Ok(history) => history,
        Err(failure) => {
            tracing::warn!(error = %failure, "could not load the conversation");
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "the conversation could not be loaded",
            );
        }
    };
    let run = inner
        .agent
        .prompt(prompt)
        .history(history)
        .record_content_telemetry(inner.record_content)
        .run()
        .await;
    match run {
        Ok(response) => {
            save(inner, &session, response.messages.clone()).await;
            let answer = response.output();
            record_answer(inner.record_content, &answer);
            Json(json!({"response": answer, "status": "success"})).into_response()
        }
        Err(failure) => {
            tracing::warn!(error = %failure, "agent run failed");
            error(StatusCode::INTERNAL_SERVER_ERROR, "the agent run failed")
        }
    }
}

/// Add a finished run to its session. A save that fails is logged, not
/// reported: the answer was already produced.
async fn save(inner: &Inner, session: &str, messages: Vec<Message>) {
    // With a summarizer, the session is kept whole until it is summarized:
    // trimming it to the window here would drop the summary with the messages
    // before it.
    let keep_all = LastMessages(usize::MAX);
    let window: &dyn Recent = match inner.summarizing {
        Some(_) => &keep_all,
        None => inner.window.as_ref(),
    };
    if let Err(failure) = inner.history.append(session, messages, window).await {
        tracing::warn!(error = %failure, "could not save the conversation");
    }
}

/// Stream the reply as server-sent events: `{"text": ...}` as the model
/// writes, then `{"response": ...}` with the whole answer, or `{"error": ...}`.
fn stream(app: AgentCoreApp, session: String, prompt: String, span: tracing::Span) -> Response {
    let (sender, receiver) = mpsc::unbounded_channel::<serde_json::Value>();
    tokio::spawn(
        async move {
            let inner = &app.inner;
            let history = match history_of(inner, &session).await {
                Ok(history) => history,
                Err(failure) => {
                    tracing::warn!(error = %failure, "could not load the conversation");
                    let _ = sender.send(json!({"error": "the conversation could not be loaded"}));
                    return;
                }
            };
            let mut run = inner
                .agent
                .prompt(prompt)
                .history(history)
                .record_content_telemetry(inner.record_content)
                .stream();
            while let Some(item) = run.next().await {
                match item {
                    Ok(MultiTurnStreamItem::StreamAssistantItem(Item::Event(
                        StreamEvent::Text { text, .. },
                    ))) => {
                        let _ = sender.send(json!({"text": text}));
                    }
                    Ok(MultiTurnStreamItem::FinalResponse(response)) => {
                        save(inner, &session, response.messages.clone()).await;
                        let answer = response.output();
                        record_answer(inner.record_content, &answer);
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
