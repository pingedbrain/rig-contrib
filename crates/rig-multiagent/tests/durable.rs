//! What a resumed run does not repeat: finished nodes of a cut batch, tool
//! calls with side effects, answers from a person, a nested swarm's turns.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures::StreamExt;
use rig_agent::{Agent, AgentBuilder};
use rig_core::test_utils::{MockCompletionModel, MockStreamEvent, MockTurn};
use rig_core::tool::{Tool, ToolContext};
use rig_memory::{DirStore, MemoryStore};
use rig_multiagent::{
    ASK_TOOL, Graph, GraphEvent, HANDOFF_TOOL, Node, NodeError, Swarm, SwarmEvent, node_fn,
};
use serde_json::{Value, json};

type Reply = std::pin::Pin<Box<dyn Future<Output = Result<String, NodeError>> + Send>>;

/// A tool with a side effect: it counts the mails it "sends".
#[derive(Clone, Default)]
struct SendMail(Arc<AtomicUsize>);

#[derive(Debug, thiserror::Error)]
#[error("unreachable")]
struct Never;

impl Tool for SendMail {
    const NAME: &'static str = "send_mail";
    type Args = Value;
    type Output = String;
    type Error = Never;

    fn description(&self) -> String {
        "Send a mail.".into()
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {"to": {"type": "string"}}})
    }

    async fn call(&self, _context: &mut ToolContext, args: Value) -> Result<String, Never> {
        let sent = self.0.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(format!("mail #{sent} sent to {}", args["to"]))
    }
}

fn mailer(name: &str, turns: Vec<MockTurn>, mail: &SendMail) -> (Agent, MockCompletionModel) {
    let model = MockCompletionModel::from_turns(turns);
    let agent = AgentBuilder::new(model.clone())
        .name(name)
        .tool(mail.clone())
        .default_max_turns(4)
        .build();
    (agent, model)
}

fn send_to(who: &str) -> MockTurn {
    MockTurn::tool_call("call-1", "send_mail", json!({"to": who}))
}

fn sent(model: &MockCompletionModel, request: usize) -> String {
    serde_json::to_string(&model.requests()[request].chat_history).unwrap()
}

fn counted(text: &'static str, millis: u64) -> (impl Node, Arc<AtomicUsize>) {
    let runs = Arc::new(AtomicUsize::new(0));
    let count = runs.clone();
    let node = node_fn(move |_: String| -> Reply {
        count.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(millis)).await;
            Ok(text.to_owned())
        })
    });
    (node, runs)
}

fn fan(dir: &std::path::Path) -> (Graph, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let (fast, fasts) = counted("fast result", 0);
    let (slow, slows) = counted("slow result", 200);
    let join = node_fn(|input: String| -> Reply { Box::pin(async move { Ok(input) }) });
    let graph = Graph::builder()
        .node("fast", fast)
        .node("slow", slow)
        .node("join", join)
        .edge("fast", "join")
        .edge("slow", "join")
        .checkpoints(DirStore::open(dir).unwrap())
        .build()
        .unwrap();
    (graph, fasts, slows)
}

#[tokio::test]
async fn a_cut_batch_reruns_only_its_unfinished_nodes() {
    let dir = tempfile::tempdir().unwrap();
    let (graph, _, _) = fan(dir.path());
    // The process stops once the fast node is done and the slow one is not.
    let mut events = graph.stream_as("fan-1", "Task");
    while let Some(event) = events.next().await {
        if matches!(&event, GraphEvent::NodeFinished { node, .. } if node == "fast") {
            break;
        }
    }
    drop(events);

    let (graph, fasts, slows) = fan(dir.path());
    let run = graph.run_as("fan-1", "Task").await.unwrap();

    assert_eq!(
        (fasts.load(Ordering::SeqCst), slows.load(Ordering::SeqCst)),
        (0, 1)
    );
    let joined = run.output("join").unwrap();
    assert!(
        joined.contains("From fast:\nfast result") && joined.contains("From slow:\nslow result")
    );
}

#[tokio::test]
async fn a_resumed_turn_replays_its_tools_instead_of_running_them_again() {
    let mail = SendMail::default();
    // The turn sends a mail, then the model fails; resumed, the model asks for
    // the same mail, which the journal answers without sending it again.
    let (agent, model) = mailer(
        "notifier",
        vec![
            send_to("ops"),
            MockTurn::error("503"),
            send_to("ops"),
            MockTurn::text("Notified."),
        ],
        &mail,
    );
    let swarm = Swarm::builder()
        .agent(agent)
        .checkpoints(MemoryStore::default())
        .build()
        .unwrap();

    assert!(swarm.run_as("notify-1", "Tell ops").await.is_err());
    let run = swarm.run_as("notify-1", "Tell ops").await.unwrap();

    assert_eq!(run.answer, "Notified.");
    assert_eq!(mail.0.load(Ordering::SeqCst), 1, "one mail, not two");
    assert!(
        sent(&model, 3).contains("mail #1 sent"),
        "the model got the recorded result"
    );
}

#[tokio::test]
async fn a_different_call_on_replay_runs_for_real() {
    let mail = SendMail::default();
    let (agent, _) = mailer(
        "notifier",
        vec![
            send_to("ops"),
            MockTurn::error("503"),
            send_to("dev"),
            MockTurn::text("Notified."),
        ],
        &mail,
    );
    let swarm = Swarm::builder()
        .agent(agent)
        .checkpoints(MemoryStore::default())
        .build()
        .unwrap();

    assert!(swarm.run_as("notify-2", "Tell someone").await.is_err());
    swarm.run_as("notify-2", "Tell someone").await.unwrap();

    assert_eq!(mail.0.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn an_agent_node_rerun_replays_its_tools() {
    let mail = SendMail::default();
    let (agent, _) = mailer(
        "notifier",
        vec![
            send_to("ops"),
            MockTurn::error("503"),
            send_to("ops"),
            MockTurn::text("Notified."),
        ],
        &mail,
    );
    let graph = Graph::builder()
        .node("notify", agent)
        .checkpoints(MemoryStore::default())
        .build()
        .unwrap();

    assert!(graph.run_as("notify-3", "Tell ops").await.is_err());
    assert_eq!(
        graph.run_as("notify-3", "Tell ops").await.unwrap().answer(),
        "Notified."
    );
    assert_eq!(mail.0.load(Ordering::SeqCst), 1);
}

fn asking(turns: Vec<Vec<MockStreamEvent>>) -> Agent {
    let model = MockCompletionModel::from_stream_turns(turns);
    AgentBuilder::new(model)
        .name("planner")
        .default_max_turns(4)
        .build()
}

fn ask() -> Vec<MockStreamEvent> {
    vec![
        MockStreamEvent::tool_call("call-1", ASK_TOOL, json!({"question": "Which database?"})),
        MockStreamEvent::final_response_with_default_usage(),
    ]
}

#[tokio::test]
async fn an_answer_from_a_person_is_not_asked_for_twice() {
    let answer = vec![
        MockStreamEvent::text("Using Postgres."),
        MockStreamEvent::final_response_with_default_usage(),
    ];
    // Asks, gets the answer, then its model fails; resumed, it asks again.
    let failing = vec![MockStreamEvent::error("503")];
    let agent = asking(vec![ask(), failing, ask(), answer]);
    let swarm = Swarm::builder()
        .agent(agent)
        .allow_questions()
        .checkpoints(MemoryStore::default())
        .build()
        .unwrap();

    let mut questions = 0;
    let mut events = swarm.stream_as("plan-1", "Set up storage");
    while let Some(event) = events.next().await {
        if let SwarmEvent::Question(question) = event {
            questions += 1;
            question.answer("Postgres");
        }
    }
    drop(events);
    let mut events = swarm.stream_as("plan-1", "Set up storage");
    let mut outcome = None;
    while let Some(event) = events.next().await {
        match event {
            SwarmEvent::Question(_) => questions += 1,
            SwarmEvent::Finished(finished) => outcome = Some(*finished),
            _ => {}
        }
    }

    assert_eq!(questions, 1, "the second run replays the answer");
    assert_eq!(outcome.unwrap().unwrap().answer, "Using Postgres.");
}

#[tokio::test]
async fn a_nested_swarm_resumes_inside_its_node() {
    let first_model = MockCompletionModel::from_turns([MockTurn::tool_call(
        "call-1",
        HANDOFF_TOOL,
        json!({"agent_name": "second", "message": "Finish it"}),
    )]);
    let first = AgentBuilder::new(first_model.clone()).name("first").build();
    let second_model =
        MockCompletionModel::from_turns([MockTurn::error("503"), MockTurn::text("finished")]);
    let second = AgentBuilder::new(second_model).name("second").build();
    // The swarm has no store of its own: it keeps its progress in the graph's.
    let team = Swarm::builder().agent(first).agent(second).build().unwrap();
    let graph = Graph::builder()
        .node("team", team)
        .checkpoints(MemoryStore::default())
        .build()
        .unwrap();

    assert!(graph.run_as("build-1", "Build it").await.is_err());
    let run = graph.run_as("build-1", "Build it").await.unwrap();

    assert_eq!(run.answer(), "finished");
    assert_eq!(
        first_model.request_count(),
        1,
        "the first agent's turn is not redone"
    );
}
