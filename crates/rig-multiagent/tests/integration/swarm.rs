//! Swarms driven by scripted models: no keys, no network.

use rig_agent::{Agent, AgentBuilder};
use rig_core::completion::{CompletionRequest, Usage};
use rig_core::test_utils::{MockCompletionModel, MockTurn};
use rig_multiagent::{BuildError, HANDOFF_TOOL, Swarm, SwarmFailure};
use serde_json::json;

fn agent(name: &str, description: &str, turns: Vec<MockTurn>) -> (Agent, MockCompletionModel) {
    let model = MockCompletionModel::from_turns(turns);
    let agent = AgentBuilder::new(model.clone())
        .name(name)
        .description(description)
        .default_max_turns(5)
        .build();
    (agent, model)
}

fn handoff(to: &str, message: &str) -> MockTurn {
    MockTurn::tool_call(
        "call-1",
        HANDOFF_TOOL,
        json!({"agent_name": to, "message": message}),
    )
}

/// Everything the model was sent, as JSON text to search.
fn sent(request: &CompletionRequest) -> String {
    serde_json::to_string(&request.chat_history).unwrap()
}

fn tokens(input: u64, output: u64) -> Usage {
    let mut usage = Usage::default();
    usage.input_tokens = Some(input);
    usage.output_tokens = Some(output);
    usage.total_tokens = Some(input + output);
    usage
}

#[tokio::test]
async fn an_agent_hands_off_and_the_next_one_answers() {
    let (researcher, researcher_model) = agent(
        "researcher",
        "Finds facts",
        vec![
            MockTurn::tool_call(
                "call-1",
                HANDOFF_TOOL,
                json!({"agent_name": "coder", "message": "Write the parser", "context": {"format": "csv"}}),
            )
            .with_usage(tokens(10, 2)),
        ],
    );
    let (coder, coder_model) = agent(
        "coder",
        "Writes Rust",
        vec![MockTurn::text("fn parse() {}").with_usage(tokens(20, 5))],
    );
    let swarm = Swarm::builder()
        .agent(researcher)
        .agent(coder)
        .build()
        .expect("the builder is valid");

    let run = swarm
        .run("Parse the export")
        .await
        .expect("the run succeeds");

    assert_eq!(run.answer, "fn parse() {}");
    assert_eq!(run.agent, "coder");
    assert_eq!(run.agents, ["researcher", "coder"]);
    assert_eq!(run.handoffs.len(), 1);
    assert_eq!(run.handoffs.first().expect("item 0").from, "researcher");
    assert_eq!(
        run.handoffs.first().expect("item 0").message,
        "Write the parser"
    );
    assert_eq!(
        run.handoffs.first().expect("item 0").context,
        Some(json!({"format": "csv"}))
    );
    assert_eq!(
        run.usage.input_tokens,
        Some(30),
        "the stopped turn counts too"
    );
    assert_eq!(run.usage.output_tokens, Some(7));

    // The researcher's turn ended at the handoff: one model call.
    let researcher_calls = researcher_model.requests();
    assert_eq!(researcher_calls.len(), 1);
    let first = sent(researcher_calls.first().expect("item 0"));
    assert!(first.contains("User request: Parse the export"), "{first}");
    assert!(first.contains("- coder: Writes Rust"), "{first}");
    assert!(
        researcher_calls
            .first()
            .expect("item 0")
            .tools
            .iter()
            .any(|tool| tool.name == HANDOFF_TOOL)
    );

    let next = sent(coder_model.requests().first().expect("item 0"));
    assert!(
        next.contains("Handoff from researcher: Write the parser"),
        "{next}"
    );
    assert!(
        next.contains("Agents who worked on this so far: researcher"),
        "{next}"
    );
    assert!(
        next.contains(r#"- researcher: {\"format\":\"csv\"}"#),
        "{next}"
    );
    assert!(next.contains("- researcher: Finds facts"), "{next}");
}

#[tokio::test]
async fn a_handoff_to_nobody_is_refused_and_the_agent_carries_on() {
    let (solo, model) = agent(
        "solo",
        "Does it all",
        vec![handoff("ghost", "Help"), MockTurn::text("Did it myself.")],
    );
    let (other, _) = agent("other", "Unused", vec![]);
    let swarm = Swarm::builder()
        .agent(solo)
        .agent(other)
        .build()
        .expect("the builder is valid");

    let run = swarm.run("Task").await.expect("the run succeeds");

    assert_eq!(run.answer, "Did it myself.");
    assert!(run.handoffs.is_empty());
    let retry = sent(model.requests().get(1).expect("item 1"));
    assert!(retry.contains("no agent is named"), "{retry}");
    assert!(retry.contains("other"), "{retry}");
}

#[tokio::test]
async fn too_many_handoffs_fail_the_run() {
    let (ping, _) = agent("ping", "", vec![handoff("pong", "1"), handoff("pong", "3")]);
    let (pong, _) = agent("pong", "", vec![handoff("ping", "2")]);
    let swarm = Swarm::builder()
        .agent(ping)
        .agent(pong)
        .max_handoffs(2)
        .build()
        .expect("the builder is valid");

    let error = swarm.run("Task").await.unwrap_err();

    assert!(
        matches!(error.reason, SwarmFailure::MaxHandoffs { limit: 2 }),
        "{error}"
    );
    assert_eq!(error.handoffs.len(), 2);
    assert_eq!(error.agents, ["ping", "pong", "ping"]);
}

#[tokio::test]
async fn work_bouncing_between_two_agents_is_caught() {
    let (ping, _) = agent("ping", "", vec![handoff("pong", "1"), handoff("pong", "3")]);
    let (pong, pong_model) = agent("pong", "", vec![handoff("ping", "2")]);
    let (judge, _) = agent("judge", "Settles it", vec![]);
    let swarm = Swarm::builder()
        .agent(ping)
        .agent(pong)
        .agent(judge)
        .detect_repetition(4, 3)
        .build()
        .expect("the builder is valid");

    let error = swarm.run("Task").await.unwrap_err();

    assert!(
        matches!(
            error.reason,
            SwarmFailure::Repetitive {
                window: 4,
                unique: 2
            }
        ),
        "{error}"
    );
    assert_eq!(
        pong_model.request_count(),
        1,
        "the repeated turn does not run"
    );
}

#[tokio::test]
async fn an_agent_failure_ends_the_run_with_the_trail() {
    let (first, _) = agent("first", "", vec![handoff("second", "Go")]);
    let (second, _) = agent("second", "", vec![MockTurn::error("model down")]);
    let swarm = Swarm::builder()
        .agent(first)
        .agent(second)
        .build()
        .expect("the builder is valid");

    let error = swarm.run("Task").await.unwrap_err();

    match &error.reason {
        SwarmFailure::Agent { agent, .. } => assert_eq!(agent, "second"),
        other => panic!("unexpected failure: {other}"),
    }
    assert_eq!(error.handoffs.len(), 1);
}

#[tokio::test]
async fn the_run_starts_with_the_entry_agent() {
    let (first, first_model) = agent("first", "", vec![]);
    let (second, _) = agent("second", "", vec![MockTurn::text("From second.")]);
    let swarm = Swarm::builder()
        .agent(first)
        .agent(second)
        .entry("second")
        .build()
        .expect("the builder is valid");

    let run = swarm.run("Task").await.expect("the run succeeds");

    assert_eq!(run.agent, "second");
    assert_eq!(first_model.request_count(), 0);
}

#[tokio::test]
async fn a_lone_agent_gets_no_handoff_tool() {
    let (solo, model) = agent("solo", "", vec![MockTurn::text("Done.")]);
    let run = Swarm::builder()
        .agent(solo)
        .build()
        .expect("the builder is valid")
        .run("Task")
        .await
        .expect("the run succeeds");

    assert_eq!(run.answer, "Done.");
    assert!(model.requests().first().expect("item 0").tools.is_empty());
    assert!(!sent(model.requests().first().expect("item 0")).contains(HANDOFF_TOOL));
}

#[test]
fn bad_swarms_do_not_build() {
    let unnamed = AgentBuilder::new(MockCompletionModel::text("")).build();
    assert_eq!(
        Swarm::builder().agent(unnamed).build().err(),
        Some(BuildError::UnnamedAgent)
    );
    let (a, _) = agent("a", "", vec![]);
    let (also_a, _) = agent("a", "", vec![]);
    assert_eq!(
        Swarm::builder()
            .agent(a.clone())
            .agent(also_a)
            .build()
            .err(),
        Some(BuildError::DuplicateName("a".into()))
    );
    assert_eq!(
        Swarm::builder().agent(a).entry("b").build().err(),
        Some(BuildError::UnknownName("b".into()))
    );
    assert_eq!(Swarm::builder().build().err(), Some(BuildError::Empty));
}
