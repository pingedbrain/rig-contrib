//! The agent card and the Axum router that serve an agent over A2A.

use std::sync::Arc;

use a2a::{
    AgentCapabilities, AgentCard, AgentInterface, AgentSkill, HttpAuthSecurityScheme,
    SecurityScheme, TRANSPORT_PROTOCOL_HTTP_JSON, TRANSPORT_PROTOCOL_JSONRPC,
};
use a2a_server::agent_card::agent_card_router;
use a2a_server::jsonrpc::jsonrpc_router;
use a2a_server::rest::rest_router;
use a2a_server::{
    DefaultRequestHandler, InMemoryTaskStore, PushConfigStore, StaticAgentCard, TaskStore,
};
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use rig_agent::Agent;
use rig_core::wasm_compat::{WasmCompatSend, WasmCompatSync};

use crate::A2aExecutor;
use crate::push::{PushSettings, SharedTasks};
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
/// Streaming is on. Push notifications are off unless
/// [`push_notifications`](Self::push_notifications) turns them on.
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
    push: PushSettings,
}

/// The largest request body a server reads by default: 2 MiB.
pub const DEFAULT_MAX_REQUEST_BYTES: usize = 2 * 1024 * 1024;

impl A2aServer {
    /// Serve `agent` at `url`, the base URL clients reach this server at.
    ///
    /// The card takes the agent's name and description, lists both bindings
    /// under `url`, accepts `text/plain` and `application/json` parts, and
    /// declares one `chat` skill. Tasks live in an [`InMemoryTaskStore`],
    /// which keeps finished tasks without limit unless
    /// [`terminal_task_limit`](Self::terminal_task_limit) is set.
    pub fn new(agent: Agent, url: impl AsRef<str>) -> Self {
        let url = url.as_ref().trim_end_matches('/').to_owned();
        Self {
            card: default_card(&agent, &url),
            executor: A2aExecutor::new(agent),
            store: InMemoryTaskStore::new(),
            url,
            bearer: None,
            max_request_bytes: DEFAULT_MAX_REQUEST_BYTES,
            push: PushSettings::new(),
        }
    }

    /// Keep at most `max` finished tasks in memory, purging the oldest
    /// first. A purged task answers `GetTask` with `TaskNotFound`. Tasks
    /// still running are never purged. A `max` of 0 is treated as 1.
    ///
    /// Off by default: a client may fetch a finished task long after it ended,
    /// and a limit would change that answer. Set it for a server that runs
    /// for long and takes many tasks.
    ///
    /// Caveats:
    /// - A late status update for a purged task can bring it back. The
    ///   executor's save path falls back to `create` when `update` finds no
    ///   task (in `a2a-server-lf`, not in this crate), so a cancel or event
    ///   that arrives after the purge re-creates the task as a finished one,
    ///   where it can be purged again. Set the limit well above the number of
    ///   tasks in flight.
    /// - Only the default in-memory store has this limit. Call it before
    ///   [`task_store`](Self::task_store) or the limit is dropped; for a
    ///   custom store, set its own limit instead.
    #[must_use]
    pub fn terminal_task_limit(mut self, max: usize) -> Self {
        self.store = InMemoryTaskStore::new().with_terminal_task_limit(max.max(1));
        self
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
    #[doc(alias = "max_sessions")]
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
            + WasmCompatSend
            + WasmCompatSync
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
        T: rig_memory::AsyncStore + WasmCompatSend + WasmCompatSync + 'static,
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

    /// Accept push notification configs, so a client can ask the server to
    /// POST task updates to a webhook. Off by default.
    ///
    /// Push needs [`bearer_token`](Self::bearer_token): without one, the
    /// server logs an error when it is built and keeps push off, so the card
    /// says `pushNotifications: false` and a config set is refused with
    /// `PushNotificationNotSupported`. With push on, the card declares
    /// `pushNotifications: true`.
    ///
    /// Configs are kept in memory unless [`push_config_store`](Self::push_config_store)
    /// is set. At most 10 configs per task are accepted, and 10,000 in all
    /// counted since the server started. When the total is reached, configs
    /// of tasks that [`terminal_task_limit`](Self::terminal_task_limit) has
    /// purged are dropped, so the total counts live tasks.
    /// Each config is checked when it is saved: its URL must be `https`, on
    /// the list from [`push_allowed_hosts`](Self::push_allowed_hosts) if one
    /// is set, and not a literal IP in 100.64.0.0/10, 64:ff9b::/96, 2002::/16
    /// or ::a.b.c.d/96.
    ///
    /// The SDK's address guard runs when a webhook is called. It refuses
    /// 127.0.0.0/8, 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, 169.254.0.0/16,
    /// 224.0.0.0/4, 0.0.0.0, ::1, ::, fc00::/7, fe80::/10, ff00::/8, the
    /// IPv4-mapped forms of those IPv4 ranges, and the hosts `localhost`,
    /// `metadata.google.internal` and `metadata.azure.com`. It checks the
    /// address a name resolves to, and each redirect. It does not cover the
    /// ranges listed above, so a name resolving to them is not refused.
    /// Redirects are followed only to hosts that pass the same check.
    ///
    /// # Security
    ///
    /// - A webhook URL comes from the client. An open server with push on
    ///   lets any caller make it POST to any host the guard allows. Set an
    ///   allowlist when clients are not trusted.
    /// - The server signs nothing: there is no JWT or JWKS. It forwards the
    ///   client's `token` as the `A2A-Notification-Token` header and its
    ///   `authentication` credentials as `Authorization`. A receiver must
    ///   check both itself.
    /// - Each POST carries the full task update, including the agent's
    ///   output and any artifacts.
    /// - Anyone who holds a task id can attach a webhook to it, and so
    ///   receive that task's updates. Push requires a bearer token for this
    ///   reason, and the bearer token is the only check on who may do so.
    /// - A slow webhook delays the task. Delivery runs in the task's event
    ///   loop, one config at a time, and each POST has a 5 second timeout. A
    ///   task with 10 configs whose receivers all stall can take up to 50
    ///   seconds per update, and the client's stream waits for it.
    /// - A finished task's configs stay until the total cap is reached. Set
    ///   [`terminal_task_limit`](Self::terminal_task_limit) too: then the
    ///   oldest finished tasks are purged, and their configs are dropped at
    ///   the next save that reaches the total cap.
    ///
    /// ```ignore
    /// let server = A2aServer::new(agent, "https://agent.example.com")
    ///     .bearer_token(token)
    ///     .push_notifications(true)
    ///     .push_allowed_hosts(["hooks.example.com"])
    ///     .terminal_task_limit(1_000);
    /// ```
    #[must_use]
    pub fn push_notifications(mut self, enabled: bool) -> Self {
        self.push.enabled = enabled;
        self.card.capabilities.push_notifications = Some(enabled);
        self
    }

    /// Keep push configs in `store` instead of in memory. Also turns push
    /// notifications on, as [`push_notifications(true)`](Self::push_notifications)
    /// does. Every config still goes through the URL checks and the caps.
    #[must_use]
    pub fn push_config_store<P: PushConfigStore>(mut self, store: P) -> Self {
        self.push.store = Some(Arc::new(store));
        self.push_notifications(true)
    }

    /// Accept webhook URLs only on these hosts, compared without regard to
    /// case. A config naming another host is refused when it is saved. Takes
    /// effect only while push notifications are on.
    #[must_use]
    pub fn push_allowed_hosts<I, H>(mut self, hosts: I) -> Self
    where
        I: IntoIterator<Item = H>,
        H: Into<String>,
    {
        self.push.allowed_hosts = Some(hosts.into_iter().map(Into::into).collect());
        self
    }

    /// Drop the checks that keep a webhook URL from reaching a private
    /// target: the `https` requirement, the refusal on save of the literal
    /// ranges 100.64.0.0/10, 64:ff9b::/96, 2002::/16 and ::a.b.c.d/96, and
    /// the SDK's address guard (loopback, private and link-local addresses,
    /// DNS and redirects). Also lets the SDK send webhooks through the
    /// proxies named in the environment. For tests against a webhook on this
    /// machine only. The host allowlist still applies.
    #[must_use]
    pub fn allow_unguarded_push_urls(mut self) -> Self {
        self.push.allow_unguarded = true;
        self
    }

    /// Keep tasks in `store` instead of in memory.
    ///
    /// This replaces the default store, and with it any limit set by
    /// [`terminal_task_limit`](A2aServer::terminal_task_limit) called before
    /// it: that limit is dropped. Set the limit on `store` itself, for
    /// example with `InMemoryTaskStore::with_terminal_task_limit`.
    pub fn task_store<T: TaskStore>(self, store: T) -> A2aServer<T> {
        A2aServer {
            executor: self.executor,
            card: self.card,
            store,
            url: self.url,
            bearer: self.bearer,
            max_request_bytes: self.max_request_bytes,
            push: self.push,
        }
    }

    /// The agent card the router serves.
    pub fn card(&self) -> &AgentCard {
        &self.card
    }

    /// The Axum router serving the card and both bindings.
    pub fn into_router(self) -> axum::Router {
        let mut card = self.card;
        // Push needs a bearer token; without one it stays off and the card says so.
        let push_on = self.push.enabled && self.bearer.is_some();
        if self.push.enabled && !push_on {
            tracing::error!(
                "push notifications need a bearer_token on the A2aServer; push stays off"
            );
        }
        card.capabilities.push_notifications = Some(push_on);
        // The handler and the push wrapper read one task store: the wrapper
        // needs it to tell live tasks from purged ones.
        let tasks: Arc<dyn TaskStore> = Arc::new(self.store);
        let mut handler =
            DefaultRequestHandler::new(self.executor, SharedTasks(Arc::clone(&tasks)))
                .with_capabilities(card.capabilities.clone())
                .with_default_input_modes(card.default_input_modes.clone());
        if push_on {
            handler = handler.with_push_notifications(
                self.push.checked_store(Arc::clone(&tasks)),
                self.push.sender(),
            );
        }
        let handler = Arc::new(handler);
        let v03_card = v03::card(
            &card,
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
            .merge(agent_card_router(Arc::new(StaticAgentCard::new(card))))
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
