//! Shared producer authentication and catalog-scoped transport authority.
//!
//! Code collection and the typed Git/provenance transport deliberately use
//! one credential table. This module owns bearer verification, scope
//! resolution, and whole-repository grant derivation; lane runtimes own only
//! their stores and handlers.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use axum::Json;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use bbox_code_source::{ErrorResponse, validate_producer_id, validate_scope};
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::project_catalog::{
    CatalogSnapshotV2, CommitNamespace, ProjectId, ProjectScope, RepoHistoryId,
};
use bbox_corpus_core::project_record::ProjectRecord;
use bbox_indexing::checkout_access::{
    CheckoutAccessBroker, CheckoutAccessIntent, CheckoutAccessKind, CheckoutAccessRequest,
    CheckoutAccessSourceLane, CheckoutAttachmentSelector,
};
use bro_rpc::ServiceToken;
use sha2::{Digest, Sha256};

use super::SharedState;

#[derive(Clone)]
pub(crate) struct ProducerGrant {
    pub(crate) producer_id: String,
    pub(crate) projects: BTreeMap<PublishedScope, String>,
}

#[derive(Clone)]
struct AuthEntry {
    token: ServiceToken,
    grant: ProducerGrant,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RepoTransportMember {
    pub(crate) project_id: ProjectId,
    pub(crate) scope: PublishedScope,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RepoTransportGrant {
    pub(crate) producer_id: String,
    pub(crate) authority_scope: PublishedScope,
    pub(crate) repo_history_id: RepoHistoryId,
    pub(crate) primary_namespace: CommitNamespace,
    pub(crate) members: Vec<RepoTransportMember>,
    pub(crate) commitment: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RepoTransportGrantState {
    Granted(RepoTransportGrant),
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RepoTransportGrantError {
    ScopeForbidden,
    RepoHistoryNotFound,
    RepoHistoryScopeSplit,
}

impl RepoTransportGrantError {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::ScopeForbidden => "scope_forbidden",
            Self::RepoHistoryNotFound => "repo_history_not_found",
            Self::RepoHistoryScopeSplit => "repo_history_scope_split",
        }
    }
}

/// Immutable replacement snapshot. `CodeSourceRuntime` swaps this only after
/// every token, scope, and whole-repo relationship validates.
pub(crate) struct ProducerAuthRuntime {
    enabled: bool,
    git_transport_enabled: bool,
    entries: Vec<AuthEntry>,
    scope_to_project: BTreeMap<PublishedScope, ProjectId>,
    #[cfg(test)]
    producer_to_scopes: BTreeMap<String, BTreeSet<PublishedScope>>,
    project_to_repo_history: BTreeMap<ProjectId, RepoHistoryId>,
    repo_grants: BTreeMap<RepoHistoryId, RepoTransportGrantState>,
    #[cfg(test)]
    catalog_mode: bool,
}

/// How configured scopes resolve during one replacement build.
pub(crate) enum GrantScopeResolution {
    Bridge {
        project_scopes: Vec<(String, Option<PublishedScope>)>,
    },
    Catalog {
        catalog: Arc<CatalogSnapshotV2>,
    },
}

impl ProducerAuthRuntime {
    pub(crate) fn build(
        config: &crate::config::Config,
        projects: &[ProjectRecord],
        catalog_store: Option<&Arc<bbox_indexing::project_catalog_store::ProjectCatalogStore>>,
        checkout_access: &CheckoutAccessBroker,
    ) -> Result<Self> {
        if config.code_collection.git_transport_enabled && !config.code_collection.enabled {
            bail!("Git transport requires code collection to be enabled");
        }
        if config.code_collection.git_transport_enabled
            && (config.code_collection.max_git_history_commits == 0
                || config.code_collection.max_git_history_logical_bytes == 0
                || config.code_collection.max_provenance_documents == 0
                || config.code_collection.max_provenance_logical_bytes == 0)
        {
            bail!("Git transport limits must be nonzero");
        }
        if !config.code_collection.enabled {
            return Ok(Self::disabled());
        }
        if config.code_collection.producers.is_empty() {
            bail!("enabled code collection requires at least one producer");
        }

        let resolution = match catalog_store {
            Some(store) => GrantScopeResolution::Catalog {
                catalog: store
                    .snapshot()
                    .map_err(|error| anyhow::anyhow!("catalog snapshot unavailable: {error}"))?
                    .catalog()
                    .clone(),
            },
            None => GrantScopeResolution::Bridge {
                project_scopes: projects
                    .iter()
                    .map(|project| {
                        let lease = checkout_access
                            .acquire(CheckoutAccessRequest {
                                project_id: project.project_id.clone(),
                                attachment: CheckoutAttachmentSelector::Selected,
                                expected_scope: None,
                                kind: CheckoutAccessKind::PublisherConfigTreeRead,
                                intent: CheckoutAccessIntent::Read,
                                source_lane: CheckoutAccessSourceLane::LegacyProjectRecord,
                            })
                            .map_err(anyhow::Error::new)?;
                        let scope = lease.published_scope().cloned();
                        checkout_access
                            .revalidate(&lease)
                            .map_err(anyhow::Error::new)?;
                        Ok::<_, anyhow::Error>((project.project_id.clone(), scope))
                    })
                    .collect::<Result<Vec<_>>>()?,
            },
        };
        if config.code_collection.git_transport_enabled
            && matches!(resolution, GrantScopeResolution::Bridge { .. })
        {
            bail!("Git transport requires catalog project authority");
        }

        let mut entries = Vec::new();
        let mut scope_to_project = BTreeMap::new();
        #[cfg(test)]
        let mut producer_to_scopes = BTreeMap::new();
        let mut producer_ids = BTreeSet::new();
        let mut token_digests = BTreeSet::new();
        let mut assigned_scopes = BTreeSet::new();

        for producer in &config.code_collection.producers {
            validate_producer_id(&producer.producer_id)?;
            if !producer_ids.insert(producer.producer_id.clone()) {
                bail!("duplicate code-collection producer id");
            }
            let token = ServiceToken::load(&producer.token_file).with_context(|| {
                format!("loading code-collection token for {}", producer.producer_id)
            })?;
            let token_digest = Sha256::digest(token.expose_secret().as_bytes());
            if !token_digests.insert(token_digest.to_vec()) {
                bail!("code-collection token values must be unique");
            }
            if producer.scopes.is_empty() {
                bail!("enabled code-collection producer has no scopes");
            }

            let mut resolved = BTreeMap::new();
            for scope in &producer.scopes {
                validate_scope(scope)?;
                if !assigned_scopes.insert(scope.clone()) {
                    bail!("code-collection scope is assigned more than once");
                }
                let project_id = resolve_grant_scope(&resolution, scope)?;
                if let GrantScopeResolution::Catalog { catalog } = &resolution {
                    scope_to_project
                        .insert(scope.clone(), resolve_catalog_project(catalog, scope)?);
                }
                resolved.insert(scope.clone(), project_id);
            }
            #[cfg(test)]
            producer_to_scopes.insert(
                producer.producer_id.clone(),
                producer.scopes.iter().cloned().collect(),
            );
            entries.push(AuthEntry {
                token,
                grant: ProducerGrant {
                    producer_id: producer.producer_id.clone(),
                    projects: resolved,
                },
            });
        }

        let (project_to_repo_history, repo_grants) = match &resolution {
            GrantScopeResolution::Catalog { catalog } => {
                derive_repo_transport_grants(catalog, &entries)
            }
            GrantScopeResolution::Bridge { .. } => (BTreeMap::new(), BTreeMap::new()),
        };

        Ok(Self {
            enabled: true,
            git_transport_enabled: config.code_collection.git_transport_enabled,
            entries,
            scope_to_project,
            #[cfg(test)]
            producer_to_scopes,
            project_to_repo_history,
            repo_grants,
            #[cfg(test)]
            catalog_mode: matches!(resolution, GrantScopeResolution::Catalog { .. }),
        })
    }

    pub(crate) fn disabled() -> Self {
        Self {
            enabled: false,
            git_transport_enabled: false,
            entries: Vec::new(),
            scope_to_project: BTreeMap::new(),
            #[cfg(test)]
            producer_to_scopes: BTreeMap::new(),
            project_to_repo_history: BTreeMap::new(),
            repo_grants: BTreeMap::new(),
            #[cfg(test)]
            catalog_mode: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        enabled: bool,
        git_transport_enabled: bool,
        entries: Vec<(ServiceToken, ProducerGrant)>,
    ) -> Self {
        Self {
            enabled,
            git_transport_enabled,
            entries: entries
                .into_iter()
                .map(|(token, grant)| AuthEntry { token, grant })
                .collect(),
            scope_to_project: BTreeMap::new(),
            producer_to_scopes: BTreeMap::new(),
            project_to_repo_history: BTreeMap::new(),
            repo_grants: BTreeMap::new(),
            catalog_mode: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test_catalog(
        entries: Vec<(ServiceToken, ProducerGrant)>,
        catalog: &CatalogSnapshotV2,
    ) -> Self {
        let entries = entries
            .into_iter()
            .map(|(token, grant)| AuthEntry { token, grant })
            .collect::<Vec<_>>();
        let scope_to_project = entries
            .iter()
            .flat_map(|entry| entry.grant.projects.keys())
            .map(|scope| {
                (
                    scope.clone(),
                    resolve_catalog_project(catalog, scope).expect("test catalog scope"),
                )
            })
            .collect();
        let producer_to_scopes = entries
            .iter()
            .map(|entry| {
                (
                    entry.grant.producer_id.clone(),
                    entry.grant.projects.keys().cloned().collect(),
                )
            })
            .collect();
        let (project_to_repo_history, repo_grants) =
            derive_repo_transport_grants(catalog, &entries);
        Self {
            enabled: true,
            git_transport_enabled: true,
            entries,
            scope_to_project,
            producer_to_scopes,
            project_to_repo_history,
            repo_grants,
            catalog_mode: true,
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    pub(crate) fn git_transport_enabled(&self) -> bool {
        self.git_transport_enabled
    }

    pub(crate) fn authenticate(&self, candidate: &str) -> Option<ProducerGrant> {
        if !self.enabled {
            return None;
        }
        let mut matched = None;
        for entry in &self.entries {
            if entry.token.verify(candidate) {
                matched = Some(entry.grant.clone());
            }
        }
        matched
    }

    pub(crate) fn assignments(&self) -> Vec<(PublishedScope, String)> {
        self.entries
            .iter()
            .flat_map(|entry| entry.grant.projects.clone())
            .collect()
    }

    pub(crate) fn assignment_map(&self) -> BTreeMap<PublishedScope, (String, String)> {
        self.entries
            .iter()
            .flat_map(|entry| {
                entry.grant.projects.iter().map(|(scope, project_id)| {
                    (
                        scope.clone(),
                        (project_id.clone(), entry.grant.producer_id.clone()),
                    )
                })
            })
            .collect()
    }

    pub(crate) fn assigned_project_ids(&self) -> BTreeSet<String> {
        self.entries
            .iter()
            .flat_map(|entry| entry.grant.projects.values().cloned())
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn scope_project(&self, scope: &PublishedScope) -> Option<&ProjectId> {
        self.scope_to_project.get(scope)
    }

    #[cfg(test)]
    pub(crate) fn producer_scopes(&self, producer_id: &str) -> Option<&BTreeSet<PublishedScope>> {
        self.producer_to_scopes.get(producer_id)
    }

    #[cfg(test)]
    pub(crate) fn is_catalog_mode(&self) -> bool {
        self.catalog_mode
    }

    pub(crate) fn repo_transport_grant(
        &self,
        grant: &ProducerGrant,
        scope: &PublishedScope,
    ) -> std::result::Result<&RepoTransportGrant, RepoTransportGrantError> {
        let project_id = self
            .scope_to_project
            .get(scope)
            .filter(|project_id| {
                grant
                    .projects
                    .get(scope)
                    .is_some_and(|configured| configured == project_id.as_str())
            })
            .ok_or(RepoTransportGrantError::ScopeForbidden)?;
        let repo_history_id = self
            .project_to_repo_history
            .get(project_id)
            .ok_or(RepoTransportGrantError::RepoHistoryNotFound)?;
        match self.repo_grants.get(repo_history_id) {
            Some(RepoTransportGrantState::Granted(repo_grant))
                if repo_grant.producer_id == grant.producer_id =>
            {
                Ok(repo_grant)
            }
            _ => Err(RepoTransportGrantError::RepoHistoryScopeSplit),
        }
    }

    /// Resolve one authenticated project assignment without requiring every
    /// published member of its repository to belong to this producer. Git
    /// history is repository-wide; provenance export is intentionally scoped
    /// to exactly one project.
    pub(crate) fn project_transport_grant(
        &self,
        grant: &ProducerGrant,
        scope: &PublishedScope,
    ) -> std::result::Result<&ProjectId, RepoTransportGrantError> {
        self.scope_to_project
            .get(scope)
            .filter(|project_id| {
                grant
                    .projects
                    .get(scope)
                    .is_some_and(|configured| configured == project_id.as_str())
            })
            .ok_or(RepoTransportGrantError::ScopeForbidden)
    }

    pub(crate) fn repo_transport_grant_for_id(
        &self,
        producer_id: &str,
        repo_history_id: &RepoHistoryId,
    ) -> std::result::Result<&RepoTransportGrant, RepoTransportGrantError> {
        match self.repo_grants.get(repo_history_id) {
            Some(RepoTransportGrantState::Granted(grant)) if grant.producer_id == producer_id => {
                Ok(grant)
            }
            Some(_) => Err(RepoTransportGrantError::RepoHistoryScopeSplit),
            None => Err(RepoTransportGrantError::RepoHistoryNotFound),
        }
    }
}

pub(crate) fn resolve_grant_scope(
    resolution: &GrantScopeResolution,
    scope: &PublishedScope,
) -> Result<String> {
    match resolution {
        GrantScopeResolution::Bridge { project_scopes } => {
            let matching: Vec<&str> = project_scopes
                .iter()
                .filter(|(_, project_scope)| project_scope.as_ref() == Some(scope))
                .map(|(project_id, _)| project_id.as_str())
                .collect();
            let [project_id] = matching.as_slice() else {
                if matching.is_empty() {
                    bail!("code-collection scope is not registered");
                }
                bail!("code-collection scope resolves to multiple registered projects");
            };
            Ok((*project_id).to_string())
        }
        GrantScopeResolution::Catalog { catalog } => Ok(resolve_catalog_project(catalog, scope)?
            .as_str()
            .to_string()),
    }
}

pub(crate) fn resolve_catalog_project(
    catalog: &CatalogSnapshotV2,
    scope: &PublishedScope,
) -> Result<ProjectId> {
    let matching: Vec<&ProjectId> = catalog
        .projects
        .iter()
        .filter(|(_, project)| match &project.scope {
            ProjectScope::Published(published) => published == scope,
            ProjectScope::LegacyLocal => false,
        })
        .map(|(project_id, _)| project_id)
        .collect();
    let [project_id] = matching.as_slice() else {
        if matching.is_empty() {
            bail!("code-collection scope is not registered");
        }
        bail!("code-collection scope resolves to multiple registered projects");
    };
    Ok((*project_id).clone())
}

fn derive_repo_transport_grants(
    catalog: &CatalogSnapshotV2,
    entries: &[AuthEntry],
) -> (
    BTreeMap<ProjectId, RepoHistoryId>,
    BTreeMap<RepoHistoryId, RepoTransportGrantState>,
) {
    let assignments = entries
        .iter()
        .flat_map(|entry| {
            entry
                .grant
                .projects
                .keys()
                .map(|scope| (scope.clone(), entry.grant.producer_id.clone()))
        })
        .collect::<BTreeMap<_, _>>();
    let mut project_to_repo_history = BTreeMap::new();
    let mut members_by_history: BTreeMap<RepoHistoryId, Vec<RepoTransportMember>> = BTreeMap::new();
    for (project_id, project) in &catalog.projects {
        let (ProjectScope::Published(scope), Some(repo_history_id)) =
            (&project.scope, &project.repo_history)
        else {
            continue;
        };
        project_to_repo_history.insert(project_id.clone(), repo_history_id.clone());
        members_by_history
            .entry(repo_history_id.clone())
            .or_default()
            .push(RepoTransportMember {
                project_id: project_id.clone(),
                scope: scope.clone(),
            });
    }

    let mut grants = BTreeMap::new();
    for (repo_history_id, mut members) in members_by_history {
        members.sort_by(|left, right| left.project_id.cmp(&right.project_id));
        let producers = members
            .iter()
            .filter_map(|member| assignments.get(&member.scope).cloned())
            .collect::<BTreeSet<_>>();
        let complete = members
            .iter()
            .all(|member| assignments.contains_key(&member.scope));
        let Some(history) = catalog.repo_histories.get(&repo_history_id) else {
            grants.insert(repo_history_id, RepoTransportGrantState::Blocked);
            continue;
        };
        if producers.len() != 1 {
            grants.insert(repo_history_id, RepoTransportGrantState::Blocked);
            continue;
        }
        if !complete {
            grants.insert(repo_history_id, RepoTransportGrantState::Blocked);
            continue;
        }
        let producer_id = producers.into_iter().next().expect("length checked above");
        // The capture authority is the shallowest registered scope, not the
        // first project id. Project-id order is unrelated to monorepo
        // topology and could otherwise make adding or renaming a member
        // silently change the source descriptor expected by the server.
        let authority_scope = members
            .iter()
            .min_by(|left, right| {
                let left_depth = scope_depth(&left.scope);
                let right_depth = scope_depth(&right.scope);
                left_depth
                    .cmp(&right_depth)
                    .then_with(|| left.scope.cmp(&right.scope))
                    .then_with(|| left.project_id.cmp(&right.project_id))
            })
            .expect("repo transport groups are nonempty")
            .scope
            .clone();
        let commitment = repo_grant_commitment(
            &producer_id,
            &repo_history_id,
            &history.primary_namespace,
            &members,
        );
        grants.insert(
            repo_history_id.clone(),
            RepoTransportGrantState::Granted(RepoTransportGrant {
                producer_id,
                authority_scope,
                repo_history_id,
                primary_namespace: history.primary_namespace.clone(),
                members,
                commitment,
            }),
        );
    }
    (project_to_repo_history, grants)
}

fn scope_depth(scope: &PublishedScope) -> usize {
    let relative = scope.bbox_root_relpath();
    if relative == "." {
        0
    } else {
        relative.split('/').count()
    }
}

fn repo_grant_commitment(
    producer_id: &str,
    repo_history_id: &RepoHistoryId,
    primary_namespace: &CommitNamespace,
    members: &[RepoTransportMember],
) -> String {
    let mut hasher = Sha256::new();
    hash_field(&mut hasher, b"bbox-repo-transport-grant-v1");
    hash_field(&mut hasher, producer_id.as_bytes());
    hash_field(&mut hasher, repo_history_id.as_str().as_bytes());
    hash_field(&mut hasher, primary_namespace.as_str().as_bytes());
    for member in members {
        hash_field(&mut hasher, member.project_id.as_str().as_bytes());
        hash_field(&mut hasher, member.scope.repo_id().as_bytes());
        hash_field(&mut hasher, member.scope.bbox_root_relpath().as_bytes());
    }
    hex::encode(hasher.finalize())
}

fn hash_field(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

pub(crate) async fn authenticate_code_source_request(
    State(state): State<Arc<SharedState>>,
    request: Request,
    next: Next,
) -> Response {
    authenticate_request(state, request, next, false).await
}

pub(crate) async fn authenticate_git_source_request(
    State(state): State<Arc<SharedState>>,
    request: Request,
    next: Next,
) -> Response {
    authenticate_request(state, request, next, true).await
}

async fn authenticate_request(
    state: Arc<SharedState>,
    mut request: Request,
    next: Next,
    require_git_transport: bool,
) -> Response {
    let auth = state.code_sources.producer_auth();
    let enabled = if require_git_transport {
        auth.git_transport_enabled()
    } else {
        auth.enabled()
    };
    if !enabled {
        let (code, message) = if require_git_transport {
            ("git_transport_disabled", "Git transport is disabled")
        } else {
            ("service_disabled", "code collection is disabled")
        };
        return error_response(StatusCode::SERVICE_UNAVAILABLE, code, message);
    }
    let candidate = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let Some(grant) = candidate.and_then(|value| auth.authenticate(value)) else {
        return error_response(StatusCode::UNAUTHORIZED, "unauthorized", "unauthorized");
    };
    request.extensions_mut().insert(grant);
    next.run(request).await
}

fn error_response(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(ErrorResponse {
            code: code.to_string(),
            message: message.to_string(),
        }),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bbox_corpus_core::project_catalog::{
        CorpusProject, RecordedRepoAuthority, RepoHistoryAuthority, RepoHistoryMaterialization,
        RepoHistoryRecord,
    };

    fn project(
        project_id: &str,
        scope: ProjectScope,
        repo_history_id: &RepoHistoryId,
    ) -> CorpusProject {
        let project_id = ProjectId::parse(project_id).unwrap();
        CorpusProject {
            project_id: project_id.clone(),
            scope,
            operator_aliases: BTreeSet::new(),
            nominated_aliases: BTreeSet::new(),
            display_name: project_id.as_str().to_string(),
            created_at: "2026-08-08T00:00:00Z".into(),
            registered_at_compat: None,
            repo_history: Some(repo_history_id.clone()),
            languages: BTreeSet::new(),
        }
    }

    fn catalog_with_two_published_members() -> (
        CatalogSnapshotV2,
        RepoHistoryId,
        PublishedScope,
        PublishedScope,
    ) {
        let repo_history_id = RepoHistoryId::parse("rh_00000000000000000000000000000001").unwrap();
        let root_scope = PublishedScope::try_new("repo-a", ".").unwrap();
        let child_scope = PublishedScope::try_new("repo-a", "child").unwrap();
        let mut catalog = CatalogSnapshotV2::empty(1).unwrap();
        catalog.repo_histories.insert(
            repo_history_id.clone(),
            RepoHistoryRecord {
                repo_history_id: repo_history_id.clone(),
                authority: RepoHistoryAuthority::Recorded(
                    RecordedRepoAuthority::parse("repo-a").unwrap(),
                ),
                primary_namespace: CommitNamespace::parse("repo-a").unwrap(),
                compatibility_namespaces: BTreeSet::new(),
                materialization: RepoHistoryMaterialization::NotBuilt,
            },
        );
        for (id, scope) in [
            ("p_00000000000000000000000000000002", root_scope.clone()),
            ("p_00000000000000000000000000000001", child_scope.clone()),
        ] {
            let project = project(id, ProjectScope::Published(scope), &repo_history_id);
            catalog.projects.insert(project.project_id.clone(), project);
        }
        catalog.validate().unwrap();
        (catalog, repo_history_id, root_scope, child_scope)
    }

    fn entry(producer_id: &str, scopes: &[(PublishedScope, &str)]) -> AuthEntry {
        AuthEntry {
            token: ServiceToken::parse("a".repeat(64)).unwrap(),
            grant: ProducerGrant {
                producer_id: producer_id.into(),
                projects: scopes
                    .iter()
                    .map(|(scope, project_id)| (scope.clone(), (*project_id).to_string()))
                    .collect(),
            },
        }
    }

    #[test]
    fn repo_grant_requires_every_published_member_on_one_producer() {
        let (catalog, history_id, root_scope, child_scope) = catalog_with_two_published_members();
        let root_id = "p_00000000000000000000000000000002";
        let child_id = "p_00000000000000000000000000000001";

        let (_, grants) = derive_repo_transport_grants(
            &catalog,
            &[entry(
                "producer-a",
                &[
                    (root_scope.clone(), root_id),
                    (child_scope.clone(), child_id),
                ],
            )],
        );
        let RepoTransportGrantState::Granted(grant) = &grants[&history_id] else {
            panic!("complete same-producer membership must grant repo transport")
        };
        assert_eq!(grant.producer_id, "producer-a");
        assert_eq!(grant.members.len(), 2);
        assert_eq!(grant.authority_scope, root_scope);
        assert_eq!(grant.commitment.len(), 64);

        let (_, missing) = derive_repo_transport_grants(
            &catalog,
            &[entry("producer-a", &[(root_scope.clone(), root_id)])],
        );
        assert_eq!(missing[&history_id], RepoTransportGrantState::Blocked);

        let (_, split) = derive_repo_transport_grants(
            &catalog,
            &[
                entry("producer-a", &[(root_scope, root_id)]),
                entry("producer-b", &[(child_scope, child_id)]),
            ],
        );
        assert_eq!(split[&history_id], RepoTransportGrantState::Blocked);
    }
}
