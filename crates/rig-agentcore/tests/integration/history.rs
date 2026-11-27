//! Sessions kept in a rig-memory store: they survive a restart, sessions do
//! not mix, and the window limits what is sent while the store keeps all.

use rig_agent::AgentBuilder;
use rig_agentcore::{AgentCoreApp, SESSION_HEADER};
use rig_core::test_utils::{MockCompletionModel, MockTurn};
use rig_memory::DirStore;
use rig_memory::recent::{LastMessages, TokenBudget};
use serde_json::{Value, json};

async fn serve(app: AgentCoreApp) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app.router()).await.unwrap() });
    format!("http://{address}")
}

fn sent(model: &MockCompletionModel, request: usize) -> String {
    let requests = model.requests();
    serde_json::to_string(&requests.get(request).expect("request").chat_history).unwrap()
}

fn dir_store(dir: &tempfile::TempDir) -> DirStore {
    DirStore::open(dir.path()).unwrap()
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
async fn a_restarted_app_continues_the_session_on_the_same_store() {
    let dir = tempfile::tempdir().unwrap();

    let first_model = MockCompletionModel::from_turns([MockTurn::text("Hi Ana.")]);
    let first =
        AgentCoreApp::new(AgentBuilder::new(first_model).build()).history_store(dir_store(&dir));
    let url = serve(first).await;
    let _: Value = invoke(&url, "session-1", "I am Ana")
        .await
        .json()
        .await
        .unwrap();

    // The app restarts: a new process, the same directory.
    let second_model = MockCompletionModel::from_turns([MockTurn::text("Your name is Ana.")]);
    let second = AgentCoreApp::new(AgentBuilder::new(second_model.clone()).build())
        .history_store(dir_store(&dir));
    let url = serve(second).await;
    let reply: Value = invoke(&url, "session-1", "What is my name?")
        .await
        .json()
        .await
        .unwrap();

    assert_eq!(reply.get("response"), Some(&json!("Your name is Ana.")));
    assert!(
        sent(&second_model, 0).contains("I am Ana"),
        "the restarted app sends the session's earlier turn"
    );
}

#[tokio::test]
async fn concurrent_sessions_on_a_store_do_not_mix() {
    let dir = tempfile::tempdir().unwrap();
    let model = MockCompletionModel::from_turns((0..4).map(|_| MockTurn::text("ok")));
    let app =
        AgentCoreApp::new(AgentBuilder::new(model.clone()).build()).history_store(dir_store(&dir));
    let url = serve(app).await;

    for (session, prompt) in [
        ("alpha-1", "alpha one"),
        ("beta-1", "beta one"),
        ("alpha-1", "alpha two"),
        ("beta-1", "beta two"),
    ] {
        invoke(&url, session, prompt).await;
    }

    let alpha_second = sent(&model, 2);
    let beta_second = sent(&model, 3);
    assert!(alpha_second.contains("alpha one") && !alpha_second.contains("beta"));
    assert!(beta_second.contains("beta one") && !beta_second.contains("alpha"));
}

#[tokio::test]
async fn the_window_limits_what_is_sent_and_the_store_keeps_all() {
    let dir = tempfile::tempdir().unwrap();
    let model = MockCompletionModel::from_turns((0..3).map(|_| MockTurn::text("ok")));
    let app = AgentCoreApp::new(AgentBuilder::new(model.clone()).build())
        .history_store(dir_store(&dir))
        .history_window(LastMessages(2));
    let url = serve(app).await;
    for prompt in ["first question", "second question", "third question"] {
        invoke(&url, "session-1", prompt).await;
    }
    // Only the newest two messages (the second question and its reply) are sent.
    let third = sent(&model, 2);
    assert!(third.contains("second question"));
    assert!(!third.contains("first question"));

    // A restart with a wider window sees the whole conversation again: the
    // store was never trimmed to the window.
    let wide_model = MockCompletionModel::from_turns([MockTurn::text("ok")]);
    let wide = AgentCoreApp::new(AgentBuilder::new(wide_model.clone()).build())
        .history_store(dir_store(&dir))
        .history_window(TokenBudget::new(32_000));
    let url = serve(wide).await;
    invoke(&url, "session-1", "fourth question").await;
    assert!(sent(&wide_model, 0).contains("first question"));
}

#[tokio::test]
async fn a_session_id_a_file_store_could_not_name_is_refused_before_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let model = MockCompletionModel::from_turns([MockTurn::text("unused")]);
    let app =
        AgentCoreApp::new(AgentBuilder::new(model.clone()).build()).history_store(dir_store(&dir));
    let url = serve(app).await;

    let response = invoke(&url, "../escape", "hello").await;
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    assert!(model.requests().is_empty(), "the model is never called");
}
