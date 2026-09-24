//! Project render completion through the checkout-owner collector.
//!
//! A catalog project render that no bound workspace applies is executed by
//! the collector that owns the project's checkout. Ownership is the scope
//! grant (config pin or durable claim) that maps the project's published
//! scope to exactly one producer; only then do render-lane presence,
//! transport capability, and the collector's own report of holding the
//! checkout decide whether the owner can complete now. Enrollment-root
//! prefixes and checkout paths never select an owner.
//!
//! The daemon keeps view selection: the plan carries exactly the entries the
//! caller's existing visibility allows (unbound defaults to published, `own`
//! still requires authoritative checkout context), rebound to the transport
//! scope. The owner applies it with the shared renderer and returns a
//! path-free receipt, which is revalidated against the current plan and
//! owner before it is reported as current convergence.

use anyhow::{Context, Result, bail};
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::project_catalog::{ProjectId, ProjectScope};
use bbox_knowledge::overlay::ProvisionalMode;
use bbox_project_render::transport::{
    PROJECT_RENDER_TRANSPORT_SCOPE, PROJECT_RENDER_TRANSPORT_VERSION,
    ProjectRenderProducerAuthorityV1, ProjectRenderViewV1, validate_render_operation_id,
};

use super::BlackboxServer;
use super::render_operations::{
    NewRenderOperation, RenderCompletionValidation, RenderLanePresence, RenderOperationRecord,
    RenderOperationState,
};
use crate::knowledge::{ProjectRenderPlanV1, RenderParams, Scope};

pub(crate) const ONBOARDING_RESOURCE: &str = "blackbox://skills/onboard-project/SKILL.md";
const MAX_GLOBAL_RESULT_BYTES: usize = 16 * 1024;

/// The producer that owns and can currently complete one project render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RenderOwner {
    pub producer_id: String,
    pub project_id: String,
    pub scope: PublishedScope,
}

/// Why an owner exists but cannot complete this render now. Each names the
/// setup that restores completion; none falls back to a daemon checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RenderOwnerRefusal {
    Ambiguous {
        producers: Vec<String>,
    },
    ScopeMismatch {
        producer_id: String,
    },
    NeverPolled {
        producer_id: String,
    },
    Stale {
        producer_id: String,
    },
    MissingCapability {
        producer_id: String,
        collector_version: String,
    },
    NotHolding {
        producer_id: String,
    },
}

impl RenderOwnerRefusal {
    pub(crate) fn message(&self, project_id: &str) -> String {
        match self {
            Self::Ambiguous { producers } => format!(
                "error.render_owner_ambiguous: project {project_id}'s published scope is granted to more than one producer ({}); leave exactly one scope pin or claim in the daemon's code_collection producers, then retry",
                producers.join(", ")
            ),
            Self::ScopeMismatch { producer_id } => format!(
                "error.render_owner_scope: producer {producer_id} holds a grant for this project's published scope that resolves to a different catalog project; reconcile the catalog scope with the producer grant, then retry"
            ),
            Self::NeverPolled { producer_id } => format!(
                "error.render_owner_capability: producer {producer_id} owns project {project_id}'s checkout but has never polled the project render lane; upgrade and restart its bbox-code-collector service on the checkout host, then retry"
            ),
            Self::Stale { producer_id } => format!(
                "error.render_owner_unavailable: producer {producer_id} owns project {project_id}'s checkout but has not polled its render lane within the presence window; start or restart its bbox-code-collector service on the checkout host, then retry"
            ),
            Self::MissingCapability {
                producer_id,
                collector_version,
            } => format!(
                "error.render_owner_capability: producer {producer_id} (collector {collector_version}) does not execute project render transport version {PROJECT_RENDER_TRANSPORT_VERSION}; upgrade its bbox-code-collector, then retry"
            ),
            Self::NotHolding { producer_id } => format!(
                "error.render_owner_checkout: producer {producer_id} owns project {project_id}'s published scope but does not report holding its checkout; configure or enroll the project root in that collector (bbox-code-collector add <path> on the checkout host), then retry"
            ),
        }
    }
}

/// Outcome of owner selection for one catalog project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RenderOwnerSelection {
    /// The project has no published scope or no producer grant covers it.
    None,
    Owner(RenderOwner),
    Refused(RenderOwnerRefusal),
}

/// Pure owner selection: grant rows first, then lane facts. `grants` rows are
/// `(scope, project_id, producer_id)`.
pub(crate) fn select_render_owner(
    project_id: &str,
    scope: &PublishedScope,
    grants: &[(PublishedScope, String, String)],
    lane: impl Fn(&str) -> Option<RenderLanePresence>,
) -> RenderOwnerSelection {
    let rows = grants
        .iter()
        .filter(|(granted, _, _)| granted == scope)
        .collect::<Vec<_>>();
    let producer_id = match rows.as_slice() {
        [] => return RenderOwnerSelection::None,
        [(_, granted_project, producer_id)] => {
            if granted_project != project_id {
                return RenderOwnerSelection::Refused(RenderOwnerRefusal::ScopeMismatch {
                    producer_id: producer_id.clone(),
                });
            }
            producer_id.clone()
        }
        rows => {
            let mut producers = rows
                .iter()
                .map(|(_, _, producer)| producer.clone())
                .collect::<Vec<_>>();
            producers.sort();
            producers.dedup();
            return RenderOwnerSelection::Refused(RenderOwnerRefusal::Ambiguous { producers });
        }
    };
    let Some(presence) = lane(&producer_id) else {
        return RenderOwnerSelection::Refused(RenderOwnerRefusal::NeverPolled { producer_id });
    };
    if !presence.fresh {
        return RenderOwnerSelection::Refused(RenderOwnerRefusal::Stale { producer_id });
    }
    if !presence.supports_current_transport {
        return RenderOwnerSelection::Refused(RenderOwnerRefusal::MissingCapability {
            producer_id,
            collector_version: presence.collector_version,
        });
    }
    if !presence.covered_scopes.contains(scope) {
        return RenderOwnerSelection::Refused(RenderOwnerRefusal::NotHolding { producer_id });
    }
    RenderOwnerSelection::Owner(RenderOwner {
        producer_id,
        project_id: project_id.to_string(),
        scope: scope.clone(),
    })
}

/// Refusal for a project with no checkout owner. It names the setup that
/// would let an owner complete the render.
pub(crate) fn owner_required_message(project_id: &str, detail: Option<&str>) -> String {
    let mut message = format!(
        "error.render_locality_required: no checkout owner covers project {project_id}, so its project render has nowhere to apply. Enroll the checkout with the code collector on the host that holds it (bbox-code-collector add <path> there, or bbox_project_register with that producer); once its collector polls the project render lane, retry this call. Onboarding guide: {ONBOARDING_RESOURCE}"
    );
    if let Some(detail) = detail {
        message.push_str(&format!(" (daemon checkout access: {detail})"));
    }
    message
}

/// View parameters of one owner render, reused to revalidate a completion.
struct OwnerPlanRequest<'a> {
    provider: Option<String>,
    dry_run: bool,
    provisional: Option<&'a str>,
    requested_scope: String,
}

impl BlackboxServer {
    /// Resolve the checkout owner of one catalog project.
    pub(crate) fn render_owner_for(&self, project_id: &str) -> Result<RenderOwnerSelection> {
        let Some(store) = self.state.project_authority.catalog_store() else {
            return Ok(RenderOwnerSelection::None);
        };
        let snapshot = store
            .snapshot()
            .map_err(|error| anyhow::anyhow!("reading the project catalog: {error}"))?;
        let id = ProjectId::parse(project_id)
            .map_err(|error| anyhow::anyhow!("invalid catalog project id: {error}"))?;
        let Some(project) = snapshot.catalog().projects.get(&id) else {
            return Ok(RenderOwnerSelection::None);
        };
        let ProjectScope::Published(scope) = &project.scope else {
            return Ok(RenderOwnerSelection::None);
        };
        let grants = self.state.code_sources.producer_auth().scope_grant_rows();
        let runtime = &self.state.render_operations;
        Ok(select_render_owner(
            project_id,
            scope,
            &grants,
            |producer| runtime.lane(producer),
        ))
    }

    fn owner_render_view_mode(&self, provisional: Option<&str>) -> Result<ProjectRenderViewV1> {
        let has_checkout = self.authoritative_session_checkout().is_some()
            || self.authoritative_session_workspace_binding().is_some();
        Ok(match ProvisionalMode::parse(provisional, has_checkout)? {
            ProvisionalMode::Published => ProjectRenderViewV1::Published,
            ProvisionalMode::Own => ProjectRenderViewV1::Own,
            ProvisionalMode::All => ProjectRenderViewV1::All,
        })
    }

    /// Build the path-free plan an owner applies, under one producer
    /// authority.
    fn owner_render_plan(
        &self,
        owner: &RenderOwner,
        request: &OwnerPlanRequest<'_>,
        authority: ProjectRenderProducerAuthorityV1,
    ) -> Result<(
        ProjectRenderPlanV1,
        super::knowledge_view::SessionKnowledgeView,
    )> {
        let view_mode = self.owner_render_view_mode(request.provisional)?;
        let view = self.session_knowledge_view(Some(&owner.project_id), request.provisional)?;
        let mut entries = view
            .items
            .iter()
            .filter(|item| {
                item.entry.scope == Scope::Project
                    && (item.entry.project_id.as_deref() == Some(owner.project_id.as_str())
                        || item.metadata.published_scope.as_ref() == Some(&owner.scope))
            })
            .map(|item| item.entry.clone())
            .collect::<Vec<_>>();
        for entry in &mut entries {
            entry.project = Some(PROJECT_RENDER_TRANSPORT_SCOPE.into());
            entry.project_id = Some(owner.project_id.clone());
        }
        entries.sort_by(|left, right| left.id.cmp(&right.id));
        let plan = ProjectRenderPlanV1 {
            version: PROJECT_RENDER_TRANSPORT_VERSION,
            project_id: owner.project_id.clone(),
            scope: owner.scope.clone(),
            workspace_id: String::new(),
            producer: Some(authority),
            provider: request.provider.clone(),
            dry_run: request.dry_run,
            view: view_mode,
            requested_scope: request.requested_scope.clone(),
            entries,
            diagnostics: view.diagnostics_text(),
        };
        plan.validate()?;
        Ok((plan, view))
    }

    /// Complete one unbound project render through its checkout owner:
    /// global first for `scope=both`, then one fresh operation for the
    /// project half.
    pub(crate) fn owner_project_render(
        &self,
        p: &RenderParams,
        owner: RenderOwner,
        handle: &tokio::runtime::Handle,
    ) -> Result<String> {
        if p.global_plan.is_some() {
            bail!("error.bad_input: global_plan is only valid with scope \"global\"");
        }
        let requested_scope = p.scope.clone().unwrap_or_else(|| "both".into());
        if !matches!(requested_scope.as_str(), "project" | "both") {
            bail!("a checkout-owner render requires scope=project or scope=both");
        }
        let request = OwnerPlanRequest {
            provider: p.provider.clone(),
            dry_run: p.dry_run.unwrap_or(false),
            provisional: p.provisional.as_deref(),
            requested_scope: requested_scope.clone(),
        };
        // Resolve the view before any effect, so an unauthorized `own`
        // refuses without rendering the global half.
        self.owner_render_view_mode(request.provisional)?;
        // Global first: the project half is not issued until the global
        // half rendered.
        let global_result = if requested_scope == "both" {
            let view = self.session_knowledge_view(Some(&owner.project_id), request.provisional)?;
            Some(
                view.knowledge
                    .render(&RenderParams {
                        provider: request.provider.clone(),
                        scope: Some("global".into()),
                        dry_run: Some(request.dry_run),
                        ..Default::default()
                    })
                    .context("global render failed; the project half was not issued")?,
            )
        } else {
            None
        };
        let runtime = self.state.render_operations.clone();
        let mut attempt = 0;
        let (record, diagnostics) = loop {
            attempt += 1;
            let (operation_id, sequence) = runtime.reserve(&owner.project_id)?;
            let authority = ProjectRenderProducerAuthorityV1 {
                producer_id: owner.producer_id.clone(),
                operation_id,
                sequence,
                issued_at_ms: bbox_project_render::execute::now_unix_ms(),
            };
            let (plan, _) = self.owner_render_plan(&owner, &request, authority)?;
            match runtime.create(
                NewRenderOperation {
                    producer_id: &owner.producer_id,
                    project_id: &owner.project_id,
                    scope: &owner.scope,
                    provider: request.provider.clone(),
                    dry_run: request.dry_run,
                    view: plan.view,
                    requested_scope: requested_scope.clone(),
                },
                &plan,
            ) {
                Ok(record) => break (record, plan.diagnostics.clone()),
                Err(error)
                    if attempt < 3 && format!("{error:#}").contains("error.render_plan_stale") => {}
                Err(error) => return Err(error),
            }
        };
        let settled = handle
            .block_on(runtime.wait_settled(&record.operation_id, runtime.wait_timeout()))
            .unwrap_or(record);
        self.owner_render_response(&settled, &request, global_result, diagnostics)
    }

    /// Explicit recovery of one earlier operation: its recorded receipt,
    /// never a re-application.
    pub(crate) fn recover_owner_render(
        &self,
        p: &RenderParams,
        operation_id: &str,
        handle: &tokio::runtime::Handle,
    ) -> Result<String> {
        validate_render_operation_id(operation_id)?;
        let raw = p
            .project
            .as_deref()
            .context("operation recovery requires the project the render targeted")?;
        let project_id = self.validate_project_selection(raw)?;
        let runtime = self.state.render_operations.clone();
        let record = runtime.record(operation_id).with_context(|| {
            format!(
                "error.render_operation_unknown: operation {operation_id} is not retained; call bbox_render again for a fresh render"
            )
        })?;
        if record.project_id != project_id {
            bail!(
                "error.render_operation_unknown: operation {operation_id} did not target this project"
            );
        }
        let record = if record.is_pending() {
            handle
                .block_on(runtime.wait_settled(operation_id, runtime.wait_timeout()))
                .unwrap_or(record)
        } else {
            record
        };
        let request = OwnerPlanRequest {
            provider: record.provider.clone(),
            dry_run: record.dry_run,
            provisional: Some(record.view.as_str()),
            requested_scope: record.requested_scope.clone(),
        };
        let diagnostics = runtime
            .issued_plan(operation_id)
            .ok()
            .and_then(|plan| plan.diagnostics);
        self.owner_render_response(&record, &request, None, diagnostics)
    }

    /// Revalidate a completed receipt against the current owner and plan.
    fn revalidate_owner_completion(
        &self,
        record: &RenderOperationRecord,
        request: &OwnerPlanRequest<'_>,
    ) -> Result<(RenderCompletionValidation, Option<ProjectRenderPlanV1>)> {
        let owner = match self.render_owner_for(&record.project_id)? {
            RenderOwnerSelection::Owner(owner) if owner.producer_id == record.producer_id => owner,
            _ => {
                return Ok((
                    RenderCompletionValidation::Stale {
                        reason: "the checkout owner's authority changed after the plan was issued"
                            .into(),
                    },
                    None,
                ));
            }
        };
        let authority = ProjectRenderProducerAuthorityV1 {
            producer_id: record.producer_id.clone(),
            operation_id: record.operation_id.clone(),
            sequence: record.sequence,
            issued_at_ms: record.issued_at_ms,
        };
        let (plan, _) = self.owner_render_plan(&owner, request, authority)?;
        if plan.transport_sha256()? != record.plan_sha256 {
            return Ok((
                RenderCompletionValidation::Stale {
                    reason: "project knowledge or visibility changed after the plan was issued"
                        .into(),
                },
                None,
            ));
        }
        Ok((RenderCompletionValidation::Current, Some(plan)))
    }

    fn owner_render_response(
        &self,
        record: &RenderOperationRecord,
        request: &OwnerPlanRequest<'_>,
        global_result: Option<String>,
        diagnostics: Option<String>,
    ) -> Result<String> {
        let runtime = &self.state.render_operations;
        let latest = runtime.latest_sequence(&record.project_id);
        let mut response = serde_json::json!({
            "operation_id": record.operation_id,
            "sequence": record.sequence,
            "owner": record.producer_id,
            "project_id": record.project_id,
            "view": record.view.as_str(),
            "provider": record.provider,
            "dry_run": record.dry_run,
            "recover": {
                "tool": "bbox_render",
                "arguments": {"project": record.project_id, "operation": record.operation_id},
            },
        });
        if let Some(global) = global_result {
            response["global_result"] = serde_json::json!(bounded_text(global));
        }
        match &record.state {
            RenderOperationState::Pending => {
                response["status"] = "render_pending".into();
                response["detail"] = "The checkout owner has not returned a receipt yet. Recover this operation with the arguments under `recover`; a fresh bbox_render starts a new operation instead.".into();
            }
            RenderOperationState::Superseded { by } => {
                response["status"] = "render_superseded".into();
                response["superseded_by"] = by.clone().into();
                response["current"] = false.into();
                response["detail"] = "A newer render of this project was issued before this operation settled; it was not applied.".into();
            }
            RenderOperationState::Failed { error } => {
                bail!(
                    "error.render_owner_failed: the checkout owner {} refused operation {} and wrote nothing: {}: {}",
                    record.producer_id,
                    record.operation_id,
                    error.code,
                    error.message
                );
            }
            RenderOperationState::Completed {
                receipt,
                validation,
                late,
            } => {
                // `validation` is what held when the receipt arrived; it is
                // history. Present validity is rechecked on every response,
                // so a receipt stops being current once knowledge or owner
                // authority changes, even without a newer render.
                let (at_completion, present) = match validation {
                    RenderCompletionValidation::Stale { .. } => {
                        (validation.clone(), Some(validation.clone()))
                    }
                    RenderCompletionValidation::Unverified
                    | RenderCompletionValidation::Current => {
                        match self.revalidate_owner_completion(record, request) {
                            Ok((checked, current_plan)) => {
                                if *validation == RenderCompletionValidation::Unverified {
                                    // The current plan is byte-identical to
                                    // the one the owner applied, so it
                                    // proves the receipt.
                                    if let Some(plan) = current_plan {
                                        self.state
                                            .render_locality_observations
                                            .record_completed(&plan, receipt)?;
                                    }
                                    runtime
                                        .set_validation(&record.operation_id, checked.clone())?;
                                    (checked.clone(), Some(checked))
                                } else {
                                    (validation.clone(), Some(checked))
                                }
                            }
                            Err(error) => {
                                response["validation_error"] =
                                    bounded_text(format!("{error:#}")).into();
                                (validation.clone(), None)
                            }
                        }
                    }
                };
                let current = present == Some(RenderCompletionValidation::Current)
                    && !late
                    && latest == Some(record.sequence);
                let outcome = receipt.outcome();
                let stale = matches!(present, Some(RenderCompletionValidation::Stale { .. }));
                response["status"] = match (stale, outcome) {
                    (true, _) => "render_stale",
                    (_, bbox_project_render::transport::ProjectRenderOutcomeV1::Partial) => {
                        "render_partial"
                    }
                    _ => "render_complete",
                }
                .into();
                response["outcome"] = serde_json::to_value(outcome)?;
                response["validation"] = serde_json::to_value(&at_completion)?;
                response["present_validation"] = match &present {
                    Some(present) => serde_json::to_value(present)?,
                    None => serde_json::json!({"status": "unverified"}),
                };
                response["current"] = current.into();
                if !current && latest.is_some_and(|latest| latest > record.sequence) {
                    response["detail"] = "A newer render of this project exists; this receipt is historical and does not describe current checkout state.".into();
                } else if !current && stale {
                    response["detail"] = "Project knowledge or checkout-owner authority changed since this receipt; it does not describe current convergence. Render again for a fresh operation.".into();
                }
                response["dispositions"] = serde_json::to_value(receipt.disposition_counts())?;
                response["receipt"] = serde_json::to_value(receipt)?;
            }
        }
        if let Some(diagnostics) = diagnostics {
            response["diagnostics"] = bounded_text(diagnostics).into();
        }
        Ok(serde_json::to_string_pretty(&response)?)
    }
}

fn bounded_text(mut text: String) -> String {
    if text.len() <= MAX_GLOBAL_RESULT_BYTES {
        return text;
    }
    let mut end = MAX_GLOBAL_RESULT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let omitted = text.len() - end;
    text.truncate(end);
    text.push_str(&format!("\n[{omitted} more bytes omitted]"));
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    const PROJECT: &str = "p_render_owner";

    fn scope() -> PublishedScope {
        PublishedScope::try_new("repo-render-owner", ".").unwrap()
    }

    fn lane(fresh: bool, current: bool, covered: bool) -> RenderLanePresence {
        RenderLanePresence {
            producer_id: "producer-a".into(),
            supports_current_transport: current,
            covered_scopes: if covered {
                BTreeSet::from([scope()])
            } else {
                BTreeSet::new()
            },
            collector_version: "0.0.1".into(),
            fresh,
        }
    }

    fn grants(rows: &[(&str, &str)]) -> Vec<(PublishedScope, String, String)> {
        rows.iter()
            .map(|(project, producer)| (scope(), project.to_string(), producer.to_string()))
            .collect()
    }

    #[test]
    fn the_scope_grant_selects_the_owner_before_lane_facts() {
        let owner = select_render_owner(
            PROJECT,
            &scope(),
            &grants(&[(PROJECT, "producer-a")]),
            |producer| (producer == "producer-a").then(|| lane(true, true, true)),
        );
        assert_eq!(
            owner,
            RenderOwnerSelection::Owner(RenderOwner {
                producer_id: "producer-a".into(),
                project_id: PROJECT.into(),
                scope: scope(),
            })
        );
        // A second fresh producer that reports holding a checkout at the
        // identical path, with no grant, never becomes the owner.
        let owner = select_render_owner(
            PROJECT,
            &scope(),
            &grants(&[(PROJECT, "producer-a")]),
            |_| Some(lane(true, true, true)),
        );
        assert!(
            matches!(owner, RenderOwnerSelection::Owner(owner) if owner.producer_id == "producer-a")
        );
    }

    #[test]
    fn every_owner_failure_is_a_distinct_refusal() {
        let granted = grants(&[(PROJECT, "producer-a")]);
        let cases = [
            (None, "error.render_owner_capability"),
            (
                Some(lane(false, true, true)),
                "error.render_owner_unavailable",
            ),
            (
                Some(lane(true, false, true)),
                "error.render_owner_capability",
            ),
            (Some(lane(true, true, false)), "error.render_owner_checkout"),
        ];
        for (presence, code) in cases {
            let RenderOwnerSelection::Refused(refusal) =
                select_render_owner(PROJECT, &scope(), &granted, |_| presence.clone())
            else {
                panic!("expected a refusal for {code}");
            };
            let message = refusal.message(PROJECT);
            assert!(message.starts_with(code), "{message}");
            assert!(!message.contains("bro-harness"), "{message}");
        }
        assert!(matches!(
            select_render_owner(
                PROJECT,
                &scope(),
                &grants(&[(PROJECT, "producer-a"), (PROJECT, "producer-b")]),
                |_| Some(lane(true, true, true)),
            ),
            RenderOwnerSelection::Refused(RenderOwnerRefusal::Ambiguous { producers })
                if producers == ["producer-a", "producer-b"]
        ));
        assert!(matches!(
            select_render_owner(
                PROJECT,
                &scope(),
                &grants(&[("p_other", "producer-a")]),
                |_| Some(lane(true, true, true)),
            ),
            RenderOwnerSelection::Refused(RenderOwnerRefusal::ScopeMismatch { .. })
        ));
        assert_eq!(
            select_render_owner(PROJECT, &scope(), &[], |_| Some(lane(true, true, true))),
            RenderOwnerSelection::None
        );
        let required =
            owner_required_message(PROJECT, Some("error.checkout_access.attachment_inactive"));
        assert!(required.starts_with("error.render_locality_required"));
        assert!(required.contains("bbox-code-collector add"));
        assert!(required.contains(ONBOARDING_RESOURCE));
        assert!(!required.contains("bro-harness"));
    }
}

#[cfg(test)]
#[path = "render_owner_tests.rs"]
mod render_tests;
