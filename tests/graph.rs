//! Graphs of functions, scripted agents and swarms: no keys, no network.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rig_agent::AgentBuilder;
use rig_core::completion::Usage;
use rig_core::test_utils::{MockCompletionModel, MockTurn};
use rig_multiagent::{
    BuildError, FnNode, Graph, GraphFailure, GraphState, NodeError, Swarm, node_fn,
};

type Reply = std::pin::Pin<Box<dyn Future<Output = Result<String, NodeError>> + Send>>;

type Inputs = Arc<Mutex<Vec<String>>>;

/// A node that answers `text` and records every input it gets.
fn recording(text: &'static str) -> (FnNode<impl Fn(String) -> Reply + Send + Sync>, Inputs) {
    let inputs = Arc::new(Mutex::new(Vec::new()));
    let seen = inputs.clone();
    let node = node_fn(move |input: String| -> Reply {
        seen.lock().unwrap().push(input);
        Box::pin(async move { Ok(text.to_owned()) })
    });
    (node, inputs)
}

fn sleeping(text: &'static str, millis: u64) -> FnNode<impl Fn(String) -> Reply + Send + Sync> {
    node_fn(move |_: String| -> Reply {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(millis)).await;
            Ok(text.to_owned())
        })
    })
}

fn tokens(input: u64, output: u64) -> Usage {
    let mut usage = Usage::default();
    usage.input_tokens = Some(input);
    usage.output_tokens = Some(output);
    usage.total_tokens = Some(input + output);
    usage
}

#[tokio::test]
async fn an_agent_feeds_a_function() -> anyhow::Result<()> {
    let model =
        MockCompletionModel::from_turns([MockTurn::text("three facts").with_usage(tokens(8, 3))]);
    let research = AgentBuilder::new(model.clone()).name("research").build();
    let (shout, inputs) = recording("THREE FACTS");
    let graph = Graph::builder()
        .node("research", research)
        .node("shout", shout)
        .edge("research", "shout")
        .build()?;

    let run = graph.run("Find facts").await?;

    assert_eq!(run.answer(), "THREE FACTS");
    assert_eq!(run.output("research"), Some("three facts"));
    assert_eq!(run.state.order(), ["research", "shout"]);
    assert_eq!(run.usage.input_tokens, Some(8));
    assert_eq!(
        inputs.lock().unwrap().as_slice(),
        ["Original task: Find facts\n\nInputs from previous nodes:\n\nFrom research:\nthree facts"]
    );
    let sent = serde_json::to_string(&model.requests()[0].chat_history)?;
    assert!(
        sent.contains(r#""text":"Find facts""#),
        "the entry node gets the task: {sent}"
    );
    Ok(())
}

#[tokio::test]
async fn branches_run_in_parallel_and_join_once() -> anyhow::Result<()> {
    let (join, inputs) = recording("joined");
    let graph = Graph::builder()
        .node("split", sleeping("split", 0))
        .node("left", sleeping("from left", 300))
        .node("right", sleeping("from right", 300))
        .node("join", join)
        .edge("split", "left")
        .edge("split", "right")
        .edge("left", "join")
        .edge("right", "join")
        .build()?;

    let started = Instant::now();
    let run = graph.run("Task").await?;

    assert!(
        started.elapsed() < Duration::from_millis(550),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(run.state.order(), ["split", "left", "right", "join"]);
    let inputs = inputs.lock().unwrap();
    assert_eq!(inputs.len(), 1);
    assert!(
        inputs[0].contains("From left:\nfrom left")
            && inputs[0].contains("From right:\nfrom right")
    );
    Ok(())
}

#[tokio::test]
async fn conditional_edges_pick_the_branch() -> anyhow::Result<()> {
    let graph = Graph::builder()
        .node("triage", sleeping("bug", 0))
        .node("fix", sleeping("patched", 0))
        .node("docs", sleeping("documented", 0))
        .edge_if("triage", "fix", |state: &GraphState| {
            state.output("triage") == Some("bug")
        })
        .edge_if("triage", "docs", |state: &GraphState| {
            state.output("triage") != Some("bug")
        })
        .build()?;

    let run = graph.run("Task").await?;

    assert_eq!(run.answer(), "patched");
    assert_eq!(run.state.runs("docs"), 0);
    Ok(())
}

#[tokio::test]
async fn a_review_loop_runs_until_approved() -> anyhow::Result<()> {
    let drafts = Arc::new(AtomicUsize::new(0));
    let count = drafts.clone();
    let writer = node_fn(move |_: String| -> Reply {
        let draft = count.fetch_add(1, Ordering::SeqCst) + 1;
        Box::pin(async move { Ok(format!("draft {draft}")) })
    });
    let reviewer = node_fn(|input: String| -> Reply {
        Box::pin(async move {
            Ok(if input.contains("draft 2") {
                "APPROVED"
            } else {
                "REVISE"
            }
            .to_owned())
        })
    });
    let needs_work = |state: &GraphState| state.output("review") == Some("REVISE");
    let unbounded = Graph::builder()
        .node("write", sleeping("", 0))
        .node("review", sleeping("", 0))
        .edge("write", "review")
        .edge_if("review", "write", needs_work)
        .entry("write")
        .build();
    assert_eq!(unbounded.err(), Some(BuildError::UnboundedCycle));

    let graph = Graph::builder()
        .node("write", writer)
        .node("review", reviewer)
        .edge("write", "review")
        .edge_if("review", "write", needs_work)
        .entry("write")
        .max_node_executions(10)
        .build()?;

    let run = graph.run("Write a haiku").await?;

    assert_eq!(run.state.order(), ["write", "review", "write", "review"]);
    assert_eq!(run.answer(), "APPROVED");
    assert_eq!(run.output("write"), Some("draft 2"));
    Ok(())
}

#[tokio::test]
async fn a_failed_node_stops_its_branch_only() -> anyhow::Result<()> {
    let broken = node_fn(|_: String| -> Reply { Box::pin(async { Err("disk full".into()) }) });
    let graph = Graph::builder()
        .node("start", sleeping("go", 0))
        .node("broken", broken)
        .node("after_broken", sleeping("unreachable", 0))
        .node("healthy", sleeping("fine", 0))
        .edge("start", "broken")
        .edge("broken", "after_broken")
        .edge("start", "healthy")
        .build()?;

    let error = graph.run("Task").await.unwrap_err();

    assert_eq!(error.failures.len(), 1);
    assert!(
        matches!(&error.failures[0], GraphFailure::Node { node, source } if node == "broken" && source.to_string() == "disk full")
    );
    assert_eq!(error.run.output("healthy"), Some("fine"));
    assert_eq!(error.run.state.runs("after_broken"), 0);
    assert_eq!(error.to_string(), r#"node "broken" failed: disk full"#);
    Ok(())
}

#[tokio::test]
async fn a_slow_node_times_out() -> anyhow::Result<()> {
    let graph = Graph::builder()
        .node("slow", sleeping("late", 5_000))
        .node_timeout(Duration::from_millis(50))
        .build()?;

    let error = graph.run("Task").await.unwrap_err();

    assert!(matches!(&error.failures[..], [GraphFailure::NodeTimeout { node }] if node == "slow"));
    Ok(())
}

#[tokio::test]
async fn an_endless_loop_hits_the_execution_limit() -> anyhow::Result<()> {
    let graph = Graph::builder()
        .node("a", sleeping("a", 0))
        .node("b", sleeping("b", 0))
        .edge("a", "b")
        .edge("b", "a")
        .entry("a")
        .max_node_executions(5)
        .build()?;

    let error = graph.run("Task").await.unwrap_err();

    assert!(matches!(
        error.failures[..],
        [GraphFailure::MaxNodeExecutions { limit: 5 }]
    ));
    assert_eq!(error.run.state.order().len(), 5);
    Ok(())
}

#[tokio::test]
async fn swarms_and_graphs_nest() -> anyhow::Result<()> {
    let planner = AgentBuilder::new(MockCompletionModel::from_turns([MockTurn::tool_call(
        "call-1",
        rig_multiagent::HANDOFF_TOOL,
        serde_json::json!({"agent_name": "builder", "message": "Build it"}),
    )
    .with_usage(tokens(5, 1))]))
    .name("planner")
    .build();
    let builder = AgentBuilder::new(MockCompletionModel::from_turns([
        MockTurn::text("built").with_usage(tokens(7, 2))
    ]))
    .name("builder")
    .build();
    let swarm = Swarm::builder().agent(planner).agent(builder).build()?;
    let inner = Graph::builder()
        .node("team", swarm)
        .node(
            "stamp",
            node_fn(|input: String| -> Reply {
                Box::pin(async move {
                    Ok(format!(
                        "stamped({})",
                        input.lines().last().unwrap_or_default()
                    ))
                })
            }),
        )
        .edge("team", "stamp")
        .build()?;
    let (outer_last, inputs) = recording("shipped");
    let outer = Graph::builder()
        .node("inner", inner)
        .node("ship", outer_last)
        .edge("inner", "ship")
        .build()?;

    let run = outer.run("Make a thing").await?;

    assert_eq!(run.output("inner"), Some("stamped(built)"));
    assert_eq!(run.answer(), "shipped");
    assert!(inputs.lock().unwrap()[0].ends_with("From inner:\nstamped(built)"));
    assert_eq!(
        run.usage.input_tokens,
        Some(12),
        "usage adds up through the nesting"
    );
    Ok(())
}

#[test]
fn bad_graphs_do_not_build() {
    let node = || sleeping("", 0);
    assert_eq!(Graph::builder().build().err(), Some(BuildError::Empty));
    assert_eq!(
        Graph::builder()
            .node("a", node())
            .node("a", node())
            .build()
            .err(),
        Some(BuildError::DuplicateName("a".into()))
    );
    assert_eq!(
        Graph::builder()
            .node("a", node())
            .edge("a", "b")
            .build()
            .err(),
        Some(BuildError::UnknownName("b".into()))
    );
    assert_eq!(
        Graph::builder()
            .node("a", node())
            .node("b", node())
            .edge("a", "b")
            .edge("b", "a")
            .max_node_executions(4)
            .build()
            .err(),
        Some(BuildError::NoEntry)
    );
}
