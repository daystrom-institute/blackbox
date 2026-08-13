//! Policy-gated auto-advance of the accepted publication for producer
//! lanes (`design/daemon-runtime/publisher-auto-advance.md`).
//!
//! Two things live here:
//!
//! 1. [`publish_from_ready_candidate`], the single candidate-acceptance
//!    path. `bbox_project_publisher_advance` and the policy trigger both
//!    call it, so "auto-advance reuses the exact same acceptance path" is
//!    a structural fact rather than a claim about two similar functions.
//! 2. [`PublisherAutoAdvanceLedger`], the bounded per-project record of
//!    what the last policy attempt did, which makes a refusal observable
//!    in `bbox_project_publisher_status` instead of only in logs.
//!
//! The narrowing argument for the transport plan's "no automatic knowledge
//! acceptance by a producer or model" non-goal lives in the design doc. In
//! code it reduces to one invariant: the grant that authorizes an
//! acceptance is read from the pointer the operator installed, never from
//! the candidate being accepted, and a policy attempt always passes
//! [`AutoAdvanceGrantUpdate::Inherit`] so it cannot widen its own
//! authority.

use std::collections::BTreeMap;
use std::sync::Arc;

use bbox_corpus_core::project_catalog::ProjectId;
use bbox_indexing::accepted_publication_runtime::{
    AcceptedPublicationRuntime, AutoAdvanceGrantUpdate, PublishError, PublishReceipt,
    PublishSourceFile, PublishSources, PublisherPublishMode,
};
use bbox_indexing::project_catalog_admin;
use bbox_indexing::project_catalog_store::ProjectCatalogStore;
use bbox_knowledge_source_store::KnowledgeSourceStore;

use super::producer_auth::ProducerAuthRuntime;

/// Longest `audit_reason` the catalog accepts, mirrored here so a
/// generated policy reason is bounded at the point it is built.
const MAX_AUDIT_REASON_BYTES: usize = 1024;

/// The stable refusal every candidate-selection failure carries.
const CANDIDATE_REQUIRED: &str = "error.accepted_publication_candidate_required";

/// Most recent policy attempts retained per daemon lifetime. The ledger is
/// an observability surface, not a queue: it must never be the reason the
/// daemon grows without bound.
const MAX_LEDGER_PROJECTS: usize = 512;

/// Attempted candidates remembered per project. "At most one attempt per
/// uploaded candidate" needs memory of which candidates were attempted;
/// bounding it is what keeps a chatty producer from turning that memory
/// into a leak. Eviction is oldest-first within a project, and an evicted
/// candidate cannot be retried into a pointer move anyway: the accepted
/// pointer already names it, which the pre-checks refuse.
const MAX_ATTEMPTED_PER_PROJECT: usize = 64;

/// The single candidate-acceptance path.
///
/// It resolves the Ready candidate, re-proves the producer grant, builds
/// the publish probe, and hands the whole thing to the same admin
/// entry point the operator tool has always used. Callers differ only in
/// the mode, the audit reason, and whether they may touch the standing
/// grant.
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
    auto_advance: AutoAdvanceGrantUpdate,
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
            auto_advance,
        },
        probe,
    )
}

/// Why a policy attempt did not move the pointer, or that it did.
///
/// Every non-accepting outcome is a REASON, never silence. A candidate
/// that sat unserved with nothing recorded anywhere is the failure this
/// feature exists to end, and replacing it with an unexplained skip would
/// reproduce it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub(crate) enum AutoAdvanceOutcome {
    /// The pointer moved. `generation_id` is the newly accepted generation.
    Accepted { generation_id: String },
    /// The project has no installed pointer, so there is nothing to
    /// advance from. Establish stays manual by design.
    NoAcceptedPublication,
    /// A pointer exists and carries no standing operator grant. This is
    /// the default for every project.
    PolicyDisabled,
    /// The accepted pointer is bound to an attachment, not a producer.
    /// Only producer-bound projects are in scope.
    BindingNotProducer,
    /// The candidate came from a producer other than the one the accepted
    /// pointer is bound to.
    ProducerMismatch,
    /// The candidate's published scope is not the accepted scope. A scope
    /// change is a non-linear move and stays manual.
    ScopeChanged,
    /// The candidate's full ref is not the accepted ref. A ref change is a
    /// non-linear move and stays manual.
    RefChanged,
    /// The accepted pointer already names this candidate. Re-finalizing an
    /// upload must not re-attempt an acceptance that already happened.
    AlreadyAccepted,
    /// This candidate was already attempted in this daemon lifetime. At
    /// most one attempt per uploaded candidate, always.
    AlreadyAttempted,
    /// The acceptance path refused. The prior accepted generation keeps
    /// serving; there is no retry.
    Refused { code: String, detail: String },
}

impl AutoAdvanceOutcome {
    pub(crate) fn accepted(&self) -> bool {
        matches!(self, Self::Accepted { .. })
    }

    /// A refusal from the acceptance path keeps the refusing layer's own
    /// code verbatim, exactly as the operator tool reports it.
    fn from_publish_error(error: &PublishError) -> Self {
        Self::Refused {
            code: error.code().to_string(),
            detail: bounded_detail(error.detail().to_string()),
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
                "error.accepted_publication_auto_advance_failed".to_string(),
                rendered,
            ),
        };
        Self::Refused {
            code,
            detail: bounded_detail(detail),
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

/// One recorded policy attempt, surfaced by publisher status.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct AutoAdvanceAttempt {
    pub(crate) source_generation_id: String,
    pub(crate) producer_id: String,
    #[serde(flatten)]
    pub(crate) outcome: AutoAdvanceOutcome,
    pub(crate) at_unix_secs: u64,
}

/// Bounded per-project memory of policy attempts.
///
/// Deliberately in-process and non-durable. The ledger answers "what did
/// the policy just do", not "what has the policy ever done": the durable
/// answer is the accepted pointer itself plus the audit trail, and making
/// this durable would add a write to a path whose whole safety argument is
/// that it adds no new authority.
#[derive(Debug, Default)]
pub(crate) struct PublisherAutoAdvanceLedger {
    inner: parking_lot::Mutex<LedgerInner>,
}

#[derive(Debug, Default)]
struct LedgerInner {
    last: BTreeMap<String, AutoAdvanceAttempt>,
    attempted: BTreeMap<String, Vec<String>>,
}

impl PublisherAutoAdvanceLedger {
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

    pub(crate) fn record(&self, project_id: &str, attempt: AutoAdvanceAttempt) {
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

    pub(crate) fn last_attempt(&self, project_id: &str) -> Option<AutoAdvanceAttempt> {
        self.inner.lock().last.get(project_id).cloned()
    }
}

/// The audit reason a policy acceptance writes.
///
/// It names the policy, the producer, and the source generation, so
/// `bbox_audit` history distinguishes a policy acceptance from an operator
/// one without inspecting anything else.
pub(crate) fn policy_audit_reason(producer_id: &str, source_generation_id: &str) -> String {
    let reason =
        format!("policy:auto_advance producer={producer_id} source={source_generation_id}");
    if reason.len() <= MAX_AUDIT_REASON_BYTES {
        return reason;
    }
    reason.chars().take(MAX_AUDIT_REASON_BYTES / 4).collect()
}

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

impl super::BlackboxServer {
    /// One policy attempt for one freshly Ready publication candidate.
    ///
    /// Blocking, at most once per candidate, and never retried. Every exit
    /// records a reason in the ledger, so `bbox_project_publisher_status`
    /// can answer "why is my Ready candidate not serving" without a log
    /// dive. On any refusal the prior accepted generation keeps serving:
    /// this function only ever calls the ordinary acceptance path, which
    /// swaps a pointer or refuses.
    pub(crate) fn attempt_publisher_auto_advance(
        &self,
        project_id: &str,
        source_generation_id: &str,
    ) -> AutoAdvanceOutcome {
        let ledger = self.state.knowledge_sources.auto_advance_ledger();
        if !ledger.claim_attempt(project_id, source_generation_id) {
            return AutoAdvanceOutcome::AlreadyAttempted;
        }
        let (producer_id, outcome) =
            self.run_publisher_auto_advance(project_id, source_generation_id);
        let audit_reason = policy_audit_reason(&producer_id, source_generation_id);
        ledger.record(
            project_id,
            AutoAdvanceAttempt {
                source_generation_id: source_generation_id.to_string(),
                producer_id,
                outcome: outcome.clone(),
                at_unix_secs: now_unix_secs(),
            },
        );
        match &outcome {
            AutoAdvanceOutcome::Accepted { generation_id } => {
                // The same convergence the operator tool performs after a
                // real (non dry-run) swap. Skipping it would leave the
                // published index serving a generation no pointer names.
                if let Ok(parsed) = ProjectId::parse(project_id.to_string()) {
                    self.invalidate_catalog_published_content(&parsed);
                    self.converge_published_knowledge_index(&parsed);
                    self.refresh_published_graph_views(&parsed);
                }
                self.observe_knowledge_transport_operation(
                    project_id,
                    bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::AcceptedPublicationMutation,
                    bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Remote,
                );
                tracing::info!(
                    tool = "publisher_auto_advance",
                    project_id,
                    source_generation_id,
                    generation_id = %generation_id,
                    audit_reason = %audit_reason,
                    "catalog administration mutation"
                );
            }
            AutoAdvanceOutcome::Refused { code, detail } => {
                // Loud, once, and then done. A retry loop here would turn
                // one bad candidate into a storm against the publication
                // lock; the operator advances manually after a refusal.
                tracing::warn!(
                    project_id,
                    source_generation_id,
                    code = %code,
                    detail = %detail,
                    "publisher auto-advance refused; the prior accepted generation keeps serving"
                );
            }
            skipped => {
                tracing::debug!(
                    project_id,
                    source_generation_id,
                    outcome = ?skipped,
                    "publisher auto-advance did not apply"
                );
            }
        }
        outcome
    }

    /// The decision half, split out so the ledger write and the logging
    /// happen on exactly one path regardless of where the attempt exits.
    fn run_publisher_auto_advance(
        &self,
        project_id: &str,
        source_generation_id: &str,
    ) -> (String, AutoAdvanceOutcome) {
        let unknown_producer = String::new();
        let Some(store) = self.state.project_authority.catalog_store().cloned() else {
            return (unknown_producer, AutoAdvanceOutcome::NoAcceptedPublication);
        };
        let Some(runtime) = self.state.accepted_publications.clone() else {
            return (unknown_producer, AutoAdvanceOutcome::NoAcceptedPublication);
        };
        let parsed = match ProjectId::parse(project_id.to_string()) {
            Ok(parsed) => parsed,
            Err(error) => {
                return (
                    unknown_producer,
                    AutoAdvanceOutcome::refused(&anyhow::anyhow!("{error}")),
                );
            }
        };
        // THE activation rule: the grant comes from the pointer that is
        // currently accepted, which an operator installed. Nothing about
        // the incoming candidate can put it there.
        let grant = match runtime.auto_advance_grant(&parsed) {
            Ok(Some(grant)) => grant,
            Ok(None) => {
                return (unknown_producer, AutoAdvanceOutcome::NoAcceptedPublication);
            }
            Err(error) => {
                return (
                    unknown_producer,
                    AutoAdvanceOutcome::refused(&anyhow::anyhow!("{error}")),
                );
            }
        };
        if !grant.enabled {
            return (unknown_producer, AutoAdvanceOutcome::PolicyDisabled);
        }
        let (accepted_producer, accepted_source_generation) = match (
            grant.source.producer_id(),
            grant.source.source_generation_id(),
        ) {
            (Some(producer_id), Some(source)) => (producer_id.to_string(), source.to_string()),
            // An attachment-bound project is out of scope: its accepted
            // content comes from a checkout the operator drives, and the
            // linear fast path this policy covers does not exist there.
            _ => return (unknown_producer, AutoAdvanceOutcome::BindingNotProducer),
        };
        if accepted_source_generation == source_generation_id {
            return (accepted_producer, AutoAdvanceOutcome::AlreadyAccepted);
        }
        let knowledge_sources = self.state.knowledge_sources.store();
        let pinned = match knowledge_sources.pin_ready_publication_candidate(source_generation_id) {
            Ok(pinned) => pinned,
            Err(error) => {
                return (
                    accepted_producer,
                    AutoAdvanceOutcome::refused(&anyhow::anyhow!(
                        "error.accepted_publication_candidate_required: {error}"
                    )),
                );
            }
        };
        // The linear fast path, and only it. Same producer, same catalog
        // scope, same published ref. Anything else is a move an operator
        // has to look at.
        {
            let candidate = pinned.candidate();
            if candidate.producer_id != accepted_producer {
                return (
                    candidate.producer_id.clone(),
                    AutoAdvanceOutcome::ProducerMismatch,
                );
            }
            if candidate.descriptor.scope != grant.accepted_scope {
                return (
                    candidate.producer_id.clone(),
                    AutoAdvanceOutcome::ScopeChanged,
                );
            }
            if candidate.descriptor.full_ref != grant.full_ref {
                return (
                    candidate.producer_id.clone(),
                    AutoAdvanceOutcome::RefChanged,
                );
            }
        }
        drop(pinned);
        let epoch = match store.snapshot() {
            Ok(snapshot) => snapshot.epoch(),
            Err(error) => {
                return (
                    accepted_producer,
                    AutoAdvanceOutcome::refused(&anyhow::anyhow!("{error}")),
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
            // Advance only. Establish is the operator's first pointer and
            // is also the act that can grant this policy in the first
            // place, so a policy establish is a contradiction in terms.
            PublisherPublishMode::Advance {
                expected_generation_id: grant.expected_generation_id.clone(),
                expected_pointer_sha256: grant.expected_pointer_sha256.clone(),
            },
            epoch,
            false,
            // Never Set. A policy acceptance inherits the operator's grant
            // and cannot widen it.
            AutoAdvanceGrantUpdate::Inherit,
        );
        match outcome {
            Ok(receipt) => (
                accepted_producer,
                AutoAdvanceOutcome::Accepted {
                    generation_id: receipt.generation_id().to_string(),
                },
            ),
            Err(error) => (
                accepted_producer,
                AutoAdvanceOutcome::from_publish_error(&error),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_candidate_may_be_attempted_exactly_once() {
        let ledger = PublisherAutoAdvanceLedger::default();
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
        let ledger = PublisherAutoAdvanceLedger::default();
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
    fn the_policy_audit_reason_names_the_policy_producer_and_source() {
        let reason = policy_audit_reason("producer-a", "kps_abc");
        assert!(reason.starts_with("policy:auto_advance"), "{reason}");
        assert!(reason.contains("producer=producer-a"), "{reason}");
        assert!(reason.contains("source=kps_abc"), "{reason}");
        assert!(reason.len() <= MAX_AUDIT_REASON_BYTES);
    }

    #[test]
    fn a_refusal_keeps_the_refusing_layers_error_code() {
        let outcome = AutoAdvanceOutcome::refused(&anyhow::anyhow!(
            "error.project_catalog_stale_epoch: the catalog changed"
        ));
        assert_eq!(
            outcome,
            AutoAdvanceOutcome::Refused {
                code: "error.project_catalog_stale_epoch".into(),
                detail: "the catalog changed".into(),
            }
        );
        assert!(!outcome.accepted());
    }

    #[test]
    fn an_uncoded_failure_still_reports_a_stable_code() {
        let outcome = AutoAdvanceOutcome::refused(&anyhow::anyhow!("something unstructured"));
        let AutoAdvanceOutcome::Refused { code, detail } = outcome else {
            panic!("expected a refusal");
        };
        assert_eq!(code, "error.accepted_publication_auto_advance_failed");
        assert_eq!(detail, "something unstructured");
    }

    #[test]
    fn the_ledger_reports_the_last_attempt_per_project() {
        let ledger = PublisherAutoAdvanceLedger::default();
        assert_eq!(ledger.last_attempt("p_one"), None);
        ledger.record(
            "p_one",
            AutoAdvanceAttempt {
                source_generation_id: "kps_a".into(),
                producer_id: "producer-a".into(),
                outcome: AutoAdvanceOutcome::PolicyDisabled,
                at_unix_secs: 10,
            },
        );
        ledger.record(
            "p_one",
            AutoAdvanceAttempt {
                source_generation_id: "kps_b".into(),
                producer_id: "producer-a".into(),
                outcome: AutoAdvanceOutcome::Accepted {
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
