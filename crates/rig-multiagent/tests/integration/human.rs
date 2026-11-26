//! A person in the loop: approval nodes, agents asking, paused timeouts.

use std::time::Duration;

use futures::StreamExt;
use rig_agent::{Agent, AgentBuilder};
use rig_core::completion::CompletionRequest;
use rig_core::test_utils::{MockCompletionModel, MockStreamEvent, MockTurn};
use rig_multiagent::{
    ASK_TOOL, Graph, GraphEvent, GraphFailure, GraphRun, GraphState, NodeError, Question, Swarm,
    SwarmEvent, SwarmRun, human, node_fn,
};
use serde_json::json;

type Reply = std::pin::Pin<Box<dyn Future<Output = Result<String, NodeError>> + Send>>;

fn fixed(text: &'static str) -> impl rig_multiagent::Node {
    node_fn(move |_: String| -> Reply { Box::pin(async move { Ok(text.to_owned()) }) })
}

fn echo() -> impl rig_multiagent::Node {
    node_fn(|input: String| -> Reply { Box::pin(async move { Ok(input) }) })
}

fn release() -> Graph {
    let approved = |state: &GraphState| state.output("approve") == Some("yes");
    Graph::builder()
        .node("build", fixed("release notes"))
        .node("approve", human("Deploy this release?"))
        .node("deploy", echo())
        .edge("build", "approve")
        .edge_if("approve", "deploy", approved)
        .edge_if("build", "deploy", approved)
        .build()
        .unwrap()
}

/// Drive a graph run, answering every question with `answer`; the questions
/// asked and the outcome.
async fn answer_graph(
    graph: &Graph,
    answer: &str,
    delay: Duration,
) -> (Vec<(String, String, String)>, Result<GraphRun, String>) {
    let mut asked = Vec::new();
    let mut events = graph.stream("Ship it");
    while let Some(event) = events.next().await {
        match event {
            GraphEvent::Question(question) => {
                asked.push(describe(&question));
                tokio::time::sleep(delay).await;
                question.answer(answer);
            }
            GraphEvent::Finished(outcome) => {
                return (asked, (*outcome).map_err(|error| error.to_string()));
            }
            _ => {}
        }
    }
    unreachable!("a run always finishes")
}

fn describe(question: &Question) -> (String, String, String) {
    (
        question.from().to_owned(),
        question.question().to_owned(),
        question.context().to_owned(),
    )
}

#[tokio::test]
async fn an_approval_gates_the_work_after_it() {
    let (asked, outcome) = answer_graph(&release(), "yes", Duration::ZERO).await;
    let run = outcome.unwrap();

    assert_eq!(asked.len(), 1);
    let (from, question, context) = &asked.first().expect("item 0");
    assert_eq!(
        (from.as_str(), question.as_str()),
        ("approve", "Deploy this release?")
    );
    assert!(context.ends_with("From build:\nrelease notes"), "{context}");
    let deployed = run.output("deploy").unwrap();
    assert!(
        deployed.contains("From build:\nrelease notes") && deployed.contains("From approve:\nyes")
    );

    let (_, outcome) = answer_graph(&release(), "no", Duration::ZERO).await;
    let run = outcome.unwrap();
    assert_eq!(run.state.runs("deploy"), 0, "a no stops the work");
    assert_eq!(run.answer(), "no");
}

#[tokio::test]
async fn waiting_for_a_person_does_not_count_against_timeouts() {
    let graph = Graph::builder()
        .node("approve", human("OK?"))
        .node_timeout(Duration::from_millis(100))
        .execution_timeout(Duration::from_millis(150))
        .build()
        .unwrap();

    let (_, outcome) = answer_graph(&graph, "yes", Duration::from_millis(400)).await;

    assert_eq!(outcome.unwrap().answer(), "yes");
}

#[tokio::test]
async fn a_dismissed_question_fails_its_node() {
    let graph = Graph::builder()
        .node("approve", human("OK?"))
        .build()
        .unwrap();
    let mut events = graph.stream("Task");
    let mut failure = None;
    while let Some(event) = events.next().await {
        match event {
            GraphEvent::Question(question) => drop(question),
            GraphEvent::Finished(outcome) => failure = Some((*outcome).unwrap_err()),
            _ => {}
        }
    }
    let failure = failure.unwrap();
    assert!(
        matches!(&failure.failures[..], [GraphFailure::Node { node, source }]
            if node == "approve" && source.to_string().contains("dismissed")),
        "{failure}"
    );
}

#[tokio::test]
async fn a_plain_run_cannot_ask() {
    let graph = Graph::builder()
        .node("approve", human("OK?"))
        .build()
        .unwrap();
    let error = graph.run("Task").await.unwrap_err();
    assert!(error.to_string().contains("stream()"), "{error}");
}

fn sent(request: &CompletionRequest) -> String {
    serde_json::to_string(&request.chat_history).unwrap()
}

/// An agent that asks, then answers with the reply it got.
fn asking_agent(name: &str) -> (Agent, MockCompletionModel) {
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::tool_call("call-1", ASK_TOOL, json!({"question": "Which database?"})),
            MockStreamEvent::final_response_with_default_usage(),
        ],
        vec![
            MockStreamEvent::text("Using Postgres."),
            MockStreamEvent::final_response_with_default_usage(),
        ],
    ]);
    let agent = AgentBuilder::new(model.clone())
        .name(name)
        .default_max_turns(5)
        .build();
    (agent, model)
}

async fn answer_swarm(swarm: &Swarm, answer: &str) -> (Vec<(String, String, String)>, SwarmRun) {
    let mut asked = Vec::new();
    let mut events = swarm.stream("Set up storage");
    while let Some(event) = events.next().await {
        match event {
            SwarmEvent::Question(question) => {
                asked.push(describe(&question));
                question.answer(answer);
            }
            SwarmEvent::Finished(outcome) => return (asked, (*outcome).unwrap()),
            _ => {}
        }
    }
    unreachable!("a run always finishes")
}

#[tokio::test]
async fn an_agent_asks_and_carries_on_with_the_answer() {
    let (agent, model) = asking_agent("planner");
    let swarm = Swarm::builder()
        .agent(agent)
        .allow_questions()
        .build()
        .unwrap();

    let (asked, run) = answer_swarm(&swarm, "Postgres").await;

    assert_eq!(
        asked,
        [("planner".into(), "Which database?".into(), String::new())]
    );
    assert_eq!(run.answer, "Using Postgres.");
    let requests = model.requests();
    assert!(
        sent(requests.first().expect("item 0")).contains(ASK_TOOL),
        "the prompt mentions the tool"
    );
    assert!(
        requests
            .first()
            .expect("item 0")
            .tools
            .iter()
            .any(|tool| tool.name == ASK_TOOL)
    );
    assert!(
        sent(requests.get(1).expect("item 1")).contains("Postgres"),
        "the answer is the tool result"
    );
}

#[tokio::test]
async fn in_a_plain_run_the_agent_is_told_nobody_can_answer() {
    let model = MockCompletionModel::from_turns([
        MockTurn::tool_call("call-1", ASK_TOOL, json!({"question": "Which database?"})),
        MockTurn::text("Picked SQLite myself."),
    ]);
    let agent = AgentBuilder::new(model.clone())
        .name("planner")
        .default_max_turns(5)
        .build();
    let swarm = Swarm::builder()
        .agent(agent)
        .allow_questions()
        .build()
        .unwrap();

    let run = swarm.run("Set up storage").await.unwrap();

    assert_eq!(run.answer, "Picked SQLite myself.");
    assert!(sent(model.requests().get(1).expect("item 1")).contains("nobody can answer"));
}

#[tokio::test]
async fn questions_from_a_nested_swarm_name_the_node() {
    let (agent, _) = asking_agent("planner");
    let swarm = Swarm::builder()
        .agent(agent)
        .allow_questions()
        .build()
        .unwrap();
    let graph = Graph::builder().node("team", swarm).build().unwrap();

    let (asked, outcome) = answer_graph(&graph, "Postgres", Duration::ZERO).await;

    assert_eq!(asked.first().expect("item 0").0, "team/planner");
    assert_eq!(outcome.unwrap().answer(), "Using Postgres.");
}

#[tokio::test]
async fn swarms_do_not_ask_unless_allowed() {
    let model = MockCompletionModel::from_turns([MockTurn::text("Done.")]);
    let agent = AgentBuilder::new(model.clone()).name("solo").build();
    Swarm::builder()
        .agent(agent)
        .build()
        .unwrap()
        .run("Task")
        .await
        .unwrap();

    assert!(model.requests().first().expect("item 0").tools.is_empty());
    assert!(!sent(model.requests().first().expect("item 0")).contains(ASK_TOOL));
}
