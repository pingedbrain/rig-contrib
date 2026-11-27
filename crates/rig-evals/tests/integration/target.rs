//! What targets report: an agent's answer, tool calls and usage; a function's
//! answer.

use rig_agent::AgentBuilder;
use rig_agent::test_utils::{MockAddTool, MockToolError};
use rig_agent::tool::{Tool, ToolContext};
use rig_core::test_utils::{MockCompletionModel, MockTurn};
use rig_evals::{Case, Target, TargetError, TargetOutput, ToolCall, target_fn};
use serde_json::{Value, json};

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
        [ToolCall::new("add", json!({"x": 2, "y": 3})).with_result("5")]
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

/// A tool whose output is longer than a recorded result may be: three-byte
/// characters, so the cut falls inside one.
struct Verbose;

impl Tool for Verbose {
    const NAME: &'static str = "verbose";
    type Error = MockToolError;
    type Args = Value;
    type Output = String;

    fn description(&self) -> String {
        "Returns a long text".to_owned()
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        _args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        Ok("\u{20ac}".repeat(2000))
    }
}

#[tokio::test]
async fn an_agent_target_records_each_tool_result_on_its_call() {
    let model = MockCompletionModel::from_turns([
        MockTurn::tool_call("call-1", "add", json!({"x": 2, "y": 3})),
        MockTurn::text("5"),
    ]);
    let agent = AgentBuilder::new(model)
        .tool(MockAddTool)
        .default_max_turns(3)
        .build();

    let output = Target::run(&agent, &Case::new("add", "What is 2 + 3?"))
        .await
        .expect("runs");

    let [call] = output.tool_calls.as_slice() else {
        panic!("one tool call: {:?}", output.tool_calls);
    };
    assert_eq!(call.result.as_deref(), Some("5"));
}

#[tokio::test]
async fn a_long_tool_result_is_cut_at_a_char_boundary() {
    let model = MockCompletionModel::from_turns([
        MockTurn::tool_call("call-1", "verbose", json!({})),
        MockTurn::text("done"),
    ]);
    let agent = AgentBuilder::new(model)
        .tool(Verbose)
        .default_max_turns(3)
        .build();

    let output = Target::run(&agent, &Case::new("long", "Go"))
        .await
        .expect("runs");

    let [call] = output.tool_calls.as_slice() else {
        panic!("one tool call: {:?}", output.tool_calls);
    };
    let result = call.result.as_deref().expect("a recorded result");
    assert!(result.ends_with(" [truncated]"), "{result}");
    assert!(
        result.len() <= 4096 + " [truncated]".len(),
        "{}",
        result.len()
    );
}

#[tokio::test]
async fn a_call_without_a_recorded_result_has_none() {
    let call = ToolCall::new("add", json!({"x": 1}));
    assert_eq!(call.result, None);
    let json = serde_json::to_value(&call).expect("json");
    assert!(json.get("result").is_none(), "{json}");
}
