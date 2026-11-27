//! Serve a Rig agent on the AgentCore Runtime HTTP contract, locally.
//!
//! Listens on `PORT` (default 8080), the port AgentCore sends to. Without
//! AWS, a scripted model answers each invocation with the next line of a
//! fixed script, and logs how many messages each request carries, so a
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

use rig_agent::AgentBuilder;
use rig_agentcore::AgentCoreApp;
use rig_core::completion::CompletionRequest;
use rig_core::driver::{Exchange, Model, Opening, Transport};
use rig_core::test_utils::{
    MockCompletionModel, MockFrame, MockRuntime, MockScript, MockStreamEvent, MockTurn,
};
use rig_core::wire::Mode;

/// Replies the scripted model gives before it runs out. Each invocation uses
/// one reply; past this many the run fails with a 500.
// ponytail: fixed script per process, no cycling; OpenRouter for longer runs.
const SCRIPTED_REPLIES: usize = 1000;

/// Rig's mock answers from one script per mode, so this transport keeps one
/// of each: a whole reply and a streamed one carry the same scripted text.
#[derive(Clone)]
struct Scripted {
    whole: MockRuntime,
    streamed: MockRuntime,
}

impl Transport<MockScript> for Scripted {
    fn send(&self, request: CompletionRequest, exchange: Exchange) -> Opening<MockFrame> {
        // The messages this call sends: the prompt plus the session's history.
        eprintln!(
            "scripted model: {} message(s) in the request",
            request.chat_history.len()
        );
        match exchange.mode {
            Mode::Unary => self.whole.send(request, exchange),
            Mode::Streaming => self.streamed.send(request, exchange),
        }
    }
}

fn scripted_model() -> Model<MockScript, Scripted> {
    let text = |n: usize| format!("Scripted reply {n}.");
    let whole =
        MockCompletionModel::from_turns((1..=SCRIPTED_REPLIES).map(|n| MockTurn::text(text(n))));
    let streamed = MockCompletionModel::from_stream_turns((1..=SCRIPTED_REPLIES).map(|n| {
        vec![
            MockStreamEvent::text(text(n)),
            MockStreamEvent::final_response_with_default_usage(),
        ]
    }));
    Model::new(
        MockScript::default(),
        Scripted {
            whole: whole.transport,
            streamed: streamed.transport,
        },
    )
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
            println!("model: scripted ({SCRIPTED_REPLIES} replies)");
            let agent = AgentBuilder::new(scripted_model())
                .name("Rig AgentCore helper")
                .preamble(PREAMBLE)
                .build();
            AgentCoreApp::new(agent).serve().await?;
        }
    }
    Ok(())
}
