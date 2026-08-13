//! The producer-side connector interface (design section 6).
//!
//! No corpus type appears in it. A connector's entire job is delivering bytes
//! with stable logical identity and honest change signals; everything
//! downstream of byte acquisition already exists on the corpus host.
//!
//! Deliberate omissions, both inherited from the design: no write, delete, or
//! permission method (the read-only invariant is structural, not a default),
//! and no watch or subscription API (vendor push notifications are a latency
//! optimization that changes nothing about the manifest contract).

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use async_trait::async_trait;

/// Bound on one checkpoint stream name and on one opaque checkpoint token.
pub const MAX_CHECKPOINT_NAME_BYTES: usize = 64;
pub const MAX_CHECKPOINT_TOKEN_BYTES: usize = 8192;
/// Bound on how many independent enumeration streams one source may carry.
pub const MAX_CHECKPOINTS: usize = 64;

/// A NAMED SET of change cursors, rather than one opaque blob.
///
/// This is the shape choice worth explaining. The design calls for a
/// "connector-opaque incremental-enumeration token"; every real store the
/// catalog names actually has more than one. Drive keeps a changes token per
/// drive, Graph a delta link per driveItem root, S3 a continuation token per
/// prefix. Collapsing those into a single opaque blob would force each
/// connector to invent a private encoding for a set, and, worse, would make
/// the invalidation of ONE stream indistinguishable from the invalidation of
/// all of them: a single expired Drive token would force a full re-enumeration
/// of every other drive in the scope.
///
/// So a checkpoint set is `stream name -> opaque token`, the names are the
/// connector's own, and nothing outside the connector interprets either half.
/// An EMPTY set means "enumerate fully"; that is the only meaning the
/// satellite assigns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckpointSet {
    checkpoints: BTreeMap<String, String>,
}

impl CheckpointSet {
    /// The empty set: a full enumeration.
    pub fn full_enumeration() -> Self {
        Self::default()
    }

    pub fn is_full_enumeration(&self) -> bool {
        self.checkpoints.is_empty()
    }

    pub fn insert(&mut self, name: impl Into<String>, token: impl Into<String>) -> Result<()> {
        let name = name.into();
        let token = token.into();
        validate_checkpoint_name(&name)?;
        if token.is_empty()
            || token.len() > MAX_CHECKPOINT_TOKEN_BYTES
            || token.bytes().any(|byte| byte == 0)
        {
            bail!("checkpoint token for {name} is empty, oversized, or carries NUL");
        }
        if !self.checkpoints.contains_key(&name) && self.checkpoints.len() >= MAX_CHECKPOINTS {
            bail!("a connector source may carry at most {MAX_CHECKPOINTS} checkpoint streams");
        }
        self.checkpoints.insert(name, token);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.checkpoints.get(name).map(String::as_str)
    }

    pub fn remove(&mut self, name: &str) -> Option<String> {
        self.checkpoints.remove(name)
    }

    pub fn len(&self) -> usize {
        self.checkpoints.len()
    }

    pub fn is_empty(&self) -> bool {
        self.checkpoints.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.checkpoints
            .iter()
            .map(|(name, token)| (name.as_str(), token.as_str()))
    }

    /// A short, stable rendering for the descriptor's `remote_watermark`.
    ///
    /// Display and diagnostic only, and named that way on the wire: the
    /// corpus must never treat a vendor's version token as freshness
    /// authority, so this string exists to let an operator SEE where
    /// enumeration stands and nothing else.
    pub fn watermark(&self) -> String {
        if self.checkpoints.is_empty() {
            return "full-enumeration".to_string();
        }
        let rendered = self
            .checkpoints
            .iter()
            .map(|(name, token)| format!("{name}:{}", short_token(token)))
            .collect::<Vec<_>>()
            .join(",");
        rendered
            .char_indices()
            .take_while(|(index, _)| *index < bbox_file_source::MAX_REMOTE_WATERMARK_BYTES - 8)
            .map(|(_, ch)| ch)
            .collect()
    }
}

fn short_token(token: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(token.as_bytes()))[..8].to_string()
}

fn validate_checkpoint_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > MAX_CHECKPOINT_NAME_BYTES
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        bail!("checkpoint name {name:?} must be a bounded alphanumeric token");
    }
    Ok(())
}

/// What a remote entry IS, as the store reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteEntryKind {
    File,
    Directory,
    /// A provider-native document with no canonical bytes (a Google Doc, a
    /// Sheet, a SharePoint list). It must be EXPORTED before it has bytes to
    /// hash, and its size is therefore unknown before fetching.
    NativeDocument,
}

/// One entry as enumeration metadata describes it, before any fetch.
///
/// Policy runs on exactly this, which is what makes "policy exclusion prevents
/// fetching, not merely indexing" true rather than aspirational.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEntry {
    /// Connector-stable identity. The journal's key; never a path.
    pub remote_id: String,
    /// Raw, untrusted remote name components from the scope root down. The
    /// satellite derives a logical path from these and never trusts them as
    /// one.
    pub name_path: Vec<String>,
    pub kind: RemoteEntryKind,
    /// etag, revision, or changestamp at enumeration.
    pub remote_version: String,
    /// Known for ordinary files; `None` for a native document, whose export
    /// size cannot be bounded before fetching.
    pub size: Option<u64>,
    pub media_type: Option<String>,
    /// A renderable URL for the document, when the store has one.
    pub remote_url: Option<String>,
    /// A tombstone from an incremental walk: the entry left the scope.
    pub deleted: bool,
}

/// One page of change: entries plus where enumeration now stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeBatch {
    pub entries: Vec<RemoteEntry>,
    pub next_checkpoints: CheckpointSet,
    /// True when this batch enumerated the WHOLE scope rather than a delta.
    ///
    /// Load-bearing: orphan detection (a journal entry the remote no longer
    /// has) is only sound against a complete enumeration. Treating a delta as
    /// complete would delete every unchanged document from the corpus on the
    /// first incremental cycle.
    pub complete: bool,
}

/// The connector's typed "your cursor is gone" signal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointInvalidation {
    /// Which enumeration stream expired. Named so an operator can see WHICH
    /// drive or prefix is churning, not merely that something did.
    pub checkpoint_name: String,
    pub cause: String,
}

/// The outcome of one observation.
///
/// Invalidation is a value rather than an error because it is an expected
/// operating condition (Drive and Graph both expire change tokens on their own
/// schedule), not a failure. Modelling it as an error would put it on the same
/// footing as a revoked credential and invite a catch-all handler to swallow
/// it, which is exactly the "silently absorbed degradation" the design
/// forbids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observation {
    Batch(ChangeBatch),
    CheckpointInvalidated(CheckpointInvalidation),
}

/// Bytes a connector produced for one entry, plus what it produced them as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedContent {
    pub bytes: Vec<u8>,
    /// `Some(extension)` when this is a provider-native export. It REPLACES
    /// whatever extension the remote name carried, because the corpus chunks
    /// the export.
    pub export_format: Option<String>,
    pub media_type: Option<String>,
}

/// A fetch either produced bytes or was refused, with a counted reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchOutcome {
    Fetched(FetchedContent),
    /// The streamed cap fired mid-transfer, or the store refused. Counted per
    /// reason and reported; never silently dropped.
    Skipped(String),
}

/// Remote identity facts probed for onboarding and status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteInfo {
    /// The vendor tenant or account. Checked against the operator's grant.
    pub remote_authority: String,
    /// The store's own id for the scope root, when it has one.
    pub remote_root_id: Option<String>,
    pub remote_display_name: Option<String>,
}

/// One adapter for one remote store kind.
#[async_trait]
pub trait RemoteSourceConnector: Send + Sync {
    /// The connector kind, matching the operator's declared `connector_kind`.
    fn kind(&self) -> &'static str;

    /// Validate config plus credentials and return remote identity facts.
    /// Fails closed with remediation text.
    async fn validate(&self) -> Result<RemoteInfo>;

    /// An empty checkpoint set is a full walk; a populated one resumes
    /// incrementally. Returns entries plus the next set, or a typed
    /// invalidation signal.
    async fn observe(&self, checkpoints: &CheckpointSet) -> Result<Observation>;

    /// A bounded byte read that aborts mid-transfer at `max_bytes`.
    ///
    /// The cap is a parameter rather than connector state because export size
    /// is unknown before fetching: metadata screening cannot bound an export,
    /// so this is the only cap in the system that can fire mid-transfer.
    async fn fetch(&self, entry: &RemoteEntry, max_bytes: u64) -> Result<FetchOutcome>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_checkpoint_set_is_named_bounded_and_defaults_to_full_enumeration() {
        let mut set = CheckpointSet::full_enumeration();
        assert!(set.is_full_enumeration());
        assert_eq!(set.watermark(), "full-enumeration");

        set.insert("drive-root", "token-a").unwrap();
        set.insert("shared-drive-2", "token-b").unwrap();
        assert!(!set.is_full_enumeration());
        assert_eq!(set.len(), 2);
        assert_eq!(set.get("drive-root"), Some("token-a"));

        // One expired stream invalidates one stream, not the whole set.
        set.remove("drive-root");
        assert_eq!(set.len(), 1);
        assert!(set.get("shared-drive-2").is_some());
    }

    #[test]
    fn checkpoint_names_and_tokens_fail_closed() {
        let mut set = CheckpointSet::full_enumeration();
        for name in ["", "has space", "slash/name", &"n".repeat(65)] {
            assert!(set.insert(name, "token").is_err(), "{name:?}");
        }
        assert!(set.insert("ok", "").is_err());
        assert!(
            set.insert("ok", "a".repeat(MAX_CHECKPOINT_TOKEN_BYTES + 1))
                .is_err()
        );
        assert!(set.insert("ok", "tok\0en").is_err());

        for index in 0..MAX_CHECKPOINTS {
            set.insert(format!("stream-{index}"), "token").unwrap();
        }
        assert!(
            set.insert("one-too-many", "token").is_err(),
            "checkpoint cardinality is bounded"
        );
        // Replacing an existing stream at the ceiling is still allowed.
        set.insert("stream-0", "token-2").unwrap();
    }

    #[test]
    fn a_watermark_is_bounded_and_leaks_no_token() {
        let mut set = CheckpointSet::full_enumeration();
        for index in 0..MAX_CHECKPOINTS {
            set.insert(format!("stream-{index}"), format!("secret-token-{index}"))
                .unwrap();
        }
        let watermark = set.watermark();
        assert!(watermark.len() <= bbox_file_source::MAX_REMOTE_WATERMARK_BYTES);
        assert!(
            !watermark.contains("secret-token"),
            "a watermark is a digest of enumeration state, never the tokens themselves"
        );
    }
}
