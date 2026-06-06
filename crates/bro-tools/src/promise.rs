//! Stream-progress tracking shared by long-running producers (the shell
//! yield/`shell_poll` path) so status responses can expose elapsed runtime,
//! last-output recency, and stdout/stderr byte counts without polling the
//! producer.
//!
//! History: this module once hosted the harness-local Promise push-system
//! (`PromiseStore` + `promise_*` tools + auto-injected `HARNESS_EVENT` turns).
//! That was retired in favor of codex's pull model — long-running work yields
//! and is resumed by the model via `shell_poll` (and code-mode `wait`) — so
//! only the progress-tracking infra survives here.

use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Which output stream a reader belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Stdout,
    Stderr,
}

/// Progress tracking shared between a producer (e.g. shell readers) and the
/// status snapshot so `shell_poll`/`shell_list` can expose running-progress
/// heartbeat metadata without polling the producer.
#[derive(Debug)]
pub struct PromiseProgress {
    /// Timestamp (ms) of the last output write from the producer, or 0.
    pub last_output_at_ms: AtomicU64,
    /// Cumulative bytes written to stdout.
    pub stdout_bytes: AtomicU64,
    /// Cumulative bytes written to stderr.
    pub stderr_bytes: AtomicU64,
}

impl PromiseProgress {
    pub fn new() -> Self {
        Self {
            last_output_at_ms: AtomicU64::new(0),
            stdout_bytes: AtomicU64::new(0),
            stderr_bytes: AtomicU64::new(0),
        }
    }

    pub fn heartbeat(&self, kind: StreamKind, n: usize) {
        self.last_output_at_ms.store(now_ms(), Ordering::Relaxed);
        match kind {
            StreamKind::Stdout => {
                self.stdout_bytes.fetch_add(n as u64, Ordering::Relaxed);
            }
            StreamKind::Stderr => {
                self.stderr_bytes.fetch_add(n as u64, Ordering::Relaxed);
            }
        }
    }

    /// Build a json! snapshot of progress suitable for inclusion in a running
    /// shell-session status response, given the producer's wall-clock start.
    /// Before any output, byte counters read 0 and `last_output_at_ms` is 0
    /// (rendered as "no output yet" by consumers).
    pub fn snapshot(&self, started_ms: u64) -> Value {
        let now = now_ms();
        let elapsed = now.saturating_sub(started_ms);
        let last_at = self.last_output_at_ms.load(Ordering::Relaxed);
        let last_elapsed = if last_at > 0 {
            now.saturating_sub(last_at)
        } else {
            0
        };
        json!({
            "elapsed_ms": elapsed,
            "last_output_at_ms": last_at,
            "last_output_elapsed_ms": last_elapsed,
            "stdout_bytes": self.stdout_bytes.load(Ordering::Relaxed),
            "stderr_bytes": self.stderr_bytes.load(Ordering::Relaxed),
        })
    }
}

impl Default for PromiseProgress {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
