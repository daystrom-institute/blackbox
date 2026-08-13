//! Dependency-clean wire contract for connector-sourced CONVERSATIONS.
//!
//! This is the append-only lane of the connector family: the leaf crate the
//! daemon and the conversation producer satellite both link, so neither side
//! can drift from the other's idea of a message record. The conversation it
//! describes mounts under `/internal/conversation-source/v1/*`:
//!
//! ```text
//! POST /internal/conversation-source/v1/catalog/onboard   mint the catalog project
//! POST /internal/conversation-source/v1/channels          roster visible under policy
//! GET  /internal/conversation-source/v1/cursors           server's per-channel high-water marks
//! POST /internal/conversation-source/v1/batches           ordered batch of new message records
//! POST /internal/conversation-source/v1/revisions         edits and tombstones for landed records
//! GET  /internal/conversation-source/v1/status            per-channel lag, last batch, tombstones
//! ```
//!
//! `design/connectors/slack-ingestion-connector.md` sections 2, 4, and 5 own
//! the argument; this crate owns the shapes.
//!
//! # S0 gate: this wire against the code-source / file-source wire
//!
//! The S0 review is "shapes reviewed against the code-source wire". Here it is,
//! one line per axis, because every difference below is a consequence of the
//! ONE decision in design section 2: Slack is an append-only API dataset, not a
//! mutable tree of named blobs.
//!
//! | Axis | code / file source (manifest lane) | conversation source (this lane) |
//! |---|---|---|
//! | Scope | [`ConnectorScope`] (file) / `PublishedScope` (code) | [`ConnectorScope`], phase 0, UNCHANGED. There is no conversation-shaped scope family and minting one would fork the grant table. |
//! | Publication quantum | a generation: a coherent whole-set snapshot | a batch: an ordered run of new records on ONE channel. There is no snapshot boundary in a channel and no reason to hide new messages until one completes. |
//! | Identity | `(scope, logical_path)` within a generation | `(workspace_id, channel_id, message_ts, revision)`, workspace-level. Two observers of one channel converge on one record rather than two (design 4.2). |
//! | Fingerprint | `manifest_sha256` over the whole ordered entry set | `batch_digest` over the ordered records of ONE batch. There is no whole-set digest to compute; paging channel history to derive one is the cost design section 2 refuses. |
//! | Freshness authority | `content_sha256`; vendor version tokens are never freshness | `message_ts` plus `revision`; the producer's own edit timestamps are never freshness. |
//! | Cursor ownership | server derives `generation_id`; producer holds no durable cursor | server owns the per-channel high-water mark; producer ASKS where to resume. Same invariant, different quantum. |
//! | Deletion | falls out of the manifest diff, exactly, bought with full enumeration | an explicit [`ConversationTombstoneRecordV1`], because full re-enumeration is a paginated rate-limited sweep. Detection is window-bounded and design 5.4 says so out loud. |
//! | Edits | a changed `content_sha256` is a new entry in the next generation | an explicit [`ConversationRevisionRecordV1`] superseding IN PLACE on stable identity, so an edit updates rather than duplicating. |
//! | Bytes | content-addressed blob CAS, PUT per hash | none. Records carry raw text inline under a hard cap; attachment BYTES are future work and only REFERENCES cross this wire. |
//! | Activation | stage, install, flip, retain across two stores | none. There is no generation to activate; a landed record is immediately part of the channel. |
//! | Telemetry | `PublicationTelemetryV1` skip counters per generation | per-channel lag and counts on the status read, because a throttled producer shows up as LAG, not as errors (design 5.5). |
//! | Auth | bearer `ServiceToken`, scope allowlist checked before any durable write | identical, verbatim, including the 404-over-403 privacy posture. |
//!
//! Shared by construction: the `ServiceToken` bearer transport, the
//! operator-minted [`ConnectorScope`], the pending-onboarding admission, the
//! `deny_unknown_fields` strictness, and the rule that a producer's claim never
//! introduces a scope the operator did not already write into config.
//!
//! Deliberately NOT shared: this crate does not depend on `bbox-file-source`.
//! The two lanes have exactly one type in common and it lives one level down in
//! `bbox-corpus-core`. A dependency edge here would put the manifest lane's
//! byte caps, path derivation, and generation model into the conversation
//! satellite's tree for no gain.
//!
//! # Relationship to `TranscriptCursor`
//!
//! Design section 2 names `TranscriptCursor`'s `ProviderEventId` and
//! `MessageIdSet` variants as the vocabulary a `ts` watermark and a set of
//! in-flight thread parents need, and that reading is correct:
//! [`ChannelCursorV1::history_watermark`] is a `ProviderEventId` and
//! [`ChannelCursorV1::inflight_thread_parents`] is a `MessageIdSet`.
//!
//! This crate does NOT reference that type. `TranscriptCursor` lives in
//! `bbox-corpus-index`, which pulls Tantivy, the chunker, and the whole
//! indexing stack; naming it here would break the satellite's dependency
//! ceiling on the first `use`. The correspondence is therefore structural and
//! is converted at the S2 projection boundary, corpus-side, where
//! `bbox-corpus-index` is already linked. The DTO carries the two shapes so
//! that conversion is total and mechanical rather than a re-derivation.
//!
//! # What this crate deliberately does NOT contain
//!
//! Any Slack API knowledge, any credential type, any chunker knowledge, any
//! rate-limit policy, and any document shaping. The producer ships records and
//! stops (design 3.2): it does not concatenate messages into documents, choose
//! thread windows, summarize, render markdown, or embed. Every
//! document-shaping decision stays corpus-side so exactly one chunker version
//! exists in the system.

use bbox_corpus_core::project_catalog::ConnectorScope;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Wire schema version for every `/internal/conversation-source/v1/*` payload.
pub const SCHEMA_VERSION: u32 = 1;

/// Producer-side policy version. Skew between satellite and server is a typed
/// rejection rather than a best-effort merge, mirroring the file lane. It
/// changes whenever record normalization, the caps below, or the batch digest
/// changes.
pub const CONVERSATION_POLICY_VERSION: &str = "conversation-source-connector-policy-v1";

/// Onboarding schema version, versioned independently of the ingest
/// conversation so the onboarding shape can move without a batch bump.
pub const CATALOG_ONBOARD_SCHEMA_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Caps
//
// Every one of these is enforced on BOTH sides: the satellite refuses to build
// an oversized payload and the daemon refuses to accept one. A cap enforced on
// one side only is a cap that a compromised or buggy producer does not have.
// ---------------------------------------------------------------------------

/// A workspace's opaque provider-issued id. Never parsed, never a name.
pub const MAX_WORKSPACE_ID_BYTES: usize = 128;
/// A channel's opaque provider-issued id. Never parsed, never a name.
pub const MAX_CHANNEL_ID_BYTES: usize = 128;
/// A message timestamp, the message's identity within its channel.
pub const MAX_MESSAGE_TS_BYTES: usize = 32;
/// An author's opaque provider-issued id.
pub const MAX_AUTHOR_ID_BYTES: usize = 128;
/// Raw message text. Generous relative to any provider's own composer limit,
/// because truncating a message corpus-side would be a silent lie about what
/// the workspace holds.
pub const MAX_TEXT_BYTES: usize = 64 * 1024;
/// A message subtype label (`bot_message`, `channel_join`, ...). Opaque.
pub const MAX_SUBTYPE_BYTES: usize = 64;
/// A reaction name, an attachment name, an observed display name.
pub const MAX_LABEL_BYTES: usize = 256;
/// An RFC 3339 observation stamp.
pub const MAX_TIMESTAMP_BYTES: usize = 64;
/// Reaction references carried on one record.
pub const MAX_REACTIONS_PER_RECORD: usize = 64;
/// Attachment references carried on one record.
pub const MAX_ATTACHMENTS_PER_RECORD: usize = 64;
/// Records in one batch. A batch is one channel's forward run, so this bounds
/// both the body and the amount of work one accepted batch commits.
pub const MAX_BATCH_RECORDS: usize = 500;
/// Byte ceiling the route layer applies to a batch body before parsing it.
pub const MAX_BATCH_BODY_BYTES: usize = 8 * 1024 * 1024;
/// Revision or tombstone records in one revisions request.
pub const MAX_REVISIONS_PER_REQUEST: usize = 500;
/// Byte ceiling the route layer applies to a revisions body.
pub const MAX_REVISIONS_BODY_BYTES: usize = 4 * 1024 * 1024;
/// Channel observations in one roster post.
pub const MAX_ROSTER_CHANNELS: usize = 5_000;
/// Byte ceiling the route layer applies to a roster body.
pub const MAX_ROSTER_BODY_BYTES: usize = 4 * 1024 * 1024;
/// In-flight thread parents the server reports back per channel. Bounded so a
/// pathological channel cannot make the cursor read unbounded.
pub const MAX_INFLIGHT_THREAD_PARENTS: usize = 1_024;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ConversationSourceError {
    #[error("unsupported conversation-source schema version {0}")]
    UnsupportedSchema(u32),
    #[error("conversation policy mismatch: received {received}, expected {expected}")]
    ConversationPolicyMismatch {
        received: String,
        expected: &'static str,
    },
    #[error("invalid connector scope: {0}")]
    InvalidScope(String),
    #[error("invalid conversation field: {0}")]
    InvalidField(&'static str),
    #[error("invalid message timestamp: {0}")]
    InvalidMessageTs(String),
    #[error("batch records are not strictly ordered by message_ts")]
    RecordsNotOrdered,
    #[error("batch carries duplicate message_ts {0}")]
    DuplicateMessageTs(String),
    #[error("roster carries duplicate channel_id {0}")]
    DuplicateChannelId(String),
    #[error("record names channel {actual}, batch declares {declared}")]
    ChannelMismatch { declared: String, actual: String },
    #[error("batch declares {actual} records, limit is {limit}")]
    TooManyRecords { actual: usize, limit: usize },
    #[error("batch declares {actual} bytes of text, limit is {limit}")]
    TooMuchText { actual: usize, limit: usize },
    #[error("batch digest does not match the declared records")]
    BatchDigestMismatch,
    #[error("a revision record must carry a revision greater than zero")]
    InvalidRevision,
    #[error("invalid onboard field: {0}")]
    InvalidOnboardField(&'static str),
}

// ---------------------------------------------------------------------------
// Shared validation
// ---------------------------------------------------------------------------

/// Opaque provider token: non-empty, bounded, free of NUL and control bytes.
///
/// Nothing parses these, so nothing more is demanded of them. That is the
/// discipline design 4.2 names: provider-issued ids are identity precisely
/// because the corpus never interprets them.
fn validate_opaque(value: &str, max_bytes: usize, field: &'static str) -> ConversationResult<()> {
    if value.is_empty()
        || value.len() > max_bytes
        || value
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control())
    {
        return Err(ConversationSourceError::InvalidField(field));
    }
    Ok(())
}

fn validate_optional_opaque(
    value: Option<&String>,
    max_bytes: usize,
    field: &'static str,
) -> ConversationResult<()> {
    match value {
        Some(value) => validate_opaque(value, max_bytes, field),
        None => Ok(()),
    }
}

type ConversationResult<T> = Result<T, ConversationSourceError>;

pub fn validate_scope(scope: &ConnectorScope) -> ConversationResult<()> {
    scope
        .validate()
        .map_err(|error| ConversationSourceError::InvalidScope(error.to_string()))
}

/// Validate a message timestamp: the message's identity within its channel.
///
/// The accepted shape is a decimal integer with an optional fractional part
/// (`1755000000.123456`), which is what a monotonic provider event id looks
/// like. It is deliberately NOT pinned to one provider's exact digit widths:
/// [`message_ts_order_key`] carries the ordering, so this only has to refuse
/// what could not possibly be a timestamp.
pub fn validate_message_ts(value: &str) -> ConversationResult<()> {
    let invalid = || ConversationSourceError::InvalidMessageTs(value.chars().take(64).collect());
    if value.is_empty() || value.len() > MAX_MESSAGE_TS_BYTES {
        return Err(invalid());
    }
    let mut parts = value.split('.');
    let seconds = parts.next().unwrap_or_default();
    let fraction = parts.next();
    if parts.next().is_some() {
        return Err(invalid());
    }
    if seconds.is_empty() || seconds.len() > 20 || !seconds.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    if let Some(fraction) = fraction
        && (fraction.is_empty()
            || fraction.len() > 12
            || !fraction.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(invalid());
    }
    Ok(())
}

/// A total order key for a message timestamp.
///
/// Lexicographic comparison of two keys equals numeric comparison of the two
/// timestamps. This exists because the store keys channels on `message_ts` and
/// a naive string compare gets `"999.1" > "1000.0"` wrong the first time a
/// provider changes its integer width. Zero-padding both halves to a fixed
/// width makes the byte order the numeric order, permanently.
///
/// Returns `None` for a timestamp [`validate_message_ts`] would refuse, so a
/// caller cannot silently order garbage.
pub fn message_ts_order_key(value: &str) -> Option<String> {
    validate_message_ts(value).ok()?;
    let mut parts = value.split('.');
    let seconds = parts.next().unwrap_or_default();
    let fraction = parts.next().unwrap_or("");
    Some(format!("{seconds:0>20}.{fraction:0<12}"))
}

/// True when `candidate` is strictly later than `current`.
pub fn message_ts_is_after(candidate: &str, current: &str) -> bool {
    match (
        message_ts_order_key(candidate),
        message_ts_order_key(current),
    ) {
        (Some(candidate), Some(current)) => candidate > current,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// The record
// ---------------------------------------------------------------------------

/// Who spoke, as a KIND rather than as a transcript role.
///
/// Design open question 3 flags collapsing authorship onto the transcript role
/// vocabulary as pragmatic but not obviously right, and this crate takes the
/// dedicated-field side: the role vocabulary describes turn KIND (user,
/// assistant, tool) and authorship is identity. Callers who care who spoke
/// filter on `author_id`; this field exists only so an app-versus-human
/// distinction survives without overloading role.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum AuthorKindV1 {
    Human,
    App,
    #[default]
    Unknown,
}

/// One reaction, as a REFERENCE.
///
/// The reactor list is deliberately absent. It is unbounded, it is the most
/// personally identifying part of a message, and no v1 retrieval surface reads
/// it. A count answers "was this noticed"; a name list would be a privacy
/// surface with no consumer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReactionRefV1 {
    pub name: String,
    pub count: u32,
}

impl ReactionRefV1 {
    pub fn validate(&self) -> ConversationResult<()> {
        validate_opaque(&self.name, MAX_LABEL_BYTES, "reaction.name")
    }
}

/// One attachment, as a REFERENCE and nothing more.
///
/// No bytes and no `url_private`. Bytes are future work on the
/// content-addressed blob lane (design section 9, "Future"), and a private
/// download URL is both credential-adjacent and expiring, so persisting one
/// would durably record a link that stops working the moment the workspace
/// deletes the file. The remote id survives, which is what a later blob pass
/// needs to fetch bytes at observation time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AttachmentRefV1 {
    pub remote_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

impl AttachmentRefV1 {
    pub fn validate(&self) -> ConversationResult<()> {
        validate_opaque(&self.remote_id, MAX_LABEL_BYTES, "attachment.remote_id")?;
        validate_optional_opaque(self.name.as_ref(), MAX_LABEL_BYTES, "attachment.name")?;
        validate_optional_opaque(
            self.mime_type.as_ref(),
            MAX_LABEL_BYTES,
            "attachment.mime_type",
        )
    }
}

/// One observed conversational turn.
///
/// `workspace_id` is NOT a field here. It rides the batch envelope exactly
/// once, for two reasons: a per-record copy could only ever disagree with the
/// envelope, and one channel's batch of 500 records would otherwise carry 500
/// identical copies of it. The durable key is still
/// `(workspace_id, channel_id, message_ts, revision)`; the store composes it
/// from the envelope and the record, and
/// [`ConversationBatchV1::validate`] refuses a record whose `channel_id`
/// disagrees with the envelope, which is the tamper check that makes composing
/// it safe.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConversationMessageRecordV1 {
    /// Provider-issued, opaque. Identity, never a name.
    pub channel_id: String,
    /// Monotonic within the channel; the message's identity there.
    pub message_ts: String,
    /// `0` for an original observation. A [`ConversationRevisionRecordV1`]
    /// lands a higher revision under the same identity.
    #[serde(default)]
    pub revision: u64,
    pub author_id: String,
    #[serde(default)]
    pub author_kind: AuthorKindV1,
    /// The thread parent's `message_ts`, present on a reply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_parent_ts: Option<String>,
    /// Provider subtype label, opaque and never interpreted here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtype: Option<String>,
    /// RAW text exactly as the provider returned it. Not rendered, not
    /// chunked, not summarized: producer discipline, design 3.2.
    pub text: String,
    /// The provider's own edit stamp when the observed message already carried
    /// one. Diagnostic; `revision` is the freshness authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited_ts: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reactions: Vec<ReactionRefV1>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<AttachmentRefV1>,
    /// When the producer observed this record.
    pub observed_at: String,
}

impl ConversationMessageRecordV1 {
    pub fn validate(&self) -> ConversationResult<()> {
        validate_opaque(&self.channel_id, MAX_CHANNEL_ID_BYTES, "channel_id")?;
        validate_message_ts(&self.message_ts)?;
        validate_opaque(&self.author_id, MAX_AUTHOR_ID_BYTES, "author_id")?;
        if let Some(parent) = &self.thread_parent_ts {
            validate_message_ts(parent)?;
        }
        validate_optional_opaque(self.subtype.as_ref(), MAX_SUBTYPE_BYTES, "subtype")?;
        validate_optional_opaque(self.edited_ts.as_ref(), MAX_TIMESTAMP_BYTES, "edited_ts")?;
        validate_opaque(&self.observed_at, MAX_TIMESTAMP_BYTES, "observed_at")?;
        if self.text.len() > MAX_TEXT_BYTES {
            return Err(ConversationSourceError::TooMuchText {
                actual: self.text.len(),
                limit: MAX_TEXT_BYTES,
            });
        }
        // NUL is refused; other control bytes are NOT, because a real message
        // legitimately contains newlines and tabs. This is the one field where
        // the opaque-token rule would destroy content.
        if self.text.bytes().any(|byte| byte == 0) {
            return Err(ConversationSourceError::InvalidField("text"));
        }
        if self.reactions.len() > MAX_REACTIONS_PER_RECORD {
            return Err(ConversationSourceError::InvalidField("reactions"));
        }
        if self.attachments.len() > MAX_ATTACHMENTS_PER_RECORD {
            return Err(ConversationSourceError::InvalidField("attachments"));
        }
        for reaction in &self.reactions {
            reaction.validate()?;
        }
        for attachment in &self.attachments {
            attachment.validate()?;
        }
        Ok(())
    }

    /// The message's stable identity within a workspace, ignoring revision.
    pub fn identity(&self, workspace_id: &str) -> ConversationMessageIdentityV1 {
        ConversationMessageIdentityV1 {
            workspace_id: workspace_id.to_string(),
            channel_id: self.channel_id.clone(),
            message_ts: self.message_ts.clone(),
        }
    }
}

/// The durable identity of one message, revision excluded.
///
/// Workspace-level on purpose (design 4.2): two bot identities installed in one
/// workspace are two SOURCES with two minted scopes, but where their visibility
/// overlaps their observations land on ONE record. Keying identity on the scope
/// instead would produce two documents for one message and a search hit count
/// that varied with how many observers happened to cover the channel.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(deny_unknown_fields)]
pub struct ConversationMessageIdentityV1 {
    pub workspace_id: String,
    pub channel_id: String,
    pub message_ts: String,
}

/// The full durable key: identity plus revision.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(deny_unknown_fields)]
pub struct ConversationRecordKeyV1 {
    pub workspace_id: String,
    pub channel_id: String,
    pub message_ts: String,
    pub revision: u64,
}

impl ConversationRecordKeyV1 {
    pub fn new(identity: &ConversationMessageIdentityV1, revision: u64) -> Self {
        Self {
            workspace_id: identity.workspace_id.clone(),
            channel_id: identity.channel_id.clone(),
            message_ts: identity.message_ts.clone(),
            revision,
        }
    }

    pub fn identity(&self) -> ConversationMessageIdentityV1 {
        ConversationMessageIdentityV1 {
            workspace_id: self.workspace_id.clone(),
            channel_id: self.channel_id.clone(),
            message_ts: self.message_ts.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Revisions and tombstones
// ---------------------------------------------------------------------------

/// An edit observed against an already-landed record.
///
/// It supersedes IN PLACE: same `(workspace_id, channel_id, message_ts)`, a
/// higher `revision`. That is what makes an edit an update rather than a
/// duplicate, and it is why identity deliberately excludes revision.
///
/// The APPLY semantics belong to S4. This crate and the landing store only
/// have to make the record durable, ordered, and unambiguous.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConversationRevisionRecordV1 {
    pub channel_id: String,
    pub message_ts: String,
    /// Strictly greater than zero: revision 0 is the original observation and
    /// an edit that claimed it would overwrite history rather than supersede
    /// it.
    pub revision: u64,
    /// The edited text as observed. Absent means the producer saw an edit stamp
    /// move without being able to read the new body, which is recorded rather
    /// than guessed at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// The provider's edit stamp that triggered this revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited_ts: Option<String>,
    pub observed_at: String,
}

impl ConversationRevisionRecordV1 {
    pub fn validate(&self) -> ConversationResult<()> {
        validate_opaque(&self.channel_id, MAX_CHANNEL_ID_BYTES, "channel_id")?;
        validate_message_ts(&self.message_ts)?;
        if self.revision == 0 {
            return Err(ConversationSourceError::InvalidRevision);
        }
        validate_optional_opaque(self.edited_ts.as_ref(), MAX_TIMESTAMP_BYTES, "edited_ts")?;
        validate_opaque(&self.observed_at, MAX_TIMESTAMP_BYTES, "observed_at")?;
        if let Some(text) = &self.text {
            if text.len() > MAX_TEXT_BYTES {
                return Err(ConversationSourceError::TooMuchText {
                    actual: text.len(),
                    limit: MAX_TEXT_BYTES,
                });
            }
            if text.bytes().any(|byte| byte == 0) {
                return Err(ConversationSourceError::InvalidField("text"));
            }
        }
        Ok(())
    }
}

/// A tombstone: an absence the producer confirmed inside its reconciliation
/// window.
///
/// It MARKS; it never erases. The landed record stays in the journal and the
/// projection reports it as tombstoned, which is what lets the corpus tell a
/// redaction from an append, and what lets a later pass distinguish "we know
/// this was deleted" from "we never saw this".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConversationTombstoneRecordV1 {
    pub channel_id: String,
    pub message_ts: String,
    /// Strictly greater than zero, same reasoning as an edit.
    pub revision: u64,
    pub observed_at: String,
}

impl ConversationTombstoneRecordV1 {
    pub fn validate(&self) -> ConversationResult<()> {
        validate_opaque(&self.channel_id, MAX_CHANNEL_ID_BYTES, "channel_id")?;
        validate_message_ts(&self.message_ts)?;
        if self.revision == 0 {
            return Err(ConversationSourceError::InvalidRevision);
        }
        validate_opaque(&self.observed_at, MAX_TIMESTAMP_BYTES, "observed_at")
    }
}

/// One entry of a revisions request. Tagged rather than a nullable text field,
/// because "edited to empty" and "deleted" are different facts and a shape that
/// cannot tell them apart will eventually be asked to.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConversationRevisionV1 {
    Edit(ConversationRevisionRecordV1),
    Tombstone(ConversationTombstoneRecordV1),
}

impl ConversationRevisionV1 {
    pub fn channel_id(&self) -> &str {
        match self {
            Self::Edit(record) => &record.channel_id,
            Self::Tombstone(record) => &record.channel_id,
        }
    }

    pub fn message_ts(&self) -> &str {
        match self {
            Self::Edit(record) => &record.message_ts,
            Self::Tombstone(record) => &record.message_ts,
        }
    }

    pub fn revision(&self) -> u64 {
        match self {
            Self::Edit(record) => record.revision,
            Self::Tombstone(record) => record.revision,
        }
    }

    pub fn validate(&self) -> ConversationResult<()> {
        match self {
            Self::Edit(record) => record.validate(),
            Self::Tombstone(record) => record.validate(),
        }
    }
}

// ---------------------------------------------------------------------------
// Channel roster
// ---------------------------------------------------------------------------

/// What class of conversation this is, so policy can gate on it.
///
/// Design section 6 makes each conversation class a separate operator flag,
/// default off for the sensitive ones. Carrying the class on the wire is what
/// lets the corpus host enforce that independently of the producer host's
/// config, rather than trusting a producer to have filtered correctly.
///
/// **Channels only, by operator ruling (2026-08-13).** The deployed posture
/// observes channels from the bot's own membership; no direct or group-direct
/// message ever enters this corpus, so no variant names one. That is not a
/// gap to be filled in later out of tidiness: because the enum is closed and
/// every ingest payload is `deny_unknown_fields`, a producer that somehow
/// presented a DM roster entry gets a PARSE REFUSAL rather than an ingest, and
/// the class it would need cannot be conjured by a config typo. A dormant
/// `DirectMessage` variant would trade that fail-closed property for nothing,
/// since design section 5 gates DMs behind operator-gated S5 anyway. Adding
/// one later is additive and requires the deliberate act of adding it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ChannelClassV1 {
    #[default]
    Public,
    Private,
}

/// One channel as the producer observed it.
///
/// The name is an OBSERVATION, never identity (design 4.2). Channels get
/// renamed; ids do not. This is the same discipline the project catalog applies
/// to absolute paths, and the reason `observed_name` is optional and
/// `channel_id` is not.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ChannelObservationV1 {
    pub channel_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_name: Option<String>,
    #[serde(default)]
    pub class: ChannelClassV1,
    /// Whether the observing identity is a member. Under the ruled one-app
    /// bot-perspective posture (design 3.1) this is exactly the visibility
    /// bound: the bot indexes what its own membership allows.
    #[serde(default)]
    pub is_member: bool,
    pub observed_at: String,
}

impl ChannelObservationV1 {
    pub fn validate(&self) -> ConversationResult<()> {
        validate_opaque(&self.channel_id, MAX_CHANNEL_ID_BYTES, "channel_id")?;
        validate_optional_opaque(
            self.observed_name.as_ref(),
            MAX_LABEL_BYTES,
            "observed_name",
        )?;
        validate_opaque(&self.observed_at, MAX_TIMESTAMP_BYTES, "observed_at")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ChannelRosterRequestV1 {
    pub schema_version: u32,
    pub conversation_policy_version: String,
    pub scope: ConnectorScope,
    pub workspace_id: String,
    pub channels: Vec<ChannelObservationV1>,
}

impl ChannelRosterRequestV1 {
    pub fn validate(&self) -> ConversationResult<()> {
        validate_envelope(
            self.schema_version,
            &self.conversation_policy_version,
            &self.scope,
            &self.workspace_id,
        )?;
        if self.channels.len() > MAX_ROSTER_CHANNELS {
            return Err(ConversationSourceError::TooManyRecords {
                actual: self.channels.len(),
                limit: MAX_ROSTER_CHANNELS,
            });
        }
        let mut previous: Option<&str> = None;
        for channel in &self.channels {
            channel.validate()?;
            if let Some(previous) = previous {
                if channel.channel_id.as_str() == previous {
                    return Err(ConversationSourceError::DuplicateChannelId(
                        channel.channel_id.clone(),
                    ));
                }
                if channel.channel_id.as_str() < previous {
                    return Err(ConversationSourceError::RecordsNotOrdered);
                }
            }
            previous = Some(&channel.channel_id);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChannelRosterReceiptV1 {
    pub workspace_id: String,
    pub channels_recorded: u64,
}

// ---------------------------------------------------------------------------
// Batches
// ---------------------------------------------------------------------------

/// An ordered run of new records on ONE channel.
///
/// One channel per batch is load-bearing rather than a simplification: the
/// server advances a per-channel cursor only after the batch is durable
/// (design 5.2), and a batch spanning channels would either advance several
/// cursors from one non-atomic write or advance none of them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConversationBatchV1 {
    pub schema_version: u32,
    pub conversation_policy_version: String,
    pub scope: ConnectorScope,
    pub workspace_id: String,
    pub channel_id: String,
    /// Strictly increasing by `message_ts`. Two distinct messages never share
    /// a timestamp within a channel, so strict ordering also gives uniqueness.
    pub records: Vec<ConversationMessageRecordV1>,
    /// Covers every field of every record, so a batch rewritten in flight is a
    /// rejected batch rather than a silently landed lie. This is the file
    /// lane's `manifest_sha256` discipline at batch quantum.
    pub batch_digest: String,
    pub observed_at: String,
}

impl ConversationBatchV1 {
    pub fn validate(&self) -> ConversationResult<()> {
        validate_envelope(
            self.schema_version,
            &self.conversation_policy_version,
            &self.scope,
            &self.workspace_id,
        )?;
        validate_opaque(&self.channel_id, MAX_CHANNEL_ID_BYTES, "channel_id")?;
        validate_opaque(&self.observed_at, MAX_TIMESTAMP_BYTES, "observed_at")?;
        if self.records.len() > MAX_BATCH_RECORDS {
            return Err(ConversationSourceError::TooManyRecords {
                actual: self.records.len(),
                limit: MAX_BATCH_RECORDS,
            });
        }
        let mut previous: Option<String> = None;
        for record in &self.records {
            record.validate()?;
            // The tamper check that lets the store compose the durable key
            // from the envelope: a record may not name a channel the envelope
            // does not declare, so an authenticated producer cannot land into
            // a channel by burying it in a record.
            if record.channel_id != self.channel_id {
                return Err(ConversationSourceError::ChannelMismatch {
                    declared: self.channel_id.clone(),
                    actual: record.channel_id.clone(),
                });
            }
            let key = message_ts_order_key(&record.message_ts).ok_or_else(|| {
                ConversationSourceError::InvalidMessageTs(record.message_ts.clone())
            })?;
            if let Some(previous) = &previous {
                if &key == previous {
                    return Err(ConversationSourceError::DuplicateMessageTs(
                        record.message_ts.clone(),
                    ));
                }
                if &key < previous {
                    return Err(ConversationSourceError::RecordsNotOrdered);
                }
            }
            previous = Some(key);
        }
        if batch_digest(&self.records) != self.batch_digest {
            return Err(ConversationSourceError::BatchDigestMismatch);
        }
        Ok(())
    }

    /// The greatest `message_ts` this batch carries, or `None` when empty.
    ///
    /// An empty batch is legal and means "I swept and found nothing"; it must
    /// not advance a watermark, which is exactly what `None` produces.
    pub fn max_message_ts(&self) -> Option<&str> {
        self.records.last().map(|record| record.message_ts.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConversationRevisionsRequestV1 {
    pub schema_version: u32,
    pub conversation_policy_version: String,
    pub scope: ConnectorScope,
    pub workspace_id: String,
    pub channel_id: String,
    pub revisions: Vec<ConversationRevisionV1>,
    pub observed_at: String,
}

impl ConversationRevisionsRequestV1 {
    pub fn validate(&self) -> ConversationResult<()> {
        validate_envelope(
            self.schema_version,
            &self.conversation_policy_version,
            &self.scope,
            &self.workspace_id,
        )?;
        validate_opaque(&self.channel_id, MAX_CHANNEL_ID_BYTES, "channel_id")?;
        validate_opaque(&self.observed_at, MAX_TIMESTAMP_BYTES, "observed_at")?;
        if self.revisions.len() > MAX_REVISIONS_PER_REQUEST {
            return Err(ConversationSourceError::TooManyRecords {
                actual: self.revisions.len(),
                limit: MAX_REVISIONS_PER_REQUEST,
            });
        }
        for revision in &self.revisions {
            revision.validate()?;
            if revision.channel_id() != self.channel_id {
                return Err(ConversationSourceError::ChannelMismatch {
                    declared: self.channel_id.clone(),
                    actual: revision.channel_id().to_string(),
                });
            }
        }
        Ok(())
    }
}

/// What one accepted batch did, and where the server's cursor now stands.
///
/// `duplicates` is reported rather than folded into `accepted` because a
/// producer resweeping from an older watermark is the ORDINARY shape of a
/// resume (design 4.1: a resweep is always safe because landing is idempotent),
/// and a producer that cannot see how much of its sweep was redundant cannot
/// tell a healthy resume from a stuck cursor.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationBatchReceiptV1 {
    pub channel_id: String,
    pub accepted: u64,
    pub duplicates: u64,
    pub cursor: ChannelCursorV1,
}

/// What one revisions request did.
///
/// The counts partition the request exactly: `accepted + duplicates` equals the
/// number of revisions sent, and `accepted` itself splits into `applied` and
/// `held`. Both invariants are asserted by the store's tests, because a receipt
/// whose numbers do not add up is worse than no receipt: a producer would have
/// no way to tell a silently dropped revision from one it miscounted.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationRevisionsReceiptV1 {
    pub channel_id: String,
    /// Durably journaled by this request. Equals `applied + held`.
    ///
    /// "Accepted" means DURABLE, not "took effect". A held revision is every
    /// bit as durable as an applied one, and the producer's only real question
    /// is whether it needs to send the revision again. It does not.
    pub accepted: u64,
    /// Of `accepted`: superseded a landed message in place.
    pub applied: u64,
    /// Of `accepted`: journaled and waiting on a message this corpus has not
    /// landed YET.
    ///
    /// This is the never-landed case, deliberately distinguishable from
    /// `applied`. An observer whose visibility began mid-history can
    /// legitimately see a deletion before it ever sees the message, and a
    /// backfill lane running behind steady state makes that ordinary rather
    /// than exotic. Refusing would make the producer retry forever;
    /// accepting-and-discarding would lose the deletion the moment the slower
    /// lane delivered the message. So it is journaled, held, applied when its
    /// message lands, and REPORTED here the whole time: held is not silent.
    ///
    /// The running total per channel rides
    /// [`ChannelCursorV1::held_revisions`] and the status read, so the S4
    /// reconciliation lane can watch the backlog rather than infer it.
    pub held: u64,
    /// Revisions the corpus already holds at an equal or higher revision,
    /// whether that revision is applied or still held. Nothing was journaled.
    pub duplicates: u64,
    pub cursor: ChannelCursorV1,
}

// ---------------------------------------------------------------------------
// Cursors and status
// ---------------------------------------------------------------------------

/// The server's truth about one channel.
///
/// Every field is DERIVED from what the corpus durably holds, never from what a
/// producer asserted. That is the collector invariant restated for this lane:
/// the producer asks where to resume, so a producer restart, reinstall, or host
/// move needs no producer-side durable state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ChannelCursorV1 {
    pub channel_id: String,
    /// The greatest landed `message_ts`. Corresponds to
    /// `TranscriptCursor::ProviderEventId`; see the crate doc.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_watermark: Option<String>,
    /// The least landed `message_ts`, so a bounded backfill resumes from the
    /// server's recorded oldest-observed mark (design 5.1) rather than from a
    /// producer-side memory that a reinstall loses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_observed: Option<String>,
    /// Thread parents whose replies are still owed. Corresponds to
    /// `TranscriptCursor::MessageIdSet`; see the crate doc.
    ///
    /// FIRST CLASS, not an S3 afterthought. Threaded replies are a primary
    /// communication surface in the deployed workspace and they do not appear
    /// in a channel history sweep unless they were also broadcast, so a
    /// history-only connector silently loses most thread bodies (design 5.3).
    /// The record carries `thread_parent_ts` and the cursor carries this set
    /// from the FIRST wire version precisely so S3 adds sweep POLICY rather
    /// than a shape change that would need a re-ingest to backfill.
    ///
    /// Bounded by [`MAX_INFLIGHT_THREAD_PARENTS`]: on a pathological channel
    /// the newest parents are kept and the oldest are dropped, because an old
    /// thread is the one least likely to still be taking replies.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inflight_thread_parents: Vec<String>,
    pub landed_records: u64,
    /// Revisions that superseded a landed message in place.
    pub revisions: u64,
    pub tombstones: u64,
    /// Revisions and tombstones journaled for messages this channel has not
    /// landed yet, still waiting to apply.
    ///
    /// A standing nonzero value is a real signal, not noise: it means the
    /// corpus holds deletions for bodies it has never seen, so either backfill
    /// is behind or the observer's visibility began mid-history. S4's
    /// reconciliation lane reads it to know whether the gap is closing.
    #[serde(default)]
    pub held_revisions: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_batch_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationCursorsResponseV1 {
    pub schema_version: u32,
    pub scope: ConnectorScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    pub channels: Vec<ChannelCursorV1>,
}

/// Per-channel operational truth.
///
/// `lag_seconds` is the headline: design 5.5 says a throttled producer shows up
/// corpus-side as LAG, not as errors, so the status surface has to make lag a
/// first-class number rather than something an operator infers from timestamps.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChannelStatusV1 {
    pub channel_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_name: Option<String>,
    #[serde(default)]
    pub class: ChannelClassV1,
    pub landed_records: u64,
    pub revisions: u64,
    pub tombstones: u64,
    /// See [`ChannelCursorV1::held_revisions`]. Surfaced on status because a
    /// held deletion is a coverage fact an operator has to be able to see.
    #[serde(default)]
    pub held_revisions: u64,
    pub inflight_thread_parents: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_watermark: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_batch_at: Option<String>,
    /// Seconds between the newest landed message and the moment of the read.
    /// `None` when the channel has landed nothing, which is distinct from zero
    /// lag and must not be rendered as healthy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lag_seconds: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationStatusResponseV1 {
    pub schema_version: u32,
    pub scope: ConnectorScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    pub channels: Vec<ChannelStatusV1>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ErrorResponse {
    pub code: String,
    pub message: String,
}

// ---------------------------------------------------------------------------
// Catalog onboarding
// ---------------------------------------------------------------------------

/// Facts the satellite probed from the workspace, presented over the
/// authenticated producer channel.
///
/// The producer id is NOT carried here: it comes from the authenticated bearer,
/// so a producer cannot claim to be another. This mirrors the file lane's
/// onboard request field for field except that the probed coordinate is a
/// workspace rather than a folder, and it is a separate type rather than a
/// shared one because sharing it would put the file lane's crate into the
/// conversation satellite's dependency tree.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConversationCatalogOnboardRequestV1 {
    pub schema_version: u32,
    pub scope: ConnectorScope,
    /// The connector kind the producer actually ran, checked against the
    /// grant's declared kind.
    pub probed_connector_kind: String,
    /// The vendor tenant the producer actually reached (a workspace domain).
    pub probed_remote_authority: String,
    /// The provider-issued workspace id. Durable identity for every record
    /// this scope will ever land, which is why it is probed at onboarding and
    /// then held constant.
    pub probed_workspace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probed_workspace_display_name: Option<String>,
    pub display_name: String,
}

impl ConversationCatalogOnboardRequestV1 {
    pub fn validate(&self) -> ConversationResult<()> {
        if self.schema_version != CATALOG_ONBOARD_SCHEMA_VERSION {
            return Err(ConversationSourceError::UnsupportedSchema(
                self.schema_version,
            ));
        }
        validate_scope(&self.scope)?;
        validate_opaque(&self.probed_connector_kind, 64, "probed_connector_kind")
            .map_err(|_| ConversationSourceError::InvalidOnboardField("probed_connector_kind"))?;
        validate_opaque(
            &self.probed_remote_authority,
            MAX_LABEL_BYTES,
            "probed_remote_authority",
        )
        .map_err(|_| ConversationSourceError::InvalidOnboardField("probed_remote_authority"))?;
        validate_opaque(
            &self.probed_workspace_id,
            MAX_WORKSPACE_ID_BYTES,
            "probed_workspace_id",
        )
        .map_err(|_| ConversationSourceError::InvalidOnboardField("probed_workspace_id"))?;
        validate_optional_opaque(
            self.probed_workspace_display_name.as_ref(),
            MAX_LABEL_BYTES,
            "probed_workspace_display_name",
        )
        .map_err(|_| {
            ConversationSourceError::InvalidOnboardField("probed_workspace_display_name")
        })?;
        validate_opaque(&self.display_name, MAX_LABEL_BYTES, "display_name")
            .map_err(|_| ConversationSourceError::InvalidOnboardField("display_name"))?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConversationCatalogOnboardResponseV1 {
    pub project_id: String,
    /// True only when THIS call minted the project. Find-or-create, so a
    /// producer driving it before every cycle is safe to retry.
    pub created_project: bool,
    pub epoch: u64,
}

// ---------------------------------------------------------------------------
// Envelope and digests
// ---------------------------------------------------------------------------

/// The four checks every ingest payload shares.
fn validate_envelope(
    schema_version: u32,
    policy_version: &str,
    scope: &ConnectorScope,
    workspace_id: &str,
) -> ConversationResult<()> {
    if schema_version != SCHEMA_VERSION {
        return Err(ConversationSourceError::UnsupportedSchema(schema_version));
    }
    if policy_version != CONVERSATION_POLICY_VERSION {
        return Err(ConversationSourceError::ConversationPolicyMismatch {
            received: policy_version.chars().take(128).collect(),
            expected: CONVERSATION_POLICY_VERSION,
        });
    }
    validate_scope(scope)?;
    validate_opaque(workspace_id, MAX_WORKSPACE_ID_BYTES, "workspace_id")
}

/// The batch fingerprint: every field of every record, in order.
///
/// Reactions and attachments are covered too. That is deliberate: they are the
/// fields most likely to be rewritten in flight by a proxy that "helpfully"
/// normalizes them, and a batch whose attachment reference was rewritten must
/// fail here rather than land a reference to something the workspace never
/// held.
pub fn batch_digest(records: &[ConversationMessageRecordV1]) -> String {
    let mut hasher = Sha256::new();
    put_field(&mut hasher, b"bbox-conversation-source-batch-v1");
    for record in records {
        put_field(&mut hasher, record.channel_id.as_bytes());
        put_field(&mut hasher, record.message_ts.as_bytes());
        hasher.update(record.revision.to_be_bytes());
        put_field(&mut hasher, record.author_id.as_bytes());
        put_field(&mut hasher, author_kind_tag(record.author_kind).as_bytes());
        put_optional(&mut hasher, record.thread_parent_ts.as_deref());
        put_optional(&mut hasher, record.subtype.as_deref());
        put_field(&mut hasher, record.text.as_bytes());
        put_optional(&mut hasher, record.edited_ts.as_deref());
        hasher.update((record.reactions.len() as u64).to_be_bytes());
        for reaction in &record.reactions {
            put_field(&mut hasher, reaction.name.as_bytes());
            hasher.update(reaction.count.to_be_bytes());
        }
        hasher.update((record.attachments.len() as u64).to_be_bytes());
        for attachment in &record.attachments {
            put_field(&mut hasher, attachment.remote_id.as_bytes());
            put_optional(&mut hasher, attachment.name.as_deref());
            put_optional(&mut hasher, attachment.mime_type.as_deref());
            match attachment.size {
                Some(size) => {
                    hasher.update([1_u8]);
                    hasher.update(size.to_be_bytes());
                }
                None => hasher.update([0_u8]),
            }
        }
        put_field(&mut hasher, record.observed_at.as_bytes());
    }
    hex::encode(hasher.finalize())
}

fn author_kind_tag(kind: AuthorKindV1) -> &'static str {
    match kind {
        AuthorKindV1::Human => "human",
        AuthorKindV1::App => "app",
        AuthorKindV1::Unknown => "unknown",
    }
}

/// The scope's durable directory key, mirroring the file lane so a reader of
/// one store recognizes the other's layout instantly.
pub fn scope_hash(scope: &ConnectorScope) -> String {
    let mut hasher = Sha256::new();
    put_field(&mut hasher, b"bbox-conversation-source-scope-v1");
    put_field(&mut hasher, scope.connector_source_id().as_str().as_bytes());
    put_field(&mut hasher, scope.connector_kind().as_str().as_bytes());
    hex::encode(hasher.finalize())
}

/// A channel's durable directory key. Provider channel ids are opaque and may
/// contain characters a path component cannot, so the store never uses one as a
/// filename directly.
pub fn channel_hash(workspace_id: &str, channel_id: &str) -> String {
    let mut hasher = Sha256::new();
    put_field(&mut hasher, b"bbox-conversation-source-channel-v1");
    put_field(&mut hasher, workspace_id.as_bytes());
    put_field(&mut hasher, channel_id.as_bytes());
    hex::encode(hasher.finalize())
}

fn put_optional(hasher: &mut Sha256, value: Option<&str>) {
    match value {
        Some(value) => {
            hasher.update([1_u8]);
            put_field(hasher, value.as_bytes());
        }
        None => hasher.update([0_u8]),
    }
}

fn put_field(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE_ID: &str = "csrc_5f2c1d9a4b6e470e";
    const WORKSPACE: &str = "T0FIXTURE01";
    const CHANNEL: &str = "C0FIXTURE01";

    fn scope() -> ConnectorScope {
        ConnectorScope::try_new(SOURCE_ID, "fixture").unwrap()
    }

    fn record(ts: &str, text: &str) -> ConversationMessageRecordV1 {
        ConversationMessageRecordV1 {
            channel_id: CHANNEL.into(),
            message_ts: ts.into(),
            revision: 0,
            author_id: "U0FIXTURE".into(),
            author_kind: AuthorKindV1::Human,
            thread_parent_ts: None,
            subtype: None,
            text: text.into(),
            edited_ts: None,
            reactions: vec![ReactionRefV1 {
                name: "eyes".into(),
                count: 2,
            }],
            attachments: vec![AttachmentRefV1 {
                remote_id: "F0FIXTURE".into(),
                name: Some("plan.md".into()),
                mime_type: Some("text/markdown".into()),
                size: Some(12),
            }],
            observed_at: "2026-08-13T00:00:00Z".into(),
        }
    }

    fn batch(records: Vec<ConversationMessageRecordV1>) -> ConversationBatchV1 {
        ConversationBatchV1 {
            schema_version: SCHEMA_VERSION,
            conversation_policy_version: CONVERSATION_POLICY_VERSION.into(),
            scope: scope(),
            workspace_id: WORKSPACE.into(),
            channel_id: CHANNEL.into(),
            batch_digest: batch_digest(&records),
            records,
            observed_at: "2026-08-13T00:00:01Z".into(),
        }
    }

    #[test]
    fn a_valid_batch_round_trips_through_its_digest() {
        let batch = batch(vec![
            record("1755000000.000100", "first"),
            record("1755000001.000200", "second"),
        ]);
        batch.validate().unwrap();
        assert_eq!(batch.max_message_ts(), Some("1755000001.000200"));
        assert_eq!(batch.batch_digest.len(), 64);
    }

    #[test]
    fn an_empty_batch_is_legal_and_advances_no_watermark() {
        // "I swept and found nothing" is a real, frequent observation. It must
        // not be an error, and it must not move a cursor.
        let batch = batch(Vec::new());
        batch.validate().unwrap();
        assert_eq!(batch.max_message_ts(), None);
    }

    #[test]
    fn every_record_field_is_covered_by_the_batch_digest() {
        let base = record("1755000000.000100", "first");
        let batch = batch(vec![base.clone()]);
        let mutations: Vec<Box<dyn Fn(&mut ConversationMessageRecordV1)>> = vec![
            Box::new(|record| record.text = "rewritten".into()),
            Box::new(|record| record.author_id = "U0OTHER".into()),
            Box::new(|record| record.author_kind = AuthorKindV1::App),
            Box::new(|record| record.thread_parent_ts = Some("1754999999.000001".into())),
            Box::new(|record| record.subtype = Some("bot_message".into())),
            Box::new(|record| record.edited_ts = Some("1755000009.000000".into())),
            Box::new(|record| record.reactions[0].count = 99),
            Box::new(|record| record.reactions[0].name = "fire".into()),
            Box::new(|record| record.attachments[0].remote_id = "F0EVIL".into()),
            Box::new(|record| record.attachments[0].mime_type = Some("text/html".into())),
            Box::new(|record| record.attachments[0].size = Some(9_999)),
            Box::new(|record| record.observed_at = "2026-01-01T00:00:00Z".into()),
        ];
        for mutate in mutations {
            let mut tampered = base.clone();
            mutate(&mut tampered);
            let mut rewritten = batch.clone();
            rewritten.records = vec![tampered];
            assert!(
                matches!(
                    rewritten.validate(),
                    Err(ConversationSourceError::BatchDigestMismatch)
                ),
                "a record rewritten in flight must fail the batch digest"
            );
        }
    }

    #[test]
    fn a_record_may_not_name_a_channel_the_envelope_does_not_declare() {
        // The tamper check the store depends on: the durable key is composed
        // from the envelope, so a record must never be able to redirect itself
        // into another channel.
        let mut smuggled = record("1755000000.000100", "first");
        smuggled.channel_id = "C0SOMEWHEREELSE".into();
        let batch = batch(vec![smuggled]);
        assert!(matches!(
            batch.validate(),
            Err(ConversationSourceError::ChannelMismatch { .. })
        ));
    }

    #[test]
    fn batch_records_must_be_strictly_ordered_and_unique() {
        let out_of_order = batch(vec![
            record("1755000001.000200", "second"),
            record("1755000000.000100", "first"),
        ]);
        assert!(matches!(
            out_of_order.validate(),
            Err(ConversationSourceError::RecordsNotOrdered)
        ));

        let duplicated = batch(vec![
            record("1755000000.000100", "first"),
            record("1755000000.000100", "again"),
        ]);
        assert!(matches!(
            duplicated.validate(),
            Err(ConversationSourceError::DuplicateMessageTs(_))
        ));
    }

    #[test]
    fn message_timestamps_order_numerically_not_lexically() {
        // The bug this function exists to prevent: a plain string compare puts
        // "999.1" after "1000.0" the first time a provider changes its integer
        // width, which would silently strand a channel's watermark.
        assert!(message_ts_is_after("1000.000000", "999.100000"));
        assert!(!message_ts_is_after("999.100000", "1000.000000"));
        assert!(message_ts_is_after(
            "1755000000.000002",
            "1755000000.000001"
        ));
        assert!(!message_ts_is_after(
            "1755000000.000001",
            "1755000000.000001"
        ));
        // Differing fraction widths still compare correctly.
        assert!(message_ts_is_after("10.2", "10.1999999"));
        assert!(message_ts_order_key("nope").is_none());
        assert!(message_ts_order_key("").is_none());
    }

    #[test]
    fn message_timestamps_refuse_shapes_that_cannot_be_timestamps() {
        for valid in ["0", "1755000000", "1755000000.000100", "10.2"] {
            validate_message_ts(valid).unwrap_or_else(|error| panic!("{valid}: {error}"));
        }
        for invalid in [
            "",
            ".",
            ".1",
            "1.",
            "1.2.3",
            "-1",
            "1e9",
            "17550000001755000000175500000017",
            "abc",
        ] {
            assert!(
                validate_message_ts(invalid).is_err(),
                "{invalid:?} must be refused"
            );
        }
    }

    #[test]
    fn an_envelope_rejects_schema_and_policy_skew() {
        let mut skewed = batch(vec![record("1755000000.000100", "first")]);
        skewed.conversation_policy_version = "conversation-source-connector-policy-v0".into();
        assert!(matches!(
            skewed.validate(),
            Err(ConversationSourceError::ConversationPolicyMismatch { .. })
        ));

        let mut old_schema = batch(vec![record("1755000000.000100", "first")]);
        old_schema.schema_version = SCHEMA_VERSION + 1;
        assert!(matches!(
            old_schema.validate(),
            Err(ConversationSourceError::UnsupportedSchema(_))
        ));
    }

    #[test]
    fn record_text_keeps_newlines_but_refuses_nul_and_oversize() {
        let mut multiline = record("1755000000.000100", "line one\nline two\ttabbed");
        multiline
            .validate()
            .expect("real messages contain newlines");

        multiline.text = "before\0after".into();
        assert!(matches!(
            multiline.validate(),
            Err(ConversationSourceError::InvalidField("text"))
        ));

        multiline.text = "x".repeat(MAX_TEXT_BYTES + 1);
        assert!(matches!(
            multiline.validate(),
            Err(ConversationSourceError::TooMuchText { .. })
        ));
    }

    #[test]
    fn a_revision_may_never_claim_the_original_observation() {
        let edit = ConversationRevisionRecordV1 {
            channel_id: CHANNEL.into(),
            message_ts: "1755000000.000100".into(),
            revision: 0,
            text: Some("edited".into()),
            edited_ts: Some("1755000100.000000".into()),
            observed_at: "2026-08-13T00:00:00Z".into(),
        };
        assert!(matches!(
            edit.validate(),
            Err(ConversationSourceError::InvalidRevision)
        ));

        let tombstone = ConversationTombstoneRecordV1 {
            channel_id: CHANNEL.into(),
            message_ts: "1755000000.000100".into(),
            revision: 0,
            observed_at: "2026-08-13T00:00:00Z".into(),
        };
        assert!(matches!(
            tombstone.validate(),
            Err(ConversationSourceError::InvalidRevision)
        ));
    }

    #[test]
    fn an_edit_and_a_tombstone_stay_distinguishable_on_the_wire() {
        // "edited to empty" and "deleted" are different facts, so the tag is
        // load-bearing rather than decorative.
        let edit = ConversationRevisionV1::Edit(ConversationRevisionRecordV1 {
            channel_id: CHANNEL.into(),
            message_ts: "1755000000.000100".into(),
            revision: 1,
            text: Some(String::new()),
            edited_ts: Some("1755000100.000000".into()),
            observed_at: "2026-08-13T00:00:00Z".into(),
        });
        let tombstone = ConversationRevisionV1::Tombstone(ConversationTombstoneRecordV1 {
            channel_id: CHANNEL.into(),
            message_ts: "1755000000.000100".into(),
            revision: 1,
            observed_at: "2026-08-13T00:00:00Z".into(),
        });
        edit.validate().unwrap();
        tombstone.validate().unwrap();
        assert_ne!(
            serde_json::to_value(&edit).unwrap(),
            serde_json::to_value(&tombstone).unwrap()
        );
        assert_eq!(edit.revision(), tombstone.revision());
        assert_eq!(edit.message_ts(), tombstone.message_ts());
    }

    #[test]
    fn a_revisions_request_refuses_a_channel_mismatch() {
        let request = ConversationRevisionsRequestV1 {
            schema_version: SCHEMA_VERSION,
            conversation_policy_version: CONVERSATION_POLICY_VERSION.into(),
            scope: scope(),
            workspace_id: WORKSPACE.into(),
            channel_id: CHANNEL.into(),
            revisions: vec![ConversationRevisionV1::Tombstone(
                ConversationTombstoneRecordV1 {
                    channel_id: "C0ELSEWHERE".into(),
                    message_ts: "1755000000.000100".into(),
                    revision: 1,
                    observed_at: "2026-08-13T00:00:00Z".into(),
                },
            )],
            observed_at: "2026-08-13T00:00:00Z".into(),
        };
        assert!(matches!(
            request.validate(),
            Err(ConversationSourceError::ChannelMismatch { .. })
        ));
    }

    #[test]
    fn a_roster_orders_channels_and_keeps_names_as_observations() {
        let roster = ChannelRosterRequestV1 {
            schema_version: SCHEMA_VERSION,
            conversation_policy_version: CONVERSATION_POLICY_VERSION.into(),
            scope: scope(),
            workspace_id: WORKSPACE.into(),
            channels: vec![
                ChannelObservationV1 {
                    channel_id: "C0AAA".into(),
                    observed_name: Some("ops".into()),
                    class: ChannelClassV1::Public,
                    is_member: true,
                    observed_at: "2026-08-13T00:00:00Z".into(),
                },
                ChannelObservationV1 {
                    channel_id: "C0BBB".into(),
                    // A channel with no readable name is still a channel: the
                    // id is identity and the name is an observation.
                    observed_name: None,
                    class: ChannelClassV1::Private,
                    is_member: false,
                    observed_at: "2026-08-13T00:00:00Z".into(),
                },
            ],
        };
        roster.validate().unwrap();

        let mut unsorted = roster.clone();
        unsorted.channels.reverse();
        assert!(matches!(
            unsorted.validate(),
            Err(ConversationSourceError::RecordsNotOrdered)
        ));
    }

    #[test]
    fn an_onboard_request_validates_its_probed_facts() {
        let valid = ConversationCatalogOnboardRequestV1 {
            schema_version: CATALOG_ONBOARD_SCHEMA_VERSION,
            scope: scope(),
            probed_connector_kind: "fixture".into(),
            probed_remote_authority: "fixture.example".into(),
            probed_workspace_id: WORKSPACE.into(),
            probed_workspace_display_name: Some("Fixture Workspace".into()),
            display_name: "Fixture Workspace".into(),
        };
        valid.validate().unwrap();

        let mut bad_schema = valid.clone();
        bad_schema.schema_version += 1;
        assert!(matches!(
            bad_schema.validate(),
            Err(ConversationSourceError::UnsupportedSchema(_))
        ));

        let mut no_workspace = valid.clone();
        no_workspace.probed_workspace_id = String::new();
        assert!(matches!(
            no_workspace.validate(),
            Err(ConversationSourceError::InvalidOnboardField(
                "probed_workspace_id"
            ))
        ));

        let mut control_name = valid.clone();
        control_name.display_name = "Fixture\nWorkspace".into();
        assert!(control_name.validate().is_err());
    }

    #[test]
    fn scope_and_channel_keys_are_stable_and_distinct() {
        let other = ConnectorScope::try_new("csrc_00000000deadbeef", "fixture").unwrap();
        assert_eq!(scope_hash(&scope()), scope_hash(&scope()));
        assert_ne!(scope_hash(&scope()), scope_hash(&other));
        assert_eq!(
            channel_hash(WORKSPACE, CHANNEL),
            channel_hash(WORKSPACE, CHANNEL)
        );
        // Field separation: the digest must not let a workspace/channel split
        // slide, or two different pairs would share a directory.
        assert_ne!(channel_hash("AB", "C"), channel_hash("A", "BC"));
    }

    #[test]
    fn wire_payloads_deny_unknown_fields() {
        let json = r#"{"channel_id":"C0","message_ts":"1.0","author_id":"U0",
            "text":"hi","observed_at":"2026-08-13T00:00:00Z","surprise":true}"#;
        assert!(serde_json::from_str::<ConversationMessageRecordV1>(json).is_err());

        let ok = r#"{"channel_id":"C0","message_ts":"1.0","author_id":"U0",
            "text":"hi","observed_at":"2026-08-13T00:00:00Z"}"#;
        let parsed: ConversationMessageRecordV1 = serde_json::from_str(ok).unwrap();
        assert_eq!(parsed.revision, 0);
        assert_eq!(parsed.author_kind, AuthorKindV1::Unknown);
        parsed.validate().unwrap();
    }
}
