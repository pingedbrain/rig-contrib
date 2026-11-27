//! The conversations of the sessions an app has served.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use rig_core::message::Message;
use rig_memory::recent::{Recent, view};

/// Each session's messages, forgetting the oldest session past `capacity`
/// and the messages of a session that its `window` no longer keeps.
pub(crate) struct Sessions {
    capacity: usize,
    window: Arc<dyn Recent>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    conversations: HashMap<String, Vec<Arc<Message>>>,
    order: VecDeque<String>,
}

impl Sessions {
    pub(crate) fn new(capacity: usize, window: Arc<dyn Recent>) -> Self {
        Self {
            capacity: capacity.max(1),
            window,
            state: Mutex::new(State::default()),
        }
    }

    pub(crate) fn capacity(&self) -> usize {
        self.capacity
    }

    pub(crate) fn window(&self) -> Arc<dyn Recent> {
        Arc::clone(&self.window)
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The session's conversation so far.
    pub(crate) fn history(&self, session: &str) -> Vec<Message> {
        self.state()
            .conversations
            .get(session)
            .map(|messages| messages.iter().map(|message| (**message).clone()).collect())
            .unwrap_or_default()
    }

    /// Add a run's messages to the session's conversation.
    pub(crate) fn append(&self, session: &str, messages: Vec<Message>) {
        let mut state = self.state();
        if !state.conversations.contains_key(session) {
            state.order.push_back(session.to_owned());
            while state.order.len() > self.capacity {
                if let Some(oldest) = state.order.pop_front() {
                    state.conversations.remove(&oldest);
                }
            }
        }
        let conversation = state.conversations.entry(session.to_owned()).or_default();
        conversation.extend(messages.into_iter().map(Arc::new));
        // Keep only what a request would send (never splitting a tool call from
        // its result), so memory stays bounded by the window.
        let kept = view(conversation, self.window.as_ref()).len();
        conversation.drain(..conversation.len() - kept);
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

    #[test]
    fn a_session_keeps_only_its_newest_history_within_the_window() {
        // Every message costs the same (400 bytes), so the budget holds exactly
        // ten: runs 195 to 199, each a prompt and a reply.
        let filler = "x".repeat(392);
        let cost =
            HeuristicTokenCounter::default().count(&Message::user(format!("run 000 {filler}")));
        let sessions = Sessions::new(1, Arc::new(TokenBudget::new(cost * 10 + cost / 2)));
        for run in 0..200 {
            sessions.append(
                "session",
                vec![
                    Message::user(format!("run {run:03} {filler}")),
                    Message::assistant(format!("reply {run:03} {filler}")),
                ],
            );
        }
        let kept = sessions.history("session");
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

    #[test]
    fn an_empty_window_keeps_no_history() {
        let sessions = Sessions::new(1, Arc::new(LastMessages(0)));
        sessions.append("session", vec![Message::user("hello")]);
        assert!(sessions.history("session").is_empty());
    }
}
