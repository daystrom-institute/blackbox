//! Offline collected-source cutover and runtime LocalProjectWalk refusal.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use bbox_code_source::GenerationState;
use bbox_code_source_store::{
    CodeSourceStore, MixedActivationRecord, MixedStoredGeneration, RuntimeRecordMode, StoreLimits,
};
use bbox_config::config::Config;
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::json_store::{atomic_write_json_locked, with_store_lock};
use bbox_corpus_core::project_catalog::{ProjectId, ProjectScope};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::checkout_access::{
    CheckoutAccessHealth, CheckoutAccessKind, CheckoutAccessObservations,
    CheckoutAccessTargetCounter,
};
use crate::code_source_locality_observations::{
    CodeSourceLocalityEvidenceKindV1, CodeSourceLocalityObservationSnapshotV1,
    CodeSourceLocalityObservationV1, CodeSourceLocalityObservationsV1,
    observation_path_from_code_source_root,
};
use crate::project_catalog_migration::ProjectCatalogMigrationResolvedLayoutV1;
use crate::project_catalog_store::ProjectCatalogStore;

const REPORT_VERSION: u32 = 1;
const MARKER_VERSION: u32 = 1;
const RECEIPT_VERSION: u32 = 1;
pub const MIN_CODE_SOURCE_LOCALITY_QUIET_SECS: u64 = 5 * 60;
pub const CODE_SOURCE_LOCALITY_CUTOVER_MARKER_FILE: &str =
    "code-source-locality-cutover-marker.json";
const MAX_ARTIFACT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeSourceLocalityGenerationEvidenceV1 {
    pub project_id: ProjectId,
    pub scope: PublishedScope,
    pub producer_id: String,
    pub generation_id: String,
    pub selector: String,
    pub snapshot_id: String,
    pub document_count: u64,
    pub entity_inventory_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeSourceLocalityCutoverRowV1 {
    pub project_id: ProjectId,
    pub scope: PublishedScope,
    pub producer_id: String,
    pub generation: CodeSourceLocalityGenerationEvidenceV1,
    pub startup_recovery: CodeSourceLocalityObservationV1,
    pub full_rebuild: CodeSourceLocalityObservationV1,
    pub checkout_baselines: Vec<CheckoutAccessTargetCounter>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeSourceLocalityCutoverReportV1 {
    pub version: u32,
    pub generated_at: String,
    pub generated_at_unix_secs: u64,
    pub min_quiet_secs: u64,
    pub catalog_epoch: u64,
    pub catalog_sha256: String,
    pub checkout_observation_sequence: u64,
    pub source_observation_sequence: u64,
    pub rows: Vec<CodeSourceLocalityCutoverRowV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeSourceLocalityCutoverMarkerV1 {
    pub version: u32,
    pub applied_at: String,
    pub report_sha256: String,
    pub catalog_epoch: u64,
    pub catalog_sha256: String,
    pub rows: Vec<CodeSourceLocalityCutoverRowV1>,
    pub checksum_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeSourceLocalityCutoverReceiptV1 {
    pub version: u32,
    pub status: String,
    pub marker_checksum_sha256: Option<String>,
    pub project_count: u64,
    pub checkout_observation_sequence: u64,
    pub source_observation_sequence: u64,
}

pub struct CodeSourceLocalityCutoverPreflightRequestV1 {
    pub layout: ProjectCatalogMigrationResolvedLayoutV1,
    pub config: Config,
    pub report_path: PathBuf,
    pub project_ids: Vec<ProjectId>,
    pub min_quiet_secs: u64,
    pub generated_at: String,
}

pub struct CodeSourceLocalityCutoverApplyRequestV1 {
    pub layout: ProjectCatalogMigrationResolvedLayoutV1,
    pub config: Config,
    pub report_path: PathBuf,
    pub applied_at: String,
}

pub struct CodeSourceLocalityCutoverVerifyRequestV1 {
    pub layout: ProjectCatalogMigrationResolvedLayoutV1,
    pub config: Config,
}

#[derive(Debug, Clone, Default)]
pub struct CodeSourceLocalityCutoverRuntimeV1 {
    rows: BTreeMap<ProjectId, CodeSourceLocalityCutoverRowV1>,
}

impl CodeSourceLocalityCutoverRuntimeV1 {
    pub fn open(state_dir: &Path) -> Result<Self> {
        let path = state_dir.join(CODE_SOURCE_LOCALITY_CUTOVER_MARKER_FILE);
        let Some(marker) = read_json_optional::<CodeSourceLocalityCutoverMarkerV1>(&path)? else {
            return Ok(Self::default());
        };
        validate_marker(&marker)?;
        Ok(Self {
            rows: marker
                .rows
                .into_iter()
                .map(|row| (row.project_id.clone(), row))
                .collect(),
        })
    }

    pub fn transport_governed(&self, project_id: &str) -> bool {
        ProjectId::parse(project_id)
            .ok()
            .is_some_and(|project_id| self.rows.contains_key(&project_id))
    }

    pub fn project_ids(&self) -> Vec<ProjectId> {
        self.rows.keys().cloned().collect()
    }

    pub fn verify_assignments(
        &self,
        assignments: &BTreeMap<PublishedScope, (String, String)>,
    ) -> Result<()> {
        for row in self.rows.values() {
            match assignments.get(&row.scope) {
                Some((project_id, producer_id))
                    if project_id == row.project_id.as_str() && producer_id == &row.producer_id => {
                }
                _ => bail!(
                    "error.code_source_locality_assignment: governed project {} must retain producer {}",
                    row.project_id,
                    row.producer_id
                ),
            }
        }
        Ok(())
    }

    pub fn verify_live(
        &self,
        catalog_store: &ProjectCatalogStore,
        config: &Config,
        store: &CodeSourceStore,
        projects_path: &Path,
    ) -> Result<()> {
        if self.rows.is_empty() {
            return Ok(());
        }
        let catalog = catalog_store.snapshot()?.catalog().as_ref().clone();
        for row in self.rows.values() {
            let project = catalog
                .projects
                .get(&row.project_id)
                .context("governed code-source project is absent from the catalog")?;
            if project.scope != ProjectScope::Published(row.scope.clone()) {
                bail!("governed code-source project scope changed");
            }
            if assigned_producer(config, &row.scope)? != row.producer_id {
                bail!("governed code-source producer assignment changed");
            }
            let current = current_generation_evidence(store, projects_path, &row.project_id)?;
            if current.project_id != row.project_id
                || current.scope != row.scope
                || current.producer_id != row.producer_id
            {
                bail!(
                    "error.code_source_locality_generation: governed project {} no longer has a current generation from its cutover authority",
                    row.project_id
                );
            }
        }
        Ok(())
    }

    #[cfg(feature = "test-support")]
    pub fn governed_for_test(project_id: &str) -> Self {
        let project_id = ProjectId::parse(project_id).expect("valid test project id");
        let scope = PublishedScope::try_new("test", ".").unwrap();
        let observation = |kind| CodeSourceLocalityObservationV1 {
            project_id: project_id.clone(),
            scope: scope.clone(),
            producer_id: "producer".into(),
            generation_id: "a".repeat(64),
            selector: "collected:test".into(),
            snapshot_id: "snapshot".into(),
            document_count: 1,
            entity_inventory_sha256: "b".repeat(64),
            evidence_kind: kind,
            sequence: 1,
            observed_at_unix_secs: 1,
        };
        let generation = generation_from_observation(&observation(
            CodeSourceLocalityEvidenceKindV1::StartupRecovery,
        ));
        let row = CodeSourceLocalityCutoverRowV1 {
            project_id: project_id.clone(),
            scope: scope.clone(),
            producer_id: "producer".into(),
            generation,
            startup_recovery: observation(CodeSourceLocalityEvidenceKindV1::StartupRecovery),
            full_rebuild: observation(CodeSourceLocalityEvidenceKindV1::FullRebuild),
            checkout_baselines: Vec::new(),
        };
        Self {
            rows: BTreeMap::from([(project_id, row)]),
        }
    }
}

pub struct ProjectCatalogCodeSourceLocalityCutoverFacadeV1;

impl ProjectCatalogCodeSourceLocalityCutoverFacadeV1 {
    pub fn preflight(
        request: CodeSourceLocalityCutoverPreflightRequestV1,
    ) -> Result<CodeSourceLocalityCutoverReceiptV1> {
        if request.min_quiet_secs < MIN_CODE_SOURCE_LOCALITY_QUIET_SECS {
            bail!(
                "code-source locality quiet window must be at least {MIN_CODE_SOURCE_LOCALITY_QUIET_SECS} seconds"
            );
        }
        if request.project_ids.is_empty() {
            bail!("code-source locality cutover requires at least one explicit project id");
        }
        let selected = request.project_ids.into_iter().collect::<BTreeSet<_>>();
        let catalog_store = ProjectCatalogStore::open_existing(request.layout.projects_path())?;
        let catalog = catalog_store.snapshot()?.catalog().as_ref().clone();
        let catalog_sha256 = sha256_json(&catalog)?;
        let store = open_code_store(&request.config)?;
        let observations = open_observations(&store)?.snapshot();
        let checkout = open_checkout_observations(&request.layout)?.health();
        let mut rows = Vec::with_capacity(selected.len());
        for project_id in selected {
            let project = catalog
                .projects
                .get(&project_id)
                .with_context(|| format!("unknown cutover project {project_id}"))?;
            let ProjectScope::Published(scope) = &project.scope else {
                bail!("code-source locality cutover requires a Published project: {project_id}");
            };
            let producer_id = assigned_producer(&request.config, scope)?;
            let generation =
                current_generation_evidence(&store, request.layout.projects_path(), &project_id)?;
            if generation.scope != *scope || generation.producer_id != producer_id {
                bail!("active collected generation is not owned by the configured producer");
            }
            let startup_recovery = required_observation(
                &observations,
                &generation,
                CodeSourceLocalityEvidenceKindV1::StartupRecovery,
            )?;
            let full_rebuild = required_observation(
                &observations,
                &generation,
                CodeSourceLocalityEvidenceKindV1::FullRebuild,
            )?;
            rows.push(CodeSourceLocalityCutoverRowV1 {
                project_id,
                scope: scope.clone(),
                producer_id,
                generation,
                startup_recovery,
                full_rebuild,
                checkout_baselines: local_walk_counters(&checkout, project.project_id.as_str()),
            });
        }
        let report = CodeSourceLocalityCutoverReportV1 {
            version: REPORT_VERSION,
            generated_at: request.generated_at,
            generated_at_unix_secs: now_unix_secs(),
            min_quiet_secs: request.min_quiet_secs,
            catalog_epoch: catalog.epoch,
            catalog_sha256,
            checkout_observation_sequence: checkout.sequence,
            source_observation_sequence: observations.sequence,
            rows,
        };
        validate_report(&report)?;
        write_json(&request.report_path, &report)?;
        Ok(receipt(
            "preflight_clean",
            None,
            report.rows.len(),
            checkout.sequence,
            observations.sequence,
        ))
    }

    pub fn apply(
        request: CodeSourceLocalityCutoverApplyRequestV1,
    ) -> Result<CodeSourceLocalityCutoverReceiptV1> {
        let report: CodeSourceLocalityCutoverReportV1 = read_json_required(&request.report_path)?;
        validate_report(&report)?;
        let elapsed = now_unix_secs().saturating_sub(report.generated_at_unix_secs);
        if elapsed < report.min_quiet_secs {
            bail!(
                "code-source locality quiet window is incomplete: {elapsed}/{} seconds",
                report.min_quiet_secs
            );
        }
        let catalog_store = ProjectCatalogStore::open_existing(request.layout.projects_path())?;
        let catalog = catalog_store.snapshot()?.catalog().as_ref().clone();
        if catalog.epoch != report.catalog_epoch || sha256_json(&catalog)? != report.catalog_sha256
        {
            bail!("code-source locality cutover report is stale against the current catalog");
        }
        let store = open_code_store(&request.config)?;
        let observations = open_observations(&store)?.snapshot();
        let checkout = open_checkout_observations(&request.layout)?.health();
        for row in &report.rows {
            if assigned_producer(&request.config, &row.scope)? != row.producer_id {
                bail!("code-source locality producer assignment changed");
            }
            if current_generation_evidence(&store, request.layout.projects_path(), &row.project_id)?
                != row.generation
            {
                bail!("active collected generation changed during the quiet window");
            }
            let startup = required_observation(
                &observations,
                &row.generation,
                CodeSourceLocalityEvidenceKindV1::StartupRecovery,
            )?;
            let rebuild = required_observation(
                &observations,
                &row.generation,
                CodeSourceLocalityEvidenceKindV1::FullRebuild,
            )?;
            if !same_observation_evidence(&startup, &row.startup_recovery)
                || !same_observation_evidence(&rebuild, &row.full_rebuild)
            {
                bail!("code-source recovery evidence changed during the quiet window");
            }
            if local_walk_counters(&checkout, row.project_id.as_str()) != row.checkout_baselines {
                bail!(
                    "LocalProjectWalk access changed during the quiet window for {}",
                    row.project_id
                );
            }
        }
        let marker_path = request
            .layout
            .state_dir
            .join(CODE_SOURCE_LOCALITY_CUTOVER_MARKER_FILE);
        if marker_path.exists() {
            bail!("code-source locality cutover marker already exists; verify it instead");
        }
        let mut marker = CodeSourceLocalityCutoverMarkerV1 {
            version: MARKER_VERSION,
            applied_at: request.applied_at,
            report_sha256: sha256_json(&report)?,
            catalog_epoch: report.catalog_epoch,
            catalog_sha256: report.catalog_sha256,
            rows: report.rows,
            checksum_sha256: String::new(),
        };
        marker.checksum_sha256 = marker_checksum(&marker)?;
        write_json(&marker_path, &marker)?;
        Ok(receipt(
            "applied",
            Some(marker.checksum_sha256),
            marker.rows.len(),
            checkout.sequence,
            observations.sequence,
        ))
    }

    pub fn verify(
        request: CodeSourceLocalityCutoverVerifyRequestV1,
    ) -> Result<CodeSourceLocalityCutoverReceiptV1> {
        let path = request
            .layout
            .state_dir
            .join(CODE_SOURCE_LOCALITY_CUTOVER_MARKER_FILE);
        let marker: CodeSourceLocalityCutoverMarkerV1 = read_json_required(&path)?;
        validate_marker(&marker)?;
        let runtime = CodeSourceLocalityCutoverRuntimeV1::open(&request.layout.state_dir)?;
        let catalog_store = ProjectCatalogStore::open_existing(request.layout.projects_path())?;
        let store = open_code_store(&request.config)?;
        runtime.verify_live(
            &catalog_store,
            &request.config,
            &store,
            request.layout.projects_path(),
        )?;
        let observations = open_observations(&store)?.snapshot();
        let checkout = open_checkout_observations(&request.layout)?.health();
        for row in &marker.rows {
            if local_walk_counters(&checkout, row.project_id.as_str()) != row.checkout_baselines {
                bail!(
                    "LocalProjectWalk access changed after cutover for {}",
                    row.project_id
                );
            }
        }
        Ok(receipt(
            "verified",
            Some(marker.checksum_sha256),
            marker.rows.len(),
            checkout.sequence,
            observations.sequence,
        ))
    }
}

fn current_generation_evidence(
    store: &CodeSourceStore,
    projects_path: &Path,
    project_id: &ProjectId,
) -> Result<CodeSourceLocalityGenerationEvidenceV1> {
    let activation = store
        .load_activation_mixed(project_id.as_str())?
        .context("code-source locality cutover requires an activation record")?;
    let MixedActivationRecord::CurrentV2(activation) = activation else {
        bail!("code-source locality cutover requires a current activation record");
    };
    if activation.cutback.is_some() || activation.cutback_pending {
        bail!("code-source locality cutover refuses a cutback-pending project");
    }
    let generation = store.find_generation_mixed(&activation.generation_id)?;
    let MixedStoredGeneration::CurrentV2(generation) = generation else {
        bail!("code-source locality cutover requires a current generation");
    };
    activation.validate_against_generation(&generation)?;
    let edges_dir = bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(projects_path);
    let manifest = bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(&edges_dir)?;
    let entry = manifest
        .workspaces
        .get(project_id.as_str())
        .context("active collected generation is absent from the workspace manifest")?;
    let expected_snapshot = bbox_edge_sidecar::snapshot::active_snapshot_rel(
        project_id.as_str(),
        &activation.snapshot_id,
    );
    if entry.code_source_selector.as_deref() != Some(activation.selector.as_str())
        || entry.code_source_generation.as_deref() != Some(activation.generation_id.as_str())
        || entry.active_snapshot.as_deref() != Some(expected_snapshot.as_str())
    {
        bail!("active collected generation disagrees with the workspace manifest");
    }
    // A staging-index state with the workspace manifest and the activation
    // journal in full agreement is a completed activation whose final state
    // flip was lost to a crash; the reconciler flips it to Active right
    // after startup. The agreement checks above are what distinguish that
    // from a genuinely incomplete staging, which still fails closed.
    if generation.state != GenerationState::Active
        && generation.state != GenerationState::StagingIndex
    {
        bail!("code-source locality cutover requires an active generation");
    }
    Ok(CodeSourceLocalityGenerationEvidenceV1 {
        project_id: activation.project_id,
        scope: activation.published_scope,
        producer_id: generation.producer_id,
        generation_id: activation.generation_id,
        selector: activation.selector,
        snapshot_id: activation.snapshot_id,
        document_count: activation.document_count,
        entity_inventory_sha256: activation.entity_inventory_sha256,
    })
}

fn required_observation(
    snapshot: &CodeSourceLocalityObservationSnapshotV1,
    generation: &CodeSourceLocalityGenerationEvidenceV1,
    kind: CodeSourceLocalityEvidenceKindV1,
) -> Result<CodeSourceLocalityObservationV1> {
    snapshot
        .observations
        .iter()
        .find(|observation| {
            observation.project_id == generation.project_id && observation.evidence_kind == kind
        })
        .filter(|observation| generation_from_observation(observation) == *generation)
        .cloned()
        .with_context(|| {
            format!(
                "exact {kind:?} evidence is missing for {} generation {}",
                generation.project_id, generation.generation_id
            )
        })
}

fn generation_from_observation(
    observation: &CodeSourceLocalityObservationV1,
) -> CodeSourceLocalityGenerationEvidenceV1 {
    CodeSourceLocalityGenerationEvidenceV1 {
        project_id: observation.project_id.clone(),
        scope: observation.scope.clone(),
        producer_id: observation.producer_id.clone(),
        generation_id: observation.generation_id.clone(),
        selector: observation.selector.clone(),
        snapshot_id: observation.snapshot_id.clone(),
        document_count: observation.document_count,
        entity_inventory_sha256: observation.entity_inventory_sha256.clone(),
    }
}

fn same_observation_evidence(
    current: &CodeSourceLocalityObservationV1,
    baseline: &CodeSourceLocalityObservationV1,
) -> bool {
    current.evidence_kind == baseline.evidence_kind
        && generation_from_observation(current) == generation_from_observation(baseline)
}

fn local_walk_counters(
    health: &CheckoutAccessHealth,
    project_id: &str,
) -> Vec<CheckoutAccessTargetCounter> {
    health
        .target_counters
        .iter()
        .filter(|counter| {
            counter.project_id == project_id && counter.kind == CheckoutAccessKind::LocalProjectWalk
        })
        .cloned()
        .collect()
}

fn assigned_producer(config: &Config, scope: &PublishedScope) -> Result<String> {
    let producers = config
        .code_collection
        .producers
        .iter()
        .filter(|producer| producer.scopes.contains(scope))
        .map(|producer| producer.producer_id.clone())
        .collect::<Vec<_>>();
    match producers.as_slice() {
        [producer] => Ok(producer.clone()),
        [] => bail!("no configured producer owns code-source scope {scope:?}"),
        _ => bail!("multiple configured producers own code-source scope {scope:?}"),
    }
}

fn open_code_store(config: &Config) -> Result<CodeSourceStore> {
    CodeSourceStore::open_with_mode(
        config.paths.state_dir.join("code-sources"),
        StoreLimits::default(),
        RuntimeRecordMode::CatalogV2,
    )
}

fn open_observations(store: &CodeSourceStore) -> Result<CodeSourceLocalityObservationsV1> {
    CodeSourceLocalityObservationsV1::open(observation_path_from_code_source_root(store.root())?)
}

fn open_checkout_observations(
    layout: &ProjectCatalogMigrationResolvedLayoutV1,
) -> Result<CheckoutAccessObservations> {
    CheckoutAccessObservations::open(layout.bro_home.join("checkout-access-observations.json"))
}

fn validate_report(report: &CodeSourceLocalityCutoverReportV1) -> Result<()> {
    if report.version != REPORT_VERSION
        || report.min_quiet_secs < MIN_CODE_SOURCE_LOCALITY_QUIET_SECS
        || report.rows.is_empty()
    {
        bail!("invalid code-source locality cutover report");
    }
    validate_sha256(&report.catalog_sha256)?;
    let mut seen = BTreeSet::new();
    for row in &report.rows {
        if !seen.insert(row.project_id.clone()) {
            bail!("duplicate code-source locality cutover project");
        }
        validate_row(row)?;
    }
    Ok(())
}

fn validate_marker(marker: &CodeSourceLocalityCutoverMarkerV1) -> Result<()> {
    if marker.version != MARKER_VERSION || marker.rows.is_empty() {
        bail!("invalid code-source locality cutover marker");
    }
    validate_sha256(&marker.report_sha256)?;
    validate_sha256(&marker.catalog_sha256)?;
    validate_sha256(&marker.checksum_sha256)?;
    if marker_checksum(marker)? != marker.checksum_sha256 {
        bail!("code-source locality cutover marker checksum mismatch");
    }
    let mut seen = BTreeSet::new();
    for row in &marker.rows {
        if !seen.insert(row.project_id.clone()) {
            bail!("duplicate code-source locality cutover project");
        }
        validate_row(row)?;
    }
    Ok(())
}

fn validate_row(row: &CodeSourceLocalityCutoverRowV1) -> Result<()> {
    if row.producer_id.trim().is_empty()
        || row.producer_id.len() > 256
        || row.generation.project_id != row.project_id
        || row.generation.scope != row.scope
        || row.generation.producer_id != row.producer_id
        || row.startup_recovery.evidence_kind != CodeSourceLocalityEvidenceKindV1::StartupRecovery
        || row.full_rebuild.evidence_kind != CodeSourceLocalityEvidenceKindV1::FullRebuild
        || generation_from_observation(&row.startup_recovery) != row.generation
        || generation_from_observation(&row.full_rebuild) != row.generation
        || row.checkout_baselines.iter().any(|counter| {
            counter.project_id != row.project_id.as_str()
                || counter.kind != CheckoutAccessKind::LocalProjectWalk
        })
    {
        bail!("invalid code-source locality cutover row");
    }
    validate_sha256(&row.generation.generation_id)?;
    validate_sha256(&row.generation.entity_inventory_sha256)?;
    Ok(())
}

fn marker_checksum(marker: &CodeSourceLocalityCutoverMarkerV1) -> Result<String> {
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(&(
        marker.version,
        &marker.applied_at,
        &marker.report_sha256,
        marker.catalog_epoch,
        &marker.catalog_sha256,
        &marker.rows,
    ))?)))
}

fn receipt(
    status: &str,
    checksum: Option<String>,
    rows: usize,
    checkout_sequence: u64,
    source_sequence: u64,
) -> CodeSourceLocalityCutoverReceiptV1 {
    CodeSourceLocalityCutoverReceiptV1 {
        version: RECEIPT_VERSION,
        status: status.into(),
        marker_checksum_sha256: checksum,
        project_count: rows as u64,
        checkout_observation_sequence: checkout_sequence,
        source_observation_sequence: source_sequence,
    }
}

fn sha256_json(value: &impl Serialize) -> Result<String> {
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(value)?)))
}

fn validate_sha256(value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        bail!("invalid SHA-256 value");
    }
    Ok(())
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > MAX_ARTIFACT_BYTES {
        bail!("code-source locality artifact exceeds its byte bound");
    }
    with_store_lock(path, || atomic_write_json_locked(path, value))
}

fn read_json_required<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    read_json_optional(path)?.with_context(|| format!("{} does not exist", path.display()))
}

fn read_json_optional<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>> {
    match std::fs::read(path) {
        Ok(bytes) => {
            if bytes.len() > MAX_ARTIFACT_BYTES {
                bail!("code-source locality artifact exceeds its byte bound");
            }
            Ok(Some(serde_json::from_slice(&bytes)?))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROJECT: &str = "p_00000000000000000000000000000001";

    fn current_v2_fixture(
        root: &Path,
    ) -> (
        Config,
        ProjectCatalogMigrationResolvedLayoutV1,
        ProjectId,
        PublishedScope,
    ) {
        use bbox_config::config::{CodeCollectionProducerConfig, LoadOptions};
        use bbox_corpus_core::project_catalog::{CatalogSnapshotV2, CorpusProject};

        let state_dir = root.join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let config_path = root.join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "[paths]\nstate_dir = {:?}\nvectors_dir = {:?}\n",
                state_dir,
                state_dir.join("vectors")
            ),
        )
        .unwrap();
        let mut config = bbox_config::config::load_with(LoadOptions {
            config_path: Some(config_path),
            ..Default::default()
        })
        .unwrap();
        config.paths.state_dir = state_dir.clone();
        config.paths.projects_path = state_dir.join("projects.json");
        config.paths.bro_home = state_dir.join("bro");
        config.paths.index_path = state_dir.join("index");
        config.paths.vectors_path = state_dir.join("vectors");
        let project_id = ProjectId::parse(PROJECT).unwrap();
        let scope = PublishedScope::try_new("fixture-repo", ".").unwrap();
        config.code_collection.enabled = true;
        config.code_collection.producers = vec![CodeCollectionProducerConfig {
            producer_id: "producer".into(),
            token_file: root.join("producer-token"),
            scopes: vec![scope.clone()],
        }];
        let layout = ProjectCatalogMigrationResolvedLayoutV1::from_config(
            &config,
            crate::project_catalog_migration::ProjectCatalogMigrationLayoutOverridesV1 {
                projects_path: Some(config.paths.projects_path.clone()),
                state_dir: Some(state_dir.clone()),
            },
        )
        .unwrap();
        let catalog = ProjectCatalogStore::initialize_empty(layout.projects_path()).unwrap();
        let epoch = catalog.snapshot().unwrap().epoch();
        let project_id_for_catalog = project_id.clone();
        let scope_for_catalog = scope.clone();
        catalog
            .transact(
                epoch,
                move |catalog: &mut CatalogSnapshotV2, _attachments| {
                    catalog.projects.insert(
                        project_id_for_catalog.clone(),
                        CorpusProject {
                            project_id: project_id_for_catalog.clone(),
                            scope: ProjectScope::Published(scope_for_catalog.clone()),
                            operator_aliases: Default::default(),
                            nominated_aliases: Default::default(),
                            display_name: "fixture".into(),
                            created_at: "2026-08-09T00:00:00Z".into(),
                            registered_at_compat: None,
                            repo_history: None,
                            languages: Default::default(),
                        },
                    );
                    Ok(())
                },
            )
            .unwrap();

        let store = CodeSourceStore::open_with_mode(
            state_dir.join("code-sources"),
            StoreLimits::default(),
            RuntimeRecordMode::CatalogV2,
        )
        .unwrap();
        let head_commit = "a".repeat(40);
        let entries = Vec::new();
        let descriptor = bbox_code_source::GenerationDescriptor {
            schema_version: bbox_code_source::SCHEMA_VERSION,
            walker_policy_version: bbox_code_source::WALKER_POLICY_VERSION.into(),
            scope: scope.clone(),
            head_commit: head_commit.clone(),
            dirty_fingerprint: bbox_code_source::dirty_fingerprint(&head_commit, &entries),
            manifest_sha256: bbox_code_source::manifest_sha256(&entries),
            file_count: 0,
            logical_bytes: 0,
        };
        let upload = store.begin_upload("producer", descriptor).unwrap();
        store
            .complete_manifest("producer", &upload.upload_id)
            .unwrap();
        let generation = store
            .finalize_upload_mixed("producer", &upload.upload_id)
            .unwrap();
        let generation_id = generation.generation_id().to_string();
        let inventory = "b".repeat(64);
        store
            .record_materialization_mixed(&scope, &generation_id, 0, inventory.clone())
            .unwrap();
        store
            .mark_generation_state_mixed(&scope, &generation_id, GenerationState::Active, None)
            .unwrap();
        let selector = crate::index::project_files::collected_materialization_selector(
            project_id.as_str(),
            &generation_id,
        );
        let snapshot_id =
            bbox_edge_sidecar::snapshot::collected_snapshot_id(project_id.as_str(), &generation_id);
        store
            .save_activation_v2(&bbox_code_source_store::ActivationRecordV2 {
                version: bbox_code_source_store::MIGRATION_STORE_VERSION,
                project_id: project_id.clone(),
                published_scope: scope.clone(),
                generation_id: generation_id.clone(),
                selector: selector.clone(),
                snapshot_id: snapshot_id.clone(),
                document_count: 0,
                entity_inventory_sha256: inventory,
                current_chunk_targets: BTreeMap::new(),
                activated_unix_secs: 1,
                cutback_pending: false,
                cutback: None,
                diagnostic: None,
            })
            .unwrap();
        let edges_dir =
            bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(layout.projects_path());
        std::fs::create_dir_all(bbox_edge_sidecar::snapshot::snapshot_dir(
            &edges_dir,
            project_id.as_str(),
            &snapshot_id,
        ))
        .unwrap();
        bbox_edge_sidecar::snapshot::activate_collected_snapshot(
            &edges_dir,
            project_id.as_str(),
            scope.repo_id(),
            &head_commit,
            &generation_id,
            &selector,
            &snapshot_id,
        )
        .unwrap();
        let observations = CodeSourceLocalityObservationsV1::open(
            observation_path_from_code_source_root(store.root()).unwrap(),
        )
        .unwrap();
        observations
            .record_verified_activations(
                &store,
                layout.projects_path(),
                CodeSourceLocalityEvidenceKindV1::StartupRecovery,
            )
            .unwrap();
        observations
            .record_verified_activations(
                &store,
                layout.projects_path(),
                CodeSourceLocalityEvidenceKindV1::FullRebuild,
            )
            .unwrap();
        (config, layout, project_id, scope)
    }

    fn row() -> CodeSourceLocalityCutoverRowV1 {
        let project_id = ProjectId::parse(PROJECT).unwrap();
        let scope = PublishedScope::try_new("repo", ".").unwrap();
        let observation = |kind, sequence| CodeSourceLocalityObservationV1 {
            project_id: project_id.clone(),
            scope: scope.clone(),
            producer_id: "producer".into(),
            generation_id: "a".repeat(64),
            selector: "collected:fixture".into(),
            snapshot_id: "collected-fixture".into(),
            document_count: 3,
            entity_inventory_sha256: "b".repeat(64),
            evidence_kind: kind,
            sequence,
            observed_at_unix_secs: sequence,
        };
        let startup = observation(CodeSourceLocalityEvidenceKindV1::StartupRecovery, 1);
        CodeSourceLocalityCutoverRowV1 {
            project_id: project_id.clone(),
            scope: scope.clone(),
            producer_id: "producer".into(),
            generation: generation_from_observation(&startup),
            startup_recovery: startup,
            full_rebuild: observation(CodeSourceLocalityEvidenceKindV1::FullRebuild, 2),
            checkout_baselines: Vec::new(),
        }
    }

    fn activate_successor_generation(
        config: &Config,
        layout: &ProjectCatalogMigrationResolvedLayoutV1,
        project_id: &ProjectId,
        scope: &PublishedScope,
    ) -> String {
        let store = CodeSourceStore::open_with_mode(
            config.paths.state_dir.join("code-sources"),
            StoreLimits::default(),
            RuntimeRecordMode::CatalogV2,
        )
        .unwrap();
        let head_commit = "c".repeat(40);
        let entries = Vec::new();
        let descriptor = bbox_code_source::GenerationDescriptor {
            schema_version: bbox_code_source::SCHEMA_VERSION,
            walker_policy_version: bbox_code_source::WALKER_POLICY_VERSION.into(),
            scope: scope.clone(),
            head_commit: head_commit.clone(),
            dirty_fingerprint: bbox_code_source::dirty_fingerprint(&head_commit, &entries),
            manifest_sha256: bbox_code_source::manifest_sha256(&entries),
            file_count: 0,
            logical_bytes: 0,
        };
        let upload = store.begin_upload("producer", descriptor).unwrap();
        store
            .complete_manifest("producer", &upload.upload_id)
            .unwrap();
        let generation = store
            .finalize_upload_mixed("producer", &upload.upload_id)
            .unwrap();
        let generation_id = generation.generation_id().to_string();
        let inventory = "d".repeat(64);
        store
            .record_materialization_mixed(scope, &generation_id, 0, inventory.clone())
            .unwrap();
        store
            .mark_generation_state_mixed(scope, &generation_id, GenerationState::Active, None)
            .unwrap();
        let selector = crate::index::project_files::collected_materialization_selector(
            project_id.as_str(),
            &generation_id,
        );
        let snapshot_id =
            bbox_edge_sidecar::snapshot::collected_snapshot_id(project_id.as_str(), &generation_id);
        store
            .save_activation_v2(&bbox_code_source_store::ActivationRecordV2 {
                version: bbox_code_source_store::MIGRATION_STORE_VERSION,
                project_id: project_id.clone(),
                published_scope: scope.clone(),
                generation_id: generation_id.clone(),
                selector: selector.clone(),
                snapshot_id: snapshot_id.clone(),
                document_count: 0,
                entity_inventory_sha256: inventory,
                current_chunk_targets: BTreeMap::new(),
                activated_unix_secs: 2,
                cutback_pending: false,
                cutback: None,
                diagnostic: None,
            })
            .unwrap();
        let edges_dir =
            bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(layout.projects_path());
        std::fs::create_dir_all(bbox_edge_sidecar::snapshot::snapshot_dir(
            &edges_dir,
            project_id.as_str(),
            &snapshot_id,
        ))
        .unwrap();
        bbox_edge_sidecar::snapshot::activate_collected_snapshot(
            &edges_dir,
            project_id.as_str(),
            scope.repo_id(),
            &head_commit,
            &generation_id,
            &selector,
            &snapshot_id,
        )
        .unwrap();
        generation_id
    }

    fn marker() -> CodeSourceLocalityCutoverMarkerV1 {
        let mut marker = CodeSourceLocalityCutoverMarkerV1 {
            version: MARKER_VERSION,
            applied_at: "2026-08-09T00:00:00Z".into(),
            report_sha256: "c".repeat(64),
            catalog_epoch: 1,
            catalog_sha256: "d".repeat(64),
            rows: vec![row()],
            checksum_sha256: String::new(),
        };
        marker.checksum_sha256 = marker_checksum(&marker).unwrap();
        marker
    }

    #[test]
    fn runtime_marker_closes_assignment_removal() {
        let dir = tempfile::tempdir().unwrap();
        write_json(
            &dir.path().join(CODE_SOURCE_LOCALITY_CUTOVER_MARKER_FILE),
            &marker(),
        )
        .unwrap();
        let runtime = CodeSourceLocalityCutoverRuntimeV1::open(dir.path()).unwrap();
        assert!(runtime.transport_governed(PROJECT));
        let row = row();
        let assignments = BTreeMap::from([(
            row.scope.clone(),
            (PROJECT.to_string(), "producer".to_string()),
        )]);
        runtime.verify_assignments(&assignments).unwrap();
        let error = runtime.verify_assignments(&BTreeMap::new()).unwrap_err();
        assert!(format!("{error:#}").contains("must retain producer"));
    }

    #[test]
    fn corrupt_marker_checksum_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let mut marker = marker();
        marker.checksum_sha256 = "e".repeat(64);
        write_json(
            &dir.path().join(CODE_SOURCE_LOCALITY_CUTOVER_MARKER_FILE),
            &marker,
        )
        .unwrap();
        let error = CodeSourceLocalityCutoverRuntimeV1::open(dir.path()).unwrap_err();
        assert!(format!("{error:#}").contains("checksum mismatch"));
    }

    #[test]
    fn current_v2_preflight_apply_verify_closes_post_cutover_local_walk() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let (config, layout, project_id, scope) = current_v2_fixture(&root);
        let report_path = root.join("code-source-locality-report.json");

        let preflight = ProjectCatalogCodeSourceLocalityCutoverFacadeV1::preflight(
            CodeSourceLocalityCutoverPreflightRequestV1 {
                layout: layout.clone(),
                config: config.clone(),
                report_path: report_path.clone(),
                project_ids: vec![project_id.clone()],
                min_quiet_secs: MIN_CODE_SOURCE_LOCALITY_QUIET_SECS,
                generated_at: "2026-08-09T00:00:00Z".into(),
            },
        )
        .unwrap();
        assert_eq!(preflight.status, "preflight_clean");
        let mut report: CodeSourceLocalityCutoverReportV1 =
            read_json_required(&report_path).unwrap();
        report.generated_at_unix_secs =
            now_unix_secs().saturating_sub(MIN_CODE_SOURCE_LOCALITY_QUIET_SECS);
        write_json(&report_path, &report).unwrap();

        let applied = ProjectCatalogCodeSourceLocalityCutoverFacadeV1::apply(
            CodeSourceLocalityCutoverApplyRequestV1 {
                layout: layout.clone(),
                config: config.clone(),
                report_path,
                applied_at: "2026-08-09T00:05:00Z".into(),
            },
        )
        .unwrap();
        assert_eq!(applied.status, "applied");
        assert!(
            CodeSourceLocalityCutoverRuntimeV1::open(&layout.state_dir)
                .unwrap()
                .transport_governed(project_id.as_str())
        );
        let verified = ProjectCatalogCodeSourceLocalityCutoverFacadeV1::verify(
            CodeSourceLocalityCutoverVerifyRequestV1 {
                layout: layout.clone(),
                config: config.clone(),
            },
        )
        .unwrap();
        assert_eq!(verified.status, "verified");

        let successor = activate_successor_generation(&config, &layout, &project_id, &scope);
        assert_ne!(successor, report.rows[0].generation.generation_id);
        let verified_after_successor = ProjectCatalogCodeSourceLocalityCutoverFacadeV1::verify(
            CodeSourceLocalityCutoverVerifyRequestV1 {
                layout: layout.clone(),
                config: config.clone(),
            },
        )
        .unwrap();
        assert_eq!(verified_after_successor.status, "verified");

        let observation_path = layout.bro_home.join("checkout-access-observations.json");
        std::fs::create_dir_all(observation_path.parent().unwrap()).unwrap();
        let counter = CheckoutAccessTargetCounter {
            project_id: project_id.to_string(),
            kind: CheckoutAccessKind::LocalProjectWalk,
            source_lane: crate::checkout_access::CheckoutAccessSourceLane::LegacyProjectRecord,
            outcome: crate::checkout_access::CheckoutAccessOutcome::Granted,
            count: 1,
            last_sequence: 1,
            last_unix_secs: 1,
        };
        std::fs::write(
            observation_path,
            serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "sequence": 1,
                "counters": [{
                    "kind": "local_project_walk",
                    "source_lane": "legacy_project_record",
                    "outcome": "granted",
                    "count": 1,
                    "last_sequence": 1,
                    "last_unix_secs": 1
                }],
                "target_counters": [counter]
            }))
            .unwrap(),
        )
        .unwrap();
        let error = ProjectCatalogCodeSourceLocalityCutoverFacadeV1::verify(
            CodeSourceLocalityCutoverVerifyRequestV1 { layout, config },
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("changed after cutover"),
            "unexpected verification error: {error:#}"
        );
    }

    #[test]
    fn verify_tolerates_a_crash_wedged_completed_staging() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let (config, layout, project_id, scope) = current_v2_fixture(&root);
        let report_path = root.join("code-source-locality-report.json");

        let preflight = ProjectCatalogCodeSourceLocalityCutoverFacadeV1::preflight(
            CodeSourceLocalityCutoverPreflightRequestV1 {
                layout: layout.clone(),
                config: config.clone(),
                report_path: report_path.clone(),
                project_ids: vec![project_id.clone()],
                min_quiet_secs: MIN_CODE_SOURCE_LOCALITY_QUIET_SECS,
                generated_at: "2026-08-09T00:00:00Z".into(),
            },
        )
        .unwrap();
        assert_eq!(preflight.status, "preflight_clean");
        let mut report: CodeSourceLocalityCutoverReportV1 =
            read_json_required(&report_path).unwrap();
        report.generated_at_unix_secs =
            now_unix_secs().saturating_sub(MIN_CODE_SOURCE_LOCALITY_QUIET_SECS);
        write_json(&report_path, &report).unwrap();
        let generation_id = report.rows[0].generation.generation_id.clone();
        ProjectCatalogCodeSourceLocalityCutoverFacadeV1::apply(
            CodeSourceLocalityCutoverApplyRequestV1 {
                layout: layout.clone(),
                config: config.clone(),
                report_path,
                applied_at: "2026-08-09T00:05:00Z".into(),
            },
        )
        .unwrap();

        // Wedge: staging completed (manifest, journal, and index all agree,
        // which the preflight established) but the final state flip was lost,
        // as when a crash lands between journal write and state flip.
        let store = CodeSourceStore::open_with_mode(
            config.paths.state_dir.join("code-sources"),
            StoreLimits::default(),
            RuntimeRecordMode::CatalogV2,
        )
        .unwrap();
        store
            .mark_generation_state_mixed(
                &scope,
                &generation_id,
                GenerationState::StagingIndex,
                None,
            )
            .unwrap();

        let verified = ProjectCatalogCodeSourceLocalityCutoverFacadeV1::verify(
            CodeSourceLocalityCutoverVerifyRequestV1 { layout, config },
        )
        .unwrap();
        assert_eq!(verified.status, "verified");
    }
}
