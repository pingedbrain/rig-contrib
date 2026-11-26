//! Where an executor keeps conversations, and how much of one a request sends.
//!
//! By default conversations live in memory, at most [`DEFAULT_MAX_CONTEXTS`]
//! of them. [`A2aExecutor::history_store`](crate::A2aExecutor::history_store)
//! keeps them in any [`rig_memory`] store instead (`MemoryStore`, `DirStore`
//! on disk, `rig-sqlite`, `rig-postgres`), so they survive a restart and can
//! be shared by several servers. Whatever the store, a request sends the
//! model only the newest messages its window allows
//! ([`DEFAULT_HISTORY_TOKENS`] tokens by default), never splitting a tool call
//! from its result.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use futures::future::BoxFuture;
use rig_core::message::Message;
use rig_memory::{AsyncStore, SessionError, load_async, save_async};

/// How many conversations the in-memory store remembers by default.
pub const DEFAULT_MAX_CONTEXTS: usize = 1_000;
/// How many tokens of history a request sends by default.
pub const DEFAULT_HISTORY_TOKENS: usize = 32_000;
/// The longest `contextId` accepted, so it can name a file or a row.
const MAX_CONTEXT_ID: usize = 128;
/// How often a save that lost a race with another writer is retried.
const SAVE_ATTEMPTS: usize = 3;

pub(crate) type HistoryError = Box<dyn std::error::Error + Send + Sync>;

/// A store of conversation histories by `contextId`.
pub(crate) trait HistoryStore: Send + Sync + 'static {
    /// The messages of `context` so far, oldest first; none for a new one.
    fn load<'a>(
        &'a self,
        context: &'a str,
    ) -> BoxFuture<'a, Result<Vec<Arc<Message>>, HistoryError>>;

    /// Add a finished run's `messages` to the end of `context`.
    fn append<'a>(
        &'a self,
        context: &'a str,
        messages: Vec<Message>,
    ) -> BoxFuture<'a, Result<(), HistoryError>>;
}

/// Whether `context` can name a conversation in any store: 1 to 128 ASCII
/// letters, digits, `-` or `_`. A client may choose its own `contextId`, and a
/// file store turns it into a path, so nothing else (`/`, `.`) gets through.
pub(crate) fn valid_context_id(context: &str) -> bool {
    !context.is_empty()
        && context.len() <= MAX_CONTEXT_ID
        && context
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// Conversations in memory, bounded in count.
// ponytail: FIFO eviction by start time; switch to LRU if long-lived contexts get evicted while active.
pub(crate) struct InMemory(Mutex<Bounded>);

struct Bounded {
    by_context: HashMap<String, Vec<Arc<Message>>>,
    started: VecDeque<String>,
    max: usize,
}

impl InMemory {
    pub(crate) fn new(max: usize) -> Self {
        Self(Mutex::new(Bounded {
            by_context: HashMap::new(),
            started: VecDeque::new(),
            max: max.max(1),
        }))
    }

    fn lock(&self) -> MutexGuard<'_, Bounded> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl HistoryStore for InMemory {
    fn load<'a>(
        &'a self,
        context: &'a str,
    ) -> BoxFuture<'a, Result<Vec<Arc<Message>>, HistoryError>> {
        let messages = self
            .lock()
            .by_context
            .get(context)
            .cloned()
            .unwrap_or_default();
        Box::pin(async move { Ok(messages) })
    }

    fn append<'a>(
        &'a self,
        context: &'a str,
        messages: Vec<Message>,
    ) -> BoxFuture<'a, Result<(), HistoryError>> {
        let mut bounded = self.lock();
        if !bounded.by_context.contains_key(context) {
            bounded.started.push_back(context.to_owned());
            while bounded.started.len() > bounded.max {
                if let Some(oldest) = bounded.started.pop_front() {
                    bounded.by_context.remove(&oldest);
                }
            }
        }
        bounded
            .by_context
            .entry(context.to_owned())
            .or_default()
            .extend(messages.into_iter().map(Arc::new));
        Box::pin(async { Ok(()) })
    }
}

/// Conversations in a [`rig_memory`] store, one session log per context.
pub(crate) struct Persistent<S>(pub(crate) S);

impl<S> Persistent<S> {
    /// The session log of `context`, kept apart from other sessions in the store.
    fn log(context: &str) -> String {
        format!("a2a-{context}")
    }
}

impl<S> HistoryStore for Persistent<S>
where
    S: AsyncStore + Send + Sync + 'static,
{
    fn load<'a>(
        &'a self,
        context: &'a str,
    ) -> BoxFuture<'a, Result<Vec<Arc<Message>>, HistoryError>> {
        Box::pin(async move {
            let conversation = load_async(&self.0, &Self::log(context)).await?;
            Ok(conversation.messages().to_vec())
        })
    }

    fn append<'a>(
        &'a self,
        context: &'a str,
        messages: Vec<Message>,
    ) -> BoxFuture<'a, Result<(), HistoryError>> {
        Box::pin(async move {
            let log = Self::log(context);
            let mut attempt = 1;
            loop {
                // Another writer may have saved since this one read; the store
                // then refuses the save whole, so reread and add on top.
                let mut conversation = load_async(&self.0, &log).await?;
                conversation.extend(messages.iter().cloned());
                match save_async(&self.0, &log, &mut conversation).await {
                    Ok(()) => return Ok(()),
                    Err(SessionError::Changed { .. }) if attempt < SAVE_ATTEMPTS => attempt += 1,
                    Err(error) => return Err(error.into()),
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn in_memory_forgets_the_oldest_context_past_the_limit() {
        let store = InMemory::new(2);
        let hi = || vec![Message::user("hi")];
        store.append("a", hi()).await.unwrap();
        store.append("b", hi()).await.unwrap();
        store.append("a", hi()).await.unwrap(); // an existing context does not count again
        assert_eq!(store.load("a").await.unwrap().len(), 2);
        store.append("c", hi()).await.unwrap();
        assert!(
            store.load("a").await.unwrap().is_empty(),
            "oldest context forgotten"
        );
        assert_eq!(store.load("b").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_rig_memory_store_keeps_conversations_across_handles() {
        let shared = rig_memory::MemoryStore::default();
        let one = Persistent(shared.clone());
        one.append(
            "ctx",
            vec![Message::user("hi"), Message::assistant("hello")],
        )
        .await
        .unwrap();
        let other = Persistent(shared); // a second server on the same store
        other
            .append("ctx", vec![Message::user("again")])
            .await
            .unwrap();
        assert_eq!(one.load("ctx").await.unwrap().len(), 3);
        assert!(one.load("new").await.unwrap().is_empty());
    }

    #[test]
    fn context_ids_that_could_escape_a_file_store_are_refused() {
        assert!(valid_context_id("019a2b3c-4d5e-7f00-8a9b-0c1d2e3f4a5b"));
        assert!(valid_context_id("user_42"));
        for bad in [
            "",
            "../etc/passwd",
            "a/b",
            "..",
            "a.b",
            "con text",
            &"x".repeat(129),
        ] {
            assert!(!valid_context_id(bad), "{bad:?}");
        }
    }
}
