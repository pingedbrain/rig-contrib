//! Export to Amazon CloudWatch's OTLP endpoints, signed with AWS SigV4.
//!
//! CloudWatch takes OTLP over HTTP at `https://xray.REGION.amazonaws.com/v1/traces`
//! and `https://monitoring.REGION.amazonaws.com/v1/metrics`, each request
//! signed with SigV4 for its service (`xray`, `monitoring`). The credentials
//! come from the standard AWS chain: the IAM role of an AgentCore runtime, an
//! ECS task, an EC2 instance or a Lambda function, or a local profile.
//! Traces need CloudWatch Transaction Search enabled on the account.

use std::time::{Duration, SystemTime};

use aws_credential_types::provider::{ProvideCredentials, SharedCredentialsProvider};
use aws_sigv4::http_request::{SignableBody, SignableRequest, SigningSettings, sign};
use aws_sigv4::sign::v4;
use bytes::Bytes;
use opentelemetry_http::{HttpClient, HttpError, Request, Response};
use opentelemetry_otlp::{WithExportConfig, WithHttpConfig};

use crate::{InitError, TelemetryBuilder};

/// Why the CloudWatch exporters could not be set up.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AwsError {
    /// No region in the environment or the profile.
    #[error("no AWS region: set AWS_REGION or a profile region")]
    NoRegion,
    /// No credential provider in the environment.
    #[error("no AWS credentials provider")]
    NoCredentials,
    /// The tokio runtime the exporters sign requests on is missing.
    #[error("set up CloudWatch export from inside a tokio runtime")]
    NoRuntime,
    /// The HTTP client for the endpoints could not be built. The cause is
    /// logged with `tracing` at the failure.
    #[error("could not build the HTTP client for CloudWatch")]
    HttpClient,
}

/// The OTel spec default for OTLP exporter timeouts.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
/// Bounds the TCP and TLS connect, so a black-holed host fails fast.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// The export timeout for one signal, as the plain OTLP exporters read it:
/// `signal_var` (`OTEL_EXPORTER_OTLP_TRACES_TIMEOUT` or
/// `OTEL_EXPORTER_OTLP_METRICS_TIMEOUT`), then `OTEL_EXPORTER_OTLP_TIMEOUT`,
/// in milliseconds. Zero and unparsable values are ignored; then 10 s.
fn export_timeout(signal_var: &str) -> Duration {
    export_timeout_from(signal_var, |var| std::env::var(var).ok())
}

fn export_timeout_from(signal_var: &str, lookup: impl Fn(&str) -> Option<String>) -> Duration {
    [signal_var, "OTEL_EXPORTER_OTLP_TIMEOUT"]
        .into_iter()
        .find_map(|var| lookup(var)?.trim().parse::<u64>().ok().filter(|ms| *ms > 0))
        .map_or(DEFAULT_TIMEOUT, Duration::from_millis)
}

/// An OTLP HTTP client that signs each request with SigV4 for the service
/// its host names (`xray.…` → `xray`).
#[derive(Debug, Clone)]
struct SigV4Client {
    http: reqwest::Client,
    credentials: SharedCredentialsProvider,
    region: String,
    runtime: tokio::runtime::Handle,
}

impl SigV4Client {
    async fn sign_and_send(&self, request: Request<Bytes>) -> Result<Response<Bytes>, HttpError> {
        let (parts, body) = request.into_parts();
        let service = parts
            .uri
            .host()
            .and_then(|host| host.split('.').next())
            .unwrap_or("xray")
            .to_owned();
        let credentials = self.credentials.provide_credentials().await?;
        let identity = credentials.into();
        let params = v4::SigningParams::builder()
            .identity(&identity)
            .region(&self.region)
            .name(&service)
            .time(SystemTime::now())
            .settings(SigningSettings::default())
            .build()?
            .into();
        let uri = parts.uri.to_string();
        let headers: Vec<(&str, &str)> = parts
            .headers
            .iter()
            .filter_map(|(name, value)| Some((name.as_str(), value.to_str().ok()?)))
            .collect();
        let signable = SignableRequest::new(
            parts.method.as_str(),
            &uri,
            headers.into_iter(),
            SignableBody::Bytes(&body),
        )?;
        let (instructions, _signature) = sign(signable, &params)?.into_parts();
        let mut signed = http::Request::from_parts(parts, body);
        instructions.apply_to_request_http1x(&mut signed);

        let response = self.http.execute(signed.try_into()?).await?;
        let mut builder = http::Response::builder().status(response.status());
        for (name, value) in response.headers() {
            builder = builder.header(name, value);
        }
        Ok(builder.body(response.bytes().await?)?)
    }
}

#[async_trait::async_trait]
impl HttpClient for SigV4Client {
    async fn send_bytes(&self, request: Request<Bytes>) -> Result<Response<Bytes>, HttpError> {
        // The batch exporters call this from their own threads, outside any
        // tokio runtime, and the AWS credential providers need one.
        if tokio::runtime::Handle::try_current().is_ok() {
            self.sign_and_send(request).await
        } else {
            self.runtime.block_on(self.sign_and_send(request))
        }
    }
}

impl TelemetryBuilder {
    /// Export to Amazon CloudWatch's OTLP endpoints in the configured region,
    /// signed with SigV4 from the standard AWS credential chain. Call it from
    /// inside a tokio runtime.
    ///
    /// For AgentCore Observability, also set `OTEL_RESOURCE_ATTRIBUTES` to
    /// `service.name=…,aws.log.group.names=/aws/bedrock-agentcore/runtimes/RUNTIME_ID`,
    /// as AWS documents.
    pub async fn aws_cloudwatch(self) -> Result<Self, InitError> {
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| InitError::Aws(AwsError::NoRuntime))?;
        let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        let region = config
            .region()
            .ok_or(InitError::Aws(AwsError::NoRegion))?
            .to_string();
        let credentials = config
            .credentials_provider()
            .ok_or(InitError::Aws(AwsError::NoCredentials))?;
        let spans_url = format!("https://xray.{region}.amazonaws.com/v1/traces");
        let metrics_url = format!("https://monitoring.{region}.amazonaws.com/v1/metrics");
        self.cloudwatch_exporters(
            runtime,
            region,
            credentials,
            (
                spans_url,
                export_timeout("OTEL_EXPORTER_OTLP_TRACES_TIMEOUT"),
            ),
            (
                metrics_url,
                export_timeout("OTEL_EXPORTER_OTLP_METRICS_TIMEOUT"),
            ),
        )
    }

    fn cloudwatch_exporters(
        self,
        runtime: tokio::runtime::Handle,
        region: String,
        credentials: SharedCredentialsProvider,
        spans: (String, Duration),
        metrics: (String, Duration),
    ) -> Result<Self, InitError> {
        let client = |timeout: Duration| -> Result<SigV4Client, InitError> {
            let http = reqwest::Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .timeout(timeout)
                .build()
                .map_err(|error| {
                    tracing::error!(%error, "building the CloudWatch HTTP client failed");
                    InitError::Aws(AwsError::HttpClient)
                })?;
            Ok(SigV4Client {
                http,
                credentials: credentials.clone(),
                region: region.clone(),
                runtime: runtime.clone(),
            })
        };
        let spans_exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_http_client(client(spans.1)?)
            .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
            .with_endpoint(spans.0)
            .build()
            .map_err(InitError::SpanExporter)?;
        let metrics_exporter = opentelemetry_otlp::MetricExporter::builder()
            .with_http()
            .with_http_client(client(metrics.1)?)
            .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
            .with_endpoint(metrics.0)
            .build()
            .map_err(InitError::MetricExporter)?;
        Ok(self
            .span_exporter(spans_exporter)
            .metric_exporter(metrics_exporter))
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use aws_credential_types::Credentials;
    use aws_credential_types::provider::SharedCredentialsProvider;
    use tokio::net::TcpListener;

    use super::{DEFAULT_TIMEOUT, TelemetryBuilder, export_timeout_from};

    /// As `tests/integration`: a global default, so exporter-thread warnings
    /// do not depend on which test ran first.
    fn pin_global_default() {
        static PIN: std::sync::Once = std::sync::Once::new();
        PIN.call_once(|| {
            let _ = tracing::subscriber::set_global_default(
                tracing::subscriber::NoSubscriber::default(),
            );
        });
    }
    use crate::Telemetry;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |var| {
            pairs
                .iter()
                .find(|(name, _)| *name == var)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    #[test]
    fn the_signal_variable_beats_the_general_one() {
        let lookup = env(&[
            ("OTEL_EXPORTER_OTLP_TRACES_TIMEOUT", "1500"),
            ("OTEL_EXPORTER_OTLP_TIMEOUT", "3000"),
        ]);
        assert_eq!(
            export_timeout_from("OTEL_EXPORTER_OTLP_TRACES_TIMEOUT", &lookup),
            Duration::from_millis(1500)
        );
        assert_eq!(
            export_timeout_from("OTEL_EXPORTER_OTLP_METRICS_TIMEOUT", &lookup),
            Duration::from_millis(3000)
        );
    }

    #[test]
    fn zero_and_junk_fall_through_to_the_general_variable_then_the_default() {
        let lookup = env(&[
            ("OTEL_EXPORTER_OTLP_TRACES_TIMEOUT", "0"),
            ("OTEL_EXPORTER_OTLP_TIMEOUT", "0"),
        ]);
        assert_eq!(
            export_timeout_from("OTEL_EXPORTER_OTLP_TRACES_TIMEOUT", &lookup),
            DEFAULT_TIMEOUT
        );
        let lookup = env(&[
            ("OTEL_EXPORTER_OTLP_TRACES_TIMEOUT", "0"),
            ("OTEL_EXPORTER_OTLP_TIMEOUT", "2000"),
        ]);
        assert_eq!(
            export_timeout_from("OTEL_EXPORTER_OTLP_TRACES_TIMEOUT", &lookup),
            Duration::from_millis(2000)
        );
        let lookup = env(&[("OTEL_EXPORTER_OTLP_TRACES_TIMEOUT", "soon")]);
        assert_eq!(
            export_timeout_from("OTEL_EXPORTER_OTLP_TRACES_TIMEOUT", &lookup),
            DEFAULT_TIMEOUT
        );
        assert_eq!(
            export_timeout_from("OTEL_EXPORTER_OTLP_TRACES_TIMEOUT", env(&[])),
            DEFAULT_TIMEOUT
        );
    }

    /// Exporting to a server that accepts and never answers fails within the
    /// timeout, and flush and shutdown return.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_hung_cloudwatch_endpoint_fails_the_export_within_its_timeout() {
        pin_global_default();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let accept = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        });

        let credentials = Credentials::new("AKIDEXAMPLE", "dummy-secret", None, None, "test");
        let timeout = Duration::from_millis(300);
        let url = |path: &str| format!("{base}{path}");
        let (telemetry, subscriber): (Telemetry, _) = TelemetryBuilder::default()
            .service_name("rig-otel-test")
            .cloudwatch_exporters(
                tokio::runtime::Handle::current(),
                "us-east-1".to_owned(),
                SharedCredentialsProvider::new(credentials),
                (url("/v1/traces"), timeout),
                (url("/v1/metrics"), timeout),
            )
            .unwrap()
            .logs(false)
            .build()
            .unwrap();
        let guard = tracing::subscriber::set_default(subscriber);
        tracing::info_span!("hung export").in_scope(|| ());
        drop(guard);

        let outer = Duration::from_secs(10);
        let (telemetry, flushed, flush_took) = tokio::time::timeout(
            outer,
            tokio::task::spawn_blocking(move || {
                let started = Instant::now();
                let flushed = telemetry.force_flush();
                (telemetry, flushed, started.elapsed())
            }),
        )
        .await
        .expect("flush did not return within 10 s")
        .unwrap();
        assert!(flushed.is_err(), "flush against a hung endpoint must fail");
        assert!(
            flush_took < Duration::from_secs(2),
            "flush took {flush_took:?}"
        );

        let shutdown_took = tokio::time::timeout(
            outer,
            tokio::task::spawn_blocking(move || {
                let started = Instant::now();
                drop(telemetry);
                started.elapsed()
            }),
        )
        .await
        .expect("shutdown did not return within 10 s")
        .unwrap();
        assert!(
            shutdown_took < Duration::from_secs(2),
            "shutdown took {shutdown_took:?}"
        );
        accept.abort();
    }
}
