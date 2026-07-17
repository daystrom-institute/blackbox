//! Core connector types: mount configuration, remote entry identity, change
//! batches and cursors, fetched content, and remote validation info.
//!
//! None of these types carry credential material. `MountConfig::options` is
//! a free-form string map for connector-specific non-secret knobs (shallow
//! depth, etc.) - never a home for tokens.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::policy::MountPolicy;

/// Configuration handed to a connector for one mount pass. Connectors
/// resolve credentials ambiently (ssh agent, credential helper, a token the
/// operator already embedded in the scope URL) or, in later phases, through
/// the secrets layer - never from a field on this struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountConfig {
    /// Connector kind discriminator, e.g. "git".
    pub connector_kind: String,
    /// Connector-shaped scope string. For `git`: a clone URL plus optional
    /// `#<ref>` suffix (default: the remote's default branch).
    pub remote_scope: String,
    /// Absolute local materialization root this mount syncs into.
    pub materialization_root: PathBuf,
    /// Include/exclude/size policy applied at enumeration - exclusion here
    /// prevents fetching, not merely indexing.
    #[serde(default)]
    pub policy: MountPolicy,
    /// Free-form connector options. Never a home for credentials.
    #[serde(default)]
    pub options: BTreeMap<String, String>,
}

/// What kind of thing a [`RemoteEntry`] refers to.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RemoteEntryKind {
    File,
    Dir,
    /// A provider-native document with no canonical bytes (Google Doc,
    /// Sheet, Slides, ...); the connector applies an export map to produce
    /// bytes a downstream chunker can claim.
    NativeDoc,
}

/// One enumerated remote object: stable identity, scope-relative path,
/// size, remote version, kind, and media type when the store knows it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteEntry {
    /// Connector-stable identity. Stable across renames for stores with a
    /// native persistent file id; for path-identity stores (git) the driver
    /// reconciles renames by carrying the OLD manifest remote_id forward
    /// rather than trusting a freshly minted one (see `driver::reconcile`).
    pub remote_id: String,
    /// Scope-relative logical path (faithful, not yet encoded for the local
    /// filesystem).
    pub path: String,
    pub size: Option<u64>,
    /// etag / revision / commit-oid / mtime - whatever the store's change
    /// signal is.
    pub remote_version: String,
    pub kind: RemoteEntryKind,
    pub media_type: Option<String>,
}

/// One change relative to the prior enumeration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum RemoteChangeKind {
    Added,
    Modified,
    Deleted,
    /// `from_path` is the prior scope-relative logical path. The driver
    /// reconciles this as a manifest move (same remote_id, new
    /// logical_path) instead of delete+refetch.
    Renamed {
        from_path: String,
    },
}

/// One entry in a [`ChangeBatch`]. For `Deleted`, `entry` carries the
/// last-known metadata (size/remote_version may be stale - only `remote_id`
/// and `path` are load-bearing for reconciliation).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteChange {
    pub entry: RemoteEntry,
    pub kind: RemoteChangeKind,
}

/// Connector-opaque incremental-enumeration token. `None` means a full walk
/// of the scope.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ChangeCursor(pub String);

/// Result of one `list_changes` call.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChangeBatch {
    pub changes: Vec<RemoteChange>,
    /// Cursor to persist once this batch's manifest updates are committed.
    /// `None` from a connector that has no incremental cursor concept
    /// (unusual; the git connector always returns `Some`).
    pub next_cursor: Option<ChangeCursor>,
    /// Set when the connector could not resume from the supplied cursor
    /// (an expired Drive/Graph token, a git ref no longer reachable, ...)
    /// and fell back to a full walk. The driver must surface this in
    /// `SyncSummary::degradations`, never absorb it silently.
    #[serde(default)]
    pub degraded_to_full_walk: bool,
    /// True whenever `changes` represents every currently-live entry in
    /// the scope - a normal `cursor=None` first sync, an operator-forced
    /// full resync, or a cursor-invalidation degradation. Distinct from
    /// `degraded_to_full_walk`: this is set on ordinary first syncs too,
    /// where nothing degraded. `bulk_materializes()` connectors use this
    /// to reconcile orphaned manifest entries (files present in a prior
    /// enumeration but absent from this one - e.g. after a mount's target
    /// ref switches) that an `Added`-only change list cannot express.
    #[serde(default)]
    pub is_full_walk: bool,
}

/// Bytes fetched for one entry. Bounded: the driver enforces
/// `MountPolicy::max_file_bytes` against the returned length (v1's
/// approximation of a true streamed hard cap - see AGENTS.md).
#[derive(Debug, Clone)]
pub struct FetchedContent {
    pub bytes: Vec<u8>,
    pub media_type: Option<String>,
    /// Set when the connector applied an export mapping (native-doc ->
    /// office/pdf format); the materialized filename should carry this
    /// extension instead of the source kind's.
    pub export_format: Option<String>,
}

/// Result of `RemoteSourceConnector::validate` - remote identity/quota info
/// for status surfaces, and for git, the resolved ref/sha.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RemoteInfo {
    pub display_name: String,
    /// Resolved ref or revision (e.g. a resolved git sha) when the
    /// connector's scope names a symbolic target.
    pub resolved_ref: Option<String>,
    #[serde(default)]
    pub detail: BTreeMap<String, String>,
}
