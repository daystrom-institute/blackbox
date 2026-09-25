//! Catalog-mode project bro configuration: the daemon's one read view and one
//! write lane for repo-owned `.bro/brofiles`, `.bro/teamplates` and
//! `.bbox/mcp.json`.
//!
//! A caller's path or selector only selects a catalog project; it never grants
//! filesystem authority. Reads come from the project's verified accepted
//! publication. Writes are guarded checkout mutations delivered to the
//! scope's checkout owner and become visible only after the owner commits and
//! publishes them. Bridge mode keeps its daemon-local checkout behavior.

use std::sync::Arc;

use bbox_code_source::ProjectConfigTargetV1;
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::project_catalog::{ProjectId, ProjectScope};
use bbox_indexing::accepted_publication_runtime::{
    AcceptedPublicationContentStamp, ERROR_ACCEPTED_PUBLICATION_MISSING,
};
use serde::Serialize;

use crate::checkout_mutations::CheckoutMutationProgress;
use crate::orchestration;
use crate::orchestration::project_config::{
    ProjectConfigError, ProjectConfigProvenance, ProjectConfigSnapshot, ProjectConfigSource,
    Resolved,
};
use crate::server::state::SharedState;

/// Where project configuration for one call comes from.
#[derive(Debug, Clone)]
pub(crate) enum ProjectConfigContext {
    /// Bridge mode: the daemon's own checkout paths stay authoritative.
    Local(Option<String>),
    /// No project selected (or the selector names no catalog project):
    /// global configuration only.
    GlobalOnly,
    /// The selected catalog project's accepted configuration.
    Accepted(Arc<ProjectConfigSnapshot>),
}

/// A verified accepted configuration view plus the identity it was read at.
pub(crate) struct AcceptedProjectConfig {
    pub(crate) project_id: String,
    pub(crate) scope: PublishedScope,
    pub(crate) stamp: AcceptedPublicationContentStamp,
    pub(crate) snapshot: Arc<ProjectConfigSnapshot>,
}

/// One queued guarded configuration mutation, as its producer reports it.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ProjectConfigMutationReceipt {
    pub(crate) mutation_id: String,
    pub(crate) state: CheckoutMutationProgress,
    /// `write` or `delete`.
    pub(crate) mode: String,
    pub(crate) project_id: String,
    pub(crate) landing: ProjectConfigLanding,
    /// Exact-byte precondition the owner checks; `None` asserts absence.
    pub(crate) expected_sha256: Option<String>,
    pub(crate) predecessor: Option<String>,
    /// The accepted generation reads and dispatch keep using until the
    /// owner commits and publishes this change.
    pub(crate) accepted_generation: String,
    pub(crate) next_step: String,
}

/// The authorized landing place: the published scope whose checkout owner
/// receives the mutation, and the file inside it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct ProjectConfigLanding {
    pub(crate) repo_id: String,
    pub(crate) bbox_root_relpath: String,
    pub(crate) scope_relative_path: String,
    pub(crate) repository_relative_path: String,
}

impl ProjectConfigLanding {
    fn new(scope: &PublishedScope, scope_relative_path: &str) -> Self {
        Self {
            repo_id: scope.repo_id().to_string(),
            bbox_root_relpath: scope.bbox_root_relpath().to_string(),
            scope_relative_path: scope_relative_path.to_string(),
            repository_relative_path:
                bbox_knowledge_source::config_source_repository_relative_filename(
                    scope,
                    scope_relative_path,
                ),
        }
    }
}

/// Where one configuration mutation stands now.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ProjectConfigMutationStatus {
    pub(crate) mutation_id: String,
    pub(crate) state: CheckoutMutationProgress,
    pub(crate) mode: String,
    pub(crate) landing: ProjectConfigLanding,
    /// For `conflicted`: the owner's bytes at apply time (`None` = absent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) observed_sha256: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) blocked_by: Option<String>,
    /// A poll withheld it because the owner's collector lacks guarded support.
    pub(crate) owner_collector_unsupported: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) owner_error: Option<String>,
    pub(crate) next_step: String,
}

/// The change a configuration edit makes to its base bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProjectConfigEdit {
    Write(String),
    Delete,
}

const COMMIT_TO_PUBLISH: &str = "The checkout owner's collector applies it on its next mutation poll. Commit and publish the file on the project's publisher ref for it to take effect; until the next accepted publication includes it, reads and dispatch keep the accepted configuration";
const RECONCILE_AND_RETRY: &str = "The owner's file did not match the expected bytes, so the owner kept its bytes. Reconcile in the owning checkout (commit and publish the local version, or revert it), then re-issue the edit; it is recomputed from the accepted configuration with a fresh precondition";

impl SharedState {
    /// The catalog project's published scope, when it has one.
    pub(crate) fn catalog_published_scope(&self, project_id: &str) -> Option<PublishedScope> {
        let store = self.project_authority.catalog_store()?.clone();
        let snapshot = store.snapshot().ok()?;
        let id = ProjectId::parse(project_id).ok()?;
        match &snapshot.catalog().projects.get(&id)?.scope {
            ProjectScope::Published(scope) => Some(scope.clone()),
            _ => None,
        }
    }

    /// Selection-class resolution through the catalog resolver (the same
    /// engine and intent `resolve_project_selection` uses). A selector is a
    /// name for a catalog project, never a path the daemon reads.
    fn catalog_project_for_selector(&self, selector: &str) -> Option<String> {
        let store = self.project_authority.catalog_store()?.clone();
        let snapshot = store.snapshot().ok()?;
        let engine = bbox_indexing::project_resolver::ProjectResolverEngine::v2(
            snapshot.catalog(),
            snapshot.attachments(),
        );
        engine
            .resolve(
                &bbox_corpus_core::project_selector::ProjectSelectorRequest::selection(
                    selector,
                    bbox_corpus_core::project_selector::ResolveIntent::Read,
                ),
            )
            .ok()?
            .project_id()
            .map(str::to_owned)
    }

    fn catalog_project_for_scope(&self, scope: &PublishedScope) -> Option<String> {
        let store = self.project_authority.catalog_store()?.clone();
        let snapshot = store.snapshot().ok()?;
        snapshot
            .catalog()
            .projects
            .values()
            .find(|project| matches!(&project.scope, ProjectScope::Published(published) if published == scope))
            .map(|project| project.project_id.as_str().to_string())
    }

    /// Load and parse one catalog project's accepted configuration. Every
    /// state other than a parsed configuration lane is an error; none of them
    /// is allowed to look like an absent project override.
    pub(crate) fn load_accepted_project_config(
        &self,
        project_id: &str,
    ) -> Result<AcceptedProjectConfig, ProjectConfigError> {
        let unavailable = |detail: &str| ProjectConfigError::PublicationUnavailable {
            project_id: project_id.to_string(),
            detail: detail.to_string(),
        };
        let scope = self
            .catalog_published_scope(project_id)
            .ok_or_else(|| unavailable("the catalog project has no published scope"))?;
        let runtime = self
            .accepted_publications
            .as_ref()
            .ok_or_else(|| unavailable("the accepted publication runtime is unavailable"))?;
        let project = ProjectId::parse(project_id)
            .map_err(|_| unavailable("the project id is not a catalog project id"))?;
        let verified = runtime.load_verified(&project).map_err(|error| {
            if error.code() == ERROR_ACCEPTED_PUBLICATION_MISSING {
                unavailable("no accepted publication exists yet")
            } else {
                unavailable(error.code())
            }
        })?;
        let stamp = verified.content_stamp().clone();
        if stamp.accepted_scope() != &scope {
            return Err(unavailable(
                "the accepted publication's scope differs from the catalog scope; scope reconciliation is pending",
            ));
        }
        let sources =
            verified
                .config_sources()
                .ok_or_else(|| ProjectConfigError::LaneUnsupported {
                    project_id: project_id.to_string(),
                    generation_id: stamp.generation_id().to_string(),
                })?;
        let snapshot = ProjectConfigSnapshot::parse(
            ProjectConfigProvenance {
                project_id: project_id.to_string(),
                accepted_generation: stamp.generation_id().to_string(),
                accepted_commit: stamp.accepted_commit().to_string(),
            },
            &scope,
            sources
                .iter()
                .map(|(filename, source)| (filename.as_str(), source.source_bytes.as_slice())),
        )?;
        Ok(AcceptedProjectConfig {
            project_id: project_id.to_string(),
            scope,
            stamp,
            snapshot: Arc::new(snapshot),
        })
    }

    /// Select the configuration context for one call. In catalog mode a
    /// selector that names no catalog project is not a project context, so
    /// only global configuration applies; a selector that names one reads
    /// its accepted configuration or fails with a named reason.
    pub(crate) fn project_config_context(
        &self,
        project_dir: Option<&str>,
    ) -> Result<ProjectConfigContext, ProjectConfigError> {
        if self.project_authority.is_bridge() {
            return Ok(ProjectConfigContext::Local(project_dir.map(str::to_owned)));
        }
        let Some(selector) = project_dir.map(str::trim).filter(|value| !value.is_empty()) else {
            return Ok(ProjectConfigContext::GlobalOnly);
        };
        let project_id = self.catalog_project_for_selector(selector);
        let Some(project_id) = project_id else {
            return Ok(ProjectConfigContext::GlobalOnly);
        };
        self.load_accepted_project_config(&project_id)
            .map(|accepted| ProjectConfigContext::Accepted(accepted.snapshot))
    }

    /// Dispatch-time brofile resolution: project first, then global only
    /// when the project view proves the override absent.
    pub(crate) fn resolve_config_brofile(
        &self,
        name: &str,
        project_dir: Option<&str>,
    ) -> Result<Option<Resolved<orchestration::brofile::Brofile>>, ProjectConfigError> {
        let store_dir = &self.store_dir;
        Ok(match self.project_config_context(project_dir)? {
            ProjectConfigContext::Local(project_dir) => {
                orchestration::brofile::resolve_brofile(name, store_dir, project_dir.as_deref())
                    .map(|value| Resolved {
                        value,
                        source: ProjectConfigSource::Local,
                    })
            }
            ProjectConfigContext::GlobalOnly => {
                orchestration::project_config::resolve_brofile(None, name, store_dir)
            }
            ProjectConfigContext::Accepted(snapshot) => {
                orchestration::project_config::resolve_brofile(Some(&snapshot), name, store_dir)
            }
        })
    }

    /// [`Self::resolve_config_brofile`] for dispatch paths, which report a
    /// caller-facing string. An unavailable or invalid project view is an
    /// error here, never a silent global fallback.
    pub(crate) fn dispatch_brofile(
        &self,
        name: &str,
        project_dir: Option<&str>,
    ) -> Result<Option<orchestration::brofile::Brofile>, String> {
        self.resolve_config_brofile(name, project_dir)
            .map(|resolved| resolved.map(|resolved| resolved.value))
            .map_err(|error| error.to_string())
    }

    /// Dispatch-time teamplate resolution, same precedence as brofiles.
    pub(crate) fn resolve_config_teamplate(
        &self,
        name: &str,
        project_dir: Option<&str>,
    ) -> Result<Option<Resolved<orchestration::team::Teamplate>>, ProjectConfigError> {
        let store_dir = &self.store_dir;
        Ok(match self.project_config_context(project_dir)? {
            ProjectConfigContext::Local(project_dir) => {
                orchestration::team::resolve_teamplate(name, store_dir, project_dir.as_deref()).map(
                    |value| Resolved {
                        value,
                        source: ProjectConfigSource::Local,
                    },
                )
            }
            ProjectConfigContext::GlobalOnly => {
                orchestration::project_config::resolve_teamplate(None, name, store_dir)
            }
            ProjectConfigContext::Accepted(snapshot) => {
                orchestration::project_config::resolve_teamplate(Some(&snapshot), name, store_dir)
            }
        })
    }

    /// The project MCP store dispatch filters merge over the global store.
    /// Bridge mode keeps its daemon-local read, including discarding load
    /// errors; catalog mode reads the accepted store or fails by name.
    pub(crate) fn dispatch_project_mcp_store(
        &self,
        project_dir: Option<&str>,
    ) -> Result<Option<orchestration::mcp::McpStore>, ProjectConfigError> {
        Ok(match self.project_config_context(project_dir)? {
            ProjectConfigContext::Local(Some(project_dir)) => orchestration::mcp::McpStore::load(
                &orchestration::mcp::project_store_path(std::path::Path::new(&project_dir)),
            )
            .ok(),
            ProjectConfigContext::Local(None) | ProjectConfigContext::GlobalOnly => None,
            ProjectConfigContext::Accepted(snapshot) => snapshot.mcp_store().cloned(),
        })
    }

    /// Prepare and queue one guarded configuration mutation for a catalog
    /// project. `edit` receives the private edit base (accepted bytes
    /// overlaid with this path's outstanding intents) under the queue lock
    /// and returns the change, or `None` when there is nothing to change.
    /// The caller persists the queue durably before reporting success.
    pub(crate) fn prepare_project_config_mutation(
        &self,
        project_id: &str,
        target: &ProjectConfigTargetV1,
        reason: &str,
        edit: impl FnOnce(Option<&str>) -> anyhow::Result<Option<ProjectConfigEdit>>,
    ) -> anyhow::Result<Option<ProjectConfigMutationReceipt>> {
        self.prepare_project_config_mutation_with_snapshot_hook(
            project_id,
            target,
            reason,
            || {},
            edit,
        )
    }

    pub(crate) fn prepare_project_config_mutation_with_snapshot_hook(
        &self,
        project_id: &str,
        target: &ProjectConfigTargetV1,
        reason: &str,
        mut after_snapshot: impl FnMut(),
        edit: impl FnOnce(Option<&str>) -> anyhow::Result<Option<ProjectConfigEdit>>,
    ) -> anyhow::Result<Option<ProjectConfigMutationReceipt>> {
        anyhow::ensure!(
            !self.project_authority.is_bridge(),
            "error.project_config_lane_catalog_only: bridge mode writes project configuration in its own checkout"
        );
        // Base selection, transformation and enqueue share one queue lock,
        // and the accepted generation and scope are re-read under it: an
        // edit is never computed from a publication that moved underneath.
        let mut captured = None;
        for _ in 0..4 {
            let accepted = self.load_accepted_project_config(project_id)?;
            after_snapshot();
            let queue = self.checkout_mutations.write();
            anyhow::ensure!(
                self.catalog_published_scope(project_id).as_ref() == Some(&accepted.scope),
                "error.project_config_scope_changed: the project's published scope changed; retry after scope reconciliation"
            );
            let current = self.load_accepted_project_config(project_id)?;
            if current.stamp == accepted.stamp {
                captured = Some((accepted, queue));
                break;
            }
        }
        let (accepted, mut queue) = captured.ok_or_else(|| {
            anyhow::anyhow!(
                "error.checkout_publication_busy: accepted publication changed repeatedly; retry the configuration edit"
            )
        })?;
        let relative_path = target.relative_path();
        let published = accepted.snapshot.accepted_bytes(target);
        let base = queue.write_base(&accepted.scope, &relative_path, published)?;
        let content = match edit(base.as_deref())? {
            None => return Ok(None),
            Some(ProjectConfigEdit::Write(content))
                if base.as_deref() == Some(content.as_str()) =>
            {
                return Ok(None);
            }
            Some(ProjectConfigEdit::Write(content)) => Some(content),
            Some(ProjectConfigEdit::Delete) if base.is_none() => return Ok(None),
            Some(ProjectConfigEdit::Delete) => None,
        };
        let mutation = queue.enqueue_guarded(
            accepted.scope.clone(),
            relative_path.clone(),
            content,
            base.as_deref(),
            published.map(str::to_owned),
            reason.to_string(),
            bbox_util::util::now_iso(),
        )?;
        drop(queue);
        self.checkout_mutations_persister.request();
        let guard = mutation
            .guard
            .clone()
            .expect("enqueue_guarded always sets a guard");
        Ok(Some(ProjectConfigMutationReceipt {
            mutation_id: mutation.mutation_id,
            state: CheckoutMutationProgress::Queued,
            mode: mutation.mode,
            project_id: accepted.project_id,
            landing: ProjectConfigLanding::new(&accepted.scope, &relative_path),
            expected_sha256: guard.expected_sha256,
            predecessor: guard.predecessor,
            accepted_generation: accepted.stamp.generation_id().to_string(),
            next_step: COMMIT_TO_PUBLISH.to_string(),
        }))
    }

    /// Report one configuration mutation's state, first retiring whatever the
    /// current accepted publication already incorporates. Delivery is never
    /// reported as publication.
    pub(crate) fn project_config_mutation_status(
        &self,
        mutation_id: &str,
    ) -> anyhow::Result<ProjectConfigMutationStatus> {
        let (scope, relative_path) = {
            let queue = self.checkout_mutations.read();
            let row = queue.get(mutation_id).ok_or_else(|| {
                anyhow::anyhow!("error.checkout_mutation_unknown: no mutation {mutation_id}")
            })?;
            anyhow::ensure!(
                row.mutation.guard.is_some(),
                "error.checkout_mutation_unknown: {mutation_id} is not a project configuration mutation"
            );
            (
                row.mutation.scope.clone(),
                row.mutation.relative_path.clone(),
            )
        };
        let target = ProjectConfigTargetV1::from_relative_path(&relative_path)
            .ok_or_else(|| anyhow::anyhow!("queued configuration target is not recognized"))?;
        if let Some(project_id) = self.catalog_project_for_scope(&scope)
            && let Ok(accepted) = self.load_accepted_project_config(&project_id)
            && accepted.scope == scope
        {
            self.checkout_mutations.write().observe_publication(
                &scope,
                &relative_path,
                accepted.snapshot.accepted_bytes(&target),
            );
        }
        let queue = self.checkout_mutations.read();
        let row = queue.get(mutation_id).ok_or_else(|| {
            anyhow::anyhow!("error.checkout_mutation_unknown: no mutation {mutation_id}")
        })?;
        let state = queue
            .progress(mutation_id)
            .expect("the row was found above");
        let owner_collector_unsupported =
            state == CheckoutMutationProgress::Queued && row.owner_unsupported_at.is_some();
        let next_step = match state {
            CheckoutMutationProgress::Queued if owner_collector_unsupported => "The checkout owner's collector does not support guarded configuration mutations, so the mutation is withheld rather than delivered unguarded. Upgrade the collector; the mutation stays queued and delivers on its next poll".to_string(),
            CheckoutMutationProgress::Queued => COMMIT_TO_PUBLISH.to_string(),
            CheckoutMutationProgress::Delivered => "Applied in the owner's checkout but not yet published. Commit and publish the file on the project's publisher ref; reads and dispatch keep the accepted configuration until then".to_string(),
            CheckoutMutationProgress::Published => "Included in the accepted publication; reads and dispatch use it".to_string(),
            CheckoutMutationProgress::Conflicted => RECONCILE_AND_RETRY.to_string(),
            CheckoutMutationProgress::Blocked => format!(
                "Not delivered because predecessor {} did not apply. {RECONCILE_AND_RETRY}",
                row.blocked_by.as_deref().unwrap_or("unknown")
            ),
            CheckoutMutationProgress::Failed => "The owner could not apply it and left its bytes unchanged. Resolve the reported cause in the owning checkout, then re-issue the edit".to_string(),
        };
        Ok(ProjectConfigMutationStatus {
            mutation_id: mutation_id.to_string(),
            state,
            mode: row.mutation.mode.clone(),
            landing: ProjectConfigLanding::new(&scope, &relative_path),
            observed_sha256: row
                .conflict
                .as_ref()
                .map(|conflict| conflict.observed_sha256.clone()),
            blocked_by: row.blocked_by.clone(),
            owner_collector_unsupported,
            owner_error: row
                .last_error
                .clone()
                .filter(|_| state == CheckoutMutationProgress::Failed),
            next_step,
        })
    }
}

#[cfg(test)]
#[path = "project_config_tests.rs"]
mod tests;
