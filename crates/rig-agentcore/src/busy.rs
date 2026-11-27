//! The work an app has open, which decides whether `/ping` says `Healthy` or
//! `HealthyBusy`.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

/// The open work of one app, and when its status last changed.
#[derive(Debug)]
pub(crate) struct Busy {
    state: Mutex<State>,
}

#[derive(Debug)]
struct State {
    open: usize,
    /// Unix seconds of the last status change: the app's start, or the moment
    /// the open count went from zero to one or back to zero.
    since: u64,
}

impl Busy {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                open: 0,
                since: now(),
            }),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Count one more piece of open work, held until the guard drops.
    pub(crate) fn open(this: &Arc<Self>) -> BusyGuard {
        {
            let mut state = this.lock();
            if state.open == 0 {
                state.since = now();
            }
            state.open += 1;
        }
        BusyGuard {
            busy: Arc::clone(this),
        }
    }

    /// Whether any work is open, and when the status last changed.
    pub(crate) fn status(&self) -> (bool, u64) {
        let state = self.lock();
        (state.open > 0, state.since)
    }
}

/// Keeps the app's `/ping` at `HealthyBusy` while it is alive. Get one from
/// [`AgentCoreApp::track_task`](crate::AgentCoreApp::track_task). The work is
/// released when the guard drops, which also happens when a task holding it is
/// aborted or panics.
#[must_use = "dropping the guard at once ends the busy status immediately"]
#[derive(Debug)]
pub struct BusyGuard {
    busy: Arc<Busy>,
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        let mut state = self.busy.lock();
        state.open = state.open.saturating_sub(1);
        if state.open == 0 {
            state.since = now();
        }
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}
