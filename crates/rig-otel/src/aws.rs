//! Export to Amazon CloudWatch's OTLP endpoints, signed with AWS SigV4.
//!
//! CloudWatch takes OTLP over HTTP at `https://xray.REGION.amazonaws.com/v1/traces`
//! and `https://monitoring.REGION.amazonaws.com/v1/metrics`, each request
//! signed with SigV4 for its service (`xray`, `monitoring`). The credentials
//! come from the standard AWS chain: the IAM role of an AgentCore runtime, an
//! ECS task, an EC2 instance or a Lambda function, or a local profile.
//! Traces need CloudWatch Transaction Search enabled on the account.

use std::time::SystemTime;

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
        let client = SigV4Client {
            http: reqwest::Client::new(),
            credentials,
            region: region.clone(),
            runtime,
        };
        let spans = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_http_client(client.clone())
            .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
            .with_endpoint(format!("https://xray.{region}.amazonaws.com/v1/traces"))
            .build()
            .map_err(InitError::SpanExporter)?;
        let metrics = opentelemetry_otlp::MetricExporter::builder()
            .with_http()
            .with_http_client(client)
            .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
            .with_endpoint(format!(
                "https://monitoring.{region}.amazonaws.com/v1/metrics"
            ))
            .build()
            .map_err(InitError::MetricExporter)?;
        Ok(self.span_exporter(spans).metric_exporter(metrics))
    }
}
