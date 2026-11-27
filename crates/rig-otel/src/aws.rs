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
    /// The HTTP client for the endpoints could not be built.
    #[error("HTTP client for CloudWatch: {0}")]
    Client(#[source] reqwest::Error),
}

/// The OTel spec default for OTLP exporter timeouts.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
/// Bounds the TCP and TLS connect, so a black-holed host fails fast.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// The export timeout for one signal, as the plain OTLP exporters read it:
/// `signal_var` (`OTEL_EXPORTER_OTLP_TRACES_TIMEOUT` or
/// `OTEL_EXPORTER_OTLP_METRICS_TIMEOUT`), then `OTEL_EXPORTER_OTLP_TIMEOUT`,
/// in milliseconds, then 10 s.
fn export_timeout(signal_var: &str) -> Duration {
    [signal_var, "OTEL_EXPORTER_OTLP_TIMEOUT"]
        .into_iter()
        .find_map(|var| std::env::var(var).ok()?.trim().parse::<u64>().ok())
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

    /// Like [`aws_cloudwatch`](Self::aws_cloudwatch), with the region, the
    /// credentials, the base URL of the endpoints (`{base}/v1/traces` and
    /// `{base}/v1/metrics`) and one timeout for each request, given
    /// directly. Use it for a proxy or a private endpoint, or to test
    /// against a local server.
    ///
    /// The SigV4 service name comes from the first label of the host, so
    /// the base URL must start with `xray.` or `monitoring.` for AWS to
    /// accept the signature. Call it from inside a tokio runtime.
    pub fn aws_cloudwatch_at(
        self,
        region: impl Into<String>,
        credentials: SharedCredentialsProvider,
        base_url: impl Into<String>,
        timeout: Duration,
    ) -> Result<Self, InitError> {
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| InitError::Aws(AwsError::NoRuntime))?;
        let base_url = base_url.into();
        let spans_url = format!("{base_url}/v1/traces");
        let metrics_url = format!("{base_url}/v1/metrics");
        self.cloudwatch_exporters(
            runtime,
            region.into(),
            credentials,
            (spans_url, timeout),
            (metrics_url, timeout),
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
                .map_err(|error| InitError::Aws(AwsError::Client(error)))?;
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
