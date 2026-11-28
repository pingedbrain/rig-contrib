//! Swarms driven by scripted models: no keys, no network.

use rig_agent::completion::PromptError;
use rig_agent::{Agent, AgentBuilder};
use rig_core::completion::{CompletionRequest, Usage};
use rig_core::test_utils::{MockCompletionModel, MockTurn};
use rig_core::tool::{Tool, ToolContext};
use rig_multiagent::{BuildError, HANDOFF_TOOL, Swarm, SwarmFailure};
use serde_json::{Value, json};

/// A tool that answers with a fact the next model call can read.
#[derive(Clone, Default)]
pub(crate) struct Lookup;

#[derive(Debug, thiserror::Error)]
#[error("unreachable")]
pub(crate) struct Never;

impl Tool for Lookup {
    const NAME: &'static str = "lookup";
    type Args = Value;
    type Output = String;
    type Error = Never;

    fn description(&self) -> String {
        "Look a fact up.".into()
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {"topic": {"type": "string"}}})
    }

    async fn call(&self, _context: &mut ToolContext, _args: Value) -> Result<String, Never> {
        Ok("The export has 3 columns.".into())
    }
}

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
async fn a_chain_that_moves_forward_is_not_stopped() {
    let (first, first_model) = agent("first", "", vec![handoff("second", "Step two")]);
    let (second, _) = agent("second", "", vec![handoff("third", "Step three")]);
    let (third, _) = agent("third", "", vec![MockTurn::text("Done.")]);
    let swarm = Swarm::builder()
        .agent(first)
        .agent(second)
        .agent(third)
        .detect_repetition(3, 3)
        .build()
        .expect("the builder is valid");

    let run = swarm.run("Task").await.expect("A, B, C is not repetitive");

    assert_eq!(run.agents, ["first", "second", "third"]);
    assert_eq!(run.answer, "Done.");
    let requests = first_model.requests();
    let tools = &requests.first().expect("item 0").tools;
    let handoff_tool = tools
        .iter()
        .find(|tool| tool.name == HANDOFF_TOOL)
        .expect("the handoff tool");
    assert!(
        handoff_tool
            .description
            .contains("Answer directly yourself"),
        "{}",
        handoff_tool.description
    );
    assert!(
        handoff_tool.description.contains("Do not hand back"),
        "{}",
        handoff_tool.description
    );
}

#[tokio::test]
async fn bouncing_is_not_stopped_unless_repetition_is_on() {
    let (ping, _) = agent("ping", "", vec![handoff("pong", "1"), handoff("pong", "3")]);
    let (pong, _) = agent("pong", "", vec![handoff("ping", "2"), handoff("ping", "4")]);
    let swarm = Swarm::builder()
        .agent(ping)
        .agent(pong)
        .max_handoffs(3)
        .build()
        .expect("the builder is valid");

    let error = swarm.run("Task").await.unwrap_err();

    assert!(
        matches!(error.reason, SwarmFailure::MaxHandoffs { limit: 3 }),
        "{error}"
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

/// An agent with no turn budget of its own: the swarm must give it one.
fn bare(name: &str, turns: Vec<MockTurn>) -> (Agent, MockCompletionModel) {
    let model = MockCompletionModel::from_turns(turns);
    let agent = AgentBuilder::new(model.clone())
        .name(name)
        .description("Looks things up")
        .tool(Lookup)
        .build();
    (agent, model)
}

#[tokio::test]
async fn an_agent_calls_a_tool_then_answers_inside_a_swarm() {
    let (researcher, model) = bare(
        "researcher",
        vec![
            MockTurn::tool_call("call-1", "lookup", json!({"topic": "export"})),
            MockTurn::text("It has 3 columns."),
        ],
    );
    let (coder, _) = agent("coder", "Writes Rust", vec![]);
    let swarm = Swarm::builder()
        .agent(researcher)
        .agent(coder)
        .build()
        .expect("the builder is valid");

    let run = swarm
        .run("How many columns?")
        .await
        .expect("the run succeeds");

    assert_eq!(run.answer, "It has 3 columns.");
    assert_eq!(run.agents, ["researcher"]);
    assert_eq!(
        model.requests().len(),
        2,
        "one call for the tool, one for the answer"
    );
    let follow_up = sent(model.requests().get(1).expect("item 1"));
    assert!(
        follow_up.contains("The export has 3 columns."),
        "{follow_up}"
    );
}

#[tokio::test]
async fn a_refused_handoff_is_followed_by_an_answer() {
    let (solo, model) = bare(
        "solo",
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
    assert_eq!(run.agents, ["solo"]);
    assert!(run.handoffs.is_empty());
    assert_eq!(model.requests().len(), 2);
}

#[tokio::test]
async fn max_turns_per_agent_caps_each_run() {
    let (researcher, _) = bare(
        "researcher",
        vec![
            MockTurn::tool_call("call-1", "lookup", json!({"topic": "export"})),
            MockTurn::text("It has 3 columns."),
        ],
    );
    let (coder, _) = agent("coder", "Writes Rust", vec![]);
    let swarm = Swarm::builder()
        .agent(researcher)
        .agent(coder)
        .max_turns_per_agent(1)
        .build()
        .expect("the builder is valid");

    let error = swarm
        .run("How many columns?")
        .await
        .expect_err("one turn cannot fit a tool call and its answer");

    match &error.reason {
        SwarmFailure::Agent { agent, source } => {
            assert_eq!(agent, "researcher");
            assert!(matches!(**source, PromptError::MaxTurns { .. }), "{source}");
        }
        other => panic!("expected an agent failure, got {other}"),
    }
}

#[tokio::test]
async fn an_agent_keeps_a_higher_turn_budget_of_its_own() {
    let model = MockCompletionModel::from_turns([
        MockTurn::tool_call("call-1", "lookup", json!({"topic": "export"})),
        MockTurn::tool_call("call-2", "lookup", json!({"topic": "export"})),
        MockTurn::text("It has 3 columns."),
    ]);
    let researcher = AgentBuilder::new(model.clone())
        .name("researcher")
        .description("Looks things up")
        .tool(Lookup)
        .default_max_turns(3)
        .build();
    let (coder, _) = agent("coder", "Writes Rust", vec![]);
    let swarm = Swarm::builder()
        .agent(researcher)
        .agent(coder)
        .max_turns_per_agent(2)
        .build()
        .expect("the builder is valid");

    let run = swarm
        .run("How many columns?")
        .await
        .expect("the run succeeds");

    assert_eq!(run.answer, "It has 3 columns.");
    assert_eq!(model.requests().len(), 3);
}

#[test]
fn a_zero_turn_budget_does_not_build() {
    let (solo, _) = agent("solo", "Does it all", vec![]);
    let built = Swarm::builder().agent(solo).max_turns_per_agent(0).build();
    assert_eq!(built.err(), Some(BuildError::ZeroTurns));
}

#[tokio::test]
async fn one_turn_still_runs_handoffs_and_direct_answers() {
    let (researcher, researcher_model) = bare("researcher", vec![handoff("coder", "Write it")]);
    let (coder, coder_model) = bare("coder", vec![MockTurn::text("fn parse() {}")]);
    let swarm = Swarm::builder()
        .agent(researcher)
        .agent(coder)
        .max_turns_per_agent(1)
        .build()
        .expect("the builder is valid");

    let run = swarm.run("Parse it").await.expect("the run succeeds");

    assert_eq!(run.answer, "fn parse() {}");
    assert_eq!(researcher_model.requests().len(), 1);
    assert_eq!(coder_model.requests().len(), 1);
}

/// A tool that never returns, so the agent that calls it never answers.
#[derive(Clone, Default)]
struct Stall;

impl Tool for Stall {
    const NAME: &'static str = "stall";
    type Args = Value;
    type Output = String;
    type Error = Never;

    fn description(&self) -> String {
        "Never returns.".into()
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    async fn call(&self, _context: &mut ToolContext, _args: Value) -> Result<String, Never> {
        std::future::pending().await
    }
}

#[tokio::test]
async fn an_agent_that_never_answers_hits_the_agent_timeout() {
    let model =
        MockCompletionModel::from_turns([MockTurn::tool_call("call-1", "stall", json!({}))]);
    let stuck = AgentBuilder::new(model)
        .name("stuck")
        .description("Never answers")
        .tool(Stall)
        .default_max_turns(5)
        .build();
    let swarm = Swarm::builder()
        .agent(stuck)
        .agent_timeout(std::time::Duration::from_millis(50))
        .build()
        .unwrap();

    let error = swarm.run("Task").await.unwrap_err();

    assert!(
        matches!(&error.reason, SwarmFailure::Timeout { agent } if agent == "stuck"),
        "{error:?}"
    );
}
