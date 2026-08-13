//! One publication cycle: observe, decide, acquire, publish.
//!
//! This is the satellite's loop body. It is deliberately written against a
//! [`PublicationSink`] rather than against HTTP directly, for one reason worth
//! stating: the interesting properties of a cycle -- an unchanged entry is
//! never re-acquired, a restart mid-publication resumes instead of
//! re-uploading, an invalidated checkpoint converges on a distinct generation
//! -- are properties of the DECISIONS, not of the transport. A test that has
//! to stand up a socket to observe them is a test that will eventually be
//! deleted for being slow.
//!
//! # The three properties the loop exists to hold
//!
//! **An unchanged entry is never re-acquired.** [`Journal::needs_acquire`] is
//! the single condition, and it is deliberately one condition rather than two:
//! "fetch only changed bytes" and "never re-export an unchanged native
//! document" are the same rule, because a native document's export is exactly
//! the acquisition its remote version gates.
//!
//! **A restart mid-publication resumes.** The server recomputes the missing
//! blob set from its own content-addressed store, so it is authority on what
//! is still owed. That means the producer must be able to supply a blob it did
//! not fetch THIS cycle: [`resolve_blob`] falls back to re-fetching from the
//! remote by the journal's `remote_id`. Without that fallback a resumed cycle
//! would present a manifest it could not complete, which is worse than not
//! resuming at all.
//!
//! **An invalidated checkpoint converges.** Degradation is never absorbed
//! silently: the journal's `cursor_epoch` increments durably, the descriptor
//! carries it, and the generation id folds it in, so the re-enumerated
//! generation is DISTINCT from the one before it even when the manifest is
//! byte-identical. The cause and the cost travel to publication status so an
//! operator watching repeated increments is watching a real problem.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use bbox_file_source::{
    BeginFileUploadRequestV1, BeginFileUploadResponseV1, CursorDegradationV1,
    FileGenerationDescriptorV1, FileGenerationStatusV1, FileManifestEntryV1,
    FinalizeFileUploadResponseV1, MissingFileBlobsPageV1, PublicationTelemetryV1,
};
use sha2::{Digest, Sha256};

use crate::connector::{
    CheckpointSet, FetchOutcome, Observation, RemoteEntry, RemoteEntryKind, RemoteSourceConnector,
};
use crate::journal::{Journal, JournalEntry, JournalState};
use crate::logical::{AssignmentInput, assign_logical_paths, display_name};
use crate::policy::{CompiledPolicy, PolicyDecision, SkipCounters};

/// The server side of one publication, as the cycle needs it.
///
/// Mirrors the `/internal/file-source/v1/*` verbs one for one. Nothing here
/// knows about bearers, retries, or URLs; the HTTP implementation owns those.
#[async_trait]
pub trait PublicationSink: Send + Sync {
    async fn begin(&self, request: &BeginFileUploadRequestV1) -> Result<BeginFileUploadResponseV1>;
    async fn put_manifest_page(
        &self,
        upload_id: &str,
        page: u64,
        entries: &[FileManifestEntryV1],
    ) -> Result<()>;
    async fn complete_manifest(&self, upload_id: &str) -> Result<MissingFileBlobsPageV1>;
    async fn missing_blobs(
        &self,
        upload_id: &str,
        cursor: Option<&str>,
    ) -> Result<MissingFileBlobsPageV1>;
    async fn put_blob(&self, upload_id: &str, hash: &str, bytes: Vec<u8>) -> Result<()>;
    async fn finalize(&self, upload_id: &str) -> Result<FinalizeFileUploadResponseV1>;
    async fn status(&self, generation_id: &str) -> Result<FileGenerationStatusV1>;
}

/// What one cycle did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CycleOutcome {
    pub generation_id: String,
    pub upload_id: String,
    pub cursor_epoch: u64,
    pub file_count: u64,
    /// Blobs actually read from the remote this cycle. The number an operator
    /// watches to know incremental refresh is working.
    pub blobs_fetched: u64,
    /// Provider-native documents re-exported this cycle. An unchanged native
    /// document must never appear here.
    pub documents_exported: u64,
    /// Blobs the server still lacked after the manifest closed, which is the
    /// count actually uploaded.
    pub blobs_uploaded: u64,
    pub skipped: BTreeMap<String, u64>,
    pub degradation: Option<CursorDegradationV1>,
}

/// Run one publication cycle to a finalized generation.
///
/// `observed_at` is supplied rather than read from the clock inside so a test
/// can pin it; it is opaque, bounded, diagnostic text on the wire.
pub async fn run_publication_cycle(
    connector: &dyn RemoteSourceConnector,
    policy: &CompiledPolicy,
    scope: &bbox_corpus_core::project_catalog::ConnectorScope,
    journal_path: &Path,
    sink: &dyn PublicationSink,
    observed_at: &str,
) -> Result<CycleOutcome> {
    let mut journal = Journal::load(journal_path)?;
    let mut counters = SkipCounters::default();
    let mut fetched: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut blobs_fetched = 0_u64;
    let mut documents_exported = 0_u64;
    let mut entries_enumerated = 0_u64;

    // -- observe, degrading to a full walk when a checkpoint is invalid ----
    let checkpoints = journal.checkpoint_set()?;
    let (batch, degradation) = match connector.observe(&checkpoints).await? {
        Observation::Batch(batch) => (batch, None),
        Observation::CheckpointInvalidated(invalidation) => {
            // The epoch increment is durable BEFORE the re-enumeration runs:
            // a crash mid-walk must not be able to replay the old epoch and
            // mint a generation id that collides with the pre-degradation one.
            let cursor_epoch = journal.invalidate_checkpoints();
            journal.save(journal_path)?;
            tracing::warn!(
                checkpoint = %invalidation.checkpoint_name,
                cause = %invalidation.cause,
                cursor_epoch,
                "connector checkpoint invalidated; degrading to full re-enumeration"
            );
            let batch = match connector
                .observe(&CheckpointSet::full_enumeration())
                .await?
            {
                Observation::Batch(batch) => batch,
                Observation::CheckpointInvalidated(again) => {
                    // A full enumeration cannot itself be checkpoint-invalid.
                    // Refusing loudly beats looping.
                    bail!(
                        "connector invalidated checkpoint {} during a full re-enumeration",
                        again.checkpoint_name
                    );
                }
            };
            (
                batch,
                Some(CursorDegradationV1 {
                    checkpoint_name: invalidation.checkpoint_name,
                    cause: invalidation.cause,
                    cursor_epoch,
                    entries_enumerated: 0,
                    blobs_refetched: 0,
                    documents_reexported: 0,
                    observed_at: observed_at.to_string(),
                }),
            )
        }
    };

    // -- assign logical paths over journal + batch ------------------------
    let removed: Vec<String> = batch
        .entries
        .iter()
        .filter(|entry| entry.deleted)
        .map(|entry| entry.remote_id.clone())
        .collect();
    let live: Vec<&RemoteEntry> = batch
        .entries
        .iter()
        .filter(|entry| !entry.deleted)
        .collect();
    let assignment_inputs: Vec<AssignmentInput> = live
        .iter()
        .map(|entry| {
            let export_extension = (entry.kind == RemoteEntryKind::NativeDocument)
                .then(|| crate::policy::export_extension(entry).to_string());
            AssignmentInput::from_remote(entry, export_extension)
        })
        .collect();
    let logical_paths = assign_logical_paths(&journal, &assignment_inputs, &removed)?;

    // -- decide and acquire ------------------------------------------------
    for entry in live {
        entries_enumerated = entries_enumerated.saturating_add(1);
        let logical_path = logical_paths
            .get(&entry.remote_id)
            .ok_or_else(|| anyhow!("logical assignment omitted remote id {}", entry.remote_id))?;
        let decision = policy.decide(entry, logical_path);
        let (max_bytes, export_extension) = match decision {
            PolicyDecision::Skip { reason } => {
                counters.record(reason);
                // A previously published entry that policy now excludes must
                // leave the manifest, or an operator's new exclude rule would
                // be invisible until the entry happened to change.
                journal.remove(&entry.remote_id);
                continue;
            }
            PolicyDecision::Fetch { max_bytes } => (max_bytes, None),
            PolicyDecision::Export {
                extension,
                max_bytes,
            } => (max_bytes, Some(extension)),
        };

        // THE incremental-refresh gate. An unchanged entry is not fetched and
        // an unchanged native document is not re-exported, because those are
        // the same condition.
        if !journal.needs_acquire(entry) {
            // The path may still have moved (a rename keeps the remote id and
            // the bytes), so the row is refreshed without acquiring anything.
            if let Some(known) = journal.get(&entry.remote_id) {
                let mut refreshed = known.clone();
                refreshed.logical_path = logical_path.clone();
                refreshed.name_path = entry.name_path.clone();
                refreshed.display_name = display_name(&entry.name_path);
                refreshed.remote_url = entry.remote_url.clone();
                journal.upsert(refreshed);
            }
            continue;
        }

        match connector.fetch(entry, max_bytes).await? {
            FetchOutcome::Skipped(reason) => {
                counters.record(leak_reason(&reason));
                journal.remove(&entry.remote_id);
            }
            FetchOutcome::Fetched(content) => {
                blobs_fetched = blobs_fetched.saturating_add(1);
                if export_extension.is_some() {
                    documents_exported = documents_exported.saturating_add(1);
                }
                let content_hash = hex::encode(Sha256::digest(&content.bytes));
                let size = content.bytes.len() as u64;
                fetched.insert(content_hash.clone(), content.bytes);
                journal.upsert(JournalEntry {
                    remote_id: entry.remote_id.clone(),
                    remote_version: entry.remote_version.clone(),
                    logical_path: logical_path.clone(),
                    name_path: entry.name_path.clone(),
                    display_name: display_name(&entry.name_path),
                    export_format: content.export_format.clone().or(export_extension),
                    remote_url: entry.remote_url.clone(),
                    state: JournalState::Published { content_hash, size },
                });
            }
        }
    }

    // -- reconcile removals -----------------------------------------------
    for remote_id in &removed {
        journal.remove(remote_id);
    }
    // Orphan detection is gated on a COMPLETE enumeration. An incremental
    // batch says nothing about entries it did not mention, so pruning on one
    // would delete the whole corpus on the first quiet cycle.
    let seen = batch
        .entries
        .iter()
        .map(|entry| entry.remote_id.clone())
        .collect();
    for orphan in journal.orphans(&seen, batch.complete) {
        journal.remove(&orphan);
    }
    journal.record_checkpoints(&batch.next_checkpoints);

    // -- build the descriptor ---------------------------------------------
    let manifest = journal.manifest()?;
    let sized: Vec<(String, u64)> = manifest
        .iter()
        .map(|entry| (entry.logical_path.clone(), entry.size))
        .collect();
    // A per-source total breach ABORTS loudly naming the offenders rather
    // than truncating a corpus silently.
    policy.check_total(&sized)?;
    let logical_bytes = manifest.iter().try_fold(0_u64, |total, entry| {
        total
            .checked_add(entry.size)
            .ok_or_else(|| anyhow!("connector manifest byte count overflow"))
    })?;
    let descriptor = FileGenerationDescriptorV1 {
        schema_version: bbox_file_source::SCHEMA_VERSION,
        connector_policy_version: bbox_file_source::CONNECTOR_POLICY_VERSION.to_string(),
        scope: scope.clone(),
        remote_watermark: batch.next_checkpoints.watermark(),
        cursor_epoch: journal.cursor_epoch,
        manifest_sha256: bbox_file_source::manifest_sha256(&manifest),
        file_count: manifest.len() as u64,
        logical_bytes,
    };
    let telemetry = PublicationTelemetryV1 {
        skipped: counters.into_map(),
        entries_enumerated,
        blobs_fetched,
        documents_exported,
    };
    let degradation = degradation.map(|mut degradation| {
        degradation.entries_enumerated = entries_enumerated;
        degradation.blobs_refetched = blobs_fetched;
        degradation.documents_reexported = documents_exported;
        degradation
    });

    // The journal is durable BEFORE the publication conversation starts. A
    // crash after this point re-derives the same descriptor, so it resumes
    // the same upload rather than minting a second one for bytes the server
    // may already hold.
    journal.save(journal_path)?;

    // -- publish -----------------------------------------------------------
    let request = BeginFileUploadRequestV1 {
        descriptor,
        telemetry,
        degradation: degradation.clone(),
    };
    request
        .validate_header()
        .map_err(|error| anyhow!("refusing to publish an invalid descriptor: {error}"))?;
    let begun = sink.begin(&request).await.context("beginning the upload")?;
    for (page, chunk) in manifest.chunks(begun.max_page_entries.max(1)).enumerate() {
        sink.put_manifest_page(&begun.upload_id, page as u64, chunk)
            .await
            .with_context(|| format!("delivering manifest page {page}"))?;
    }
    let mut owed = sink
        .complete_manifest(&begun.upload_id)
        .await
        .context("closing the manifest")?;
    let mut blobs_uploaded = 0_u64;
    loop {
        for hash in &owed.hashes {
            let bytes = resolve_blob(connector, &journal, &fetched, hash)
                .await
                .with_context(|| format!("supplying blob {hash}"))?;
            sink.put_blob(&begun.upload_id, hash, bytes)
                .await
                .with_context(|| format!("uploading blob {hash}"))?;
            blobs_uploaded = blobs_uploaded.saturating_add(1);
        }
        let Some(cursor) = owed.next_cursor.clone() else {
            break;
        };
        owed = sink
            .missing_blobs(&begun.upload_id, Some(&cursor))
            .await
            .context("paging the missing-blob cursor")?;
    }
    let finalized = sink
        .finalize(&begun.upload_id)
        .await
        .context("finalizing the upload")?;

    Ok(CycleOutcome {
        generation_id: finalized.generation_id,
        upload_id: begun.upload_id,
        cursor_epoch: journal.cursor_epoch,
        file_count: manifest.len() as u64,
        blobs_fetched,
        documents_exported,
        blobs_uploaded,
        skipped: request.telemetry.skipped.clone(),
        degradation,
    })
}

/// Supply one blob the server says it lacks.
///
/// The cycle's own fetches first, then a re-fetch from the remote. The second
/// arm is what makes a resumed publication work: the server recomputes the
/// missing set from its content-addressed store, so a producer that crashed
/// after fetching and before uploading is asked for bytes it no longer holds
/// in memory. Re-fetching is bounded by the same declared size the manifest
/// carries and the result is re-hashed, so a remote that changed underneath
/// the journal fails here rather than uploading bytes under another entry's
/// digest.
async fn resolve_blob(
    connector: &dyn RemoteSourceConnector,
    journal: &Journal,
    fetched: &BTreeMap<String, Vec<u8>>,
    hash: &str,
) -> Result<Vec<u8>> {
    if let Some(bytes) = fetched.get(hash) {
        return Ok(bytes.clone());
    }
    let (remote_id, entry) = journal
        .entries
        .iter()
        .find(|(_, entry)| {
            entry
                .state
                .published()
                .is_some_and(|(known, _)| known == hash)
        })
        .ok_or_else(|| anyhow!("the server asked for a blob no journal entry claims"))?;
    let (_, size) = entry
        .state
        .published()
        .ok_or_else(|| anyhow!("journal entry {remote_id} claims no published bytes"))?;
    let remote = RemoteEntry {
        remote_id: remote_id.clone(),
        name_path: entry.name_path.clone(),
        kind: if entry.export_format.is_some() {
            RemoteEntryKind::NativeDocument
        } else {
            RemoteEntryKind::File
        },
        remote_version: entry.remote_version.clone(),
        size: Some(size),
        media_type: None,
        remote_url: entry.remote_url.clone(),
        deleted: false,
    };
    match connector.fetch(&remote, size).await? {
        FetchOutcome::Fetched(content) => {
            let actual = hex::encode(Sha256::digest(&content.bytes));
            if actual != hash {
                bail!(
                    "re-fetched blob for {remote_id} hashes to {actual}, not the {hash} the \
                     manifest declares"
                );
            }
            Ok(content.bytes)
        }
        FetchOutcome::Skipped(reason) => {
            bail!("re-fetching {remote_id} to resume a publication was skipped: {reason}")
        }
    }
}

/// Bound a connector-supplied skip reason to the labels the wire accepts.
///
/// Reason labels are opaque and connector-chosen, but the per-reason counter
/// map is bounded, so a connector inventing a fresh label per entry must not
/// be able to present unbounded cardinality as telemetry.
fn leak_reason(reason: &str) -> &'static str {
    match reason {
        crate::policy::REASON_OVERSIZE => crate::policy::REASON_OVERSIZE,
        crate::policy::REASON_EXPORT_OVERSIZE => crate::policy::REASON_EXPORT_OVERSIZE,
        crate::policy::REASON_UNSUPPORTED => crate::policy::REASON_UNSUPPORTED,
        crate::policy::REASON_NATIVE_EXPORT_DISABLED => {
            crate::policy::REASON_NATIVE_EXPORT_DISABLED
        }
        _ => "fetch_skipped",
    }
}
