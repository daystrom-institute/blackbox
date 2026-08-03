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
//! This facade reads. Establish, bind, and advance are the later publisher
//! milestone; nothing here mutates a pointer or a generation.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::project_catalog::{AttachmentId, ProjectId};
use parking_lot::RwLock;

use crate::accepted_publication_store::{
    AcceptedPublicationGenerationV1, AcceptedPublicationLimits, AcceptedPublicationLockGuard,
    AcceptedPublicationStoreError, AcceptedPublicationStorePaths,
    VerifiedAcceptedPublicationSelectionV1, acquire_accepted_publication_lock,
    pointer_generation_roots_locked, probe_global_store_locked,
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
pub struct AcceptedPublicationBindingStamp {
    project_id: ProjectId,
    attachment_id: AttachmentId,
    pointer_sha256: String,
    selection: AcceptedPublicationSelection,
    accepted_scope: PublishedScope,
}

impl AcceptedPublicationBindingStamp {
    pub fn project_id(&self) -> &ProjectId {
        &self.project_id
    }

    pub fn attachment_id(&self) -> &AttachmentId {
        &self.attachment_id
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
            let outcome = self.read_project(&guard, &project_id);
            let status = self.install(&project_id, &outcome, false);
            scan.record(&status.with_scope(catalog_scope.as_ref()));
        }
        Ok(scan)
    }

    /// Drop the cached binding for one project, keeping its verified
    /// content. This is the rebind invalidation: the pointer's attachment
    /// changed, the accepted bytes did not.
    pub fn invalidate_binding(&self, project_id: &ProjectId) {
        if let Some(entry) = self.cache.write().get_mut(project_id) {
            entry.binding = None;
            entry.status = None;
        }
    }

    /// Drop cached content and binding for one project. This is the
    /// advance invalidation: new accepted content replaces the old.
    pub fn invalidate_content(&self, project_id: &ProjectId) {
        self.cache.write().remove(project_id);
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
            protected.roots.insert(project_id, roots);
        }
        Ok(protected)
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
        let guard = self.lock()?;
        let outcome = self.read_project(&guard, project_id);
        drop(guard);
        let outcome = self.reuse_cached_content(project_id, outcome);
        let status = self.install(project_id, &outcome, retain_content);
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
                let binding_stamp = AcceptedPublicationBindingStamp {
                    project_id: read.pointer.project_id.clone(),
                    attachment_id: read.pointer.attachment_id.clone(),
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
    ) -> AcceptedPublicationStatus {
        let status = status_from(project_id, outcome);
        let mut cache = self.cache.write();
        let entry = cache.entry(project_id.clone()).or_default();
        match outcome {
            ProjectReadOutcome::Verified { content, binding } => {
                if retain_content {
                    entry.content = Some(content.clone());
                    entry.binding = Some(binding.clone());
                } else {
                    // A bounded scan keeps identity, not payload.
                    entry.content = None;
                    entry.binding = None;
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
        AcceptedPublicationGenerationId, AcceptedPublicationLimits,
        MAX_ACCEPTED_PUBLICATION_POINTER_BYTES, PreparedAcceptedPublicationV1,
        acquire_accepted_publication_lock, rebind_pointer_attachment_locked,
    };
    use crate::checkout_access::{
        CheckoutAccessBroker, CheckoutAccessObservations, DenyCheckoutAccess,
    };

    use super::*;

    const COMMIT_ONE: &str = "1111111111111111111111111111111111111111";
    const COMMIT_TWO: &str = "2222222222222222222222222222222222222222";

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
        assert_eq!(binding.attachment_id(), &attachment("a1"));
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
        // decoded content it already verified.
        runtime.invalidate_binding(&project_id);
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
        assert_eq!(rebound.binding_stamp().attachment_id(), &rebound_attachment);
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
