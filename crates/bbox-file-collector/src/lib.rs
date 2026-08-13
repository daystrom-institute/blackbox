//! The connector satellite: a producer-host binary that observes a remote
//! store, applies policy, exports provider-native documents, and publishes
//! manifests plus blobs over the collector wire.
//!
//! Two properties are the point of the whole design, and both are properties
//! of where this code RUNS rather than of what it does:
//!
//! - **The daemon never fetches remote bytes.** It holds no vendor credential
//!   and opens no socket to a third-party cloud. The only new network trust
//!   edge is producer-host-to-vendor, where the OAuth refresh token has to
//!   live anyway.
//! - **The daemon never materializes.** No daemon-local mount root, no project
//!   whose identity is a filesystem path. Where a connector wants a local
//!   cache, that cache is producer working state: losable, re-derivable, never
//!   corpus authority.
//!
//! This crate is a DUMB PRODUCER. Everything downstream of byte acquisition
//! already exists on the corpus host: the chunker registry claims PDF, the
//! office family, XLSX, IPYNB, HTML, AV transcripts, and standalone images,
//! with visual payloads content-hash-addressed. A connector's entire job is
//! delivering bytes with stable logical identity and honest change signals.
//!
//! The dependency ceiling in `Cargo.toml` is enforced mechanically. Exactly
//! one chunker version exists in the system, on the corpus host, so a
//! satellite deploy can never skew against the index.
//!
//! Module map:
//!
//! - [`connector`] -- the `RemoteSourceConnector` trait and its shared types,
//!   including the named [`connector::CheckpointSet`];
//! - [`policy`] -- include/exclude globs, byte caps, the native-export
//!   toggle, and the per-source total that aborts loudly;
//! - [`journal`] -- producer working state keyed on `remote_id`;
//! - [`logical`] -- collision-safe logical-path assignment;
//! - [`fixture`] -- the filesystem-backed fixture connector (no network, no
//!   OAuth, no vendor code);
//! - [`cycle`] -- one publication cycle (observe, decide, acquire, publish),
//!   written against a sink trait so its decisions are testable without a
//!   socket;
//! - [`client`] -- the `/internal/file-source/v1/*` wire client;
//! - [`config`] -- the operator-declared satellite configuration the binary
//!   loads.

pub mod client;
pub mod config;
pub mod connector;
pub mod cycle;
pub mod fixture;
pub mod journal;
pub mod logical;
pub mod policy;

pub use client::FileSourceClient;
pub use config::SatelliteConfig;
pub use connector::{
    ChangeBatch, CheckpointInvalidation, CheckpointSet, FetchOutcome, FetchedContent, Observation,
    RemoteEntry, RemoteEntryKind, RemoteInfo, RemoteSourceConnector,
};
pub use cycle::{CycleOutcome, PublicationSink, run_publication_cycle};
pub use fixture::{FIXTURE_KIND, FixtureConnector};
pub use journal::{Journal, JournalEntry, JournalState};
pub use logical::{AssignmentInput, assign_logical_paths};
pub use policy::{CompiledPolicy, PolicyDecision, SkipCounters, SourcePolicy};
