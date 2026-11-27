//! A scripted judge model grading answers.

use rig_agent::AgentBuilder;
use rig_core::completion::FinishReason;
use rig_core::test_utils::{MockCompletionModel, MockTurn};
use rig_evals::{
    Case, Evaluator, Experiment, LlmJudge, TargetError, TargetOutput, ToolCall, Trial, Turn,
    target_fn,
};
use serde_json::json;

use crate::tokens;

fn judge(verdict: &str) -> (LlmJudge, MockCompletionModel) {
    let model = MockCompletionModel::from_turns([MockTurn::text(verdict)]);
    let agent = AgentBuilder::new(model.clone()).build();
    (
        LlmJudge::new(agent, "Is the answer polite and correct?").threshold(0.7),
        model,
    )
}

fn case() -> Case {
    Case::new("capital", "What is the capital of France?")
        .expected("Paris")
        .assertion("names Paris")
}

#[tokio::test]
async fn a_verdict_above_the_threshold_passes_with_the_reasoning() {
    let (judge, model) = judge(r#"{"score": 0.9, "reasoning": "Correct and polite."}"#);
    let score = judge
        .evaluate(&case(), &Trial::new("capital", 1, "It is Paris."))
        .await
        .expect("a verdict");

    assert!(score.passed);
    assert_eq!(score.value, 0.9);
    assert_eq!(score.explanation.as_deref(), Some("Correct and polite."));

    let request = model.requests().into_iter().next().expect("one request");
    let sent = serde_json::to_string(&request.chat_history).expect("json");
    for part in [
        "Is the answer polite and correct?",
        "What is the capital of France?",
        "Paris",
        "names Paris",
        "It is Paris.",
    ] {
        assert!(sent.contains(part), "the prompt lacks {part:?}");
    }
}

#[tokio::test]
async fn the_faithfulness_prompt_carries_the_tool_results() {
    let model = MockCompletionModel::from_turns([MockTurn::text(
        r#"{"score": 1, "reasoning": "Matches the tool."}"#,
    )]);
    let agent = AgentBuilder::new(model.clone()).build();
    let mut trial = Trial::new("weather", 1, "It is sunny in Lisbon.");
    trial.tool_calls = vec![
        ToolCall::new("weather", json!({"city": "Lisbon"})).with_result("Sunny, 24 degrees"),
        ToolCall::new("clock", json!({})),
    ];
    LlmJudge::faithfulness(agent)
        .evaluate(&Case::new("weather", "Weather in Lisbon?"), &trial)
        .await
        .expect("a verdict");

    let request = model.requests().into_iter().next().expect("one request");
    let sent = serde_json::to_string(&request.chat_history).expect("json");
    assert!(sent.contains("Result: Sunny, 24 degrees"), "{sent}");
    assert!(sent.contains("clock({})"), "{sent}");
    assert!(
        sent.contains("Leave out of the score any claim whose only source is a tool result that is not recorded"),
        "the rubric lacks the no-source rule"
    );
}

#[tokio::test]
async fn a_verdict_below_the_threshold_fails() {
    let (judge, _) = judge(r#"{"score": 0.4, "reasoning": "Rude."}"#);
    let score = judge
        .evaluate(&case(), &Trial::new("capital", 1, "Paris, obviously."))
        .await
        .expect("a verdict");
    assert!(!score.passed);
    assert_eq!(score.value, 0.4);
}

#[tokio::test]
async fn an_out_of_range_score_is_clamped() {
    let (judge, _) = judge(r#"{"score": 7, "reasoning": "Great."}"#);
    let score = judge
        .evaluate(&case(), &Trial::new("capital", 1, "Paris"))
        .await
        .expect("a verdict");
    assert_eq!(score.value, 1.0);
    assert!(score.passed);
}

#[tokio::test]
async fn no_verdict_is_a_failed_score_labelled_error() {
    let (judge, _) = judge("I think it is fine");
    let score = judge
        .evaluate(&case(), &Trial::new("capital", 1, "Paris"))
        .await
        .expect("a scored failure, not an evaluator error");
    assert!(!score.passed);
    assert_eq!(score.label.as_deref(), Some("error"));
    let why = score.explanation.expect("why");
    assert!(why.starts_with("the judge returned no verdict: "), "{why}");
    assert!(!why.contains("truncated"), "{why}");
}

/// A judge whose one scripted verdict reports 30 input and 5 output tokens.
fn judge_with_usage() -> LlmJudge {
    let model = MockCompletionModel::from_turns([MockTurn::text(
        r#"{"score": 0.9, "reasoning": "Fine."}"#,
    )
    .with_usage(tokens(30, 5))]);
    LlmJudge::new(AgentBuilder::new(model).build(), "Is it correct?")
}

#[tokio::test]
async fn the_judges_usage_is_on_its_score_and_in_the_report_totals() {
    let score = judge_with_usage()
        .evaluate(&case(), &Trial::new("capital", 1, "Paris"))
        .await
        .expect("a verdict");
    assert_eq!(score.usage, Some(tokens(30, 5)));

    let report = Experiment::new("judged")
        .dataset(vec![case()])
        .target(target_fn(|_case: Case| async {
            Ok::<_, TargetError>(TargetOutput::new("Paris").with_usage(tokens(10, 2)))
        }))
        .evaluator(judge_with_usage())
        .run()
        .await
        .expect("runs");
    assert_eq!(
        (
            report.judge_tokens.input,
            report.judge_tokens.output,
            report.judge_tokens.total
        ),
        (30, 5, 35)
    );
    assert_eq!(
        (
            report.tokens.input,
            report.tokens.output,
            report.tokens.total
        ),
        (40, 7, 47),
        "the target's tokens and the judge's"
    );
    assert!(
        report
            .to_markdown()
            .contains("Of which LLM judges: 30 input, 5 output, 35 total.")
    );
}

/// A judge whose reply stops at its token cap: the scripted reply is cut
/// mid-object, marked as stopped for length, and reports the tokens it spent.
#[tokio::test]
async fn a_reply_cut_at_max_tokens_says_so_and_keeps_its_usage() {
    let model = MockCompletionModel::from_turns([MockTurn::text(r#"{"score": 0.9, "reas"#)
        .with_finish_reason(FinishReason::Length)
        .with_usage(tokens(40, 64))]);
    let agent = AgentBuilder::new(model).max_tokens(64).build();
    let judge = LlmJudge::new(agent, "Is it correct?");
    let score = judge
        .evaluate(&case(), &Trial::new("capital", 1, "Paris"))
        .await
        .expect("a scored failure");
    assert!(!score.passed);
    assert_eq!(score.label.as_deref(), Some("error"));
    assert_eq!(
        score.explanation.as_deref(),
        Some("the judge returned no verdict: the reply was truncated at max_tokens (64)")
    );
    assert_eq!(score.usage, Some(tokens(40, 64)));
}

/// The judge's prompt as the model received it.
fn sent(model: &MockCompletionModel) -> String {
    let request = model.requests().into_iter().next().expect("one request");
    serde_json::to_string(&request.chat_history).expect("json")
}

#[tokio::test]
async fn helpfulness_sends_its_rubric_and_the_answer_and_parses_the_score() {
    let model = MockCompletionModel::from_turns([MockTurn::text(
        r#"{"score": 0.9, "reasoning": "Answers the question."}"#,
    )]);
    let judge = LlmJudge::helpfulness(AgentBuilder::new(model.clone()).build());
    let score = judge
        .evaluate(&case(), &Trial::new("capital", 1, "It is Paris."))
        .await
        .expect("a verdict");

    assert_eq!(score.evaluator, "helpfulness");
    assert!(score.passed);
    assert_eq!(score.value, 0.9);
    assert_eq!(score.explanation.as_deref(), Some("Answers the question."));

    let sent = sent(&model);
    for part in [
        "Score how helpful the answer is",
        "What is the capital of France?",
        "It is Paris.",
    ] {
        assert!(sent.contains(part), "the prompt lacks {part:?}");
    }
}

#[tokio::test]
async fn faithfulness_sends_the_transcript_and_tool_calls_and_parses_the_score() {
    let model = MockCompletionModel::from_turns([MockTurn::text(
        r#"{"score": 0.4, "reasoning": "The price is not in the evidence."}"#,
    )]);
    let judge = LlmJudge::faithfulness(AgentBuilder::new(model.clone()).build());
    let mut trial = Trial::new("capital", 1, "The fare is 42 euros.");
    trial.tool_calls = vec![ToolCall::new("lookup_fare", json!({"route": "LIS-OPO"}))];
    trial.transcript = vec![Turn::new("How much is the train?", "Let me check.")];
    let score = judge.evaluate(&case(), &trial).await.expect("a verdict");

    assert_eq!(score.evaluator, "faithfulness");
    assert!(!score.passed, "0.4 is below the default threshold");
    assert_eq!(score.value, 0.4);
    assert_eq!(
        score.explanation.as_deref(),
        Some("The price is not in the evidence.")
    );

    let sent = sent(&model);
    for part in [
        "Score whether the answer is faithful",
        "Leave out of the score any claim whose only source is a tool result that is not recorded",
        "lookup_fare",
        "LIS-OPO",
        "How much is the train?",
        "Let me check.",
        "The fare is 42 euros.",
    ] {
        assert!(sent.contains(part), "the prompt lacks {part:?}");
    }
}

#[tokio::test]
async fn goal_success_sends_every_turn_and_parses_the_score() {
    let model = MockCompletionModel::from_turns([MockTurn::text(
        r#"{"score": 1.0, "reasoning": "Booked on the second turn."}"#,
    )]);
    let judge = LlmJudge::goal_success(AgentBuilder::new(model.clone()).build());
    let case = Case::multi_turn(
        "booking",
        ["Book a table for two.", "Make it 19:00.", "Confirm it."],
    )
    .expected("Table booked for two at 19:00.");
    let mut trial = Trial::new("booking", 1, "Booked: two people, 19:00.");
    trial.transcript = vec![
        Turn::new("Book a table for two.", "For when?"),
        Turn::new("Make it 19:00.", "Done for 19:00."),
        Turn::new("Confirm it.", "Booked: two people, 19:00."),
    ];
    let score = judge.evaluate(&case, &trial).await.expect("a verdict");

    assert_eq!(score.evaluator, "goal_success");
    assert!(score.passed);
    assert_eq!(score.value, 1.0);

    let sent = sent(&model);
    for part in [
        "Score whether the agent achieved the user's goal",
        "1. Book a table for two.",
        "2. Make it 19:00.",
        "3. Confirm it.",
        "User: Book a table for two.",
        "Agent: For when?",
        "Agent: Done for 19:00.",
        "Table booked for two at 19:00.",
    ] {
        assert!(sent.contains(part), "the prompt lacks {part:?}");
    }
}

#[tokio::test]
async fn presets_keep_the_default_threshold_and_can_be_retuned() {
    let model = MockCompletionModel::from_turns([MockTurn::text(
        r#"{"score": 0.6, "reasoning": "Partly."}"#,
    )]);
    let judge = LlmJudge::goal_success(AgentBuilder::new(model).build()).threshold(0.7);
    let score = judge
        .evaluate(&case(), &Trial::new("capital", 1, "Paris"))
        .await
        .expect("a verdict");
    assert!(!score.passed);
    assert_eq!(score.evaluator, "goal_success");
}
