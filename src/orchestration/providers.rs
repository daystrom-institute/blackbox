//! Provider taxonomy and dispatch traits.
//!
//! The [`Provider`] enum and its *pure* facts (identity, capabilities,
//! model/effort catalog) live in `bro-core` so the thin fleet client can name
//! providers and render selectors without linking the daemon crate. This module
//! re-exports them and owns the *daemon-logic* surface: the `ProviderExec` /
//! `ProviderEvents` / `ProviderMcp` / `ProviderSession` traits (impl'd for
//! `Provider` in the submodules), whose bodies reach daemon-internal types.
//! Bring them all into scope with one glob via [`dispatch_prelude`]. See
//! `design/bro-harness/harness-daemon-boundary.md` §2/§7 (contract bottom +
//! thin-client decoupling).

mod events;
mod exec_args;
mod mcp_args;
mod session;
#[cfg(test)]
mod tests;

pub use bro_core::{Capability, Provider};

pub use events::{Disruption, EventSink, Usage};
pub use exec_args::{ExecOpts, dispatch_path_env, exec_opts_with_provider_defaults, resolve_bin};
#[cfg(test)]
use mcp_args::MatchState;

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
