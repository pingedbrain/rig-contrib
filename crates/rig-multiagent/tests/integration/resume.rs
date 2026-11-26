//! Runs that stop (a failed step, a dropped stream standing in for a
//! restarted process) and resume from their checkpoints.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::StreamExt;
use rig_agent::{Agent, AgentBuilder};
use rig_core::test_utils::{MockCompletionModel, MockStreamEvent, MockTurn};
use rig_memory::{AsyncStore, DirStore, MemoryStore};
use rig_multiagent::{
    Graph, GraphEvent, GraphFailure, GraphState, HANDOFF_TOOL, Node, NodeError, Swarm, SwarmEvent,
    SwarmFailure, human, node_fn,
};
use serde_json::json;

type Reply = std::pin::Pin<Box<dyn Future<Output = Result<String, NodeError>> + Send>>;

fn agent(name: &str, turns: Vec<MockTurn>) -> (Agent, MockCompletionModel) {
    let model = MockCompletionModel::from_turns(turns);
    let agent = AgentBuilder::new(model.clone())
        .name(name)
        .default_max_turns(3)
        .build();
    (agent, model)
}

fn streaming(name: &str, turns: Vec<Vec<MockStreamEvent>>) -> (Agent, MockCompletionModel) {
    let model = MockCompletionModel::from_stream_turns(turns.into_iter().map(|mut turn| {
        turn.push(MockStreamEvent::final_response_with_default_usage());
        turn
    }));
    let agent = AgentBuilder::new(model.clone())
        .name(name)
        .default_max_turns(3)
        .build();
    (agent, model)
}

fn handoff(to: &str) -> serde_json::Value {
    json!({"agent_name": to, "message": "Your turn"})
}

/// A node answering `text` that counts its runs, failing the first `fails`.
fn counted(text: &'static str, fails: usize) -> (impl Node, Arc<AtomicUsize>) {
    let runs = Arc::new(AtomicUsize::new(0));
    let count = runs.clone();
    let node = node_fn(move |_: String| -> Reply {
        let run = count.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if run < fails {
                Err("flaky".into())
            } else {
                Ok(text.to_owned())
            }
        })
    });
    (node, runs)
}

#[tokio::test]
async fn a_swarm_resumes_with_the_turn_that_failed() {
    let store = MemoryStore::default();
    let (first, _) = agent(
        "first",
        vec![MockTurn::tool_call(
            "call-1",
            HANDOFF_TOOL,
            handoff("second"),
        )],
    );
    // Fails once, then answers; the first agent has no turn left to replay.
    let (second, _) = agent(
        "second",
        vec![MockTurn::error("503"), MockTurn::text("done")],
    );
    let swarm = Swarm::builder()
        .agent(first)
        .agent(second)
        .checkpoints(store)
        .build()
        .unwrap();

    let error = swarm.run_as("job-1", "Do it").await.unwrap_err();
    assert!(matches!(error.reason, SwarmFailure::Agent { ref agent, .. } if agent == "second"));

    let run = swarm.run_as("job-1", "ignored on resume").await.unwrap();
    assert_eq!(run.answer, "done");
    assert_eq!(run.agents, ["first", "second"]);
    assert_eq!(
        run.handoffs.len(),
        1,
        "the handoff from before the failure is kept"
    );
}

#[tokio::test]
async fn a_swarm_resumes_in_a_new_process() {
    let dir = tempfile::tempdir().unwrap();
    let (first, _) = streaming(
        "first",
        vec![vec![MockStreamEvent::tool_call(
            "call-1",
            HANDOFF_TOOL,
            handoff("second"),
        )]],
    );
    let (second, _) = streaming("second", vec![]);
    let swarm = Swarm::builder()
        .agent(first)
        .agent(second)
        .checkpoints(DirStore::open(dir.path()).unwrap())
        .build()
        .unwrap();
    // The process "dies" as the second agent starts.
    let mut events = swarm.stream_as("job-2", "Do it");
    while let Some(event) = events.next().await {
        if matches!(&event, SwarmEvent::AgentStarted { agent } if agent == "second") {
            break;
        }
    }
    drop(events);

    // A new process: new agents, same store.
    let (first, first_model) = streaming("first", vec![]);
    let (second, _) = streaming("second", vec![vec![MockStreamEvent::text("done")]]);
    let swarm = Swarm::builder()
        .agent(first)
        .agent(second)
        .checkpoints(DirStore::open(dir.path()).unwrap())
        .build()
        .unwrap();
    let events: Vec<SwarmEvent> = swarm.stream_as("job-2", "Do it").collect().await;

    assert!(
        matches!(&events.first().expect("item 0"), SwarmEvent::Resumed { agent, handoffs: 1 } if agent == "second"),
        "{events:?}"
    );
    let SwarmEvent::Finished(outcome) = events.last().unwrap() else {
        panic!("{events:?}")
    };
    let run = outcome.as_ref().as_ref().unwrap();
    assert_eq!(run.answer, "done");
    assert_eq!(
        first_model.request_count(),
        0,
        "the first agent's turn is not replayed"
    );
}

#[tokio::test]
async fn a_finished_run_id_starts_over() {
    let store = MemoryStore::default();
    let (solo, model) = agent("solo", vec![MockTurn::text("one"), MockTurn::text("two")]);
    let swarm = Swarm::builder()
        .agent(solo)
        .checkpoints(store.clone())
        .build()
        .unwrap();

    assert_eq!(swarm.run_as("job-3", "First").await.unwrap().answer, "one");
    let saved = store
        .read_file_async("rig-multiagent/swarm/job-3.json")
        .await
        .unwrap();
    let saved: serde_json::Value = serde_json::from_slice(&saved).unwrap();
    assert_eq!(saved.get("finished"), Some(&serde_json::Value::Bool(true)));

    assert_eq!(swarm.run_as("job-3", "Second").await.unwrap().answer, "two");
    assert!(
        serde_json::to_string(&model.requests().get(1).expect("item 1").chat_history)
            .unwrap()
            .contains("Second")
    );
}

#[tokio::test]
async fn a_bad_run_id_is_refused() {
    let (solo, _) = agent("solo", vec![]);
    let swarm = Swarm::builder()
        .agent(solo)
        .checkpoints(MemoryStore::default())
        .build()
        .unwrap();
    let error = swarm.run_as("../escape", "Task").await.unwrap_err();
    assert!(
        matches!(error.reason, SwarmFailure::Checkpoint { .. }),
        "{error}"
    );
}

#[tokio::test]
async fn a_graph_reruns_only_what_failed() {
    let store = MemoryStore::default();
    let (fetch, fetches) = counted("data", 0);
    let (parse, parses) = counted("rows", 1);
    let (report, reports) = counted("report", 0);
    let graph = Graph::builder()
        .node("fetch", fetch)
        .node("parse", parse)
        .node("report", report)
        .edge("fetch", "parse")
        .edge("parse", "report")
        .checkpoints(store)
        .build()
        .unwrap();

    let error = graph.run_as("job-4", "Task").await.unwrap_err();
    assert!(matches!(&error.failures[..], [GraphFailure::Node { node, .. }] if node == "parse"));

    let run = graph.run_as("job-4", "Task").await.unwrap();
    assert_eq!(run.answer(), "report");
    assert_eq!(
        (
            fetches.load(Ordering::SeqCst),
            parses.load(Ordering::SeqCst),
            reports.load(Ordering::SeqCst)
        ),
        (1, 2, 1)
    );
}

fn release(dir: &std::path::Path) -> (Graph, Arc<AtomicUsize>) {
    let (build, builds) = counted("release notes", 0);
    let approved = |state: &GraphState| state.output("approve") == Some("yes");
    let graph = Graph::builder()
        .node("build", build)
        .node("approve", human("Deploy?"))
        .node(
            "deploy",
            node_fn(|_: String| -> Reply { Box::pin(async { Ok("deployed".into()) }) }),
        )
        .edge("build", "approve")
        .edge_if("approve", "deploy", approved)
        .edge_if("build", "deploy", approved)
        .checkpoints(DirStore::open(dir).unwrap())
        .build()
        .unwrap();
    (graph, builds)
}

#[tokio::test]
async fn a_question_outlives_the_process_that_asked_it() {
    let dir = tempfile::tempdir().unwrap();

    // The process stops while the question waits for a person.
    let (graph, builds) = release(dir.path());
    let mut events = graph.stream_as("release-7", "Ship 1.2");
    while let Some(event) = events.next().await {
        if let GraphEvent::Question(question) = event {
            assert_eq!(question.question(), "Deploy?");
            break;
        }
    }
    drop(events);
    assert_eq!(builds.load(Ordering::SeqCst), 1);

    // Later, another process asks again and gets the answer.
    let (graph, builds) = release(dir.path());
    let mut events = graph.stream_as("release-7", "Ship 1.2");
    let mut resumed = None;
    let mut outcome = None;
    while let Some(event) = events.next().await {
        match event {
            GraphEvent::Resumed { done } => resumed = Some(done),
            GraphEvent::Question(question) => question.answer("yes"),
            GraphEvent::Finished(finished) => outcome = Some(*finished),
            _ => {}
        }
    }
    assert_eq!(resumed.unwrap(), ["build"]);
    assert_eq!(outcome.unwrap().unwrap().answer(), "deployed");
    assert_eq!(builds.load(Ordering::SeqCst), 0, "the build is not redone");
}
