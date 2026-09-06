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
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Result, bail};
use bbox_corpus_core::project_catalog::{
    CatalogSnapshotV2, ConnectorScope, ConnectorSourceId, ProjectId,
};
use bbox_indexing::project_catalog_admin::{ConnectorGrantExpectation, find_connector_project};
use bro_rpc::ServiceTokenSet;
use sha2::{Digest, Sha256};

/// Sentinel stored in [`ConnectorProducerToken::last_matched_slot`] before
/// this producer's token set has ever verified a request this boot. Not a
/// valid slot index.
const NEVER_MATCHED: usize = usize::MAX;

/// One authenticated producer: its operator-declared id and its ordered,
/// overlap-tolerant set of live bearers.
struct ConnectorProducerToken {
    producer_id: String,
    tokens: ServiceTokenSet,
    /// Slot index of the LAST successful verification against this
    /// producer's token set, this boot (`NEVER_MATCHED` before the first
    /// one). Rotation observability only, never token material: an
    /// operator watching this move off slot 0 toward a higher index knows
    /// the fleet has migrated onto a staged token and the retired slot is
    /// safe to remove.
    last_matched_slot: Arc<AtomicUsize>,
}

/// One producer's token-rotation observability snapshot. Never carries
/// token material -- `matched_slot` is an index, not a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConnectorTokenRotationStatus {
    pub(crate) producer_id: String,
    pub(crate) slots: usize,
    pub(crate) matched_slot: Option<usize>,
}

/// Immutable grant table, rebuilt whenever the producer snapshot is rebuilt.
///
/// **This type now RETAINS credential material, and that is the phase-1
/// resolution of the note phase 0 left here.** Phase 0 loaded each producer's
/// token during [`ConnectorGrantRuntime::build`] purely to validate it (the
/// owner, mode, symlink, hardlink, and shape checks) and to digest it, then
/// dropped it: with no endpoint mounted there was nothing to authenticate and
/// holding a secret would have been custody without purpose. Phase 1 mounts
/// `/internal/file-source/v1/*`, so the table is now the thing that answers
/// "which producer is this bearer?" and the tokens have to live here.
///
/// The consequence is that `Debug` can no longer be DERIVED. It is written by
/// hand below and renders producer ids, connector scopes, remote authorities,
/// catalog project ids, and token-rotation status (slot count, matched slot
/// index) -- all operator-declared config or index-only metadata -- while
/// the token values themselves are unreachable. That follows the
/// `bro_rpc::ServiceToken`/`ServiceTokenSet` precedent, whose own `Debug`
/// prints `REDACTED`/a slot count: this is the second line of
/// that defense, not a replacement for it.
///
/// `the_grant_table_holds_no_credential_material` still passes verbatim and is
/// now doing real work rather than asserting a vacuous property: it renders a
/// table that genuinely holds a secret and proves the secret is unreachable.
pub(crate) struct ConnectorGrantRuntime {
    enabled: bool,
    grants: Vec<ConnectorGrantExpectation>,
    /// Live bearers, one per configured producer.
    producers: Vec<ConnectorProducerToken>,
    /// Granted scopes that already have a catalog project.
    scope_to_project: BTreeMap<ConnectorSourceId, ProjectId>,
    /// Granted scopes with no catalog project yet. Only onboarding may
    /// accept these.
    pending_onboard: BTreeSet<ConnectorScope>,
    /// Which ingest lane each granted id opens.
    ///
    /// Keyed on `connector_source_id` alone, which is sound because the config
    /// loader and [`ConnectorGrantRuntime::build`] both refuse a doubly-granted
    /// id. It is a separate map rather than a field on
    /// [`ConnectorGrantExpectation`] deliberately: that type is the shared
    /// onboarding contract in `bbox-indexing`, the onboarding composite has no
    /// business branching on a lane, and widening it would make every consumer
    /// of that crate carry a field only two route layers read.
    profiles: BTreeMap<ConnectorSourceId, crate::config::ConnectorProfile>,
}

impl std::fmt::Debug for ConnectorGrantRuntime {
    /// Hand-written so a bearer can never reach a panic message, a tracing
    /// field, or a test log through this type. Token slot counts and
    /// rotation-status metadata (matched slot index) are rendered; the
    /// token values themselves never are.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConnectorGrantRuntime")
            .field("enabled", &self.enabled)
            .field("grants", &self.grants)
            .field("producer_tokens", &self.token_rotation_status())
            .field("scope_to_project", &self.scope_to_project)
            .field("pending_onboard", &self.pending_onboard)
            .field("profiles", &self.profiles)
            .finish()
    }
}

impl ConnectorGrantRuntime {
    pub(crate) fn disabled() -> Self {
        Self {
            enabled: false,
            grants: Vec::new(),
            producers: Vec::new(),
            scope_to_project: BTreeMap::new(),
            pending_onboard: BTreeSet::new(),
            profiles: BTreeMap::new(),
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
        config.validate_retained_conversations()?;
        if !config.enabled {
            return Ok(Self::disabled());
        }
        let Some(catalog) = catalog else {
            bail!("source connectors require catalog project authority");
        };
        if config.producers.is_empty() {
            bail!("enabled source connectors require at least one producer");
        }

        let mut grants = Vec::new();
        let mut producer_tokens = Vec::new();
        let mut scope_to_project = BTreeMap::new();
        let mut pending_onboard = BTreeSet::new();
        let mut profiles = BTreeMap::new();
        let mut producer_ids = BTreeSet::new();
        let mut token_digests = BTreeSet::new();
        let mut granted_sources = BTreeSet::new();

        for producer in &config.producers {
            if !producer_ids.insert(producer.producer_id.clone()) {
                bail!("duplicate source-connector producer id");
            }
            // Load to VALIDATE and to RETAIN. `ServiceTokenSet::load`
            // performs the owner, mode, symlink, hardlink, and shape checks
            // on every staged token file, so a misconfigured token file
            // refuses startup here rather than on a first publication
            // attempt. The digest proves no two producers share a token
            // value; the tokens themselves are kept because phase 1 mounts
            // an endpoint this table has to authenticate.
            let token_paths = producer.resolved_token_files().map_err(|error| {
                anyhow::anyhow!(
                    "resolving source-connector token files for {}: {error}",
                    producer.producer_id
                )
            })?;
            let tokens = ServiceTokenSet::load(&token_paths).map_err(|error| {
                anyhow::anyhow!(
                    "loading source-connector tokens for {}: {error}",
                    producer.producer_id
                )
            })?;
            for token in tokens.tokens() {
                let token_digest = Sha256::digest(token.expose_secret().as_bytes());
                if !token_digests.insert(token_digest.to_vec()) {
                    bail!("source-connector token values must be unique");
                }
            }
            producer_tokens.push(ConnectorProducerToken {
                producer_id: producer.producer_id.clone(),
                tokens,
                last_matched_slot: Arc::new(AtomicUsize::new(NEVER_MATCHED)),
            });
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
                profiles.insert(scope.connector_source_id().clone(), grant.profile);
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
        }

        Ok(Self {
            enabled: true,
            grants,
            producers: producer_tokens,
            scope_to_project,
            pending_onboard,
            profiles,
        })
    }

    /// Resolve a presented bearer to the producer it authenticates.
    ///
    /// Comparison runs through `ServiceTokenSet::verify`, which checks every
    /// staged slot in constant time without short-circuiting, and EVERY
    /// configured producer is checked even after a match so the number of
    /// comparisons does not vary with which producer or slot presented the
    /// bearer. Returns the operator-declared `producer_id`, which is what
    /// the onboarding composite matches grants against; the token never
    /// leaves this method. On a match, the matched slot is recorded (index
    /// only, never token material) for rotation observability and logged
    /// with the producer id.
    pub(crate) fn authenticate(&self, bearer: &str) -> Option<&str> {
        if !self.enabled {
            return None;
        }
        let mut matched: Option<&str> = None;
        for producer in &self.producers {
            if let Some(slot) = producer.tokens.verify(bearer) {
                producer.last_matched_slot.store(slot, Ordering::Relaxed);
                tracing::info!(
                    producer_id = %producer.producer_id,
                    matched_slot = slot,
                    total_slots = producer.tokens.len(),
                    "source-connector token matched"
                );
                matched = Some(producer.producer_id.as_str());
            }
        }
        matched
    }

    /// Per-producer token-rotation observability: how many slots are
    /// staged and which one last verified a request, this boot. Never
    /// returns token material.
    pub(crate) fn token_rotation_status(&self) -> Vec<ConnectorTokenRotationStatus> {
        self.producers
            .iter()
            .map(|producer| {
                let slot = producer.last_matched_slot.load(Ordering::Relaxed);
                ConnectorTokenRotationStatus {
                    producer_id: producer.producer_id.clone(),
                    slots: producer.tokens.len(),
                    matched_slot: (slot != NEVER_MATCHED).then_some(slot),
                }
            })
            .collect()
    }

    // The read surface below is consumed by the file-source route layer: the
    // authentication middleware gates on `enabled`, `catalog_onboard` matches
    // against `grants`, and every publication handler routes through
    // `is_pending_onboard` plus `project_for`.
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

    /// The ingest lane a granted scope opens.
    ///
    /// Returns `None` for a scope this table does not hold, so a caller cannot
    /// mistake "not granted" for "granted on the default lane". Both route
    /// layers check this BEFORE reaching their store: a conversation grant must
    /// not be able to publish a file manifest, and a file grant must not be
    /// able to land messages, even though both authenticate through the same
    /// producer table.
    pub(crate) fn profile_for(
        &self,
        connector_source_id: &ConnectorSourceId,
    ) -> Option<crate::config::ConnectorProfile> {
        self.profiles.get(connector_source_id).copied()
    }

    /// Project ids a connector grant makes eligible for a PUBLICATION lane.
    ///
    /// Phase 0 returned empty by contract; phase 1 opens it, and this is the
    /// exactly-one place that opening happens. Eligibility is CATALOG
    /// membership, not grant presence: a granted scope with no catalog project
    /// is pending onboarding and is deliberately absent here, which is what
    /// keeps a pending scope out of every publication lane while leaving it
    /// acceptable to the onboard endpoint.
    pub(crate) fn publication_project_ids(&self) -> BTreeSet<String> {
        self.scope_to_project
            .values()
            .map(|project_id| project_id.as_str().to_string())
            .collect()
    }

    /// The granted scope owning a project id, for the publisher-status read.
    ///
    /// Inverts `scope_to_project` on demand rather than keeping a second map:
    /// the table is one entry per operator-configured scope, so a linear scan
    /// is free, and a second map is a second thing that can disagree.
    pub(crate) fn scope_for_project(&self, project_id: &ProjectId) -> Option<&ConnectorScope> {
        let source_id = self
            .scope_to_project
            .iter()
            .find(|(_, owned)| *owned == project_id)
            .map(|(source_id, _)| source_id)?;
        self.grants
            .iter()
            .map(|grant| &grant.scope)
            .find(|scope| scope.connector_source_id() == source_id)
    }

    /// Configured producer ids, derived from the grants rather than stored
    /// beside them: a producer with no scopes never builds, so the grant list
    /// already names every producer and a second copy could only disagree.
    #[cfg(test)]
    pub(crate) fn producer_ids(&self) -> BTreeSet<&str> {
        self.grants
            .iter()
            .map(|grant| grant.producer_id.as_str())
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
    use std::path::{Path, PathBuf};

    const SOURCE_A: &str = "csrc_5f2c1d9a4b6e470e";
    /// A second operator-minted source id, granted alongside `SOURCE_A` but
    /// deliberately absent from the test catalog.
    const SOURCE_B: &str = "csrc_00000000deadbeef";

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
        SourceConnectorsConfig {
            enabled,
            producers,
            retained_conversations: Vec::new(),
        }
    }

    fn grant(connector_source_id: &str, kind: &str, authority: &str) -> ConnectorScopeGrant {
        grant_on(
            connector_source_id,
            kind,
            authority,
            crate::config::ConnectorProfile::File,
        )
    }

    fn grant_on(
        connector_source_id: &str,
        kind: &str,
        authority: &str,
        profile: crate::config::ConnectorProfile,
    ) -> ConnectorScopeGrant {
        ConnectorScopeGrant {
            connector_source_id: ConnectorSourceId::parse(connector_source_id).unwrap(),
            connector_kind: ConnectorKind::parse(kind).unwrap(),
            remote_authority: authority.to_string(),
            profile,
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
                token_files: Vec::new(),
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
        assert_eq!(runtime.producer_ids(), BTreeSet::from(["producer-a"]));
    }

    /// SUPERSESSION (phase 1). This test previously asserted that an
    /// onboarded scope "still claims no publication lane", pinning the phase-0
    /// contract from when the connector family shipped deliberately unwired.
    /// Phase 1 opens the lane in exactly one place --
    /// [`ConnectorGrantRuntime::publication_project_ids`] -- and the file-source
    /// publication routes plus the activation lane are the consumers. The
    /// phase-0 pin was retired ON PURPOSE, not lost; what replaces it is the
    /// sharper half of the same rule, which phase 0 could not express because
    /// nothing was eligible: eligibility is CATALOG MEMBERSHIP, not grant
    /// presence.
    #[test]
    fn an_onboarded_scope_is_publication_eligible_and_a_pending_one_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let config = config_with(
            vec![
                ConnectorProducerConfig {
                    producer_id: "producer-a".into(),
                    token_file: token_file(&root, "token-a", &"a".repeat(64)),
                    token_files: Vec::new(),
                    scopes: vec![grant(SOURCE_A, "gdrive", "tenant.example")],
                },
                // Granted identically, and deliberately absent from the
                // catalog: the ONLY difference between these two scopes is
                // onboarding.
                ConnectorProducerConfig {
                    producer_id: "producer-b".into(),
                    token_file: token_file(&root, "token-b", &"b".repeat(64)),
                    token_files: Vec::new(),
                    scopes: vec![grant(SOURCE_B, "gdrive", "tenant.example")],
                },
            ],
            true,
        );
        let catalog = catalog_with_connector(SOURCE_A, "gdrive");
        let runtime = ConnectorGrantRuntime::build(&config, Some(&catalog)).unwrap();

        // The onboarded scope resolves through the catalog surfaces, which is
        // what the phase-0 test proved and still holds.
        let onboarded = ConnectorScope::try_new(SOURCE_A, "gdrive").unwrap();
        assert!(!runtime.is_pending_onboard(&onboarded));
        let project = runtime
            .project_for(onboarded.connector_source_id())
            .expect("an onboarded scope resolves to its catalog project");
        assert_eq!(
            runtime.scope_for_project(project),
            Some(&onboarded),
            "and the inverse resolution agrees, so the two directions cannot \
             disagree about which scope owns a project"
        );

        // The phase-1 contract: it IS publication-eligible.
        assert_eq!(
            runtime.publication_project_ids(),
            BTreeSet::from([project.as_str().to_string()]),
            "phase 1 opens the publication lane for an onboarded connector \
             scope; the activation lane publishes into exactly this project"
        );

        // And the pending scope is not, even though it is equally granted.
        let pending = ConnectorScope::try_new(SOURCE_B, "gdrive").unwrap();
        assert!(runtime.is_pending_onboard(&pending));
        assert!(
            runtime.project_for(pending.connector_source_id()).is_none(),
            "a pending scope resolves to no project, so there is nothing for a \
             publication lane to name"
        );
        assert_eq!(
            runtime.grants().len(),
            2,
            "both scopes remain visible to onboarding: eligibility is catalog \
             membership, not grant presence"
        );
    }

    #[test]
    fn a_grants_ingest_lane_travels_with_it_into_the_table() {
        // Two producers, two lanes, one table. The lane is what keeps a
        // conversation grant out of the file publication routes and a file
        // grant out of the conversation ones, even though both authenticate
        // through the same producer list.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let config = config_with(
            vec![
                ConnectorProducerConfig {
                    producer_id: "producer-files".into(),
                    token_file: token_file(&root, "token-a", &"a".repeat(64)),
                    token_files: Vec::new(),
                    scopes: vec![grant(SOURCE_A, "gdrive", "tenant.example")],
                },
                ConnectorProducerConfig {
                    producer_id: "producer-conversation".into(),
                    token_file: token_file(&root, "token-b", &"b".repeat(64)),
                    token_files: Vec::new(),
                    scopes: vec![grant_on(
                        SOURCE_B,
                        "slack",
                        "workspace.example",
                        crate::config::ConnectorProfile::Conversation,
                    )],
                },
            ],
            true,
        );
        let catalog = CatalogSnapshotV2::empty(1).unwrap();
        let runtime = ConnectorGrantRuntime::build(&config, Some(&catalog)).unwrap();

        let files = ConnectorSourceId::parse(SOURCE_A).unwrap();
        let conversation = ConnectorSourceId::parse(SOURCE_B).unwrap();
        assert_eq!(
            runtime.profile_for(&files),
            Some(crate::config::ConnectorProfile::File)
        );
        assert_eq!(
            runtime.profile_for(&conversation),
            Some(crate::config::ConnectorProfile::Conversation)
        );
        // An ungranted id resolves to no lane at all, so a caller cannot
        // mistake "not granted" for "granted on the default lane".
        assert_eq!(
            runtime.profile_for(&ConnectorSourceId::parse("csrc_1111111111111111").unwrap()),
            None
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
                token_files: Vec::new(),
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
                token_files: Vec::new(),
                scopes: vec![grant(SOURCE_A, "gdrive", "tenant.example")],
            }],
            true,
        );
        let error = ConnectorGrantRuntime::build(&config, None).unwrap_err();
        assert!(error.to_string().contains("catalog project authority"));
    }

    #[test]
    fn the_grant_table_holds_no_credential_material() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let secret = "b".repeat(64);
        let config = config_with(
            vec![ConnectorProducerConfig {
                producer_id: "producer-a".into(),
                token_file: token_file(&root, "token-a", &secret),
                token_files: Vec::new(),
                scopes: vec![grant(SOURCE_A, "gdrive", "tenant.example")],
            }],
            true,
        );
        let catalog = CatalogSnapshotV2::empty(1).unwrap();
        let runtime = ConnectorGrantRuntime::build(&config, Some(&catalog)).unwrap();

        // Phase 1 RETAINS this token, so the assertion below is no longer
        // vacuous: it renders a table that genuinely holds the secret and
        // proves the secret is unreachable through it. That is what the
        // hand-written `Debug` buys, and why the derive had to go.
        let rendered = format!("{runtime:?}");
        assert!(
            !rendered.contains(&secret),
            "a token value must never be reachable through the grant table"
        );
        assert!(
            rendered.contains("producer-a") && rendered.contains(SOURCE_A),
            "the rendering must still carry the operator-declared grant facts: {rendered}"
        );
    }

    #[test]
    fn the_grant_table_holds_no_credential_material_across_a_staged_rotation() {
        // Extends the test above to the token_files list form: a producer
        // staging TWO accepted tokens (an overlap-tolerant rotation) must
        // leak neither value through Debug, only the slot count and, once a
        // request has verified, which slot matched.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let secret_old = "1".repeat(64);
        let secret_new = "2".repeat(64);
        let config = config_with(
            vec![ConnectorProducerConfig {
                producer_id: "producer-a".into(),
                token_file: PathBuf::new(),
                token_files: vec![
                    token_file(&root, "token-old", &secret_old),
                    token_file(&root, "token-new", &secret_new),
                ],
                scopes: vec![grant(SOURCE_A, "gdrive", "tenant.example")],
            }],
            true,
        );
        let catalog = CatalogSnapshotV2::empty(1).unwrap();
        let runtime = ConnectorGrantRuntime::build(&config, Some(&catalog)).unwrap();
        assert_eq!(runtime.authenticate(&secret_new), Some("producer-a"));

        let rendered = format!("{runtime:?}");
        assert!(
            !rendered.contains(&secret_old) && !rendered.contains(&secret_new),
            "no staged token value may ever be reachable through Debug: {rendered}"
        );
        assert!(
            rendered.contains("producer-a") && rendered.contains(SOURCE_A),
            "the rendering must still carry the operator-declared grant facts: {rendered}"
        );
        assert!(
            rendered.contains("slots: 2") && rendered.contains("matched_slot: Some(1)"),
            "the rendering carries rotation-status metadata by index only: {rendered}"
        );
    }

    #[test]
    fn a_rotation_overlap_accepts_the_old_and_new_token_then_refuses_the_retired_one() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let secret_old = "1".repeat(64);
        let secret_new = "2".repeat(64);
        let staged = config_with(
            vec![ConnectorProducerConfig {
                producer_id: "producer-a".into(),
                token_file: PathBuf::new(),
                token_files: vec![
                    token_file(&root, "token-old", &secret_old),
                    token_file(&root, "token-new", &secret_new),
                ],
                scopes: vec![grant(SOURCE_A, "gdrive", "tenant.example")],
            }],
            true,
        );
        let catalog = CatalogSnapshotV2::empty(1).unwrap();
        let runtime = ConnectorGrantRuntime::build(&staged, Some(&catalog)).unwrap();

        assert_eq!(
            runtime.authenticate(&secret_old),
            Some("producer-a"),
            "the old token stays accepted during the overlap window"
        );
        assert_eq!(
            runtime.authenticate(&secret_new),
            Some("producer-a"),
            "the newly staged token is accepted immediately"
        );
        assert_eq!(
            runtime.token_rotation_status(),
            vec![ConnectorTokenRotationStatus {
                producer_id: "producer-a".into(),
                slots: 2,
                matched_slot: Some(1),
            }],
            "the most recent verification (slot 1, the new token) is what an operator sees"
        );

        // Retirement is a config reload dropping the old slot, not a
        // runtime call: a freshly built table with only the new token
        // refuses the old one.
        let rotated = config_with(
            vec![ConnectorProducerConfig {
                producer_id: "producer-a".into(),
                token_file: token_file(&root, "token-new", &secret_new),
                token_files: Vec::new(),
                scopes: vec![grant(SOURCE_A, "gdrive", "tenant.example")],
            }],
            true,
        );
        let runtime = ConnectorGrantRuntime::build(&rotated, Some(&catalog)).unwrap();
        assert_eq!(
            runtime.authenticate(&secret_old),
            None,
            "a retired token must be refused once its slot is removed"
        );
        assert_eq!(runtime.authenticate(&secret_new), Some("producer-a"));
    }

    #[test]
    fn a_retained_bearer_resolves_to_exactly_its_own_producer() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let secret_a = "a".repeat(64);
        let secret_b = "b".repeat(64);
        let config = config_with(
            vec![
                ConnectorProducerConfig {
                    producer_id: "producer-a".into(),
                    token_file: token_file(&root, "token-a", &secret_a),
                    token_files: Vec::new(),
                    scopes: vec![grant(SOURCE_A, "gdrive", "tenant.example")],
                },
                ConnectorProducerConfig {
                    producer_id: "producer-b".into(),
                    token_file: token_file(&root, "token-b", &secret_b),
                    token_files: Vec::new(),
                    scopes: vec![grant("csrc_00000000deadbeef", "gdrive", "tenant.example")],
                },
            ],
            true,
        );
        let catalog = CatalogSnapshotV2::empty(1).unwrap();
        let runtime = ConnectorGrantRuntime::build(&config, Some(&catalog)).unwrap();

        assert_eq!(runtime.authenticate(&secret_a), Some("producer-a"));
        assert_eq!(runtime.authenticate(&secret_b), Some("producer-b"));
        assert_eq!(
            runtime.authenticate(&"c".repeat(64)),
            None,
            "an unknown bearer authenticates as nobody"
        );
        assert_eq!(runtime.authenticate(""), None);
        assert_eq!(
            runtime.authenticate(&secret_a[..63]),
            None,
            "a truncated bearer is not a prefix match"
        );
    }

    #[test]
    fn a_disabled_family_authenticates_nobody() {
        // Retaining tokens must not create a lane that answers while the
        // family is switched off.
        let runtime = ConnectorGrantRuntime::build(&config_with(Vec::new(), false), None).unwrap();
        assert_eq!(runtime.authenticate(&"a".repeat(64)), None);
    }

    #[test]
    fn an_unsafe_token_file_still_refuses_startup() {
        // Retaining nothing must not weaken the fail-closed load: the token
        // is read at build time precisely so a bad file refuses here.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = token_file(&root, "token-a", &"a".repeat(64));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        let config = config_with(
            vec![ConnectorProducerConfig {
                producer_id: "producer-a".into(),
                token_file: path,
                token_files: Vec::new(),
                scopes: vec![grant(SOURCE_A, "gdrive", "tenant.example")],
            }],
            true,
        );
        let catalog = CatalogSnapshotV2::empty(1).unwrap();
        let error = ConnectorGrantRuntime::build(&config, Some(&catalog)).unwrap_err();
        assert!(
            error.to_string().contains("producer-a"),
            "the refusal must name the producer whose token file is unsafe: {error}"
        );
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
                    token_files: Vec::new(),
                    scopes: vec![grant(SOURCE_A, "gdrive", "tenant.example")],
                },
                ConnectorProducerConfig {
                    producer_id: "producer-a".into(),
                    token_file: shared.clone(),
                    token_files: Vec::new(),
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
                    token_files: Vec::new(),
                    scopes: vec![grant(SOURCE_A, "gdrive", "tenant.example")],
                },
                ConnectorProducerConfig {
                    producer_id: "producer-b".into(),
                    token_file: shared,
                    token_files: Vec::new(),
                    scopes: vec![grant("csrc_00000000deadbeef", "gdrive", "tenant.example")],
                },
            ],
            true,
        );
        assert!(ConnectorGrantRuntime::build(&shared_token, Some(&catalog)).is_err());
    }
}
