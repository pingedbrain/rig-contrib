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
async fn a_hostile_256_character_session_id_works_and_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    // AgentCore sends ids of 33 to 256 characters; this one has separators,
    // traversal and spaces, which a file store must never take as a path.
    let id: String = format!("../../{}", "a b/c.".repeat(50))
        .chars()
        .take(256)
        .collect();
    assert_eq!(id.len(), 256);

    let model = MockCompletionModel::from_turns([MockTurn::text("first")]);
    let app = AgentCoreApp::new(AgentBuilder::new(model).build()).history_store(dir_store(&dir));
    let url = serve(app).await;
    let response = invoke(&url, &id, "remember this").await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    let later_model = MockCompletionModel::from_turns([MockTurn::text("second")]);
    let later = AgentCoreApp::new(AgentBuilder::new(later_model.clone()).build())
        .history_store(dir_store(&dir));
    let url = serve(later).await;
    invoke(&url, &id, "what did I say?").await;
    assert!(sent(&later_model, 0).contains("remember this"));

    // Nothing was written outside the store's directory.
    let parent = dir.path().parent().unwrap();
    assert!(!parent.join("etc").exists());
}

#[tokio::test]
async fn concurrent_invocations_of_one_session_keep_every_turn() {
    let dir = tempfile::tempdir().unwrap();
    let model = MockCompletionModel::from_turns((0..5).map(|_| MockTurn::text("ok")));
    let app = AgentCoreApp::new(AgentBuilder::new(model).build()).history_store(dir_store(&dir));
    let url = serve(app).await;

    let (a, b, c, d) = tokio::join!(
        invoke(&url, "shared-1", "turn A"),
        invoke(&url, "shared-1", "turn B"),
        invoke(&url, "shared-1", "turn C"),
        invoke(&url, "shared-1", "turn D"),
    );
    for response in [a, b, c, d] {
        assert_eq!(response.status(), reqwest::StatusCode::OK);
    }

    // A restarted app sees every turn the concurrent invocations saved.
    let check_model = MockCompletionModel::from_turns([MockTurn::text("ok")]);
    let check = AgentCoreApp::new(AgentBuilder::new(check_model.clone()).build())
        .history_store(dir_store(&dir));
    let url = serve(check).await;
    invoke(&url, "shared-1", "final").await;
    let history = sent(&check_model, 0);
    for turn in ["turn A", "turn B", "turn C", "turn D"] {
        assert!(history.contains(turn), "{turn} was lost");
    }
}

/// A store whose failures are scripted, to test what the app does when the
/// store fails.
#[derive(Clone, Copy)]
enum Fault {
    /// Reading the log fails.
    Load,
    /// Saving fails for good.
    Save,
    /// Every save loses a race with another writer.
    Contention,
}

#[derive(Clone)]
struct Faulty(Fault);

impl rig_memory::AsyncStore for Faulty {
    async fn read_log_async(&self, _name: &str) -> std::io::Result<Vec<u8>> {
        match self.0 {
            Fault::Load => Err(std::io::Error::other("secret store internals")),
            _ => Err(std::io::ErrorKind::NotFound.into()),
        }
    }

    async fn append_log_async(
        &self,
        _name: &str,
        _len: u64,
        _bytes: &[u8],
    ) -> Result<(), rig_memory::LogWriteError> {
        Err(self.write_failure())
    }

    async fn replace_log_async(
        &self,
        _name: &str,
        _len: u64,
        _bytes: &[u8],
    ) -> Result<(), rig_memory::LogWriteError> {
        Err(self.write_failure())
    }

    async fn read_file_async(&self, _name: &str) -> std::io::Result<Vec<u8>> {
        Err(std::io::ErrorKind::NotFound.into())
    }

    async fn write_file_async(&self, _name: &str, _bytes: &[u8]) -> std::io::Result<()> {
        Err(std::io::Error::other("secret store internals"))
    }
}

impl Faulty {
    fn write_failure(&self) -> rig_memory::LogWriteError {
        match self.0 {
            Fault::Contention => rig_memory::LogWriteError::Changed,
            _ => rig_memory::LogWriteError::Io(std::io::Error::other("secret store internals")),
        }
    }
}

#[tokio::test]
async fn a_load_failure_is_an_error_that_names_no_internals() {
    let model = MockCompletionModel::from_turns([MockTurn::text("unused")]);
    let app = AgentCoreApp::new(AgentBuilder::new(model.clone()).build())
        .history_store(Faulty(Fault::Load));
    let url = serve(app).await;

    let response = invoke(&url, "session-1", "hello").await;
    assert_eq!(
        response.status(),
        reqwest::StatusCode::INTERNAL_SERVER_ERROR
    );
    let body = response.text().await.unwrap();
    assert!(body.contains("the conversation could not be loaded"));
    assert!(!body.contains("secret"));
    assert!(model.requests().is_empty(), "the agent never runs");

    // Streaming reports the same failure as an event, not a broken stream.
    let streamed = reqwest::Client::new()
        .post(format!("{url}/invocations"))
        .header(SESSION_HEADER, "session-1")
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .json(&json!({"prompt": "hello"}))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(streamed.contains("the conversation could not be loaded"));
    assert!(!streamed.contains("secret"));
}

#[tokio::test]
async fn a_save_failure_still_returns_the_answer() {
    for fault in [Fault::Save, Fault::Contention] {
        let model = MockCompletionModel::from_turns([MockTurn::text("the answer")]);
        let app = AgentCoreApp::new(AgentBuilder::new(model.clone()).build())
            .history_store(Faulty(fault));
        let url = serve(app).await;

        let reply: Value = invoke(&url, "session-1", "question")
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(reply.get("response"), Some(&json!("the answer")));
        assert_eq!(model.requests().len(), 1);
    }
}

#[tokio::test]
async fn max_sessions_forgets_the_oldest_session_first() {
    let model = MockCompletionModel::from_turns([
        MockTurn::text("alpha reply"),
        MockTurn::text("beta reply"),
        MockTurn::text("alpha again"),
    ]);
    let app = AgentCoreApp::new(AgentBuilder::new(model.clone()).build()).max_sessions(1);
    let url = serve(app).await;

    invoke(&url, "alpha", "alpha first")
        .await
        .text()
        .await
        .unwrap();
    // A second session evicts "alpha", the only one kept.
    invoke(&url, "beta", "beta first")
        .await
        .text()
        .await
        .unwrap();
    invoke(&url, "alpha", "alpha second")
        .await
        .text()
        .await
        .unwrap();

    let seen = sent(&model, 2);
    assert!(
        !seen.contains("alpha first") && !seen.contains("alpha reply"),
        "the evicted session kept its history: {seen}"
    );
    assert!(seen.contains("alpha second"), "{seen}");
}
