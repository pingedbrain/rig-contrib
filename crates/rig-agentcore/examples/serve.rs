//! Serve a Rig agent on the AgentCore Runtime HTTP contract, locally.
//!
//! Listens on `PORT` (default 8080), the port AgentCore sends to. Without
//! AWS, a scripted model answers each invocation with the next numbered
//! reply, never running out, and logs how many messages each request carries, so a
//! session's history shows up as it grows. Set `OPENROUTER_MODEL` and
//! `OPENROUTER_API_KEY` to answer with an OpenRouter model instead.
//!
//! ```sh
//! PORT=3200 cargo run -p rig-agentcore --example serve
//! ```
//!
//! In another shell, with the `PORT` above (8080 when unset):
//!
//! ```sh
//! # Health check: {"status":"Healthy"}
//! curl -s localhost:3200/ping
//!
//! # One JSON answer: {"response":"...","status":"success"}
//! curl -s localhost:3200/invocations -H 'content-type: application/json' \
//!   -d '{"prompt":"Hi, I am Ana."}'
//!
//! # The same contract as server-sent events. The session header names the
//! # conversation, so this call sees the first one.
//! curl -sN localhost:3200/invocations \
//!   -H 'content-type: application/json' -H 'accept: text/event-stream' \
//!   -H 'X-Amzn-Bedrock-AgentCore-Runtime-Session-Id: demo-session-1234567890' \
//!   -d '{"prompt":"What is my name?"}'
//! ```
//!
//! A real deployment builds this app as an ARM64 container image, pushes it
//! to Amazon ECR and runs it on AgentCore Runtime, which then sends the same
//! requests. This example creates no AWS resources.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rig_agent::AgentBuilder;
use rig_agentcore::AgentCoreApp;
use rig_core::completion::CompletionRequest;
use rig_core::driver::{Exchange, Model, Opening, Transport};
use rig_core::test_utils::{MockCompletionModel, MockFrame, MockScript, MockStreamEvent, MockTurn};
use rig_core::wire::Mode;

/// Numbers the scripted replies. Rig's mock takes its turns up front, so each
/// request gets a fresh one-turn mock, which keeps the script endless.
#[derive(Clone, Default)]
struct Scripted {
    replies: Arc<AtomicUsize>,
}

impl Transport<MockScript> for Scripted {
    fn send(&self, request: CompletionRequest, exchange: Exchange) -> Opening<MockFrame> {
        // The messages this call sends: the prompt plus the session's history.
        eprintln!(
            "scripted model: {} message(s) in the request",
            request.chat_history.len()
        );
        let n = self.replies.fetch_add(1, Ordering::Relaxed) + 1;
        let text = format!("Scripted reply {n}.");
        match exchange.mode {
            Mode::Unary => MockCompletionModel::from_turns([MockTurn::text(text)])
                .transport
                .send(request, exchange),
            Mode::Streaming => MockCompletionModel::from_stream_turns([vec![
                MockStreamEvent::text(text),
                MockStreamEvent::final_response_with_default_usage(),
            ]])
            .transport
            .send(request, exchange),
        }
    }
}

fn scripted_model() -> Model<MockScript, Scripted> {
    Model::new(MockScript::default(), Scripted::default())
}

const PREAMBLE: &str = "You are a concise, friendly assistant.";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let openrouter = std::env::var("OPENROUTER_MODEL").ok();
    let key = std::env::var("OPENROUTER_API_KEY").ok();

    match (openrouter, key) {
        (Some(model), Some(_)) => {
            println!("model: OpenRouter {model}");
            let model = rig_core::providers::openrouter::from_env()?.chat(&model);
            let agent = AgentBuilder::new(model)
                .name("Rig AgentCore helper")
                .preamble(PREAMBLE)
                .build();
            AgentCoreApp::new(agent).serve().await?;
        }
        _ => {
            println!("model: scripted (endless)");
            let agent = AgentBuilder::new(scripted_model())
                .name("Rig AgentCore helper")
                .preamble(PREAMBLE)
                .build();
            AgentCoreApp::new(agent).serve().await?;
        }
    }
    Ok(())
}
