//! The satellite publication cycle, against a sink that models the server.
//!
//! The sink here is deliberately a MODEL of `/internal/file-source/v1/*`, not
//! a mock that records calls: it keeps a content-addressed blob set and
//! recomputes the missing list from it, exactly as the daemon's store does.
//! That is what makes the resume test meaningful. A mock returning a canned
//! missing-list would pass whether or not the producer could actually supply a
//! blob it no longer held.
//!
//! The crate's dependency ceiling forbids `bbox-file-source-store`, so the
//! model is written here rather than imported. The behaviors it reproduces are
//! the ones the cycle depends on and no others: derive the generation id from
//! the descriptor, replay an identical descriptor onto the same upload,
//! recompute the owed set from the blob store.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use anyhow::{Result, bail};
use async_trait::async_trait;
use bbox_corpus_core::project_catalog::ConnectorScope;
use bbox_file_collector::cycle::{PublicationSink, run_publication_cycle};
use bbox_file_collector::fixture::{FixtureConnector, NATIVE_DOCUMENT_EXTENSION};
use bbox_file_collector::policy::{CompiledPolicy, SourcePolicy};
use bbox_file_source::{
    BeginFileUploadRequestV1, BeginFileUploadResponseV1, FileGenerationStateV1,
    FileGenerationStatusV1, FileManifestEntryV1, FinalizeFileUploadResponseV1,
    MissingFileBlobsPageV1,
};

const SOURCE_ID: &str = "csrc_5f2c1d9a4b6e470e";
const PRODUCER: &str = "producer-a";
const OBSERVED_AT: &str = "2026-08-13T00:00:00Z";

fn scope() -> ConnectorScope {
    ConnectorScope::try_new(SOURCE_ID, "fixture").unwrap()
}

fn policy() -> CompiledPolicy {
    CompiledPolicy::compile(&SourcePolicy::default()).unwrap()
}

#[derive(Default)]
struct SinkState {
    /// The server's content-addressed blob store.
    blobs: BTreeSet<String>,
    /// upload_id -> the manifest received so far.
    manifests: BTreeMap<String, Vec<FileManifestEntryV1>>,
    /// Generation ids that reached finalize.
    finalized: Vec<String>,
    uploads: u64,
    /// When set, the Nth blob upload of a run fails, modelling a producer or
    /// network death mid-publication.
    fail_upload_after: Option<u64>,
}

struct ModelSink {
    state: Mutex<SinkState>,
}

impl ModelSink {
    fn new() -> Self {
        Self {
            state: Mutex::new(SinkState::default()),
        }
    }

    fn failing_after(uploads: u64) -> Self {
        let sink = Self::new();
        sink.state.lock().unwrap().fail_upload_after = Some(uploads);
        sink
    }

    fn holds(&self, hash: &str) -> bool {
        self.state.lock().unwrap().blobs.contains(hash)
    }

    fn blob_count(&self) -> usize {
        self.state.lock().unwrap().blobs.len()
    }

    fn stop_failing(&self) {
        let mut state = self.state.lock().unwrap();
        state.fail_upload_after = None;
        state.uploads = 0;
    }

    fn owed(&self, upload_id: &str) -> Vec<String> {
        let state = self.state.lock().unwrap();
        let mut owed: BTreeSet<String> = BTreeSet::new();
        for entry in state.manifests.get(upload_id).into_iter().flatten() {
            if !state.blobs.contains(&entry.content_sha256) {
                owed.insert(entry.content_sha256.clone());
            }
        }
        owed.into_iter().collect()
    }
}

#[async_trait]
impl PublicationSink for ModelSink {
    async fn begin(&self, request: &BeginFileUploadRequestV1) -> Result<BeginFileUploadResponseV1> {
        request.validate_header().map_err(anyhow::Error::from)?;
        // The generation id is derived server-side from the descriptor, which
        // is exactly what makes an identical descriptor replay onto the same
        // upload instead of minting a second one.
        let generation_id = bbox_file_source::generation_id(PRODUCER, &request.descriptor);
        let upload_id = format!("fsu-{}", &generation_id[..24]);
        let mut state = self.state.lock().unwrap();
        state.manifests.entry(upload_id.clone()).or_default();
        Ok(BeginFileUploadResponseV1 {
            upload_id,
            ordinal: 1,
            max_page_entries: bbox_file_source::MAX_MANIFEST_PAGE_ENTRIES,
            max_page_bytes: bbox_file_source::MAX_MANIFEST_PAGE_BYTES,
        })
    }

    async fn put_manifest_page(
        &self,
        upload_id: &str,
        page: u64,
        entries: &[FileManifestEntryV1],
    ) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        let manifest = state.manifests.entry(upload_id.to_string()).or_default();
        // Page zero replaces, matching the store's redelivery semantics: a
        // retried page is not a duplicated entry set.
        if page == 0 {
            manifest.clear();
        }
        manifest.extend(entries.iter().cloned());
        Ok(())
    }

    async fn complete_manifest(&self, upload_id: &str) -> Result<MissingFileBlobsPageV1> {
        Ok(MissingFileBlobsPageV1 {
            generation_id: upload_id.to_string(),
            hashes: self.owed(upload_id),
            next_cursor: None,
        })
    }

    async fn missing_blobs(
        &self,
        upload_id: &str,
        cursor: Option<&str>,
    ) -> Result<MissingFileBlobsPageV1> {
        let hashes = self
            .owed(upload_id)
            .into_iter()
            .filter(|hash| cursor.is_none_or(|cursor| hash.as_str() > cursor))
            .collect();
        Ok(MissingFileBlobsPageV1 {
            generation_id: upload_id.to_string(),
            hashes,
            next_cursor: None,
        })
    }

    async fn put_blob(&self, _upload_id: &str, hash: &str, bytes: Vec<u8>) -> Result<()> {
        use sha2::{Digest, Sha256};
        if hex::encode(Sha256::digest(&bytes)) != hash {
            bail!("blob content does not match its claimed hash");
        }
        let mut state = self.state.lock().unwrap();
        state.uploads += 1;
        if let Some(limit) = state.fail_upload_after
            && state.uploads > limit
        {
            bail!("modelled producer death mid-publication");
        }
        state.blobs.insert(hash.to_string());
        Ok(())
    }

    async fn finalize(&self, upload_id: &str) -> Result<FinalizeFileUploadResponseV1> {
        let mut state = self.state.lock().unwrap();
        let owed: Vec<String> = state
            .manifests
            .get(upload_id)
            .into_iter()
            .flatten()
            .filter(|entry| !state.blobs.contains(&entry.content_sha256))
            .map(|entry| entry.content_sha256.clone())
            .collect();
        if !owed.is_empty() {
            bail!("upload {upload_id} is missing {} blob(s)", owed.len());
        }
        let generation_id = format!("gen-{}", &upload_id[4..]);
        state.finalized.push(generation_id.clone());
        Ok(FinalizeFileUploadResponseV1 {
            status_url: format!("/internal/file-source/v1/generations/{generation_id}/status"),
            generation_id,
        })
    }

    async fn status(&self, generation_id: &str) -> Result<FileGenerationStatusV1> {
        Ok(FileGenerationStatusV1 {
            generation_id: generation_id.to_string(),
            state: FileGenerationStateV1::Ready,
            file_count: 0,
            logical_bytes: 0,
            cursor_epoch: 0,
            diagnostic: None,
        })
    }
}

/// A fixture remote with two ordinary documents and one provider-native one.
fn remote(root: &std::path::Path) {
    std::fs::create_dir_all(root.join("notes")).unwrap();
    std::fs::write(root.join("notes/plan.md"), b"# Plan\n\nquarterly targets\n").unwrap();
    std::fs::write(
        root.join("notes/handbook.md"),
        b"# Handbook\n\nonboarding\n",
    )
    .unwrap();
    std::fs::write(
        root.join(format!("notes/Runbook.{NATIVE_DOCUMENT_EXTENSION}")),
        b"restart the widget service\n",
    )
    .unwrap();
}

async fn cycle(
    connector: &FixtureConnector,
    journal: &std::path::Path,
    sink: &ModelSink,
) -> bbox_file_collector::CycleOutcome {
    run_publication_cycle(connector, &policy(), &scope(), journal, sink, OBSERVED_AT)
        .await
        .unwrap()
}

#[tokio::test]
async fn a_second_cycle_over_an_unchanged_remote_acquires_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let remote_root = root.join("remote");
    std::fs::create_dir_all(&remote_root).unwrap();
    remote(&remote_root);
    let journal = root.join("journal.json");
    let connector = FixtureConnector::open(&remote_root).unwrap();
    let sink = ModelSink::new();

    let first = cycle(&connector, &journal, &sink).await;
    assert_eq!(first.file_count, 3);
    assert_eq!(first.blobs_fetched, 3, "a cold journal acquires everything");
    assert_eq!(
        first.documents_exported, 1,
        "the provider-native document is exported once"
    );
    assert_eq!(first.blobs_uploaded, 3);

    let second = cycle(&connector, &journal, &sink).await;
    assert_eq!(
        second.blobs_fetched, 0,
        "an unchanged entry is never re-acquired"
    );
    assert_eq!(
        second.documents_exported, 0,
        "and an unchanged native document is never re-exported: it is the \
         same rule, not a second one"
    );
    assert_eq!(
        second.blobs_uploaded, 0,
        "the server already holds them all"
    );
    assert_eq!(
        second.generation_id, first.generation_id,
        "an unchanged corpus is the same generation"
    );
    assert_eq!(second.file_count, 3);
}

#[tokio::test]
async fn a_changed_document_is_reacquired_and_its_siblings_are_not() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let remote_root = root.join("remote");
    std::fs::create_dir_all(&remote_root).unwrap();
    remote(&remote_root);
    let journal = root.join("journal.json");
    let connector = FixtureConnector::open(&remote_root).unwrap();
    let sink = ModelSink::new();

    let first = cycle(&connector, &journal, &sink).await;
    assert_eq!(first.blobs_fetched, 3);

    // The fixture derives its remote version from CONTENT, so this moves the
    // version of exactly one entry.
    std::fs::write(
        remote_root.join("notes/plan.md"),
        b"# Plan\n\nrevised quarterly targets\n",
    )
    .unwrap();

    let second = cycle(&connector, &journal, &sink).await;
    assert_eq!(
        second.blobs_fetched, 1,
        "exactly the changed entry is re-acquired"
    );
    assert_eq!(
        second.documents_exported, 0,
        "the unchanged native document stays unexported"
    );
    assert_eq!(second.file_count, 3, "the corpus is still three documents");
    assert_ne!(
        second.generation_id, first.generation_id,
        "changed bytes are a new generation"
    );
    assert_eq!(
        sink.blob_count(),
        4,
        "the superseded blob is retained; blobs are content addressed and \
         shared across generations"
    );
}

#[tokio::test]
async fn a_restart_between_the_manifest_and_the_blobs_resumes() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let remote_root = root.join("remote");
    std::fs::create_dir_all(&remote_root).unwrap();
    remote(&remote_root);
    let journal = root.join("journal.json");
    let connector = FixtureConnector::open(&remote_root).unwrap();

    // Two blobs land, then the producer dies.
    let sink = ModelSink::failing_after(2);
    let error = run_publication_cycle(
        &connector,
        &policy(),
        &scope(),
        &journal,
        &sink,
        OBSERVED_AT,
    )
    .await
    .expect_err("the modelled death must fail the cycle");
    assert!(error.to_string().contains("supplying blob") || error.chain().count() > 1);
    assert_eq!(sink.blob_count(), 2, "two blobs survived the death");

    // Restart. The journal was durable BEFORE the publication conversation
    // started, so the same descriptor is re-derived and the same upload is
    // resumed.
    sink.stop_failing();
    let resumed = cycle(&connector, &journal, &sink).await;
    assert_eq!(
        resumed.blobs_fetched, 0,
        "the journal already knows these entries, so the REMOTE is not \
         re-enumerated for bytes"
    );
    assert_eq!(
        resumed.blobs_uploaded, 1,
        "only the blob that never landed is uploaded; the server recomputes \
         the owed set from its own store rather than replaying a list"
    );
    assert_eq!(sink.blob_count(), 3);
    assert_eq!(resumed.file_count, 3);
}

#[tokio::test]
async fn an_invalidated_checkpoint_converges_on_a_distinct_generation() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let remote_root = root.join("remote");
    std::fs::create_dir_all(&remote_root).unwrap();
    remote(&remote_root);
    let journal = root.join("journal.json");
    let connector = FixtureConnector::open(&remote_root).unwrap();
    let sink = ModelSink::new();

    let first = cycle(&connector, &journal, &sink).await;
    assert_eq!(first.cursor_epoch, 0);
    assert!(first.degradation.is_none());

    // The vendor expires the delta token. The remote content is UNCHANGED, so
    // the manifest is byte-identical and the generation must still differ:
    // a full re-enumeration is a materially different event from the
    // incremental cycle before it.
    std::fs::write(
        remote_root.join(".fixture-state.json"),
        serde_json::json!({
            "invalidate_checkpoint": "root",
            "invalidate_cause": "checkpoint_expired",
        })
        .to_string(),
    )
    .unwrap();

    let degraded = cycle(&connector, &journal, &sink).await;
    assert_eq!(
        degraded.cursor_epoch, 1,
        "the epoch increment is the operator's signal and it is durable"
    );
    let degradation = degraded
        .degradation
        .as_ref()
        .expect("degradation is never absorbed silently");
    assert_eq!(degradation.cause, "checkpoint_expired");
    assert_eq!(degradation.checkpoint_name, "root");
    assert_eq!(
        degradation.cursor_epoch, degraded.cursor_epoch,
        "the reported epoch is the one the degradation produced"
    );
    assert_ne!(
        degraded.generation_id, first.generation_id,
        "a converged re-enumeration is still a distinct generation"
    );
    assert_eq!(
        degraded.file_count, first.file_count,
        "and it converges on the same corpus"
    );
    assert_eq!(
        degraded.blobs_uploaded, 0,
        "convergence uploads nothing: every blob is already content addressed \
         in the server's store"
    );

    // A third cycle, with the control file still requesting invalidation,
    // must not thrash the epoch upward forever once the caller no longer
    // holds that checkpoint.
    let after = cycle(&connector, &journal, &sink).await;
    assert!(
        after.cursor_epoch >= degraded.cursor_epoch,
        "the epoch is monotonic"
    );
    assert_eq!(after.file_count, first.file_count);
}

#[tokio::test]
async fn a_removed_document_leaves_the_manifest_on_a_complete_enumeration() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let remote_root = root.join("remote");
    std::fs::create_dir_all(&remote_root).unwrap();
    remote(&remote_root);
    let journal = root.join("journal.json");
    let connector = FixtureConnector::open(&remote_root).unwrap();
    let sink = ModelSink::new();

    let first = cycle(&connector, &journal, &sink).await;
    assert_eq!(first.file_count, 3);

    std::fs::remove_file(remote_root.join("notes/handbook.md")).unwrap();
    let second = cycle(&connector, &journal, &sink).await;
    assert_eq!(
        second.file_count, 2,
        "an orphan is pruned once the enumeration is complete"
    );
    assert_ne!(second.generation_id, first.generation_id);
    assert_eq!(
        second.blobs_fetched, 0,
        "removing a document acquires nothing"
    );
}

#[tokio::test]
async fn every_published_blob_is_content_addressed_by_the_manifest() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let remote_root = root.join("remote");
    std::fs::create_dir_all(&remote_root).unwrap();
    remote(&remote_root);
    let journal = root.join("journal.json");
    let connector = FixtureConnector::open(&remote_root).unwrap();
    let sink = ModelSink::new();
    cycle(&connector, &journal, &sink).await;

    // Every hash the journal claims is a hash the server actually holds. The
    // sink verifies each blob's digest on write, so this also proves the
    // producer never uploaded bytes under another entry's name.
    let journal_state = bbox_file_collector::Journal::load(&journal).unwrap();
    let manifest = journal_state.manifest().unwrap();
    assert_eq!(manifest.len(), 3);
    for entry in &manifest {
        assert!(
            sink.holds(&entry.content_sha256),
            "the server must hold every blob the manifest names: {}",
            entry.logical_path
        );
        assert!(
            entry.remote_url.is_some(),
            "remote provenance rides from the first wire version so evidence \
             can cite the source document"
        );
    }
    assert!(
        manifest
            .iter()
            .any(|entry| entry.logical_path.ends_with(".pdf")),
        "the provider-native document takes its EXPORTED extension, because \
         those are the bytes the corpus chunks"
    );
}
