//! Trace context and baggage across processes, in HTTP headers.

use opentelemetry::Context;
use opentelemetry::propagation::{TextMapCompositePropagator, TextMapPropagator};
use opentelemetry_http::{HeaderExtractor, HeaderInjector};
use opentelemetry_sdk::propagation::{BaggagePropagator, TraceContextPropagator};
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// W3C trace context and baggage: what every OpenTelemetry SDK reads.
fn propagator() -> TextMapCompositePropagator {
    TextMapCompositePropagator::new(vec![
        Box::new(TraceContextPropagator::new()),
        Box::new(BaggagePropagator::new()),
    ])
}

/// Write the current span's trace context (`traceparent`) and baggage (a
/// session, say) into `headers`, for an outgoing request: the other side
/// continues this trace.
pub fn inject(headers: &mut http::HeaderMap) {
    let cx = tracing::Span::current().context();
    propagator().inject_context(&cx, &mut HeaderInjector(headers));
}

/// Read the trace context and baggage of an incoming request, to continue
/// its trace: pass it to
/// [`OpenTelemetrySpanExt::set_parent`] or to [`set_session`](crate::set_session).
pub fn extract(headers: &http::HeaderMap) -> Context {
    propagator().extract(&HeaderExtractor(headers))
}
