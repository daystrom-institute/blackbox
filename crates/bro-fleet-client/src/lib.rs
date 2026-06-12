//! `bro-fleet-client` — the daemon-driving engine behind `bro fleet`.
//!
//! The fleet cockpit (`bro-cli`) links this crate, not `blackbox`. It holds the
//! `FleetOrchestrator` that POSTs `/control/*` to the daemon singleton, the
//! in-memory roster projection, the stream-json transcript parser, and the
//! `fleet.json` config types — all over the contract bottom
//! (`bro-protocol` + `bro-core`) plus transport deps, never the daemon crate
//! (harness-daemon-boundary.md §7).

// Match the `blackbox` crate idiom: opt out of edition-2024's stylistic
// collapsible_if so the engine code ported from `orchestration::fleet` stays
// verbatim (nested guards kept as-is).
#![allow(clippy::collapsible_if)]

mod config;
mod fleet;
mod mcp;
mod tail;
mod task;

pub use bro_protocol::{
    CloseoutErrorClass, CloseoutHooksWire, CloseoutOutcome, CloseoutPhase, CloseoutRequest,
    PhaseResult, SERVICE_TIER_DEFAULT, SERVICE_TIER_PRIORITY,
};
pub use config::{bro_home, daemon_port};
pub use fleet::{
    AgentHandle, CLASSIFIER_NAME_PREFIX, ClassifierConfig, CloseoutEvent,
    DEFAULT_CLASSIFIER_PROMPT, DispatchSpec, FleetConfig, FleetOrchestrator, HookOnFail,
    HookPolicy, INTERN_PREFIX, ProjectCloseout, ProjectDispatch, Provider, ResumeSpec,
    TaskSnapshot, TaskStatus, TodoItem, TodoItemStatus, TodoState, TranscriptItem, intern_rider,
    parse_transcript, provider_supports_bidi, seed_worktree_dirs,
};
pub use mcp::McpServerConfig;
pub use tail::TailEvent;

/// Serialize tests that mutate process-global env (e.g. `BLACKBOX_CONFIG`).
/// Non-reentrant — do not double-take.
#[cfg(test)]
pub(crate) fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}
