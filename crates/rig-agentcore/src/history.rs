//! Where an app keeps sessions, and how much of one a request sends.
//!
//! By default sessions live in memory, at most `max_sessions` of them, and
//! each keeps only the newest messages its window allows, so memory stays
//! bounded. [`AgentCoreApp::history_store`](crate::AgentCoreApp::history_store)
//! keeps them in any [`rig_memory`] store instead (`MemoryStore`, `DirStore`
//! on disk, `rig-sqlite`, `rig-postgres`). The store then keeps each
//! session's whole conversation, so it survives a restart and can be shared
//! by several apps, while the window still decides what a request sends.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use rig_core::message::Message;
use rig_core::wasm_compat::{WasmBoxedFuture, WasmCompatSend, WasmCompatSync};
use rig_memory::recent::{Recent, view};
use rig_memory::{AsyncStore, SessionError, load_async, save_async};

/// The longest session id accepted by a persistent store, so it can name a
/// file or a row.
const MAX_SESSION_ID: usize = 128;
/// How often a save that lost a race with another writer is retried.
const SAVE_ATTEMPTS: usize = 3;

pub(crate) type HistoryError = Box<dyn std::error::Error + Send + Sync>;

/// A store of sessions' conversations by session id.
pub(crate) trait HistoryStore: WasmCompatSend + WasmCompatSync + 'static {
    /// Whether `session` can name a conversation in this store.
    fn accepts(&self, _session: &str) -> bool {
        true
    }

    /// The messages of `session` the store holds, oldest first; none for a
    /// new one. The caller applies the window.
    fn load<'a>(
        &'a self,
        session: &'a str,
    ) -> WasmBoxedFuture<'a, Result<Vec<Arc<Message>>, HistoryError>>;

    /// Add a finished run's `messages` to the end of `session`. `window` is
    /// what a request sends; a store that keeps only that much may trim to it.
    fn append<'a>(
        &'a self,
        session: &'a str,
        messages: Vec<Message>,
        window: &'a dyn Recent,
    ) -> WasmBoxedFuture<'a, Result<(), HistoryError>>;
}

/// Whether `session` can name a conversation in a persistent store: 1 to 128
/// ASCII letters, digits, `-` or `_`. The session id comes from a request
/// header, and a file store turns it into a path, so nothing else gets through.
pub(crate) fn valid_session_id(session: &str) -> bool {
    !session.is_empty()
        && session.len() <= MAX_SESSION_ID
        && session
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// Sessions in memory, bounded in count. Each keeps only what the window
/// sends, trimmed when a run is saved.
// ponytail: FIFO eviction by start time; switch to LRU if long-lived sessions get evicted while active.
pub(crate) struct InMemory(Mutex<Bounded>);

struct Bounded {
    by_session: HashMap<String, Vec<Arc<Message>>>,
    started: VecDeque<String>,
    max: usize,
}

impl InMemory {
    pub(crate) fn new(max: usize) -> Self {
        Self(Mutex::new(Bounded {
            by_session: HashMap::new(),
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
        session: &'a str,
    ) -> WasmBoxedFuture<'a, Result<Vec<Arc<Message>>, HistoryError>> {
        let messages = self
            .lock()
            .by_session
            .get(session)
            .cloned()
            .unwrap_or_default();
        Box::pin(async move { Ok(messages) })
    }

    fn append<'a>(
        &'a self,
        session: &'a str,
        messages: Vec<Message>,
        window: &'a dyn Recent,
    ) -> WasmBoxedFuture<'a, Result<(), HistoryError>> {
        let mut bounded = self.lock();
        if !bounded.by_session.contains_key(session) {
            bounded.started.push_back(session.to_owned());
            while bounded.started.len() > bounded.max {
                if let Some(oldest) = bounded.started.pop_front() {
                    bounded.by_session.remove(&oldest);
                }
            }
        }
        let conversation = bounded.by_session.entry(session.to_owned()).or_default();
        conversation.extend(messages.into_iter().map(Arc::new));
        // Keep only what a request would send (never splitting a tool call from
        // its result), so memory stays bounded by the window.
        let kept = view(conversation, window).len();
        conversation.drain(..conversation.len() - kept);
        Box::pin(async { Ok(()) })
    }
}

/// Sessions in a [`rig_memory`] store, one session log per session id.
pub(crate) struct Persistent<S>(pub(crate) S);

impl<S> Persistent<S> {
    /// The session log of `session`, kept apart from other sessions in the store.
    fn log(session: &str) -> String {
        format!("agentcore-{session}")
    }
}

impl<S> HistoryStore for Persistent<S>
where
    S: AsyncStore + WasmCompatSend + WasmCompatSync + 'static,
{
    fn accepts(&self, session: &str) -> bool {
        valid_session_id(session)
    }

    fn load<'a>(
        &'a self,
        session: &'a str,
    ) -> WasmBoxedFuture<'a, Result<Vec<Arc<Message>>, HistoryError>> {
        Box::pin(async move {
            let conversation = load_async(&self.0, &Self::log(session)).await?;
            Ok(conversation.messages().to_vec())
        })
    }

    fn append<'a>(
        &'a self,
        session: &'a str,
        messages: Vec<Message>,
        _window: &'a dyn Recent,
    ) -> WasmBoxedFuture<'a, Result<(), HistoryError>> {
        Box::pin(async move {
            let log = Self::log(session);
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

/// A session id for a call that came without one (outside AgentCore).
pub(crate) fn local_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "local-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(test)]
mod tests {
    use rig_memory::HeuristicTokenCounter;
    use rig_memory::recent::{LastMessages, TokenBudget};

    use super::*;

    #[tokio::test]
    async fn a_session_keeps_only_its_newest_history_within_the_window() {
        // Every message costs the same (400 bytes), so the budget holds exactly
        // ten: runs 195 to 199, each a prompt and a reply.
        let filler = "x".repeat(392);
        let cost =
            HeuristicTokenCounter::default().count(&Message::user(format!("run 000 {filler}")));
        let window = TokenBudget::new(cost * 10 + cost / 2);
        let store = InMemory::new(1);
        for run in 0..200 {
            store
                .append(
                    "session",
                    vec![
                        Message::user(format!("run {run:03} {filler}")),
                        Message::assistant(format!("reply {run:03} {filler}")),
                    ],
                    &window,
                )
                .await
                .unwrap();
        }
        let kept = store.load("session").await.unwrap();
        assert_eq!(kept.len(), 10);
        assert!(
            kept.first()
                .is_some_and(|m| format!("{m:?}").contains("run 195"))
        );
        assert!(
            kept.last()
                .is_some_and(|m| format!("{m:?}").contains("reply 199"))
        );
    }

    #[tokio::test]
    async fn an_empty_window_keeps_no_history() {
        let store = InMemory::new(1);
        store
            .append("session", vec![Message::user("hello")], &LastMessages(0))
            .await
            .unwrap();
        assert!(store.load("session").await.unwrap().is_empty());
    }

    #[test]
    fn session_ids_that_could_escape_a_file_store_are_refused() {
        assert!(valid_session_id("019a2b3c-4d5e-7f00-8a9b-0c1d2e3f4a5b"));
        assert!(valid_session_id("user_42"));
        for bad in ["", "../etc", "a/b", "a.b", "a b", &"x".repeat(129)] {
            assert!(!valid_session_id(bad), "{bad:?}");
        }
    }
}
