use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// Real-time events emitted during game preparation, downloading, mod-loader patching,
/// and Minecraft process execution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LaunchEvent {
    Check {
        current: usize,
        total: usize,
        category: String,
    },
    Progress {
        downloaded: u64,
        total: u64,
        element: String,
    },
    Speed {
        bytes_per_sec: u64,
    },
    Estimated {
        seconds_remaining: f64,
    },
    Extract {
        message: String,
    },
    Patch {
        line: String,
    },
    Data {
        line: String,
    },
    Close {
        exit_code: i32,
    },
    Error {
        message: String,
    },
}

/// Thread-safe callback invoked with [`LaunchEvent`] updates.
pub type EventCallback = Arc<dyn Fn(LaunchEvent) + Send + Sync + 'static>;

/// Convenience helper returning a no-op [`EventCallback`].
pub fn noop_event_callback() -> EventCallback {
    Arc::new(|_| {})
}
