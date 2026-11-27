//! GenAI metrics read from Rig's spans: how long each model and tool call
//! took, the tokens it used, and the tool calls and model turns (Strands'
//! event-loop cycles) behind an agent run.

use std::time::Instant;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, Meter};
use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::{LookupSpan, SpanRef};

/// Bucket boundaries the GenAI semantic conventions advise for
/// `gen_ai.client.operation.duration`, in seconds.
const DURATION_BUCKETS: [f64; 14] = [
    0.01, 0.02, 0.04, 0.08, 0.16, 0.32, 0.64, 1.28, 2.56, 5.12, 10.24, 20.48, 40.96, 81.92,
];

/// Bucket boundaries the GenAI semantic conventions advise for
/// `gen_ai.client.token.usage`.
const TOKEN_BUCKETS: [f64; 14] = [
    1.0, 4.0, 16.0, 64.0, 256.0, 1024.0, 4096.0, 16384.0, 65536.0, 262144.0, 1048576.0, 4194304.0,
    16777216.0, 67108864.0,
];

/// Records `gen_ai.client.operation.duration` for every span with a
/// `gen_ai.operation.name` (Rig's `chat`, `execute_tool`, `invoke_agent`)
/// and `gen_ai.client.token.usage` for the ones that report tokens.
///
/// Also derives Strands Agents' tool and event-loop metrics from the same
/// spans: `execute_tool` spans give the tool counts, durations and errors,
/// and `chat` spans (one per model turn) give the event-loop cycles.
pub(crate) struct GenAiMetrics {
    duration: Histogram<f64>,
    tokens: Histogram<u64>,
    tool_calls: Counter<u64>,
    tool_successes: Counter<u64>,
    tool_errors: Counter<u64>,
    tool_duration: Histogram<f64>,
    cycles: Counter<u64>,
    cycle_duration: Histogram<f64>,
}

impl GenAiMetrics {
    pub(crate) fn new(meter: &Meter) -> Self {
        Self {
            duration: meter
                .f64_histogram("gen_ai.client.operation.duration")
                .with_unit("s")
                .with_description("GenAI operation duration")
                .with_boundaries(DURATION_BUCKETS.to_vec())
                .build(),
            tokens: meter
                .u64_histogram("gen_ai.client.token.usage")
                .with_unit("{token}")
                .with_description("Number of input and output tokens used")
                .with_boundaries(TOKEN_BUCKETS.to_vec())
                .build(),
            tool_calls: meter
                .u64_counter("strands.tool.call_count")
                .with_unit("{call}")
                .with_description("Tool calls, one per execute_tool span")
                .build(),
            tool_successes: meter
                .u64_counter("strands.tool.success_count")
                .with_unit("{call}")
                .with_description("Tool calls that returned a result")
                .build(),
            tool_errors: meter
                .u64_counter("strands.tool.error_count")
                .with_unit("{call}")
                .with_description("Tool calls that failed")
                .build(),
            tool_duration: meter
                .f64_histogram("strands.tool.duration")
                .with_unit("s")
                .with_description("Tool call duration")
                .with_boundaries(DURATION_BUCKETS.to_vec())
                .build(),
            cycles: meter
                .u64_counter("strands.event_loop.cycle_count")
                .with_unit("{cycle}")
                .with_description("Model turns (event-loop cycles) run by the agent")
                .build(),
            cycle_duration: meter
                .f64_histogram("strands.event_loop.cycle_duration")
                .with_unit("s")
                .with_description("Duration of each model turn (event-loop cycle)")
                .with_boundaries(DURATION_BUCKETS.to_vec())
                .build(),
        }
    }
}

/// What a GenAI span says about itself, as its fields are recorded.
#[derive(Default)]
struct Operation {
    started: Option<Instant>,
    name: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    agent: Option<String>,
    tool: Option<String>,
    /// `gen_ai.tool.call.outcome` of an `execute_tool` span: `success`,
    /// `error`, `denied` or `skipped`.
    outcome: Option<String>,
    /// `gen_ai.tool.error.type` of a failed tool call. Rig records a tool's
    /// failure under this name, not `error.type`.
    tool_error: Option<String>,
    error: Option<String>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

impl Operation {
    fn set(&mut self, field: &str, value: String) {
        match field {
            "gen_ai.operation.name" => self.name = Some(value),
            "gen_ai.provider.name" => self.provider = Some(value),
            "gen_ai.request.model" => self.model = Some(value),
            "gen_ai.agent.name" => self.agent = Some(value),
            "gen_ai.tool.name" => self.tool = Some(value),
            "gen_ai.tool.call.outcome" => self.outcome = Some(value),
            "gen_ai.tool.error.type" => self.tool_error = Some(value),
            "error.type" => self.error = Some(value),
            "gen_ai.usage.input_tokens" => self.input_tokens = value.parse().ok(),
            "gen_ai.usage.output_tokens" => self.output_tokens = value.parse().ok(),
            _ => {}
        }
    }

    fn attributes(&self) -> Vec<KeyValue> {
        let fields = [
            ("gen_ai.operation.name", &self.name),
            ("gen_ai.provider.name", &self.provider),
            ("gen_ai.request.model", &self.model),
            ("gen_ai.tool.name", &self.tool),
            ("error.type", &self.error),
        ];
        fields
            .into_iter()
            .filter_map(|(key, value)| {
                value
                    .as_ref()
                    .map(|value| KeyValue::new(key, value.clone()))
            })
            .collect()
    }
}

impl Visit for Operation {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.set(field.name(), value.to_owned());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.set(field.name(), value.to_string());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.set(field.name(), value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let text = format!("{value:?}");
        self.set(field.name(), text.trim_matches('"').to_owned());
    }
}

/// The agent a span runs for: its own `gen_ai.agent.name`, or the nearest
/// enclosing span that has one (a tool call inherits its agent this way).
fn enclosing_agent<R>(span: &SpanRef<'_, R>) -> Option<String>
where
    R: for<'a> LookupSpan<'a>,
{
    span.scope().skip(1).find_map(|ancestor| {
        ancestor
            .extensions()
            .get::<Operation>()
            .and_then(|operation| operation.agent.clone())
    })
}

impl<S> Layer<S> for GenAiMetrics
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut operation = Operation::default();
        attrs.record(&mut operation);
        if let Some(span) = ctx.span(id) {
            operation.started = Some(Instant::now());
            span.extensions_mut().insert(operation);
        }
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id)
            && let Some(operation) = span.extensions_mut().get_mut::<Operation>()
        {
            values.record(operation);
        }
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else {
            return;
        };
        let Some(operation) = span.extensions_mut().remove::<Operation>() else {
            return;
        };
        // Only GenAI operations: the spans of the app around them have no
        // operation name.
        let (Some(name), Some(started)) = (&operation.name, operation.started) else {
            return;
        };
        let elapsed = started.elapsed().as_secs_f64();
        let attributes = operation.attributes();
        self.duration.record(elapsed, &attributes);
        for (kind, count) in [
            ("input", operation.input_tokens),
            ("output", operation.output_tokens),
        ] {
            if let Some(count) = count {
                let mut attributes = attributes.clone();
                attributes.push(KeyValue::new("gen_ai.token.type", kind));
                self.tokens.record(count, &attributes);
            }
        }

        // Strands' attributes: the agent, the tool, and the error type on a
        // failure. Only these; the rest of the span stays out of the metrics.
        let mut labels = Vec::new();
        if let Some(agent) = operation.agent.clone().or_else(|| enclosing_agent(&span)) {
            labels.push(KeyValue::new("gen_ai.agent.name", agent));
        }
        match name.as_str() {
            "execute_tool" => {
                if let Some(tool) = &operation.tool {
                    labels.push(KeyValue::new("gen_ai.tool.name", tool.clone()));
                }
                self.tool_calls.add(1, &labels);
                let mut duration_labels = labels.clone();
                match operation.outcome.as_deref() {
                    Some("success") => self.tool_successes.add(1, &labels),
                    Some("error") => {
                        let kind = operation
                            .tool_error
                            .clone()
                            .unwrap_or_else(|| "error".to_owned());
                        let mut failure = labels.clone();
                        failure.push(KeyValue::new("error.type", kind.clone()));
                        self.tool_errors.add(1, &failure);
                        duration_labels.push(KeyValue::new("error.type", kind));
                    }
                    // Denied and skipped calls count as calls only.
                    _ => {}
                }
                self.tool_duration.record(elapsed, &duration_labels);
            }
            // One `chat` span per model turn: one Strands event-loop cycle.
            "chat" => {
                self.cycles.add(1, &labels);
                self.cycle_duration.record(elapsed, &labels);
            }
            _ => {}
        }
    }
}
