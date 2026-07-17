//! `RemoteSourceConnector`: the pluggable adapter trait for one remote
//! store kind. Deliberate omissions: no `write`/`delete` (read-only
//! invariant - the trait has no mutating methods, so callers cannot even
//! request mutation), no watch/subscription API (polling via cursors only).

use std::path::Path;

use anyhow::Result;
use async_trait::async_trait;

use crate::manifest::Manifest;
use crate::types::{
    ChangeBatch, ChangeCursor, FetchedContent, MountConfig, RemoteEntry, RemoteInfo,
};

/// Outcome of a bulk materialization pass (see
/// [`RemoteSourceConnector::materialize_bulk`]). `upserts` carries fully
/// materialized-or-skipped manifest entries the driver should record;
/// `deleted_remote_ids` names entries the driver should drop from the
/// manifest (the connector already removed their bytes from `root`).
#[derive(Debug, Clone, Default)]
pub struct BulkMaterializeOutcome {
    pub upserts: Vec<crate::manifest::ManifestEntry>,
    pub deleted_remote_ids: Vec<String>,
}

#[async_trait]
pub trait RemoteSourceConnector: Send + Sync {
    /// Connector kind discriminator, e.g. "git", "gdrive", "webdav".
    fn kind(&self) -> &'static str;

    /// Validate config + credentials; return remote identity/quota info for
    /// status surfaces. Fail-closed with remediation text in the error.
    async fn validate(&self, mount: &MountConfig) -> Result<RemoteInfo>;

    /// Enumerate entries. `cursor=None` is a full walk of the scope;
    /// `Some` resumes incrementally. Returns changes + the next cursor.
    async fn list_changes(
        &self,
        mount: &MountConfig,
        cursor: Option<ChangeCursor>,
    ) -> Result<ChangeBatch>;

    /// Fetch one entry's bytes. For provider-native documents the connector
    /// applies its export mapping and reports the exported media type.
    /// Must behave honestly even for connectors that normally bulk-
    /// materialize (see `bulk_materializes`) - callers (tests, ad hoc
    /// re-fetch) may still invoke this directly.
    async fn fetch(&self, mount: &MountConfig, entry: &RemoteEntry) -> Result<FetchedContent>;

    /// Stage-1 seam beyond the base four-method contract: `true` for
    /// connectors whose sync mechanics already produce a full local tree
    /// (a git clone/fetch/reset) rather than a stream of discrete blobs, so
    /// driving them through the generic per-entry fetch loop would be
    /// redundant round-trips against the connector's own local state.
    /// Default `false` - the generic driver loop calls `fetch` per changed
    /// entry.
    fn bulk_materializes(&self) -> bool {
        false
    }

    /// Materialize the whole changed tree directly into `root` when
    /// `bulk_materializes()` is `true`. Receives the current `manifest` so
    /// the connector can preserve `remote_id` continuity across renames it
    /// detects internally (e.g. git's `--find-renames` diff). Never called
    /// when `bulk_materializes()` is `false`.
    async fn materialize_bulk(
        &self,
        _mount: &MountConfig,
        _root: &Path,
        _manifest: &Manifest,
        _batch: &ChangeBatch,
    ) -> Result<BulkMaterializeOutcome> {
        anyhow::bail!(
            "materialize_bulk not implemented for a connector reporting \
             bulk_materializes() = true"
        )
    }
}
