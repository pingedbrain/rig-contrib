//! GenAI metrics read from Rig's spans: how long each model and tool call
//! took, and the tokens it used.

use std::time::Instant;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Histogram, Meter};
use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

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
pub(crate) struct GenAiMetrics {
    duration: Histogram<f64>,
    tokens: Histogram<u64>,
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
    tool: Option<String>,
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
            "gen_ai.tool.name" => self.tool = Some(value),
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
        let (Some(_), Some(started)) = (&operation.name, operation.started) else {
            return;
        };
        let attributes = operation.attributes();
        self.duration
            .record(started.elapsed().as_secs_f64(), &attributes);
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
    }
}
