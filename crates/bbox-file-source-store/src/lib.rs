//! Durable store for connector-sourced generations, keyed on
//! [`ConnectorScope`].
//!
//! # Why this is a separate store rather than polymorphism over the code store
//!
//! Decided by the operator, 2026-08-13. Three options were on the table and
//! two are refused:
//!
//! - **Thread `ConnectorScope` through `bbox-code-source-store`.** Refused.
//!   That store is keyed on `PublishedScope` end to end, and its generation
//!   model carries `head_commit`, `dirty_fingerprint`, and relpath-derived
//!   edge targets. Every one of those is MEANINGLESS for a connector source:
//!   there is no commit, no worktree to be dirty, and no repository-relative
//!   root. Making it polymorphic would mean optionality bleeding through an
//!   eleven-thousand-line store, where each `Option` is a place a future
//!   reader has to re-derive which source family it is holding.
//! - **Map connector scopes onto synthetic published scopes.** Refused, and
//!   this is the important refusal: it would mint a `repo_id` that names no
//!   repository purely so a connector source could borrow the code store's
//!   key. That is the path-shaped identity bridge
//!   `locality-first-decomposition.md` killed by name, rebuilt one layer
//!   down, and it would put a fabricated published scope into an activation
//!   record that the catalog would then disagree with.
//! - **A parallel store.** Taken. It matches the existing `*-source-store`
//!   family precedent (`bbox-code-source-store`,
//!   `bbox-git-source-store`, `bbox-knowledge-source-store` are already three
//!   separate stores rather than one generic one), and it keeps the code
//!   store untouched, which matters while sibling work is in flight against
//!   it.
//!
//! The duplicated-discipline cost is paid deliberately. Where a helper could
//! move to a shared home WITHOUT modifying `bbox-code-source-store`, it did:
//! digests, path validation, manifest ordering, and byte caps all live in the
//! leaf `bbox-file-source` / `bbox-code-source` crates and are shared. What
//! remains duplicated here is the store's own crash-safe write discipline, and
//! each such helper carries a one-line pointer naming its twin so a future
//! extraction pass can find both halves.
//!
//! # The two stores and their write ordering
//!
//! A connector activation touches two durable stores that share no
//! transaction, in this literal order. The shape is deliberately the SAME as
//! the collected-code activation documented in
//! `bbox-indexing::code_source_locality_manifest`, so one mental model covers
//! both lanes:
//!
//! 1. [`FileSourceStore::stage_activation`] - this store: the generation
//!    records its staged document count and entity inventory.
//! 2. [`FileSourceStore::install_activation`] - this store: the activation
//!    record for the new generation is durably installed.
//! 3. the derived edge-sidecar workspace manifest: one atomic replacement
//!    publishes the `collected:` selector and the active snapshot.
//! 4. [`FileSourceStore::mark_active`] - this store: the generation's state
//!    flips to `Active`.
//!
//! Each store is individually crash safe (fsync, atomic rename, parent fsync);
//! the PAIR is not. So every boundary above is an observable crash window, and
//! the generation's state is what tells the two interesting ones apart:
//!
//! - **Crash between 2 and 3.** This store names generation N, the manifest
//!   still names its predecessor, and N is NOT `Active`. The activation did
//!   not complete. **The manifest is the authority**: the project is still
//!   correctly serving the predecessor, which is exactly why the prior
//!   generation is retained rather than pruned at step 1. Recovery direction
//!   is backward, to the predecessor, and nothing rewrites the manifest.
//! - **Crash between 3 and 4.** Both agree on N, but N is not `Active`. That
//!   is a completed activation whose state flip was lost. **This store is the
//!   authority**: recovery direction is forward, and the flip is replayed.
//!
//! Because the state flip is written LAST, an `Active` generation is proof
//! that its manifest write was issued and returned. An `Active` generation
//! whose manifest entry is absent or names an older generation is therefore a
//! lost derived write, not an in-flight activation, and THIS store repairs it.
//!
//! [`classify_activation_tear`] is that rule as a total function, and
//! `a_tear_between_the_record_and_the_manifest_recovers_backward` plus
//! `a_tear_between_the_manifest_and_the_state_flip_recovers_forward` are its
//! fixtures. They exist BEFORE the activation wiring that will consume them,
//! on purpose: a torn-state fixture written after the first probe-kill is a
//! fixture written to match whatever the bug did.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use bbox_corpus_core::project_catalog::ConnectorScope;
use bbox_file_source::{
    CursorDegradationV1, FileGenerationDescriptorV1, FileGenerationStateV1, FileGenerationStatusV1,
    FileManifestEntryV1, PublicationTelemetryV1,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// How many superseded generations are retained behind the active one.
///
/// At least one is REQUIRED, not merely nice: the crash window between the
/// activation record and the derived manifest leaves readers correctly serving
/// the predecessor, so pruning it at stage time would turn a recoverable tear
/// into data loss.
pub const DEFAULT_RETAINED_GENERATIONS: usize = 2;

/// Blob-hash pages returned to a producer negotiating uploads.
pub const MISSING_BLOBS_PAGE: usize = 512;

// ---------------------------------------------------------------------------
// Durable records
// ---------------------------------------------------------------------------

/// One generation as this store holds it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StoredFileGeneration {
    pub generation_id: String,
    pub scope: ConnectorScope,
    pub producer_id: String,
    /// Monotonic per scope, advancing by EXACTLY one. See
    /// [`FileSourceStore::begin_upload`] for why replay does not advance it.
    pub ordinal: u64,
    pub descriptor: FileGenerationDescriptorV1,
    pub state: FileGenerationStateV1,
    #[serde(default)]
    pub telemetry: PublicationTelemetryV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degradation: Option<CursorDegradationV1>,
    /// Installed by [`FileSourceStore::stage_activation`], before the
    /// activation record exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staged_document_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staged_entity_inventory_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
}

impl StoredFileGeneration {
    pub fn status(&self) -> FileGenerationStatusV1 {
        FileGenerationStatusV1 {
            generation_id: self.generation_id.clone(),
            state: self.state,
            file_count: self.descriptor.file_count,
            logical_bytes: self.descriptor.logical_bytes,
            cursor_epoch: self.descriptor.cursor_epoch,
            diagnostic: self.diagnostic.clone(),
        }
    }
}

/// The durably installed pointer to the generation a scope serves.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FileActivationRecord {
    pub scope: ConnectorScope,
    pub generation_id: String,
    pub ordinal: u64,
    /// The `collected:` selector the index and the derived manifest carry.
    /// A connector source is an ordinary collected source downstream, so this
    /// is deliberately not a new selector family.
    pub selector: String,
    pub document_count: u64,
    pub entity_inventory_sha256: String,
    pub installed_at: String,
    /// The generation this one superseded, retained so a tear between the
    /// record and the derived manifest has somewhere to recover backward to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_generation_id: Option<String>,
}

/// In-flight upload state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UploadRecord {
    pub upload_id: String,
    pub generation_id: String,
    pub scope: ConnectorScope,
    pub producer_id: String,
    pub ordinal: u64,
    pub descriptor: FileGenerationDescriptorV1,
    #[serde(default)]
    pub telemetry: PublicationTelemetryV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degradation: Option<CursorDegradationV1>,
    /// Manifest pages received so far, keyed by page index so a redelivered
    /// page replaces rather than appends.
    #[serde(default)]
    pub pages: BTreeMap<u64, Vec<FileManifestEntryV1>>,
    #[serde(default)]
    pub manifest_complete: bool,
}

impl UploadRecord {
    /// Flatten received pages in page order.
    pub fn entries(&self) -> Vec<FileManifestEntryV1> {
        self.pages.values().flatten().cloned().collect()
    }
}

/// A deferred connector selector retirement, journaled durably so a
/// vector-store readiness deferral survives a daemon restart and can be
/// redriven to completion rather than stranding the superseded generation's
/// documents forever (gap-7e44ee3b).
///
/// TWIN: `bbox-code-source-store::RetirementRecord` is the code lane's
/// equivalent. This record is deliberately its own type rather than a reuse
/// of that one: it is keyed on [`ConnectorScope`] and carries the connector's
/// own generation identity, and the two stores are already kept separate for
/// the reasons the module doc explains. `reason` is a short operator-facing
/// diagnostic (for example the readiness error's `Display`), kept for the
/// same audit purpose `bbox-code-source-store`'s health records serve.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConnectorRetirementRecord {
    pub version: u32,
    pub project_id: String,
    pub scope: ConnectorScope,
    /// The superseded generation whose documents this retirement removes.
    pub superseded_generation_id: String,
    /// The `collected:` selector the superseded generation's documents are
    /// indexed under. Identity for the journal row: one selector can only
    /// ever have one outstanding deferred retirement.
    pub selector: String,
    /// Why the retirement was deferred, for operators reading the journal
    /// directly. Not interpreted by any code path.
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectorRetirementEnqueueConflict;

impl std::fmt::Display for ConnectorRetirementEnqueueConflict {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .write_str("connector selector already has a different retirement identity journaled")
    }
}

impl std::error::Error for ConnectorRetirementEnqueueConflict {}

// ---------------------------------------------------------------------------
// Activation tear classification
// ---------------------------------------------------------------------------

/// Which side of a two-store activation is authoritative for a given tear.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationTear {
    /// Store and derived manifest agree and the state is `Active`. Nothing to
    /// do.
    Converged,
    /// The activation record names N, the manifest names its predecessor, and
    /// N is not `Active`: the activation never completed. The MANIFEST is the
    /// authority and readers are correctly serving the predecessor. Recover
    /// backward; do not rewrite the manifest.
    RecoverBackwardToManifest,
    /// Both name N but N is not `Active`: a completed activation whose state
    /// flip was lost. THIS STORE is the authority. Recover forward by
    /// replaying the flip.
    RecoverForwardReplayStateFlip,
    /// N is `Active` but the manifest is absent or names an older generation.
    /// Because the flip is written last, `Active` proves the manifest write
    /// was issued, so this is a LOST DERIVED WRITE rather than an in-flight
    /// activation. THIS STORE is the authority; republish the manifest.
    RecoverForwardRepublishManifest,
}

/// Classify a store-versus-derived-manifest disagreement.
///
/// Total by construction: every combination of (recorded generation, manifest
/// generation, state) lands on exactly one arm, so no interleaving of the four
/// activation writes produces an unhandled shape and none of them is a crash
/// loop.
pub fn classify_activation_tear(
    recorded_generation: &str,
    recorded_state: FileGenerationStateV1,
    manifest_generation: Option<&str>,
) -> ActivationTear {
    let agrees = manifest_generation == Some(recorded_generation);
    match (agrees, recorded_state) {
        (true, FileGenerationStateV1::Active) => ActivationTear::Converged,
        (true, _) => ActivationTear::RecoverForwardReplayStateFlip,
        (false, FileGenerationStateV1::Active) => ActivationTear::RecoverForwardRepublishManifest,
        (false, _) => ActivationTear::RecoverBackwardToManifest,
    }
}

// ---------------------------------------------------------------------------
// The store
// ---------------------------------------------------------------------------

pub struct FileSourceStore {
    root: PathBuf,
    retained_generations: usize,
}

impl FileSourceStore {
    /// Open (creating if absent) the store beneath `root`.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(root.join("blobs"))
            .with_context(|| format!("creating {}", root.join("blobs").display()))?;
        std::fs::create_dir_all(root.join("scopes"))?;
        std::fs::create_dir_all(root.join("retirements"))?;
        Ok(Self {
            root,
            retained_generations: DEFAULT_RETAINED_GENERATIONS,
        })
    }

    pub fn with_retained_generations(mut self, retained: usize) -> Self {
        // One is the floor, not zero: see DEFAULT_RETAINED_GENERATIONS.
        self.retained_generations = retained.max(1);
        self
    }

    fn scope_dir(&self, scope: &ConnectorScope) -> PathBuf {
        self.root
            .join("scopes")
            .join(bbox_file_source::scope_hash(scope))
    }

    fn generation_path(&self, scope: &ConnectorScope, generation_id: &str) -> PathBuf {
        self.scope_dir(scope)
            .join("generations")
            .join(format!("{generation_id}.json"))
    }

    fn manifest_path(&self, scope: &ConnectorScope, generation_id: &str) -> PathBuf {
        self.scope_dir(scope)
            .join("manifests")
            .join(format!("{generation_id}.json"))
    }

    fn upload_path(&self, upload_id: &str) -> PathBuf {
        self.root.join("uploads").join(format!("{upload_id}.json"))
    }

    fn activation_path(&self, scope: &ConnectorScope) -> PathBuf {
        self.scope_dir(scope).join("activation.json")
    }

    /// Blob path, sharded two hex characters deep so one directory never holds
    /// the whole CAS.
    pub fn blob_path(&self, hash: &str) -> PathBuf {
        self.root.join("blobs").join(&hash[..2]).join(hash)
    }

    pub fn has_blob(&self, hash: &str) -> bool {
        self.blob_path(hash).is_file()
    }

    /// Install one blob, refusing bytes that do not hash to the claimed name.
    ///
    /// Content addressing is only a guarantee if the address is CHECKED on
    /// write; an unverified CAS is a filename convention.
    pub fn install_blob(&self, hash: &str, bytes: &[u8]) -> Result<()> {
        bbox_file_source::validate_sha256(hash)
            .map_err(|error| anyhow::anyhow!("invalid blob hash: {error}"))?;
        let actual = hex::encode(Sha256::digest(bytes));
        if actual != hash {
            bail!("blob content does not match its claimed hash");
        }
        let path = self.blob_path(hash);
        if path.is_file() {
            return Ok(());
        }
        write_durable(&path, bytes)
    }

    /// Read a blob back, re-verifying its size and hash.
    ///
    /// The indexing seam re-verifies too. That is not redundancy: the store
    /// proves the CAS is intact, the seam proves the MANIFEST entry and the
    /// bytes agree, and only the second one catches a manifest that points at
    /// the wrong (still valid) blob.
    pub fn verified_blob(&self, hash: &str, size: u64) -> Result<Vec<u8>> {
        let bytes =
            std::fs::read(self.blob_path(hash)).with_context(|| format!("reading blob {hash}"))?;
        if bytes.len() as u64 != size {
            bail!("blob {hash} has size {}, expected {size}", bytes.len());
        }
        if hex::encode(Sha256::digest(&bytes)) != hash {
            bail!("blob {hash} failed content verification");
        }
        Ok(bytes)
    }

    // -- uploads ----------------------------------------------------------

    /// Begin an upload for a descriptor, deriving the generation id
    /// server-side.
    ///
    /// **Exact-replay idempotency.** The generation id is a pure function of
    /// the producer, scope, policy version, cursor epoch, and manifest digest,
    /// so a producer that crashes and re-presents the SAME descriptor derives
    /// the same id, resumes the same upload record, and keeps the same
    /// ordinal. Only a genuinely new descriptor advances the ordinal, and it
    /// advances by exactly one. That is what makes "resumes without
    /// re-uploading blobs the server holds" true: the replayed upload finds
    /// its manifest already received and its blobs already in the CAS.
    pub fn begin_upload(
        &self,
        producer_id: &str,
        descriptor: &FileGenerationDescriptorV1,
        telemetry: &PublicationTelemetryV1,
        degradation: Option<&CursorDegradationV1>,
    ) -> Result<UploadRecord> {
        descriptor
            .validate_header()
            .map_err(|error| anyhow::anyhow!("invalid generation descriptor: {error}"))?;
        let generation_id = bbox_file_source::generation_id(producer_id, descriptor);
        let upload_id = format!("fsu-{}", &generation_id[..24]);

        if let Some(existing) = self.load_upload(&upload_id)? {
            // Exact replay. Refuse a DIFFERENT descriptor under the same
            // derived id, which can only mean a hash collision or a tampered
            // record; silently adopting either would publish one generation's
            // manifest under another's identity.
            if existing.descriptor != *descriptor || existing.producer_id != producer_id {
                bail!("upload {upload_id} exists with a different descriptor");
            }
            return Ok(existing);
        }
        // An already-finalized generation replays to its own record rather
        // than minting a second upload for bytes the store already holds.
        //
        // The record is PERSISTED, not merely returned. Every verb after
        // begin -- manifest page, complete, missing, finalize -- is keyed on
        // the upload id and resolves it through `load_upload`, and `finalize`
        // deliberately deletes the record once the generation becomes the
        // authority. A returned-but-unsaved record therefore answers "unknown
        // upload" to every one of those calls, which is exactly the case this
        // branch exists to serve: a producer whose finalize response was lost,
        // or one resuming after a crash, re-presents the descriptor and must
        // be able to drive the conversation to an idempotent finalize.
        //
        // The pages are rehydrated from the generation's durable manifest
        // rather than left empty. An empty page set would make `missing_blobs`
        // report that the producer owes NOTHING (it recomputes the owed set
        // from these entries), and would make a replayed `complete_manifest`
        // fail its own descriptor's file-count check. `finalize` writes the
        // manifest before it saves the generation, so a generation that loads
        // always has one; a generation without a manifest is a corrupt store
        // rather than a state to paper over, and failing here is louder than
        // handing back a record claiming a complete manifest with no entries.
        if let Some(stored) = self.load_generation(&descriptor.scope, &generation_id)? {
            let entries = self.manifest(&descriptor.scope, &generation_id)?;
            let record = UploadRecord {
                upload_id,
                generation_id,
                scope: descriptor.scope.clone(),
                producer_id: producer_id.to_string(),
                ordinal: stored.ordinal,
                descriptor: descriptor.clone(),
                telemetry: telemetry.clone(),
                degradation: degradation.cloned(),
                pages: BTreeMap::from([(0, entries)]),
                manifest_complete: true,
            };
            self.save_upload(&record)?;
            return Ok(record);
        }

        let ordinal = self.max_ordinal(&descriptor.scope)?.saturating_add(1);
        let record = UploadRecord {
            upload_id: upload_id.clone(),
            generation_id,
            scope: descriptor.scope.clone(),
            producer_id: producer_id.to_string(),
            ordinal,
            descriptor: descriptor.clone(),
            telemetry: telemetry.clone(),
            degradation: degradation.cloned(),
            pages: BTreeMap::new(),
            manifest_complete: false,
        };
        self.save_upload(&record)?;
        Ok(record)
    }

    pub fn load_upload(&self, upload_id: &str) -> Result<Option<UploadRecord>> {
        read_json_optional(&self.upload_path(upload_id))
    }

    fn save_upload(&self, record: &UploadRecord) -> Result<()> {
        write_durable_json(&self.upload_path(&record.upload_id), record)
    }

    /// Receive one manifest page. Redelivery replaces the page in place, so a
    /// retry after a lost response is not a duplicated entry set.
    ///
    /// After the manifest is COMPLETE an identical redelivery is accepted as a
    /// no-op and a differing page is refused. The asymmetry is the point: a
    /// completed manifest was validated against its descriptor, so letting a
    /// producer change it would silently invalidate that check, while
    /// re-sending the same bytes changes nothing. Accepting the identical case
    /// is not a convenience -- [`BeginFileUploadResponseV1`] carries no
    /// "manifest already received" signal, so a producer re-driving the
    /// conversation after a crash CANNOT know to skip its pages, which makes
    /// redelivery the ordinary shape of a resume rather than a protocol
    /// violation.
    ///
    /// [`BeginFileUploadResponseV1`]: bbox_file_source::BeginFileUploadResponseV1
    pub fn append_manifest_page(
        &self,
        upload_id: &str,
        page_index: u64,
        entries: Vec<FileManifestEntryV1>,
    ) -> Result<()> {
        let mut record = self
            .load_upload(upload_id)?
            .with_context(|| format!("unknown upload {upload_id}"))?;
        if record.manifest_complete {
            if record.pages.get(&page_index) == Some(&entries) {
                return Ok(());
            }
            bail!("upload {upload_id} already completed its manifest");
        }
        for entry in &entries {
            entry
                .validate()
                .map_err(|error| anyhow::anyhow!("invalid manifest entry: {error}"))?;
        }
        record.pages.insert(page_index, entries);
        self.save_upload(&record)
    }

    /// Close the manifest, VALIDATE it against its descriptor, and report the
    /// blobs the store still lacks.
    ///
    /// Validation happens here rather than at finalize because a manifest that
    /// disagrees with its descriptor must never cause a single blob upload:
    /// the producer would spend bandwidth on a generation the store was always
    /// going to refuse.
    pub fn complete_manifest(&self, upload_id: &str) -> Result<Vec<String>> {
        let mut record = self
            .load_upload(upload_id)?
            .with_context(|| format!("unknown upload {upload_id}"))?;
        let entries = record.entries();
        record
            .descriptor
            .validate_manifest(
                &entries,
                bbox_file_source::DEFAULT_MAX_MANIFEST_FILES,
                bbox_file_source::DEFAULT_MAX_MANIFEST_LOGICAL_BYTES,
            )
            .map_err(|error| anyhow::anyhow!("manifest rejected: {error}"))?;
        record.manifest_complete = true;
        self.save_upload(&record)?;
        Ok(self.missing_hashes(&entries))
    }

    fn missing_hashes(&self, entries: &[FileManifestEntryV1]) -> Vec<String> {
        let mut missing = BTreeSet::new();
        for entry in entries {
            if !self.has_blob(&entry.content_sha256) {
                missing.insert(entry.content_sha256.clone());
            }
        }
        missing.into_iter().collect()
    }

    /// The blobs still missing for an upload, recomputed from the CAS rather
    /// than from a remembered list: a restart mid-publication must not
    /// re-request blobs that landed before the crash.
    pub fn missing_blobs(&self, upload_id: &str) -> Result<Vec<String>> {
        let record = self
            .load_upload(upload_id)?
            .with_context(|| format!("unknown upload {upload_id}"))?;
        Ok(self.missing_hashes(&record.entries()))
    }

    /// Promote a complete upload into a durable `Ready` generation.
    pub fn finalize(&self, upload_id: &str) -> Result<StoredFileGeneration> {
        let record = self
            .load_upload(upload_id)?
            .with_context(|| format!("unknown upload {upload_id}"))?;
        if !record.manifest_complete {
            bail!("upload {upload_id} has not completed its manifest");
        }
        if let Some(existing) = self.load_generation(&record.scope, &record.generation_id)? {
            // Idempotent finalize: a redelivered finalize returns the record
            // that already exists rather than rewriting a generation that may
            // already be Active.
            //
            // The replayed working state is retired here for the same reason
            // the fresh path retires it below: once the generation exists it
            // is the authority, so leaving the record behind would accumulate
            // one dead upload per redelivery.
            let _ = std::fs::remove_file(self.upload_path(upload_id));
            return Ok(existing);
        }
        let entries = record.entries();
        let missing = self.missing_hashes(&entries);
        if !missing.is_empty() {
            bail!(
                "upload {upload_id} is missing {} blob(s); first: {}",
                missing.len(),
                missing[0]
            );
        }
        write_durable_json(
            &self.manifest_path(&record.scope, &record.generation_id),
            &entries,
        )?;
        let generation = StoredFileGeneration {
            generation_id: record.generation_id.clone(),
            scope: record.scope.clone(),
            producer_id: record.producer_id.clone(),
            ordinal: record.ordinal,
            descriptor: record.descriptor.clone(),
            state: FileGenerationStateV1::Ready,
            telemetry: record.telemetry.clone(),
            degradation: record.degradation.clone(),
            staged_document_count: None,
            staged_entity_inventory_sha256: None,
            diagnostic: None,
        };
        self.save_generation(&generation)?;
        // The upload record is working state; the generation is authority.
        let _ = std::fs::remove_file(self.upload_path(upload_id));
        Ok(generation)
    }

    // -- generations ------------------------------------------------------

    pub fn load_generation(
        &self,
        scope: &ConnectorScope,
        generation_id: &str,
    ) -> Result<Option<StoredFileGeneration>> {
        read_json_optional(&self.generation_path(scope, generation_id))
    }

    fn save_generation(&self, generation: &StoredFileGeneration) -> Result<()> {
        write_durable_json(
            &self.generation_path(&generation.scope, &generation.generation_id),
            generation,
        )
    }

    pub fn manifest(
        &self,
        scope: &ConnectorScope,
        generation_id: &str,
    ) -> Result<Vec<FileManifestEntryV1>> {
        read_json_optional(&self.manifest_path(scope, generation_id))?
            .with_context(|| format!("no manifest for generation {generation_id}"))
    }

    pub fn generations(&self, scope: &ConnectorScope) -> Result<Vec<StoredFileGeneration>> {
        let dir = self.scope_dir(scope).join("generations");
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Ok(out);
        };
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Some(generation) = read_json_optional::<StoredFileGeneration>(&path)? {
                out.push(generation);
            }
        }
        out.sort_by_key(|generation| generation.ordinal);
        Ok(out)
    }

    fn max_ordinal(&self, scope: &ConnectorScope) -> Result<u64> {
        Ok(self
            .generations(scope)?
            .last()
            .map(|generation| generation.ordinal)
            .unwrap_or(0))
    }

    pub fn status(
        &self,
        scope: &ConnectorScope,
        generation_id: &str,
    ) -> Result<Option<FileGenerationStatusV1>> {
        Ok(self
            .load_generation(scope, generation_id)?
            .map(|generation| generation.status()))
    }

    pub fn set_state(
        &self,
        scope: &ConnectorScope,
        generation_id: &str,
        state: FileGenerationStateV1,
        diagnostic: Option<String>,
    ) -> Result<()> {
        let mut generation = self
            .load_generation(scope, generation_id)?
            .with_context(|| format!("unknown generation {generation_id}"))?;
        generation.state = state;
        if diagnostic.is_some() {
            generation.diagnostic = diagnostic;
        }
        self.save_generation(&generation)
    }

    // -- activation: stage, install, flip, retain -------------------------

    /// Step 1: record what staging produced, BEFORE any activation record
    /// exists. State becomes `StagingIndex`.
    pub fn stage_activation(
        &self,
        scope: &ConnectorScope,
        generation_id: &str,
        document_count: u64,
        entity_inventory_sha256: &str,
    ) -> Result<()> {
        let mut generation = self
            .load_generation(scope, generation_id)?
            .with_context(|| format!("unknown generation {generation_id}"))?;
        generation.state = FileGenerationStateV1::StagingIndex;
        generation.staged_document_count = Some(document_count);
        generation.staged_entity_inventory_sha256 = Some(entity_inventory_sha256.to_string());
        self.save_generation(&generation)
    }

    /// Step 2: durably install the activation record, atomically replacing the
    /// prior one and naming the generation it supersedes.
    ///
    /// Naming the predecessor is what lets the backward recovery direction be
    /// taken without consulting the derived manifest: the record itself says
    /// where readers still are.
    ///
    /// `project_id` is passed in rather than derived: the CATALOG owns the
    /// mapping from a connector scope to a project, and a store that guessed
    /// it would be a second, quieter authority on project identity.
    pub fn install_activation(
        &self,
        scope: &ConnectorScope,
        generation_id: &str,
        project_id: &str,
        installed_at: &str,
    ) -> Result<FileActivationRecord> {
        let generation = self
            .load_generation(scope, generation_id)?
            .with_context(|| format!("unknown generation {generation_id}"))?;
        let document_count = generation
            .staged_document_count
            .context("activation record requires a staged document count")?;
        let inventory = generation
            .staged_entity_inventory_sha256
            .clone()
            .context("activation record requires a staged entity inventory")?;
        let previous = self.active_generation(scope)?;
        let record = FileActivationRecord {
            scope: scope.clone(),
            generation_id: generation_id.to_string(),
            ordinal: generation.ordinal,
            selector: bbox_file_source::source_selector(project_id, generation_id),
            document_count,
            entity_inventory_sha256: inventory,
            installed_at: installed_at.to_string(),
            superseded_generation_id: previous.as_ref().map(|r| r.generation_id.clone()),
        };
        write_durable_json(&self.activation_path(scope), &record)?;
        Ok(record)
    }

    /// Step 4: the flip, written LAST so `Active` is proof the derived
    /// manifest write was issued and returned.
    pub fn mark_active(&self, scope: &ConnectorScope, generation_id: &str) -> Result<()> {
        let record = self
            .active_generation(scope)?
            .context("no activation record to flip")?;
        if record.generation_id != generation_id {
            bail!(
                "refusing to flip {generation_id} active: the installed activation record names {}",
                record.generation_id
            );
        }
        for generation in self.generations(scope)? {
            if generation.generation_id == generation_id {
                self.set_state(scope, generation_id, FileGenerationStateV1::Active, None)?;
            } else if generation.state == FileGenerationStateV1::Active {
                self.set_state(
                    scope,
                    &generation.generation_id,
                    FileGenerationStateV1::Superseded,
                    None,
                )?;
            }
        }
        self.retain_generations(scope)
    }

    pub fn active_generation(
        &self,
        scope: &ConnectorScope,
    ) -> Result<Option<FileActivationRecord>> {
        read_json_optional(&self.activation_path(scope))
    }

    /// Prune superseded generations beyond the retention window.
    ///
    /// Never prunes the active generation, and never prunes the generation the
    /// active record supersedes: that one is the backward recovery target.
    /// Blobs are left alone entirely, because they are content addressed and
    /// shared across generations; blob GC is a separate sweep with its own
    /// grace window.
    pub fn retain_generations(&self, scope: &ConnectorScope) -> Result<()> {
        let active = self.active_generation(scope)?;
        let protected: BTreeSet<String> = active
            .iter()
            .flat_map(|record| {
                std::iter::once(record.generation_id.clone())
                    .chain(record.superseded_generation_id.clone())
            })
            .collect();
        let mut generations = self.generations(scope)?;
        generations.reverse(); // newest first
        let mut kept = 0usize;
        for generation in generations {
            if protected.contains(&generation.generation_id) {
                continue;
            }
            kept += 1;
            if kept <= self.retained_generations {
                continue;
            }
            let _ = std::fs::remove_file(self.generation_path(scope, &generation.generation_id));
            let _ = std::fs::remove_file(self.manifest_path(scope, &generation.generation_id));
        }
        Ok(())
    }

    /// Classify this scope's activation against the derived manifest's view.
    pub fn classify_tear(
        &self,
        scope: &ConnectorScope,
        manifest_generation: Option<&str>,
    ) -> Result<Option<ActivationTear>> {
        let Some(record) = self.active_generation(scope)? else {
            return Ok(None);
        };
        let Some(generation) = self.load_generation(scope, &record.generation_id)? else {
            return Ok(None);
        };
        Ok(Some(classify_activation_tear(
            &record.generation_id,
            generation.state,
            manifest_generation,
        )))
    }

    // -- retirement journal -------------------------------------------------
    //
    // Durable redrive state for a selector retirement deferred by writer
    // readiness (typically the vector store still warming up just after
    // boot). Rows are journaled at defer time, before any retry, so a
    // restart mid-deferral does not lose the stranded selector. See
    // `ConnectorRetirementRecord` for the shape and gap-7e44ee3b for the
    // defect this closes.

    fn retirement_path(&self, selector: &str) -> PathBuf {
        let hash = hex::encode(Sha256::digest(selector.as_bytes()));
        self.root.join("retirements").join(format!("{hash}.json"))
    }

    /// Journal one deferred retirement. Idempotent for the identical record
    /// (a redundant defer of the same selector is a no-op); refuses a
    /// different record under the same selector, which would mean two
    /// generations minted the same selector and is a bug rather than a state
    /// to paper over.
    pub fn enqueue_retirement(&self, record: &ConnectorRetirementRecord) -> Result<()> {
        let path = self.retirement_path(&record.selector);
        if let Some(existing) = read_json_optional::<ConnectorRetirementRecord>(&path)? {
            if existing == *record {
                return Ok(());
            }
            return Err(anyhow::Error::new(ConnectorRetirementEnqueueConflict));
        }
        write_durable_json(&path, record)
    }

    /// Every retirement still journaled, for the redrive's boot recovery
    /// sweep. Order is deterministic (by selector) but otherwise
    /// unimportant: the redrive coordinator schedules each independently.
    pub fn retirement_records(&self) -> Result<Vec<ConnectorRetirementRecord>> {
        let dir = self.root.join("retirements");
        if !dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut records = Vec::new();
        for entry in
            std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))?
        {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                if let Some(record) = read_json_optional(&entry.path())? {
                    records.push(record);
                }
            }
        }
        records
            .sort_by(|left: &ConnectorRetirementRecord, right| left.selector.cmp(&right.selector));
        Ok(records)
    }

    pub fn retirement_pending(&self, selector: &str) -> Result<bool> {
        Ok(
            read_json_optional::<ConnectorRetirementRecord>(&self.retirement_path(selector))?
                .is_some(),
        )
    }

    /// Mark a journaled retirement complete, removing its row.
    ///
    /// Idempotent: a selector with no journaled row (already completed, or
    /// never deferred in the first place) is a no-op success rather than an
    /// error, so redriving an already-retired selector is safe to repeat.
    /// Refuses if the row present disagrees with `record`, which would mean
    /// the journal was rewritten out from under an in-flight redrive.
    pub fn complete_retirement(&self, record: &ConnectorRetirementRecord) -> Result<()> {
        let path = self.retirement_path(&record.selector);
        let Some(queued) = read_json_optional::<ConnectorRetirementRecord>(&path)? else {
            return Ok(());
        };
        if queued != *record {
            bail!("connector retirement journal row changed before completion");
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {
                if let Some(parent) = path.parent() {
                    if let Ok(dir) = std::fs::File::open(parent) {
                        let _ = dir.sync_all();
                    }
                }
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context(format!("removing {}", path.display())),
        }
    }
}

// ---------------------------------------------------------------------------
// Durable write helpers
// ---------------------------------------------------------------------------

/// Crash-safe replacement: staged sibling, fsync, rename, parent fsync.
///
/// TWIN: `bbox-code-source-store` carries the same discipline for the code
/// lane. Deliberately duplicated rather than extracted, because extracting it
/// would mean modifying that store, which this pass does not touch.
fn write_durable(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("durable write needs a parent")?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    {
        use std::io::Write as _;
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

    const SOURCE_ID: &str = "csrc_5f2c1d9a4b6e470e";
    const PRODUCER: &str = "producer-a";

    fn scope() -> ConnectorScope {
        ConnectorScope::try_new(SOURCE_ID, "fixture").unwrap()
    }

    fn store(dir: &tempfile::TempDir) -> FileSourceStore {
        let root = dir.path().canonicalize().unwrap();
        FileSourceStore::open(root.join("file-source")).unwrap()
    }

    fn entry(path: &str, bytes: &[u8], remote_id: &str) -> FileManifestEntryV1 {
        FileManifestEntryV1 {
            logical_path: path.into(),
            content_sha256: hex::encode(Sha256::digest(bytes)),
            size: bytes.len() as u64,
            remote_id: remote_id.into(),
            remote_version: "v1".into(),
            remote_url: Some(format!("https://fixture.invalid/d/{remote_id}")),
        }
    }

    fn descriptor(
        entries: &[FileManifestEntryV1],
        cursor_epoch: u64,
    ) -> FileGenerationDescriptorV1 {
        FileGenerationDescriptorV1 {
            schema_version: bbox_file_source::SCHEMA_VERSION,
            connector_policy_version: bbox_file_source::CONNECTOR_POLICY_VERSION.into(),
            scope: scope(),
            remote_watermark: "watermark-1".into(),
            cursor_epoch,
            manifest_sha256: bbox_file_source::manifest_sha256(entries),
            file_count: entries.len() as u64,
            logical_bytes: entries.iter().map(|entry| entry.size).sum(),
        }
    }

    /// Drive one full publication to `Ready`.
    fn publish(
        store: &FileSourceStore,
        payloads: &[(&str, &[u8], &str)],
        cursor_epoch: u64,
    ) -> StoredFileGeneration {
        let entries: Vec<_> = payloads
            .iter()
            .map(|(path, bytes, id)| entry(path, bytes, id))
            .collect();
        let descriptor = descriptor(&entries, cursor_epoch);
        let upload = store
            .begin_upload(
                PRODUCER,
                &descriptor,
                &PublicationTelemetryV1::default(),
                None,
            )
            .unwrap();
        store
            .append_manifest_page(&upload.upload_id, 0, entries.clone())
            .unwrap();
        let missing = store.complete_manifest(&upload.upload_id).unwrap();
        for hash in &missing {
            let (_, bytes, _) = payloads
                .iter()
                .find(|(_, bytes, _)| &hex::encode(Sha256::digest(bytes)) == hash)
                .unwrap();
            store.install_blob(hash, bytes).unwrap();
        }
        store.finalize(&upload.upload_id).unwrap()
    }

    fn activate(store: &FileSourceStore, generation_id: &str, at: &str) {
        store
            .stage_activation(&scope(), generation_id, 3, &"a".repeat(64))
            .unwrap();
        store
            .install_activation(&scope(), generation_id, "p_project", at)
            .unwrap();
        store.mark_active(&scope(), generation_id).unwrap();
    }

    #[test]
    fn a_publication_stages_validates_flips_and_serves_exactly_one_generation() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let generation = publish(&store, &[("a.md", b"alpha", "r1")], 0);
        assert_eq!(generation.state, FileGenerationStateV1::Ready);
        assert_eq!(generation.ordinal, 1);

        activate(&store, &generation.generation_id, "2026-08-13T00:00:00Z");
        let record = store.active_generation(&scope()).unwrap().unwrap();
        assert_eq!(record.generation_id, generation.generation_id);
        assert!(
            record.selector.starts_with("collected:"),
            "a connector source is an ordinary collected source downstream"
        );
        assert_eq!(
            store
                .load_generation(&scope(), &generation.generation_id)
                .unwrap()
                .unwrap()
                .state,
            FileGenerationStateV1::Active
        );
    }

    #[test]
    fn the_ordinal_advances_by_exactly_one_and_replay_does_not_advance_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let first = publish(&store, &[("a.md", b"alpha", "r1")], 0);
        let second = publish(&store, &[("a.md", b"beta", "r1")], 0);
        assert_eq!(first.ordinal, 1);
        assert_eq!(
            second.ordinal, 2,
            "a new descriptor advances by exactly one"
        );

        // Exact replay of the SECOND descriptor: same id, same ordinal, no
        // third generation.
        let replayed = publish(&store, &[("a.md", b"beta", "r1")], 0);
        assert_eq!(replayed.generation_id, second.generation_id);
        assert_eq!(replayed.ordinal, second.ordinal);
        assert_eq!(store.generations(&scope()).unwrap().len(), 2);
    }

    #[test]
    fn a_restart_mid_publication_reuses_the_blobs_the_store_already_holds() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let entries = vec![entry("a.md", b"alpha", "r1"), entry("b.md", b"bravo", "r2")];
        let descriptor = descriptor(&entries, 0);
        let upload = store
            .begin_upload(
                PRODUCER,
                &descriptor,
                &PublicationTelemetryV1::default(),
                None,
            )
            .unwrap();
        store
            .append_manifest_page(&upload.upload_id, 0, entries.clone())
            .unwrap();
        let missing = store.complete_manifest(&upload.upload_id).unwrap();
        assert_eq!(missing.len(), 2);
        // One blob lands, then the producer "crashes".
        store
            .install_blob(&entries[0].content_sha256, b"alpha")
            .unwrap();

        // Replay: same descriptor, same upload, and only the blob that never
        // landed is still owed.
        let resumed = store
            .begin_upload(
                PRODUCER,
                &descriptor,
                &PublicationTelemetryV1::default(),
                None,
            )
            .unwrap();
        assert_eq!(resumed.upload_id, upload.upload_id);
        assert!(
            resumed.manifest_complete,
            "the received manifest survived the restart"
        );
        let still_missing = store.missing_blobs(&upload.upload_id).unwrap();
        assert_eq!(still_missing, vec![entries[1].content_sha256.clone()]);
    }

    #[test]
    fn a_manifest_disagreeing_with_its_descriptor_is_refused_before_any_upload() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let entries = vec![entry("a.md", b"alpha", "r1")];
        let mut descriptor = descriptor(&entries, 0);
        descriptor.file_count = 2;
        let upload = store
            .begin_upload(
                PRODUCER,
                &descriptor,
                &PublicationTelemetryV1::default(),
                None,
            )
            .unwrap();
        store
            .append_manifest_page(&upload.upload_id, 0, entries)
            .unwrap();
        let error = store.complete_manifest(&upload.upload_id).unwrap_err();
        assert!(error.to_string().contains("manifest rejected"));
    }

    #[test]
    fn a_blob_whose_bytes_do_not_match_its_name_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let hash = hex::encode(Sha256::digest(b"alpha"));
        assert!(store.install_blob(&hash, b"not alpha").is_err());
        store.install_blob(&hash, b"alpha").unwrap();
        assert_eq!(store.verified_blob(&hash, 5).unwrap(), b"alpha");
        assert!(
            store.verified_blob(&hash, 6).is_err(),
            "a size disagreement is a refusal, not a truncation"
        );
    }

    #[test]
    fn finalize_refuses_while_a_blob_is_still_missing_and_is_idempotent_after() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let entries = vec![entry("a.md", b"alpha", "r1")];
        let descriptor = descriptor(&entries, 0);
        let upload = store
            .begin_upload(
                PRODUCER,
                &descriptor,
                &PublicationTelemetryV1::default(),
                None,
            )
            .unwrap();
        store
            .append_manifest_page(&upload.upload_id, 0, entries.clone())
            .unwrap();
        store.complete_manifest(&upload.upload_id).unwrap();
        assert!(store.finalize(&upload.upload_id).is_err());

        store
            .install_blob(&entries[0].content_sha256, b"alpha")
            .unwrap();
        let first = store.finalize(&upload.upload_id).unwrap();
        // A redelivered finalize must not rewrite a generation that may
        // already be Active.
        let replayed = store
            .begin_upload(
                PRODUCER,
                &descriptor,
                &PublicationTelemetryV1::default(),
                None,
            )
            .unwrap();
        assert_eq!(
            store.finalize(&replayed.upload_id).unwrap().generation_id,
            first.generation_id
        );
    }

    #[test]
    fn a_tear_between_the_record_and_the_manifest_recovers_backward() {
        // Crash between step 2 and step 3: the record names N, the derived
        // manifest still names the predecessor, and N is not Active. Readers
        // are correctly serving the predecessor, so the MANIFEST wins.
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let first = publish(&store, &[("a.md", b"alpha", "r1")], 0);
        activate(&store, &first.generation_id, "2026-08-13T00:00:00Z");
        let second = publish(&store, &[("a.md", b"beta", "r1")], 0);
        store
            .stage_activation(&scope(), &second.generation_id, 3, &"b".repeat(64))
            .unwrap();
        store
            .install_activation(
                &scope(),
                &second.generation_id,
                "p_project",
                "2026-08-13T00:01:00Z",
            )
            .unwrap();
        // ...crash before the manifest replacement and before the flip.

        assert_eq!(
            store
                .classify_tear(&scope(), Some(&first.generation_id))
                .unwrap(),
            Some(ActivationTear::RecoverBackwardToManifest)
        );
        let record = store.active_generation(&scope()).unwrap().unwrap();
        assert_eq!(
            record.superseded_generation_id.as_deref(),
            Some(first.generation_id.as_str()),
            "the record names the predecessor, so backward recovery needs no \
             second source of truth"
        );
        assert!(
            store
                .load_generation(&scope(), &first.generation_id)
                .unwrap()
                .is_some(),
            "the predecessor must still exist; pruning it would turn a \
             recoverable tear into data loss"
        );
    }

    #[test]
    fn a_tear_between_the_manifest_and_the_state_flip_recovers_forward() {
        // Crash between step 3 and step 4: both sides name N, but N is not
        // Active. The activation completed; only the flip was lost.
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let generation = publish(&store, &[("a.md", b"alpha", "r1")], 0);
        store
            .stage_activation(&scope(), &generation.generation_id, 3, &"a".repeat(64))
            .unwrap();
        store
            .install_activation(
                &scope(),
                &generation.generation_id,
                "p_project",
                "2026-08-13T00:00:00Z",
            )
            .unwrap();

        assert_eq!(
            store
                .classify_tear(&scope(), Some(&generation.generation_id))
                .unwrap(),
            Some(ActivationTear::RecoverForwardReplayStateFlip)
        );
        // Replaying the flip converges.
        store
            .mark_active(&scope(), &generation.generation_id)
            .unwrap();
        assert_eq!(
            store
                .classify_tear(&scope(), Some(&generation.generation_id))
                .unwrap(),
            Some(ActivationTear::Converged)
        );
    }

    #[test]
    fn an_active_generation_with_a_lost_manifest_write_recovers_forward() {
        // Because the flip is written LAST, Active proves the manifest write
        // was issued. An absent or stale manifest entry is therefore a lost
        // derived write, not an in-flight activation.
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let generation = publish(&store, &[("a.md", b"alpha", "r1")], 0);
        activate(&store, &generation.generation_id, "2026-08-13T00:00:00Z");
        assert_eq!(
            store.classify_tear(&scope(), None).unwrap(),
            Some(ActivationTear::RecoverForwardRepublishManifest)
        );
        assert_eq!(
            store
                .classify_tear(&scope(), Some("older-generation"))
                .unwrap(),
            Some(ActivationTear::RecoverForwardRepublishManifest)
        );
    }

    #[test]
    fn tear_classification_is_total() {
        // Every combination of agreement and state lands on exactly one arm,
        // so no interleaving of the four activation writes is unhandled.
        for state in [
            FileGenerationStateV1::ReceivingManifest,
            FileGenerationStateV1::MissingBlobs,
            FileGenerationStateV1::Ready,
            FileGenerationStateV1::StagingIndex,
            FileGenerationStateV1::Active,
            FileGenerationStateV1::Superseded,
            FileGenerationStateV1::MissingBlobData,
            FileGenerationStateV1::Failed,
        ] {
            for manifest in [Some("gen-n"), Some("gen-old"), None] {
                let _ = classify_activation_tear("gen-n", state, manifest);
            }
        }
        assert_eq!(
            classify_activation_tear("gen-n", FileGenerationStateV1::Active, Some("gen-n")),
            ActivationTear::Converged
        );
    }

    #[test]
    fn the_flip_refuses_a_generation_the_installed_record_does_not_name() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let first = publish(&store, &[("a.md", b"alpha", "r1")], 0);
        let second = publish(&store, &[("a.md", b"beta", "r1")], 0);
        store
            .stage_activation(&scope(), &first.generation_id, 3, &"a".repeat(64))
            .unwrap();
        store
            .install_activation(
                &scope(),
                &first.generation_id,
                "p_project",
                "2026-08-13T00:00:00Z",
            )
            .unwrap();
        let error = store
            .mark_active(&scope(), &second.generation_id)
            .unwrap_err();
        assert!(error.to_string().contains("activation record names"));
    }

    #[test]
    fn retention_keeps_the_prior_generation_and_prunes_beyond_the_window() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = FileSourceStore::open(root.join("file-source"))
            .unwrap()
            .with_retained_generations(1);
        let mut ids = Vec::new();
        for (index, payload) in [b"one".as_slice(), b"two", b"three", b"four"]
            .iter()
            .enumerate()
        {
            let generation = publish(&store, &[("a.md", *payload, "r1")], 0);
            activate(
                &store,
                &generation.generation_id,
                &format!("2026-08-13T00:0{index}:00Z"),
            );
            ids.push(generation.generation_id);
        }
        let active = store.active_generation(&scope()).unwrap().unwrap();
        assert_eq!(active.generation_id, *ids.last().unwrap());
        assert!(
            store
                .load_generation(&scope(), &ids[ids.len() - 2])
                .unwrap()
                .is_some(),
            "the superseded predecessor is protected: it is the backward \
             recovery target"
        );
        assert!(
            store.load_generation(&scope(), &ids[0]).unwrap().is_none(),
            "generations beyond the retention window are pruned"
        );
    }

    #[test]
    fn a_completed_manifest_accepts_an_identical_page_and_refuses_a_changed_one() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let entries = vec![entry("a.md", b"alpha", "r1")];
        let descriptor = descriptor(&entries, 0);
        let upload = store
            .begin_upload(
                PRODUCER,
                &descriptor,
                &PublicationTelemetryV1::default(),
                None,
            )
            .unwrap();
        store
            .append_manifest_page(&upload.upload_id, 0, entries.clone())
            .unwrap();
        store.complete_manifest(&upload.upload_id).unwrap();

        // A resuming producer cannot know the manifest already landed, so an
        // identical redelivery is a no-op rather than a refusal.
        store
            .append_manifest_page(&upload.upload_id, 0, entries.clone())
            .expect("an identical page after completion is idempotent");
        assert_eq!(
            store
                .load_upload(&upload.upload_id)
                .unwrap()
                .unwrap()
                .entries(),
            entries,
            "and it does not duplicate the entry set"
        );

        // A DIFFERENT page would invalidate the descriptor check that
        // completion already performed, so it stays refused.
        let error = store
            .append_manifest_page(&upload.upload_id, 0, vec![entry("b.md", b"bravo", "r2")])
            .unwrap_err();
        assert!(error.to_string().contains("already completed its manifest"));
    }

    #[test]
    fn a_replay_after_finalize_is_durable_and_drivable_to_an_idempotent_finalize() {
        // The regression this pins: the already-finalized replay branch used
        // to RETURN an upload record without saving it. Every verb after
        // begin resolves the id through `load_upload`, and finalize deletes
        // the record once the generation is authority, so a memory-only
        // record answered "unknown upload" to everything that followed. That
        // breaks the redelivery contract directly: a producer whose finalize
        // response was lost re-presents the same descriptor and must be able
        // to reach a successful, idempotent finalize.
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let entries = vec![entry("a.md", b"alpha", "r1")];
        let descriptor = descriptor(&entries, 0);
        let first = publish(&store, &[("a.md", b"alpha", "r1")], 0);

        // Finalize retired the working state; the generation is authority.
        let upload_id = format!("fsu-{}", &first.generation_id[..24]);
        assert!(
            store.load_upload(&upload_id).unwrap().is_none(),
            "finalize retires the upload record"
        );

        let replayed = store
            .begin_upload(
                PRODUCER,
                &descriptor,
                &PublicationTelemetryV1::default(),
                None,
            )
            .unwrap();
        assert_eq!(
            replayed.upload_id, upload_id,
            "an exact replay derives the same id"
        );
        assert_eq!(replayed.ordinal, first.ordinal, "and keeps the ordinal");
        assert!(
            store.load_upload(&replayed.upload_id).unwrap().is_some(),
            "the replayed record must be DURABLE: every later verb resolves \
             the upload id through the store, not through the returned value"
        );

        // Rehydrated from the durable manifest, so the owed set is computed
        // honestly. An empty page set would report that a producer owes
        // nothing regardless of what the CAS actually holds.
        let reloaded = store.load_upload(&replayed.upload_id).unwrap().unwrap();
        assert_eq!(
            reloaded.entries(),
            entries,
            "the replayed record carries the generation's real manifest"
        );
        assert!(
            store.missing_blobs(&replayed.upload_id).unwrap().is_empty(),
            "the store already holds these blobs"
        );

        // The whole point: the conversation reaches an idempotent finalize.
        assert_eq!(
            store.finalize(&replayed.upload_id).unwrap().generation_id,
            first.generation_id
        );
        assert_eq!(
            store.generations(&scope()).unwrap().len(),
            1,
            "a redelivery is not a second generation"
        );
        assert!(
            store.load_upload(&replayed.upload_id).unwrap().is_none(),
            "and the replayed working state is retired again rather than \
             accumulating one dead upload per redelivery"
        );
    }

    #[test]
    fn a_replayed_upload_id_with_a_different_descriptor_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let entries = vec![entry("a.md", b"alpha", "r1")];
        let descriptor = descriptor(&entries, 0);
        let upload = store
            .begin_upload(
                PRODUCER,
                &descriptor,
                &PublicationTelemetryV1::default(),
                None,
            )
            .unwrap();
        let mut tampered = descriptor.clone();
        tampered.remote_watermark = "watermark-2".into();
        // The watermark is excluded from the generation id, so this collides
        // on the derived id while carrying a different descriptor: exactly the
        // shape that must never be silently adopted.
        let error = store
            .begin_upload(
                PRODUCER,
                &tampered,
                &PublicationTelemetryV1::default(),
                None,
            )
            .unwrap_err();
        assert!(error.to_string().contains("different descriptor"));
        assert!(store.load_upload(&upload.upload_id).unwrap().is_some());
    }

    #[test]
    fn a_redelivered_manifest_page_replaces_rather_than_appends() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let entries = vec![entry("a.md", b"alpha", "r1")];
        let descriptor = descriptor(&entries, 0);
        let upload = store
            .begin_upload(
                PRODUCER,
                &descriptor,
                &PublicationTelemetryV1::default(),
                None,
            )
            .unwrap();
        store
            .append_manifest_page(&upload.upload_id, 0, entries.clone())
            .unwrap();
        store
            .append_manifest_page(&upload.upload_id, 0, entries.clone())
            .unwrap();
        let reloaded = store.load_upload(&upload.upload_id).unwrap().unwrap();
        assert_eq!(reloaded.entries().len(), 1, "a retry is not a duplicate");
    }

    #[test]
    fn a_degradation_and_its_telemetry_survive_onto_the_generation() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let entries = vec![entry("a.md", b"alpha", "r1")];
        let descriptor = descriptor(&entries, 7);
        let mut telemetry = PublicationTelemetryV1::default();
        telemetry.skipped.insert("oversize".into(), 3);
        telemetry.entries_enumerated = 12;
        let degradation = CursorDegradationV1 {
            checkpoint_name: "root".into(),
            cause: "checkpoint_expired".into(),
            cursor_epoch: 7,
            entries_enumerated: 12,
            blobs_refetched: 4,
            documents_reexported: 1,
            observed_at: "2026-08-13T00:00:00Z".into(),
        };
        let upload = store
            .begin_upload(PRODUCER, &descriptor, &telemetry, Some(&degradation))
            .unwrap();
        store
            .append_manifest_page(&upload.upload_id, 0, entries.clone())
            .unwrap();
        store.complete_manifest(&upload.upload_id).unwrap();
        store
            .install_blob(&entries[0].content_sha256, b"alpha")
            .unwrap();
        let generation = store.finalize(&upload.upload_id).unwrap();

        // An operator watching repeated epoch increments must be able to see
        // the cause and the cost on the generation itself.
        assert_eq!(
            generation.degradation.as_ref().unwrap().cause,
            "checkpoint_expired"
        );
        assert_eq!(generation.telemetry.skipped.get("oversize"), Some(&3));
        assert_eq!(generation.status().cursor_epoch, 7);
    }

    fn retirement_record(selector: &str, generation_id: &str) -> ConnectorRetirementRecord {
        ConnectorRetirementRecord {
            version: 1,
            project_id: "p_project".into(),
            scope: scope(),
            superseded_generation_id: generation_id.into(),
            selector: selector.into(),
            reason: "the vector store is still warming up".into(),
        }
    }

    #[test]
    fn a_deferred_retirement_is_journaled_and_survives_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let record = retirement_record("collected:p_project:gen-1", "gen-1");
        {
            let store = store(&dir);
            assert!(!store.retirement_pending(&record.selector).unwrap());
            store.enqueue_retirement(&record).unwrap();
            assert!(store.retirement_pending(&record.selector).unwrap());
        }
        // A fresh open of the same root is the durable-restart proof: the
        // journal row is bytes on disk, not in-memory state the process
        // owns.
        let reopened = store(&dir);
        let records = reopened.retirement_records().unwrap();
        assert_eq!(records, vec![record]);
    }

    #[test]
    fn enqueueing_the_identical_record_twice_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let record = retirement_record("collected:p_project:gen-1", "gen-1");
        store.enqueue_retirement(&record).unwrap();
        store.enqueue_retirement(&record).unwrap();
        assert_eq!(store.retirement_records().unwrap(), vec![record]);
    }

    #[test]
    fn enqueueing_a_conflicting_record_under_the_same_selector_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let record = retirement_record("collected:p_project:gen-1", "gen-1");
        store.enqueue_retirement(&record).unwrap();
        let mut conflicting = record.clone();
        conflicting.superseded_generation_id = "gen-2".into();
        let error = store.enqueue_retirement(&conflicting).unwrap_err();
        assert!(
            error
                .downcast_ref::<ConnectorRetirementEnqueueConflict>()
                .is_some(),
            "got: {error:#}"
        );
    }

    #[test]
    fn completing_a_journaled_retirement_removes_its_row() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let record = retirement_record("collected:p_project:gen-1", "gen-1");
        store.enqueue_retirement(&record).unwrap();
        store.complete_retirement(&record).unwrap();
        assert!(!store.retirement_pending(&record.selector).unwrap());
        assert!(store.retirement_records().unwrap().is_empty());
    }

    #[test]
    fn redriving_an_already_retired_selector_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let record = retirement_record("collected:p_project:gen-1", "gen-1");
        store.enqueue_retirement(&record).unwrap();
        store.complete_retirement(&record).unwrap();
        // A redrive that observes the completed row (for example a stale
        // in-memory entry raced by a concurrent completion) must not error.
        store.complete_retirement(&record).unwrap();
        store.complete_retirement(&record).unwrap();
        assert!(store.retirement_records().unwrap().is_empty());
    }

    #[test]
    fn stacked_deferrals_for_distinct_selectors_are_all_journaled() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let first = retirement_record("collected:p_project:gen-1", "gen-1");
        let second = retirement_record("collected:p_project:gen-2", "gen-2");
        let third = retirement_record("collected:p_project:gen-3", "gen-3");
        store.enqueue_retirement(&first).unwrap();
        store.enqueue_retirement(&second).unwrap();
        store.enqueue_retirement(&third).unwrap();
        let mut records = store.retirement_records().unwrap();
        records.sort_by(|a, b| a.selector.cmp(&b.selector));
        let mut expected = vec![first.clone(), second.clone(), third.clone()];
        expected.sort_by(|a, b| a.selector.cmp(&b.selector));
        assert_eq!(records, expected);

        // Completing one leaves the other two stranded corpora still queued,
        // not silently dropped.
        store.complete_retirement(&second).unwrap();
        let remaining = store.retirement_records().unwrap();
        assert_eq!(remaining.len(), 2);
        assert!(remaining.contains(&first));
        assert!(remaining.contains(&third));
        assert!(!remaining.contains(&second));
    }
}
