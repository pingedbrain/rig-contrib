//! rig-multiagent nodes as targets (feature `multiagent`).

use rig_agent::AgentBuilder;
use rig_core::test_utils::{MockCompletionModel, MockTurn};
use rig_evals::{Case, Experiment, Target, evaluators, node_target};
use rig_multiagent::{Graph, NodeError, node_fn};

use crate::tokens;

#[tokio::test]
async fn a_graph_is_a_target_with_its_answer_and_usage() {
    let writer = AgentBuilder::new(MockCompletionModel::from_turns([
        MockTurn::text("draft").with_usage(tokens(7, 3))
    ]))
    .build();
    let graph = Graph::builder()
        .node("write", writer)
        .build()
        .expect("graph");
    let target = node_target(graph);

    let output = target.run(&Case::new("a", "Write")).await.expect("runs");
    assert_eq!(output.output, "draft");
    assert_eq!(output.usage.input_tokens, Some(7));
    assert!(output.tool_calls.is_empty());

    let report = Experiment::new("graph")
        .dataset(vec![Case::new("a", "go")])
        .target(node_target(node_fn(|input: String| async move {
            Ok::<_, NodeError>(format!("did {input}"))
        })))
        .evaluator(evaluators::contains("did go"))
        .run()
        .await
        .expect("runs");
    assert!(report.passed());
}
