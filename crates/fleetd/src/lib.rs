//! Standalone live execution service.
//!
//! `fleet-core` owns durable state machines. This crate adds service hosting,
//! a private worker socket, process launch, materialized roster views, and
//! fail-closed capability clients.

mod authority_actor;
mod capability;
mod closeout;
mod closeout_driver;
mod compatibility;
mod config;
mod control;
mod error;
mod launch;
mod mcp;
mod projection;
mod provider;
mod records;
mod roster;
mod sandbox;
mod service;
mod shadow;
mod worker;
mod worktree;

pub use capability::{CapabilityBroker, HttpCapabilityBroker, UnavailableCapabilityBroker};
pub use config::{FleetMode, FleetdConfig};
pub use error::{FleetdError, FleetdResult};
pub use launch::{LaunchSpec, LaunchedWorker, ProcessWorkerLauncher, WorkerLauncher};
pub use service::{Fleetd, FleetdHandle};
pub use shadow::{ParityMismatch, ParityReport, ShadowReplica};
