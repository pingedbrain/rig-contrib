//! What the agent a handoff reaches is sent, checked on scripted models.

use rig_agent::{Agent, AgentBuilder};
use rig_core::test_utils::{MockCompletionModel, MockTurn};
use rig_multiagent::{HANDOFF_TOOL, Swarm};
use serde_json::json;

use super::swarm::Lookup;

fn agent(name: &str, description: &str, turns: Vec<MockTurn>) -> (Agent, MockCompletionModel) {
    let model = MockCompletionModel::from_turns(turns);
    let agent = AgentBuilder::new(model.clone())
        .name(name)
        .description(description)
        .default_max_turns(5)
        .build();
    (agent, model)
}

/// The explorer reads a fact with a tool, then hands off with a message that
/// does not repeat the fact. The writer must still be given the fact, because
/// it is the material the writer has to write from.
#[tokio::test]
async fn the_next_agent_is_given_what_the_previous_one_read() {
    let explorer_model = MockCompletionModel::from_turns(vec![
        MockTurn::tool_call("call-1", "lookup", json!({"topic": "export"})),
        MockTurn::tool_call(
            "call-2",
            HANDOFF_TOOL,
            json!({"agent_name": "writer", "message": "Write a summary."}),
        ),
    ]);
    let explorer = AgentBuilder::new(explorer_model)
        .name("explorer")
        .description("Reads the export")
        .default_max_turns(5)
        .tool(Lookup)
        .build();
    let (writer, writer_model) = agent(
        "writer",
        "Writes the final answer",
        vec![MockTurn::text("The export has three columns.")],
    );
    let swarm = Swarm::builder()
        .agent(explorer)
        .agent(writer)
        .build()
        .expect("the builder is valid");

    let run = swarm
        .run("What does the export hold?")
        .await
        .expect("the run succeeds");
    assert_eq!(run.answer, "The export has three columns.");

    let requests = writer_model.requests();
    let request = requests.first().expect("the writer made a model call");
    let sent = serde_json::to_string(&request.chat_history).expect("json");
    assert!(
        sent.contains("What explorer wrote and read before handing off"),
        "the writer is told what the explorer did: {sent}"
    );
    assert!(
        sent.contains("The export has 3 columns."),
        "the writer sees the tool result the explorer read: {sent}"
    );
    assert!(
        sent.contains("complete the user request yourself"),
        "the writer is told it can answer directly: {sent}"
    );
}

/// The writer hands the work back. The explorer's second turn still has its
/// own earlier reading, so it does not have to start the search again.
#[tokio::test]
async fn an_agent_handed_back_the_work_keeps_its_own_reading() {
    let explorer_model = MockCompletionModel::from_turns(vec![
        MockTurn::tool_call("call-1", "lookup", json!({"topic": "export"})),
        MockTurn::tool_call(
            "call-2",
            HANDOFF_TOOL,
            json!({"agent_name": "writer", "message": "Write a summary."}),
        ),
        MockTurn::text("The export has three columns."),
    ]);
    let explorer = AgentBuilder::new(explorer_model.clone())
        .name("explorer")
        .description("Reads the export")
        .default_max_turns(5)
        .tool(Lookup)
        .build();
    let (writer, _) = agent(
        "writer",
        "Writes the final answer",
        vec![MockTurn::tool_call(
            "call-3",
            HANDOFF_TOOL,
            json!({"agent_name": "explorer", "message": "Which columns does it have?"}),
        )],
    );
    let swarm = Swarm::builder()
        .agent(explorer)
        .agent(writer)
        .build()
        .expect("the builder is valid");

    let run = swarm
        .run("What does the export hold?")
        .await
        .expect("the run succeeds");
    assert_eq!(run.answer, "The export has three columns.");

    let requests = explorer_model.requests();
    let last = requests.last().expect("the explorer made a model call");
    let sent = serde_json::to_string(&last.chat_history).expect("json");
    assert!(
        sent.contains("What explorer wrote and read before handing off"),
        "the explorer is told its own earlier work: {sent}"
    );
    assert!(
        sent.contains("The export has 3 columns."),
        "the explorer keeps the fact it read: {sent}"
    );
}

/// A tool whose output is large: a tag first, then 100 KiB of filler.
#[derive(Clone, Default)]
struct Big;

#[derive(Debug, thiserror::Error)]
#[error("unreachable")]
struct NeverBig;

impl rig_core::tool::Tool for Big {
    const NAME: &'static str = "big";
    type Args = serde_json::Value;
    type Output = String;
    type Error = NeverBig;

    fn description(&self) -> String {
        "Read a large document.".into()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({"type": "object", "properties": {"tag": {"type": "string"}}})
    }

    async fn call(
        &self,
        _context: &mut rig_core::tool::ToolContext,
        args: serde_json::Value,
    ) -> Result<String, NeverBig> {
        let tag = args
            .get("tag")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        Ok(format!("{tag} {}", "x".repeat(100 * 1024)))
    }
}

/// Two agents pass the work back and forth ten times, each reading a 100 KiB
/// document. The last agent's prompt stays within the work budget and keeps the
/// newest reading, not the oldest.
#[tokio::test]
async fn large_work_is_bounded_and_keeps_the_newest() {
    const TURNS: usize = 10;
    let mut left = Vec::new();
    let mut right = Vec::new();
    for turn in 0..TURNS {
        let (me, other) = if turn % 2 == 0 {
            (&mut left, "right")
        } else {
            (&mut right, "left")
        };
        me.push(MockTurn::tool_call(
            format!("big-{turn}"),
            "big",
            json!({"tag": format!("turn-{turn}")}),
        ));
        me.push(MockTurn::tool_call(
            format!("handoff-{turn}"),
            HANDOFF_TOOL,
            json!({"agent_name": other, "message": format!("Continue after turn {turn}.")}),
        ));
    }
    // The agent that answers in turn TURNS, which is `left` (TURNS is even).
    left.push(MockTurn::text("done"));
    let left_model = MockCompletionModel::from_turns(left);
    let right_model = MockCompletionModel::from_turns(right);
    let build = |name: &str, model: &MockCompletionModel| {
        AgentBuilder::new(model.clone())
            .name(name)
            .description("Reads big documents")
            .default_max_turns(5)
            .tool(Big)
            .build()
    };
    let swarm = Swarm::builder()
        .agent(build("left", &left_model))
        .agent(build("right", &right_model))
        .max_handoffs(TURNS)
        .build()
        .expect("the builder is valid");

    let run = swarm.run("Read them all").await.expect("the run succeeds");
    assert_eq!(run.answer, "done");
    assert_eq!(run.handoffs.len(), TURNS);

    let requests = left_model.requests();
    let last = requests.last().expect("left made a model call");
    let sent = serde_json::to_string(&last.chat_history).expect("json");
    assert!(
        sent.len() < 40 * 1024,
        "the prompt stays within the work budget, was {} bytes",
        sent.len()
    );
    assert!(sent.contains("turn-9 "), "the newest reading is kept");
    assert!(!sent.contains("turn-0 "), "the oldest reading is dropped");
    assert!(
        sent.contains("[truncated]"),
        "a long reading is marked as cut"
    );
}
