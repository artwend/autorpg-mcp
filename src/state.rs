//! In-memory session state shared between the MCP tools and the capture thread.

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

/// Live game metrics tracked by the MCP session.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct GameMetrics {
    pub player_hp: i32,
    pub stamina: i32,
    pub q_ready: bool,
    pub r_ready: bool,
    pub f_ready: bool,
    pub location: String,
}

/// A single logged action with the metrics snapshot taken at that moment.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SessionEvent {
    pub timestamp: u64,
    pub action_taken: String,
    pub state_snapshot: GameMetrics,
}

/// Full session state: current metrics plus the event history.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct SessionState {
    pub current_metrics: GameMetrics,
    pub event_history: Vec<SessionEvent>,
}

/// Maximum number of events retained in the session history.
/// Older events are dropped first, so the log cannot grow without bound.
pub const MAX_EVENT_HISTORY: usize = 512;

impl SessionState {
    /// Appends an event, snapshotting the current metrics, and evicts the oldest
    /// entry once [`MAX_EVENT_HISTORY`] is reached.
    pub fn record_event(&mut self, action_taken: impl Into<String>) {
        if self.event_history.len() >= MAX_EVENT_HISTORY {
            self.event_history.remove(0);
        }

        self.event_history.push(SessionEvent {
            timestamp: unix_timestamp(),
            action_taken: action_taken.into(),
            state_snapshot: self.current_metrics.clone(),
        });
    }
}

/// Thread-safe pointer for state management (async readers/writers).
pub type SharedSession = Arc<RwLock<SessionState>>;

/// Latest compressed screenshot shared between the capture thread and the MCP tools.
pub type SharedFrameBuffer = Arc<Mutex<Option<Vec<u8>>>>;

/// Current UNIX timestamp in seconds (0 if the clock is before the epoch).
pub fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
