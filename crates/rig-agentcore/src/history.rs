//! Where an app keeps sessions, and how much of one a request sends.
//!
//! By default sessions live in memory, at most `max_sessions` of them, and
//! each keeps only the newest messages its window allows, so memory stays
//! bounded. [`AgentCoreApp::history_store`](crate::AgentCoreApp::history_store)
//! keeps them in any [`rig_memory`] store instead (`MemoryStore`, `DirStore`
//! on disk, `rig-sqlite`, `rig-postgres`). The store then keeps each
//! session's whole conversation, so it survives a restart and can be shared
//! by several apps, while the window still decides what a request sends.
//! Whatever the session id, its log has a safe name (see [`Persistent`]).

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use rig_core::message::Message;
use rig_core::wasm_compat::{WasmBoxedFuture, WasmCompatSend, WasmCompatSync};
use rig_memory::recent::{Recent, view};
use rig_memory::{AsyncStore, SessionError, load_async, save_async};
use sha2::{Digest, Sha256};

/// The longest session id whose own characters name its log. Longer or
/// other ids are hashed.
const MAX_LOG_ID: usize = 128;
/// How often a save that lost a race with another writer is tried in all.
const SAVE_ATTEMPTS: u32 = 10;

pub(crate) type HistoryError = Box<dyn std::error::Error + Send + Sync>;

/// A store of sessions' conversations by session id.
pub(crate) trait HistoryStore: WasmCompatSend + WasmCompatSync + 'static {
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

/// Whether `session` can name its log as it is: 1 to 128 ASCII letters,
/// digits, `-` or `_`, so a file store can use it as a path part.
fn names_its_log(session: &str) -> bool {
    !session.is_empty()
        && session.len() <= MAX_LOG_ID
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
    /// The log of `session`, kept apart from other sessions in the store. A
    /// session id that names its log as it is keeps its characters. Any
    /// other (AgentCore sends ids of 33 to 256 characters, of anything) is
    /// hashed, so no id can escape a file store. The two prefixes keep a
    /// kept id and a hash from ever sharing a name.
    fn log(session: &str) -> String {
        if names_its_log(session) {
            format!("agentcore-s-{session}")
        } else {
            let digest = Sha256::digest(session.as_bytes());
            let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
            format!("agentcore-h-{hex}")
        }
    }
}

impl<S> HistoryStore for Persistent<S>
where
    S: AsyncStore + WasmCompatSend + WasmCompatSync + 'static,
{
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
            for attempt in 1..=SAVE_ATTEMPTS {
                // Another writer may have saved since this one read; the store
                // then refuses the save whole, so reread and add on top.
                let mut conversation = load_async(&self.0, &log).await?;
                conversation.extend(messages.iter().cloned());
                match save_async(&self.0, &log, &mut conversation).await {
                    Ok(()) => return Ok(()),
                    Err(SessionError::Changed { .. }) if attempt < SAVE_ATTEMPTS => {
                        tokio::time::sleep(backoff(attempt)).await;
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            Err(format!("the log {log} changed under {SAVE_ATTEMPTS} saves").into())
        })
    }
}

/// How long a save waits before its `attempt`-th retry: a few milliseconds,
/// growing, with a jitter from the clock so writers that collided do not
/// collide again in step.
fn backoff(attempt: u32) -> std::time::Duration {
    let jitter = u64::from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos())
            % 8,
    );
    std::time::Duration::from_millis(u64::from(attempt) * 2 + jitter)
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
    fn every_session_id_names_a_safe_log() {
        assert_eq!(
            Persistent::<()>::log("019a2b3c-4d5e-7f00-8a9b-0c1d2e3f4a5b"),
            "agentcore-s-019a2b3c-4d5e-7f00-8a9b-0c1d2e3f4a5b"
        );
        // Escapes, separators and a 256-character id all become a fixed-length hash.
        for odd in ["../etc", "a/b", "a.b", "a b", &"x".repeat(256)] {
            let log = Persistent::<()>::log(odd);
            assert!(log.starts_with("agentcore-h-"), "{odd:?}");
            assert_eq!(log.len(), "agentcore-h-".len() + 64, "{odd:?}");
            assert!(
                log["agentcore-h-".len()..]
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit()),
                "{odd:?}"
            );
        }
        // A hashed id can never equal a kept one, even one that looks like a hash.
        assert_eq!(Persistent::<()>::log("h-kept"), "agentcore-s-h-kept");
        assert_ne!(
            Persistent::<()>::log("a/b"),
            Persistent::<()>::log("a_b"),
            "distinct ids keep distinct logs"
        );
    }
}
