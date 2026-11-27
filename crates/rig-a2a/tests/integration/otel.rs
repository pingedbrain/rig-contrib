//! With the `otel` feature, a call to a remote agent and the task it starts
//! make one OpenTelemetry trace, in the session of the task's context (or the
//! one AWS Bedrock AgentCore names).
#![cfg(all(feature = "otel", not(target_family = "wasm")))]

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, ensure};
use opentelemetry_sdk::metrics::InMemoryMetricExporter;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SpanData};
use rig_a2a::a2a::{Message, Part, Role, SendMessageRequest};
use rig_a2a::a2a_client::A2AClientFactory;
use rig_a2a::a2a_client::agent_card::AgentCardResolver;
use rig_a2a::a2a_client::auth::AuthInterceptor;
use rig_a2a::{A2aServer, RemoteAgent, V03_PATH};
use rig_agent::AgentBuilder;
use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};
use rig_otel::{SESSION_ID, Telemetry};

/// Serve an agent answering `pong` once, on a free loopback port.
async fn serve() -> anyhow::Result<String> {
    let model = MockCompletionModel::from_stream_turns([vec![
        MockStreamEvent::text("pong"),
        MockStreamEvent::final_response_with_default_usage(),
    ]]);
    let agent = AgentBuilder::new(model).name("Echo").build();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let app = A2aServer::new(agent, &url).into_router();
    tokio::spawn(async move { rig_a2a::axum::serve(listener, app).await });
    Ok(url)
}

/// Telemetry exported to memory, for the current thread (and so for the
/// server tasks a current-thread runtime spawns).
struct Capture {
    telemetry: Telemetry,
    spans: InMemorySpanExporter,
    _guard: tracing::subscriber::DefaultGuard,
}

impl Capture {
    fn start() -> anyhow::Result<Self> {
        crate::pin_global_default();
        let spans = InMemorySpanExporter::default();
        let (telemetry, subscriber) = Telemetry::builder()
            .span_exporter(spans.clone())
            .metric_exporter(InMemoryMetricExporter::default())
            .logs(false)
            .build()?;
        Ok(Self {
            telemetry,
            spans,
            _guard: tracing::subscriber::set_default(subscriber),
        })
    }

    /// The finished spans, once one named `name` has ended (the server may
    /// close its task span just after the client has the answer).
    async fn spans_after(&self, name: &str) -> anyhow::Result<Vec<SpanData>> {
        for _ in 0..200 {
            self.telemetry.force_flush()?;
            let spans = self.spans.get_finished_spans()?;
            if spans.iter().any(|span| span.name == name) {
                return Ok(spans);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        anyhow::bail!("no `{name}` span ended")
    }
}

fn find<'a>(spans: &'a [SpanData], name: &str) -> anyhow::Result<&'a SpanData> {
    spans
        .iter()
        .find(|span| span.name == name)
        .with_context(|| format!("no `{name}` span among {:?}", names(spans)))
}

fn names(spans: &[SpanData]) -> Vec<&str> {
    spans.iter().map(|span| span.name.as_ref()).collect()
}

/// Whether `span` is `ancestor` or one of the spans under it.
fn descends<'a>(spans: &'a [SpanData], mut span: &'a SpanData, ancestor: &SpanData) -> bool {
    loop {
        if span.span_context == ancestor.span_context {
            return true;
        }
        match spans
            .iter()
            .find(|parent| parent.span_context.span_id() == span.parent_span_id)
        {
            Some(parent) => span = parent,
            None => return false,
        }
    }
}

fn session(span: &SpanData) -> Option<String> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == SESSION_ID)
        .map(|kv| kv.value.as_str().into_owned())
}

#[tokio::test(flavor = "current_thread")]
async fn a_call_and_the_remote_task_make_one_trace_on_both_versions() -> anyhow::Result<()> {
    for prefix in ["", V03_PATH] {
        let capture = Capture::start()?;
        let url = serve().await?;
        let reply = RemoteAgent::connect(&format!("{url}{prefix}"))
            .await?
            .send("ping", None)
            .await?;
        ensure!(reply.text == "pong", "{prefix}: {reply:?}");

        let spans = capture.spans_after("a2a.task").await?;
        let call = find(&spans, "a2a.call")?;
        let task = find(&spans, "a2a.task")?;
        let trace = call.span_context.trace_id();
        ensure!(
            task.span_context.trace_id() == trace,
            "{prefix}: the task continues the caller's trace"
        );
        ensure!(
            task.parent_span_id == call.span_context.span_id(),
            "{prefix}: the task's parent is the call"
        );
        ensure!(
            session(task) == reply.context_id,
            "{prefix}: the session is the context: {:?}",
            task.attributes
        );
        // The agent's own spans (the run, the model call) join the trace and
        // the session too.
        let under_task: Vec<&SpanData> = spans
            .iter()
            .filter(|span| descends(&spans, span, task))
            .collect();
        ensure!(
            under_task.len() > 1
                && under_task
                    .iter()
                    .all(|span| session(span) == reply.context_id),
            "{prefix}: {:?}",
            under_task
                .iter()
                .map(|span| (span.name.as_ref(), session(span)))
                .collect::<Vec<_>>()
        );
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn the_agentcore_session_header_names_the_session() -> anyhow::Result<()> {
    let capture = Capture::start()?;
    let url = serve().await?;
    let card = AgentCardResolver::new(None).resolve(&url).await?;
    let client = A2AClientFactory::builder()
        .with_interceptor(Arc::new(AuthInterceptor::custom(
            "X-Amzn-Bedrock-AgentCore-Runtime-Session-Id",
            "agentcore-session-1",
        )))
        .build()
        .create_from_card(&card)
        .await?;
    client
        .send_message(&SendMessageRequest {
            message: Message::new(Role::User, vec![Part::text("ping")]),
            configuration: None,
            metadata: None,
            tenant: None,
        })
        .await?;

    let spans = capture.spans_after("a2a.task").await?;
    let task = find(&spans, "a2a.task")?;
    ensure!(
        session(task).as_deref() == Some("agentcore-session-1"),
        "{:?}",
        task.attributes
    );
    Ok(())
}
