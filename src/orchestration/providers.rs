//! Provider taxonomy and dispatch traits.
//!
//! The [`Provider`] enum and its *pure* facts (identity, capabilities,
//! model/effort catalog) live in `bro-core` so the thin fleet client can name
//! providers and render selectors without linking the daemon crate. This module
//! re-exports them and owns the *daemon-logic* surface: the `ProviderExec` /
//! `ProviderEvents` / `ProviderMcp` / `ProviderSession` traits (impl'd for
//! `Provider` in the submodules), whose bodies reach daemon-internal types.
//! Bring them all into scope with one glob via [`dispatch_prelude`]. See
//! `design/bro-harness/harness-process-boundary.md` (contract bottom +
//! thin-client decoupling).

mod events;
mod exec_args;
mod mcp_args;
mod session;
#[cfg(test)]
mod tests;

use bro_core::ProviderLane;
pub use bro_core::{Capability, Provider};

/// Whether the executor that owns the worker's stdout writes the session log.
/// A vendor CLI lane's child keeps no log under `BRO_HOME`; the harness writes
/// its own.
pub fn supervisor_writes_event_log(lane: ProviderLane) -> bool {
    match lane {
        ProviderLane::ClaudeCli | ProviderLane::Codex => true,
        ProviderLane::Harness | ProviderLane::Workflow => false,
    }
}

/// Whether the worker runs until its stdin closes, so the daemon drops the
/// control lane after each turn's `result`. The harness exits when idle on
/// its own (`--exit-when-idle`).
pub fn closes_input_after_result(lane: ProviderLane) -> bool {
    match lane {
        ProviderLane::ClaudeCli | ProviderLane::Codex => true,
        ProviderLane::Harness | ProviderLane::Workflow => false,
    }
}

/// Whether the worker mints its own session id when a session starts. The
/// codex app-server assigns the thread id, which then becomes the session id,
/// so a fresh dispatch on that lane starts `pending` and adopts the id the
/// worker's first event carries.
pub fn worker_assigns_session_id(lane: ProviderLane) -> bool {
    match lane {
        ProviderLane::Codex => true,
        ProviderLane::ClaudeCli | ProviderLane::Harness | ProviderLane::Workflow => false,
    }
}

pub use events::{AssistantPreview, Disruption, EventSink, Usage};
pub use exec_args::{
    ExecOpts, ProviderLaunch, dispatch_path_env, exec_opts_with_provider_defaults, resolve_bin,
};
#[cfg(test)]
use mcp_args::MatchState;
pub use mcp_args::{codex_fleet_mcp_servers, fleet_mcp_args};

/// Bring every provider dispatch trait into scope with one glob import:
/// `use crate::orchestration::providers::dispatch_prelude::*;`
///
/// The traits are reachable only through this prelude (and direct submodule
/// paths); they are intentionally not re-exported at the `providers` root, so
/// the single canonical import path is the prelude glob.
pub mod dispatch_prelude {
    pub use super::events::ProviderEvents;
    pub use super::exec_args::ProviderExec;
    pub use super::mcp_args::ProviderMcp;
    pub use super::session::ProviderSession;
}
