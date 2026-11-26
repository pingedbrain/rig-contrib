//! Each conversation of a run (a swarm turn, an agent node) gets one prompt
//! cache key, so gateways that route by it reuse the conversation's cache.

use rig_agent::AgentBuilder;
use rig_core::completion::GenerationOptions;
use rig_core::test_utils::{MockCompletionModel, MockTurn};
use rig_memory::MemoryStore;
use rig_multiagent::{Graph, HANDOFF_TOOL, Swarm};
use serde_json::json;

fn keys(model: &MockCompletionModel) -> Vec<Option<String>> {
    model
        .requests()
        .into_iter()
        .map(|request| request.options.cache_key)
        .collect()
}

fn handoff(to: &str) -> MockTurn {
    MockTurn::tool_call(
        "call-1",
        HANDOFF_TOOL,
        json!({"agent_name": to, "message": "Go"}),
    )
}

#[tokio::test]
async fn each_swarm_turn_is_one_conversation() {
    // Two calls in the first turn (a refused handoff, then a good one), one in the second.
    let first_model = MockCompletionModel::from_turns([handoff("ghost"), handoff("second")]);
    let second_model = MockCompletionModel::from_turns([MockTurn::text("done")]);
    let first = AgentBuilder::new(first_model.clone())
        .name("first")
        .default_max_turns(3)
        .build();
    let second = AgentBuilder::new(second_model.clone())
        .name("second")
        .build();
    let swarm = Swarm::builder().agent(first).agent(second).build().unwrap();

    swarm.run("Task").await.unwrap();

    let first_keys = keys(&first_model);
    let second_keys = keys(&second_model);
    assert!(first_keys.first().expect("item 0").is_some());
    assert_eq!(
        first_keys.first().expect("item 0"),
        first_keys.get(1).expect("item 1"),
        "one key for the whole turn"
    );
    assert_ne!(
        first_keys.first().expect("item 0"),
        second_keys.first().expect("item 0"),
        "another turn, another key"
    );
}

#[tokio::test]
async fn a_resumed_node_keeps_its_key() {
    let model = MockCompletionModel::from_turns([MockTurn::error("503"), MockTurn::text("done")]);
    let agent = AgentBuilder::new(model.clone()).name("writer").build();
    let graph = Graph::builder()
        .node("write", agent)
        .checkpoints(MemoryStore::default())
        .build()
        .unwrap();

    assert!(graph.run_as("job", "Task").await.is_err());
    graph.run_as("job", "Task").await.unwrap();

    let keys = keys(&model);
    assert!(keys.first().expect("item 0").is_some());
    assert_eq!(
        keys.first().expect("item 0"),
        keys.get(1).expect("item 1"),
        "the retried node continues the same conversation"
    );
}

#[tokio::test]
async fn an_agents_own_key_is_kept() {
    let model = MockCompletionModel::from_turns([MockTurn::text("done")]);
    let agent = AgentBuilder::new(model.clone())
        .name("solo")
        .options(GenerationOptions::default().cache_key("mine"))
        .build();

    Swarm::builder()
        .agent(agent)
        .build()
        .unwrap()
        .run("Task")
        .await
        .unwrap();

    assert_eq!(keys(&model), [Some("mine".to_owned())]);
}
