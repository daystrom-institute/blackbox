//! Satellite transcript collector: a slim log-shipper that tails interactive
//! provider transcript roots (claude, codex) on a source machine and ships
//! inline-payload byte increments to the corpus host's `POST /internal/records`.
//!
//! Peeled deliberately thin (remote-corpus-host design, slice 2c:
//! `design/daemon-runtime/remote-corpus-host.md`). The dependency tree carries
//! NO tantivy, NO v8, NO `bbox-corpus-index`: discovery and the durable cursor
//! model come from the tantivy-free `bbox-transcript-read` reading layer, the
//! wire contract from `bro-capabilities`.
//!
//! Delivery is at-least-once with the corpus server as the cursor authority.
//! A local sidecar caches per-stream byte tails for fast resume, but on startup
//! (and after any gap/overlap rejection) the collector resyncs by POSTing an
//! empty ingest batch and adopting the server's acknowledged tails. See
//! [`shipper::Shipper`] and `crates/bbox-collector/AGENTS.md`.

pub mod client;
pub mod config;
pub mod cursor;
pub mod prefix;
pub mod record;
pub mod shipper;

pub use client::{IngestClient, IngestError};
pub use config::{AccountRoot, CollectorConfig};
pub use shipper::{Shipper, TickSummary};

use std::sync::Arc;

use tokio::sync::Notify;

/// Build a shipper from a resolved config and run its tick loop until the
/// provided shutdown notifier fires. Thin wrapper used by the binary.
pub async fn run(config: CollectorConfig, shutdown: Arc<Notify>) -> anyhow::Result<()> {
    let mut shipper = Shipper::from_config(config)?;
    shipper.run(shutdown).await
}
