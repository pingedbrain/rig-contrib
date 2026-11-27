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
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};

use rig_agent::Agent;
use rig_core::conversation::pairing::safe_start;
use rig_core::message::{Message, UserContent};
use rig_core::wasm_compat::{WasmBoxedFuture, WasmCompatSend, WasmCompatSync};
use rig_memory::recent::{Recent, view};
use rig_memory::{
    AsyncStore, SUMMARIZATION_SYSTEM_PROMPT, SessionError, SummaryState, apply_summary, load_async,
    save_async, serialize_conversation,
};
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

    /// Replace the messages `read` (the first of `session`, as the caller
    /// loaded them) with one summary message. `None`, changing nothing, when
    /// the session no longer starts with `read`. Otherwise the session's
    /// messages after the change.
    fn replace_head<'a>(
        &'a self,
        session: &'a str,
        read: &'a [Arc<Message>],
        summary: &'a SummaryState,
    ) -> WasmBoxedFuture<'a, Result<Option<Vec<Arc<Message>>>, HistoryError>>;
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

    fn replace_head<'a>(
        &'a self,
        session: &'a str,
        read: &'a [Arc<Message>],
        summary: &'a SummaryState,
    ) -> WasmBoxedFuture<'a, Result<Option<Vec<Arc<Message>>>, HistoryError>> {
        let mut bounded = self.lock();
        let replaced = bounded
            .by_session
            .get_mut(session)
            .and_then(|conversation| {
                let same = conversation
                    .get(..read.len())?
                    .iter()
                    .zip(read)
                    .all(|(now, read)| Arc::ptr_eq(now, read));
                if !same || read.is_empty() {
                    return None;
                }
                let text = summary.message()?;
                conversation.drain(..read.len());
                conversation.insert(0, Arc::new(Message::user(text)));
                Some(conversation.clone())
            });
        Box::pin(async { Ok(replaced) })
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

    fn replace_head<'a>(
        &'a self,
        session: &'a str,
        read: &'a [Arc<Message>],
        summary: &'a SummaryState,
    ) -> WasmBoxedFuture<'a, Result<Option<Vec<Arc<Message>>>, HistoryError>> {
        Box::pin(async move {
            let log = Self::log(session);
            let mut conversation = load_async(&self.0, &log).await?;
            // The store's own messages, so rig-memory's splice (which needs the
            // same messages it was given) can apply; a session that no longer
            // starts with `read` is left alone.
            let replaces: Vec<Arc<Message>> = {
                let now = conversation.messages();
                let same = now
                    .get(..read.len())
                    .is_some_and(|now| now.iter().zip(read).all(|(now, read)| **now == **read));
                match now.get(..read.len()) {
                    Some(head) if same && !read.is_empty() => head.to_vec(),
                    _ => return Ok(None),
                }
            };
            if apply_summary(&mut conversation, &replaces, summary).is_none() {
                return Ok(None);
            }
            save_async(&self.0, &log, &mut conversation).await?;
            Ok(Some(conversation.messages().to_vec()))
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

/// The summarizing window of [`AgentCoreApp::summarize_history`](crate::AgentCoreApp::summarize_history).
#[derive(Clone)]
pub(crate) struct Summarizing {
    pub(crate) summarizer: Agent,
    pub(crate) keep_recent: usize,
    pub(crate) trigger_messages: usize,
}

/// Why a session's history was not summarized. Only logged: the caller then
/// sends the full history.
#[derive(Debug, thiserror::Error)]
enum SummarizeError {
    #[error("the summarizer failed: {0}")]
    Run(String),
    #[error("the summarizer returned no text")]
    Empty,
    #[error("the summary could not be stored: {0}")]
    Store(String),
    #[error("the conversation changed while it was summarized")]
    Changed,
}

/// A probe summary whose text marks where the summary text falls in its frame.
const PROBE: &str = "\u{1}";

/// The text every summary message opens with: rig-memory's frame, before the summary.
static OPENING: LazyLock<String> = LazyLock::new(|| {
    SummaryState {
        summary: PROBE.to_owned(),
        ..SummaryState::default()
    }
    .message()
    .unwrap_or_default()
    .split(PROBE)
    .next()
    .unwrap_or_default()
    .to_owned()
});

/// Whether `message` is a summary this module wrote (or rig-memory's splice did).
pub(crate) fn is_summary(message: &Message) -> bool {
    let Message::User { content } = message else {
        return false;
    };
    matches!(
        content.first(),
        Some(UserContent::Text(text)) if text.text.starts_with(OPENING.as_str())
    )
}

/// What a session keeps when a summarizer is set: the window's messages, and
/// at least the newest `floor` messages, so a summarizer that keeps failing
/// cannot grow a session without bound.
pub(crate) struct Retained<'a> {
    pub(crate) window: &'a dyn Recent,
    pub(crate) floor: usize,
}

impl Recent for Retained<'_> {
    fn start(&self, messages: &[Arc<Message>]) -> usize {
        self.window
            .start(messages)
            .min(messages.len().saturating_sub(self.floor))
    }
}

/// The request text for a summary of `read`, the messages it replaces.
fn summary_prompt(read: &[Arc<Message>]) -> String {
    format!(
        "{SUMMARIZATION_SYSTEM_PROMPT}\n\n<conversation>\n{}\n</conversation>\n\n\
         Summarize the conversation above in plain prose and short lists: the user's goals, \
         decisions made, facts and names later turns depend on, and tasks still open. If it \
         begins with an earlier summary, carry its facts forward.",
        serialize_conversation(read)
    )
}

/// Replace the older part of `messages` (all but the newest `keep_recent`, cut
/// so no tool result is separated from its call) with one summary in `store`,
/// and return the session's messages after it. `None` when nothing is older
/// than the newest messages or the summary failed; the failure is logged and
/// the history is left as it was.
pub(crate) async fn summarize(
    store: &dyn HistoryStore,
    summarizing: &Summarizing,
    session: &str,
    messages: &[Arc<Message>],
) -> Option<Vec<Arc<Message>>> {
    let cut = safe_start(
        messages,
        messages.len().saturating_sub(summarizing.keep_recent),
    );
    let read = messages.get(..cut).filter(|read| !read.is_empty())?;
    match try_summarize(store, summarizing, session, read).await {
        Ok(messages) => Some(messages),
        Err(error) => {
            tracing::warn!(error = %error, "could not summarize the conversation; keeping its full history");
            None
        }
    }
}

async fn try_summarize(
    store: &dyn HistoryStore,
    summarizing: &Summarizing,
    session: &str,
    read: &[Arc<Message>],
) -> Result<Vec<Arc<Message>>, SummarizeError> {
    let response = summarizing
        .summarizer
        .prompt(summary_prompt(read))
        .run()
        .await
        .map_err(|failure| SummarizeError::Run(failure.to_string()))?;
    let text = response.output();
    let text = text.trim();
    if text.is_empty() {
        return Err(SummarizeError::Empty);
    }
    let state = SummaryState {
        summary: text.to_owned(),
        ..SummaryState::default()
    };
    store
        .replace_head(session, read, &state)
        .await
        .map_err(|failure| SummarizeError::Store(failure.to_string()))?
        .ok_or(SummarizeError::Changed)
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
