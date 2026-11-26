use rig_agent::completion::PromptError;
use rig_core::error::{ErrorKind, ErrorReport};

use rig_agent::AgentBuilder;
use rig_core::test_utils::{MockCompletionModel, MockTurn};

use super::{failure_message, tool_name};
use crate::{BuildError, McpServer};

#[test]
fn streamed_failures_are_classified_by_status_and_retryability() {
    let report = |status: Option<u16>, retryable: bool| {
        let mut report = ErrorReport::new(ErrorKind::ProviderResponse, "upstream said no")
            .with_retryable(retryable);
        if let Some(status) = status {
            report = report.with_http_status(status);
        }
        PromptError::Report(report)
    };
    assert!(failure_message(&report(Some(429), true)).contains("rate limiting"));
    assert!(failure_message(&report(Some(401), false)).contains("credentials"));
    assert!(failure_message(&report(Some(503), true)).contains("unavailable"));
    assert!(failure_message(&report(None, true)).contains("could not be reached"));
    assert_eq!(
        failure_message(&report(None, false)),
        "The agent run failed."
    );
}

#[test]
fn agent_names_become_provider_safe_tool_names() {
    assert_eq!(tool_name("rig-lab assistant"), "rig_lab_assistant");
    assert_eq!(tool_name("  "), "agent");
    assert_eq!(tool_name(&"x".repeat(80)).len(), 64);
}

#[test]
fn two_tools_with_one_name_are_refused_when_served() {
    let agent = |name: &str| {
        AgentBuilder::new(MockCompletionModel::from_turns(Vec::<MockTurn>::new()))
            .name(name)
            .build()
    };
    // Both names become the same tool name, `rig_helper`.
    let server = McpServer::new("test")
        .agent(agent("Rig helper"))
        .agent(agent("rig-helper"));
    let refused = server.into_router();
    assert!(
        matches!(&refused, Err(BuildError::DuplicateTool(name)) if name == "rig_helper"),
        "{:?}",
        refused.err()
    );
}
