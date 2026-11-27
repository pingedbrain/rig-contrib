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
