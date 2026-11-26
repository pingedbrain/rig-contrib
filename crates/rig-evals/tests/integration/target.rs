//! What targets report: an agent's answer, tool calls and usage; a function's
//! answer.

use rig_agent::AgentBuilder;
use rig_agent::test_utils::MockAddTool;
use rig_core::test_utils::{MockCompletionModel, MockTurn};
use rig_evals::{Case, Target, TargetError, TargetOutput, ToolCall, target_fn};
use serde_json::json;

use crate::tokens;

#[tokio::test]
async fn an_agent_target_reports_its_tool_calls_and_usage() {
    let model = MockCompletionModel::from_turns([
        MockTurn::tool_call("call-1", "add", json!({"x": 2, "y": 3})).with_usage(tokens(10, 2)),
        MockTurn::text("5").with_usage(tokens(20, 1)),
    ]);
    let agent = AgentBuilder::new(model)
        .tool(MockAddTool)
        .default_max_turns(3)
        .build();

    let output = Target::run(&agent, &Case::new("add", "What is 2 + 3?"))
        .await
        .expect("the run succeeds");

    assert_eq!(output.output, "5");
    assert_eq!(
        output.tool_calls,
        [ToolCall::new("add", json!({"x": 2, "y": 3}))]
    );
    assert_eq!(output.usage.input_tokens, Some(30));
    assert_eq!(output.usage.output_tokens, Some(3));
    assert_eq!(output.usage.total_tokens, Some(33));
}

#[tokio::test]
async fn a_failing_agent_is_a_target_error() {
    let agent =
        AgentBuilder::new(MockCompletionModel::from_turns([MockTurn::error("down")])).build();
    assert!(Target::run(&agent, &Case::new("a", "Hi")).await.is_err());
}

#[tokio::test]
async fn a_function_target_gets_the_case() {
    let echo = target_fn(|case: Case| async move {
        Ok::<_, TargetError>(TargetOutput::new(case.input).with_usage(tokens(1, 1)))
    });
    let output = echo.run(&Case::new("a", "ping")).await.expect("runs");
    assert_eq!(output.output, "ping");
    assert_eq!(output.usage.total_tokens, Some(2));
}
