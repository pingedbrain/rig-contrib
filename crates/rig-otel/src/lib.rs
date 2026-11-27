//! OpenTelemetry for [Rig](https://github.com/0xPlaygrounds/rig) agents in one
//! call.
//!
//! Rig already describes every agent run, model call and tool call as
//! `tracing` spans that follow the OpenTelemetry GenAI semantic conventions.
//! This crate sends them on, and adds what an observability backend needs
//! beyond the spans:
//!
//! - **Traces** over OTLP, to any collector (Jaeger, Grafana Tempo,
//!   Honeycomb, Langfuse, Phoenix, AWS CloudWatch), configured by the standard
//!   `OTEL_*` variables.
//! - **Metrics**: `gen_ai.client.operation.duration`,
//!   `gen_ai.client.token.usage` and tool and model-request metrics
//!   (`rig.tool.*`, `rig.model.*`), read from Rig's spans, so no agent code
//!   changes.
//! - **Sessions**: [`set_session`] gives a span and everything under it a
//!   `session.id`, the attribute AWS Bedrock AgentCore groups a conversation's
//!   traces by.
//! - **Traces across processes**: [`inject`] and [`extract`] carry the W3C
//!   `traceparent` and `baggage` headers, so an agent calling another over
//!   HTTP (A2A, MCP) makes one trace.
//!
//! Spans and metrics carry the instrumentation scope
//! `opentelemetry.instrumentation.rig`, the prefix AgentCore Observability and
//! Evaluations read spans from.
//!
//! ```no_run
//! # fn run() -> Result<(), rig_otel::InitError> {
//! // OTEL_EXPORTER_OTLP_ENDPOINT, OTEL_SERVICE_NAME, ... choose where and as what.
//! let _telemetry = rig_otel::init()?; // flushes when dropped, at the end of main
//! # Ok(())
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

#[cfg(feature = "aws")]
mod aws;
mod endpoint;
mod metrics;
mod propagation;
mod session;

use opentelemetry::InstrumentationScope;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkError;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::metrics::exporter::PushMetricExporter;
use opentelemetry_sdk::trace::{SdkTracerProvider, SpanExporter};
use tracing::Subscriber;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, Registry};

#[cfg(feature = "aws")]
pub use aws::AwsError;
pub use propagation::{extract, inject};
pub use session::{SESSION_ID, set_session};

/// The instrumentation scope of everything this crate exports. AWS Bedrock
/// AgentCore reads spans whose scope starts with
/// `opentelemetry.instrumentation.`.
pub const SCOPE: &str = "opentelemetry.instrumentation.rig";

/// Why telemetry could not be set up.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum InitError {
    /// The OTLP span exporter could not be built from the environment.
    #[cfg(feature = "otlp")]
    #[error("could not build the OTLP span exporter: {0}")]
    SpanExporter(#[source] opentelemetry_otlp::ExporterBuildError),
    /// The OTLP metric exporter could not be built from the environment.
    #[cfg(feature = "otlp")]
    #[error("could not build the OTLP metric exporter: {0}")]
    MetricExporter(#[source] opentelemetry_otlp::ExporterBuildError),
    /// Without the `otlp` feature, spans and metrics need exporters of your own.
    #[cfg(not(feature = "otlp"))]
    #[error("no exporter: enable the `otlp` feature or set span and metric exporters")]
    NoExporter,
    /// Export to Amazon CloudWatch could not be set up.
    #[cfg(feature = "aws")]
    #[error(transparent)]
    Aws(#[from] AwsError),
    /// Another global `tracing` subscriber was already set.
    #[error("a global tracing subscriber is already set: {0}")]
    Subscriber(#[source] tracing_subscriber::util::TryInitError),
}

/// Set up traces and metrics over OTLP from the standard `OTEL_*` variables,
/// as the process's global `tracing` subscriber. Logs go to stderr too when
/// `RUST_LOG` is set. The returned [`Telemetry`] flushes what is buffered when
/// dropped, so keep it until the end of `main`.
///
/// For more control (exporters of your own, a subscriber you compose), use
/// [`Telemetry::builder`].
pub fn init() -> Result<Telemetry, InitError> {
    Telemetry::builder().init()
}

/// The providers behind the exported traces and metrics. Dropping it flushes
/// and shuts them down.
#[derive(Debug)]
pub struct Telemetry {
    tracer_provider: SdkTracerProvider,
    meter_provider: SdkMeterProvider,
}

impl Telemetry {
    /// Start configuring telemetry.
    pub fn builder() -> TelemetryBuilder {
        TelemetryBuilder::default()
    }

    /// The trace provider, for spans of your own outside `tracing`.
    pub fn tracer_provider(&self) -> &SdkTracerProvider {
        &self.tracer_provider
    }

    /// The meter provider, for metrics of your own.
    pub fn meter_provider(&self) -> &SdkMeterProvider {
        &self.meter_provider
    }

    /// Export everything buffered now.
    pub fn force_flush(&self) -> Result<(), OTelSdkError> {
        self.tracer_provider.force_flush()?;
        self.meter_provider.force_flush()
    }
}

impl Drop for Telemetry {
    fn drop(&mut self) {
        // Nothing to tell anyone at this point: the process is ending.
        let _ = self.tracer_provider.shutdown();
        let _ = self.meter_provider.shutdown();
    }
}

/// Configures [`Telemetry`]. By default: OTLP over HTTP from the `OTEL_*`
/// variables, Rig's spans at `info`, no logs unless `RUST_LOG` is set.
#[must_use]
#[derive(Default)]
pub struct TelemetryBuilder {
    service_name: Option<String>,
    span_exporter: Option<
        Box<
            dyn FnOnce(
                opentelemetry_sdk::trace::TracerProviderBuilder,
            ) -> opentelemetry_sdk::trace::TracerProviderBuilder,
        >,
    >,
    metric_exporter: Option<
        Box<
            dyn FnOnce(
                opentelemetry_sdk::metrics::MeterProviderBuilder,
            ) -> opentelemetry_sdk::metrics::MeterProviderBuilder,
        >,
    >,
    filter: Option<String>,
    logs: Option<bool>,
}

impl TelemetryBuilder {
    /// The `service.name` the telemetry is attributed to. Defaults to
    /// `OTEL_SERVICE_NAME`, as OpenTelemetry does.
    pub fn service_name(mut self, name: impl Into<String>) -> Self {
        self.service_name = Some(name.into());
        self
    }

    /// Export spans with `exporter` instead of OTLP (an in-memory one in
    /// tests, a vendor's own).
    pub fn span_exporter(mut self, exporter: impl SpanExporter + 'static) -> Self {
        self.span_exporter = Some(Box::new(move |builder| {
            builder.with_batch_exporter(exporter)
        }));
        self
    }

    /// Export metrics with `exporter` instead of OTLP.
    pub fn metric_exporter(mut self, exporter: impl PushMetricExporter + 'static) -> Self {
        self.metric_exporter = Some(Box::new(move |builder| {
            builder.with_periodic_exporter(exporter)
        }));
        self
    }

    /// Which spans to export, as an `EnvFilter` directive. Defaults to
    /// `info`, where Rig's run, model and tool spans are.
    pub fn filter(mut self, directive: impl Into<String>) -> Self {
        self.filter = Some(directive.into());
        self
    }

    /// Whether to also log to stderr, at `RUST_LOG` (default `warn`).
    /// Defaults to whether `RUST_LOG` is set.
    pub fn logs(mut self, logs: bool) -> Self {
        self.logs = Some(logs);
        self
    }

    /// Build the providers and the `tracing` subscriber that feeds them,
    /// without installing anything globally: for tests, or to install the
    /// subscriber yourself.
    pub fn build(self) -> Result<(Telemetry, impl Subscriber + Send + Sync), InitError> {
        let (telemetry, subscriber, problems) = self.assemble()?;
        endpoint::report(&problems);
        Ok((telemetry, subscriber))
    }

    /// [`build`](Self::build) without reporting the endpoint problems yet:
    /// `init` reports them once its subscriber is installed.
    fn assemble(
        self,
    ) -> Result<(Telemetry, impl Subscriber + Send + Sync, Vec<String>), InitError> {
        #[cfg_attr(not(feature = "otlp"), allow(unused_mut))]
        let mut problems = Vec::new();
        let mut resource = Resource::builder();
        if let Some(name) = self.service_name {
            resource = resource.with_service_name(name);
        }
        let resource = resource.build();

        let mut traces = SdkTracerProvider::builder()
            .with_resource(resource.clone())
            .with_span_processor(session::SessionSpanProcessor);
        traces = match self.span_exporter {
            Some(add) => add(traces),
            #[cfg(feature = "otlp")]
            None => {
                let (url, problem) =
                    endpoint::resolve("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "v1/traces");
                problems.extend(problem);
                let exporter = opentelemetry_otlp::SpanExporter::builder()
                    .with_http()
                    .build()
                    .map_err(InitError::SpanExporter)?;
                traces.with_batch_exporter(endpoint::Spans::new(exporter, url))
            }
            #[cfg(not(feature = "otlp"))]
            None => return Err(InitError::NoExporter),
        };
        let tracer_provider = traces.build();

        let mut meters = SdkMeterProvider::builder().with_resource(resource);
        meters = match self.metric_exporter {
            Some(add) => add(meters),
            #[cfg(feature = "otlp")]
            None => {
                let (url, problem) =
                    endpoint::resolve("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT", "v1/metrics");
                problems.extend(problem);
                let exporter = opentelemetry_otlp::MetricExporter::builder()
                    .with_http()
                    .build()
                    .map_err(InitError::MetricExporter)?;
                meters.with_periodic_exporter(endpoint::Metrics::new(exporter, url))
            }
            #[cfg(not(feature = "otlp"))]
            None => return Err(InitError::NoExporter),
        };
        let meter_provider = meters.build();

        let scope = InstrumentationScope::builder(SCOPE)
            .with_version(env!("CARGO_PKG_VERSION"))
            .build();
        let tracer = tracer_provider.tracer_with_scope(scope.clone());
        let meter = opentelemetry::metrics::MeterProvider::meter_with_scope(&meter_provider, scope);

        let directive = self.filter.unwrap_or_else(|| "info".to_owned());
        let mut layers: Vec<Box<dyn Layer<Registry> + Send + Sync>> = vec![
            tracing_opentelemetry::layer()
                .with_tracer(tracer)
                .with_filter(EnvFilter::new(&directive))
                .boxed(),
            metrics::GenAiMetrics::new(&meter)
                .with_filter(EnvFilter::new(&directive))
                .boxed(),
        ];
        let logs = self
            .logs
            .unwrap_or_else(|| std::env::var_os("RUST_LOG").is_some());
        if logs {
            layers.push(
                tracing_subscriber::fmt::layer()
                    .with_writer(std::io::stderr)
                    .with_ansi(false)
                    .with_filter(
                        EnvFilter::try_from_default_env()
                            .unwrap_or_else(|_| EnvFilter::new("warn")),
                    )
                    .boxed(),
            );
        }
        // A Vec of layers rather than Options: a `None` layer reports an
        // "off" level hint that would switch the other layers off too.
        let subscriber = tracing_subscriber::registry().with(layers);
        Ok((
            Telemetry {
                tracer_provider,
                meter_provider,
            },
            subscriber,
            problems,
        ))
    }

    /// [`build`](Self::build), then install the subscriber and the providers
    /// globally.
    pub fn init(self) -> Result<Telemetry, InitError> {
        let (telemetry, subscriber, problems) = self.assemble()?;
        subscriber.try_init().map_err(InitError::Subscriber)?;
        endpoint::report(&problems);
        opentelemetry::global::set_tracer_provider(telemetry.tracer_provider.clone());
        opentelemetry::global::set_meter_provider(telemetry.meter_provider.clone());
        Ok(telemetry)
    }
}

#[cfg(doctest)]
#[doc = include_str!("../README.md")]
mod readme_doctests {}
