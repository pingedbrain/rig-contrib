//! Turning a run that reports events into a stream of them.

use std::future::Future;
use std::task::Poll;

use futures::future::BoxFuture;
use futures::stream::{self, BoxStream};
use futures::{FutureExt, StreamExt};
use rig_agent::agent::{AgentRunner, MultiTurnStreamItem, PromptResponse};
use rig_agent::completion::PromptError;
use rig_core::error::{ErrorKind, ErrorReport};
use rig_core::streaming::{Item, StreamEvent};
use tokio::sync::mpsc;

/// Where a run reports its events.
pub(crate) type Emit<'a, E> = &'a (dyn Fn(E) + Send + Sync);

/// The events `run` reports, as they happen, then `finish(result)`.
///
/// The run is driven by polling the stream, so nothing runs unless the stream
/// is polled, and dropping the stream stops the run.
pub(crate) fn events<'a, E, R, F>(
    run: impl FnOnce(mpsc::UnboundedSender<E>) -> F,
    finish: fn(R) -> E,
) -> BoxStream<'a, E>
where
    E: Send + 'a,
    R: Send + 'a,
    F: Future<Output = R> + Send + 'a,
{
    // ponytail: unbounded, since events are small and a consumer that stops
    // reading also stops the run; bound it if a run ever outpaces its reader.
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let mut run: Option<BoxFuture<'a, R>> = Some(run(sender).boxed());
    let mut result = None;
    stream::poll_fn(move |cx| {
        if let Some(future) = run.as_mut()
            && let Poll::Ready(done) = future.poll_unpin(cx)
        {
            result = Some(done);
            // Dropping the run drops its sender, so the channel closes once
            // the events it sent are read.
            run = None;
        }
        match receiver.poll_recv(cx) {
            Poll::Ready(Some(event)) => Poll::Ready(Some(event)),
            Poll::Ready(None) => Poll::Ready(result.take().map(finish)),
            Poll::Pending => Poll::Pending,
        }
    })
    .boxed()
}

/// Run an agent in streaming mode, passing its text to `on_text` as the
/// model writes it.
pub(crate) async fn stream_agent(
    runner: AgentRunner,
    on_text: &(dyn Fn(&str) + Send + Sync),
) -> Result<PromptResponse, PromptError> {
    let mut stream = runner.stream();
    while let Some(item) = stream.next().await {
        match item? {
            MultiTurnStreamItem::StreamAssistantItem(Item::Event(StreamEvent::Text {
                text,
                ..
            })) => on_text(&text),
            MultiTurnStreamItem::FinalResponse(response) => return Ok(response),
            _ => {}
        }
    }
    Err(PromptError::Report(ErrorReport::new(
        ErrorKind::Response,
        "the agent's stream ended without a final response",
    )))
}
