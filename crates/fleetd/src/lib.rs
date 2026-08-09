//! `fleetd`: the per-machine fleet supervisor.
//!
//! Slice 5 of `design/daemon-runtime/locality-first-decomposition.md`. The
//! daemon composes a fully-resolved [`bro_protocol::WorkerSpawnSpec`] and
//! hands it over a narrow typed local RPC; fleetd executes and supervises the
//! child, relays its stdio event/control lanes, and serves a bounded replay
//! window so live sessions survive daemon restarts.
//!
//! The invariant that gives this binary its reason to exist: **fleetd never
//! re-derives policy.** It reads no brofile, no credential store, no config
//! beyond its own socket paths. Policy is decided centrally by the daemon and
//! enforced here by construction. That is what makes it small enough to change
//! a few times a year, which is the whole point: a daemon rebuild-and-restart
//! must not drop live sessions.
//!
//! See `AGENTS.md` in this crate for the dependency ceiling, the spawn-parity
//! notes against the daemon's `LocalExecutor`, and the accepted v1 limits.

pub mod paths;
pub mod registry;
pub mod replay;
pub mod server;
pub mod spawn;
pub mod workspace;

pub use paths::{FleetdPaths, default_state_dir};
pub use registry::{Registry, SessionEntry};
pub use server::{Fleetd, bind_listener, build_identity, serve, serve_connection};
