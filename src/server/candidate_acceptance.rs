//! Acceptance of Ready publication candidates
//! (`design/daemon-runtime/publisher-auto-advance.md`).
//!
//! Merging to a project's configured ref is the only gate on what the
//! daemon serves. Every Ready candidate from the project's bound producer,
//! at its accepted scope, on its configured ref, is accepted as it
//! finalizes, provided it passes the validation every publish runs:
//! producer transport grant, catalog scope, byte integrity, configuration
//! parse, and normalization. No grant, policy flag, or operator act stands
//! between a valid candidate and the pointer.
//!
//! This module holds:
//!
//! 1. [`publish_from_ready_candidate`], the single candidate-acceptance
//!    path. The finalize trigger and `bbox_project_publisher_advance` both
//!    call it, so a candidate validates identically whichever caller
//!    accepts it.
//! 2. The finalize-triggered acceptance. With no pointer, the first valid
//!    candidate from the project's owning producer establishes one and its
//!    full branch ref becomes the configured ref. With a pointer, every
//!    candidate on the bound producer, scope, and ref advances it.
//!    Candidates that would change the producer, scope, or ref are refused
//!    and wait for an operator rebind or scope move.
//! 3. [`CandidateAcceptanceLedger`], the bounded per-project record of the
//!    last attempt, which makes a refusal observable in
//!    `bbox_project_publisher_status` instead of only in logs.

use std::collections::BTreeMap;
use std::sync::Arc;

use bbox_corpus_core::project_catalog::{AttachmentStatus, ProjectId, ProjectScope};
use bbox_indexing::accepted_publication_runtime::{
    AcceptedPublicationRuntime, PublishError, PublishReceipt, PublishSourceFile, PublishSources,
    PublisherPublishMode,
};
use bbox_indexing::project_catalog_admin;
use bbox_indexing::project_catalog_store::ProjectCatalogStore;
use bbox_knowledge_source_store::KnowledgeSourceStore;

use super::producer_auth::ProducerAuthRuntime;

/// Longest audit reason the catalog accepts, mirrored here so a generated
/// reason is bounded at the point it is built.
const MAX_AUDIT_REASON_BYTES: usize = 1024;

/// The stable refusal every candidate-selection failure carries.
const CANDIDATE_REQUIRED: &str = "error.accepted_publication_candidate_required";

/// Most recent attempts retained per daemon lifetime. The ledger is an
/// observability surface, not a queue: it must never be the reason the
/// daemon grows without bound.
const MAX_LEDGER_PROJECTS: usize = 512;

/// Attempted candidates remembered per project. "At most one attempt per
/// uploaded candidate" needs memory of which candidates were attempted;
/// bounding it is what keeps a chatty producer from turning that memory
/// into a leak. Eviction is oldest-first within a project, and an evicted
/// candidate that the pointer already names is refused as already
/// accepted.
const MAX_ATTEMPTED_PER_PROJECT: usize = 64;

/// The single candidate-acceptance path.
///
/// It resolves the Ready candidate, re-proves the producer transport grant,
/// parses the configuration lane, builds the publish probe, and hands the
/// whole thing to the admin entry point that normalizes and swaps. The
/// finalize trigger and the operator tool differ only in the mode they
/// pass.
pub(crate) fn publish_from_ready_candidate(
    store: &ProjectCatalogStore,
    runtime: &AcceptedPublicationRuntime,
    producer_auth: &ProducerAuthRuntime,
    knowledge_sources: &KnowledgeSourceStore,
    project_id: &ProjectId,
    source_generation_id: &str,
    mode: PublisherPublishMode,
    expected_catalog_epoch: u64,
    dry_run: bool,
) -> Result<PublishReceipt, PublishError> {
    // `PublishError` and not `anyhow` on purpose: it carries the refusing
    // layer's own code AND `may_have_swapped`, which the operator tool uses
    // to decide whether to reconverge after a failure. Flattening it here
    // would silently drop that signal from one of the two callers.
    project_catalog_admin::preflight_candidate_publish_authority(
        store,
        expected_catalog_epoch,
        project_id,
    )?;
    let pinned = Arc::new(
        knowledge_sources
            .pin_ready_publication_candidate(source_generation_id)
            .map_err(|error| PublishError::refusal(CANDIDATE_REQUIRED, format!("{error}")))?,
    );
    let candidate = pinned.candidate();
    if candidate.project_id != project_id.as_str() {
        return Err(PublishError::refusal(
            CANDIDATE_REQUIRED,
            "candidate belongs to another project",
        ));
    }
    let granted_project = producer_auth
        .project_transport_grant_for_id(&candidate.producer_id, &candidate.descriptor.scope)
        .map_err(|error| {
            PublishError::refusal(
                CANDIDATE_REQUIRED,
                format!(
                    "current producer grant rejected the candidate ({})",
                    error.code()
                ),
            )
        })?;
    if granted_project != project_id {
        return Err(PublishError::refusal(
            CANDIDATE_REQUIRED,
            "current producer grant resolves the candidate to another project",
        ));
    }
    // Configuration is parsed in the daemon's configuration domain before it
    // can become accepted: a candidate whose configuration does not parse is
    // refused whole, so it never displaces a valid accepted generation.
    if let Some(config) = &candidate.config {
        crate::orchestration::project_config::ProjectConfigSnapshot::parse(
            crate::orchestration::project_config::ProjectConfigProvenance {
                project_id: project_id.as_str().to_string(),
                accepted_generation: candidate.source_generation_id.clone(),
                accepted_commit: candidate.descriptor.publisher_commit.clone(),
            },
            &candidate.descriptor.scope,
            config.iter().map(|file| {
                (
                    file.manifest.repository_relative_filename.as_str(),
                    file.source_bytes.as_slice(),
                )
            }),
        )
        .map_err(|error| PublishError::refusal(error.code(), error.to_string()))?;
    }
    let expected_generation = candidate.source_generation_id.clone();
    let expected_sha256 = candidate.source_generation_sha256.clone();
    let revalidate_pin = Arc::clone(&pinned);
    let probe = project_catalog_admin::PublisherCandidatePublishProbe {
        producer_id: candidate.producer_id.clone(),
        source_generation_id: expected_generation.clone(),
        source_generation_sha256: expected_sha256.clone(),
        scope: candidate.descriptor.scope.clone(),
        full_ref: candidate.descriptor.full_ref.clone(),
        accepted_commit: candidate.descriptor.publisher_commit.clone(),
        sources: PublishSources {
            knowledge: candidate
                .knowledge
                .iter()
                .map(|file| PublishSourceFile {
                    repository_relative_filename: file
                        .manifest
                        .repository_relative_filename
                        .clone(),
                    source_bytes: file.source_bytes.clone(),
                })
                .collect(),
            gaps: candidate
                .gaps
                .iter()
                .map(|file| PublishSourceFile {
                    repository_relative_filename: file
                        .manifest
                        .repository_relative_filename
                        .clone(),
                    source_bytes: file.source_bytes.clone(),
                })
                .collect(),
            graphs: candidate
                .graphs
                .iter()
                .map(|file| PublishSourceFile {
                    repository_relative_filename: file
                        .manifest
                        .repository_relative_filename
                        .clone(),
                    source_bytes: file.source_bytes.clone(),
                })
                .collect(),
            evidence: candidate
                .evidence
                .iter()
                .map(|file| PublishSourceFile {
                    repository_relative_filename: file
                        .manifest
                        .repository_relative_filename
                        .clone(),
                    source_bytes: file.source_bytes.clone(),
                })
                .collect(),
            config: candidate.config.as_ref().map(|files| {
                files
                    .iter()
                    .map(|file| PublishSourceFile {
                        repository_relative_filename: file
                            .manifest
                            .repository_relative_filename
                            .clone(),
                        source_bytes: file.source_bytes.clone(),
                    })
                    .collect()
            }),
        },
        revalidate_source: Box::new(move || {
            let candidate = revalidate_pin.candidate();
            if candidate.source_generation_id == expected_generation
                && candidate.source_generation_sha256 == expected_sha256
            {
                Ok(())
            } else {
                Err(PublishError::refusal(
                    "error.accepted_publication_candidate_stale",
                    "the pinned publication candidate changed before commit",
                ))
            }
        }),
    };
    project_catalog_admin::publish_accepted_publication_candidate(
        store,
        runtime,
        &project_catalog_admin::PublisherCandidatePublishRequest {
            mode,
            project_id: project_id.clone(),
            source_generation_id: source_generation_id.to_string(),
            expected_epoch: expected_catalog_epoch,
            dry_run,
        },
        probe,
    )
}

/// Why an acceptance attempt did not move the pointer, or that it did.
///
/// Every non-accepting outcome is a REASON, never silence. A candidate
/// that sits unserved with nothing recorded anywhere is the failure this
/// ledger exists to prevent.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub(crate) enum AcceptanceOutcome {
    /// The pointer moved. `generation_id` is the newly accepted generation.
    Accepted { generation_id: String },
    /// The daemon is not running the project catalog, so there is no
    /// accepted publication to move.
    CatalogInactive,
    /// The project has no pointer and no producer owns its catalog scope,
    /// so no candidate can establish one.
    NoOwningProducer,
    /// The project has no pointer and no attached, repo-knowledge capable
    /// attachment carries its catalog scope, so it has not been admitted
    /// for publication.
    NoAttachedCheckout,
    /// The project has no pointer and the candidate's ref is not a full
    /// branch ref, so it cannot become the configured ref.
    RefNotBranch,
    /// The accepted pointer is bound to an attachment, not a producer. An
    /// operator rebind moves it onto a producer.
    BindingNotProducer,
    /// The candidate came from a producer other than the bound (or, with no
    /// pointer, owning) producer. An operator rebind changes the producer.
    ProducerMismatch,
    /// The candidate's scope is not the accepted (or, with no pointer,
    /// catalog) scope. An operator scope move changes the scope.
    ScopeChanged,
    /// The candidate's full ref is not the configured ref. An operator
    /// rebind changes the configured ref.
    RefChanged,
    /// The accepted pointer already names this candidate.
    AlreadyAccepted,
    /// This candidate was already attempted in this daemon lifetime. At
    /// most one attempt per uploaded candidate, always.
    AlreadyAttempted,
    /// The acceptance path refused. There is no retry. The prior accepted
    /// generation keeps serving UNLESS the refusal was raised at or after
    /// the pointer swap, which `may_have_swapped` reports: the same signal
    /// `bbox_project_publisher_advance` uses to decide whether it still has
    /// to reconverge after a failure.
    Refused {
        code: String,
        detail: String,
        may_have_swapped: bool,
    },
}

impl AcceptanceOutcome {
    /// Test-only. Production code matches the variant it cares about
    /// directly; this exists so an assertion can say "it accepted" without
    /// naming the generation id it does not know in advance.
    #[cfg(test)]
    pub(crate) fn accepted(&self) -> bool {
        matches!(self, Self::Accepted { .. })
    }

    /// Whether this outcome moved (or may have moved) the accepted
    /// pointer, and therefore obliges the caller to reconverge every
    /// projection built from accepted content.
    ///
    /// Acceptance is the obvious case. The other one is the reason this
    /// predicate exists: a refusal raised at or after the swap leaves the
    /// new pointer installed while reporting an error, so treating every
    /// refusal as "nothing moved" would leave the knowledge index, the
    /// catalog caches, and the graph views projecting a generation the
    /// pointer no longer names.
    pub(crate) fn requires_convergence(&self) -> bool {
        match self {
            Self::Accepted { .. } => true,
            Self::Refused {
                may_have_swapped, ..
            } => *may_have_swapped,
            _ => false,
        }
    }

    /// Outcomes an operator has to act on: the candidate is valid content
    /// but changes the producer, scope, or ref, or the project cannot be
    /// established. These log at warn; the benign skips log at debug.
    fn needs_operator(&self) -> bool {
        matches!(
            self,
            Self::NoOwningProducer
                | Self::NoAttachedCheckout
                | Self::RefNotBranch
                | Self::BindingNotProducer
                | Self::ProducerMismatch
                | Self::ScopeChanged
                | Self::RefChanged
        )
    }

    /// A refusal from the acceptance path keeps the refusing layer's own
    /// code verbatim, exactly as the operator tool reports it, and carries
    /// its swap uncertainty rather than flattening it away.
    fn from_publish_error(error: &PublishError) -> Self {
        Self::Refused {
            code: error.code().to_string(),
            detail: bounded_detail(error.detail().to_string()),
            may_have_swapped: error.may_have_swapped(),
        }
    }

    fn refused(error: &anyhow::Error) -> Self {
        let rendered = error.to_string();
        // Refusals are `code: detail` by convention across the catalog and
        // publication layers. Split on the first separator so status can
        // report a stable code without the caller parsing prose.
        let (code, detail) = match rendered.split_once(": ") {
            Some((code, detail)) if code.starts_with("error.") => {
                (code.to_string(), detail.to_string())
            }
            _ => (
                "error.accepted_publication_acceptance_failed".to_string(),
                rendered,
            ),
        };
        Self::Refused {
            code,
            detail: bounded_detail(detail),
            // A refusal built from a plain error never reached the
            // acceptance path's swap: these are pre-checks, which run
            // before any pointer is touched.
            may_have_swapped: false,
        }
    }
}

fn bounded_detail(detail: String) -> String {
    detail
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .take(384)
        .collect()
}

/// One recorded acceptance attempt, surfaced by publisher status.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct AcceptanceAttempt {
    pub(crate) source_generation_id: String,
    pub(crate) producer_id: String,
    #[serde(flatten)]
    pub(crate) outcome: AcceptanceOutcome,
    pub(crate) at_unix_secs: u64,
}

/// Bounded per-project memory of acceptance attempts.
///
/// Deliberately in-process and non-durable. The ledger answers "what did
/// acceptance just do", not "what has it ever done": the durable answer is
/// the accepted pointer itself, whose producer binding names the exact
/// source generation it serves.
#[derive(Debug, Default)]
pub(crate) struct CandidateAcceptanceLedger {
    inner: parking_lot::Mutex<LedgerInner>,
}

#[derive(Debug, Default)]
struct LedgerInner {
    last: BTreeMap<String, AcceptanceAttempt>,
    attempted: BTreeMap<String, Vec<String>>,
}

impl CandidateAcceptanceLedger {
    /// Claim the single attempt for one candidate.
    ///
    /// Returns false when this candidate was already claimed, which is how
    /// a repeated finalize of the same upload stays at one attempt. The
    /// claim happens before the attempt, so a panic or an early return
    /// still consumes it: a candidate that failed once must not be retried
    /// by the next finalize.
    pub(crate) fn claim_attempt(&self, project_id: &str, source_generation_id: &str) -> bool {
        let mut inner = self.inner.lock();
        let attempted = inner.attempted.entry(project_id.to_string()).or_default();
        if attempted.iter().any(|id| id == source_generation_id) {
            return false;
        }
        attempted.push(source_generation_id.to_string());
        if attempted.len() > MAX_ATTEMPTED_PER_PROJECT {
            attempted.remove(0);
        }
        true
    }

    pub(crate) fn record(&self, project_id: &str, attempt: AcceptanceAttempt) {
        let mut inner = self.inner.lock();
        inner.last.insert(project_id.to_string(), attempt);
        while inner.last.len() > MAX_LEDGER_PROJECTS {
            let Some(oldest) = inner
                .last
                .iter()
                .min_by_key(|(_, attempt)| attempt.at_unix_secs)
                .map(|(project, _)| project.clone())
            else {
                break;
            };
            inner.last.remove(&oldest);
            inner.attempted.remove(&oldest);
        }
    }

    pub(crate) fn last_attempt(&self, project_id: &str) -> Option<AcceptanceAttempt> {
        self.inner.lock().last.get(project_id).cloned()
    }
}

/// Which kind of acceptance an attempt made, for its audit reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AcceptanceKind {
    Establish,
    Advance,
}

/// The audit reason an acceptance writes into its log line.
///
/// It names the acceptance kind, the producer, and the source generation,
/// so a daemon acceptance is distinguishable from an operator move.
pub(crate) fn acceptance_audit_reason(
    establish: bool,
    producer_id: &str,
    source_generation_id: &str,
) -> String {
    let kind = if establish { "establish" } else { "advance" };
    let reason = format!("acceptance:{kind} producer={producer_id} source={source_generation_id}");
    if reason.len() <= MAX_AUDIT_REASON_BYTES {
        return reason;
    }
    reason.chars().take(MAX_AUDIT_REASON_BYTES / 4).collect()
}

pub(crate) fn is_full_branch_ref(value: &str) -> bool {
    value
        .strip_prefix("refs/heads/")
        .is_some_and(|branch| !branch.is_empty())
}

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

impl super::BlackboxServer {
    /// One acceptance attempt for one freshly Ready publication candidate.
    ///
    /// Blocking, at most once per candidate, and never retried. Every exit
    /// records a reason in the ledger, so `bbox_project_publisher_status`
    /// can answer "why is my Ready candidate not serving" without a log
    /// dive. On any refusal the prior accepted generation keeps serving:
    /// this function only ever calls the ordinary acceptance path, which
    /// swaps a pointer or refuses.
    pub(crate) fn accept_ready_candidate(
        &self,
        project_id: &str,
        source_generation_id: &str,
    ) -> AcceptanceOutcome {
        let ledger = self.state.knowledge_sources.acceptance_ledger();
        if !ledger.claim_attempt(project_id, source_generation_id) {
            return AcceptanceOutcome::AlreadyAttempted;
        }
        let (producer_id, kind, outcome) =
            self.run_candidate_acceptance(project_id, source_generation_id);
        ledger.record(
            project_id,
            AcceptanceAttempt {
                source_generation_id: source_generation_id.to_string(),
                producer_id: producer_id.clone(),
                outcome: outcome.clone(),
                at_unix_secs: now_unix_secs(),
            },
        );
        // Converge on acceptance OR on a refusal that reached the swap. A
        // refusal raised at or after the pointer replacement leaves the new
        // pointer durably installed, and skipping convergence there leaves
        // every projection built from accepted content serving a generation
        // no pointer names. Graph views are the sticky case: they have no
        // rebuild-on-read, so they stay stale until the next accept or a
        // daemon restart. Converging touches projections only and never
        // re-enters the acceptance path.
        if outcome.requires_convergence()
            && let Ok(parsed) = ProjectId::parse(project_id.to_string())
        {
            self.invalidate_catalog_published_content(&parsed);
            self.converge_published_knowledge_index(&parsed);
            self.refresh_published_graph_views(&parsed);
        }
        match &outcome {
            AcceptanceOutcome::Accepted { generation_id } => {
                self.observe_knowledge_transport_operation(
                    project_id,
                    bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::AcceptedPublicationMutation,
                    bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Remote,
                );
                let audit_reason = acceptance_audit_reason(
                    kind == AcceptanceKind::Establish,
                    &producer_id,
                    source_generation_id,
                );
                tracing::info!(
                    tool = "candidate_acceptance",
                    project_id,
                    source_generation_id,
                    generation_id = %generation_id,
                    audit_reason = %audit_reason,
                    "catalog administration mutation"
                );
            }
            AcceptanceOutcome::Refused {
                code,
                detail,
                may_have_swapped,
            } => {
                // Loud, once, and then done. A retry loop here would turn
                // one bad candidate into a storm against the publication
                // lock. `may_have_swapped` says which generation is serving
                // after this refusal, so the log answers that without a
                // pointer read.
                tracing::warn!(
                    project_id,
                    source_generation_id,
                    code = %code,
                    detail = %detail,
                    may_have_swapped,
                    "candidate acceptance refused; the prior accepted generation keeps serving \
                     unless the refusal reached the pointer swap"
                );
            }
            operator if operator.needs_operator() => {
                tracing::warn!(
                    project_id,
                    source_generation_id,
                    producer_id = %producer_id,
                    outcome = ?operator,
                    "Ready candidate not accepted; it changes the bound producer, scope, or \
                     configured ref, which takes bbox_project_publisher_advance"
                );
            }
            skipped => {
                tracing::debug!(
                    project_id,
                    source_generation_id,
                    outcome = ?skipped,
                    "candidate acceptance did not apply"
                );
            }
        }
        outcome
    }

    /// The decision half, split out so the ledger write and the logging
    /// happen on exactly one path regardless of where the attempt exits.
    fn run_candidate_acceptance(
        &self,
        project_id: &str,
        source_generation_id: &str,
    ) -> (String, AcceptanceKind, AcceptanceOutcome) {
        // Each early exit builds its own empty producer label: the
        // producer is not known until the accepted binding is read.
        let unknown_producer = String::new;
        let Some(store) = self.state.project_authority.catalog_store().cloned() else {
            return (
                unknown_producer(),
                AcceptanceKind::Advance,
                AcceptanceOutcome::CatalogInactive,
            );
        };
        let Some(runtime) = self.state.accepted_publications.clone() else {
            return (
                unknown_producer(),
                AcceptanceKind::Advance,
                AcceptanceOutcome::CatalogInactive,
            );
        };
        let parsed = match ProjectId::parse(project_id.to_string()) {
            Ok(parsed) => parsed,
            Err(error) => {
                return (
                    unknown_producer(),
                    AcceptanceKind::Advance,
                    AcceptanceOutcome::refused(&anyhow::anyhow!("{error}")),
                );
            }
        };
        let installed = match runtime.installed_pointer(&parsed) {
            Ok(Some(installed)) => installed,
            Ok(None) => {
                let (producer_id, outcome) = self.run_candidate_establish(
                    &store,
                    runtime.as_ref(),
                    &parsed,
                    source_generation_id,
                );
                return (producer_id, AcceptanceKind::Establish, outcome);
            }
            Err(error) => {
                return (
                    unknown_producer(),
                    AcceptanceKind::Advance,
                    AcceptanceOutcome::refused(&anyhow::anyhow!("{error}")),
                );
            }
        };
        let (bound_producer, bound_source_generation) = match (
            installed.source.producer_id(),
            installed.source.source_generation_id(),
        ) {
            (Some(producer_id), Some(source)) => (producer_id.to_string(), source.to_string()),
            // An attachment-bound project publishes from a checkout the
            // operator drives; an operator rebind moves it onto a producer.
            _ => {
                return (
                    unknown_producer(),
                    AcceptanceKind::Advance,
                    AcceptanceOutcome::BindingNotProducer,
                );
            }
        };
        if bound_source_generation == source_generation_id {
            return (
                bound_producer,
                AcceptanceKind::Advance,
                AcceptanceOutcome::AlreadyAccepted,
            );
        }
        let knowledge_sources = self.state.knowledge_sources.store();
        let pinned = match knowledge_sources.pin_ready_publication_candidate(source_generation_id) {
            Ok(pinned) => pinned,
            Err(error) => {
                return (
                    bound_producer,
                    AcceptanceKind::Advance,
                    AcceptanceOutcome::refused(&anyhow::anyhow!(
                        "error.accepted_publication_candidate_required: {error}"
                    )),
                );
            }
        };
        // Same producer, same scope, same configured ref. A candidate that
        // changes any of them is a move an operator makes.
        {
            let candidate = pinned.candidate();
            if candidate.producer_id != bound_producer {
                return (
                    candidate.producer_id.clone(),
                    AcceptanceKind::Advance,
                    AcceptanceOutcome::ProducerMismatch,
                );
            }
            if candidate.descriptor.scope != installed.accepted_scope {
                return (
                    candidate.producer_id.clone(),
                    AcceptanceKind::Advance,
                    AcceptanceOutcome::ScopeChanged,
                );
            }
            if candidate.descriptor.full_ref != installed.full_ref {
                return (
                    candidate.producer_id.clone(),
                    AcceptanceKind::Advance,
                    AcceptanceOutcome::RefChanged,
                );
            }
        }
        drop(pinned);
        let epoch = match store.snapshot() {
            Ok(snapshot) => snapshot.epoch(),
            Err(error) => {
                return (
                    bound_producer,
                    AcceptanceKind::Advance,
                    AcceptanceOutcome::refused(&anyhow::anyhow!("{error}")),
                );
            }
        };
        let producer_auth = self.state.code_sources.producer_auth();
        let outcome = publish_from_ready_candidate(
            &store,
            runtime.as_ref(),
            producer_auth.as_ref(),
            knowledge_sources.as_ref(),
            &parsed,
            source_generation_id,
            // The tokens of the pointer the checks above read: a concurrent
            // move between that read and the swap refuses as a pointer
            // conflict instead of being overwritten.
            PublisherPublishMode::Advance {
                expected_generation_id: installed.expected_generation_id.clone(),
                expected_pointer_sha256: installed.expected_pointer_sha256.clone(),
            },
            epoch,
            false,
        );
        match outcome {
            Ok(receipt) => (
                bound_producer,
                AcceptanceKind::Advance,
                AcceptanceOutcome::Accepted {
                    generation_id: receipt.generation_id().to_string(),
                },
            ),
            Err(error) => (
                bound_producer,
                AcceptanceKind::Advance,
                AcceptanceOutcome::from_publish_error(&error),
            ),
        }
    }

    /// Establish the first pointer from the first valid candidate: the
    /// project's owning producer, its catalog scope, a full branch ref that
    /// becomes the configured ref, and an attached repo-knowledge capable
    /// attachment at that scope proving the project was admitted.
    fn run_candidate_establish(
        &self,
        store: &ProjectCatalogStore,
        runtime: &AcceptedPublicationRuntime,
        project_id: &ProjectId,
        source_generation_id: &str,
    ) -> (String, AcceptanceOutcome) {
        let snapshot = match store.snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return (
                    String::new(),
                    AcceptanceOutcome::refused(&anyhow::anyhow!("{error}")),
                );
            }
        };
        let Some(project) = snapshot.catalog().projects.get(project_id) else {
            return (
                String::new(),
                AcceptanceOutcome::refused(&anyhow::anyhow!(
                    "error.project_catalog_admin_unknown_project: the requested project is not in the catalog"
                )),
            );
        };
        let ProjectScope::Published(catalog_scope) = &project.scope else {
            return (
                String::new(),
                AcceptanceOutcome::refused(&anyhow::anyhow!(
                    "error.project_catalog_admin_scope_required: a legacy-local project has no published scope"
                )),
            );
        };
        let producer_auth = self.state.code_sources.producer_auth();
        let Some(owning_producer) = producer_auth.project_assignment(project_id, catalog_scope)
        else {
            return (String::new(), AcceptanceOutcome::NoOwningProducer);
        };
        let owning_producer = owning_producer.to_string();
        let knowledge_sources = self.state.knowledge_sources.store();
        let pinned = match knowledge_sources.pin_ready_publication_candidate(source_generation_id) {
            Ok(pinned) => pinned,
            Err(error) => {
                return (
                    owning_producer,
                    AcceptanceOutcome::refused(&anyhow::anyhow!(
                        "error.accepted_publication_candidate_required: {error}"
                    )),
                );
            }
        };
        {
            let candidate = pinned.candidate();
            if candidate.producer_id != owning_producer {
                return (
                    candidate.producer_id.clone(),
                    AcceptanceOutcome::ProducerMismatch,
                );
            }
            if candidate.descriptor.scope != *catalog_scope {
                return (
                    candidate.producer_id.clone(),
                    AcceptanceOutcome::ScopeChanged,
                );
            }
            if !is_full_branch_ref(&candidate.descriptor.full_ref) {
                return (
                    candidate.producer_id.clone(),
                    AcceptanceOutcome::RefNotBranch,
                );
            }
            let has_eligible_attachment =
                snapshot
                    .attachments()
                    .attachments
                    .values()
                    .any(|attachment| {
                        attachment.project_id == *project_id
                            && attachment.status == AttachmentStatus::Attached
                            && attachment.capabilities.repo_knowledge
                            && attachment.validated_scope.as_ref() == Some(catalog_scope)
                    });
            if !has_eligible_attachment {
                return (
                    candidate.producer_id.clone(),
                    AcceptanceOutcome::NoAttachedCheckout,
                );
            }
        }
        drop(pinned);

        let outcome = publish_from_ready_candidate(
            store,
            runtime,
            producer_auth.as_ref(),
            knowledge_sources.as_ref(),
            project_id,
            source_generation_id,
            PublisherPublishMode::Establish,
            snapshot.epoch(),
            false,
        );
        match outcome {
            Ok(receipt) => (
                owning_producer,
                AcceptanceOutcome::Accepted {
                    generation_id: receipt.generation_id().to_string(),
                },
            ),
            Err(error) => (
                owning_producer,
                AcceptanceOutcome::from_publish_error(&error),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_candidate_may_be_attempted_exactly_once() {
        let ledger = CandidateAcceptanceLedger::default();
        assert!(ledger.claim_attempt("p_one", "kps_a"));
        assert!(!ledger.claim_attempt("p_one", "kps_a"));
        assert!(
            ledger.claim_attempt("p_one", "kps_b"),
            "a different candidate gets its own single attempt"
        );
        assert!(
            ledger.claim_attempt("p_two", "kps_a"),
            "the claim is per project, not global"
        );
    }

    #[test]
    fn the_attempt_memory_is_bounded_per_project() {
        let ledger = CandidateAcceptanceLedger::default();
        for index in 0..(MAX_ATTEMPTED_PER_PROJECT + 8) {
            assert!(ledger.claim_attempt("p_one", &format!("kps_{index}")));
        }
        let inner = ledger.inner.lock();
        assert_eq!(
            inner.attempted.get("p_one").unwrap().len(),
            MAX_ATTEMPTED_PER_PROJECT
        );
    }

    #[test]
    fn the_audit_reason_names_the_kind_producer_and_source() {
        assert_eq!(
            acceptance_audit_reason(false, "producer-a", "kps_abc"),
            "acceptance:advance producer=producer-a source=kps_abc"
        );
        assert_eq!(
            acceptance_audit_reason(true, "producer-a", "kps_abc"),
            "acceptance:establish producer=producer-a source=kps_abc"
        );
        let long = acceptance_audit_reason(false, &"p".repeat(2048), "kps_abc");
        assert!(long.len() <= MAX_AUDIT_REASON_BYTES);
    }

    #[test]
    fn establish_ref_validation_refuses_non_branch_refs() {
        assert!(is_full_branch_ref("refs/heads/main"));
        assert!(!is_full_branch_ref("refs/heads/"));
        assert!(!is_full_branch_ref("refs/tags/v1"));
        assert!(!is_full_branch_ref("main"));
    }

    #[test]
    fn a_refusal_keeps_the_refusing_layers_error_code() {
        let outcome = AcceptanceOutcome::refused(&anyhow::anyhow!(
            "error.project_catalog_stale_epoch: the catalog changed"
        ));
        assert_eq!(
            outcome,
            AcceptanceOutcome::Refused {
                code: "error.project_catalog_stale_epoch".into(),
                detail: "the catalog changed".into(),
                may_have_swapped: false,
            }
        );
        assert!(!outcome.accepted());
    }

    #[test]
    fn an_uncoded_failure_still_reports_a_stable_code() {
        let outcome = AcceptanceOutcome::refused(&anyhow::anyhow!("something unstructured"));
        let AcceptanceOutcome::Refused { code, detail, .. } = outcome else {
            panic!("expected a refusal");
        };
        assert_eq!(code, "error.accepted_publication_acceptance_failed");
        assert_eq!(detail, "something unstructured");
    }

    /// A refusal raised at or after the swap left the new pointer
    /// installed, so every projection built from accepted content has to
    /// be reconverged even though the attempt reported an error.
    #[test]
    fn a_refusal_that_reached_the_swap_still_obliges_convergence() {
        let swapped = AcceptanceOutcome::from_publish_error(
            &PublishError::refusal("error.accepted_publication_invalid_generation", "read-back")
                .with_swap_uncertainty_for_test(true),
        );
        assert!(
            swapped.requires_convergence(),
            "a swap-uncertain refusal moves the pointer and must reconverge"
        );
        let refused_before_the_swap = AcceptanceOutcome::from_publish_error(
            &PublishError::refusal("error.project_catalog_stale_epoch", "epoch moved"),
        );
        assert!(!refused_before_the_swap.requires_convergence());
        assert!(
            AcceptanceOutcome::Accepted {
                generation_id: "apg_x".into(),
            }
            .requires_convergence()
        );
        assert!(!AcceptanceOutcome::RefChanged.requires_convergence());
        assert!(!AcceptanceOutcome::AlreadyAttempted.requires_convergence());
    }

    #[test]
    fn operator_outcomes_are_distinguished_from_benign_skips() {
        assert!(AcceptanceOutcome::RefChanged.needs_operator());
        assert!(AcceptanceOutcome::NoOwningProducer.needs_operator());
        assert!(!AcceptanceOutcome::AlreadyAccepted.needs_operator());
        assert!(!AcceptanceOutcome::AlreadyAttempted.needs_operator());
        assert!(!AcceptanceOutcome::CatalogInactive.needs_operator());
    }

    #[test]
    fn the_ledger_reports_the_last_attempt_per_project() {
        let ledger = CandidateAcceptanceLedger::default();
        assert_eq!(ledger.last_attempt("p_one"), None);
        ledger.record(
            "p_one",
            AcceptanceAttempt {
                source_generation_id: "kps_a".into(),
                producer_id: "producer-a".into(),
                outcome: AcceptanceOutcome::RefChanged,
                at_unix_secs: 10,
            },
        );
        ledger.record(
            "p_one",
            AcceptanceAttempt {
                source_generation_id: "kps_b".into(),
                producer_id: "producer-a".into(),
                outcome: AcceptanceOutcome::Accepted {
                    generation_id: "apg_x".into(),
                },
                at_unix_secs: 20,
            },
        );
        let last = ledger.last_attempt("p_one").unwrap();
        assert_eq!(last.source_generation_id, "kps_b");
        assert!(last.outcome.accepted());
    }
}
