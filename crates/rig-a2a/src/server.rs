//! The agent card and the Axum router that serve an agent over A2A.

use std::sync::Arc;

use a2a::{
    AgentCapabilities, AgentCard, AgentInterface, AgentSkill, HttpAuthSecurityScheme,
    SecurityScheme, TRANSPORT_PROTOCOL_HTTP_JSON, TRANSPORT_PROTOCOL_JSONRPC,
};
use a2a_server::agent_card::agent_card_router;
use a2a_server::jsonrpc::jsonrpc_router;
use a2a_server::rest::rest_router;
use a2a_server::{DefaultRequestHandler, InMemoryTaskStore, StaticAgentCard, TaskStore};
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use rig_agent::Agent;

use crate::A2aExecutor;
use crate::v03::{self, V03_PATH};

/// The path of the JSON-RPC binding under the server's base URL.
pub const JSONRPC_PATH: &str = "/jsonrpc";
/// The path of the HTTP+JSON binding under the server's base URL.
pub const REST_PATH: &str = "/rest";

/// A Rig agent served over A2A: its agent card, its task store, and the
/// router [`into_router`](Self::into_router) builds from them.
///
/// The router serves the card at `/.well-known/agent-card.json`, the JSON-RPC
/// binding at [`JSONRPC_PATH`] and the HTTP+JSON binding at [`REST_PATH`].
/// Streaming is on; push notifications are off.
///
/// For peers still on A2A v0.3, the same agent is served under
/// [`V03_PATH`](crate::V03_PATH): its v0.3 card at
/// `/v03/.well-known/agent-card.json` and v0.3 JSON-RPC at `/v03`. Point a
/// v0.3 client at `{url}/v03`.
pub struct A2aServer<S = InMemoryTaskStore> {
    executor: A2aExecutor,
    card: AgentCard,
    store: S,
    url: String,
    bearer: Option<String>,
    max_request_bytes: usize,
}

/// The largest request body a server reads by default: 2 MiB.
pub const DEFAULT_MAX_REQUEST_BYTES: usize = 2 * 1024 * 1024;

impl A2aServer {
    /// Serve `agent` at `url`, the base URL clients reach this server at.
    ///
    /// The card takes the agent's name and description, lists both bindings
    /// under `url`, accepts `text/plain` and `application/json` parts, and
    /// declares one `chat` skill. Tasks live in an [`InMemoryTaskStore`].
    pub fn new(agent: Agent, url: impl AsRef<str>) -> Self {
        let url = url.as_ref().trim_end_matches('/').to_owned();
        Self {
            card: default_card(&agent, &url),
            executor: A2aExecutor::new(agent),
            store: InMemoryTaskStore::new(),
            url,
            bearer: None,
            max_request_bytes: DEFAULT_MAX_REQUEST_BYTES,
        }
    }
}

impl<S: TaskStore> A2aServer<S> {
    /// Change the agent card, for example its skills, version or provider.
    ///
    /// The router serves the card as edited and refuses a part that declares
    /// a media type outside its `default_input_modes`. The routes stay at
    /// [`JSONRPC_PATH`] and [`REST_PATH`] whatever the card's interfaces say.
    #[must_use]
    pub fn edit_card(mut self, edit: impl FnOnce(&mut AgentCard)) -> Self {
        edit(&mut self.card);
        self
    }

    /// Remember at most `max` conversations; see [`A2aExecutor::max_contexts`].
    #[must_use]
    pub fn max_contexts(mut self, max: usize) -> Self {
        self.executor = self.executor.max_contexts(max);
        self
    }

    /// Answer every task with a `T` as JSON; see [`A2aExecutor::typed`]. The
    /// card then declares `application/json` output and, in each skill's
    /// description, the JSON schema of `T`, for callers to know what comes back.
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
        self.executor = self.executor.typed::<T>();
        let schema = serde_json::to_string(&schemars::schema_for!(T)).unwrap_or_default();
        let json = vec!["application/json".to_owned()];
        self.card.default_output_modes = json.clone();
        for skill in &mut self.card.skills {
            skill.output_modes = Some(json.clone());
            skill.description = format!(
                "{} Answers with JSON matching this schema: {schema}",
                skill.description
            );
        }
        self
    }

    /// Keep conversations in a [`rig_memory`] store; see
    /// [`A2aExecutor::history_store`].
    #[must_use]
    pub fn history_store<T>(mut self, store: T) -> Self
    where
        T: rig_memory::AsyncStore + Send + Sync + 'static,
    {
        self.executor = self.executor.history_store(store);
        self
    }

    /// How much of a conversation each request sends; see
    /// [`A2aExecutor::history_window`].
    #[must_use]
    pub fn history_window(mut self, window: impl rig_memory::recent::Recent + 'static) -> Self {
        self.executor = self.executor.history_window(window);
        self
    }

    /// Run at most `max` tasks at once; see [`A2aExecutor::max_concurrent_tasks`].
    #[must_use]
    pub fn max_concurrent_tasks(mut self, max: usize) -> Self {
        self.executor = self.executor.max_concurrent_tasks(max);
        self
    }

    /// Stop runs longer than `max`; see [`A2aExecutor::max_run_duration`].
    #[must_use]
    pub fn max_run_duration(mut self, max: std::time::Duration) -> Self {
        self.executor = self.executor.max_run_duration(max);
        self
    }

    /// Refuse request bodies over `bytes` with HTTP 413. Defaults to
    /// [`DEFAULT_MAX_REQUEST_BYTES`].
    #[must_use]
    pub fn max_request_bytes(mut self, bytes: usize) -> Self {
        self.max_request_bytes = bytes;
        self
    }

    /// Require `Authorization: Bearer <token>` on every call. The agent
    /// cards stay public, as discovery needs them, and declare the scheme;
    /// calls without the token get HTTP 401.
    #[must_use]
    pub fn bearer_token(mut self, token: impl Into<String>) -> Self {
        self.bearer = Some(token.into());
        let scheme = SecurityScheme::HttpAuth(HttpAuthSecurityScheme {
            scheme: "bearer".to_owned(),
            description: None,
            bearer_format: None,
        });
        self.card.security_schemes = Some([("bearer".to_owned(), scheme)].into());
        self.card.security_requirements = Some(vec![[("bearer".to_owned(), vec![])].into()]);
        self
    }

    /// Keep tasks in `store` instead of in memory.
    pub fn task_store<T: TaskStore>(self, store: T) -> A2aServer<T> {
        A2aServer {
            executor: self.executor,
            card: self.card,
            store,
            url: self.url,
            bearer: self.bearer,
            max_request_bytes: self.max_request_bytes,
        }
    }

    /// The agent card the router serves.
    pub fn card(&self) -> &AgentCard {
        &self.card
    }

    /// The Axum router serving the card and both bindings.
    pub fn into_router(self) -> axum::Router {
        let handler = Arc::new(
            DefaultRequestHandler::new(self.executor, self.store)
                .with_capabilities(self.card.capabilities.clone())
                .with_default_input_modes(self.card.default_input_modes.clone()),
        );
        let v03_card = v03::card(
            &self.card,
            &format!("{}{V03_PATH}", self.url),
            self.bearer.is_some(),
        );
        let expected: Option<Arc<str>> = self.bearer.map(|token| format!("Bearer {token}").into());
        let auth = axum::middleware::from_fn_with_state(expected, guard);
        let calls = axum::Router::new()
            .nest(JSONRPC_PATH, jsonrpc_router(Arc::clone(&handler)))
            .nest(REST_PATH, rest_router(Arc::clone(&handler)))
            .nest(V03_PATH, v03::router(handler))
            .route_layer(auth)
            // At the body itself, so it holds whichever way a handler reads it.
            .layer(tower_http::limit::RequestBodyLimitLayer::new(
                self.max_request_bytes,
            ));
        // The cards are routed after the guard, so discovery needs no token.
        calls
            .merge(agent_card_router(Arc::new(StaticAgentCard::new(self.card))))
            .route(
                &format!("{V03_PATH}{}", a2a_server::WELL_KNOWN_AGENT_CARD_PATH),
                axum::routing::get(move || async move { axum::Json(v03_card) }),
            )
    }
}

/// Answers 401 to a call without the expected `Authorization` header, or
/// passes every call when no token is set.
async fn guard(State(expected): State<Option<Arc<str>>>, request: Request, next: Next) -> Response {
    let given = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    match (expected.as_deref(), given) {
        (None, _) => next.run(request).await,
        (Some(expected), Some(given)) if same(expected, given) => next.run(request).await,
        _ => (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
        )
            .into_response(),
    }
}

/// Compares in time independent of where the strings differ.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0, |acc, (x, y)| acc | (x ^ y))
            == 0
}

fn default_card(agent: &Agent, url: &str) -> AgentCard {
    let url = url.trim_end_matches('/');
    let name = agent.name().unwrap_or("Rig agent").to_owned();
    let description = agent
        .description()
        .unwrap_or("An agent built with Rig.")
        .to_owned();
    AgentCard {
        skills: vec![AgentSkill {
            id: "chat".to_owned(),
            name: name.clone(),
            description: description.clone(),
            tags: vec!["chat".to_owned()],
            examples: None,
            input_modes: None,
            output_modes: None,
            security_requirements: None,
        }],
        name,
        description,
        version: "1.0.0".to_owned(),
        supported_interfaces: vec![
            AgentInterface::new(format!("{url}{JSONRPC_PATH}"), TRANSPORT_PROTOCOL_JSONRPC),
            AgentInterface::new(format!("{url}{REST_PATH}"), TRANSPORT_PROTOCOL_HTTP_JSON),
        ],
        capabilities: AgentCapabilities {
            streaming: Some(true),
            push_notifications: Some(false),
            extensions: None,
            extended_agent_card: None,
        },
        default_input_modes: vec!["text/plain".to_owned(), "application/json".to_owned()],
        default_output_modes: vec!["text/plain".to_owned()],
        provider: None,
        documentation_url: None,
        icon_url: None,
        security_schemes: None,
        security_requirements: None,
        signatures: None,
    }
}
