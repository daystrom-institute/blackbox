//! Reaction execution runtime for system events — the upward-coupled half
//! of the system-events surface. The contract half (event types, store,
//! hub, outbox, gating, identity, templates) lives in `crate::system_events`
//! (the bbox-system-events crate); this module owns everything that needs
//! `SharedState`, the workflow engine, or orchestration dispatch: the
//! worker loop, reaction executors, and the Forgejo built-in atoms.
mod executors;
mod forgejo;
#[cfg(test)]
mod integration_tests;
pub mod worker;
