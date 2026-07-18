//! Remote-source connectors: pluggable adapters onto remote file/document
//! stores so drives, folders, and repos mount as registered, indexable
//! blackbox projects. Materialize-then-index architecture -
//! `design/connectors/remote-source-connectors.md`.
//!
//! Stage 1 (this crate today) is library-only: the `RemoteSourceConnector`
//! trait, the shared sync driver, the manifest/policy/materialization
//! machinery, the `MountRecord` store, and the `git` connector. No daemon
//! wiring, MCP tools, or poller integration - that is stage 2. See
//! `AGENTS.md` for crate-level invariants and footguns.

pub mod connector;
pub mod driver;
pub mod git_connector;
pub mod graph_projection;
pub mod manifest;
pub mod materialize;
pub mod mount_record;
pub mod policy;
pub mod types;

pub use connector::{BulkMaterializeOutcome, RemoteSourceConnector};
pub use driver::{SyncSummary, sync_mount};
pub use git_connector::GitConnector;
pub use graph_projection::{
    GraphProjection, ObservationBatch, ObservationRecord, ProjectionContext,
};
pub use manifest::{EntryState, Manifest, ManifestEntry};
pub use mount_record::{
    MountRecord, MountStore, compute_mount_id, default_materialization_root,
    default_mount_store_path,
};
pub use policy::{BudgetExceeded, MountPolicy, PolicyDecision, TotalBytesBudget};
pub use types::{
    ChangeBatch, ChangeCursor, FetchedContent, MountConfig, RemoteChange, RemoteChangeKind,
    RemoteEntry, RemoteEntryKind, RemoteInfo,
};
