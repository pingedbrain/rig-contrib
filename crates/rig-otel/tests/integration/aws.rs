//! CloudWatch export against a local server that accepts and never answers:
//! the exporter gives up within its configured timeout, and flushing and
//! shutting down do not hang.

use std::time::{Duration, Instant};

use aws_credential_types::Credentials;
use aws_credential_types::provider::SharedCredentialsProvider;
use rig_otel::Telemetry;
use tokio::net::TcpListener;

#[tokio::test(flavor = "multi_thread")]
async fn a_hung_cloudwatch_endpoint_fails_the_export_within_its_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    // Accept every connection and hold it open without answering.
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
        }
    });

    let credentials = Credentials::new("AKIDEXAMPLE", "dummy-secret", None, None, "test");
    let timeout = Duration::from_millis(300);
    let (telemetry, subscriber) = Telemetry::builder()
        .service_name("rig-otel-test")
        .aws_cloudwatch_at(
            "us-east-1",
            SharedCredentialsProvider::new(credentials),
            format!("http://{address}"),
            timeout,
        )
        .unwrap()
        .logs(false)
        .build()
        .unwrap();
    let guard = tracing::subscriber::set_default(subscriber);
    tracing::info_span!("hung export").in_scope(|| ());
    drop(guard);

    let started = Instant::now();
    let flushed = telemetry.force_flush();
    let flush_took = started.elapsed();
    assert!(flushed.is_err(), "flush against a hung endpoint must fail");
    assert!(
        flush_took < Duration::from_secs(2),
        "flush took {flush_took:?}"
    );

    let started = Instant::now();
    drop(telemetry);
    let shutdown_took = started.elapsed();
    assert!(
        shutdown_took < Duration::from_secs(2),
        "shutdown took {shutdown_took:?}"
    );
}
