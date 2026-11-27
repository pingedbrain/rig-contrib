//! The summarizing window: an older part of a session is replaced by one
//! summary, the newest messages stay verbatim, a tool call and its result are
//! never separated, and a failed summary keeps the full history.

use rig_agent::AgentBuilder;
use rig_agent::test_utils::MockAddTool;
use rig_agentcore::{AgentCoreApp, SESSION_HEADER};
use rig_core::test_utils::{MockCompletionModel, MockTurn};
use rig_memory::DirStore;
use rig_memory::recent::LastMessages;
use serde_json::{Value, json};

async fn serve(app: AgentCoreApp) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app.router()).await.unwrap() });
    format!("http://{address}")
}

/// The messages of request `request` of `model`, as JSON text.
fn sent(model: &MockCompletionModel, request: usize) -> String {
    let requests = model.requests();
    serde_json::to_string(&requests.get(request).expect("request").chat_history).unwrap()
}

/// The messages of the model's newest request.
fn last_sent(model: &MockCompletionModel) -> String {
    sent(model, model.request_count() - 1)
}

async fn invoke(url: &str, session: &str, prompt: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{url}/invocations"))
        .header(SESSION_HEADER, session)
        .json(&json!({"prompt": prompt}))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn the_summary_replaces_the_older_messages_once_past_the_trigger() {
    let main =
        MockCompletionModel::from_turns((1..=4).map(|n| MockTurn::text(format!("answer {n}"))));
    let summarizer = MockCompletionModel::from_turns([MockTurn::text("SUMMARY-ONE: Ana")]);
    let app = AgentCoreApp::new(AgentBuilder::new(main.clone()).build()).summarize_history(
        AgentBuilder::new(summarizer.clone()).build(),
        2,
        4,
    );
    let url = serve(app).await;

    // Four messages is the trigger, not past it: nothing is summarized yet.
    for question in 1..=3 {
        invoke(&url, "session-1", &format!("question {question}")).await;
    }
    assert_eq!(summarizer.request_count(), 0, "at the limit, no summary");

    // Six messages: the first four are summarized, the newest two kept.
    invoke(&url, "session-1", "question 4").await;
    assert_eq!(summarizer.request_count(), 1, "past the limit, one summary");

    let summarized = sent(&summarizer, 0);
    assert!(summarized.contains("question 1") && summarized.contains("answer 2"));
    assert!(
        !summarized.contains("question 3"),
        "the kept part is not summarized"
    );

    let request = sent(&main, 3);
    let summary = request.find("SUMMARY-ONE").expect("the summary is sent");
    let kept = request
        .find("question 3")
        .expect("the newest messages are sent");
    assert!(summary < kept, "the summary is the first message");
    assert!(request.contains("answer 3") && request.contains("question 4"));
    assert!(!request.contains("question 1") && !request.contains("question 2"));
    assert!(!request.contains("answer 1") && !request.contains("answer 2"));
}

#[tokio::test]
async fn a_failed_summary_keeps_the_full_history_and_still_answers() {
    let main =
        MockCompletionModel::from_turns((1..=4).map(|n| MockTurn::text(format!("answer {n}"))));
    // The first attempt errors, the second returns no text.
    let summarizer = MockCompletionModel::from_turns([
        MockTurn::error("summarizer down"),
        MockTurn::text("   "),
    ]);
    let app = AgentCoreApp::new(AgentBuilder::new(main.clone()).build()).summarize_history(
        AgentBuilder::new(summarizer.clone()).build(),
        1,
        2,
    );
    let url = serve(app).await;

    for question in 1..=2 {
        invoke(&url, "session-1", &format!("question {question}")).await;
    }
    // The third invocation tries a summary, which fails: the answer comes from
    // the full history, so nothing is lost.
    let third: Value = invoke(&url, "session-1", "question 3")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(third.get("response"), Some(&json!("answer 3")));
    let request = sent(&main, 2);
    assert!(request.contains("question 1") && request.contains("question 2"));

    // The fourth tries again (empty this time) and keeps the history too.
    let fourth: Value = invoke(&url, "session-1", "question 4")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(fourth.get("response"), Some(&json!("answer 4")));
    assert_eq!(summarizer.request_count(), 2, "each invocation tries once");
    assert!(sent(&main, 3).contains("question 1"), "no data was lost");
}

/// Run a session whose second turn calls the `add` tool, with a summary cut
/// that falls at `keep_recent` messages from the end. Returns the summarizer
/// and the main model, which has answered three invocations.
async fn tool_turn_then_summary(keep_recent: usize) -> (MockCompletionModel, MockCompletionModel) {
    let main = MockCompletionModel::from_turns([
        MockTurn::text("answer 1"),
        MockTurn::tool_call("call-1", "add", json!({"x": 2, "y": 3})),
        MockTurn::text("answer 2"),
        MockTurn::text("answer 3"),
    ]);
    let summarizer = MockCompletionModel::from_turns([MockTurn::text("SUMMARY-TOOLS")]);
    let app = AgentCoreApp::new(
        AgentBuilder::new(main.clone())
            .tool(MockAddTool)
            .default_max_turns(3)
            .build(),
    )
    .summarize_history(
        AgentBuilder::new(summarizer.clone()).build(),
        keep_recent,
        4,
    );
    let url = serve(app).await;
    for question in 1..=3 {
        invoke(&url, "session-1", &format!("question {question}")).await;
    }
    (main, summarizer)
}

#[tokio::test]
async fn a_summary_never_separates_a_tool_call_from_its_result() {
    // Two kept messages would start at the result, which is cut off with its
    // call: the cut moves past the pair, so both are summarized together.
    let (main, summarizer) = tool_turn_then_summary(2).await;
    let summarized = sent(&summarizer, 0);
    assert!(
        summarized.contains("add(x=2, y=3)"),
        "the call is summarized"
    );
    assert!(summarized.contains("[Tool result]: 5"), "and its result");
    let request = last_sent(&main);
    assert!(!request.contains("call-1"), "neither half is kept alone");
    assert!(!request.contains("Tool result"));

    // Three kept messages would start at the call itself: the pair is kept
    // whole, its call and its result both sent.
    let (main, summarizer) = tool_turn_then_summary(3).await;
    let summarized = sent(&summarizer, 0);
    assert!(
        !summarized.contains("add(x=2, y=3)"),
        "the pair is not summarized"
    );
    let request = last_sent(&main);
    assert_eq!(
        request.matches("call-1").count(),
        2,
        "the call and its result"
    );
    assert!(request.contains("SUMMARY-TOOLS"));
}

#[tokio::test]
async fn a_summary_survives_a_restart_with_a_store() {
    let dir = tempfile::tempdir().unwrap();
    let main =
        MockCompletionModel::from_turns((1..=3).map(|n| MockTurn::text(format!("answer {n}"))));
    let summarizer = MockCompletionModel::from_turns([MockTurn::text("SUMMARY-STORED")]);
    let app = AgentCoreApp::new(AgentBuilder::new(main).build())
        .history_store(DirStore::open(dir.path()).unwrap())
        .summarize_history(AgentBuilder::new(summarizer.clone()).build(), 1, 2);
    let url = serve(app).await;
    for question in 1..=3 {
        invoke(&url, "session-1", &format!("question {question}")).await;
    }
    assert_eq!(summarizer.request_count(), 1);

    // The app restarts without a summarizer: the store holds the summary in
    // place of the messages it replaced.
    let later_model = MockCompletionModel::from_turns([MockTurn::text("ok")]);
    let later = AgentCoreApp::new(AgentBuilder::new(later_model.clone()).build())
        .history_store(DirStore::open(dir.path()).unwrap());
    let url = serve(later).await;
    invoke(&url, "session-1", "question 4").await;
    let request = last_sent(&later_model);
    assert!(request.contains("SUMMARY-STORED"));
    assert!(!request.contains("question 1"));
}

#[tokio::test]
async fn the_summary_is_sent_first_even_when_the_window_is_narrower() {
    let main =
        MockCompletionModel::from_turns((1..=4).map(|n| MockTurn::text(format!("answer {n}"))));
    let summarizer = MockCompletionModel::from_turns([MockTurn::text("SUMMARY-NARROW")]);
    let app = AgentCoreApp::new(AgentBuilder::new(main.clone()).build())
        .summarize_history(AgentBuilder::new(summarizer.clone()).build(), 2, 4)
        .history_window(LastMessages(1));
    let url = serve(app).await;
    for question in 1..=4 {
        invoke(&url, "session-1", &format!("question {question}")).await;
    }
    assert_eq!(summarizer.request_count(), 1);

    // The window keeps one message, the newest answer; the summary is sent
    // before it anyway.
    let request = sent(&main, 3);
    let summary = request.find("SUMMARY-NARROW").expect("the summary is sent");
    let answer = request
        .find("answer 3")
        .expect("the newest kept message is sent");
    assert!(summary < answer, "the summary comes first");
    assert!(!request.contains("question 3"));
}

#[tokio::test]
async fn a_failing_summarizer_does_not_grow_the_session_without_bound() {
    let main =
        MockCompletionModel::from_turns((1..=8).map(|n| MockTurn::text(format!("answer {n}"))));
    let summarizer =
        MockCompletionModel::from_turns((0..8).map(|_| MockTurn::error("summarizer down")));
    let app = AgentCoreApp::new(AgentBuilder::new(main.clone()).build())
        .summarize_history(AgentBuilder::new(summarizer.clone()).build(), 1, 2)
        .history_window(LastMessages(1));
    let url = serve(app).await;
    for question in 1..=8 {
        let reply: Value = invoke(&url, "session-1", &format!("question {question}"))
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(
            reply.get("response"),
            Some(&json!(format!("answer {question}")))
        );
    }

    // The window sends one message and the prompt. The session keeps its newest
    // 2 * trigger messages (4), so the last summary attempt (the sixth) read
    // only the older two of them and the one kept before the newest.
    assert_eq!(sent(&main, 7).matches("\"role\"").count(), 2);
    let attempted = sent(&summarizer, 5);
    assert!(attempted.contains("question 6") && attempted.contains("question 7"));
    assert!(!attempted.contains("question 5") && !attempted.contains("question 1"));
}
