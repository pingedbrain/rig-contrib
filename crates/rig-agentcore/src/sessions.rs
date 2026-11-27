//! The conversations of the sessions an app has served.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use rig_core::message::Message;
use rig_memory::recent::{TokenBudget, view};

/// Each session's messages, forgetting the oldest session past `capacity`
/// and the oldest messages of a session past `history_tokens`.
pub(crate) struct Sessions {
    capacity: usize,
    history_tokens: usize,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    conversations: HashMap<String, Vec<Arc<Message>>>,
    order: VecDeque<String>,
}

impl Sessions {
    pub(crate) fn new(capacity: usize, history_tokens: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            history_tokens,
            state: Mutex::new(State::default()),
        }
    }

    pub(crate) fn capacity(&self) -> usize {
        self.capacity
    }

    pub(crate) fn history_tokens(&self) -> usize {
        self.history_tokens
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
        // Keep only what a request would send (the newest within the budget,
        // never splitting a tool call from its result), so memory stays bounded.
        let kept = view(conversation, &TokenBudget::new(self.history_tokens)).len();
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

    use super::*;

    #[test]
    fn a_session_keeps_only_its_newest_history_within_the_budget() {
        // Every message costs the same, so the budget holds exactly ten.
        let filler = "x".repeat(397);
        let cost = HeuristicTokenCounter::default().count(&Message::user(format!("000{filler}")));
        let budget = cost * 10 + cost / 2;
        let sessions = Sessions::new(1, budget);
        for run in 0..200 {
            let prompt = format!("{run:03}{filler}");
            sessions.append(
                "session",
                vec![Message::user(prompt), Message::assistant(filler.as_str())],
            );
        }
        let kept = sessions.history("session");
        assert!(kept.len() <= 10, "kept {} messages", kept.len());
        assert_eq!(kept.len(), 10);
        assert!(matches!(kept.last(), Some(Message::Assistant { .. })));
    }
}
