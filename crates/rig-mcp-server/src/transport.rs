//! Serving an [`McpServer`] over stdio or Streamable HTTP.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use rmcp::ServiceExt;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};

use crate::McpServer;
use crate::server::{BuildError, Handler};

/// Where the Streamable HTTP endpoint is mounted by [`McpServer::into_router`].
pub const MCP_PATH: &str = "/mcp";

/// Why a server stopped serving.
pub type ServeError = Box<dyn std::error::Error + Send + Sync>;

impl McpServer {
    /// Require `Authorization: Bearer <token>` on the HTTP endpoint; calls
    /// without it get HTTP 401. (Over stdio the client launched the server
    /// itself, so there is nothing to check.)
    #[must_use]
    pub fn bearer_token(mut self, token: impl Into<String>) -> Self {
        self.bearer = Some(token.into());
        self
    }

    /// Host names the HTTP endpoint answers to. Defaults to `localhost`,
    /// `127.0.0.1` and `::1` only, which protects a local server from DNS
    /// rebinding; set the public host name when deploying behind one.
    #[must_use]
    pub fn allowed_hosts(mut self, hosts: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.allowed_hosts = Some(hosts.into_iter().map(Into::into).collect());
        self
    }

    /// Serve over stdin and stdout until the client disconnects: what a
    /// local MCP client (Claude Code, Cursor, Zed) expects of a server it
    /// launches. Stdout carries the protocol, so log to stderr only.
    ///
    /// # Errors
    ///
    /// A [`BuildError`] when two tools, resources or prompts clash, or the transport's error.
    pub async fn serve_stdio(self) -> Result<(), ServeError> {
        let running = Handler::new(self)?.serve(rmcp::transport::stdio()).await?;
        running.waiting().await?;
        Ok(())
    }

    /// Serve over any MCP transport (an in-memory duplex, a socket, a custom
    /// one) until the client disconnects.
    ///
    /// # Errors
    ///
    /// A [`BuildError`] when two tools, resources or prompts clash, or the transport's error.
    pub async fn serve<T, E, A>(self, transport: T) -> Result<(), ServeError>
    where
        T: rmcp::transport::IntoTransport<rmcp::RoleServer, E, A>,
        E: std::error::Error + Send + Sync + 'static,
    {
        let running = Handler::new(self)?.serve(transport).await?;
        running.waiting().await?;
        Ok(())
    }

    /// The Streamable HTTP endpoint at [`MCP_PATH`], as an Axum router.
    ///
    /// # Errors
    ///
    /// [`BuildError::DuplicateTool`] when two tools share a name,
    /// [`BuildError::DuplicateResource`] when two resources share a URI, or
    /// [`BuildError::DuplicatePrompt`] when two prompts share a name.
    pub fn into_router(self) -> Result<axum::Router, BuildError> {
        let expected: Option<Arc<str>> = self
            .bearer
            .as_ref()
            .map(|token| format!("Bearer {token}").into());
        let mut config = StreamableHttpServerConfig::default();
        if let Some(hosts) = self.allowed_hosts.clone() {
            config = config.with_allowed_hosts(hosts);
        }
        let handler = Handler::new(self)?;
        let service = StreamableHttpService::new(
            move || Ok(handler.clone()),
            Arc::new(LocalSessionManager::default()),
            config,
        );
        Ok(axum::Router::new()
            .nest_service(MCP_PATH, service)
            .layer(axum::middleware::from_fn_with_state(expected, guard)))
    }
}

/// Answers 401 to a request without the expected `Authorization` header, or
/// passes every request when no token is set.
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
