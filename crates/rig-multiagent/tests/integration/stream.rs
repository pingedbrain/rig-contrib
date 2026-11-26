//! Streamed swarm and graph runs, on scripted streaming models.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures::StreamExt;
use rig_agent::{Agent, AgentBuilder};
use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};
use rig_multiagent::{Graph, GraphEvent, HANDOFF_TOOL, NodeError, Swarm, SwarmEvent, node_fn};
use serde_json::json;

type Reply = std::pin::Pin<Box<dyn Future<Output = Result<String, NodeError>> + Send>>;

/// An agent whose model streams each turn's events, ending each turn.
fn streaming(name: &str, turns: Vec<Vec<MockStreamEvent>>) -> Agent {
    let model = MockCompletionModel::from_stream_turns(turns.into_iter().map(|mut turn| {
        turn.push(MockStreamEvent::final_response_with_default_usage());
        turn
    }));
    AgentBuilder::new(model)
        .name(name)
        .default_max_turns(5)
        .build()
}

fn text(chunk: &str) -> MockStreamEvent {
    MockStreamEvent::text(chunk)
}

/// A compact, comparable form of a swarm event.
fn swarm_line(event: &SwarmEvent) -> String {
    match event {
        SwarmEvent::AgentStarted { agent } => format!("start {agent}"),
        SwarmEvent::Text { agent, text } => format!("{agent}: {text}"),
        SwarmEvent::Handoff(handoff) => format!("handoff {} -> {}", handoff.from, handoff.to),
        SwarmEvent::Finished(outcome) => match outcome.as_ref() {
            Ok(run) => format!("done {}: {}", run.agent, run.answer),
            Err(error) => format!("failed: {error}"),
        },
        _ => "other".into(),
    }
}

fn graph_line(event: &GraphEvent) -> String {
    match event {
        GraphEvent::NodeStarted { node } => format!("start {node}"),
        GraphEvent::Text { node, text } => format!("{node}: {text}"),
        GraphEvent::NodeFinished { node, output } => format!("finish {node}: {output}"),
        GraphEvent::NodeFailed { node, error } => format!("fail {node}: {error}"),
        GraphEvent::Finished(outcome) => match outcome.as_ref() {
            Ok(run) => format!("done: {}", run.answer()),
            Err(error) => format!("failed: {error}"),
        },
        _ => "other".into(),
    }
}

#[tokio::test]
async fn a_streamed_swarm_reports_turns_text_and_handoffs() {
    let researcher = streaming(
        "researcher",
        vec![vec![
            text("Checking. "),
            MockStreamEvent::tool_call(
                "call-1",
                HANDOFF_TOOL,
                json!({"agent_name": "coder", "message": "Write it"}),
            ),
        ]],
    );
    let coder = streaming("coder", vec![vec![text("fn "), text("parse() {}")]]);
    let swarm = Swarm::builder()
        .agent(researcher)
        .agent(coder)
        .build()
        .expect("the builder is valid");

    let lines: Vec<String> = swarm
        .stream("Parse it")
        .map(|event| swarm_line(&event))
        .collect()
        .await;

    assert_eq!(
        lines,
        [
            "start researcher",
            "researcher: Checking. ",
            "handoff researcher -> coder",
            "start coder",
            "coder: fn ",
            "coder: parse() {}",
            "done coder: fn parse() {}",
        ]
    );
}

#[tokio::test]
async fn a_streamed_graph_reports_nodes_and_their_text() {
    let writer = streaming("writer", vec![vec![text("hello "), text("world")]]);
    let shout = node_fn(|input: String| -> Reply {
        Box::pin(async move { Ok(input.lines().last().unwrap_or_default().to_uppercase()) })
    });
    let graph = Graph::builder()
        .node("write", writer)
        .node("shout", shout)
        .edge("write", "shout")
        .build()
        .expect("the builder is valid");

    let lines: Vec<String> = graph
        .stream("Greet")
        .map(|event| graph_line(&event))
        .collect()
        .await;

    assert_eq!(
        lines,
        [
            "start write",
            "write: hello ",
            "write: world",
            "finish write: hello world",
            "start shout",
            "finish shout: HELLO WORLD",
            "done: HELLO WORLD",
        ]
    );
}

#[tokio::test]
async fn failures_stream_too_and_the_outcome_comes_last() {
    let broken = node_fn(|_: String| -> Reply { Box::pin(async { Err("disk full".into()) }) });
    let graph = Graph::builder()
        .node("broken", broken)
        .build()
        .expect("the builder is valid");

    let lines: Vec<String> = graph
        .stream("Task")
        .map(|event| graph_line(&event))
        .collect()
        .await;

    assert_eq!(
        lines,
        [
            "start broken",
            "fail broken: disk full",
            r#"failed: node "broken" failed: disk full"#,
        ]
    );
}

#[tokio::test]
async fn a_swarm_inside_a_graph_streams_its_text_under_the_node() {
    let planner = streaming(
        "planner",
        vec![vec![
            text("plan. "),
            MockStreamEvent::tool_call(
                "call-1",
                HANDOFF_TOOL,
                json!({"agent_name": "builder", "message": "Build"}),
            ),
        ]],
    );
    let builder = streaming("builder", vec![vec![text("built")]]);
    let swarm = Swarm::builder()
        .agent(planner)
        .agent(builder)
        .build()
        .expect("the builder is valid");
    let graph = Graph::builder()
        .node("team", swarm)
        .build()
        .expect("the builder is valid");

    let lines: Vec<String> = graph
        .stream("Make it")
        .map(|event| graph_line(&event))
        .collect()
        .await;

    assert_eq!(
        lines,
        [
            "start team",
            "team: plan. ",
            "team: built",
            "finish team: built",
            "done: built"
        ]
    );
}

#[tokio::test]
async fn dropping_the_stream_stops_the_run() {
    let ran = Arc::new(AtomicUsize::new(0));
    let counter = ran.clone();
    let slow = node_fn(move |_: String| -> Reply {
        let counter = counter.clone();
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            counter.fetch_add(1, Ordering::SeqCst);
            Ok("done".into())
        })
    });
    let graph = Graph::builder()
        .node("slow", slow)
        .build()
        .expect("the builder is valid");

    let mut events = graph.stream("Task");
    assert!(matches!(
        events.next().await,
        Some(GraphEvent::NodeStarted { .. })
    ));
    drop(events);
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(ran.load(Ordering::SeqCst), 0);
}
