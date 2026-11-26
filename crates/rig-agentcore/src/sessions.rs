//! The conversations of the sessions an app has served.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use rig_core::message::Message;

/// Each session's messages, forgetting the oldest session past `capacity`.
pub(crate) struct Sessions {
    capacity: usize,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    conversations: HashMap<String, Vec<Message>>,
    order: VecDeque<String>,
}

impl Sessions {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            state: Mutex::new(State::default()),
        }
    }

    pub(crate) fn capacity(&self) -> usize {
        self.capacity
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The session's conversation so far.
    pub(crate) fn history(&self, session: &str) -> Vec<Message> {
        self.state()
            .conversations
            .get(session)
            .cloned()
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
        state
            .conversations
            .entry(session.to_owned())
            .or_default()
            .extend(messages);
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
