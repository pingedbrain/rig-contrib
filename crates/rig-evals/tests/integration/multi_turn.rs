//! Multi-turn cases: turns sent in order on one conversation, the answer
//! to the last turn scored, and the transcript and usage over all turns.

use std::time::Duration;

use rig_agent::AgentBuilder;
use rig_core::test_utils::{MockCompletionModel, MockTurn};
use rig_evals::{
    Case, Dataset, Experiment, ExperimentError, Report, Target, TargetError, TargetOutput, Turn,
    evaluators, target_fn,
};

use crate::tokens;

/// Three turns: the first names a city, the last asks for it back.
fn three_turns() -> Case {
    Case::multi_turn(
        "city",
        [
            "My favourite city is Lisbon.",
            "I like walking there.",
            "What is my favourite city?",
        ],
    )
    .expected("Lisbon")
}

/// A scripted agent answering each of the three turns, with usage.
fn scripted_agent() -> (rig_agent::Agent, MockCompletionModel) {
    let model = MockCompletionModel::from_turns([
        MockTurn::text("Noted, Lisbon it is.").with_usage(tokens(10, 4)),
        MockTurn::text("Lisbon is lovely for walks.").with_usage(tokens(20, 6)),
        MockTurn::text("Your favourite city is Lisbon.").with_usage(tokens(30, 8)),
    ]);
    (AgentBuilder::new(model.clone()).build(), model)
}

#[tokio::test]
async fn the_last_answer_depends_on_the_first_turn_and_the_conversation_is_kept() {
    let (agent, model) = scripted_agent();
    let experiment = Experiment::new("multi-turn")
        .dataset(Dataset::new([three_turns()]))
        .target(agent)
        .evaluator(evaluators::contains("Lisbon"))
        .evaluator(evaluators::no_error())
        .threshold("contains", 1.0);

    let report = experiment.run().await.expect("the experiment runs");
    assert!(report.passed());
    let trial = &report.trials.first().expect("a trial").trial;
    assert_eq!(trial.output, "Your favourite city is Lisbon.");
    assert_eq!(
        trial.transcript,
        [
            Turn::new("My favourite city is Lisbon.", "Noted, Lisbon it is."),
            Turn::new("I like walking there.", "Lisbon is lovely for walks."),
            Turn::new(
                "What is my favourite city?",
                "Your favourite city is Lisbon."
            ),
        ]
    );

    // The third request carries the first turn: the model saw the history.
    let requests = model.requests();
    assert_eq!(requests.len(), 3);
    let third = requests.get(2).expect("a third request");
    let last = serde_json::to_string(&third.chat_history).expect("json");
    assert!(last.contains("My favourite city is Lisbon."), "{last}");
    assert!(last.contains("I like walking there."), "{last}");
}

#[tokio::test]
async fn usage_and_tool_calls_add_up_over_the_turns() {
    let (agent, _) = scripted_agent();
    let report = Experiment::new("multi-turn")
        .dataset(Dataset::new([three_turns()]))
        .target(agent)
        .evaluator(evaluators::no_error())
        .run()
        .await
        .expect("the experiment runs");

    assert_eq!(report.tokens.input, 60);
    assert_eq!(report.tokens.output, 18);
    assert_eq!(report.tokens.total, 78);
}

#[tokio::test]
async fn latency_covers_every_turn() {
    let case = Case::multi_turn("slow", ["one", "two", "three"]);
    let target = target_fn(|case: Case| async move {
        let mut transcript = Vec::new();
        for input in case.prompts() {
            tokio::time::sleep(Duration::from_millis(20)).await;
            transcript.push(Turn::new(input, format!("answer to {input}")));
        }
        let output = transcript
            .last()
            .map(|turn| turn.output.clone())
            .unwrap_or_default();
        Ok::<_, TargetError>(TargetOutput::new(output).with_transcript(transcript))
    });
    let report = Experiment::new("latency")
        .dataset(Dataset::new([case]))
        .target(target)
        .evaluator(evaluators::no_error())
        .run()
        .await
        .expect("the experiment runs");

    assert!(
        report.latency.p50 >= Duration::from_millis(60),
        "{:?}",
        report.latency
    );
    assert_eq!(
        report
            .trials
            .first()
            .expect("a trial")
            .trial
            .transcript
            .len(),
        3
    );
}

#[tokio::test]
async fn reports_render_a_multi_turn_trial() {
    let (agent, _) = scripted_agent();
    let report: Report = Experiment::new("multi-turn")
        .dataset(Dataset::new([three_turns()]))
        .target(agent)
        .evaluator(evaluators::contains("Lisbon"))
        .run()
        .await
        .expect("the experiment runs");

    let json: serde_json::Value =
        serde_json::from_str(&report.to_json().expect("json")).expect("valid json");
    let trial = json.pointer("/trials/0/trial").expect("a trial");
    assert_eq!(
        trial
            .get("transcript")
            .and_then(|t| t.as_array())
            .map(Vec::len),
        Some(3)
    );
    assert_eq!(
        trial.get("output").and_then(|o| o.as_str()),
        Some("Your favourite city is Lisbon.")
    );

    let junit = report.to_junit();
    assert!(
        junit.contains("<testsuite") && junit.contains("city"),
        "{junit}"
    );
    assert!(
        report.to_markdown().starts_with("# multi-turn: passed"),
        "{}",
        report.to_markdown()
    );
}

#[tokio::test]
async fn a_case_sets_input_or_turns_not_both() {
    // The JSON form rejects a case that sets both; a case built in code can
    // still do it, and the experiment refuses it.
    let json = r#"{"id":"both","input":"Hi","turns":["Hi","Again"]}"#;
    let parse = Dataset::from_jsonl(json).expect_err("rejected");
    assert!(
        parse.to_string().contains("sets both input and turns"),
        "{parse}"
    );
    let mut built = Case::new("both", "Hi");
    built.turns = vec!["Hi".to_owned(), "Again".to_owned()];
    let error = Experiment::new("both")
        .dataset(Dataset::new([built]))
        .target(target_fn(|_: Case| async { Ok::<_, TargetError>("x") }))
        .evaluator(evaluators::no_error())
        .run()
        .await
        .expect_err("rejected");
    assert!(matches!(error, ExperimentError::InputAndTurns(ref id) if id == "both"));
}

#[tokio::test]
async fn an_agent_target_without_turns_is_unchanged() {
    let model = MockCompletionModel::from_turns([MockTurn::text("Paris")]);
    let agent = AgentBuilder::new(model).build();
    let output = Target::run(&agent, &Case::new("capital", "Capital of France?"))
        .await
        .expect("runs");
    assert_eq!(output.output, "Paris");
    assert!(output.transcript.is_empty());
}

#[test]
fn single_turn_json_keeps_its_shape_and_multi_turn_json_parses() {
    let single =
        Dataset::from_jsonl(r#"{"id":"a","input":"Hi","expected":"Hello"}"#).expect("parses");
    let case = single.cases.first().expect("a case");
    assert_eq!(case.input, "Hi");
    assert!(case.turns.is_empty());
    let json = serde_json::to_string(case).expect("json");
    assert!(!json.contains("turns"), "{json}");

    let multi =
        Dataset::from_jsonl(r#"{"id":"b","turns":["one","two"],"expected":"x"}"#).expect("parses");
    let case = multi.cases.first().expect("a case");
    assert_eq!(case.prompts(), ["one", "two"]);
    assert_eq!(case.input, "");
}

#[tokio::test]
async fn a_judge_reads_every_turn_and_the_transcript() {
    use rig_evals::{Evaluator, LlmJudge, Trial};

    let model = MockCompletionModel::from_turns([MockTurn::text(
        r#"{"score": 0.9, "reasoning": "Recalls Lisbon."}"#,
    )]);
    let judge = LlmJudge::new(
        AgentBuilder::new(model.clone()).build(),
        "Does it recall the city?",
    );
    let mut trial = Trial::new("city", 1, "Your favourite city is Lisbon.");
    trial.transcript = vec![Turn::new("My favourite city is Lisbon.", "Noted.")];
    judge
        .evaluate(&three_turns(), &trial)
        .await
        .expect("a verdict");

    let request = model.requests().into_iter().next().expect("one request");
    let sent = serde_json::to_string(&request.chat_history).expect("json");
    for part in [
        "1. My favourite city is Lisbon.",
        "3. What is my favourite city?",
        "Transcript:",
        "Agent: Noted.",
    ] {
        assert!(sent.contains(part), "the prompt lacks {part:?}: {sent}");
    }
}
