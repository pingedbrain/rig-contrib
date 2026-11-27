//! A Rig agent run with telemetry exported in memory: no keys, no network.

use opentelemetry::Value;
use opentelemetry::trace::TraceContextExt;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
use opentelemetry_sdk::metrics::{
    InMemoryMetricExporter, InMemoryMetricExporterBuilder, Temporality,
};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SpanData};
use rig_agent::AgentBuilder;
use rig_core::completion::Usage;
use rig_core::test_utils::{MockCompletionModel, MockTurn};
use rig_core::tool::{Tool, ToolContext};
use rig_otel::{SCOPE, SESSION_ID, Telemetry};
use serde_json::{Value as Json, json};
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;

struct Add;

#[derive(Debug)]
enum Never {}

impl std::fmt::Display for Never {
    fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Ok(())
    }
}

impl std::error::Error for Never {}

impl Tool for Add {
    const NAME: &'static str = "add";
    type Args = Json;
    type Output = String;
    type Error = Never;

    fn description(&self) -> String {
        "Add two numbers.".into()
    }

    fn parameters(&self) -> Json {
        json!({"type": "object", "properties": {"a": {"type": "number"}, "b": {"type": "number"}}})
    }

    async fn call(&self, _context: &mut ToolContext, args: Json) -> Result<String, Never> {
        let sum = args.get("a").and_then(Json::as_f64).unwrap_or(0.0)
            + args.get("b").and_then(Json::as_f64).unwrap_or(0.0);
        Ok(sum.to_string())
    }
}

fn tokens(input: u64, output: u64) -> Usage {
    let mut usage = Usage::default();
    usage.input_tokens = Some(input);
    usage.output_tokens = Some(output);
    usage.total_tokens = Some(input + output);
    usage
}

struct Captured {
    spans: Vec<SpanData>,
    metrics: InMemoryMetricExporter,
}

/// Run `body` under telemetry exporting to memory, then flush.
async fn capture<F: Future<Output = ()>>(body: impl FnOnce() -> F) -> Captured {
    crate::pin_global_default();
    let spans = InMemorySpanExporter::default();
    let metrics = InMemoryMetricExporterBuilder::new()
        .with_temporality(Temporality::Cumulative)
        .build();
    let (telemetry, subscriber) = Telemetry::builder()
        .service_name("rig-otel-test")
        .span_exporter(spans.clone())
        .metric_exporter(metrics.clone())
        .logs(false)
        .build()
        .unwrap();
    let guard = tracing::subscriber::set_default(subscriber);
    body().await;
    drop(guard);
    telemetry.force_flush().unwrap();
    Captured {
        spans: spans.get_finished_spans().unwrap(),
        metrics,
    }
}

async fn run_agent() {
    let model = MockCompletionModel::from_turns([
        MockTurn::tool_call("call-1", "add", json!({"a": 2, "b": 40})).with_usage(tokens(30, 5)),
        MockTurn::text("42").with_usage(tokens(50, 2)),
    ]);
    let agent = AgentBuilder::new(model)
        .name("calculator")
        .tool(Add)
        .default_max_turns(3)
        .build();
    agent.prompt("What is 2 + 40?").await.unwrap();
}

fn attribute<'a>(span: &'a SpanData, key: &str) -> Option<&'a Value> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| &kv.value)
}

fn operation(span: &SpanData) -> Option<String> {
    attribute(span, "gen_ai.operation.name").map(Value::to_string)
}

#[tokio::test]
async fn an_agent_run_exports_its_spans_under_the_agentcore_scope() {
    let captured = capture(run_agent).await;

    let operations: Vec<String> = captured.spans.iter().filter_map(operation).collect();
    assert!(
        operations.contains(&"invoke_agent".to_owned()),
        "{operations:?}"
    );
    assert_eq!(
        operations.iter().filter(|name| *name == "chat").count(),
        2,
        "{operations:?}"
    );
    assert!(
        operations.contains(&"execute_tool".to_owned()),
        "{operations:?}"
    );
    for span in &captured.spans {
        assert_eq!(span.instrumentation_scope.name(), SCOPE);
    }
}

#[tokio::test]
async fn model_and_tool_calls_become_genai_metrics() {
    let captured = capture(run_agent).await;
    let resource_metrics = captured.metrics.get_finished_metrics().unwrap();
    let last = resource_metrics.last().expect("metrics exported");

    let mut durations = Vec::new();
    let mut tokens = Vec::new();
    for scope in last.scope_metrics() {
        assert_eq!(scope.scope().name(), SCOPE);
        for metric in scope.metrics() {
            let AggregatedMetrics::F64(MetricData::Histogram(histogram)) = metric.data() else {
                if let AggregatedMetrics::U64(MetricData::Histogram(histogram)) = metric.data() {
                    for point in histogram.data_points() {
                        let kind = point
                            .attributes()
                            .find(|kv| kv.key.as_str() == "gen_ai.token.type")
                            .map(|kv| kv.value.to_string());
                        tokens.push((metric.name().to_owned(), kind, point.sum()));
                    }
                }
                continue;
            };
            for point in histogram.data_points() {
                let name = point
                    .attributes()
                    .find(|kv| kv.key.as_str() == "gen_ai.operation.name")
                    .map(|kv| kv.value.to_string());
                durations.push((metric.name().to_owned(), name, point.count()));
            }
        }
    }

    assert!(
        durations.contains(&(
            "gen_ai.client.operation.duration".into(),
            Some("chat".into()),
            2
        )),
        "{durations:?}"
    );
    assert!(
        durations.contains(&(
            "gen_ai.client.operation.duration".into(),
            Some("execute_tool".into()),
            1
        )),
        "{durations:?}"
    );
    assert!(
        tokens.contains(&("gen_ai.client.token.usage".into(), Some("input".into()), 80)),
        "{tokens:?}"
    );
    assert!(
        tokens.contains(&("gen_ai.client.token.usage".into(), Some("output".into()), 7)),
        "{tokens:?}"
    );
}

#[tokio::test]
async fn a_session_reaches_every_span_under_it() {
    let captured = capture(|| async {
        let conversation = tracing::info_span!("conversation");
        rig_otel::set_session(&conversation, "session-42", None).unwrap();
        run_agent().instrument(conversation).await;
    })
    .await;

    // Rig adopts an enclosing span as the agent's: two model calls and a
    // tool call under the conversation span.
    let genai = captured
        .spans
        .iter()
        .filter(|span| operation(span).is_some())
        .count();
    assert_eq!(genai, 3);
    for span in &captured.spans {
        assert_eq!(
            attribute(span, SESSION_ID).map(Value::to_string).as_deref(),
            Some("session-42"),
            "{}",
            span.name
        );
    }
}

#[tokio::test]
async fn a_trace_and_its_session_cross_into_another_process() {
    let mut headers = http::HeaderMap::new();
    let mut caller_trace = None;
    let caller = capture(|| async {
        let call = tracing::info_span!("call remote agent");
        rig_otel::set_session(&call, "session-7", None).unwrap();
        caller_trace = Some(call.context().span().span_context().trace_id());
        let _entered = call.enter();
        rig_otel::inject(&mut headers);
    })
    .await;
    assert!(headers.contains_key("traceparent"));
    assert!(
        headers
            .get("baggage")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .contains("session.id=session-7")
    );
    assert!(!caller.spans.is_empty());

    // The other side: a request handler continuing the trace.
    let remote = capture(|| async {
        let handle = tracing::info_span!("handle request");
        handle.set_parent(rig_otel::extract(&headers)).unwrap();
        run_agent().instrument(handle).await;
    })
    .await;

    let caller_trace = caller_trace.unwrap();
    for span in &remote.spans {
        assert_eq!(
            span.span_context.trace_id(),
            caller_trace,
            "{} is in another trace",
            span.name
        );
        assert_eq!(
            attribute(span, SESSION_ID).map(Value::to_string).as_deref(),
            Some("session-7"),
            "{}",
            span.name
        );
    }
}

#[tokio::test]
async fn an_agent_span_is_the_exported_invoke_agent_run_with_the_session() {
    let captured = capture(|| async {
        let span = rig_otel::agent_span("calculator");
        rig_otel::set_session(&span, "session-9", None).unwrap();
        run_agent().instrument(span).await;
    })
    .await;

    // The outer span is the run's: exactly one invoke_agent, named as
    // AgentCore and GenAI backends name a run, carrying the session.
    let runs: Vec<&SpanData> = captured
        .spans
        .iter()
        .filter(|span| operation(span).as_deref() == Some("invoke_agent"))
        .collect();
    assert_eq!(
        runs.len(),
        1,
        "{:?}",
        captured.spans.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
    let run = runs[0];
    assert_eq!(run.name, "invoke_agent calculator");
    assert_eq!(
        attribute(run, "gen_ai.agent.name")
            .map(Value::to_string)
            .as_deref(),
        Some("calculator")
    );
    // Every span under it, the two model calls and the tool call, is in the session.
    assert_eq!(
        captured.spans.len(),
        4,
        "{:?}",
        captured.spans.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
    for span in &captured.spans {
        assert_eq!(
            attribute(span, SESSION_ID).map(Value::to_string).as_deref(),
            Some("session-9"),
            "{}",
            span.name
        );
    }
}

#[tokio::test]
async fn a_session_keeps_the_rest_of_the_callers_baggage() {
    let mut headers = http::HeaderMap::new();
    headers.insert(
        "baggage",
        http::HeaderValue::from_static("tenant=acme,session.id=old"),
    );
    let mut forwarded = http::HeaderMap::new();
    capture(|| async {
        let span = tracing::info_span!("handle request");
        rig_otel::set_session(&span, "new", Some(rig_otel::extract(&headers))).unwrap();
        let _entered = span.enter();
        rig_otel::inject(&mut forwarded);
    })
    .await;

    let baggage = forwarded
        .get("baggage")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(baggage.contains("tenant=acme"), "{baggage}");
    assert!(
        baggage.contains("session.id=new") && !baggage.contains("old"),
        "{baggage}"
    );
}

/// A tool that always fails, to script a call that errors.
struct Fail;

impl Tool for Fail {
    const NAME: &'static str = "fail";
    type Args = Json;
    type Output = String;
    type Error = std::io::Error;

    fn description(&self) -> String {
        "Always fails.".into()
    }

    fn parameters(&self) -> Json {
        json!({"type": "object", "properties": {}})
    }

    async fn call(&self, _context: &mut ToolContext, _args: Json) -> Result<String, Self::Error> {
        Err(std::io::Error::other("disk full"))
    }
}

/// One metric data point: its name, unit, attributes, and its value (the
/// count for a histogram, the sum for a counter).
#[derive(Debug)]
struct Point {
    name: String,
    unit: String,
    attributes: Vec<(String, String)>,
    value: u64,
}

fn points(captured: &Captured) -> Vec<Point> {
    let resource_metrics = captured.metrics.get_finished_metrics().unwrap();
    let last = resource_metrics.last().expect("metrics exported");
    let mut out = Vec::new();
    for scope in last.scope_metrics() {
        for metric in scope.metrics() {
            let name = metric.name().to_owned();
            let unit = metric.unit().to_owned();
            let mut push = |attributes: Vec<(String, String)>, value: u64| {
                out.push(Point {
                    name: name.clone(),
                    unit: unit.clone(),
                    attributes,
                    value,
                });
            };
            let attrs = |it: &mut dyn Iterator<Item = &opentelemetry::KeyValue>| {
                let mut v: Vec<(String, String)> = it
                    .map(|kv| (kv.key.as_str().to_owned(), kv.value.to_string()))
                    .collect();
                v.sort();
                v
            };
            match metric.data() {
                AggregatedMetrics::U64(MetricData::Sum(sum)) => {
                    for p in sum.data_points() {
                        push(attrs(&mut p.attributes()), p.value());
                    }
                }
                AggregatedMetrics::F64(MetricData::Histogram(h)) => {
                    for p in h.data_points() {
                        push(attrs(&mut p.attributes()), p.count());
                    }
                }
                AggregatedMetrics::U64(MetricData::Histogram(h)) => {
                    for p in h.data_points() {
                        push(attrs(&mut p.attributes()), p.count());
                    }
                }
                _ => {}
            }
        }
    }
    out
}

fn find<'a>(points: &'a [Point], name: &str, attrs: &[(&str, &str)]) -> Vec<&'a Point> {
    points
        .iter()
        .filter(|p| {
            p.name == name
                && attrs
                    .iter()
                    .all(|(k, v)| p.attributes.iter().any(|(pk, pv)| pk == k && pv == v))
        })
        .collect()
}

#[tokio::test]
async fn tool_calls_and_model_requests_become_rig_metrics() {
    let captured = capture(|| async {
        let model = MockCompletionModel::from_turns([
            MockTurn::tool_call("call-1", "add", json!({"a": 2, "b": 40}))
                .with_usage(tokens(30, 5)),
            MockTurn::tool_call("call-2", "fail", json!({})).with_usage(tokens(40, 5)),
            MockTurn::text("the sum is 42; the second tool failed").with_usage(tokens(50, 9)),
        ]);
        let agent = AgentBuilder::new(model)
            .name("calculator")
            .tool(Add)
            .tool(Fail)
            .default_max_turns(4)
            .build();
        agent
            .prompt("What is 2 + 40, and then fail?")
            .await
            .unwrap();
    })
    .await;
    let points = points(&captured);
    // The new metrics carry only the agent, tool and error-type attributes.
    for p in points.iter().filter(|p| p.name.starts_with("rig.")) {
        for (key, _) in &p.attributes {
            assert!(
                matches!(
                    key.as_str(),
                    "gen_ai.agent.name" | "gen_ai.tool.name" | "error.type"
                ),
                "{} carries {key}",
                p.name
            );
        }
    }

    let agent = [("gen_ai.agent.name", "calculator")];
    let add = [
        ("gen_ai.agent.name", "calculator"),
        ("gen_ai.tool.name", "add"),
    ];
    let fail = [
        ("gen_ai.agent.name", "calculator"),
        ("gen_ai.tool.name", "fail"),
    ];

    // Tool calls: one per execute_tool span, per tool.
    let calls = find(&points, "rig.tool.call_count", &add);
    assert_eq!(calls.len(), 1, "{points:#?}");
    assert_eq!((calls[0].unit.as_str(), calls[0].value), ("{call}", 1));
    let calls = find(&points, "rig.tool.call_count", &fail);
    assert_eq!((calls[0].unit.as_str(), calls[0].value), ("{call}", 1));

    // Successes and errors: the successful call only succeeds, the failed one only errors.
    let successes = find(&points, "rig.tool.success_count", &add);
    assert_eq!(
        (successes[0].unit.as_str(), successes[0].value),
        ("{call}", 1)
    );
    assert!(find(&points, "rig.tool.success_count", &fail).is_empty());
    let errors = find(&points, "rig.tool.error_count", &fail);
    assert_eq!(errors.len(), 1, "{points:#?}");
    assert_eq!((errors[0].unit.as_str(), errors[0].value), ("{call}", 1));
    assert!(
        errors[0]
            .attributes
            .iter()
            .any(|(k, v)| k == "error.type" && !v.is_empty()),
        "{:?}",
        errors[0]
    );
    assert!(find(&points, "rig.tool.error_count", &add).is_empty());

    // Tool duration: one recording per call; the failed one carries error.type.
    let durations = find(&points, "rig.tool.duration", &add);
    assert_eq!((durations[0].unit.as_str(), durations[0].value), ("s", 1));
    let durations = find(&points, "rig.tool.duration", &fail);
    assert_eq!((durations[0].unit.as_str(), durations[0].value), ("s", 1));
    assert!(
        durations[0]
            .attributes
            .iter()
            .any(|(k, _)| k == "error.type")
    );

    // Model requests: one per chat span, three here.
    let requests = find(&points, "rig.model.request_count", &agent);
    assert_eq!(requests.len(), 1, "{points:#?}");
    assert_eq!(
        (requests[0].unit.as_str(), requests[0].value),
        ("{request}", 3)
    );
    let request_durations = find(&points, "rig.model.request_duration", &agent);
    assert_eq!(
        (
            request_durations[0].unit.as_str(),
            request_durations[0].value
        ),
        ("s", 3)
    );

    // The existing metrics are still there, unchanged.
    assert!(!find(&points, "gen_ai.client.operation.duration", &[]).is_empty());
    assert!(!find(&points, "gen_ai.client.token.usage", &[]).is_empty());
}
