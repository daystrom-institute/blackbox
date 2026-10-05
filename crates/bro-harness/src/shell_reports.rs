//! Reports of the session's shell sessions to whoever supervises the worker.
//!
//! The daemon can see that a worker is quiet but not why. A worker blocked in
//! a long shell command looks the same as one that stopped. This publishes
//! the shell registry's state on the event plane so an idle report can say
//! which commands are still running.
//!
//! The contract, shared with the daemon side:
//!
//! - Reports are change-driven. One is sent when the process starts (an empty
//!   set for a new process, including a resumed session's), and after that
//!   only when a shell session starts, ends or is removed. There is no timer.
//! - Each report is a complete snapshot, so the newest one is the whole
//!   truth and nothing has to be replayed.
//! - One task publishes them, and it reads the registry after the change
//!   that woke it. Reports therefore leave in the order their snapshots were
//!   taken, and a burst of changes may collapse into one report.
//! - A snapshot identical to the last one sent is not sent again, apart from
//!   ages: a command that starts and is consumed before the publisher wakes
//!   leaves the picture unchanged.
//! - The registry lock is held only while the snapshot is copied out. The
//!   emitter is never called under it.
//!
//! The emitter stamps each event with the session's sequence and then writes
//! it, without one lock around both, so two emitters can put lines on stdout
//! out of sequence order. A supervisor must order reports by `seq`, not by
//! arrival; the daemon does.

use crate::emit::Emitter;
use bro_tools::{ShellSessionSummary, ShellSessions};
use std::sync::{Arc, Mutex};

/// The running publisher. Dropping it stops the task.
pub struct ShellReports {
    task: Option<tokio::task::JoinHandle<()>>,
}

impl ShellReports {
    /// Send the initial report now, then publish every later change.
    pub fn start(registry: &Arc<Mutex<ShellSessions>>, emitter: Emitter) -> Self {
        let changes = registry.lock().unwrap().changes();
        // Read the counter before the snapshot: a change that lands between
        // the two is then seen as newer and reported.
        let mut seen = changes.version();
        let mut last = snapshot(registry);
        emitter.shell_sessions(&last);
        let registry = Arc::downgrade(registry);
        let task = tokio::spawn(async move {
            loop {
                seen = changes.changed(seen).await;
                let Some(registry) = registry.upgrade() else {
                    return;
                };
                let rows = snapshot(&registry);
                drop(registry);
                if same_picture(&rows, &last) {
                    continue;
                }
                emitter.shell_sessions(&rows);
                last = rows;
            }
        });
        Self { task: Some(task) }
    }

    /// A publisher that sends nothing, for sessions assembled by hand in
    /// tests.
    pub fn inert() -> Self {
        Self { task: None }
    }
}

impl Drop for ShellReports {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

fn snapshot(registry: &Mutex<ShellSessions>) -> Vec<ShellSessionSummary> {
    registry.lock().unwrap().summaries()
}

/// Whether two snapshots describe the same sessions in the same state. Age
/// alone is not a change.
fn same_picture(a: &[ShellSessionSummary], b: &[ShellSessionSummary]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(a, b)| a.id == b.id && a.running == b.running && a.command == b.command)
}
