//! OTLP endpoints: a value that is not an http(s) URL is reported at setup,
//! and failed exports name the endpoint and signal they were for.

#![cfg_attr(not(feature = "otlp"), allow(dead_code))]

use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::metrics::Temporality;
use opentelemetry_sdk::metrics::data::ResourceMetrics;
use opentelemetry_sdk::metrics::exporter::PushMetricExporter;
use opentelemetry_sdk::trace::{SpanData, SpanExporter};

const BASE_VAR: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";
const DEFAULT_BASE: &str = "http://localhost:4318";

/// At most one failure warning per exporter in this window. A dead collector
/// would otherwise log every batch. The warning counts the failures it skipped.
// ponytail: fixed window, no backoff; a dead collector warns once a minute.
const REPORT_EVERY: Duration = Duration::from_secs(60);

/// The URL the OTLP/HTTP exporter for `signal_var` sends to, and a problem
/// with the variable that set it. `path` is the signal's default path, used
/// when only the base variable is set.
pub(crate) fn resolve(signal_var: &str, path: &str) -> (String, Option<String>) {
    let set = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
    let (name, value, is_base) = if let Some(value) = set(signal_var) {
        (signal_var, value, false)
    } else if let Some(value) = set(BASE_VAR) {
        (BASE_VAR, value, true)
    } else {
        return (format!("{DEFAULT_BASE}/{path}"), None);
    };
    let problem = (!is_http_url(&value)).then(|| {
        format!("{name}={value} is not an http or https URL with a host, so OTLP export cannot reach a collector")
    });
    let url = if is_base {
        format!("{}/{path}", value.trim_end_matches('/'))
    } else {
        value
    };
    (url, problem)
}

fn is_http_url(value: &str) -> bool {
    value.parse::<http::Uri>().is_ok_and(|uri| {
        matches!(uri.scheme_str(), Some("http" | "https"))
            && uri.host().is_some_and(|h| !h.is_empty())
    })
}

/// Emit each distinct problem once, as a `warn` event.
pub(crate) fn report(problems: &[String]) {
    let mut seen: Vec<&str> = Vec::new();
    for problem in problems {
        if !seen.contains(&problem.as_str()) {
            seen.push(problem);
            tracing::warn!("{problem}");
        }
    }
}

/// Logs failed exports for one signal and endpoint, at most once per
/// `REPORT_EVERY`.
#[derive(Debug)]
pub(crate) struct Reporter {
    signal: &'static str,
    endpoint: String,
    throttle: Mutex<Throttle>,
}

#[derive(Debug, Default)]
struct Throttle {
    reported_at: Option<Instant>,
    skipped: u64,
}

impl Reporter {
    fn new(signal: &'static str, endpoint: String) -> Self {
        Self {
            signal,
            endpoint,
            throttle: Mutex::new(Throttle::default()),
        }
    }

    fn check(&self, result: OTelSdkResult) -> OTelSdkResult {
        if let Err(err) = &result {
            // Decide under the lock, log after it is released.
            let report = {
                let mut throttle = self.throttle.lock().unwrap_or_else(PoisonError::into_inner);
                let now = Instant::now();
                if throttle
                    .reported_at
                    .is_none_or(|at| now.duration_since(at) >= REPORT_EVERY)
                {
                    throttle.reported_at = Some(now);
                    Some(std::mem::take(&mut throttle.skipped))
                } else {
                    throttle.skipped += 1;
                    None
                }
            };
            if let Some(skipped) = report {
                let more = if skipped > 0 {
                    format!(" ({skipped} more since the last warning)")
                } else {
                    String::new()
                };
                tracing::warn!(
                    "OTLP {} export to {} failed: {err}{more}",
                    self.signal,
                    self.endpoint
                );
            }
        }
        result
    }
}

/// A span exporter that reports its failures.
#[derive(Debug)]
pub(crate) struct Spans<E> {
    inner: E,
    reporter: Reporter,
}

impl<E> Spans<E> {
    pub(crate) fn new(inner: E, endpoint: String) -> Self {
        Self {
            inner,
            reporter: Reporter::new("traces", endpoint),
        }
    }
}

impl<E: SpanExporter> SpanExporter for Spans<E> {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        self.reporter.check(self.inner.export(batch).await)
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn shutdown(&self) -> OTelSdkResult {
        self.inner.shutdown()
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
}

/// A metric exporter that reports its failures.
#[derive(Debug)]
pub(crate) struct Metrics<E> {
    inner: E,
    reporter: Reporter,
}

impl<E> Metrics<E> {
    pub(crate) fn new(inner: E, endpoint: String) -> Self {
        Self {
            inner,
            reporter: Reporter::new("metrics", endpoint),
        }
    }
}

impl<E: PushMetricExporter> PushMetricExporter for Metrics<E> {
    async fn export(&self, metrics: &ResourceMetrics) -> OTelSdkResult {
        self.reporter.check(self.inner.export(metrics).await)
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn shutdown(&self) -> OTelSdkResult {
        self.inner.shutdown()
    }

    fn temporality(&self) -> Temporality {
        self.inner.temporality()
    }
}
