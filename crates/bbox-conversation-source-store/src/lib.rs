//! Durable landing store for connector-sourced conversations, keyed on
//! [`ConnectorScope`] and then on channel.
//!
//! # Why a separate store, and why `-source-store`
//!
//! This crate follows the `*-source-store` family precedent, which is already
//! four crates wide: `bbox-code-source-store`, `bbox-file-source-store`,
//! `bbox-git-source-store`, and `bbox-knowledge-source-store` are four separate
//! stores rather than one generic one, each paired one-to-one with its leaf
//! wire crate. A conversation lane that landed in `bbox-conversation-store`
//! would be the only member of the family whose name did not say which wire it
//! serves, and folding it into an existing store crate was refused for the
//! reason `bbox-file-source-store` records: those stores are keyed and shaped
//! end to end for a GENERATION model (`head_commit`, `dirty_fingerprint`,
//! manifest digests, stage-then-flip activation), and every one of those is
//! meaningless here. There is no generation to activate, no whole-set digest to
//! compute, and no snapshot boundary in a channel. Making one polymorphic would
//! be optionality bleeding through a large store where each `Option` is a place
//! a future reader has to re-derive which lane it is holding.
//!
//! What IS shared lives one level down, in the leaf crates: identity, ordering,
//! caps, and digests come from `bbox-conversation-source`. What is duplicated
//! here is the crash-safe write discipline, and each such helper carries a
//! one-line TWIN pointer naming its counterpart so a future extraction pass can
//! find both halves.
//!
//! # On-disk layout
//!
//! ```text
//! <root>/scopes/<scope_hash>/workspace.json
//! <root>/scopes/<scope_hash>/channels/<channel_hash>/journal.ndjson
//! <root>/scopes/<scope_hash>/channels/<channel_hash>/roster.json
//! <root>/scopes/<scope_hash>/channels/<channel_hash>/checkpoint.json
//! ```
//!
//! One journal file per channel. Channel ids are provider-opaque and may carry
//! bytes a path component cannot, so the directory name is
//! `bbox_conversation_source::channel_hash` and never the id itself.
//!
//! # Write ordering and the crash story
//!
//! There is exactly ONE durable authority per channel: `journal.ndjson`, an
//! append-only NDJSON log of message, revision, and tombstone entries in
//! arrival order. `checkpoint.json` is a DERIVED cache of the cursor plus the
//! journal length it was derived at. Nothing else is authoritative, which is
//! the `bbox-source-graph` single-authority house style applied to this lane.
//!
//! Every accepted write is two steps, in this literal order:
//!
//! 1. **Append to the journal**, one JSON object per line, `write_all` then
//!    `sync_all`. The bytes are durable when this returns.
//! 2. **Replace the checkpoint**, staged sibling, fsync, rename, parent fsync.
//!
//! That order is the design invariant "the server advances the cursor only
//! after the batch is durable" (design 5.2) as a write sequence. Three crash
//! windows exist and all three are recoverable:
//!
//! - **Crash before step 1 returns** (a torn append). The journal ends with a
//!   partial or unparseable trailing line. The reader folds only COMPLETE,
//!   parseable lines and reports the byte offset of the last good boundary;
//!   the next write truncates to that boundary before appending. A torn tail
//!   therefore costs exactly the records of the batch that was in flight, and
//!   the producer re-sends them on its next sweep because the cursor never
//!   advanced past them. An unparseable line in the MIDDLE of the file is not a
//!   torn tail, it is corruption, and it refuses loudly rather than silently
//!   discarding history.
//! - **Crash between step 1 and step 2.** The journal holds records the
//!   checkpoint does not know about, so `checkpoint.journal_len` is less than
//!   the journal's actual length. Every read compares the two and refolds when
//!   they disagree, so the checkpoint self-heals on the next touch and the
//!   JOURNAL is the authority. This is the ordinary direction of failure and it
//!   is safe: the corpus already holds the records, and the cursor catches up.
//! - **A checkpoint written for a journal that later shrank** (an operator
//!   restoring an older file). `journal_len` disagrees again, the refold runs,
//!   and the cursor answers what the journal actually holds rather than what
//!   the checkpoint remembered.
//!
//! The invariant that makes all of this safe is upstream of the store:
//! **landing is idempotent on `(workspace_id, channel_id, message_ts,
//! revision)`**, so a producer resweeping from an older watermark re-lands
//! nothing. That is why there is no spool, no producer-side durable state, and
//! no recovery protocol beyond "ask the server where to resume".
//!
//! # Cost, named rather than hidden
//!
//! Deduplication folds the channel journal, so an accepted write is `O(channel
//! history)`. READS do not fold: `cursors` and `status` answer from the
//! checkpoint when it agrees with the journal length, which is the common case.
//! A per-channel key index would make writes `O(batch)` and is deliberately not
//! built in S1, because it would be a SECOND durable structure that can
//! disagree with the journal, and this lane's whole safety argument is that
//! there is exactly one.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use bbox_conversation_source::{
    ChannelClassV1, ChannelCursorV1, ChannelObservationV1, ChannelStatusV1,
    ConversationBatchReceiptV1, ConversationBatchV1, ConversationMessageRecordV1,
    ConversationRecordKeyV1, ConversationRevisionRecordV1, ConversationRevisionV1,
    ConversationRevisionsReceiptV1, ConversationRevisionsRequestV1, ConversationTombstoneRecordV1,
    MAX_INFLIGHT_THREAD_PARENTS, channel_hash, message_ts_order_key, scope_hash,
};
use bbox_corpus_core::project_catalog::ConnectorScope;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Durable records
// ---------------------------------------------------------------------------

/// One journal line. Tagged so a future entry kind is additive rather than a
/// reinterpretation of existing bytes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "entry", rename_all = "snake_case")]
pub enum JournalEntryV1 {
    Message(ConversationMessageRecordV1),
    Revision(ConversationRevisionRecordV1),
    Tombstone(ConversationTombstoneRecordV1),
}

impl JournalEntryV1 {
    fn message_ts(&self) -> &str {
        match self {
            Self::Message(record) => &record.message_ts,
            Self::Revision(record) => &record.message_ts,
            Self::Tombstone(record) => &record.message_ts,
        }
    }

    fn revision(&self) -> u64 {
        match self {
            Self::Message(record) => record.revision,
            Self::Revision(record) => record.revision,
            Self::Tombstone(record) => record.revision,
        }
    }

    fn observed_at(&self) -> &str {
        match self {
            Self::Message(record) => &record.observed_at,
            Self::Revision(record) => &record.observed_at,
            Self::Tombstone(record) => &record.observed_at,
        }
    }
}

/// The durable binding of one connector scope to one workspace.
///
/// Written at onboarding and then held constant. A scope that could land into a
/// second workspace would break the workspace-level identity design 4.2 depends
/// on: two observers converge on one record only because `workspace_id` is a
/// property of the workspace, not of whoever happened to report it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceBindingV1 {
    pub workspace_id: String,
    pub bound_at: String,
}

/// A landed message after revisions and tombstones are folded in.
///
/// The APPLY semantics of an edit belong to S4; what happens here is only the
/// storage rule design section 4.3 requires: a revision supersedes IN PLACE on
/// stable identity so an edit updates rather than duplicating, and a tombstone
/// MARKS rather than erases so a redaction stays distinguishable from an
/// append.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LandedMessageV1 {
    pub key: ConversationRecordKeyV1,
    pub record: ConversationMessageRecordV1,
    pub tombstoned: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tombstoned_at: Option<String>,
}

/// The derived checkpoint. NOT an authority; see the module doc.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ChannelCheckpointV1 {
    /// The journal byte length this cursor was derived at. A disagreement with
    /// the file's actual length is the entire staleness test.
    journal_len: u64,
    cursor: ChannelCursorV1,
}

/// What folding a channel journal produced.
#[derive(Debug, Clone, Default)]
struct ChannelFold {
    /// Keyed on the timestamp ORDER KEY so iteration is chronological even
    /// when a provider changes its integer width mid-history.
    ///
    /// The keys' `workspace_id` is left empty here: the journal does not repeat
    /// it on every line, because it is a property of the SCOPE BINDING and a
    /// per-line copy could only ever disagree with it. Callers stamp it from
    /// the binding on the way out.
    messages: BTreeMap<String, LandedMessageV1>,
    /// Revisions and tombstones whose message has not been folded YET, keyed
    /// on the same order key and applied the moment it is.
    ///
    /// Arrival order must not decide whether a redaction sticks. A producer
    /// whose visibility began mid-history can legitimately observe a deletion
    /// before it ever observes the message, and a backfill lane running behind
    /// steady state makes that ordinary rather than exotic. Holding the entry
    /// is what makes the journal's replay total: the same bytes fold to the
    /// same state no matter which order the two lanes landed them in.
    pending: BTreeMap<String, Vec<JournalEntryV1>>,
    revisions_applied: u64,
    last_observed_at: Option<String>,
    /// Byte offset of the end of the last COMPLETE, parseable line.
    good_len: u64,
    /// True when bytes beyond `good_len` were discarded as a torn tail.
    torn_tail: bool,
}

// ---------------------------------------------------------------------------
// The store
// ---------------------------------------------------------------------------

pub struct ConversationSourceStore {
    root: PathBuf,
}

impl ConversationSourceStore {
    /// Open (creating if absent) the store beneath `root`.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(root.join("scopes"))
            .with_context(|| format!("creating {}", root.join("scopes").display()))?;
        Ok(Self { root })
    }

    fn scope_dir(&self, scope: &ConnectorScope) -> PathBuf {
        self.root.join("scopes").join(scope_hash(scope))
    }

    fn workspace_path(&self, scope: &ConnectorScope) -> PathBuf {
        self.scope_dir(scope).join("workspace.json")
    }

    fn channel_dir(&self, scope: &ConnectorScope, workspace_id: &str, channel_id: &str) -> PathBuf {
        self.scope_dir(scope)
            .join("channels")
            .join(channel_hash(workspace_id, channel_id))
    }

    // -- workspace binding -------------------------------------------------

    /// Bind a scope to a workspace, idempotently.
    ///
    /// Refuses a SECOND workspace under one scope. That refusal is the durable
    /// half of the tamper check: the route layer proves a request's scope is
    /// granted, and this proves the request's workspace is the one that scope
    /// onboarded, so an authenticated producer cannot redirect a granted scope
    /// at a workspace the operator never approved.
    pub fn bind_workspace(
        &self,
        scope: &ConnectorScope,
        workspace_id: &str,
        bound_at: &str,
    ) -> Result<WorkspaceBindingV1> {
        if let Some(existing) = self.workspace_binding(scope)? {
            if existing.workspace_id != workspace_id {
                bail!(
                    "connector scope is already bound to a different workspace; refusing to \
                     rebind it"
                );
            }
            return Ok(existing);
        }
        let binding = WorkspaceBindingV1 {
            workspace_id: workspace_id.to_string(),
            bound_at: bound_at.to_string(),
        };
        write_durable_json(&self.workspace_path(scope), &binding)?;
        Ok(binding)
    }

    pub fn workspace_binding(&self, scope: &ConnectorScope) -> Result<Option<WorkspaceBindingV1>> {
        read_json_optional(&self.workspace_path(scope))
    }

    /// The workspace a scope may land into, or a refusal naming the fix.
    fn require_workspace(&self, scope: &ConnectorScope, claimed: &str) -> Result<String> {
        let Some(binding) = self.workspace_binding(scope)? else {
            bail!("connector scope has no workspace binding; onboard it before landing records");
        };
        if binding.workspace_id != claimed {
            bail!("batch workspace does not match the workspace this connector scope is bound to");
        }
        Ok(binding.workspace_id)
    }

    // -- roster ------------------------------------------------------------

    /// Record the producer's channel observations.
    ///
    /// Latest observation wins per channel. Names are stored as observations
    /// and never used as identity or as a lookup key, so a renamed channel
    /// updates its label and keeps every landed record.
    pub fn record_roster(
        &self,
        scope: &ConnectorScope,
        workspace_id: &str,
        channels: &[ChannelObservationV1],
    ) -> Result<u64> {
        self.require_workspace(scope, workspace_id)?;
        for channel in channels {
            let path = self
                .channel_dir(scope, workspace_id, &channel.channel_id)
                .join("roster.json");
            write_durable_json(&path, channel)?;
        }
        Ok(channels.len() as u64)
    }

    pub fn roster(
        &self,
        scope: &ConnectorScope,
        workspace_id: &str,
        channel_id: &str,
    ) -> Result<Option<ChannelObservationV1>> {
        read_json_optional(
            &self
                .channel_dir(scope, workspace_id, channel_id)
                .join("roster.json"),
        )
    }

    /// Channel ids this scope holds state for, in stable order.
    ///
    /// Derived by reading each channel's roster rather than by decoding the
    /// directory name: `channel_hash` is one-way on purpose, so the id has to
    /// come from durable content. A channel that has landed records but was
    /// never rostered is still enumerated, from its journal.
    pub fn channels(&self, scope: &ConnectorScope) -> Result<Vec<String>> {
        let dir = self.scope_dir(scope).join("channels");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Ok(Vec::new());
        };
        let mut ids = BTreeSet::new();
        for entry in entries {
            let path = entry?.path();
            if !path.is_dir() {
                continue;
            }
            if let Some(observation) =
                read_json_optional::<ChannelObservationV1>(&path.join("roster.json"))?
            {
                ids.insert(observation.channel_id);
                continue;
            }
            // No roster: recover the id from the first journal entry, so a
            // channel that landed records before it was ever rostered is not
            // invisible to cursors and status.
            let fold = fold_journal(&path.join("journal.ndjson"))?;
            if let Some(landed) = fold.messages.values().next() {
                ids.insert(landed.record.channel_id.clone());
            }
        }
        Ok(ids.into_iter().collect())
    }

    // -- landing -----------------------------------------------------------

    /// Land an ordered batch of new records on one channel.
    ///
    /// Idempotent on `(workspace_id, channel_id, message_ts, revision)`: a
    /// record whose identity is already held at an equal or higher revision is
    /// counted as a duplicate and NOT appended, so a replayed batch grows the
    /// journal by zero bytes and moves no cursor. That is what makes a resweep
    /// from an older watermark always safe, which is what lets the producer
    /// hold no durable state at all.
    ///
    /// The caller has already proved the scope is granted. This proves the
    /// batch's own identity fields agree with what that scope is bound to.
    pub fn land_batch(
        &self,
        scope: &ConnectorScope,
        batch: &ConversationBatchV1,
    ) -> Result<ConversationBatchReceiptV1> {
        batch
            .validate()
            // Context, not interpolation: the route layer downcasts to the
            // typed leaf error to pick a status code, and `anyhow!("{error}")`
            // would flatten it into a string that can only ever render as a
            // generic 422.
            .map_err(|error| anyhow::Error::new(error).context("invalid batch"))?;
        let workspace_id = self.require_workspace(scope, &batch.workspace_id)?;
        let dir = self.channel_dir(scope, &workspace_id, &batch.channel_id);
        let journal = dir.join("journal.ndjson");
        let fold = fold_journal(&journal)?;

        let mut appended = Vec::new();
        let mut duplicates = 0_u64;
        for record in &batch.records {
            let Some(key) = message_ts_order_key(&record.message_ts) else {
                bail!("invalid message timestamp survived batch validation");
            };
            match fold.messages.get(&key) {
                // Already held at an equal or later revision: nothing this
                // record can add. Counted, not appended, not an error.
                Some(landed) if landed.key.revision >= record.revision => duplicates += 1,
                _ => appended.push(JournalEntryV1::Message(record.clone())),
            }
        }

        let accepted = appended.len() as u64;
        let cursor = self.commit(&journal, &fold, appended, &batch.channel_id)?;
        Ok(ConversationBatchReceiptV1 {
            channel_id: batch.channel_id.clone(),
            accepted,
            duplicates,
            cursor,
        })
    }

    /// Land edits and tombstones against records this corpus already holds.
    ///
    /// Storage only. The reconciliation that DECIDES a message was edited or
    /// deleted is producer-side window diffing (design 5.4) and applying a
    /// revision to the projected document is S4; this makes the observation
    /// durable, ordered, and idempotent, and nothing more.
    pub fn land_revisions(
        &self,
        scope: &ConnectorScope,
        request: &ConversationRevisionsRequestV1,
    ) -> Result<ConversationRevisionsReceiptV1> {
        request
            .validate()
            // See `land_batch`: context preserves the downcast.
            .map_err(|error| anyhow::Error::new(error).context("invalid revisions request"))?;
        let workspace_id = self.require_workspace(scope, &request.workspace_id)?;
        let dir = self.channel_dir(scope, &workspace_id, &request.channel_id);
        let journal = dir.join("journal.ndjson");
        let fold = fold_journal(&journal)?;

        let mut appended = Vec::new();
        let mut duplicates = 0_u64;
        let mut unknown_messages = 0_u64;
        for revision in &request.revisions {
            let Some(key) = message_ts_order_key(revision.message_ts()) else {
                bail!("invalid message timestamp survived revisions validation");
            };
            let entry = || match revision.clone() {
                ConversationRevisionV1::Edit(record) => JournalEntryV1::Revision(record),
                ConversationRevisionV1::Tombstone(record) => JournalEntryV1::Tombstone(record),
            };
            match fold.messages.get(&key) {
                Some(landed) if landed.key.revision >= revision.revision() => duplicates += 1,
                Some(_) => appended.push(entry()),
                None => {
                    // Reported, not refused, and STORED rather than dropped.
                    // Refusing would make a producer that legitimately observed
                    // only a deletion retry forever; accepting-and-discarding
                    // would lose that deletion permanently the moment the
                    // message itself arrived, with the producer already told
                    // the request succeeded. The fold holds an unmatched
                    // revision and applies it when its message lands, so
                    // arrival order does not decide whether a redaction sticks.
                    let held = fold
                        .pending
                        .get(&key)
                        .and_then(|entries| entries.iter().map(|entry| entry.revision()).max())
                        .unwrap_or(0);
                    if held >= revision.revision() {
                        duplicates += 1;
                    } else {
                        unknown_messages += 1;
                        appended.push(entry());
                    }
                }
            }
        }

        let accepted = appended.len() as u64;
        let cursor = self.commit(&journal, &fold, appended, &request.channel_id)?;
        Ok(ConversationRevisionsReceiptV1 {
            channel_id: request.channel_id.clone(),
            accepted,
            duplicates,
            unknown_messages,
            cursor,
        })
    }

    /// Append durable bytes, then republish the derived checkpoint.
    ///
    /// The ONLY place either write happens, so the ordering documented at the
    /// top of this file cannot be violated in one call site and honored in
    /// another. Appending nothing still refolds and republishes the checkpoint,
    /// which is how a stale checkpoint left by a crash self-heals on the next
    /// touch even when the touch is an all-duplicate replay.
    fn commit(
        &self,
        journal: &Path,
        fold: &ChannelFold,
        appended: Vec<JournalEntryV1>,
        channel_id: &str,
    ) -> Result<ChannelCursorV1> {
        if fold.torn_tail {
            // Discard the in-flight bytes of a batch that crashed mid-append,
            // BEFORE writing new ones: appending after a partial line would
            // make the partial line's tail and the new line's head one
            // unparseable record, turning a recoverable tear into corruption.
            truncate_durable(journal, fold.good_len)?;
        }
        if !appended.is_empty() {
            append_journal(journal, &appended)?;
        }
        // Refolded from the journal rather than incrementally patched: the
        // journal is the authority, and a cursor computed any other way is a
        // second opinion that can drift from it.
        let refolded = fold_journal(journal)?;
        let cursor = derive_cursor(channel_id, &refolded);
        let journal_len = file_len(journal)?;
        write_durable_json(
            &journal.with_file_name("checkpoint.json"),
            &ChannelCheckpointV1 {
                journal_len,
                cursor: cursor.clone(),
            },
        )?;
        Ok(cursor)
    }

    // -- reads -------------------------------------------------------------

    /// The server's truth about one channel.
    ///
    /// Answers from the checkpoint when it agrees with the journal's byte
    /// length, and refolds (repairing the checkpoint) when it does not. That
    /// comparison IS the crash recovery: a checkpoint that never got written
    /// after an accepted batch is exactly a checkpoint whose `journal_len` is
    /// short.
    pub fn cursor(
        &self,
        scope: &ConnectorScope,
        workspace_id: &str,
        channel_id: &str,
    ) -> Result<ChannelCursorV1> {
        let journal = self
            .channel_dir(scope, workspace_id, channel_id)
            .join("journal.ndjson");
        let actual_len = file_len(&journal)?;
        let checkpoint: Option<ChannelCheckpointV1> =
            read_json_optional(&journal.with_file_name("checkpoint.json"))?;
        if let Some(checkpoint) = checkpoint
            && checkpoint.journal_len == actual_len
        {
            return Ok(checkpoint.cursor);
        }
        let fold = fold_journal(&journal)?;
        let cursor = derive_cursor(channel_id, &fold);
        // Repair in place. A read that silently left a stale checkpoint behind
        // would refold on every subsequent read forever.
        if actual_len > 0 || checkpoint_exists(&journal) {
            write_durable_json(
                &journal.with_file_name("checkpoint.json"),
                &ChannelCheckpointV1 {
                    journal_len: actual_len,
                    cursor: cursor.clone(),
                },
            )?;
        }
        Ok(cursor)
    }

    /// Every channel's cursor under this scope.
    pub fn cursors(&self, scope: &ConnectorScope) -> Result<Vec<ChannelCursorV1>> {
        let Some(binding) = self.workspace_binding(scope)? else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for channel_id in self.channels(scope)? {
            out.push(self.cursor(scope, &binding.workspace_id, &channel_id)?);
        }
        Ok(out)
    }

    /// Per-channel operational status.
    ///
    /// `now_epoch_secs` is passed in rather than read from a clock so this
    /// crate holds no time dependency and lag is testable without sleeping.
    pub fn status(
        &self,
        scope: &ConnectorScope,
        now_epoch_secs: i64,
    ) -> Result<Vec<ChannelStatusV1>> {
        let Some(binding) = self.workspace_binding(scope)? else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for channel_id in self.channels(scope)? {
            let cursor = self.cursor(scope, &binding.workspace_id, &channel_id)?;
            let observation = self.roster(scope, &binding.workspace_id, &channel_id)?;
            out.push(ChannelStatusV1 {
                channel_id: cursor.channel_id.clone(),
                observed_name: observation
                    .as_ref()
                    .and_then(|observation| observation.observed_name.clone()),
                class: observation
                    .as_ref()
                    .map(|observation| observation.class)
                    .unwrap_or(ChannelClassV1::Public),
                landed_records: cursor.landed_records,
                revisions: cursor.revisions,
                tombstones: cursor.tombstones,
                inflight_thread_parents: cursor.inflight_thread_parents.len() as u64,
                lag_seconds: cursor
                    .history_watermark
                    .as_deref()
                    .and_then(epoch_seconds_of)
                    .map(|seconds| now_epoch_secs.saturating_sub(seconds)),
                history_watermark: cursor.history_watermark.clone(),
                last_batch_at: cursor.last_batch_at.clone(),
            });
        }
        Ok(out)
    }

    /// Every landed message on a channel, chronological, revisions folded in
    /// and tombstones marked.
    ///
    /// This is the read the S2 transcript projection consumes. It deliberately
    /// returns tombstoned records rather than hiding them: the projection has
    /// to know a message was deleted in order to REMOVE its document, and a
    /// reader that never sees the tombstone cannot distinguish a redaction from
    /// a message that was never observed.
    pub fn landed_messages(
        &self,
        scope: &ConnectorScope,
        workspace_id: &str,
        channel_id: &str,
    ) -> Result<Vec<LandedMessageV1>> {
        let journal = self
            .channel_dir(scope, workspace_id, channel_id)
            .join("journal.ndjson");
        Ok(fold_journal(&journal)?
            .messages
            .into_values()
            .map(|mut landed| {
                // The journal does not repeat the workspace id per line; it is
                // a property of the scope binding. Stamped here so the key a
                // caller receives is the complete durable key.
                landed.key.workspace_id = workspace_id.to_string();
                landed
            })
            .collect())
    }
}

fn checkpoint_exists(journal: &Path) -> bool {
    journal.with_file_name("checkpoint.json").is_file()
}

// ---------------------------------------------------------------------------
// Folding
// ---------------------------------------------------------------------------

/// Fold a channel journal into its current state.
///
/// Complete lines only. A trailing line with no terminator, or an unparseable
/// line that is the LAST line in the file, is a torn append: it is discarded
/// and `good_len` names the boundary the next write truncates to. An
/// unparseable line anywhere ELSE is corruption and refuses loudly, because
/// discarding it would silently drop every record after it.
fn fold_journal(path: &Path) -> Result<ChannelFold> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ChannelFold::default());
        }
        Err(error) => return Err(error).context(format!("reading {}", path.display())),
    };

    let mut fold = ChannelFold::default();
    let mut offset = 0_usize;
    let mut lines: Vec<(usize, &[u8])> = Vec::new();
    let mut cursor = 0_usize;
    while let Some(index) = bytes[cursor..].iter().position(|byte| *byte == b'\n') {
        let end = cursor + index;
        lines.push((end + 1, &bytes[cursor..end]));
        cursor = end + 1;
    }
    let unterminated = cursor < bytes.len();

    let last_index = lines.len().saturating_sub(1);
    for (index, (line_end, line)) in lines.into_iter().enumerate() {
        if line.is_empty() {
            offset = line_end;
            continue;
        }
        match serde_json::from_slice::<JournalEntryV1>(line) {
            Ok(entry) => {
                apply(&mut fold, entry);
                offset = line_end;
            }
            Err(error) => {
                if index == last_index && !unterminated {
                    // A torn append whose partial bytes happened to end on a
                    // newline (a zero-filled tail, most often). Same class of
                    // failure as an unterminated line, same remedy.
                    fold.torn_tail = true;
                    break;
                }
                return Err(error).context(format!(
                    "decoding {} at byte {offset}: an unparseable line in the middle of a \
                     conversation journal is corruption, not a torn append",
                    path.display()
                ));
            }
        }
    }
    if unterminated {
        fold.torn_tail = true;
    }
    fold.good_len = offset as u64;
    Ok(fold)
}

/// Apply one journal entry to the fold.
///
/// The three rules design section 4.3 asks for, and nothing beyond them:
/// a later revision supersedes IN PLACE on stable identity, a tombstone MARKS
/// rather than erases, and an entry at an equal or lower revision than what is
/// already held changes nothing (so replay is a no-op even if a duplicate
/// somehow reached the journal).
fn apply(fold: &mut ChannelFold, entry: JournalEntryV1) {
    let Some(key) = message_ts_order_key(entry.message_ts()) else {
        // A journal line that survived serde but not the timestamp contract.
        // Skipping is right: it can never be addressed by any later revision,
        // because every addressing path goes through the same order key.
        tracing::warn!("conversation journal entry carries an unorderable message_ts; skipping");
        return;
    };
    fold.last_observed_at = Some(entry.observed_at().to_string());
    match entry {
        JournalEntryV1::Message(record) => {
            let revision = record.revision;
            // The revision already held, read out as a plain number rather
            // than matched on a live `get_mut`. The insert-and-drain arm below
            // needs `&mut fold` while a borrow taken from `fold.messages` would
            // still be in scope, and a Copy scrutinee sidesteps that entirely.
            match fold.messages.get(&key).map(|landed| landed.key.revision) {
                Some(held) if held >= revision => {}
                Some(_) => {
                    if let Some(landed) = fold.messages.get_mut(&key) {
                        landed.key.revision = revision;
                        landed.record = record;
                    }
                }
                None => {
                    let landed = LandedMessageV1 {
                        key: ConversationRecordKeyV1 {
                            workspace_id: String::new(),
                            channel_id: record.channel_id.clone(),
                            message_ts: record.message_ts.clone(),
                            revision,
                        },
                        record,
                        tombstoned: false,
                        tombstoned_at: None,
                    };
                    fold.messages.insert(key.clone(), landed);
                    // Drain anything that was waiting on this message. Held
                    // entries were journaled in arrival order, so replaying
                    // them in that order reproduces exactly the state a
                    // well-ordered stream would have produced.
                    if let Some(held) = fold.pending.remove(&key) {
                        for entry in held {
                            apply_revision(fold, &key, entry);
                        }
                    }
                }
            }
        }
        other => apply_revision(fold, &key, other),
    }
}

/// Apply one revision or tombstone, holding it when its message is absent.
///
/// Split from [`apply`] because it is called twice: once when the entry is read
/// from the journal, and once more when the message it was waiting for finally
/// lands.
fn apply_revision(fold: &mut ChannelFold, key: &str, entry: JournalEntryV1) {
    match entry {
        JournalEntryV1::Revision(revision) => match fold.messages.get_mut(key) {
            Some(landed) if revision.revision > landed.key.revision => {
                landed.key.revision = revision.revision;
                landed.record.revision = revision.revision;
                if let Some(text) = revision.text {
                    landed.record.text = text;
                }
                if revision.edited_ts.is_some() {
                    landed.record.edited_ts = revision.edited_ts;
                }
                fold.revisions_applied += 1;
            }
            Some(_) => {}
            None => fold
                .pending
                .entry(key.to_string())
                .or_default()
                .push(JournalEntryV1::Revision(revision)),
        },
        JournalEntryV1::Tombstone(tombstone) => match fold.messages.get_mut(key) {
            Some(landed) if tombstone.revision > landed.key.revision => {
                landed.key.revision = tombstone.revision;
                landed.record.revision = tombstone.revision;
                landed.tombstoned = true;
                landed.tombstoned_at = Some(tombstone.observed_at);
            }
            Some(_) => {}
            None => fold
                .pending
                .entry(key.to_string())
                .or_default()
                .push(JournalEntryV1::Tombstone(tombstone)),
        },
        // A message reaches `apply` directly and never routes through here.
        JournalEntryV1::Message(_) => {}
    }
}

/// Derive the cursor from a fold. Pure: the same journal always produces the
/// same cursor, which is why the checkpoint can be thrown away and rebuilt.
fn derive_cursor(channel_id: &str, fold: &ChannelFold) -> ChannelCursorV1 {
    let watermark = fold
        .messages
        .values()
        .next_back()
        .map(|landed| landed.record.message_ts.clone());
    let oldest = fold
        .messages
        .values()
        .next()
        .map(|landed| landed.record.message_ts.clone());

    // The thread parents the corpus knows about: every parent a landed reply
    // names. S3 refines this with per-parent reply watermarks; in S1 it is the
    // honest corpus-side answer to "which threads exist here", and it is what a
    // restarting producer needs in order to resume a thread sweep without
    // producer-side durable state.
    let mut parents: BTreeMap<String, String> = BTreeMap::new();
    for landed in fold.messages.values() {
        if landed.tombstoned {
            continue;
        }
        if let Some(parent) = &landed.record.thread_parent_ts
            && let Some(key) = message_ts_order_key(parent)
        {
            parents.insert(key, parent.clone());
        }
    }
    // When a pathological channel exceeds the bound, KEEP the newest and drop
    // the oldest: an old thread is the one least likely to still be taking
    // replies, so the dropped end is the cheap one.
    let mut inflight: Vec<String> = parents.into_values().collect();
    if inflight.len() > MAX_INFLIGHT_THREAD_PARENTS {
        inflight = inflight.split_off(inflight.len() - MAX_INFLIGHT_THREAD_PARENTS);
    }

    ChannelCursorV1 {
        channel_id: channel_id.to_string(),
        history_watermark: watermark,
        oldest_observed: oldest,
        inflight_thread_parents: inflight,
        landed_records: fold.messages.len() as u64,
        revisions: fold.revisions_applied,
        tombstones: fold
            .messages
            .values()
            .filter(|landed| landed.tombstoned)
            .count() as u64,
        last_batch_at: fold.last_observed_at.clone(),
    }
}

/// The epoch-seconds half of a message timestamp, for lag arithmetic.
fn epoch_seconds_of(message_ts: &str) -> Option<i64> {
    message_ts.split('.').next()?.parse::<i64>().ok()
}

// ---------------------------------------------------------------------------
// Durable write helpers
// ---------------------------------------------------------------------------

/// Append NDJSON lines and fsync them. Step 1 of every accepted write.
///
/// One `write_all` for the whole batch rather than one per record: a torn
/// append then has exactly one boundary to recover, and the fold's
/// last-good-line rule finds it.
fn append_journal(path: &Path, entries: &[JournalEntryV1]) -> Result<()> {
    use std::io::Write as _;
    let parent = path.parent().context("journal append needs a parent")?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let mut buffer = Vec::new();
    for entry in entries {
        serde_json::to_writer(&mut buffer, entry).context("encoding journal entry")?;
        buffer.push(b'\n');
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    file.write_all(&buffer)
        .with_context(|| format!("appending to {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing {}", path.display()))?;
    if let Ok(dir) = std::fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Cut a torn tail back to the last complete line and make the cut durable.
fn truncate_durable(path: &Path, good_len: u64) -> Result<()> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .with_context(|| format!("opening {} to truncate", path.display()))?;
    file.set_len(good_len)
        .with_context(|| format!("truncating {} to {good_len}", path.display()))?;
    file.sync_all()?;
    tracing::warn!(
        path = %path.display(),
        good_len,
        "discarded a torn conversation journal tail; the producer re-sends those records \
         because the cursor never advanced past them"
    );
    Ok(())
}

fn file_len(path: &Path) -> Result<u64> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error).context(format!("stat {}", path.display())),
    }
}

/// Crash-safe replacement: staged sibling, fsync, rename, parent fsync.
///
/// TWIN: `bbox-file-source-store::write_durable` carries the same discipline
/// for the manifest lane. Deliberately duplicated rather than extracted,
/// because extracting it would mean modifying that store, which this pass does
/// not touch.
fn write_durable(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    let parent = path.parent().context("durable write needs a parent")?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    {
        let mut file = std::fs::File::create(&temporary)
            .with_context(|| format!("creating {}", temporary.display()))?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&temporary, path).with_context(|| format!("publishing {}", path.display()))?;
    if let Ok(dir) = std::fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// TWIN: see [`write_durable`].
fn write_durable_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value).context("encoding durable record")?;
    write_durable(path, &bytes)
}

fn read_json_optional<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes)
                .with_context(|| format!("decoding {}", path.display()))?,
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context(format!("reading {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bbox_conversation_source::{
        AuthorKindV1, CONVERSATION_POLICY_VERSION, SCHEMA_VERSION, batch_digest,
    };

    const SOURCE_ID: &str = "csrc_5f2c1d9a4b6e470e";
    const WORKSPACE: &str = "T0FIXTURE01";
    const OTHER_WORKSPACE: &str = "T0INTRUDER";
    const CHANNEL: &str = "C0FIXTURE01";

    fn scope() -> ConnectorScope {
        ConnectorScope::try_new(SOURCE_ID, "fixture").unwrap()
    }

    /// Canonicalized per the repo test-isolation invariant: on macOS a raw
    /// tempdir path is `/var/...` while the code under test resolves
    /// `/private/var/...`, and a raw-path assertion then silently misses.
    fn store(dir: &tempfile::TempDir) -> ConversationSourceStore {
        let root = dir.path().canonicalize().unwrap();
        ConversationSourceStore::open(root.join("conversation-sources")).unwrap()
    }

    fn bound_store(dir: &tempfile::TempDir) -> ConversationSourceStore {
        let store = store(dir);
        store
            .bind_workspace(&scope(), WORKSPACE, "2026-08-13T00:00:00Z")
            .unwrap();
        store
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
            reactions: Vec::new(),
            attachments: Vec::new(),
            observed_at: "2026-08-13T00:00:00Z".into(),
        }
    }

    fn reply(ts: &str, parent: &str) -> ConversationMessageRecordV1 {
        let mut record = record(ts, "a reply");
        record.thread_parent_ts = Some(parent.into());
        record
    }

    fn batch_with(
        workspace_id: &str,
        records: Vec<ConversationMessageRecordV1>,
    ) -> ConversationBatchV1 {
        ConversationBatchV1 {
            schema_version: SCHEMA_VERSION,
            conversation_policy_version: CONVERSATION_POLICY_VERSION.into(),
            scope: scope(),
            workspace_id: workspace_id.into(),
            channel_id: CHANNEL.into(),
            batch_digest: batch_digest(&records),
            records,
            observed_at: "2026-08-13T00:00:05Z".into(),
        }
    }

    fn batch(records: Vec<ConversationMessageRecordV1>) -> ConversationBatchV1 {
        batch_with(WORKSPACE, records)
    }

    fn revisions(entries: Vec<ConversationRevisionV1>) -> ConversationRevisionsRequestV1 {
        ConversationRevisionsRequestV1 {
            schema_version: SCHEMA_VERSION,
            conversation_policy_version: CONVERSATION_POLICY_VERSION.into(),
            scope: scope(),
            workspace_id: WORKSPACE.into(),
            channel_id: CHANNEL.into(),
            revisions: entries,
            observed_at: "2026-08-13T00:01:00Z".into(),
        }
    }

    fn journal_path(store: &ConversationSourceStore) -> PathBuf {
        store
            .channel_dir(&scope(), WORKSPACE, CHANNEL)
            .join("journal.ndjson")
    }

    // -- S1 gate: batches land idempotently -------------------------------

    #[test]
    fn a_batch_lands_and_the_cursor_answers_the_servers_truth() {
        let dir = tempfile::tempdir().unwrap();
        let store = bound_store(&dir);
        let receipt = store
            .land_batch(
                &scope(),
                &batch(vec![
                    record("1755000000.000100", "first"),
                    record("1755000001.000200", "second"),
                ]),
            )
            .unwrap();
        assert_eq!(receipt.accepted, 2);
        assert_eq!(receipt.duplicates, 0);
        assert_eq!(
            receipt.cursor.history_watermark.as_deref(),
            Some("1755000001.000200")
        );
        assert_eq!(
            receipt.cursor.oldest_observed.as_deref(),
            Some("1755000000.000100")
        );
        assert_eq!(receipt.cursor.landed_records, 2);

        // The cursor read agrees with the receipt: the producer asks the
        // server where to resume and gets one answer, not two.
        let read = store.cursor(&scope(), WORKSPACE, CHANNEL).unwrap();
        assert_eq!(read, receipt.cursor);
    }

    #[test]
    fn a_replayed_batch_does_not_double_land() {
        let dir = tempfile::tempdir().unwrap();
        let store = bound_store(&dir);
        let batch = batch(vec![
            record("1755000000.000100", "first"),
            record("1755000001.000200", "second"),
        ]);
        store.land_batch(&scope(), &batch).unwrap();
        let bytes_after_first = file_len(&journal_path(&store)).unwrap();

        let replay = store.land_batch(&scope(), &batch).unwrap();
        assert_eq!(replay.accepted, 0, "a replay lands nothing");
        assert_eq!(replay.duplicates, 2, "and says so, so a resume is visible");
        assert_eq!(replay.cursor.landed_records, 2);
        assert_eq!(
            file_len(&journal_path(&store)).unwrap(),
            bytes_after_first,
            "a replayed batch must grow the journal by zero bytes"
        );

        // An OVERLAPPING resweep from an older watermark is the ordinary shape
        // of a resume and must land only the genuinely new tail.
        let overlapping = batch(vec![
            record("1755000001.000200", "second"),
            record("1755000002.000300", "third"),
        ]);
        let receipt = store.land_batch(&scope(), &overlapping).unwrap();
        assert_eq!(receipt.accepted, 1);
        assert_eq!(receipt.duplicates, 1);
        assert_eq!(receipt.cursor.landed_records, 3);
    }

    #[test]
    fn an_empty_batch_lands_nothing_and_moves_no_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let store = bound_store(&dir);
        store
            .land_batch(&scope(), &batch(vec![record("1755000000.000100", "first")]))
            .unwrap();
        let before = store.cursor(&scope(), WORKSPACE, CHANNEL).unwrap();
        let receipt = store.land_batch(&scope(), &batch(Vec::new())).unwrap();
        assert_eq!(receipt.accepted, 0);
        assert_eq!(receipt.cursor.history_watermark, before.history_watermark);
    }

    // -- S1 gate: a tampered batch is refused before any durable write -----

    #[test]
    fn a_batch_whose_workspace_disagrees_with_the_scope_is_refused_before_any_write() {
        let dir = tempfile::tempdir().unwrap();
        let store = bound_store(&dir);
        let error = store
            .land_batch(
                &scope(),
                &batch_with(
                    OTHER_WORKSPACE,
                    vec![record("1755000000.000100", "smuggled")],
                ),
            )
            .expect_err("a granted scope may not land into another workspace");
        assert!(
            error.to_string().contains("workspace"),
            "the refusal must name the disagreement: {error}"
        );
        // The durable proof: nothing was written under EITHER workspace.
        assert_eq!(file_len(&journal_path(&store)).unwrap(), 0);
        assert!(
            !store
                .channel_dir(&scope(), OTHER_WORKSPACE, CHANNEL)
                .join("journal.ndjson")
                .exists()
        );
    }

    #[test]
    fn a_batch_whose_records_name_another_channel_is_refused_before_any_write() {
        let dir = tempfile::tempdir().unwrap();
        let store = bound_store(&dir);
        let mut smuggled = record("1755000000.000100", "smuggled");
        smuggled.channel_id = "C0ELSEWHERE".into();
        let records = vec![smuggled];
        let tampered = ConversationBatchV1 {
            batch_digest: batch_digest(&records),
            records,
            ..batch(Vec::new())
        };
        assert!(store.land_batch(&scope(), &tampered).is_err());
        assert_eq!(file_len(&journal_path(&store)).unwrap(), 0);
    }

    #[test]
    fn a_rewritten_batch_fails_its_digest_before_any_write() {
        let dir = tempfile::tempdir().unwrap();
        let store = bound_store(&dir);
        let mut tampered = batch(vec![record("1755000000.000100", "original")]);
        tampered.records[0].text = "rewritten in flight".into();
        assert!(store.land_batch(&scope(), &tampered).is_err());
        assert_eq!(file_len(&journal_path(&store)).unwrap(), 0);
    }

    #[test]
    fn an_unbound_scope_cannot_land_anything() {
        // Onboarding binds the workspace. Until it has, there is no durable
        // statement of which workspace this scope observes, so there is nothing
        // to check a batch against and landing must refuse.
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let error = store
            .land_batch(&scope(), &batch(vec![record("1755000000.000100", "x")]))
            .expect_err("an unbound scope has no workspace to land into");
        assert!(error.to_string().contains("onboard"));
    }

    #[test]
    fn a_scope_cannot_be_rebound_to_a_second_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let store = bound_store(&dir);
        // Idempotent for the same workspace.
        store
            .bind_workspace(&scope(), WORKSPACE, "2026-08-13T01:00:00Z")
            .unwrap();
        assert!(
            store
                .bind_workspace(&scope(), OTHER_WORKSPACE, "2026-08-13T01:00:00Z")
                .is_err()
        );
    }

    // -- revisions and tombstones -----------------------------------------

    #[test]
    fn an_edit_supersedes_in_place_and_keeps_its_identity() {
        let dir = tempfile::tempdir().unwrap();
        let store = bound_store(&dir);
        store
            .land_batch(
                &scope(),
                &batch(vec![record("1755000000.000100", "before")]),
            )
            .unwrap();
        let receipt = store
            .land_revisions(
                &scope(),
                &revisions(vec![ConversationRevisionV1::Edit(
                    ConversationRevisionRecordV1 {
                        channel_id: CHANNEL.into(),
                        message_ts: "1755000000.000100".into(),
                        revision: 1,
                        text: Some("after".into()),
                        edited_ts: Some("1755000100.000000".into()),
                        observed_at: "2026-08-13T00:01:00Z".into(),
                    },
                )]),
            )
            .unwrap();
        assert_eq!(receipt.accepted, 1);
        assert_eq!(
            receipt.cursor.landed_records, 1,
            "an edit updates in place; it does not duplicate the message"
        );

        let landed = store.landed_messages(&scope(), WORKSPACE, CHANNEL).unwrap();
        assert_eq!(landed.len(), 1);
        assert_eq!(landed[0].record.text, "after");
        assert_eq!(landed[0].key.message_ts, "1755000000.000100");
        assert_eq!(landed[0].key.revision, 1);
        assert!(!landed[0].tombstoned);
        // The complete durable key, workspace included, even though the
        // journal does not repeat the workspace on every line.
        assert_eq!(landed[0].key.workspace_id, WORKSPACE);
        assert_eq!(landed[0].key.channel_id, CHANNEL);
    }

    #[test]
    fn a_tombstone_marks_rather_than_erases() {
        let dir = tempfile::tempdir().unwrap();
        let store = bound_store(&dir);
        store
            .land_batch(&scope(), &batch(vec![record("1755000000.000100", "gone")]))
            .unwrap();
        let receipt = store
            .land_revisions(
                &scope(),
                &revisions(vec![ConversationRevisionV1::Tombstone(
                    ConversationTombstoneRecordV1 {
                        channel_id: CHANNEL.into(),
                        message_ts: "1755000000.000100".into(),
                        revision: 1,
                        observed_at: "2026-08-13T00:01:00Z".into(),
                    },
                )]),
            )
            .unwrap();
        assert_eq!(receipt.accepted, 1);
        assert_eq!(receipt.cursor.tombstones, 1);

        let landed = store.landed_messages(&scope(), WORKSPACE, CHANNEL).unwrap();
        assert_eq!(
            landed.len(),
            1,
            "the record survives so a redaction stays distinguishable from an append"
        );
        assert!(landed[0].tombstoned);
        assert_eq!(
            landed[0].tombstoned_at.as_deref(),
            Some("2026-08-13T00:01:00Z")
        );
        assert_eq!(
            landed[0].record.text, "gone",
            "S4 owns applying the deletion; S1 only records it"
        );
    }

    #[test]
    fn a_replayed_revision_does_not_double_apply() {
        let dir = tempfile::tempdir().unwrap();
        let store = bound_store(&dir);
        store
            .land_batch(
                &scope(),
                &batch(vec![record("1755000000.000100", "before")]),
            )
            .unwrap();
        let request = revisions(vec![ConversationRevisionV1::Edit(
            ConversationRevisionRecordV1 {
                channel_id: CHANNEL.into(),
                message_ts: "1755000000.000100".into(),
                revision: 1,
                text: Some("after".into()),
                edited_ts: None,
                observed_at: "2026-08-13T00:01:00Z".into(),
            },
        )]);
        store.land_revisions(&scope(), &request).unwrap();
        let bytes = file_len(&journal_path(&store)).unwrap();
        let replay = store.land_revisions(&scope(), &request).unwrap();
        assert_eq!(replay.accepted, 0);
        assert_eq!(replay.duplicates, 1);
        assert_eq!(file_len(&journal_path(&store)).unwrap(), bytes);
    }

    fn tombstone_for(ts: &str) -> ConversationRevisionV1 {
        ConversationRevisionV1::Tombstone(ConversationTombstoneRecordV1 {
            channel_id: CHANNEL.into(),
            message_ts: ts.into(),
            revision: 1,
            observed_at: "2026-08-13T00:01:00Z".into(),
        })
    }

    #[test]
    fn a_revision_for_a_message_this_corpus_never_landed_is_reported_not_refused() {
        // An observer whose visibility began after a message was posted can
        // legitimately see only its deletion. Refusing would make it retry
        // forever; silence would hide a real coverage gap.
        let dir = tempfile::tempdir().unwrap();
        let store = bound_store(&dir);
        let receipt = store
            .land_revisions(
                &scope(),
                &revisions(vec![tombstone_for("1700000000.000001")]),
            )
            .unwrap();
        assert_eq!(receipt.unknown_messages, 1);
        assert_eq!(receipt.accepted, 0);
        assert_eq!(receipt.cursor.landed_records, 0);

        // Reported is not the same as discarded: it was journaled, so a replay
        // recognizes it rather than counting it unknown a second time.
        let replay = store
            .land_revisions(
                &scope(),
                &revisions(vec![tombstone_for("1700000000.000001")]),
            )
            .unwrap();
        assert_eq!(replay.unknown_messages, 0);
        assert_eq!(replay.duplicates, 1);
    }

    #[test]
    fn a_deletion_observed_before_its_message_still_sticks_when_the_message_lands() {
        // Arrival order must not decide whether a redaction holds. A backfill
        // lane running behind steady state makes this ordinary, not exotic:
        // reconciliation can see a message vanish before backfill has reached
        // the message itself.
        let dir = tempfile::tempdir().unwrap();
        let store = bound_store(&dir);
        let ts = "1755000000.000100";
        let held = store
            .land_revisions(&scope(), &revisions(vec![tombstone_for(ts)]))
            .unwrap();
        assert_eq!(held.unknown_messages, 1);

        // The message arrives afterwards, from the slower lane.
        let receipt = store
            .land_batch(
                &scope(),
                &batch(vec![record(ts, "deleted before we saw it")]),
            )
            .unwrap();
        assert_eq!(receipt.accepted, 1);
        assert_eq!(
            receipt.cursor.tombstones, 1,
            "the held tombstone applies the moment its message lands"
        );

        let landed = store.landed_messages(&scope(), WORKSPACE, CHANNEL).unwrap();
        assert_eq!(landed.len(), 1);
        assert!(
            landed[0].tombstoned,
            "an out-of-order deletion is held, not lost"
        );
        assert_eq!(landed[0].key.revision, 1);

        // And it survives a reopen, because the hold lives in the journal
        // rather than in memory. Opened directly rather than through the
        // helper, which the local binding above shadows.
        let root = dir.path().canonicalize().unwrap();
        let reopened = ConversationSourceStore::open(root.join("conversation-sources")).unwrap();
        assert!(
            reopened
                .landed_messages(&scope(), WORKSPACE, CHANNEL)
                .unwrap()[0]
                .tombstoned
        );
    }

    // -- cursors, threads, status -----------------------------------------

    #[test]
    fn the_cursor_reports_thread_parents_as_a_message_id_set() {
        let dir = tempfile::tempdir().unwrap();
        let store = bound_store(&dir);
        store
            .land_batch(
                &scope(),
                &batch(vec![
                    record("1755000000.000100", "parent"),
                    reply("1755000001.000200", "1755000000.000100"),
                    reply("1755000002.000300", "1755000000.000100"),
                ]),
            )
            .unwrap();
        let cursor = store.cursor(&scope(), WORKSPACE, CHANNEL).unwrap();
        assert_eq!(
            cursor.inflight_thread_parents,
            vec!["1755000000.000100".to_string()],
            "one parent, however many replies name it"
        );
    }

    #[test]
    fn status_reports_lag_from_the_newest_landed_message() {
        let dir = tempfile::tempdir().unwrap();
        let store = bound_store(&dir);
        store
            .record_roster(
                &scope(),
                WORKSPACE,
                &[ChannelObservationV1 {
                    channel_id: CHANNEL.into(),
                    observed_name: Some("ops".into()),
                    class: ChannelClassV1::Private,
                    is_member: true,
                    observed_at: "2026-08-13T00:00:00Z".into(),
                }],
            )
            .unwrap();
        store
            .land_batch(&scope(), &batch(vec![record("1755000000.000100", "hi")]))
            .unwrap();

        let status = store.status(&scope(), 1_755_000_060).unwrap();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].channel_id, CHANNEL);
        assert_eq!(status[0].observed_name.as_deref(), Some("ops"));
        assert_eq!(status[0].class, ChannelClassV1::Private);
        assert_eq!(status[0].lag_seconds, Some(60));
        assert_eq!(status[0].landed_records, 1);
    }

    #[test]
    fn an_empty_channel_reports_no_lag_rather_than_zero_lag() {
        // Zero lag reads as healthy. A channel that has landed nothing is not
        // healthy, it is unobserved, and the status surface must not blur them.
        let dir = tempfile::tempdir().unwrap();
        let store = bound_store(&dir);
        store
            .record_roster(
                &scope(),
                WORKSPACE,
                &[ChannelObservationV1 {
                    channel_id: CHANNEL.into(),
                    observed_name: None,
                    class: ChannelClassV1::Public,
                    is_member: true,
                    observed_at: "2026-08-13T00:00:00Z".into(),
                }],
            )
            .unwrap();
        let status = store.status(&scope(), 1_755_000_060).unwrap();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].lag_seconds, None);
        assert_eq!(status[0].landed_records, 0);
    }

    #[test]
    fn a_renamed_channel_keeps_every_landed_record() {
        // Names are observations; ids are identity. A rename must be a label
        // update, never a new channel.
        let dir = tempfile::tempdir().unwrap();
        let store = bound_store(&dir);
        let observation = |name: &str| ChannelObservationV1 {
            channel_id: CHANNEL.into(),
            observed_name: Some(name.into()),
            class: ChannelClassV1::Public,
            is_member: true,
            observed_at: "2026-08-13T00:00:00Z".into(),
        };
        store
            .record_roster(&scope(), WORKSPACE, &[observation("ops")])
            .unwrap();
        store
            .land_batch(&scope(), &batch(vec![record("1755000000.000100", "hi")]))
            .unwrap();
        store
            .record_roster(&scope(), WORKSPACE, &[observation("ops-archive")])
            .unwrap();

        let cursors = store.cursors(&scope()).unwrap();
        assert_eq!(cursors.len(), 1, "one channel, not two");
        assert_eq!(cursors[0].landed_records, 1);
        let status = store.status(&scope(), 1_755_000_000).unwrap();
        assert_eq!(status[0].observed_name.as_deref(), Some("ops-archive"));
    }

    // -- the storage crash story ------------------------------------------

    #[test]
    fn a_checkpoint_lost_after_an_accepted_batch_self_heals_from_the_journal() {
        // The crash window between step 1 (journal fsync) and step 2
        // (checkpoint replace). The journal is the authority; the cursor
        // catches up on the next touch.
        let dir = tempfile::tempdir().unwrap();
        let store = bound_store(&dir);
        store
            .land_batch(
                &scope(),
                &batch(vec![
                    record("1755000000.000100", "first"),
                    record("1755000001.000200", "second"),
                ]),
            )
            .unwrap();
        let checkpoint = journal_path(&store).with_file_name("checkpoint.json");
        std::fs::remove_file(&checkpoint).unwrap();

        let cursor = store.cursor(&scope(), WORKSPACE, CHANNEL).unwrap();
        assert_eq!(cursor.landed_records, 2);
        assert_eq!(
            cursor.history_watermark.as_deref(),
            Some("1755000001.000200")
        );
        assert!(
            checkpoint.is_file(),
            "the read repairs the checkpoint rather than refolding forever"
        );
    }

    #[test]
    fn a_stale_checkpoint_never_leads_the_journal() {
        // The same window, but the checkpoint SURVIVED at an older value: it
        // claims a shorter journal than the file holds. The length comparison
        // catches it and the journal wins.
        let dir = tempfile::tempdir().unwrap();
        let store = bound_store(&dir);
        store
            .land_batch(&scope(), &batch(vec![record("1755000000.000100", "first")]))
            .unwrap();
        let checkpoint_path = journal_path(&store).with_file_name("checkpoint.json");
        let stale = ChannelCheckpointV1 {
            journal_len: 1,
            cursor: ChannelCursorV1 {
                channel_id: CHANNEL.into(),
                history_watermark: Some("1700000000.000000".into()),
                landed_records: 0,
                ..Default::default()
            },
        };
        write_durable_json(&checkpoint_path, &stale).unwrap();

        let cursor = store.cursor(&scope(), WORKSPACE, CHANNEL).unwrap();
        assert_eq!(
            cursor.history_watermark.as_deref(),
            Some("1755000000.000100"),
            "the checkpoint is derived; a disagreement is resolved toward the journal"
        );
        assert_eq!(cursor.landed_records, 1);
    }

    #[test]
    fn a_torn_journal_tail_is_discarded_and_the_next_write_recovers() {
        // A crash mid-append. The partial line is not a record and must never
        // be folded; the next write cuts back to the last complete line so the
        // partial tail and the new line cannot fuse into corruption.
        let dir = tempfile::tempdir().unwrap();
        let store = bound_store(&dir);
        store
            .land_batch(&scope(), &batch(vec![record("1755000000.000100", "first")]))
            .unwrap();

        let journal = journal_path(&store);
        let good_len = file_len(&journal).unwrap();
        {
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&journal)
                .unwrap();
            file.write_all(br#"{"entry":"message","channel_id":"C0FIX"#)
                .unwrap();
            file.sync_all().unwrap();
        }

        // The fold sees one record and knows where the good bytes end.
        let fold = fold_journal(&journal).unwrap();
        assert_eq!(fold.messages.len(), 1);
        assert!(fold.torn_tail);
        assert_eq!(fold.good_len, good_len);

        // And the cursor answers from the complete lines only.
        let cursor = store.cursor(&scope(), WORKSPACE, CHANNEL).unwrap();
        assert_eq!(cursor.landed_records, 1);

        // The next accepted write truncates the tear away and lands cleanly.
        let receipt = store
            .land_batch(&scope(), &batch(vec![record("1755000002.000300", "third")]))
            .unwrap();
        assert_eq!(receipt.accepted, 1);
        assert_eq!(receipt.cursor.landed_records, 2);
        let after = fold_journal(&journal).unwrap();
        assert!(!after.torn_tail, "the tear is gone, not carried forward");
        assert_eq!(after.messages.len(), 2);
    }

    #[test]
    fn an_unparseable_line_in_the_middle_of_a_journal_refuses_loudly() {
        // Distinct from a torn tail on purpose: discarding a mid-file line
        // would silently drop every record after it, which is data loss dressed
        // up as recovery.
        let dir = tempfile::tempdir().unwrap();
        let store = bound_store(&dir);
        store
            .land_batch(
                &scope(),
                &batch(vec![
                    record("1755000000.000100", "first"),
                    record("1755000001.000200", "second"),
                ]),
            )
            .unwrap();
        let journal = journal_path(&store);
        let mut bytes = std::fs::read(&journal).unwrap();
        let first_newline = bytes.iter().position(|byte| *byte == b'\n').unwrap();
        bytes.splice(
            first_newline + 1..first_newline + 1,
            b"not json\n".iter().copied(),
        );
        std::fs::write(&journal, &bytes).unwrap();

        let error = fold_journal(&journal).expect_err("mid-file corruption must refuse");
        assert!(error.to_string().contains("corruption"), "{error}");
    }

    #[test]
    fn a_journal_that_never_existed_folds_to_an_empty_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let store = bound_store(&dir);
        let cursor = store.cursor(&scope(), WORKSPACE, CHANNEL).unwrap();
        assert_eq!(cursor.channel_id, CHANNEL);
        assert_eq!(cursor.landed_records, 0);
        assert_eq!(cursor.history_watermark, None);
        assert!(cursor.inflight_thread_parents.is_empty());
    }

    #[test]
    fn a_reopened_store_answers_exactly_what_the_previous_process_landed() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = bound_store(&dir);
            store
                .land_batch(&scope(), &batch(vec![record("1755000000.000100", "first")]))
                .unwrap();
        }
        let reopened = store(&dir);
        let cursor = reopened.cursor(&scope(), WORKSPACE, CHANNEL).unwrap();
        assert_eq!(cursor.landed_records, 1);
        assert_eq!(
            reopened
                .workspace_binding(&scope())
                .unwrap()
                .unwrap()
                .workspace_id,
            WORKSPACE
        );
        assert_eq!(
            reopened.channels(&scope()).unwrap(),
            vec![CHANNEL.to_string()]
        );
    }

    #[test]
    fn message_order_survives_a_provider_changing_its_integer_width() {
        // The reason the fold keys on the order key rather than the raw string.
        let dir = tempfile::tempdir().unwrap();
        let store = bound_store(&dir);
        store
            .land_batch(
                &scope(),
                &batch(vec![
                    record("999.100000", "older"),
                    record("1000.000000", "newer"),
                ]),
            )
            .unwrap();
        let cursor = store.cursor(&scope(), WORKSPACE, CHANNEL).unwrap();
        assert_eq!(cursor.history_watermark.as_deref(), Some("1000.000000"));
        assert_eq!(cursor.oldest_observed.as_deref(), Some("999.100000"));
    }
}
