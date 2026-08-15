//! Shared producer authentication and catalog-scoped transport authority.
//!
//! Code collection and the typed Git/provenance transport deliberately use
//! one credential table. This module owns bearer verification, scope
//! resolution, and whole-repository grant derivation; lane runtimes own only
//! their stores and handlers.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result, bail};
use axum::Json;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use bbox_code_source::{ErrorResponse, validate_producer_id, validate_scope};
pub(crate) use bbox_corpus_core::git_transport_cutover::RepoTransportGrant;
use bbox_corpus_core::git_transport_cutover::{
    RepoTransportGrantState, derive_repo_transport_grants,
};
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::project_catalog::{
    CatalogSnapshotV2, ProjectId, ProjectScope, RepoHistoryId,
};
use bbox_corpus_core::project_record::ProjectRecord;
use bbox_indexing::checkout_access::{
    CheckoutAccessBroker, CheckoutAccessIntent, CheckoutAccessKind, CheckoutAccessRequest,
    CheckoutAccessSourceLane, CheckoutAttachmentSelector,
};
use bro_rpc::ServiceTokenSet;
use sha2::{Digest, Sha256};

use super::SharedState;
use super::connector_grants::ConnectorGrantRuntime;

/// Sentinel stored in [`AuthEntry::last_matched_slot`] before this
/// producer's token set has ever verified a request this boot. Not a valid
/// slot index (a [`ServiceTokenSet`] cannot hold `usize::MAX` slots).
const NEVER_MATCHED: usize = usize::MAX;

#[derive(Clone)]
pub(crate) struct ProducerGrant {
    pub(crate) producer_id: String,
    pub(crate) projects: BTreeMap<PublishedScope, String>,
}

/// What a file-source request proved about itself.
///
/// Only the producer id, deliberately. The code lane's [`ProducerGrant`]
/// carries a resolved scope-to-project map because its handlers need the
/// project for a published scope on every call; the connector lane resolves
/// scope authorization against the live grant table instead, so that a scope
/// promoted out of pending-onboarding mid-flight is visible immediately rather
/// than frozen into whatever the request's first middleware hop captured.
#[derive(Clone)]
pub(crate) struct ConnectorGrant {
    pub(crate) producer_id: String,
}

#[derive(Clone)]
struct AuthEntry {
    tokens: ServiceTokenSet,
    grant: ProducerGrant,
    /// Slot index of the LAST successful verification against this
    /// producer's token set, this boot (`NEVER_MATCHED` before the first
    /// one). Rotation observability only: an operator watching this move
    /// off slot 0 toward a higher index knows the fleet has migrated onto a
    /// staged token and the retired slot is safe to remove. Never carries
    /// token material. Reset on every grant-table rebuild, which is the
    /// correct behavior: a reloaded config is a fresh window to observe.
    last_matched_slot: Arc<AtomicUsize>,
}

/// One producer's token-rotation observability snapshot: how many slots are
/// staged and which one last verified a request. Never carries token
/// material -- `matched_slot` is an index, not a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProducerTokenRotationStatus {
    pub(crate) producer_id: String,
    pub(crate) slots: usize,
    pub(crate) matched_slot: Option<usize>,
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

/// A configured producer scope with no catalog project yet. Only the catalog
/// onboarding endpoint may use it; every publication lane keeps refusing it
/// until onboarding attaches the project.
#[derive(Debug, Clone, Copy)]
pub(crate) struct UnregisteredCatalogScope;

impl std::fmt::Display for UnregisteredCatalogScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("code-collection scope is pending onboarding")
    }
}

impl std::error::Error for UnregisteredCatalogScope {}

/// Immutable replacement snapshot. `CodeSourceRuntime` swaps this only after
/// every token, scope, and whole-repo relationship validates.
pub(crate) struct ProducerAuthRuntime {
    enabled: bool,
    git_transport_enabled: bool,
    knowledge_transport_enabled: bool,
    entries: Vec<AuthEntry>,
    scope_to_project: BTreeMap<PublishedScope, ProjectId>,
    /// Configured grant scopes with no catalog project yet. Startup admits
    /// them so onboarding can run; only the onboard endpoint may accept them.
    pending_onboard_scopes: BTreeSet<PublishedScope>,
    #[cfg(test)]
    producer_to_scopes: BTreeMap<String, BTreeSet<PublishedScope>>,
    project_to_repo_history: BTreeMap<ProjectId, RepoHistoryId>,
    repo_grants: BTreeMap<RepoHistoryId, RepoTransportGrantState>,
    /// Connector producer grants. A SEPARATE family riding the same
    /// rebuild: it is enabled independently of code collection, its scopes
    /// are never published scopes, and it appears in no publication view
    /// here (`assignments`, `assignment_map`, `assigned_project_ids` stay
    /// published-scope-only by construction).
    connectors: Arc<ConnectorGrantRuntime>,
    #[cfg(test)]
    catalog_mode: bool,
}

impl std::fmt::Debug for ProducerAuthRuntime {
    /// Hand-written so a bearer can never reach a panic message, a tracing
    /// field, or a test log through this type -- the same discipline
    /// `ConnectorGrantRuntime`'s own `Debug` follows. Producer ids, scope
    /// counts, and rotation-status metadata (slot count, matched slot
    /// index) are operator-facing; token values are unreachable through
    /// this rendering.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProducerAuthRuntime")
            .field("enabled", &self.enabled)
            .field("git_transport_enabled", &self.git_transport_enabled)
            .field(
                "knowledge_transport_enabled",
                &self.knowledge_transport_enabled,
            )
            .field("producers", &self.token_rotation_status())
            .finish()
    }
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
        // Connector grants are their own family: enabled independently of
        // code collection, so they are resolved BEFORE the code-collection
        // gates and survive the disabled early return below. A daemon may
        // legitimately run connector grants with code collection off.
        let connector_catalog = match catalog_store {
            Some(store) => Some(
                store
                    .snapshot()
                    .map_err(|error| anyhow::anyhow!("catalog snapshot unavailable: {error}"))?
                    .catalog()
                    .clone(),
            ),
            None => None,
        };
        let connectors = Arc::new(ConnectorGrantRuntime::build(
            &config.source_connectors,
            connector_catalog.as_deref(),
        )?);

        if config.code_collection.git_transport_enabled && !config.code_collection.enabled {
            bail!("Git transport requires code collection to be enabled");
        }
        if config.code_collection.knowledge_transport_enabled && !config.code_collection.enabled {
            bail!("knowledge transport requires code collection to be enabled");
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
            // Code collection off does not mean connector grants off.
            return Ok(Self::disabled_with(connectors));
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
        if config.code_collection.knowledge_transport_enabled
            && matches!(resolution, GrantScopeResolution::Bridge { .. })
        {
            bail!("knowledge transport requires catalog project authority");
        }

        let mut entries = Vec::new();
        let mut scope_to_project = BTreeMap::new();
        let mut pending_onboard_scopes = BTreeSet::new();
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
            let token_paths = producer.resolved_token_files().with_context(|| {
                format!(
                    "resolving code-collection token files for {}",
                    producer.producer_id
                )
            })?;
            let tokens = ServiceTokenSet::load(&token_paths).with_context(|| {
                format!(
                    "loading code-collection tokens for {}",
                    producer.producer_id
                )
            })?;
            for token in tokens.tokens() {
                let token_digest = Sha256::digest(token.expose_secret().as_bytes());
                if !token_digests.insert(token_digest.to_vec()) {
                    bail!("code-collection token values must be unique");
                }
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
                match resolve_grant_scope(&resolution, scope) {
                    Ok(project_id) => {
                        if let GrantScopeResolution::Catalog { catalog } = &resolution {
                            scope_to_project
                                .insert(scope.clone(), resolve_catalog_project(catalog, scope)?);
                        }
                        resolved.insert(scope.clone(), project_id);
                    }
                    Err(error) => {
                        // A catalog-mode scope with no project yet is pending
                        // onboarding: admit it at startup so the onboard
                        // endpoint can attach it, and keep it out of every
                        // publication lane until then.
                        let pending = matches!(resolution, GrantScopeResolution::Catalog { .. })
                            && error.downcast_ref::<UnregisteredCatalogScope>().is_some();
                        if pending {
                            pending_onboard_scopes.insert(scope.clone());
                            tracing::info!(
                                scope = %format_args!("{}/{}", scope.repo_id(), scope.bbox_root_relpath()),
                                "code-collection scope is pending onboarding"
                            );
                        } else {
                            return Err(error);
                        }
                    }
                }
            }
            #[cfg(test)]
            producer_to_scopes.insert(
                producer.producer_id.clone(),
                producer.scopes.iter().cloned().collect(),
            );
            entries.push(AuthEntry {
                tokens,
                last_matched_slot: Arc::new(AtomicUsize::new(NEVER_MATCHED)),
                grant: ProducerGrant {
                    producer_id: producer.producer_id.clone(),
                    projects: resolved,
                },
            });
        }

        let (project_to_repo_history, repo_grants) = match &resolution {
            GrantScopeResolution::Catalog { catalog } => {
                let projection =
                    derive_repo_transport_grants(catalog, &assignment_producers(&entries));
                (projection.project_to_repo_history, projection.grants)
            }
            GrantScopeResolution::Bridge { .. } => (BTreeMap::new(), BTreeMap::new()),
        };

        Ok(Self {
            enabled: true,
            git_transport_enabled: config.code_collection.git_transport_enabled,
            knowledge_transport_enabled: config.code_collection.knowledge_transport_enabled,
            entries,
            scope_to_project,
            pending_onboard_scopes,
            #[cfg(test)]
            producer_to_scopes,
            project_to_repo_history,
            repo_grants,
            connectors,
            #[cfg(test)]
            catalog_mode: matches!(resolution, GrantScopeResolution::Catalog { .. }),
        })
    }

    pub(crate) fn disabled() -> Self {
        Self::disabled_with(Arc::new(ConnectorGrantRuntime::disabled()))
    }

    fn disabled_with(connectors: Arc<ConnectorGrantRuntime>) -> Self {
        Self {
            enabled: false,
            git_transport_enabled: false,
            knowledge_transport_enabled: false,
            entries: Vec::new(),
            scope_to_project: BTreeMap::new(),
            pending_onboard_scopes: BTreeSet::new(),
            #[cfg(test)]
            producer_to_scopes: BTreeMap::new(),
            project_to_repo_history: BTreeMap::new(),
            repo_grants: BTreeMap::new(),
            connectors,
            #[cfg(test)]
            catalog_mode: false,
        }
    }

    /// The connector grant table, for onboarding and read surfaces.
    #[allow(dead_code)] // Phase-0 seam consumed by the phase-1 onboard endpoint.
    pub(crate) fn connectors(&self) -> &Arc<ConnectorGrantRuntime> {
        &self.connectors
    }

    /// A runtime whose ONLY authority is a connector grant table.
    ///
    /// The file-source middleware gates on `connectors.enabled()` alone, so
    /// the code-source half stays disabled: a connector test that also
    /// authorized published scopes would prove less, not more, because it
    /// could not tell a connector grant from a code grant leaking through.
    #[cfg(test)]
    pub(crate) fn for_test_connectors(connectors: Arc<ConnectorGrantRuntime>) -> Self {
        Self::disabled_with(connectors)
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        enabled: bool,
        git_transport_enabled: bool,
        entries: Vec<(bro_rpc::ServiceToken, ProducerGrant)>,
    ) -> Self {
        Self::for_test_rotating(
            enabled,
            git_transport_enabled,
            entries
                .into_iter()
                .map(|(token, grant)| (vec![token], grant))
                .collect(),
        )
    }

    /// Like [`Self::for_test`], but each producer stages an ORDERED list of
    /// tokens rather than exactly one, for rotation-overlap tests: index 0
    /// is the oldest accepted slot, matching the config-level
    /// `token_files` contract.
    #[cfg(test)]
    pub(crate) fn for_test_rotating(
        enabled: bool,
        git_transport_enabled: bool,
        entries: Vec<(Vec<bro_rpc::ServiceToken>, ProducerGrant)>,
    ) -> Self {
        Self {
            enabled,
            git_transport_enabled,
            knowledge_transport_enabled: git_transport_enabled,
            entries: entries
                .into_iter()
                .map(|(tokens, grant)| AuthEntry {
                    tokens: ServiceTokenSet::from_tokens(tokens)
                        .expect("test entries stage at least one token"),
                    last_matched_slot: Arc::new(AtomicUsize::new(NEVER_MATCHED)),
                    grant,
                })
                .collect(),
            scope_to_project: BTreeMap::new(),
            pending_onboard_scopes: BTreeSet::new(),
            producer_to_scopes: BTreeMap::new(),
            project_to_repo_history: BTreeMap::new(),
            repo_grants: BTreeMap::new(),
            connectors: Arc::new(ConnectorGrantRuntime::disabled()),
            catalog_mode: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test_catalog(
        entries: Vec<(bro_rpc::ServiceToken, ProducerGrant)>,
        catalog: &CatalogSnapshotV2,
    ) -> Self {
        let entries = entries
            .into_iter()
            .map(|(token, grant)| AuthEntry {
                tokens: ServiceTokenSet::from_tokens(vec![token])
                    .expect("test entries stage at least one token"),
                last_matched_slot: Arc::new(AtomicUsize::new(NEVER_MATCHED)),
                grant,
            })
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
        let projection = derive_repo_transport_grants(catalog, &assignment_producers(&entries));
        Self {
            enabled: true,
            git_transport_enabled: true,
            knowledge_transport_enabled: true,
            entries,
            scope_to_project,
            pending_onboard_scopes: BTreeSet::new(),
            producer_to_scopes,
            project_to_repo_history: projection.project_to_repo_history,
            repo_grants: projection.grants,
            connectors: Arc::new(ConnectorGrantRuntime::disabled()),
            catalog_mode: true,
        }
    }

    /// True when the scope is operator-configured but has no catalog project
    /// yet. Only the catalog onboarding endpoint may admit such a scope.
    pub(crate) fn is_pending_onboard_scope(&self, scope: &PublishedScope) -> bool {
        self.pending_onboard_scopes.contains(scope)
    }

    #[cfg(test)]
    pub(crate) fn for_test_with_pending(
        entries: Vec<(bro_rpc::ServiceToken, ProducerGrant)>,
        pending: BTreeSet<PublishedScope>,
    ) -> Self {
        let mut runtime = Self::for_test(true, false, entries);
        runtime.pending_onboard_scopes = pending;
        runtime
    }

    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    pub(crate) fn git_transport_enabled(&self) -> bool {
        self.git_transport_enabled
    }

    pub(crate) fn knowledge_transport_enabled(&self) -> bool {
        self.knowledge_transport_enabled
    }

    /// `verify` is constant time and checks every staged slot even after a
    /// match, and this loop checks EVERY configured producer even after a
    /// match, so the number of comparisons performed does not vary with
    /// which producer or slot presented the bearer. On a match, the
    /// matched slot is recorded for rotation observability (never the
    /// token itself) and logged with the producer id.
    pub(crate) fn authenticate(&self, candidate: &str) -> Option<ProducerGrant> {
        if !self.enabled {
            return None;
        }
        let mut matched = None;
        for entry in &self.entries {
            if let Some(slot) = entry.tokens.verify(candidate) {
                entry.last_matched_slot.store(slot, Ordering::Relaxed);
                tracing::info!(
                    producer_id = %entry.grant.producer_id,
                    matched_slot = slot,
                    total_slots = entry.tokens.len(),
                    "producer-grant token matched"
                );
                matched = Some(entry.grant.clone());
            }
        }
        matched
    }

    /// Per-producer token-rotation observability: how many slots are
    /// staged and which one last verified a request, this boot. Never
    /// returns token material. The seam an operator-facing status surface
    /// (a doctor section, an admin route) reads to see when the fleet has
    /// migrated off slot 0 and the old token is safe to retire.
    pub(crate) fn token_rotation_status(&self) -> Vec<ProducerTokenRotationStatus> {
        self.entries
            .iter()
            .map(|entry| {
                let slot = entry.last_matched_slot.load(Ordering::Relaxed);
                ProducerTokenRotationStatus {
                    producer_id: entry.grant.producer_id.clone(),
                    slots: entry.tokens.len(),
                    matched_slot: (slot != NEVER_MATCHED).then_some(slot),
                }
            })
            .collect()
    }

    pub(crate) fn assignments(&self) -> Vec<(PublishedScope, String)> {
        self.entries
            .iter()
            .flat_map(|entry| entry.grant.projects.clone())
            .collect()
    }

    /// The exact scope -> producer projection consumed by repository-wide
    /// transport grant commitments and cutover row classification. This is
    /// deliberately distinct from `assignments`, whose value is the resolved
    /// project id used by code-source planning.
    pub(crate) fn repo_assignment_producers(&self) -> BTreeMap<PublishedScope, String> {
        assignment_producers(&self.entries)
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
            Some(RepoTransportGrantState::Granted { grant: repo_grant })
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

    pub(crate) fn project_transport_grant_for_id(
        &self,
        producer_id: &str,
        scope: &PublishedScope,
    ) -> std::result::Result<&ProjectId, RepoTransportGrantError> {
        let grant = self
            .entries
            .iter()
            .find(|entry| entry.grant.producer_id == producer_id)
            .map(|entry| &entry.grant)
            .ok_or(RepoTransportGrantError::ScopeForbidden)?;
        self.project_transport_grant(grant, scope)
    }

    pub(crate) fn repo_transport_grant_for_id(
        &self,
        producer_id: &str,
        repo_history_id: &RepoHistoryId,
    ) -> std::result::Result<&RepoTransportGrant, RepoTransportGrantError> {
        match self.repo_grants.get(repo_history_id) {
            Some(RepoTransportGrantState::Granted { grant })
                if grant.producer_id == producer_id =>
            {
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
            // A published grant never resolves to either: one has no
            // committed authority, the other is a remote source whose
            // identity is not a published scope at all.
            ProjectScope::LegacyLocal | ProjectScope::Connector(_) => false,
        })
        .map(|(project_id, _)| project_id)
        .collect();
    let [project_id] = matching.as_slice() else {
        if matching.is_empty() {
            return Err(anyhow::Error::new(UnregisteredCatalogScope));
        }
        bail!("code-collection scope resolves to multiple registered projects");
    };
    Ok((*project_id).clone())
}

fn assignment_producers(entries: &[AuthEntry]) -> BTreeMap<PublishedScope, String> {
    entries
        .iter()
        .flat_map(|entry| {
            entry
                .grant
                .projects
                .keys()
                .map(|scope| (scope.clone(), entry.grant.producer_id.clone()))
        })
        .collect()
}

pub(crate) async fn authenticate_code_source_request(
    State(state): State<Arc<SharedState>>,
    request: Request,
    next: Next,
) -> Response {
    authenticate_request(state, request, next, ProducerAuthLane::Code).await
}

pub(crate) async fn authenticate_git_source_request(
    State(state): State<Arc<SharedState>>,
    request: Request,
    next: Next,
) -> Response {
    authenticate_request(state, request, next, ProducerAuthLane::Git).await
}

pub(crate) async fn authenticate_knowledge_source_request(
    State(state): State<Arc<SharedState>>,
    request: Request,
    next: Next,
) -> Response {
    authenticate_request(state, request, next, ProducerAuthLane::Knowledge).await
}

/// Authenticate one `/internal/file-source/v1/*` request.
///
/// A SEPARATE middleware rather than a fourth `ProducerAuthLane` arm, because
/// the connector family authenticates against a different table and inserts a
/// different extension type. Folding it into `authenticate_request` would mean
/// that function returning one of two grant types by lane, which is exactly the
/// optionality the connector store refused to bleed through its own key.
///
/// Enablement is the connector family's own flag: a daemon may legitimately run
/// connector grants with code collection switched off, and
/// `ConnectorGrantRuntime::build` resolves connectors before the
/// code-collection early return precisely so that stays true here.
pub(crate) async fn authenticate_file_source_request(
    State(state): State<Arc<SharedState>>,
    request: Request,
    next: Next,
) -> Response {
    authenticate_connector_request(state, request, next).await
}

/// The conversation lane authenticates through the SAME producer table.
///
/// A separate entry point rather than a shared route layer because the two
/// families mount separately and a reader following either router must land on
/// a name that says which lane it is reading. Which lane a producer's grant
/// actually opens is checked per handler against
/// `ConnectorGrantRuntime::profile_for`; authentication answers only "which
/// producer is this bearer", which is lane-independent by construction.
pub(crate) async fn authenticate_conversation_source_request(
    State(state): State<Arc<SharedState>>,
    request: Request,
    next: Next,
) -> Response {
    authenticate_connector_request(state, request, next).await
}

async fn authenticate_connector_request(
    state: Arc<SharedState>,
    mut request: Request,
    next: Next,
) -> Response {
    let connectors = state.code_sources.producer_auth().connectors().clone();
    if !connectors.enabled() {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "service_disabled",
            "source connectors are disabled",
        );
    }
    let candidate = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    // `authenticate` is constant time and checks EVERY configured producer
    // even after a match, so the number of comparisons does not vary with
    // which producer presented the bearer.
    let Some(producer_id) = candidate.and_then(|value| connectors.authenticate(value)) else {
        return error_response(StatusCode::UNAUTHORIZED, "unauthorized", "unauthorized");
    };
    let grant = ConnectorGrant {
        producer_id: producer_id.to_string(),
    };
    request.extensions_mut().insert(grant);
    next.run(request).await
}

#[derive(Clone, Copy)]
enum ProducerAuthLane {
    Code,
    Git,
    Knowledge,
}

async fn authenticate_request(
    state: Arc<SharedState>,
    mut request: Request,
    next: Next,
    lane: ProducerAuthLane,
) -> Response {
    let auth = state.code_sources.producer_auth();
    let enabled = match lane {
        ProducerAuthLane::Code => auth.enabled(),
        ProducerAuthLane::Git => auth.git_transport_enabled(),
        ProducerAuthLane::Knowledge => auth.knowledge_transport_enabled(),
    };
    if !enabled {
        let (code, message) = match lane {
            ProducerAuthLane::Code => ("service_disabled", "code collection is disabled"),
            ProducerAuthLane::Git => ("git_transport_disabled", "Git transport is disabled"),
            ProducerAuthLane::Knowledge => (
                "knowledge_transport_disabled",
                "knowledge transport is disabled",
            ),
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
        CommitNamespace, CorpusProject, RecordedRepoAuthority, RepoHistoryAuthority,
        RepoHistoryMaterialization, RepoHistoryRecord,
    };
    use bro_rpc::ServiceToken;

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
                membership_generation: 0,
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
            tokens: ServiceTokenSet::from_tokens(vec![
                ServiceToken::parse("a".repeat(64)).unwrap(),
            ])
            .unwrap(),
            last_matched_slot: Arc::new(AtomicUsize::new(NEVER_MATCHED)),
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

        let grants = derive_repo_transport_grants(
            &catalog,
            &assignment_producers(&[entry(
                "producer-a",
                &[
                    (root_scope.clone(), root_id),
                    (child_scope.clone(), child_id),
                ],
            )]),
        )
        .grants;
        let RepoTransportGrantState::Granted { grant } = &grants[&history_id] else {
            panic!("complete same-producer membership must grant repo transport")
        };
        assert_eq!(grant.producer_id, "producer-a");
        assert_eq!(grant.members.len(), 2);
        assert_eq!(grant.authority_scope, root_scope);
        assert_eq!(grant.commitment.len(), 64);

        let missing = derive_repo_transport_grants(
            &catalog,
            &assignment_producers(&[entry("producer-a", &[(root_scope.clone(), root_id)])]),
        )
        .grants;
        assert!(matches!(
            missing[&history_id],
            RepoTransportGrantState::Blocked { .. }
        ));

        let split = derive_repo_transport_grants(
            &catalog,
            &assignment_producers(&[
                entry("producer-a", &[(root_scope, root_id)]),
                entry("producer-b", &[(child_scope, child_id)]),
            ]),
        )
        .grants;
        assert!(matches!(
            split[&history_id],
            RepoTransportGrantState::Blocked { .. }
        ));
    }

    #[test]
    fn runtime_exposes_project_and_producer_assignment_views_separately() {
        let scope = PublishedScope::try_new("repo-a", ".").unwrap();
        let auth = ProducerAuthRuntime::for_test(
            true,
            true,
            vec![(
                ServiceToken::parse("a".repeat(64)).unwrap(),
                ProducerGrant {
                    producer_id: "producer-a".into(),
                    projects: BTreeMap::from([(scope.clone(), "project-a".into())]),
                },
            )],
        );

        assert_eq!(
            auth.assignments(),
            vec![(scope.clone(), "project-a".into())],
            "code-source planning consumes resolved project ids"
        );
        assert_eq!(
            auth.repo_assignment_producers(),
            BTreeMap::from([(scope, "producer-a".into())]),
            "transport commitments consume producer ids"
        );
    }

    #[test]
    fn a_connector_project_never_enters_a_published_publication_view() {
        use bbox_corpus_core::project_catalog::ConnectorScope;

        let published_scope = PublishedScope::try_new("repo-a", ".").unwrap();
        let mut catalog = CatalogSnapshotV2::empty(1).unwrap();
        let history_id = RepoHistoryId::parse("rh_00000000000000000000000000000001").unwrap();
        catalog.repo_histories.insert(
            history_id.clone(),
            RepoHistoryRecord {
                repo_history_id: history_id.clone(),
                membership_generation: 0,
                authority: RepoHistoryAuthority::Recorded(
                    RecordedRepoAuthority::parse("repo-a").unwrap(),
                ),
                primary_namespace: CommitNamespace::parse("repo-a").unwrap(),
                compatibility_namespaces: BTreeSet::new(),
                materialization: RepoHistoryMaterialization::NotBuilt,
            },
        );
        let published = project(
            "p_00000000000000000000000000000001",
            ProjectScope::Published(published_scope.clone()),
            &history_id,
        );
        catalog
            .projects
            .insert(published.project_id.clone(), published);
        let mut connector = project(
            "p_00000000000000000000000000000002",
            ProjectScope::Connector(
                ConnectorScope::try_new("csrc_5f2c1d9a4b6e470e", "gdrive").unwrap(),
            ),
            &history_id,
        );
        connector.repo_history = None;
        catalog
            .projects
            .insert(connector.project_id.clone(), connector);
        catalog.sync_version();
        catalog.validate().unwrap();

        // The published grant still resolves to exactly its own project.
        let resolved = resolve_catalog_project(&catalog, &published_scope).unwrap();
        assert_eq!(
            resolved.as_str(),
            "p_00000000000000000000000000000001",
            "a connector project must not be reachable through a published grant"
        );

        // And no published-scope publication view can name the connector
        // project, because those views are keyed by PublishedScope.
        let auth = ProducerAuthRuntime::for_test_catalog(
            vec![(
                ServiceToken::parse("a".repeat(64)).unwrap(),
                ProducerGrant {
                    producer_id: "producer-a".into(),
                    projects: BTreeMap::from([(
                        published_scope,
                        "p_00000000000000000000000000000001".to_string(),
                    )]),
                },
            )],
            &catalog,
        );
        assert_eq!(
            auth.assigned_project_ids(),
            BTreeSet::from(["p_00000000000000000000000000000001".to_string()]),
            "publication lanes see published projects only"
        );
        assert!(auth.connectors().publication_project_ids().is_empty());
    }

    fn rotating_entry(
        producer_id: &str,
        secrets: &[&str],
        scope: &PublishedScope,
        project_id: &str,
    ) -> (Vec<ServiceToken>, ProducerGrant) {
        (
            secrets
                .iter()
                .map(|secret| ServiceToken::parse(secret.repeat(64)).unwrap())
                .collect(),
            ProducerGrant {
                producer_id: producer_id.into(),
                projects: BTreeMap::from([(scope.clone(), project_id.to_string())]),
            },
        )
    }

    #[test]
    fn a_rotation_overlap_accepts_the_old_and_new_token_then_refuses_the_retired_one() {
        let scope = PublishedScope::try_new("repo-a", ".").unwrap();
        // Staged rotation: slot 0 is the still-live old token, slot 1 is the
        // freshly staged new one. Both must authenticate during the overlap
        // window -- that is the entire point of the feature.
        let auth = ProducerAuthRuntime::for_test_rotating(
            true,
            false,
            vec![rotating_entry(
                "producer-a",
                &["a", "b"],
                &scope,
                "project-a",
            )],
        );

        assert!(
            auth.authenticate(&"a".repeat(64)).is_some(),
            "the old token stays accepted during the overlap window"
        );
        assert!(
            auth.authenticate(&"b".repeat(64)).is_some(),
            "the newly staged token is accepted immediately, before the old one retires"
        );
        assert!(auth.authenticate(&"c".repeat(64)).is_none());

        // Retirement is removing the old slot from the grant table (a
        // config reload dropping token_files[0]), not a runtime call: a
        // freshly built table with only the new token refuses the old one.
        let rotated = ProducerAuthRuntime::for_test_rotating(
            true,
            false,
            vec![rotating_entry("producer-a", &["b"], &scope, "project-a")],
        );
        assert!(
            rotated.authenticate(&"a".repeat(64)).is_none(),
            "a retired token must be refused once its slot is removed"
        );
        assert!(rotated.authenticate(&"b".repeat(64)).is_some());
    }

    #[test]
    fn matched_slot_observability_names_the_slot_index_never_the_token() {
        let scope = PublishedScope::try_new("repo-a", ".").unwrap();
        let auth = ProducerAuthRuntime::for_test_rotating(
            true,
            false,
            vec![rotating_entry(
                "producer-a",
                &["a", "b", "c"],
                &scope,
                "project-a",
            )],
        );

        // Never matched yet: no slot reported.
        assert_eq!(
            auth.token_rotation_status(),
            vec![ProducerTokenRotationStatus {
                producer_id: "producer-a".into(),
                slots: 3,
                matched_slot: None,
            }]
        );

        auth.authenticate(&"a".repeat(64));
        assert_eq!(
            auth.token_rotation_status()[0].matched_slot,
            Some(0),
            "slot 0 (the oldest token) matched"
        );

        // A fleet migrating onto the newest staged token moves the observed
        // slot up, which is the signal an operator watches for before
        // retiring an old credential.
        auth.authenticate(&"c".repeat(64));
        assert_eq!(
            auth.token_rotation_status()[0].matched_slot,
            Some(2),
            "the most recent verification wins, so the operator sees the fleet has moved"
        );

        // A failed attempt does not disturb the last-observed slot.
        auth.authenticate("wrong");
        assert_eq!(auth.token_rotation_status()[0].matched_slot, Some(2));
    }

    #[test]
    fn producer_auth_runtime_debug_never_leaks_token_material_across_multiple_slots() {
        let scope = PublishedScope::try_new("repo-a", ".").unwrap();
        let secret_a = "1".repeat(64);
        let secret_b = "2".repeat(64);
        let auth = ProducerAuthRuntime::for_test_rotating(
            true,
            false,
            vec![rotating_entry(
                "producer-a",
                &["1", "2"],
                &scope,
                "project-a",
            )],
        );
        auth.authenticate(&secret_b);

        let rendered = format!("{auth:?}");
        assert!(
            !rendered.contains(&secret_a) && !rendered.contains(&secret_b),
            "no token value may ever be reachable through Debug: {rendered}"
        );
        assert!(
            rendered.contains("producer-a"),
            "the rendering still carries operator-declared producer facts: {rendered}"
        );
        assert!(
            rendered.contains("slots: 2") && rendered.contains("matched_slot: Some(1)"),
            "the rendering carries rotation-status metadata by index only: {rendered}"
        );
    }
}
