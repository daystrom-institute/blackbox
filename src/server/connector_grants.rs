//! Connector producer grants and their pending-onboarding admission.
//!
//! The startup contract is the code-collection precedent, unchanged: a scope
//! the operator configured but the catalog does not yet hold is admitted as
//! PENDING ONBOARDING. It is excluded from every publication lane and is
//! acceptable only to onboarding. Without that admission, "add the scope to
//! the daemon config first" is impossible, which is exactly the failure the
//! 2026-08-10 live fire found on the code-source lane.
//!
//! What this module deliberately does NOT do: reach the network, hold a
//! vendor credential, or resolve anything by vendor coordinate. Grants are
//! operator config; identity is the operator-minted `connector_source_id`.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};
use bbox_corpus_core::project_catalog::{
    CatalogSnapshotV2, ConnectorScope, ConnectorSourceId, ProjectId,
};
use bbox_indexing::project_catalog_admin::{ConnectorGrantExpectation, find_connector_project};
use bro_rpc::ServiceToken;
use sha2::{Digest, Sha256};

/// One authenticated connector producer. Phase 0 mounts no wire endpoint, so
/// nothing verifies a bearer yet; the token is loaded at build time anyway so
/// an unreadable or unsafe token file refuses startup rather than surfacing
/// on the first publication attempt.
struct ConnectorAuthEntry {
    #[allow(dead_code)] // Consumed by the Phase 1 wire endpoint.
    token: ServiceToken,
    producer_id: String,
}

/// Immutable grant table, rebuilt whenever the producer snapshot is rebuilt.
pub(crate) struct ConnectorGrantRuntime {
    enabled: bool,
    entries: Vec<ConnectorAuthEntry>,
    grants: Vec<ConnectorGrantExpectation>,
    /// Granted scopes that already have a catalog project.
    scope_to_project: BTreeMap<ConnectorSourceId, ProjectId>,
    /// Granted scopes with no catalog project yet. Only onboarding may
    /// accept these.
    pending_onboard: BTreeSet<ConnectorScope>,
}

impl ConnectorGrantRuntime {
    pub(crate) fn disabled() -> Self {
        Self {
            enabled: false,
            entries: Vec::new(),
            grants: Vec::new(),
            scope_to_project: BTreeMap::new(),
            pending_onboard: BTreeSet::new(),
        }
    }

    /// Build the grant table from daemon config against the catalog.
    ///
    /// Fails closed on a missing catalog: connector scopes are catalog
    /// identity and have no bridge-mode representation at all, so there is
    /// no honest degraded behavior to fall back to.
    pub(crate) fn build(
        config: &crate::config::SourceConnectorsConfig,
        catalog: Option<&CatalogSnapshotV2>,
    ) -> Result<Self> {
        if !config.enabled {
            return Ok(Self::disabled());
        }
        let Some(catalog) = catalog else {
            bail!("source connectors require catalog project authority");
        };
        if config.producers.is_empty() {
            bail!("enabled source connectors require at least one producer");
        }

        let mut entries = Vec::new();
        let mut grants = Vec::new();
        let mut scope_to_project = BTreeMap::new();
        let mut pending_onboard = BTreeSet::new();
        let mut producer_ids = BTreeSet::new();
        let mut token_digests = BTreeSet::new();
        let mut granted_sources = BTreeSet::new();

        for producer in &config.producers {
            if !producer_ids.insert(producer.producer_id.clone()) {
                bail!("duplicate source-connector producer id");
            }
            let token = ServiceToken::load(&producer.token_file).map_err(|error| {
                anyhow::anyhow!(
                    "loading source-connector token for {}: {error}",
                    producer.producer_id
                )
            })?;
            let token_digest = Sha256::digest(token.expose_secret().as_bytes());
            if !token_digests.insert(token_digest.to_vec()) {
                bail!("source-connector token values must be unique");
            }
            if producer.scopes.is_empty() {
                bail!("enabled source-connector producer has no scopes");
            }

            for grant in &producer.scopes {
                let scope = grant.scope();
                // One minted id, one producer. The config loader already
                // refuses a doubly-granted id; this is the runtime's own
                // check so a hand-assembled config cannot slip past it.
                if !granted_sources.insert(scope.connector_source_id().clone()) {
                    bail!("connector_source_id is granted more than once");
                }
                match find_connector_project(catalog, &scope) {
                    Ok(Some(project_id)) => {
                        scope_to_project.insert(scope.connector_source_id().clone(), project_id);
                    }
                    Ok(None) => {
                        // Pending onboarding: admitted so the onboarding
                        // path can attach it, and kept out of every
                        // publication lane until it does.
                        pending_onboard.insert(scope.clone());
                        tracing::info!(
                            connector_source_id = %scope.connector_source_id(),
                            connector_kind = %scope.connector_kind(),
                            "connector scope is pending onboarding"
                        );
                    }
                    // A cataloged id under a different kind is a real
                    // config-versus-catalog disagreement, not a pending
                    // scope. Refuse rather than silently onboard a second
                    // project or publish under the wrong kind.
                    Err(error) => return Err(anyhow::anyhow!("{error}")),
                }
                grants.push(ConnectorGrantExpectation {
                    producer_id: producer.producer_id.clone(),
                    scope,
                    remote_authority: grant.remote_authority.clone(),
                });
            }
            entries.push(ConnectorAuthEntry {
                token,
                producer_id: producer.producer_id.clone(),
            });
        }

        Ok(Self {
            enabled: true,
            entries,
            grants,
            scope_to_project,
            pending_onboard,
        })
    }

    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    /// The grant table the onboarding composite validates against.
    pub(crate) fn grants(&self) -> &[ConnectorGrantExpectation] {
        &self.grants
    }

    /// True when the scope is operator-configured but has no catalog project
    /// yet. Only onboarding may admit such a scope.
    pub(crate) fn is_pending_onboard(&self, scope: &ConnectorScope) -> bool {
        self.pending_onboard.contains(scope)
    }

    pub(crate) fn project_for(
        &self,
        connector_source_id: &ConnectorSourceId,
    ) -> Option<&ProjectId> {
        self.scope_to_project.get(connector_source_id)
    }

    /// Project ids a connector grant makes eligible for a PUBLICATION lane.
    ///
    /// Always empty in Phase 0, and that is the contract, not an oversight:
    /// the phase ends at "onboards, lists, and reports with no publication
    /// yet". Callers ask this rather than inferring eligibility from the
    /// presence of a grant, so Phase 1 has exactly one place to open.
    pub(crate) fn publication_project_ids(&self) -> BTreeSet<String> {
        BTreeSet::new()
    }

    #[cfg(test)]
    pub(crate) fn producer_ids(&self) -> Vec<&str> {
        self.entries
            .iter()
            .map(|entry| entry.producer_id.as_str())
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn pending_scopes(&self) -> &BTreeSet<ConnectorScope> {
        &self.pending_onboard
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConnectorProducerConfig, ConnectorScopeGrant, SourceConnectorsConfig};
    use bbox_corpus_core::project_catalog::{ConnectorKind, CorpusProject, ProjectScope};
    use std::collections::BTreeSet as Set;
    use std::io::Write;
    use std::path::Path;

    const SOURCE_A: &str = "csrc_5f2c1d9a4b6e470e";

    fn token_file(root: &Path, name: &str, value: &str) -> std::path::PathBuf {
        let path = root.join(name);
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(value.as_bytes()).unwrap();
        drop(file);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        path
    }

    fn config_with(
        producers: Vec<ConnectorProducerConfig>,
        enabled: bool,
    ) -> SourceConnectorsConfig {
        SourceConnectorsConfig { enabled, producers }
    }

    fn grant(connector_source_id: &str, kind: &str, authority: &str) -> ConnectorScopeGrant {
        ConnectorScopeGrant {
            connector_source_id: ConnectorSourceId::parse(connector_source_id).unwrap(),
            connector_kind: ConnectorKind::parse(kind).unwrap(),
            remote_authority: authority.to_string(),
        }
    }

    fn catalog_with_connector(connector_source_id: &str, kind: &str) -> CatalogSnapshotV2 {
        let mut catalog = CatalogSnapshotV2::empty(1).unwrap();
        let project_id = ProjectId::parse("p_000000000000000000000000000000a1").unwrap();
        catalog.projects.insert(
            project_id.clone(),
            CorpusProject {
                project_id,
                scope: ProjectScope::Connector(
                    ConnectorScope::try_new(connector_source_id, kind).unwrap(),
                ),
                operator_aliases: Set::new(),
                nominated_aliases: Set::new(),
                display_name: "Ops shared folder".into(),
                created_at: "2026-08-13T00:00:00Z".into(),
                registered_at_compat: None,
                repo_history: None,
                languages: Set::new(),
            },
        );
        catalog.sync_version();
        catalog.validate().unwrap();
        catalog
    }

    #[test]
    fn a_configured_uncataloged_scope_is_admitted_as_pending_onboarding() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let config = config_with(
            vec![ConnectorProducerConfig {
                producer_id: "producer-a".into(),
                token_file: token_file(&root, "token-a", &"a".repeat(64)),
                scopes: vec![grant(SOURCE_A, "gdrive", "tenant.example")],
            }],
            true,
        );
        let catalog = CatalogSnapshotV2::empty(1).unwrap();
        let runtime = ConnectorGrantRuntime::build(&config, Some(&catalog)).unwrap();

        let scope = ConnectorScope::try_new(SOURCE_A, "gdrive").unwrap();
        assert!(
            runtime.is_pending_onboard(&scope),
            "a configured scope with no catalog project must not refuse boot"
        );
        assert_eq!(runtime.pending_scopes().len(), 1);
        assert!(runtime.project_for(scope.connector_source_id()).is_none());
        assert!(
            runtime.publication_project_ids().is_empty(),
            "a pending scope is excluded from every publication lane"
        );
        assert_eq!(runtime.grants().len(), 1, "onboarding still sees the grant");
        assert_eq!(runtime.producer_ids(), vec!["producer-a"]);
    }

    #[test]
    fn an_onboarded_scope_resolves_and_still_claims_no_publication_lane() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let config = config_with(
            vec![ConnectorProducerConfig {
                producer_id: "producer-a".into(),
                token_file: token_file(&root, "token-a", &"a".repeat(64)),
                scopes: vec![grant(SOURCE_A, "gdrive", "tenant.example")],
            }],
            true,
        );
        let catalog = catalog_with_connector(SOURCE_A, "gdrive");
        let runtime = ConnectorGrantRuntime::build(&config, Some(&catalog)).unwrap();

        let scope = ConnectorScope::try_new(SOURCE_A, "gdrive").unwrap();
        assert!(!runtime.is_pending_onboard(&scope));
        assert!(runtime.project_for(scope.connector_source_id()).is_some());
        assert!(
            runtime.publication_project_ids().is_empty(),
            "Phase 0 opens no publication lane even for an onboarded scope"
        );
    }

    #[test]
    fn a_kind_disagreement_between_config_and_catalog_refuses_boot() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let config = config_with(
            vec![ConnectorProducerConfig {
                producer_id: "producer-a".into(),
                token_file: token_file(&root, "token-a", &"a".repeat(64)),
                scopes: vec![grant(SOURCE_A, "graph", "tenant.example")],
            }],
            true,
        );
        // The catalog holds the same minted id under a different kind: a
        // real disagreement, not a pending scope.
        let catalog = catalog_with_connector(SOURCE_A, "gdrive");
        let error = ConnectorGrantRuntime::build(&config, Some(&catalog)).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("error.project_catalog_connector_kind_mismatch"),
            "the refusal must name the mismatch: {error}"
        );
    }

    #[test]
    fn connector_grants_require_catalog_authority() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let config = config_with(
            vec![ConnectorProducerConfig {
                producer_id: "producer-a".into(),
                token_file: token_file(&root, "token-a", &"a".repeat(64)),
                scopes: vec![grant(SOURCE_A, "gdrive", "tenant.example")],
            }],
            true,
        );
        let error = ConnectorGrantRuntime::build(&config, None).unwrap_err();
        assert!(error.to_string().contains("catalog project authority"));
    }

    #[test]
    fn a_disabled_family_builds_an_empty_table() {
        let config = config_with(Vec::new(), false);
        let runtime = ConnectorGrantRuntime::build(&config, None).unwrap();
        assert!(!runtime.enabled());
        assert!(runtime.grants().is_empty());
        assert!(runtime.publication_project_ids().is_empty());
    }

    #[test]
    fn duplicate_producers_and_shared_tokens_refuse() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let shared = token_file(&root, "token-shared", &"a".repeat(64));
        let duplicate_ids = config_with(
            vec![
                ConnectorProducerConfig {
                    producer_id: "producer-a".into(),
                    token_file: shared.clone(),
                    scopes: vec![grant(SOURCE_A, "gdrive", "tenant.example")],
                },
                ConnectorProducerConfig {
                    producer_id: "producer-a".into(),
                    token_file: shared.clone(),
                    scopes: vec![grant("csrc_00000000deadbeef", "gdrive", "tenant.example")],
                },
            ],
            true,
        );
        let catalog = CatalogSnapshotV2::empty(1).unwrap();
        assert!(ConnectorGrantRuntime::build(&duplicate_ids, Some(&catalog)).is_err());

        let shared_token = config_with(
            vec![
                ConnectorProducerConfig {
                    producer_id: "producer-a".into(),
                    token_file: shared.clone(),
                    scopes: vec![grant(SOURCE_A, "gdrive", "tenant.example")],
                },
                ConnectorProducerConfig {
                    producer_id: "producer-b".into(),
                    token_file: shared,
                    scopes: vec![grant("csrc_00000000deadbeef", "gdrive", "tenant.example")],
                },
            ],
            true,
        );
        assert!(ConnectorGrantRuntime::build(&shared_token, Some(&catalog)).is_err());
    }
}
