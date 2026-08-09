//! The public runtime facade over accepted publication.
//!
//! Phase 5 plan sections 4.2, 4.3, 4.8, 6.1 through 6.4, and 7.8. The daemon
//! is a separate crate, so it needs a stable contract rather than visibility
//! promotion of the store's internals: codecs, raw pointer bytes, validated
//! string constructors, the publication lock guard, and every verification
//! helper stay crate-private. What crosses the crate boundary is immutable
//! verified content, the two stamps that identify it, per-project status,
//! and the protected generation roots a collector must honour.
//!
//! Reads and publisher mutations share this facade so pointer verification,
//! cache invalidation, and generation protection stay under one authority.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::project_catalog::{AttachmentId, ProjectId};
use parking_lot::RwLock;

use crate::accepted_publication_store::{
    AcceptedGapSourceV1, AcceptedKnowledgeSourceV1, AcceptedPublicationBuildInputV1,
    AcceptedPublicationBuildSourceV1, AcceptedPublicationFaultInjector,
    AcceptedPublicationGenerationId, AcceptedPublicationGenerationV1, AcceptedPublicationLimits,
    AcceptedPublicationLockGuard, AcceptedPublicationPointerV1, AcceptedPublicationPriorPointerV1,
    AcceptedPublicationSourceBindingV2, AcceptedPublicationStoreError,
    AcceptedPublicationStorePaths, FullPublisherRef, GitObjectId, PointerExpectationV1,
    PreparedAcceptedPublicationV1, VerifiedAcceptedPublicationSelectionV1,
    acquire_accepted_publication_lock, commit_pointer_locked, install_generation_off_lock,
    installed_pointer_tokens_locked, pointer_generation_roots_locked,
    pointer_source_generation_roots_locked, prepare_accepted_publication_v1,
    probe_global_store_locked, selected_pointer_source_binding,
    verify_selected_with_binding_locked,
};

/// The record and manifest shapes a verified view hands out. They are
/// re-exported here so the facade stays the only accepted-publication API a
/// crate-external caller imports; the generation and pointer containers that
/// hold them remain crate-private.
pub use crate::accepted_publication_store::{
    AcceptedBlockingLevelV1, AcceptedEdgeConfidenceV1, AcceptedGapEntryV1, AcceptedGapImpactV1,
    AcceptedGapKindV1, AcceptedGapResolutionV1, AcceptedKnowledgeApprovalV1,
    AcceptedKnowledgeCategoryV1, AcceptedKnowledgeEdgeKindV1, AcceptedKnowledgeEdgeV1,
    AcceptedKnowledgeEntryV1, AcceptedKnowledgePriorityV1, AcceptedKnowledgeScopeV1,
    AcceptedKnowledgeStatusV1, AcceptedPublicationCountsV1, NormalizedRepoRelativeFilename,
    PublicationFileManifestEntryV1, PublicationRecordId, PublicationSha256,
};

/// The accepted-publication authority itself could not be opened. This is
/// the only accepted-publication failure that blocks the listener bind
/// (plan sections 5.4 and 10.3); every per-project failure degrades one
/// project instead.
pub const ERROR_ACCEPTED_PUBLICATION_GLOBAL_STORE_UNAVAILABLE: &str =
    "error.accepted_publication_global_store_unavailable";
/// A project has no accepted pointer, so it has no published content to
/// serve. Migration records this state for a project that acknowledged no
/// published content, and only establish can leave it.
pub const ERROR_ACCEPTED_PUBLICATION_MISSING: &str = "error.accepted_publication_missing";
/// The selected generation does not verify, or reads are being served from
/// the prior pointer. Mutation refuses until the operator repairs, because
/// writing through a damaged pointer discards the repair evidence
/// (plan section 4.8).
pub const ERROR_ACCEPTED_PUBLICATION_REPAIR_REQUIRED: &str =
    "error.accepted_publication_repair_required";

/// An advance presented compare-and-swap tokens that do not match the
/// installed pointer, or an establish found a pointer already present.
pub const ERROR_ACCEPTED_PUBLICATION_POINTER_CONFLICT: &str =
    "error.accepted_publication_pointer_conflict";
/// The full ref moved between preparation and the pointer swap, so the
/// accepted commit this generation names is no longer what the ref points
/// at.
pub const ERROR_ACCEPTED_PUBLICATION_REF_MOVED: &str = "error.accepted_publication_ref_moved";
/// The catalog's current published scope differs from the accepted scope,
/// and only an advance at the current scope clears the bridge.
pub const ERROR_ACCEPTED_PUBLICATION_SCOPE_ADVANCE_REQUIRED: &str =
    "error.accepted_publication_scope_advance_required";
/// A dry-run preparation was handed to the commit path.
pub const ERROR_ACCEPTED_PUBLICATION_DRY_RUN: &str = "error.accepted_publication_dry_run";
/// Internal marker: the caller's freshness recheck refused. The caller's
/// own refusal is what surfaces, never this code.
const ERROR_ACCEPTED_PUBLICATION_FRESHNESS_REFUSED: &str =
    "error.accepted_publication_freshness_refused";

/// Failure detail retained per project by a startup scan. The scan visits
/// every catalog project, so the report is capped and reports how many
/// further failures it dropped rather than growing with the catalog.
const MAX_REPORTED_SCAN_FAILURES: usize = 64;

/// A code-prefixed accepted-publication runtime failure.
///
/// The code is one of the stable `error.accepted_publication_*` prefixes;
/// the detail is bounded and carries no absolute path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedPublicationRuntimeError {
    code: &'static str,
    detail: String,
}

impl AcceptedPublicationRuntimeError {
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        let detail = detail
            .into()
            .chars()
            .map(|ch| if ch.is_control() { ' ' } else { ch })
            .take(512)
            .collect();
        Self { code, detail }
    }

    fn from_store(error: &AcceptedPublicationStoreError) -> Self {
        Self::new(error.code(), error.to_string())
    }

    fn global(error: &AcceptedPublicationStoreError) -> Self {
        Self::new(
            ERROR_ACCEPTED_PUBLICATION_GLOBAL_STORE_UNAVAILABLE,
            error.to_string(),
        )
    }

    pub fn code(&self) -> &'static str {
        self.code
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for AcceptedPublicationRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for AcceptedPublicationRuntimeError {}

/// Which pointer arm served this read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AcceptedPublicationSelection {
    Current,
    Prior,
}

impl AcceptedPublicationSelection {
    fn from_store(selection: VerifiedAcceptedPublicationSelectionV1) -> Self {
        match selection {
            VerifiedAcceptedPublicationSelectionV1::Current => Self::Current,
            VerifiedAcceptedPublicationSelectionV1::Prior => Self::Prior,
        }
    }
}

/// Whether the accepted scope this content was published at still equals
/// the catalog's current published scope. A disagreement is the scope
/// migration bridge of plan section 4.9: the old accepted truth stays
/// readable under its old scope until a new-scope advance clears it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AcceptedPublicationScopeAgreement {
    Agreed,
    RefreshRequired,
    /// No catalog scope was supplied with this read.
    Unevaluated,
}

/// The accepted state of one catalog project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AcceptedPublicationState {
    /// The current pointer and its generation verify.
    Current,
    /// The current pointer arm failed and the prior arm verified. Reads
    /// continue; mutation refuses.
    Prior,
    /// No pointer exists. The project has no published content to serve.
    Missing,
    /// A pointer exists and neither arm verified.
    Corrupt,
}

impl AcceptedPublicationState {
    /// True when accepted published content can be served for this project.
    pub fn serves_published_content(self) -> bool {
        matches!(self, Self::Current | Self::Prior)
    }
}

/// What the publisher mutation surface may attempt, judged from accepted
/// state alone. Attachment selection, capability bits, and the pointer
/// compare-and-swap remain the publisher milestone's own gates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AcceptedPublicationMutationAvailability {
    Available,
    /// No pointer exists, so only an explicit establish can create one.
    EstablishRequired,
    /// Reads run off the prior arm or nothing verifies.
    RepairRequired,
}

/// Immutable accepted content identity (plan section 6.1).
///
/// It contains no path, attachment id, pointer hash, or selection state, so
/// rebinding a pointer to another attachment leaves it unchanged. That is
/// what makes it a safe cache key for projected published content.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AcceptedPublicationContentStamp {
    project_id: ProjectId,
    accepted_scope: PublishedScope,
    full_ref: String,
    accepted_commit: String,
    generation_id: String,
    generation_hash: String,
}

impl AcceptedPublicationContentStamp {
    pub fn project_id(&self) -> &ProjectId {
        &self.project_id
    }

    pub fn accepted_scope(&self) -> &PublishedScope {
        &self.accepted_scope
    }

    pub fn full_ref(&self) -> &str {
        &self.full_ref
    }

    pub fn accepted_commit(&self) -> &str {
        &self.accepted_commit
    }

    pub fn generation_id(&self) -> &str {
        &self.generation_id
    }

    pub fn generation_hash(&self) -> &str {
        &self.generation_hash
    }
}

/// Binding and compare-and-swap identity (plan section 6.2).
///
/// `pointer_sha256` digests the exact installed pointer bytes that were
/// verified, so it is the token an advance compares against.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AcceptedPublicationSourceBinding {
    Attachment {
        attachment_id: AttachmentId,
    },
    Producer {
        producer_id: String,
        source_generation_id: String,
        source_generation_sha256: String,
    },
}

impl AcceptedPublicationSourceBinding {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Attachment { .. } => "attachment",
            Self::Producer { .. } => "producer",
        }
    }

    pub fn attachment_id(&self) -> Option<&AttachmentId> {
        match self {
            Self::Attachment { attachment_id } => Some(attachment_id),
            Self::Producer { .. } => None,
        }
    }

    pub fn producer_id(&self) -> Option<&str> {
        match self {
            Self::Attachment { .. } => None,
            Self::Producer { producer_id, .. } => Some(producer_id),
        }
    }

    pub fn source_generation_id(&self) -> Option<&str> {
        match self {
            Self::Attachment { .. } => None,
            Self::Producer {
                source_generation_id,
                ..
            } => Some(source_generation_id),
        }
    }

    pub fn source_generation_sha256(&self) -> Option<&str> {
        match self {
            Self::Attachment { .. } => None,
            Self::Producer {
                source_generation_sha256,
                ..
            } => Some(source_generation_sha256),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AcceptedPublicationBindingStamp {
    project_id: ProjectId,
    source: AcceptedPublicationSourceBinding,
    pointer_sha256: String,
    selection: AcceptedPublicationSelection,
    accepted_scope: PublishedScope,
}

impl AcceptedPublicationBindingStamp {
    pub fn project_id(&self) -> &ProjectId {
        &self.project_id
    }

    pub fn source(&self) -> &AcceptedPublicationSourceBinding {
        &self.source
    }

    pub fn attachment_id(&self) -> Option<&AttachmentId> {
        self.source.attachment_id()
    }

    pub fn pointer_sha256(&self) -> &str {
        &self.pointer_sha256
    }

    pub fn selection(&self) -> AcceptedPublicationSelection {
        self.selection
    }

    /// The scope this pointer's accepted content was published at. It is a
    /// pointer fact, not a catalog fact: comparing it against the catalog
    /// is `scope_agreement`.
    pub fn accepted_scope(&self) -> &PublishedScope {
        &self.accepted_scope
    }

    /// Agreement against a supplied catalog scope. The stamp stays a pure
    /// function of the installed pointer bytes so it remains a stable
    /// identity, and the catalog comparison happens per read.
    pub fn scope_agreement(
        &self,
        catalog_scope: Option<&PublishedScope>,
    ) -> AcceptedPublicationScopeAgreement {
        match catalog_scope {
            None => AcceptedPublicationScopeAgreement::Unevaluated,
            Some(scope) if scope == &self.accepted_scope => {
                AcceptedPublicationScopeAgreement::Agreed
            }
            Some(_) => AcceptedPublicationScopeAgreement::RefreshRequired,
        }
    }
}

#[derive(Debug)]
struct AcceptedContent {
    stamp: AcceptedPublicationContentStamp,
    generation: AcceptedPublicationGenerationV1,
}

/// The immutable verified view (plan section 6.3).
///
/// The content is shared through an `Arc`, so a rebind that replaces the
/// binding stamp reuses the same decoded generation. Callers cannot mutate
/// this value and cannot construct one without facade verification.
#[derive(Debug, Clone)]
pub struct VerifiedAcceptedPublication {
    content: Arc<AcceptedContent>,
    binding: AcceptedPublicationBindingStamp,
}

impl VerifiedAcceptedPublication {
    pub fn content_stamp(&self) -> &AcceptedPublicationContentStamp {
        &self.content.stamp
    }

    pub fn binding_stamp(&self) -> &AcceptedPublicationBindingStamp {
        &self.binding
    }

    pub fn knowledge_manifest(
        &self,
    ) -> &BTreeMap<NormalizedRepoRelativeFilename, PublicationFileManifestEntryV1> {
        &self.content.generation.knowledge_file_manifest
    }

    pub fn knowledge_records(&self) -> &BTreeMap<PublicationRecordId, AcceptedKnowledgeEntryV1> {
        &self.content.generation.normalized_knowledge
    }

    pub fn gap_manifest(
        &self,
    ) -> &BTreeMap<NormalizedRepoRelativeFilename, PublicationFileManifestEntryV1> {
        &self.content.generation.gap_file_manifest
    }

    pub fn gap_records(&self) -> &BTreeMap<PublicationRecordId, AcceptedGapEntryV1> {
        &self.content.generation.normalized_gaps
    }

    pub fn counts(&self) -> &AcceptedPublicationCountsV1 {
        &self.content.generation.counts
    }

    /// True when two views share one decoded generation allocation. Rebind
    /// must preserve it; advance must not.
    pub fn shares_content_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.content, &other.content)
    }
}

/// Bounded per-project accepted status (plan section 6.8, accepted arm).
///
/// This is observational. The catalog, the attachment store, and the
/// accepted pointer remain authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedPublicationStatus {
    project_id: ProjectId,
    state: AcceptedPublicationState,
    content_stamp: Option<AcceptedPublicationContentStamp>,
    binding_stamp: Option<AcceptedPublicationBindingStamp>,
    scope_agreement: AcceptedPublicationScopeAgreement,
    mutation: AcceptedPublicationMutationAvailability,
    failure: Option<AcceptedPublicationRuntimeError>,
    last_verified_at: SystemTime,
}

impl AcceptedPublicationStatus {
    pub fn project_id(&self) -> &ProjectId {
        &self.project_id
    }

    pub fn state(&self) -> AcceptedPublicationState {
        self.state
    }

    pub fn content_stamp(&self) -> Option<&AcceptedPublicationContentStamp> {
        self.content_stamp.as_ref()
    }

    pub fn binding_stamp(&self) -> Option<&AcceptedPublicationBindingStamp> {
        self.binding_stamp.as_ref()
    }

    pub fn scope_agreement(&self) -> AcceptedPublicationScopeAgreement {
        self.scope_agreement
    }

    pub fn mutation_availability(&self) -> AcceptedPublicationMutationAvailability {
        self.mutation
    }

    /// Accepted-side advance availability only. The publisher surface adds
    /// attachment, capability, and compare-and-swap gates of its own.
    pub fn advance_available(&self) -> bool {
        matches!(
            self.mutation,
            AcceptedPublicationMutationAvailability::Available
        )
    }

    /// True when this project can serve published knowledge and gaps.
    pub fn published_available(&self) -> bool {
        self.state.serves_published_content()
    }

    pub fn failure(&self) -> Option<&AcceptedPublicationRuntimeError> {
        self.failure.as_ref()
    }

    pub fn last_verified_at(&self) -> SystemTime {
        self.last_verified_at
    }

    fn with_scope(&self, catalog_scope: Option<&PublishedScope>) -> Self {
        let scope_agreement = match &self.binding_stamp {
            Some(binding) => binding.scope_agreement(catalog_scope),
            None => AcceptedPublicationScopeAgreement::Unevaluated,
        };
        Self {
            scope_agreement,
            ..self.clone()
        }
    }
}

/// The result of the pre-bind per-project scan.
///
/// Counts are exact. Failure detail is capped so one broken catalog cannot
/// produce an unbounded startup report.
#[derive(Debug, Clone, Default)]
pub struct AcceptedPublicationStartupScan {
    scanned: usize,
    current: usize,
    prior: usize,
    missing: usize,
    corrupt: usize,
    failures: BTreeMap<ProjectId, AcceptedPublicationRuntimeError>,
    dropped_failures: usize,
}

impl AcceptedPublicationStartupScan {
    pub fn scanned(&self) -> usize {
        self.scanned
    }

    pub fn current(&self) -> usize {
        self.current
    }

    pub fn prior(&self) -> usize {
        self.prior
    }

    pub fn missing(&self) -> usize {
        self.missing
    }

    pub fn corrupt(&self) -> usize {
        self.corrupt
    }

    /// Projects whose published capability is unavailable: no pointer, or a
    /// pointer whose arms do not verify.
    pub fn published_unavailable(&self) -> usize {
        self.missing + self.corrupt
    }

    pub fn failures(&self) -> impl Iterator<Item = (&ProjectId, &AcceptedPublicationRuntimeError)> {
        self.failures.iter()
    }

    pub fn dropped_failures(&self) -> usize {
        self.dropped_failures
    }

    fn record(&mut self, status: &AcceptedPublicationStatus) {
        self.scanned += 1;
        match status.state {
            AcceptedPublicationState::Current => self.current += 1,
            AcceptedPublicationState::Prior => self.prior += 1,
            AcceptedPublicationState::Missing => self.missing += 1,
            AcceptedPublicationState::Corrupt => self.corrupt += 1,
        }
        let Some(failure) = status.failure.clone() else {
            return;
        };
        if self.failures.len() < MAX_REPORTED_SCAN_FAILURES {
            self.failures.insert(status.project_id.clone(), failure);
        } else {
            self.dropped_failures += 1;
        }
    }
}

/// Generation ids a collector must not remove (plan section 7.8).
///
/// A project whose pointer could not be read is reported unresolved rather
/// than empty: unreadable authority is not proof that nothing is
/// referenced, so collection must skip that project entirely.
#[derive(Debug, Clone, Default)]
pub struct ProtectedGenerationRoots {
    roots: BTreeMap<ProjectId, BTreeSet<String>>,
    unresolved: BTreeMap<ProjectId, AcceptedPublicationRuntimeError>,
}

impl ProtectedGenerationRoots {
    pub fn projects(&self) -> impl Iterator<Item = (&ProjectId, &BTreeSet<String>)> {
        self.roots.iter()
    }

    pub fn project_roots(&self, project_id: &ProjectId) -> Option<&BTreeSet<String>> {
        self.roots.get(project_id)
    }

    /// True only when this project's roots were fully proved. Collection
    /// for a project that is not resolved is unsafe.
    pub fn is_resolved(&self, project_id: &ProjectId) -> bool {
        !self.unresolved.contains_key(project_id)
    }

    pub fn unresolved(
        &self,
    ) -> impl Iterator<Item = (&ProjectId, &AcceptedPublicationRuntimeError)> {
        self.unresolved.iter()
    }

    pub fn protects(&self, project_id: &ProjectId, generation_id: &str) -> bool {
        self.roots
            .get(project_id)
            .is_some_and(|roots| roots.contains(generation_id))
    }
}

/// The two publish modes (plan §6.5, D-040).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublisherPublishMode {
    /// Create a project's first pointer. Carries no expected-pointer
    /// token: absence under the lock is the whole precondition, and a
    /// present pointer is a conflict rather than an overwrite.
    Establish,
    /// Move an existing pointer. Carries the pointer-specific
    /// compare-and-swap tokens, because the catalog epoch does not
    /// serialize a store the catalog does not own.
    Advance {
        expected_generation_id: String,
        expected_pointer_sha256: String,
    },
}

/// One committed source file, byte-exact as the publisher read it at the
/// accepted commit.
#[derive(Debug, Clone)]
pub struct PublishSourceFile {
    pub repository_relative_filename: String,
    pub source_bytes: Vec<u8>,
}

/// Both lanes of one publication. Knowledge and gaps travel together
/// because the codec binds them into one generation (plan §4.7).
#[derive(Debug, Clone, Default)]
pub struct PublishSources {
    pub knowledge: Vec<PublishSourceFile>,
    pub gaps: Vec<PublishSourceFile>,
}

/// A publish request after the caller has resolved Git and read sources.
#[derive(Debug, Clone)]
pub struct PublishRequest {
    pub mode: PublisherPublishMode,
    pub project_id: ProjectId,
    pub source: AcceptedPublicationSourceBinding,
    /// The catalog's current published scope. Advance always publishes at
    /// this scope, which is what clears a scope-migration bridge.
    pub scope: PublishedScope,
    pub full_ref: String,
    pub accepted_commit: String,
    pub dry_run: bool,
}

/// Generations that are durably installed but not yet named by any
/// pointer (plan §7.8, the in-flight root class).
///
/// A preparation writes its generation before the pointer moves, so
/// between those two moments the file is referenced by nothing on disk. A
/// collector reading only pointer arms would be free to remove it. The
/// registry is refcounted because two preparations can legitimately
/// produce one content id.
#[derive(Debug, Default)]
struct InFlightGenerations {
    roots: RwLock<BTreeMap<ProjectId, BTreeMap<String, usize>>>,
}

impl InFlightGenerations {
    fn acquire(
        self: &Arc<Self>,
        project_id: ProjectId,
        generation_id: String,
    ) -> InFlightGenerationGuard {
        *self
            .roots
            .write()
            .entry(project_id.clone())
            .or_default()
            .entry(generation_id.clone())
            .or_insert(0) += 1;
        InFlightGenerationGuard {
            registry: Arc::clone(self),
            project_id,
            generation_id,
        }
    }

    fn extend_into(&self, project_id: &ProjectId, roots: &mut BTreeSet<String>) {
        if let Some(in_flight) = self.roots.read().get(project_id) {
            roots.extend(in_flight.keys().cloned());
        }
    }
}

/// Holds one in-flight root for as long as its preparation is alive.
/// Dropping the preparation, committed or abandoned, releases it: after a
/// commit the pointer names the generation, and after an abandonment
/// nothing does.
#[derive(Debug)]
struct InFlightGenerationGuard {
    registry: Arc<InFlightGenerations>,
    project_id: ProjectId,
    generation_id: String,
}

impl Drop for InFlightGenerationGuard {
    fn drop(&mut self) {
        let mut roots = self.registry.roots.write();
        let Some(project) = roots.get_mut(&self.project_id) else {
            return;
        };
        if let Some(count) = project.get_mut(&self.generation_id) {
            *count -= 1;
            if *count == 0 {
                project.remove(&self.generation_id);
            }
        }
        if project.is_empty() {
            roots.remove(&self.project_id);
        }
    }
}

/// The off-lock preparation result (plan §6.6).
///
/// It carries no durable mutation receipt. For a real run the generation
/// bytes are already installed when this exists; for a dry run nothing has
/// been written and `commit_publish` refuses it.
#[derive(Debug)]
pub struct PreparedPublish {
    project_id: ProjectId,
    expectation: PointerExpectationV1,
    prepared: PreparedAcceptedPublicationV1,
    generation_installed: bool,
    dry_run: bool,
    /// Keeps the installed generation collectable-proof until this handle
    /// is committed or dropped. `None` for a dry run, which installs
    /// nothing.
    _in_flight: Option<InFlightGenerationGuard>,
}

impl PreparedPublish {
    pub fn project_id(&self) -> &ProjectId {
        &self.project_id
    }

    pub fn generation_id(&self) -> &str {
        self.prepared.generation_id.as_str()
    }

    pub fn generation_hash(&self) -> &str {
        self.prepared.generation_hash.as_str()
    }

    /// Digest of the pointer this preparation would install.
    pub fn pointer_sha256(&self) -> &str {
        self.prepared.pointer_hash.as_str()
    }

    pub fn is_dry_run(&self) -> bool {
        self.dry_run
    }

    /// False when the content-addressed generation was already present
    /// with identical bytes, which is a resumed preparation.
    pub fn generation_installed(&self) -> bool {
        self.generation_installed
    }

    pub fn counts(&self) -> &AcceptedPublicationCountsV1 {
        &self.prepared.generation.counts
    }
}

/// The durable outcome of one publish.
#[derive(Debug, Clone)]
pub struct PublishReceipt {
    generation_id: String,
    generation_hash: String,
    pointer_sha256: String,
    previous_pointer_sha256: Option<String>,
    dry_run: bool,
}

impl PublishReceipt {
    /// The receipt a dry run produces: the identities the real publish
    /// would install, and the explicit statement that nothing was written.
    pub fn dry_run(prepared: &PreparedPublish) -> Self {
        Self {
            generation_id: prepared.generation_id().to_string(),
            generation_hash: prepared.generation_hash().to_string(),
            pointer_sha256: prepared.pointer_sha256().to_string(),
            previous_pointer_sha256: None,
            dry_run: true,
        }
    }

    pub fn generation_id(&self) -> &str {
        &self.generation_id
    }

    pub fn generation_hash(&self) -> &str {
        &self.generation_hash
    }

    /// The compare-and-swap token a following advance must present.
    pub fn pointer_sha256(&self) -> &str {
        &self.pointer_sha256
    }

    pub fn previous_pointer_sha256(&self) -> Option<&str> {
        self.previous_pointer_sha256.as_deref()
    }

    pub fn is_dry_run(&self) -> bool {
        self.dry_run
    }
}

/// A publish failure that keeps the refusing layer's own vocabulary.
///
/// Accepted-publication failures carry their `error.accepted_publication_*`
/// code; a caller's freshness refusal carries the caller's code verbatim,
/// because relabelling a stale-epoch or detached-attachment refusal as a
/// publication error would lose the operator's actual repair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishError {
    code: String,
    detail: String,
    may_have_swapped: bool,
}

impl PublishError {
    /// Build a refusal from a caller-owned stable code, for use inside a
    /// freshness recheck.
    pub fn refusal(code: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            detail: detail
                .into()
                .chars()
                .map(|ch| if ch.is_control() { ' ' } else { ch })
                .take(512)
                .collect(),
            may_have_swapped: false,
        }
    }

    pub fn code(&self) -> &str {
        &self.code
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// True when this failure was raised at or after the atomic pointer
    /// replacement, so the installed pointer may be the new one.
    ///
    /// A caller holding derived state must reverify and reconverge on this,
    /// exactly as it would after a success. Read-back failure is the case
    /// that matters: the swap is durable and the transaction still reports
    /// an error.
    pub fn may_have_swapped(&self) -> bool {
        self.may_have_swapped
    }

    fn with_swap_uncertainty(mut self, may_have_swapped: bool) -> Self {
        self.may_have_swapped = may_have_swapped;
        self
    }
}

impl fmt::Display for PublishError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for PublishError {}

impl From<AcceptedPublicationStoreError> for PublishError {
    fn from(error: AcceptedPublicationStoreError) -> Self {
        Self::refusal(error.code(), error.to_string())
    }
}

impl From<AcceptedPublicationRuntimeError> for PublishError {
    fn from(error: AcceptedPublicationRuntimeError) -> Self {
        Self::refusal(error.code(), error.detail())
    }
}

#[derive(Debug, Default)]
struct ProjectCacheEntry {
    /// Retained across a binding change: rebinding a pointer to another
    /// attachment does not change accepted content.
    content: Option<Arc<AcceptedContent>>,
    status: Option<AcceptedPublicationStatus>,
    binding: Option<AcceptedPublicationBindingStamp>,
}

enum ProjectReadOutcome {
    Missing,
    Verified {
        content: Arc<AcceptedContent>,
        binding: AcceptedPublicationBindingStamp,
    },
    Corrupt(AcceptedPublicationRuntimeError),
}

/// The narrow public facade over the accepted-publication store.
///
/// One instance per daemon process in catalog mode. Bridge mode never
/// constructs it: bridge published reads keep their legacy publisher
/// authority untouched.
#[derive(Debug)]
pub struct AcceptedPublicationRuntime {
    paths: AcceptedPublicationStorePaths,
    limits: AcceptedPublicationLimits,
    cache: RwLock<BTreeMap<ProjectId, ProjectCacheEntry>>,
    /// Monotonic invalidation fence for reads that verify under the store
    /// lock and install into the process cache afterward. The cache write
    /// lock serializes the final revision check with every invalidation, so a
    /// read that raced a publish, bind, or catalog-side detach can return its
    /// point-in-time view but cannot reinstall it for later callers.
    cache_revision: AtomicU64,
    /// Test-only interruption hook for the publish transaction. Production
    /// leaves it `None`.
    faults: Option<Arc<dyn AcceptedPublicationFaultInjector>>,
    in_flight: Arc<InFlightGenerations>,
}

impl AcceptedPublicationRuntime {
    /// Open the global accepted-publication store and prove this process
    /// can act as its authority: safe derived paths, an acquirable
    /// publication lock, and real directories rather than redirects.
    ///
    /// An absent store root is success, not failure. It is the state of a
    /// catalog whose projects have not published yet, and the per-project
    /// scan reports each of those as publication-missing. Only a failure
    /// here blocks the listener bind.
    pub fn open_global(projects_path: &Path) -> Result<Self, AcceptedPublicationRuntimeError> {
        let paths = AcceptedPublicationStorePaths::derive(projects_path)
            .map_err(|error| AcceptedPublicationRuntimeError::global(&error))?;
        let guard = acquire_accepted_publication_lock(&paths)
            .map_err(|error| AcceptedPublicationRuntimeError::global(&error))?;
        probe_global_store_locked(&paths, &guard)
            .map_err(|error| AcceptedPublicationRuntimeError::global(&error))?;
        drop(guard);
        Ok(Self {
            paths,
            limits: AcceptedPublicationLimits::default(),
            cache: RwLock::new(BTreeMap::new()),
            cache_revision: AtomicU64::new(0),
            faults: None,
            in_flight: Arc::new(InFlightGenerations::default()),
        })
    }

    /// Verified accepted content for one project.
    ///
    /// No checkout lease, Git call, publisher election, authorization TTL,
    /// or repo-local recall read happens on this path.
    pub fn load_verified(
        &self,
        project_id: &ProjectId,
    ) -> Result<VerifiedAcceptedPublication, AcceptedPublicationRuntimeError> {
        if let Some(view) = self.cached_view(project_id) {
            return Ok(view);
        }
        match self.refresh(project_id, true)?.0 {
            ProjectReadOutcome::Verified { content, binding } => {
                Ok(VerifiedAcceptedPublication { content, binding })
            }
            ProjectReadOutcome::Missing => Err(AcceptedPublicationRuntimeError::new(
                ERROR_ACCEPTED_PUBLICATION_MISSING,
                "project has no accepted publication pointer",
            )),
            ProjectReadOutcome::Corrupt(error) => Err(error),
        }
    }

    /// Bounded accepted status for one project. Per-project damage is a
    /// status, never an error; only losing the global store is an error.
    pub fn status(
        &self,
        project_id: &ProjectId,
        catalog_scope: Option<&PublishedScope>,
    ) -> Result<AcceptedPublicationStatus, AcceptedPublicationRuntimeError> {
        if let Some(status) = self.cache.read().get(project_id).and_then(|entry| {
            entry
                .status
                .as_ref()
                .map(|status| status.with_scope(catalog_scope))
        }) {
            return Ok(status);
        }
        let (_, status) = self.refresh(project_id, false)?;
        Ok(status.with_scope(catalog_scope))
    }

    /// The pre-bind scan (plan section 5.4). It verifies every supplied
    /// project under one lock acquisition and retains status only, so peak
    /// memory stays one decoded generation regardless of catalog size.
    ///
    /// One corrupt project degrades that project. It never aborts the scan.
    pub fn startup_scan<I>(
        &self,
        projects: I,
    ) -> Result<AcceptedPublicationStartupScan, AcceptedPublicationRuntimeError>
    where
        I: IntoIterator<Item = (ProjectId, Option<PublishedScope>)>,
    {
        let guard = self.lock()?;
        let mut scan = AcceptedPublicationStartupScan::default();
        for (project_id, catalog_scope) in projects {
            let observed_revision = self.cache_revision.load(Ordering::Acquire);
            let outcome = self.read_project(&guard, &project_id);
            let status = self.install(&project_id, &outcome, false, observed_revision);
            scan.record(&status.with_scope(catalog_scope.as_ref()));
        }
        Ok(scan)
    }

    /// Drop the cached binding for one project, keeping its verified
    /// content. This is the rebind invalidation: the pointer's attachment
    /// changed, the accepted bytes did not.
    pub fn invalidate_binding(&self, project_id: &ProjectId) {
        let mut cache = self.cache.write();
        self.cache_revision.fetch_add(1, Ordering::Release);
        if let Some(entry) = cache.get_mut(project_id) {
            entry.binding = None;
            entry.status = None;
        }
    }

    /// Drop cached content and binding for one project. This is the
    /// advance invalidation: new accepted content replaces the old.
    pub fn invalidate_content(&self, project_id: &ProjectId) {
        let mut cache = self.cache.write();
        self.cache_revision.fetch_add(1, Ordering::Release);
        cache.remove(project_id);
    }

    /// Generation ids that must survive collection: every supplied
    /// project's current and prior pointer arms, plus every generation
    /// pinned by a cached read.
    pub fn protected_generation_roots<I>(
        &self,
        projects: I,
    ) -> Result<ProtectedGenerationRoots, AcceptedPublicationRuntimeError>
    where
        I: IntoIterator<Item = ProjectId>,
    {
        let guard = self.lock()?;
        let mut protected = ProtectedGenerationRoots::default();
        for project_id in projects {
            let mut roots = BTreeSet::new();
            match pointer_generation_roots_locked(&self.paths, &guard, &project_id, &self.limits) {
                Ok(Some(pointer_roots)) => {
                    roots.extend(pointer_roots.into_iter().map(|id| id.as_str().to_string()));
                }
                Ok(None) => {}
                Err(error) => {
                    protected.unresolved.insert(
                        project_id.clone(),
                        AcceptedPublicationRuntimeError::from_store(&error),
                    );
                }
            }
            if let Some(pinned) = self
                .cache
                .read()
                .get(&project_id)
                .and_then(|cached| cached.content.as_ref())
            {
                roots.insert(pinned.stamp.generation_id.clone());
            }
            // Installed-but-uncommitted preparations are referenced by no
            // pointer yet and must survive collection anyway.
            self.in_flight.extend_into(&project_id, &mut roots);
            protected.roots.insert(project_id, roots);
        }
        Ok(protected)
    }

    pub fn protected_source_generation_roots<I>(
        &self,
        projects: I,
    ) -> Result<BTreeSet<String>, AcceptedPublicationRuntimeError>
    where
        I: IntoIterator<Item = ProjectId>,
    {
        let guard = self.lock()?;
        let mut roots = BTreeSet::new();
        for project_id in projects {
            if let Some(project_roots) = pointer_source_generation_roots_locked(
                &self.paths,
                &guard,
                &project_id,
                &self.limits,
            )
            .map_err(|error| AcceptedPublicationRuntimeError::from_store(&error))?
            {
                roots.extend(project_roots);
            }
        }
        Ok(roots)
    }

    /// Off-lock preparation (plan §7.2 and §4.6).
    ///
    /// Everything expensive happens here: normalization, dual-lane
    /// validation, encoding, and the immutable generation write with its
    /// fsync. The publication lock is taken only for the brief token read
    /// that builds an advance's prior arm, never across encoding or the
    /// generation write. A dry run stops before any durable write.
    pub fn prepare_publish(
        &self,
        request: PublishRequest,
        sources: PublishSources,
    ) -> Result<PreparedPublish, PublishError> {
        let full_ref = FullPublisherRef::parse(request.full_ref)?;
        let accepted_commit = GitObjectId::parse(request.accepted_commit)?;
        let (expectation, prior_pointer) = match &request.mode {
            PublisherPublishMode::Establish => (PointerExpectationV1::Establish, None),
            PublisherPublishMode::Advance {
                expected_generation_id,
                expected_pointer_sha256,
            } => {
                let expected_generation =
                    AcceptedPublicationGenerationId::parse(expected_generation_id.clone())?;
                let expected_pointer_sha256 =
                    PublicationSha256::parse(expected_pointer_sha256.clone())?;
                // One short locked read: the prior arm must be the exact
                // pointer this advance intends to replace, and presenting
                // the wrong token here fails before any encoding work.
                let guard = self.lock()?;
                let installed = installed_pointer_tokens_locked(
                    &self.paths,
                    &guard,
                    &request.project_id,
                    &self.limits,
                )?;
                drop(guard);
                let Some((pointer, digest)) = installed else {
                    return Err(PublishError::refusal(
                        ERROR_ACCEPTED_PUBLICATION_POINTER_CONFLICT,
                        "advance requires an installed pointer; establish creates the first one",
                    ));
                };
                if digest != expected_pointer_sha256
                    || pointer.accepted_generation != expected_generation
                {
                    return Err(PublishError::refusal(
                        ERROR_ACCEPTED_PUBLICATION_POINTER_CONFLICT,
                        "the installed pointer does not match the expected compare-and-swap tokens",
                    ));
                }
                let prior = prior_pointer_from(&pointer);
                (
                    PointerExpectationV1::Advance {
                        expected_generation,
                        expected_pointer_sha256,
                    },
                    Some(prior),
                )
            }
        };
        let prepared = prepare_accepted_publication_v1(
            AcceptedPublicationBuildInputV1 {
                project_id: request.project_id.clone(),
                source_binding: match request.source {
                    AcceptedPublicationSourceBinding::Attachment { attachment_id } => {
                        AcceptedPublicationBuildSourceV1::Attachment(attachment_id)
                    }
                    AcceptedPublicationSourceBinding::Producer {
                        producer_id,
                        source_generation_id,
                        source_generation_sha256,
                    } => AcceptedPublicationBuildSourceV1::Producer {
                        producer_id,
                        source_generation_id,
                        source_generation_sha256: PublicationSha256::parse(
                            source_generation_sha256,
                        )?,
                    },
                },
                scope: request.scope,
                full_ref,
                accepted_commit,
                knowledge: sources
                    .knowledge
                    .into_iter()
                    .map(|file| AcceptedKnowledgeSourceV1 {
                        repository_relative_filename: file.repository_relative_filename,
                        source_bytes: file.source_bytes,
                    })
                    .collect(),
                gaps: sources
                    .gaps
                    .into_iter()
                    .map(|file| AcceptedGapSourceV1 {
                        repository_relative_filename: file.repository_relative_filename,
                        source_bytes: file.source_bytes,
                    })
                    .collect(),
                prior_pointer,
            },
            &self.limits,
        )?;
        if request.dry_run {
            return Ok(PreparedPublish {
                project_id: request.project_id,
                expectation,
                prepared,
                generation_installed: false,
                dry_run: true,
                _in_flight: None,
            });
        }
        // Register the root BEFORE the write: a collector that reads roots
        // between the write and the registration would see an unreferenced
        // file, which is the exact window this class of root exists for.
        let in_flight = self.in_flight.acquire(
            request.project_id.clone(),
            prepared.generation_id.as_str().to_string(),
        );
        let outcome = install_generation_off_lock(
            &self.paths,
            &request.project_id,
            &prepared,
            self.faults.as_deref(),
        )?;
        Ok(PreparedPublish {
            project_id: request.project_id,
            expectation,
            prepared,
            generation_installed: outcome.created,
            dry_run: false,
            _in_flight: Some(in_flight),
        })
    }

    /// Pointer commit under the publication lock (plan §7.3).
    ///
    /// `freshness` runs inside the lock immediately before the swap: it is
    /// where the caller rechecks catalog epoch, attachment status, and the
    /// live ref. Its refusal propagates verbatim.
    ///
    /// D-033 item 1 survives this design and is not claimed closed: catalog
    /// detach does not take the publication lock, so a detach landing in
    /// the final window leaves a pointer naming a freshly detached
    /// attachment. That is a misleading binding, reported by status and
    /// repaired by bind, never corruption.
    pub fn commit_publish(
        &self,
        prepared: PreparedPublish,
        freshness: &mut dyn FnMut() -> Result<(), PublishError>,
    ) -> Result<PublishReceipt, PublishError> {
        if prepared.dry_run {
            return Err(PublishError::refusal(
                ERROR_ACCEPTED_PUBLICATION_DRY_RUN,
                "a dry-run preparation installs nothing and cannot be committed",
            ));
        }
        let guard = self.lock()?;
        let mut refusal: Option<PublishError> = None;
        let mut swap_attempted = false;
        let receipt = commit_pointer_locked(
            &self.paths,
            &guard,
            &prepared.project_id,
            &prepared.prepared,
            &prepared.expectation,
            &self.limits,
            self.faults.as_deref(),
            &mut || match freshness() {
                Ok(()) => Ok(()),
                Err(error) => {
                    // Carry the caller's code out around the store's error
                    // type instead of flattening it into a publication code.
                    refusal = Some(error);
                    Err(AcceptedPublicationStoreError::new(
                        ERROR_ACCEPTED_PUBLICATION_FRESHNESS_REFUSED,
                        "the caller's freshness recheck refused this publish",
                    ))
                }
            },
            &mut swap_attempted,
        );
        // Invalidate on ANY outcome that reached the swap, not just success.
        // A read-back failure leaves the new pointer durably installed while
        // reporting an error, and a runtime that kept serving its cached
        // content would serve a generation the store no longer points at.
        if swap_attempted {
            self.invalidate_content(&prepared.project_id);
        }
        drop(guard);
        let receipt = match receipt {
            Ok(receipt) => receipt,
            Err(error) => {
                return Err(refusal
                    .unwrap_or_else(|| error.into())
                    .with_swap_uncertainty(swap_attempted));
            }
        };
        Ok(PublishReceipt {
            generation_id: receipt.generation_id.as_str().to_string(),
            generation_hash: prepared.prepared.generation_hash.as_str().to_string(),
            pointer_sha256: receipt.pointer_sha256.as_str().to_string(),
            previous_pointer_sha256: receipt
                .previous_pointer_sha256
                .map(|digest| digest.as_str().to_string()),
            dry_run: false,
        })
    }

    /// The compare-and-swap tokens a following advance must present, or
    /// `None` when this project has no pointer and must establish first.
    pub fn advance_tokens(
        &self,
        project_id: &ProjectId,
    ) -> Result<Option<(String, String)>, AcceptedPublicationRuntimeError> {
        let guard = self.lock()?;
        let installed =
            installed_pointer_tokens_locked(&self.paths, &guard, project_id, &self.limits)
                .map_err(|error| AcceptedPublicationRuntimeError::from_store(&error))?;
        Ok(installed.map(|(pointer, digest)| {
            (
                pointer.accepted_generation.as_str().to_string(),
                digest.as_str().to_string(),
            )
        }))
    }

    /// Test-only: install the publish transaction's interruption hook.
    #[cfg(test)]
    pub(crate) fn install_fault_injector_for_test(
        &mut self,
        faults: Arc<dyn AcceptedPublicationFaultInjector>,
    ) {
        self.faults = Some(faults);
    }

    fn lock(&self) -> Result<AcceptedPublicationLockGuard, AcceptedPublicationRuntimeError> {
        acquire_accepted_publication_lock(&self.paths)
            .map_err(|error| AcceptedPublicationRuntimeError::global(&error))
    }

    fn cached_view(&self, project_id: &ProjectId) -> Option<VerifiedAcceptedPublication> {
        let cache = self.cache.read();
        let entry = cache.get(project_id)?;
        Some(VerifiedAcceptedPublication {
            content: entry.content.clone()?,
            binding: entry.binding.clone()?,
        })
    }

    fn refresh(
        &self,
        project_id: &ProjectId,
        retain_content: bool,
    ) -> Result<(ProjectReadOutcome, AcceptedPublicationStatus), AcceptedPublicationRuntimeError>
    {
        let observed_revision = self.cache_revision.load(Ordering::Acquire);
        let guard = self.lock()?;
        let outcome = self.read_project(&guard, project_id);
        let outcome = self.reuse_cached_content(project_id, outcome);
        let status = self.install(project_id, &outcome, retain_content, observed_revision);
        drop(guard);
        Ok((outcome, status))
    }

    fn read_project(
        &self,
        guard: &AcceptedPublicationLockGuard,
        project_id: &ProjectId,
    ) -> ProjectReadOutcome {
        match verify_selected_with_binding_locked(&self.paths, guard, project_id, &self.limits) {
            Ok(None) => ProjectReadOutcome::Missing,
            Ok(Some(read)) => {
                let selection = read.verified.selection;
                let generation = read.verified.generation;
                // The pointer's top-level hash names the CURRENT generation,
                // so a prior-arm read must stamp the prior hash or the
                // content stamp would identify content this read did not
                // serve. Every other content field comes from the generation
                // that verified, so it is already arm-correct.
                let generation_hash = match (selection, read.pointer.prior_pointer.as_ref()) {
                    (VerifiedAcceptedPublicationSelectionV1::Prior, Some(prior)) => {
                        prior.generation_hash.as_str()
                    }
                    _ => read.pointer.generation_hash.as_str(),
                };
                let stamp = AcceptedPublicationContentStamp {
                    project_id: generation.project_id.clone(),
                    accepted_scope: generation.scope.clone(),
                    full_ref: generation.full_ref.as_str().to_string(),
                    accepted_commit: generation.accepted_commit.as_str().to_string(),
                    generation_id: read.verified.generation_id.as_str().to_string(),
                    generation_hash: generation_hash.to_string(),
                };
                let source = match selected_pointer_source_binding(&read.pointer, selection) {
                    Ok(source) => runtime_source_binding(source),
                    Err(error) => {
                        return ProjectReadOutcome::Corrupt(
                            AcceptedPublicationRuntimeError::from_store(&error),
                        );
                    }
                };
                let binding_stamp = AcceptedPublicationBindingStamp {
                    project_id: read.pointer.project_id.clone(),
                    source,
                    pointer_sha256: read.pointer_sha256.as_str().to_string(),
                    selection: AcceptedPublicationSelection::from_store(selection),
                    accepted_scope: generation.scope.clone(),
                };
                ProjectReadOutcome::Verified {
                    content: Arc::new(AcceptedContent { stamp, generation }),
                    binding: binding_stamp,
                }
            }
            Err(error) => {
                ProjectReadOutcome::Corrupt(AcceptedPublicationRuntimeError::from_store(&error))
            }
        }
    }

    /// Keep the existing allocation when a re-read produced byte-identical
    /// content identity, so a rebind does not evict decoded content.
    fn reuse_cached_content(
        &self,
        project_id: &ProjectId,
        outcome: ProjectReadOutcome,
    ) -> ProjectReadOutcome {
        let ProjectReadOutcome::Verified { content, binding } = outcome else {
            return outcome;
        };
        let cached = self
            .cache
            .read()
            .get(project_id)
            .and_then(|entry| entry.content.clone())
            .filter(|cached| cached.stamp == content.stamp);
        ProjectReadOutcome::Verified {
            content: cached.unwrap_or(content),
            binding,
        }
    }

    fn install(
        &self,
        project_id: &ProjectId,
        outcome: &ProjectReadOutcome,
        retain_content: bool,
        observed_revision: u64,
    ) -> AcceptedPublicationStatus {
        let status = status_from(project_id, outcome);
        let mut cache = self.cache.write();
        if self.cache_revision.load(Ordering::Acquire) != observed_revision {
            return status;
        }
        let entry = cache.entry(project_id.clone()).or_default();
        match outcome {
            ProjectReadOutcome::Verified { content, binding } => {
                if binding.selection == AcceptedPublicationSelection::Prior {
                    // Prior is a verified availability fallback, but the
                    // current-arm failure may have been transient. Serve this
                    // call and force the next load/status to retry current
                    // rather than latching repair-required indefinitely.
                    entry.content = None;
                    entry.binding = None;
                    entry.status = None;
                    return status;
                } else if retain_content {
                    entry.content = Some(content.clone());
                    entry.binding = Some(binding.clone());
                } else {
                    // A bounded scan does not retain a newly decoded payload,
                    // but a status refresh must not evict byte-identical
                    // content already pinned by a prior verified load.
                    if entry
                        .content
                        .as_ref()
                        .is_some_and(|cached| cached.stamp == content.stamp)
                    {
                        entry.binding = Some(binding.clone());
                    } else {
                        entry.content = None;
                        entry.binding = None;
                    }
                }
            }
            ProjectReadOutcome::Missing | ProjectReadOutcome::Corrupt(_) => {
                entry.content = None;
                entry.binding = None;
            }
        }
        entry.status = Some(status.clone());
        status
    }
}

fn runtime_source_binding(
    binding: AcceptedPublicationSourceBindingV2,
) -> AcceptedPublicationSourceBinding {
    match binding {
        AcceptedPublicationSourceBindingV2::Attachment { attachment_id } => {
            AcceptedPublicationSourceBinding::Attachment { attachment_id }
        }
        AcceptedPublicationSourceBindingV2::Producer {
            producer_id,
            source_generation_id,
            source_generation_sha256,
        } => AcceptedPublicationSourceBinding::Producer {
            producer_id,
            source_generation_id,
            source_generation_sha256: source_generation_sha256.as_str().to_string(),
        },
    }
}

/// The prior arm an advance carries: the exact pointer it replaces.
fn prior_pointer_from(pointer: &AcceptedPublicationPointerV1) -> AcceptedPublicationPriorPointerV1 {
    AcceptedPublicationPriorPointerV1 {
        attachment_id: pointer.attachment_id.clone(),
        source_binding: pointer.source_binding.clone(),
        full_ref: pointer.full_ref.clone(),
        accepted_commit: pointer.accepted_commit.clone(),
        accepted_scope: pointer.accepted_scope.clone(),
        accepted_generation: pointer.accepted_generation.clone(),
        generation_hash: pointer.generation_hash.clone(),
    }
}

fn status_from(project_id: &ProjectId, outcome: &ProjectReadOutcome) -> AcceptedPublicationStatus {
    let (state, content_stamp, binding_stamp, mutation, failure) = match outcome {
        ProjectReadOutcome::Missing => (
            AcceptedPublicationState::Missing,
            None,
            None,
            AcceptedPublicationMutationAvailability::EstablishRequired,
            None,
        ),
        ProjectReadOutcome::Corrupt(error) => (
            AcceptedPublicationState::Corrupt,
            None,
            None,
            AcceptedPublicationMutationAvailability::RepairRequired,
            Some(error.clone()),
        ),
        ProjectReadOutcome::Verified { content, binding } => match binding.selection {
            AcceptedPublicationSelection::Current => (
                AcceptedPublicationState::Current,
                Some(content.stamp.clone()),
                Some(binding.clone()),
                AcceptedPublicationMutationAvailability::Available,
                None,
            ),
            AcceptedPublicationSelection::Prior => (
                AcceptedPublicationState::Prior,
                Some(content.stamp.clone()),
                Some(binding.clone()),
                AcceptedPublicationMutationAvailability::RepairRequired,
                Some(AcceptedPublicationRuntimeError::new(
                    ERROR_ACCEPTED_PUBLICATION_REPAIR_REQUIRED,
                    "current accepted generation did not verify; reads are served from prior",
                )),
            ),
        },
    };
    AcceptedPublicationStatus {
        project_id: project_id.clone(),
        state,
        content_stamp,
        binding_stamp,
        scope_agreement: AcceptedPublicationScopeAgreement::Unevaluated,
        mutation,
        failure,
        last_verified_at: SystemTime::now(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc as StdArc;

    use crate::accepted_publication_store::fixtures;
    use crate::accepted_publication_store::{
        AcceptedPublicationFaultPoint, AcceptedPublicationGenerationId, AcceptedPublicationLimits,
        MAX_ACCEPTED_PUBLICATION_POINTER_BYTES, PreparedAcceptedPublicationV1,
        acquire_accepted_publication_lock, rebind_pointer_attachment_locked,
    };
    use crate::checkout_access::{
        CheckoutAccessBroker, CheckoutAccessObservations, DenyCheckoutAccess,
    };

    use super::*;

    const COMMIT_ONE: &str = "1111111111111111111111111111111111111111";
    const COMMIT_TWO: &str = "2222222222222222222222222222222222222222";
    const COMMIT_THREE: &str = "3333333333333333333333333333333333333333";

    struct Fixture {
        _directory: tempfile::TempDir,
        projects_path: std::path::PathBuf,
        paths: AcceptedPublicationStorePaths,
    }

    fn fixture() -> Fixture {
        let directory = tempfile::tempdir().unwrap();
        // Canonicalize before deriving: the code under test opens the
        // canonical path, and a raw `/var/...` root would not match.
        let root = directory.path().canonicalize().unwrap();
        let projects_path = root.join("projects.json");
        let paths = AcceptedPublicationStorePaths::derive(&projects_path).unwrap();
        Fixture {
            _directory: directory,
            projects_path,
            paths,
        }
    }

    impl Fixture {
        fn runtime(&self) -> AcceptedPublicationRuntime {
            AcceptedPublicationRuntime::open_global(&self.projects_path).unwrap()
        }
    }

    fn project(id: &str) -> ProjectId {
        ProjectId::parse(id).unwrap()
    }

    fn attachment(suffix: &str) -> AttachmentId {
        AttachmentId::parse(format!("att_{suffix:0>32}")).unwrap()
    }

    fn scope() -> PublishedScope {
        PublishedScope::try_new("repo_example", ".").unwrap()
    }

    fn publish(
        paths: &AcceptedPublicationStorePaths,
        project_id: &ProjectId,
        commit: &str,
        content: &str,
        prior: Option<&PreparedAcceptedPublicationV1>,
    ) -> PreparedAcceptedPublicationV1 {
        let prepared = fixtures::prepare(
            project_id,
            &attachment("a1"),
            &scope(),
            commit,
            content,
            prior.map(fixtures::prior_of),
        );
        fixtures::install(paths, project_id, &prepared);
        prepared
    }

    fn rewrite_pointer_field(
        paths: &AcceptedPublicationStorePaths,
        project_id: &ProjectId,
        field: &str,
        value: &str,
    ) {
        let path = paths.pointer(project_id);
        let mut pointer: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        pointer[field] = serde_json::Value::String(value.to_string());
        fs::write(&path, serde_json::to_vec_pretty(&pointer).unwrap()).unwrap();
    }

    #[test]
    fn current_selection_serves_verified_content_with_both_stamps() {
        let fixture = fixture();
        let project_id = project("p_current");
        let prepared = publish(&fixture.paths, &project_id, COMMIT_ONE, "accepted", None);
        let runtime = fixture.runtime();

        let verified = runtime.load_verified(&project_id).unwrap();
        let content = verified.content_stamp();
        assert_eq!(content.project_id(), &project_id);
        assert_eq!(content.accepted_scope(), &scope());
        assert_eq!(content.full_ref(), "refs/heads/main");
        assert_eq!(content.accepted_commit(), COMMIT_ONE);
        assert_eq!(content.generation_id(), prepared.generation_id.as_str());
        assert_eq!(content.generation_hash(), prepared.generation_hash.as_str());

        let binding = verified.binding_stamp();
        assert_eq!(binding.project_id(), &project_id);
        assert_eq!(binding.attachment_id(), Some(&attachment("a1")));
        assert_eq!(binding.pointer_sha256(), prepared.pointer_hash.as_str());
        assert_eq!(binding.selection(), AcceptedPublicationSelection::Current);
        assert_eq!(
            binding.scope_agreement(Some(&scope())),
            AcceptedPublicationScopeAgreement::Agreed
        );

        assert_eq!(verified.knowledge_records().len(), 1);
        assert_eq!(verified.gap_records().len(), 1);
        assert_eq!(verified.knowledge_manifest().len(), 1);
        assert_eq!(verified.gap_manifest().len(), 1);
        assert_eq!(verified.counts().knowledge_entries, 1);
        assert_eq!(verified.counts().gap_entries, 1);
        assert_eq!(
            verified
                .knowledge_records()
                .values()
                .next()
                .unwrap()
                .content,
            "accepted"
        );

        let status = runtime.status(&project_id, Some(&scope())).unwrap();
        assert_eq!(status.state(), AcceptedPublicationState::Current);
        assert!(status.published_available());
        assert!(status.advance_available());
        assert!(status.failure().is_none());
        assert_eq!(
            status.scope_agreement(),
            AcceptedPublicationScopeAgreement::Agreed
        );
    }

    #[test]
    fn scope_migration_keeps_old_accepted_truth_and_reports_refresh() {
        let fixture = fixture();
        let project_id = project("p_scope");
        publish(&fixture.paths, &project_id, COMMIT_ONE, "accepted", None);
        let runtime = fixture.runtime();
        let migrated = PublishedScope::try_new("repo_example", "sub/project").unwrap();

        let status = runtime.status(&project_id, Some(&migrated)).unwrap();
        assert_eq!(status.state(), AcceptedPublicationState::Current);
        assert_eq!(
            status.scope_agreement(),
            AcceptedPublicationScopeAgreement::RefreshRequired
        );
        // The old accepted snapshot is never relabeled: it keeps serving
        // under the scope it was published at until a new-scope advance.
        assert_eq!(
            runtime
                .load_verified(&project_id)
                .unwrap()
                .content_stamp()
                .accepted_scope(),
            &scope()
        );
    }

    #[test]
    fn prior_fallback_serves_prior_content_and_refuses_mutation() {
        let fixture = fixture();
        let project_id = project("p_prior");
        let first = publish(&fixture.paths, &project_id, COMMIT_ONE, "first", None);
        let second = publish(
            &fixture.paths,
            &project_id,
            COMMIT_TWO,
            "second",
            Some(&first),
        );
        fixtures::corrupt_generation(&fixture.paths, &project_id, &second.generation_id);
        let runtime = fixture.runtime();

        let verified = runtime.load_verified(&project_id).unwrap();
        assert_eq!(
            verified.binding_stamp().selection(),
            AcceptedPublicationSelection::Prior
        );
        // The content stamp must name the arm that actually verified, not
        // the damaged current arm the pointer's head names.
        assert_eq!(
            verified.content_stamp().generation_id(),
            first.generation_id.as_str()
        );
        assert_eq!(
            verified.content_stamp().generation_hash(),
            first.generation_hash.as_str()
        );
        assert_eq!(verified.content_stamp().accepted_commit(), COMMIT_ONE);
        assert_eq!(
            verified
                .knowledge_records()
                .values()
                .next()
                .unwrap()
                .content,
            "first"
        );

        let status = runtime.status(&project_id, Some(&scope())).unwrap();
        assert_eq!(status.state(), AcceptedPublicationState::Prior);
        assert!(status.published_available());
        assert!(!status.advance_available());
        assert_eq!(
            status.mutation_availability(),
            AcceptedPublicationMutationAvailability::RepairRequired
        );
        assert_eq!(
            status.failure().unwrap().code(),
            ERROR_ACCEPTED_PUBLICATION_REPAIR_REQUIRED
        );
    }

    #[test]
    fn missing_pointer_is_publication_unavailable_not_corruption() {
        let fixture = fixture();
        let project_id = project("p_missing");
        let runtime = fixture.runtime();

        let error = runtime.load_verified(&project_id).unwrap_err();
        assert_eq!(error.code(), ERROR_ACCEPTED_PUBLICATION_MISSING);

        let status = runtime.status(&project_id, Some(&scope())).unwrap();
        assert_eq!(status.state(), AcceptedPublicationState::Missing);
        assert!(!status.published_available());
        assert!(!status.advance_available());
        assert_eq!(
            status.mutation_availability(),
            AcceptedPublicationMutationAvailability::EstablishRequired
        );
        assert!(status.content_stamp().is_none());
        assert!(status.binding_stamp().is_none());

        // An installed sibling proves the absent-pointer arm is per project,
        // not a store-wide absence.
        let other = project("p_other");
        publish(&fixture.paths, &other, COMMIT_ONE, "accepted", None);
        let runtime = fixture.runtime();
        assert_eq!(
            runtime.status(&project_id, None).unwrap().state(),
            AcceptedPublicationState::Missing
        );
        assert_eq!(
            runtime.status(&other, None).unwrap().state(),
            AcceptedPublicationState::Current
        );
    }

    #[test]
    fn corrupt_current_without_prior_and_with_corrupt_prior_are_both_corrupt() {
        let fixture = fixture();
        let project_id = project("p_corrupt");
        let first = publish(&fixture.paths, &project_id, COMMIT_ONE, "first", None);
        fixtures::corrupt_generation(&fixture.paths, &project_id, &first.generation_id);
        let runtime = fixture.runtime();

        let status = runtime.status(&project_id, Some(&scope())).unwrap();
        assert_eq!(status.state(), AcceptedPublicationState::Corrupt);
        assert!(!status.published_available());
        assert_eq!(
            status.mutation_availability(),
            AcceptedPublicationMutationAvailability::RepairRequired
        );
        assert!(status.failure().is_some());
        assert!(runtime.load_verified(&project_id).is_err());

        // Advance to a second generation, then damage both arms.
        let second_project = project("p_corrupt_pair");
        let first = publish(&fixture.paths, &second_project, COMMIT_ONE, "first", None);
        let second = publish(
            &fixture.paths,
            &second_project,
            COMMIT_TWO,
            "second",
            Some(&first),
        );
        fixtures::corrupt_generation(&fixture.paths, &second_project, &second.generation_id);
        fixtures::corrupt_generation(&fixture.paths, &second_project, &first.generation_id);
        let runtime = fixture.runtime();
        assert_eq!(
            runtime.status(&second_project, None).unwrap().state(),
            AcceptedPublicationState::Corrupt
        );
    }

    #[test]
    fn pointer_field_and_generation_hash_mismatches_fail_closed() {
        let fixture = fixture();
        let mismatched_field = project("p_field");
        publish(
            &fixture.paths,
            &mismatched_field,
            COMMIT_ONE,
            "accepted",
            None,
        );
        // The pointer now claims a commit its generation does not carry.
        rewrite_pointer_field(
            &fixture.paths,
            &mismatched_field,
            "accepted_commit",
            COMMIT_TWO,
        );

        let mismatched_hash = project("p_hash");
        publish(
            &fixture.paths,
            &mismatched_hash,
            COMMIT_ONE,
            "accepted",
            None,
        );
        rewrite_pointer_field(
            &fixture.paths,
            &mismatched_hash,
            "generation_hash",
            &"b".repeat(64),
        );

        let runtime = fixture.runtime();
        for project_id in [&mismatched_field, &mismatched_hash] {
            assert_eq!(
                runtime.status(project_id, None).unwrap().state(),
                AcceptedPublicationState::Corrupt,
                "{project_id}"
            );
            assert!(runtime.load_verified(project_id).is_err(), "{project_id}");
        }
    }

    #[test]
    fn oversized_pointer_is_refused_by_the_bounded_read() {
        let fixture = fixture();
        let project_id = project("p_bounded");
        publish(&fixture.paths, &project_id, COMMIT_ONE, "accepted", None);
        let mut oversized = fs::read(fixture.paths.pointer(&project_id)).unwrap();
        oversized.resize(MAX_ACCEPTED_PUBLICATION_POINTER_BYTES + 1, b' ');
        fs::write(fixture.paths.pointer(&project_id), &oversized).unwrap();

        let runtime = fixture.runtime();
        assert_eq!(
            runtime.status(&project_id, None).unwrap().state(),
            AcceptedPublicationState::Corrupt
        );
    }

    #[test]
    fn global_store_failure_is_the_only_bind_blocking_failure() {
        // A relative configured path can never be a safe store root.
        let error = AcceptedPublicationRuntime::open_global(std::path::Path::new("projects.json"))
            .unwrap_err();
        assert_eq!(
            error.code(),
            ERROR_ACCEPTED_PUBLICATION_GLOBAL_STORE_UNAVAILABLE
        );

        // An absent store root is not a failure: it is a catalog whose
        // projects have not published yet.
        let fixture = fixture();
        assert!(AcceptedPublicationRuntime::open_global(&fixture.projects_path).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn a_redirected_pointer_directory_blocks_the_global_open() {
        use std::os::unix::fs::symlink;

        let fixture = fixture();
        let elsewhere = fixture.paths.root().parent().unwrap().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        fs::create_dir_all(fixture.paths.root()).unwrap();
        symlink(&elsewhere, fixture.paths.pointers()).unwrap();

        let error = AcceptedPublicationRuntime::open_global(&fixture.projects_path).unwrap_err();
        assert_eq!(
            error.code(),
            ERROR_ACCEPTED_PUBLICATION_GLOBAL_STORE_UNAVAILABLE
        );
    }

    #[test]
    fn one_corrupt_project_does_not_block_the_startup_scan() {
        let fixture = fixture();
        let healthy = project("p_healthy");
        let corrupt = project("p_broken");
        let prior = project("p_priorscan");
        let missing = project("p_nopointer");
        publish(&fixture.paths, &healthy, COMMIT_ONE, "healthy", None);
        let broken = publish(&fixture.paths, &corrupt, COMMIT_ONE, "broken", None);
        fixtures::corrupt_generation(&fixture.paths, &corrupt, &broken.generation_id);
        let first = publish(&fixture.paths, &prior, COMMIT_ONE, "first", None);
        let second = publish(&fixture.paths, &prior, COMMIT_TWO, "second", Some(&first));
        fixtures::corrupt_generation(&fixture.paths, &prior, &second.generation_id);

        let runtime = fixture.runtime();
        let scan = runtime
            .startup_scan([
                (healthy.clone(), Some(scope())),
                (corrupt.clone(), Some(scope())),
                (prior.clone(), Some(scope())),
                (missing.clone(), Some(scope())),
            ])
            .unwrap();

        assert_eq!(scan.scanned(), 4);
        assert_eq!(scan.current(), 1);
        assert_eq!(scan.prior(), 1);
        assert_eq!(scan.corrupt(), 1);
        assert_eq!(scan.missing(), 1);
        assert_eq!(scan.published_unavailable(), 2);
        assert_eq!(scan.dropped_failures(), 0);
        let failed: Vec<&ProjectId> = scan.failures().map(|(id, _)| id).collect();
        assert_eq!(failed, vec![&corrupt, &prior]);

        // The scan retains identity, not payload, and the healthy project
        // still serves after its neighbours failed.
        assert_eq!(
            runtime
                .load_verified(&healthy)
                .unwrap()
                .knowledge_records()
                .values()
                .next()
                .unwrap()
                .content,
            "healthy"
        );
        assert_eq!(
            runtime.status(&corrupt, None).unwrap().state(),
            AcceptedPublicationState::Corrupt
        );
    }

    #[test]
    fn the_read_path_acquires_no_checkout_lease() {
        let fixture = fixture();
        let project_id = project("p_lease");
        publish(&fixture.paths, &project_id, COMMIT_ONE, "accepted", None);
        let observations = CheckoutAccessObservations::in_memory();
        let broker = CheckoutAccessBroker::new(StdArc::new(DenyCheckoutAccess), observations);
        let before = broker.health();

        let runtime = fixture.runtime();
        runtime.load_verified(&project_id).unwrap();
        runtime.status(&project_id, Some(&scope())).unwrap();
        runtime
            .startup_scan([(project_id.clone(), Some(scope()))])
            .unwrap();
        runtime
            .protected_generation_roots([project_id.clone()])
            .unwrap();

        let after = broker.health();
        assert_eq!(before, after);
        assert!(
            after
                .operations
                .iter()
                .all(|operation| operation.granted == 0 && operation.denied == 0)
        );
    }

    #[test]
    fn content_survives_rebind_and_only_advance_replaces_it() {
        let fixture = fixture();
        let project_id = project("p_stamps");
        let first = publish(&fixture.paths, &project_id, COMMIT_ONE, "first", None);
        let runtime = fixture.runtime();
        let before = runtime.load_verified(&project_id).unwrap();

        // A binding invalidation re-reads the pointer and must reuse the
        // decoded content it already verified. An intervening status poll is
        // identity-only and must not evict that decoded allocation.
        runtime.invalidate_binding(&project_id);
        assert_eq!(
            runtime.status(&project_id, Some(&scope())).unwrap().state(),
            AcceptedPublicationState::Current
        );
        let after_invalidate = runtime.load_verified(&project_id).unwrap();
        assert!(before.shares_content_with(&after_invalidate));

        // A real rebind changes the binding stamp and nothing else.
        let rebound_attachment = attachment("f001");
        let limits = AcceptedPublicationLimits::default();
        let guard = acquire_accepted_publication_lock(&fixture.paths).unwrap();
        rebind_pointer_attachment_locked(
            &fixture.paths,
            &guard,
            &project_id,
            &rebound_attachment,
            Some(&scope()),
            &limits,
        )
        .unwrap();
        drop(guard);
        runtime.invalidate_binding(&project_id);
        let rebound = runtime.load_verified(&project_id).unwrap();
        assert!(before.shares_content_with(&rebound));
        assert_eq!(before.content_stamp(), rebound.content_stamp());
        assert_eq!(
            rebound.binding_stamp().attachment_id(),
            Some(&rebound_attachment)
        );
        assert_ne!(
            before.binding_stamp().pointer_sha256(),
            rebound.binding_stamp().pointer_sha256()
        );

        // Advancing content changes the content stamp and drops the cache.
        publish(
            &fixture.paths,
            &project_id,
            COMMIT_TWO,
            "second",
            Some(&first),
        );
        runtime.invalidate_content(&project_id);
        let advanced = runtime.load_verified(&project_id).unwrap();
        assert!(!before.shares_content_with(&advanced));
        assert_ne!(before.content_stamp(), advanced.content_stamp());
        assert_eq!(advanced.content_stamp().accepted_commit(), COMMIT_TWO);
    }

    #[test]
    fn invalidation_fence_rejects_a_read_started_before_invalidation() {
        let fixture = fixture();
        let project_id = project("p_cachefence");
        publish(&fixture.paths, &project_id, COMMIT_ONE, "first", None);
        let runtime = fixture.runtime();

        let observed_revision = runtime.cache_revision.load(Ordering::Acquire);
        let guard = runtime.lock().unwrap();
        let outcome = runtime.read_project(&guard, &project_id);
        drop(guard);

        // Models a publish/bind/detach landing after the pointer read but
        // before its cache install.
        runtime.invalidate_content(&project_id);
        let status = runtime.install(&project_id, &outcome, true, observed_revision);
        assert_eq!(status.state(), AcceptedPublicationState::Current);
        assert!(runtime.cached_view(&project_id).is_none());
    }

    #[test]
    fn prior_fallback_is_rechecked_instead_of_cached_indefinitely() {
        let fixture = fixture();
        let project_id = project("p_priorretry");
        let first = publish(&fixture.paths, &project_id, COMMIT_ONE, "first", None);
        let second = publish(
            &fixture.paths,
            &project_id,
            COMMIT_TWO,
            "second",
            Some(&first),
        );
        let current_path = fixture.paths.generation(&project_id, &second.generation_id);
        let current_bytes = std::fs::read(&current_path).unwrap();
        fixtures::corrupt_generation(&fixture.paths, &project_id, &second.generation_id);

        let runtime = fixture.runtime();
        let prior = runtime.load_verified(&project_id).unwrap();
        assert_eq!(
            prior.binding_stamp().selection(),
            AcceptedPublicationSelection::Prior
        );
        assert!(runtime.cached_view(&project_id).is_none());

        // A transient current-arm read failure clears; the very next call
        // must recover Current without an explicit invalidation or restart.
        std::fs::write(current_path, current_bytes).unwrap();
        let current = runtime.load_verified(&project_id).unwrap();
        assert_eq!(
            current.binding_stamp().selection(),
            AcceptedPublicationSelection::Current
        );
        assert_eq!(current.content_stamp().accepted_commit(), COMMIT_TWO);
    }

    #[test]
    fn protected_roots_cover_current_prior_pinned_and_refuse_to_guess() {
        let fixture = fixture();
        let advanced = project("p_roots");
        let missing = project("p_norootpointer");
        let broken = project("p_brokenpointer");
        let first = publish(&fixture.paths, &advanced, COMMIT_ONE, "first", None);
        let second = publish(
            &fixture.paths,
            &advanced,
            COMMIT_TWO,
            "second",
            Some(&first),
        );
        publish(&fixture.paths, &broken, COMMIT_ONE, "broken", None);
        rewrite_pointer_field(&fixture.paths, &broken, "accepted_generation", "not-a-hash");

        let runtime = fixture.runtime();
        let roots = runtime
            .protected_generation_roots([advanced.clone(), missing.clone(), broken.clone()])
            .unwrap();

        assert!(roots.protects(&advanced, second.generation_id.as_str()));
        assert!(roots.protects(&advanced, first.generation_id.as_str()));
        assert!(roots.is_resolved(&advanced));
        // No pointer means nothing is referenced, which is still a proved
        // answer; an undecodable pointer is not.
        assert!(roots.is_resolved(&missing));
        assert!(roots.project_roots(&missing).unwrap().is_empty());
        assert!(!roots.is_resolved(&broken));
        assert_eq!(roots.unresolved().count(), 1);

        // A pinned read protects the generation it is holding even when the
        // pointer has moved past it.
        let pinned = project("p_pinned");
        let pinned_first = publish(&fixture.paths, &pinned, COMMIT_ONE, "first", None);
        let runtime = fixture.runtime();
        runtime.load_verified(&pinned).unwrap();
        let pinned_second = publish(
            &fixture.paths,
            &pinned,
            COMMIT_TWO,
            "second",
            Some(&pinned_first),
        );
        let roots = runtime
            .protected_generation_roots([pinned.clone()])
            .unwrap();
        assert!(roots.protects(&pinned, pinned_first.generation_id.as_str()));
        assert!(roots.protects(&pinned, pinned_second.generation_id.as_str()));
    }

    // ── Publish transaction (plan §13.2, §13.7) ──────────────────────

    fn sources(content: &str) -> PublishSources {
        PublishSources {
            knowledge: vec![PublishSourceFile {
                repository_relative_filename: ".bbox/knowledge/knowledge-a.json".into(),
                source_bytes: serde_json::to_vec(&fixtures::knowledge_entry(
                    "knowledge-a",
                    content,
                ))
                .unwrap(),
            }],
            gaps: vec![PublishSourceFile {
                repository_relative_filename: ".bbox/gaps/gap-1234abcd.json".into(),
                source_bytes: serde_json::to_vec(&fixtures::gap_note("gap-1234abcd")).unwrap(),
            }],
        }
    }

    fn establish_request(project_id: &ProjectId, commit: &str) -> PublishRequest {
        PublishRequest {
            mode: PublisherPublishMode::Establish,
            project_id: project_id.clone(),
            source: AcceptedPublicationSourceBinding::Attachment {
                attachment_id: attachment("a1"),
            },
            scope: scope(),
            full_ref: "refs/heads/main".into(),
            accepted_commit: commit.into(),
            dry_run: false,
        }
    }

    fn advance_request(
        project_id: &ProjectId,
        commit: &str,
        tokens: (String, String),
    ) -> PublishRequest {
        PublishRequest {
            mode: PublisherPublishMode::Advance {
                expected_generation_id: tokens.0,
                expected_pointer_sha256: tokens.1,
            },
            project_id: project_id.clone(),
            source: AcceptedPublicationSourceBinding::Attachment {
                attachment_id: attachment("a1"),
            },
            scope: scope(),
            full_ref: "refs/heads/main".into(),
            accepted_commit: commit.into(),
            dry_run: false,
        }
    }

    fn producer_request(project_id: &ProjectId, commit: &str) -> PublishRequest {
        PublishRequest {
            mode: PublisherPublishMode::Establish,
            project_id: project_id.clone(),
            source: AcceptedPublicationSourceBinding::Producer {
                producer_id: "producer-a".into(),
                source_generation_id: format!("kps_{}", "1".repeat(64)),
                source_generation_sha256: "2".repeat(64),
            },
            scope: scope(),
            full_ref: "refs/heads/main".into(),
            accepted_commit: commit.into(),
            dry_run: false,
        }
    }

    fn run_publish(
        runtime: &AcceptedPublicationRuntime,
        request: PublishRequest,
        content: &str,
    ) -> Result<PublishReceipt, PublishError> {
        let prepared = runtime.prepare_publish(request, sources(content))?;
        runtime.commit_publish(prepared, &mut || Ok(()))
    }

    #[test]
    fn establish_creates_the_first_pointer_and_advance_retains_it_as_prior() {
        let fixture = fixture();
        let runtime = fixture.runtime();
        let project_id = project("p_publish");

        let established = run_publish(
            &runtime,
            establish_request(&project_id, COMMIT_ONE),
            "first",
        )
        .unwrap();
        assert!(!established.is_dry_run());
        assert_eq!(established.previous_pointer_sha256(), None);
        let verified = runtime.load_verified(&project_id).unwrap();
        assert_eq!(
            verified.binding_stamp().pointer_sha256(),
            established.pointer_sha256()
        );
        assert_eq!(
            verified.content_stamp().generation_id(),
            established.generation_id()
        );
        assert_eq!(verified.content_stamp().accepted_commit(), COMMIT_ONE);

        let tokens = runtime.advance_tokens(&project_id).unwrap().unwrap();
        let advanced = run_publish(
            &runtime,
            advance_request(&project_id, COMMIT_TWO, tokens),
            "second",
        )
        .unwrap();
        assert_eq!(
            advanced.previous_pointer_sha256(),
            Some(established.pointer_sha256())
        );
        let verified = runtime.load_verified(&project_id).unwrap();
        assert_eq!(verified.content_stamp().accepted_commit(), COMMIT_TWO);
        assert_eq!(
            verified
                .knowledge_records()
                .values()
                .next()
                .unwrap()
                .content,
            "second"
        );

        // The replaced generation stays referenced as the prior arm, so a
        // collector must not remove it.
        let roots = runtime
            .protected_generation_roots([project_id.clone()])
            .unwrap();
        assert!(roots.protects(&project_id, established.generation_id()));
        assert!(roots.protects(&project_id, advanced.generation_id()));
    }

    #[test]
    fn producer_binding_survives_attachment_advance_prior_fallback_and_roots() {
        let fixture = fixture();
        let runtime = fixture.runtime();
        let project_id = project("p_producer_binding");
        let source_generation_id = format!("kps_{}", "1".repeat(64));

        let producer = run_publish(
            &runtime,
            producer_request(&project_id, COMMIT_ONE),
            "producer",
        )
        .unwrap();
        let verified = runtime.load_verified(&project_id).unwrap();
        assert_eq!(verified.binding_stamp().attachment_id(), None);
        assert_eq!(verified.binding_stamp().source().kind(), "producer");
        assert_eq!(
            verified.binding_stamp().source().producer_id(),
            Some("producer-a")
        );
        assert_eq!(
            verified.binding_stamp().source().source_generation_id(),
            Some(source_generation_id.as_str())
        );
        assert_eq!(
            runtime
                .protected_source_generation_roots([project_id.clone()])
                .unwrap(),
            BTreeSet::from([source_generation_id.clone()])
        );

        let tokens = runtime.advance_tokens(&project_id).unwrap().unwrap();
        let attachment_receipt = run_publish(
            &runtime,
            advance_request(&project_id, COMMIT_TWO, tokens),
            "attachment",
        )
        .unwrap();
        let current = runtime.load_verified(&project_id).unwrap();
        assert_eq!(current.binding_stamp().source().kind(), "attachment");
        assert_eq!(
            current.binding_stamp().attachment_id(),
            Some(&attachment("a1"))
        );
        assert_eq!(
            runtime
                .protected_source_generation_roots([project_id.clone()])
                .unwrap(),
            BTreeSet::from([source_generation_id.clone()])
        );

        fixtures::corrupt_generation(
            &fixture.paths,
            &project_id,
            &AcceptedPublicationGenerationId::parse(attachment_receipt.generation_id().to_string())
                .unwrap(),
        );
        let fallback = fixture.runtime().load_verified(&project_id).unwrap();
        assert_eq!(
            fallback.binding_stamp().selection(),
            AcceptedPublicationSelection::Prior
        );
        assert_eq!(
            fallback.content_stamp().generation_id(),
            producer.generation_id()
        );
        assert_eq!(fallback.binding_stamp().source().kind(), "producer");
        assert_eq!(
            fallback.binding_stamp().source().source_generation_id(),
            Some(source_generation_id.as_str())
        );
    }

    #[test]
    fn establish_refuses_a_project_that_already_publishes() {
        let fixture = fixture();
        let runtime = fixture.runtime();
        let project_id = project("p_second_establish");
        run_publish(
            &runtime,
            establish_request(&project_id, COMMIT_ONE),
            "first",
        )
        .unwrap();

        let error = run_publish(
            &runtime,
            establish_request(&project_id, COMMIT_TWO),
            "second",
        )
        .unwrap_err();
        assert_eq!(error.code(), ERROR_ACCEPTED_PUBLICATION_POINTER_CONFLICT);
        // The refusal changed nothing.
        assert_eq!(
            runtime
                .load_verified(&project_id)
                .unwrap()
                .content_stamp()
                .accepted_commit(),
            COMMIT_ONE
        );
    }

    #[test]
    fn two_concurrent_establishes_leave_exactly_one_winner() {
        let fixture = fixture();
        let project_id = project("p_race_establish");
        let barrier = StdArc::new(std::sync::Barrier::new(2));
        let outcomes: Vec<_> = std::thread::scope(|scope| {
            let handles: Vec<_> = ["first", "second"]
                .into_iter()
                .map(|content| {
                    let barrier = StdArc::clone(&barrier);
                    let fixture = &fixture;
                    let project_id = project_id.clone();
                    scope.spawn(move || {
                        // Both threads prepare fully, then race the swap.
                        let runtime = fixture.runtime();
                        let prepared = runtime
                            .prepare_publish(
                                establish_request(&project_id, COMMIT_ONE),
                                sources(content),
                            )
                            .unwrap();
                        barrier.wait();
                        runtime.commit_publish(prepared, &mut || Ok(()))
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect()
        });

        let winners = outcomes.iter().filter(|outcome| outcome.is_ok()).count();
        assert_eq!(winners, 1, "{outcomes:?}");
        let loser = outcomes.iter().find_map(|outcome| outcome.as_ref().err());
        assert_eq!(
            loser.unwrap().code(),
            ERROR_ACCEPTED_PUBLICATION_POINTER_CONFLICT
        );
        // Whichever won, the installed pointer verifies as its own current
        // generation and both lanes came from one preparation.
        let runtime = fixture.runtime();
        let verified = runtime.load_verified(&project_id).unwrap();
        assert_eq!(
            verified.binding_stamp().selection(),
            AcceptedPublicationSelection::Current
        );
        assert_eq!(verified.counts().knowledge_entries, 1);
        assert_eq!(verified.counts().gap_entries, 1);
    }

    #[test]
    fn two_concurrent_advances_at_one_epoch_leave_exactly_one_winner() {
        let fixture = fixture();
        let project_id = project("p_race_advance");
        let setup = fixture.runtime();
        run_publish(&setup, establish_request(&project_id, COMMIT_ONE), "first").unwrap();
        let tokens = setup.advance_tokens(&project_id).unwrap().unwrap();

        let barrier = StdArc::new(std::sync::Barrier::new(2));
        let outcomes: Vec<_> = std::thread::scope(|scope| {
            let handles: Vec<_> = [(COMMIT_TWO, "second"), (COMMIT_THREE, "third")]
                .into_iter()
                .map(|(commit, content)| {
                    let barrier = StdArc::clone(&barrier);
                    let fixture = &fixture;
                    let project_id = project_id.clone();
                    let tokens = tokens.clone();
                    scope.spawn(move || {
                        // Both hold the SAME compare-and-swap tokens, which
                        // is the interleaving a catalog epoch cannot serialize.
                        let runtime = fixture.runtime();
                        let prepared = runtime
                            .prepare_publish(
                                advance_request(&project_id, commit, tokens),
                                sources(content),
                            )
                            .unwrap();
                        barrier.wait();
                        runtime.commit_publish(prepared, &mut || Ok(()))
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect()
        });

        assert_eq!(
            outcomes.iter().filter(|outcome| outcome.is_ok()).count(),
            1,
            "{outcomes:?}"
        );
        assert_eq!(
            outcomes
                .iter()
                .find_map(|outcome| outcome.as_ref().err())
                .unwrap()
                .code(),
            ERROR_ACCEPTED_PUBLICATION_POINTER_CONFLICT
        );
        let runtime = fixture.runtime();
        let verified = runtime.load_verified(&project_id).unwrap();
        assert_eq!(
            verified.binding_stamp().selection(),
            AcceptedPublicationSelection::Current
        );
    }

    #[test]
    fn advance_refuses_stale_generation_and_pointer_tokens() {
        let fixture = fixture();
        let runtime = fixture.runtime();
        let project_id = project("p_tokens");
        run_publish(
            &runtime,
            establish_request(&project_id, COMMIT_ONE),
            "first",
        )
        .unwrap();
        let (generation_id, pointer_sha) = runtime.advance_tokens(&project_id).unwrap().unwrap();

        let wrong_generation = run_publish(
            &runtime,
            advance_request(
                &project_id,
                COMMIT_TWO,
                ("c".repeat(64), pointer_sha.clone()),
            ),
            "second",
        )
        .unwrap_err();
        assert_eq!(
            wrong_generation.code(),
            ERROR_ACCEPTED_PUBLICATION_POINTER_CONFLICT
        );

        let wrong_pointer = run_publish(
            &runtime,
            advance_request(&project_id, COMMIT_TWO, (generation_id, "d".repeat(64))),
            "second",
        )
        .unwrap_err();
        assert_eq!(
            wrong_pointer.code(),
            ERROR_ACCEPTED_PUBLICATION_POINTER_CONFLICT
        );
        assert_eq!(
            runtime
                .load_verified(&project_id)
                .unwrap()
                .content_stamp()
                .accepted_commit(),
            COMMIT_ONE
        );
    }

    #[test]
    fn advance_refuses_while_reads_are_served_from_prior() {
        let fixture = fixture();
        let runtime = fixture.runtime();
        let project_id = project("p_prior_mutation");
        run_publish(
            &runtime,
            establish_request(&project_id, COMMIT_ONE),
            "first",
        )
        .unwrap();
        let tokens = runtime.advance_tokens(&project_id).unwrap().unwrap();
        let second = run_publish(
            &runtime,
            advance_request(&project_id, COMMIT_TWO, tokens),
            "second",
        )
        .unwrap();
        // Damage the current arm: reads fall back to prior, and mutation
        // must refuse rather than advancing from a damaged pointer.
        fixtures::corrupt_generation(
            &fixture.paths,
            &project_id,
            &AcceptedPublicationGenerationId::parse(second.generation_id().to_string()).unwrap(),
        );
        let runtime = fixture.runtime();
        assert_eq!(
            runtime
                .load_verified(&project_id)
                .unwrap()
                .binding_stamp()
                .selection(),
            AcceptedPublicationSelection::Prior
        );

        let tokens = runtime.advance_tokens(&project_id).unwrap().unwrap();
        let error = run_publish(
            &runtime,
            advance_request(&project_id, COMMIT_THREE, tokens),
            "third",
        )
        .unwrap_err();
        assert_eq!(error.code(), ERROR_ACCEPTED_PUBLICATION_REPAIR_REQUIRED);
    }

    #[test]
    fn a_dry_run_writes_nothing_and_cannot_be_committed() {
        let fixture = fixture();
        let runtime = fixture.runtime();
        let project_id = project("p_dry");
        let mut request = establish_request(&project_id, COMMIT_ONE);
        request.dry_run = true;

        let prepared = runtime.prepare_publish(request, sources("first")).unwrap();
        assert!(prepared.is_dry_run());
        assert!(!prepared.generation_installed());
        assert_eq!(prepared.counts().knowledge_entries, 1);
        // Nothing durable exists: no pointer, no generation directory.
        assert!(!fixture.paths.pointer(&project_id).exists());
        assert!(
            !fixture
                .paths
                .generations()
                .join(project_id.as_str())
                .exists()
        );

        let error = runtime
            .commit_publish(prepared, &mut || Ok(()))
            .unwrap_err();
        assert_eq!(error.code(), ERROR_ACCEPTED_PUBLICATION_DRY_RUN);
    }

    #[test]
    fn a_freshness_refusal_keeps_its_own_code_and_installs_no_pointer() {
        let fixture = fixture();
        let runtime = fixture.runtime();
        let project_id = project("p_freshness");
        let prepared = runtime
            .prepare_publish(establish_request(&project_id, COMMIT_ONE), sources("first"))
            .unwrap();
        // The generation is durable before the lock; only the pointer is
        // gated on freshness.
        assert!(prepared.generation_installed());

        let error = runtime
            .commit_publish(prepared, &mut || {
                Err(PublishError::refusal(
                    "error.project_catalog_stale_epoch",
                    "the catalog moved",
                ))
            })
            .unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_stale_epoch");
        assert!(!fixture.paths.pointer(&project_id).exists());
    }

    #[test]
    fn one_lane_validation_failure_installs_nothing() {
        let fixture = fixture();
        let runtime = fixture.runtime();
        let project_id = project("p_badlane");
        let mut broken = sources("first");
        broken.gaps[0].source_bytes = b"{not json".to_vec();

        let error = runtime
            .prepare_publish(establish_request(&project_id, COMMIT_ONE), broken)
            .unwrap_err();
        assert_eq!(
            error.code(),
            "error.accepted_publication_invalid_generation"
        );
        assert!(!fixture.paths.pointer(&project_id).exists());
    }

    #[test]
    fn preparing_the_same_generation_twice_is_idempotent() {
        let fixture = fixture();
        let runtime = fixture.runtime();
        let project_id = project("p_idempotent");
        let first = runtime
            .prepare_publish(establish_request(&project_id, COMMIT_ONE), sources("first"))
            .unwrap();
        assert!(first.generation_installed());
        let second = runtime
            .prepare_publish(establish_request(&project_id, COMMIT_ONE), sources("first"))
            .unwrap();
        // Same content, same content-derived id, already on disk.
        assert_eq!(first.generation_id(), second.generation_id());
        assert!(!second.generation_installed());
        runtime.commit_publish(second, &mut || Ok(())).unwrap();
        assert!(runtime.load_verified(&project_id).is_ok());
    }

    #[test]
    fn a_foreign_generation_under_one_content_id_fails_closed() {
        let fixture = fixture();
        let runtime = fixture.runtime();
        let project_id = project("p_foreign");
        let prepared = runtime
            .prepare_publish(establish_request(&project_id, COMMIT_ONE), sources("first"))
            .unwrap();
        // Replace the installed generation with different bytes under the
        // same content id. Content addressing makes this unreachable
        // through the public path, so the guard is the last line of
        // defence rather than an expected state.
        let generation_path = fixture.paths.generation(
            &project_id,
            &AcceptedPublicationGenerationId::parse(prepared.generation_id().to_string()).unwrap(),
        );
        std::fs::write(&generation_path, b"{\"version\":1}").unwrap();

        let error = runtime
            .prepare_publish(establish_request(&project_id, COMMIT_ONE), sources("first"))
            .unwrap_err();
        assert_eq!(
            error.code(),
            "error.accepted_publication_invalid_generation"
        );
    }

    #[derive(Debug)]
    struct FailAt {
        point: AcceptedPublicationFaultPoint,
        corrupt_generation: std::sync::Mutex<Option<(std::path::PathBuf, String)>>,
    }

    impl AcceptedPublicationFaultInjector for FailAt {
        fn checkpoint(
            &self,
            point: AcceptedPublicationFaultPoint,
        ) -> Result<(), AcceptedPublicationStoreError> {
            if point != self.point {
                return Ok(());
            }
            if let Some((path, _)) = self.corrupt_generation.lock().unwrap().take() {
                // Simulate the generation becoming unreadable between the
                // swap and the read-back.
                std::fs::write(path, b"corrupt").unwrap();
                return Ok(());
            }
            Err(AcceptedPublicationStoreError::new(
                "error.accepted_publication_io",
                "injected fault",
            ))
        }
    }

    fn runtime_failing_at(
        fixture: &Fixture,
        point: AcceptedPublicationFaultPoint,
    ) -> AcceptedPublicationRuntime {
        let mut runtime = fixture.runtime();
        runtime.install_fault_injector_for_test(StdArc::new(FailAt {
            point,
            corrupt_generation: std::sync::Mutex::new(None),
        }));
        runtime
    }

    #[test]
    fn every_publish_failpoint_leaves_a_complete_old_or_complete_new_pointer() {
        use AcceptedPublicationFaultPoint::*;

        for point in [
            BeforeGenerationInstall,
            AfterGenerationInstall,
            BeforePointerTokenCheck,
            BeforeFreshnessRecheck,
            BeforePointerSwap,
            AfterPointerSwap,
        ] {
            let fixture = fixture();
            let project_id = project("p_faults");
            // Establish a first generation with a clean runtime so every
            // failpoint is exercised against a real advance.
            let clean = fixture.runtime();
            let first =
                run_publish(&clean, establish_request(&project_id, COMMIT_ONE), "first").unwrap();
            let tokens = clean.advance_tokens(&project_id).unwrap().unwrap();

            let runtime = runtime_failing_at(&fixture, point);
            let attempt = runtime
                .prepare_publish(
                    advance_request(&project_id, COMMIT_TWO, tokens),
                    sources("second"),
                )
                .and_then(|prepared| runtime.commit_publish(prepared, &mut || Ok(())));

            // Whatever failed, a fresh reader observes ONE complete
            // publication, never a mixed-lane or half-written state.
            let reader = fixture.runtime();
            let verified = reader.load_verified(&project_id).unwrap();
            assert_eq!(
                verified.binding_stamp().selection(),
                AcceptedPublicationSelection::Current,
                "{point:?}"
            );
            assert_eq!(verified.counts().knowledge_entries, 1, "{point:?}");
            assert_eq!(verified.counts().gap_entries, 1, "{point:?}");
            let content = verified
                .knowledge_records()
                .values()
                .next()
                .unwrap()
                .content
                .clone();
            match point {
                // The swap already happened, so the new publication is
                // installed even though the transaction reported failure.
                AfterPointerSwap => {
                    assert_eq!(content, "second", "{point:?}");
                    assert_eq!(verified.content_stamp().accepted_commit(), COMMIT_TWO);
                }
                _ => {
                    assert_eq!(content, "first", "{point:?}");
                    assert_eq!(
                        verified.content_stamp().generation_id(),
                        first.generation_id(),
                        "{point:?}"
                    );
                    assert!(attempt.is_err(), "{point:?}");
                }
            }
        }
    }

    #[test]
    fn a_read_back_that_no_longer_verifies_is_reported_after_a_durable_swap() {
        let fixture = fixture();
        let project_id = project("p_readback");
        let clean = fixture.runtime();
        run_publish(&clean, establish_request(&project_id, COMMIT_ONE), "first").unwrap();
        let tokens = clean.advance_tokens(&project_id).unwrap().unwrap();

        let mut runtime = fixture.runtime();
        let prepared = runtime
            .prepare_publish(
                advance_request(&project_id, COMMIT_TWO, tokens),
                sources("second"),
            )
            .unwrap();
        let generation_path = fixture.paths.generation(
            &project_id,
            &AcceptedPublicationGenerationId::parse(prepared.generation_id().to_string()).unwrap(),
        );
        runtime.install_fault_injector_for_test(StdArc::new(FailAt {
            point: AcceptedPublicationFaultPoint::AfterPointerSwap,
            corrupt_generation: std::sync::Mutex::new(Some((
                generation_path,
                String::from("corrupt"),
            ))),
        }));

        let error = runtime
            .commit_publish(prepared, &mut || Ok(()))
            .unwrap_err();
        // Read-back refuses because the installed pointer no longer reads
        // back as its OWN current generation: the strict verifier finds the
        // damaged current arm and falls to prior, which is not what this
        // commit installed.
        assert_eq!(error.code(), "error.accepted_publication_invalid_pointer");
        // The pointer swap was durable, so the prior arm is what keeps the
        // project readable. This is a repair state, not a loss.
        let reader = fixture.runtime();
        assert_eq!(
            reader
                .load_verified(&project_id)
                .unwrap()
                .binding_stamp()
                .selection(),
            AcceptedPublicationSelection::Prior
        );
    }

    #[test]
    fn a_live_runtime_reverifies_after_a_failure_that_reached_the_swap() {
        let fixture = fixture();
        let project_id = project("p_stale_cache");
        let runtime = fixture.runtime();
        run_publish(
            &runtime,
            establish_request(&project_id, COMMIT_ONE),
            "first",
        )
        .unwrap();
        // Prewarm THIS runtime: the cached content is the first generation.
        let before = runtime.load_verified(&project_id).unwrap();
        assert_eq!(before.content_stamp().accepted_commit(), COMMIT_ONE);

        let tokens = runtime.advance_tokens(&project_id).unwrap().unwrap();
        let mut runtime = runtime;
        let prepared = runtime
            .prepare_publish(
                advance_request(&project_id, COMMIT_TWO, tokens),
                sources("second"),
            )
            .unwrap();
        let generation_path = fixture.paths.generation(
            &project_id,
            &AcceptedPublicationGenerationId::parse(prepared.generation_id().to_string()).unwrap(),
        );
        // Damage the new generation between the durable swap and read-back.
        runtime.install_fault_injector_for_test(StdArc::new(FailAt {
            point: AcceptedPublicationFaultPoint::AfterPointerSwap,
            corrupt_generation: std::sync::Mutex::new(Some((
                generation_path,
                String::from("corrupt"),
            ))),
        }));
        let error = runtime
            .commit_publish(prepared, &mut || Ok(()))
            .unwrap_err();
        assert!(
            error.may_have_swapped(),
            "a read-back failure happens after a durable swap"
        );

        // The same runtime must not keep serving the generation the
        // pointer no longer names. It reverifies and finds the installed
        // pointer serving its prior arm.
        let after = runtime.load_verified(&project_id).unwrap();
        assert_eq!(
            after.binding_stamp().selection(),
            AcceptedPublicationSelection::Prior
        );
        assert_ne!(
            before.binding_stamp().pointer_sha256(),
            after.binding_stamp().pointer_sha256(),
            "the installed pointer is the new one even though the commit reported failure"
        );
    }

    #[test]
    fn an_installed_but_uncommitted_generation_is_a_protected_root() {
        let fixture = fixture();
        let runtime = fixture.runtime();
        let project_id = project("p_inflight");
        run_publish(
            &runtime,
            establish_request(&project_id, COMMIT_ONE),
            "first",
        )
        .unwrap();
        let tokens = runtime.advance_tokens(&project_id).unwrap().unwrap();

        let prepared = runtime
            .prepare_publish(
                advance_request(&project_id, COMMIT_TWO, tokens),
                sources("second"),
            )
            .unwrap();
        // Durably installed, named by no pointer yet: exactly the window a
        // collector reading only pointer arms would free.
        let in_flight = prepared.generation_id().to_string();
        let roots = runtime
            .protected_generation_roots([project_id.clone()])
            .unwrap();
        assert!(
            roots.protects(&project_id, &in_flight),
            "an installed preparation must survive collection"
        );

        // Abandoning the preparation releases the root: nothing references
        // those bytes any more.
        drop(prepared);
        let roots = runtime
            .protected_generation_roots([project_id.clone()])
            .unwrap();
        assert!(!roots.protects(&project_id, &in_flight));
    }

    #[test]
    fn a_committed_generation_stays_protected_by_its_pointer_after_the_handle_drops() {
        let fixture = fixture();
        let runtime = fixture.runtime();
        let project_id = project("p_inflight_commit");
        let receipt = run_publish(
            &runtime,
            establish_request(&project_id, COMMIT_ONE),
            "first",
        )
        .unwrap();
        // The preparation handle is long gone; the pointer arm is what
        // protects the generation now.
        let roots = runtime
            .protected_generation_roots([project_id.clone()])
            .unwrap();
        assert!(roots.protects(&project_id, receipt.generation_id()));
    }

    #[test]
    fn a_generation_id_that_no_pointer_names_is_not_protected() {
        let fixture = fixture();
        let project_id = project("p_unreferenced");
        publish(&fixture.paths, &project_id, COMMIT_ONE, "accepted", None);
        let runtime = fixture.runtime();
        let roots = runtime
            .protected_generation_roots([project_id.clone()])
            .unwrap();
        let orphan = AcceptedPublicationGenerationId::parse("c".repeat(64)).unwrap();
        assert!(!roots.protects(&project_id, orphan.as_str()));
    }
}
