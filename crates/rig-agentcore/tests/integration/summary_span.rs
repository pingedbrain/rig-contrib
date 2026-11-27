//! The summarizer's `agentcore.summarize` span: one per attempt, under the
//! invocation span, with the counts, the outcome and the token usage. Prompt
//! and summary text are recorded only with `record_content`.

use rig_agent::AgentBuilder;
use rig_agentcore::{AgentCoreApp, SESSION_HEADER};
use rig_core::completion::Usage;
use rig_core::test_utils::{CapturedSpan, MockCompletionModel, MockTurn, TraceCapture};
use serde_json::json;

async fn serve(app: AgentCoreApp) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app.router()).await.unwrap() });
    format!("http://{address}")
}

async fn invoke(url: &str, session: &str, prompt: &str) {
    reqwest::Client::new()
        .post(format!("{url}/invocations"))
        .header(SESSION_HEADER, session)
        .json(&json!({"prompt": prompt}))
        .send()
        .await
        .unwrap();
}

fn summaries(capture: &TraceCapture) -> Vec<CapturedSpan> {
    capture
        .spans()
        .into_iter()
        .filter(|span| span.name == "agentcore.summarize")
        .collect()
}

/// Four invocations: the fourth one sees six messages past the trigger of
/// four, so it summarizes the first four and keeps two.
async fn four_invocations(
    main: &MockCompletionModel,
    summarizer: &MockCompletionModel,
    record: bool,
) -> String {
    let app = AgentCoreApp::new(AgentBuilder::new(main.clone()).build())
        .summarize_history(AgentBuilder::new(summarizer.clone()).build(), 2, 4)
        .record_content(record);
    let url = serve(app).await;
    for question in 1..=4 {
        invoke(&url, "session-1", &format!("question {question}")).await;
    }
    url
}

#[tokio::test(flavor = "current_thread")]
async fn a_summary_is_one_span_under_the_invocation_with_its_counts() {
    crate::pin_global_default();
    let capture = TraceCapture::default();
    let _guard = tracing::subscriber::set_default(capture.subscriber());
    let main =
        MockCompletionModel::from_turns((1..=4).map(|n| MockTurn::text(format!("answer {n}"))));
    let usage = Usage::new().input_tokens(120).output_tokens(30);
    let summarizer =
        MockCompletionModel::from_turns([MockTurn::text("SUMMARY-SPAN").with_usage(usage)]);
    four_invocations(&main, &summarizer, false).await;

    let found = summaries(&capture);
    assert_eq!(found.len(), 1, "one attempt, one span");
    let span = found.first().expect("one span");
    assert_eq!(span.parent_name, Some("agentcore.invocation"));
    assert_eq!(
        span.text("agentcore.session_id").as_deref(),
        Some("session-1")
    );
    assert_eq!(span.u64("agentcore.summary.messages_summarized"), Some(4));
    assert_eq!(span.u64("agentcore.summary.messages_kept"), Some(2));
    assert_eq!(
        span.text("agentcore.summary.outcome").as_deref(),
        Some("summarized")
    );
    assert_eq!(span.u64("gen_ai.usage.input_tokens"), Some(120));
    assert_eq!(span.u64("gen_ai.usage.output_tokens"), Some(30));
    assert!(
        capture
            .events()
            .iter()
            .any(|event| event.message() == "summarized the conversation"),
        "success is an info event"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_failed_summary_is_marked_failed_and_the_history_is_kept() {
    crate::pin_global_default();
    let capture = TraceCapture::default();
    let _guard = tracing::subscriber::set_default(capture.subscriber());
    let main =
        MockCompletionModel::from_turns((1..=4).map(|n| MockTurn::text(format!("answer {n}"))));
    let summarizer = MockCompletionModel::from_turns([MockTurn::error("summarizer down")]);
    four_invocations(&main, &summarizer, false).await;

    let found = summaries(&capture);
    assert_eq!(found.len(), 1);
    assert_eq!(
        found
            .first()
            .expect("one span")
            .text("agentcore.summary.outcome")
            .as_deref(),
        Some("failed")
    );
    assert_eq!(
        found
            .first()
            .expect("one span")
            .u64("agentcore.summary.messages_summarized"),
        Some(4)
    );
    assert!(
        capture
            .warnings()
            .iter()
            .any(|warning| warning.contains("could not summarize")),
        "the failure is still a warning"
    );
    // The fourth answer came from the full history: nothing was lost.
    let requests = main.requests();
    let last = serde_json::to_string(&requests.get(3).expect("request").chat_history).unwrap();
    assert!(last.contains("question 1") && last.contains("answer 2"));
}

#[tokio::test(flavor = "current_thread")]
async fn no_prompt_or_summary_is_recorded_without_record_content() {
    crate::pin_global_default();
    let capture = TraceCapture::default();
    let _guard = tracing::subscriber::set_default(capture.subscriber());
    let main =
        MockCompletionModel::from_turns((1..=4).map(|n| MockTurn::text(format!("answer {n}"))));
    let summarizer = MockCompletionModel::from_turns([MockTurn::text("SUMMARY-HIDDEN")]);
    four_invocations(&main, &summarizer, false).await;

    let found = summaries(&capture);
    assert_eq!(found.len(), 1);
    let span = found.first().expect("one span");
    assert!(span.value("agentcore.summary.prompt").is_none());
    assert!(span.value("agentcore.summary.text").is_none());
    let rendered = format!("{:?}", span.values());
    assert!(!rendered.contains("SUMMARY-HIDDEN") && !rendered.contains("question 1"));
    // The counts and outcome are still there.
    assert_eq!(
        span.text("agentcore.summary.outcome").as_deref(),
        Some("summarized")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn the_prompt_and_summary_are_recorded_with_record_content() {
    crate::pin_global_default();
    let capture = TraceCapture::default();
    let _guard = tracing::subscriber::set_default(capture.subscriber());
    let main =
        MockCompletionModel::from_turns((1..=4).map(|n| MockTurn::text(format!("answer {n}"))));
    let summarizer = MockCompletionModel::from_turns([MockTurn::text("SUMMARY-SHOWN")]);
    four_invocations(&main, &summarizer, true).await;

    let found = summaries(&capture);
    assert_eq!(found.len(), 1);
    let span = found.first().expect("one span");
    assert_eq!(
        span.text("agentcore.summary.text").as_deref(),
        Some("SUMMARY-SHOWN")
    );
    assert!(
        span.text("agentcore.summary.prompt")
            .is_some_and(|p| p.contains("question 1"))
    );
}
