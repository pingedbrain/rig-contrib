use aws_config::{BehaviorVersion, Region, SdkConfig};
use aws_credential_types::Credentials;
use aws_credential_types::provider::SharedCredentialsProvider;
use aws_sdk_bedrockagentcore::types::EvaluationResultContent;
use aws_sdk_cloudwatchlogs::types::ResultField;
use aws_smithy_http_client::test_util::{ReplayEvent, StaticReplayClient};
use aws_smithy_types::body::SdkBody;
use serde_json::json;

use super::*;
use crate::{ExpectedCall, TurnFailed};

const ARN: &str = "arn:aws:bedrock-agentcore:us-east-1:123456789012:runtime/agent-AbC123xyz";

#[test]
fn the_runtime_id_is_the_last_arn_segment() {
    assert_eq!(runtime_id(ARN).unwrap(), "agent-AbC123xyz");
    assert_eq!(
        runtime_log_group(ARN, "DEFAULT").unwrap(),
        "/aws/bedrock-agentcore/runtimes/agent-AbC123xyz-DEFAULT"
    );
    for arn in [
        "my_agent",
        "arn:aws:bedrock-agentcore:us-east-1:1:runtime/",
        "arn:x:agent/a",
    ] {
        assert!(
            matches!(runtime_id(arn), Err(AgentCoreError::InvalidArn(_))),
            "{arn}"
        );
    }
}

#[test]
fn the_query_filters_on_the_session() {
    assert_eq!(
        spans_query("rig-evals-1"),
        "fields @timestamp, @message \
         | filter ispresent(scope.name) and ispresent(attributes.session.id) \
         | filter attributes.session.id = \"rig-evals-1\" \
         | sort @timestamp asc | limit 500"
    );
    assert!(spans_query(r#"a"b"#).contains(r#"= "a\"b""#));
}

fn field(name: &str, value: &str) -> ResultField {
    ResultField::builder().field(name).value(value).build()
}

#[test]
fn query_rows_become_the_json_messages() {
    let rows = vec![
        vec![
            field("@timestamp", "2026-11-26 10:00:00.000"),
            field(
                "@message",
                r#"{"name": "invoke_agent rig", "traceId": "t1"}"#,
            ),
        ],
        vec![field("@message", "plain text")],
        vec![field("@message", "{not json")],
        vec![field("@timestamp", "2026-11-26 10:00:01.000")],
    ];
    assert_eq!(
        documents(&rows),
        vec![json!({"name": "invoke_agent rig", "traceId": "t1"})]
    );
}

#[test]
fn spans_settle_once_an_invocation_arrived_and_the_count_held() {
    let spans = vec![json!({"name": "chat"}), json!({"name": "invoke_agent rig"})];
    assert!(!settled(None, &spans, 1));
    assert!(!settled(Some(1), &spans, 1));
    assert!(settled(Some(2), &spans, 1));
    assert!(!settled(Some(1), &[json!({"name": "chat"})], 1));
}

#[test]
fn a_multi_turn_run_settles_only_once_every_turn_has_its_invocation() {
    let one = vec![json!({"name": "invoke_agent rig"})];
    let two = vec![
        json!({"name": "invoke_agent rig"}),
        json!({"name": "invoke_agent rig"}),
    ];
    // Turn 2's span is still ingesting: the count is stable but short.
    assert!(!settled(Some(1), &one, 2));
    assert!(!settled(None, &two, 2));
    assert!(settled(Some(2), &two, 2));
}

#[tokio::test]
async fn a_multi_turn_case_runs_every_turn_in_one_session() {
    let case = Case::multi_turn("chat", ["one", "two", "three"]);
    let sessions = std::sync::Mutex::new(Vec::new());
    let output = run_turns(&case, "rig-evals-session", |session, prompt| {
        sessions.lock().expect("lock").push(session);
        async move { Ok(format!("answer to {prompt}")) }
    })
    .await
    .expect("runs");

    let sessions = sessions.into_inner().expect("lock");
    assert_eq!(sessions.len(), 3);
    assert!(
        sessions
            .iter()
            .all(|session| session == "rig-evals-session")
    );
    assert_eq!(output.session_id.as_deref(), Some("rig-evals-session"));
    assert_eq!(output.output, "answer to three");
    assert_eq!(output.transcript.len(), 3);
}

#[tokio::test]
async fn a_failed_turn_is_prefixed_and_keeps_the_turns_before_it() {
    let case = Case::multi_turn("chat", ["one", "two", "three"]);
    let error = run_turns(&case, "rig-evals-session", |_, prompt| async move {
        if prompt == "two" {
            Err(AgentCoreError::Reply("boom".to_owned()))
        } else {
            Ok("ok".to_owned())
        }
    })
    .await
    .expect_err("turn 2 fails");

    assert!(error.to_string().starts_with("turn 2: "), "{error}");
    let failed = error.downcast_ref::<TurnFailed>().expect("a turn failure");
    assert_eq!(failed.turn, 2);
    assert_eq!(failed.partial.transcript.len(), 1);
    assert_eq!(
        failed.partial.session_id.as_deref(),
        Some("rig-evals-session")
    );
}

#[test]
fn the_expected_answer_goes_to_the_last_invocation() {
    let spans = vec![
        json!({"name": "invoke_agent rig", "traceId": "t1"}),
        json!({"name": "execute_tool calculate", "traceId": "t2"}),
        json!({"name": "invoke_agent rig", "traceId": "t2"}),
        json!({"name": "chat", "traceId": "t3"}),
    ];
    assert_eq!(last_trace_id(&spans), Some("t2"));
    assert_eq!(last_trace_id(&[]), None);
}

fn span_context(reference: &EvaluationReferenceInput) -> &SpanContext {
    reference
        .context()
        .and_then(|context| context.as_span_context().ok())
        .expect("a span context")
}

#[test]
fn a_case_maps_to_session_and_trace_references() {
    let case = Case::new("mul", "987 * 654?")
        .expected("645498")
        .trajectory([ExpectedCall::new("calculate")])
        .assertion("uses the calculator");
    let references = reference_inputs(&case, "s1", Some("t1")).unwrap();
    let [session, trace] = references.as_slice() else {
        panic!("two references: {references:?}");
    };
    assert_eq!(span_context(session).session_id(), "s1");
    assert_eq!(span_context(session).trace_id(), None);
    assert_eq!(
        session.assertions(),
        [EvaluationContent::Text("uses the calculator".into())]
    );
    assert_eq!(
        session.expected_trajectory().map(|t| t.tool_names()),
        Some(&["calculate".to_owned()][..])
    );
    assert_eq!(session.expected_response(), None);
    assert_eq!(span_context(trace).session_id(), "s1");
    assert_eq!(span_context(trace).trace_id(), Some("t1"));
    assert_eq!(
        trace.expected_response(),
        Some(&EvaluationContent::Text("645498".into()))
    );
}

#[test]
fn a_case_sends_only_the_references_it_has() {
    let bare = Case::new("bare", "hi");
    assert!(
        reference_inputs(&bare, "s1", Some("t1"))
            .unwrap()
            .is_empty()
    );

    // An expected answer needs a trace to compare with.
    let expected = Case::new("e", "hi").expected("hello");
    assert!(reference_inputs(&expected, "s1", None).unwrap().is_empty());

    let asserted = Case::new("a", "hi").assertion("greets");
    let references = reference_inputs(&asserted, "s1", None).unwrap();
    let [session] = references.as_slice() else {
        panic!("one reference: {references:?}");
    };
    assert_eq!(session.expected_trajectory(), None);
}

fn result(value: Option<f64>, label: &str, explanation: &str) -> EvaluationResultContent {
    EvaluationResultContent::builder()
        .evaluator_arn("Builtin.Helpfulness")
        .evaluator_id("Builtin.Helpfulness")
        .evaluator_name("Helpfulness")
        .set_value(value)
        .label(label)
        .explanation(explanation)
        .build()
        .unwrap()
}

#[test]
fn results_score_their_mean_with_labels_and_explanations() {
    let results = [
        result(Some(0.83), "Very Helpful", "Clear."),
        result(Some(0.5), "Somewhat Helpful", "Terse."),
    ];
    let both = score("Builtin.Helpfulness", &results, 0.6).unwrap();
    assert_eq!(both.evaluator, "Builtin.Helpfulness");
    assert!((both.value - 0.665).abs() < 1e-9);
    assert!(both.passed);
    assert_eq!(
        both.label.as_deref(),
        Some("Very Helpful; Somewhat Helpful")
    );
    assert_eq!(both.explanation.as_deref(), Some("Clear.\n\nTerse."));

    let single = score(
        "Builtin.Helpfulness",
        &[result(Some(0.25), "Not Helpful", "Wrong.")],
        0.5,
    )
    .unwrap();
    assert!(!single.passed);
    assert_eq!(single.label.as_deref(), Some("Not Helpful"));
}

#[test]
fn an_error_result_or_no_value_is_an_error() {
    let failed = EvaluationResultContent::builder()
        .evaluator_arn("Builtin.Correctness")
        .evaluator_id("Builtin.Correctness")
        .evaluator_name("Correctness")
        .error_code("ValidationException")
        .error_message("no traces")
        .build()
        .unwrap();
    let error = score(
        "Builtin.Correctness",
        &[result(Some(1.0), "Yes", "Fine."), failed],
        0.5,
    )
    .unwrap_err();
    assert!(
        matches!(&error, AgentCoreError::Result { code, message, .. }
            if code == "ValidationException" && message == "no traces"),
        "{error}"
    );
    assert!(matches!(
        score("x", &[result(None, "Yes", "")], 0.5),
        Err(AgentCoreError::NoScore(_))
    ));
    assert!(matches!(
        score("x", &[], 0.5),
        Err(AgentCoreError::NoScore(_))
    ));
}

#[test]
fn the_reply_is_the_response_field() {
    assert_eq!(
        reply(br#"{"response": "645498", "status": "success"}"#).unwrap(),
        "645498"
    );
    assert!(matches!(
        reply(br#"{"error": "boom", "status": "error"}"#),
        Err(AgentCoreError::Reply(body)) if body.contains("boom")
    ));
}

#[test]
fn json_becomes_a_document() {
    let Document::Object(map) = document(&json!({"a": [1, -2, 1.5, "s", true, null]})) else {
        panic!("an object");
    };
    assert_eq!(
        map.get("a"),
        Some(&Document::Array(vec![
            Document::Number(Number::PosInt(1)),
            Document::Number(Number::NegInt(-2)),
            Document::Number(Number::Float(1.5)),
            Document::String("s".into()),
            Document::Bool(true),
            Document::Null,
        ]))
    );
}

#[tokio::test]
async fn a_trial_needs_a_session_and_its_runtime() {
    let config = aws_config::SdkConfig::builder()
        .behavior_version(aws_config::BehaviorVersion::latest())
        .build();
    let evaluator = AgentCore::new(&config).evaluator("Builtin.Correctness");
    let case = Case::new("c", "hi");
    let error = evaluator
        .evaluate(&case, &Trial::new("c", 1, "hello"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("no session id"), "{error}");

    let mut trial = Trial::new("c", 1, "hello");
    trial.session_id = Some("elsewhere".into());
    let error = evaluator.evaluate(&case, &trial).await.unwrap_err();
    assert!(error.to_string().contains("runtime_log_group"), "{error}");

    trial.error = Some("boom".into());
    let score = evaluator.evaluate(&case, &trial).await.unwrap();
    assert!(!score.passed);
}

/// AgentCore clients whose CloudWatch Logs answers are `answers` empty
/// queries, one per poll: the session's spans never arrive.
fn empty_logs(answers: usize) -> AgentCore {
    let events: Vec<ReplayEvent> = (0..answers)
        .flat_map(|_| {
            [
                ReplayEvent::new(
                    http::Request::post("https://logs.us-east-1.amazonaws.com/")
                        .header("x-amz-target", "Logs_20140328.StartQuery")
                        .body(SdkBody::empty())
                        .expect("a request"),
                    http::Response::builder()
                        .status(200)
                        .header("content-type", "application/x-amz-json-1.1")
                        .body(SdkBody::from(r#"{"queryId":"q"}"#))
                        .expect("a response"),
                ),
                ReplayEvent::new(
                    http::Request::post("https://logs.us-east-1.amazonaws.com/")
                        .header("x-amz-target", "Logs_20140328.GetQueryResults")
                        .body(SdkBody::empty())
                        .expect("a request"),
                    http::Response::builder()
                        .status(200)
                        .header("content-type", "application/x-amz-json-1.1")
                        .body(SdkBody::from(r#"{"status":"Complete","results":[]}"#))
                        .expect("a response"),
                ),
            ]
        })
        .collect();
    let config = SdkConfig::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("us-east-1"))
        .credentials_provider(SharedCredentialsProvider::new(Credentials::new(
            "AKIDEXAMPLE",
            "dummy-secret",
            None,
            None,
            "test",
        )))
        .http_client(StaticReplayClient::new(events))
        .build();
    AgentCore::new(&config)
}

#[tokio::test]
async fn spans_that_never_arrive_fail_after_the_ingestion_timeout() {
    let agentcore = empty_logs(500);
    let timeout = Duration::from_millis(50);

    let error = agentcore
        .spans(
            "session-1",
            "/aws/bedrock-agentcore/runtimes/agent-AbC123xyz-DEFAULT",
            UNIX_EPOCH,
            1,
            timeout,
            Duration::from_millis(5),
        )
        .await
        .unwrap_err();

    assert!(
        matches!(&error, AgentCoreError::NoSpans { session_id, waited }
            if session_id == "session-1" && *waited == timeout),
        "{error:?}"
    );
}

#[test]
fn the_evaluator_builder_keeps_its_timing_and_log_group() {
    let evaluator = empty_logs(0)
        .evaluator("Builtin.Correctness")
        .ingestion_timeout(Duration::from_secs(7))
        .poll_interval(Duration::from_millis(250))
        .runtime_log_group("/aws/bedrock-agentcore/runtimes/x-DEFAULT");

    assert_eq!(evaluator.ingestion_timeout, Duration::from_secs(7));
    assert_eq!(evaluator.poll_interval, Duration::from_millis(250));
    assert_eq!(
        evaluator.runtime_log_group.as_deref(),
        Some("/aws/bedrock-agentcore/runtimes/x-DEFAULT")
    );
}
