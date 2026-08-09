//! Offline project-render overlap report, quiet-window gate, and runtime cut.
//!
//! Applying the marker requires exact checkout-owned render receipts for all
//! three visibility views and an unchanged daemon `RenderFileProvider`
//! checkout baseline for a nontrivial quiet window.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use bbox_config::config::Config;
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::json_store::{atomic_write_json_locked, with_store_lock};
use bbox_corpus_core::project_catalog::{ProjectId, ProjectScope};
use bbox_knowledge::knowledge::ProjectRenderViewV1;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::checkout_access::{
    CheckoutAccessKind, CheckoutAccessObservations, CheckoutAccessTargetCounter,
};
use crate::project_catalog_migration::ProjectCatalogMigrationResolvedLayoutV1;
use crate::project_catalog_store::ProjectCatalogStore;
use crate::render_locality_observations::{
    RenderLocalityCompletionV1, RenderLocalityObservationSnapshotV1, RenderLocalityObservationsV1,
};

const REPORT_VERSION: u32 = 1;
const MARKER_VERSION: u32 = 1;
const RECEIPT_VERSION: u32 = 1;
pub const MIN_RENDER_LOCALITY_QUIET_SECS: u64 = 5 * 60;
pub const RENDER_LOCALITY_CUTOVER_MARKER_FILE: &str = "render-locality-cutover-marker.json";
const MAX_ARTIFACT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderLocalityCutoverRowV1 {
    pub project_id: ProjectId,
    pub scope: PublishedScope,
    pub producer_id: String,
    pub completions: Vec<RenderLocalityCompletionV1>,
    pub checkout_baselines: Vec<CheckoutAccessTargetCounter>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderLocalityCutoverReportV1 {
    pub version: u32,
    pub generated_at: String,
    pub generated_at_unix_secs: u64,
    pub min_quiet_secs: u64,
    pub catalog_epoch: u64,
    pub catalog_sha256: String,
    pub checkout_observation_sequence: u64,
    pub render_observation_sequence: u64,
    pub rows: Vec<RenderLocalityCutoverRowV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderLocalityCutoverMarkerV1 {
    pub version: u32,
    pub applied_at: String,
    pub report_sha256: String,
    pub catalog_epoch: u64,
    pub catalog_sha256: String,
    pub rows: Vec<RenderLocalityCutoverRowV1>,
    pub checksum_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderLocalityCutoverReceiptV1 {
    pub version: u32,
    pub status: String,
    pub marker_checksum_sha256: Option<String>,
    pub project_count: u64,
    pub checkout_observation_sequence: u64,
    pub render_observation_sequence: u64,
}

pub struct RenderLocalityCutoverPreflightRequestV1 {
    pub layout: ProjectCatalogMigrationResolvedLayoutV1,
    pub config: Config,
    pub report_path: PathBuf,
    pub project_ids: Vec<ProjectId>,
    pub min_quiet_secs: u64,
    pub generated_at: String,
}

pub struct RenderLocalityCutoverApplyRequestV1 {
    pub layout: ProjectCatalogMigrationResolvedLayoutV1,
    pub config: Config,
    pub report_path: PathBuf,
    pub applied_at: String,
}

pub struct RenderLocalityCutoverVerifyRequestV1 {
    pub layout: ProjectCatalogMigrationResolvedLayoutV1,
}

#[derive(Debug, Clone, Default)]
pub struct RenderLocalityCutoverRuntimeV1 {
    rows: BTreeMap<ProjectId, RenderLocalityCutoverRowV1>,
}

impl RenderLocalityCutoverRuntimeV1 {
    pub fn open(state_dir: &Path) -> Result<Self> {
        let path = state_dir.join(RENDER_LOCALITY_CUTOVER_MARKER_FILE);
        let Some(marker) = read_json_optional::<RenderLocalityCutoverMarkerV1>(&path)? else {
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

    #[cfg(feature = "test-support")]
    pub fn governed_for_test(project_id: &str) -> Self {
        let project_id = ProjectId::parse(project_id).expect("valid test project id");
        let completions = [
            ProjectRenderViewV1::Published,
            ProjectRenderViewV1::Own,
            ProjectRenderViewV1::All,
        ]
        .into_iter()
        .enumerate()
        .map(|(index, view)| RenderLocalityCompletionV1 {
            project_id: project_id.as_str().to_string(),
            view,
            receipt_sha256: "a".repeat(64),
            all_providers: true,
            dry_run: false,
            provider_count: 3,
            written_count: 3,
            refused_count: 0,
            sequence: index as u64 + 1,
            observed_at_unix_secs: 1,
        })
        .collect();
        let row = RenderLocalityCutoverRowV1 {
            project_id: project_id.clone(),
            scope: PublishedScope::try_new("test", ".").unwrap(),
            producer_id: "test".into(),
            completions,
            checkout_baselines: vec![],
        };
        Self {
            rows: BTreeMap::from([(project_id, row)]),
        }
    }
}

pub struct ProjectCatalogRenderLocalityCutoverFacadeV1;

impl ProjectCatalogRenderLocalityCutoverFacadeV1 {
    pub fn preflight(
        request: RenderLocalityCutoverPreflightRequestV1,
    ) -> Result<RenderLocalityCutoverReceiptV1> {
        if request.min_quiet_secs < MIN_RENDER_LOCALITY_QUIET_SECS {
            bail!(
                "render locality quiet window must be at least {MIN_RENDER_LOCALITY_QUIET_SECS} seconds"
            );
        }
        if request.project_ids.is_empty() {
            bail!("render locality cutover requires at least one explicit project id");
        }
        let selected = request.project_ids.into_iter().collect::<BTreeSet<_>>();
        let catalog = open_catalog(&request.layout)?;
        let catalog_sha256 = sha256_json(&catalog)?;
        let render = RenderLocalityObservationsV1::open(
            request
                .layout
                .bro_home
                .join("render-locality-observations.json"),
        )?
        .snapshot();
        let checkout = CheckoutAccessObservations::open(
            request
                .layout
                .bro_home
                .join("checkout-access-observations.json"),
        )?
        .health();
        let mut rows = Vec::with_capacity(selected.len());
        for project_id in selected {
            let project = catalog
                .projects
                .get(&project_id)
                .with_context(|| format!("unknown cutover project {project_id}"))?;
            let ProjectScope::Published(scope) = &project.scope else {
                bail!("render locality cutover requires a Published project: {project_id}");
            };
            let producer_id = assigned_producer(&request.config, scope)?;
            let completions = required_completions(&render, project_id.as_str())?;
            let checkout_baselines = render_checkout_counters(&checkout, project_id.as_str());
            rows.push(RenderLocalityCutoverRowV1 {
                project_id,
                scope: scope.clone(),
                producer_id,
                completions,
                checkout_baselines,
            });
        }
        let report = RenderLocalityCutoverReportV1 {
            version: REPORT_VERSION,
            generated_at: request.generated_at,
            generated_at_unix_secs: now_unix_secs(),
            min_quiet_secs: request.min_quiet_secs,
            catalog_epoch: catalog.epoch,
            catalog_sha256,
            checkout_observation_sequence: checkout.sequence,
            render_observation_sequence: render.sequence,
            rows,
        };
        validate_report(&report)?;
        write_json(&request.report_path, &report)?;
        Ok(RenderLocalityCutoverReceiptV1 {
            version: RECEIPT_VERSION,
            status: "preflight_clean".into(),
            marker_checksum_sha256: None,
            project_count: report.rows.len() as u64,
            checkout_observation_sequence: report.checkout_observation_sequence,
            render_observation_sequence: report.render_observation_sequence,
        })
    }

    pub fn apply(
        request: RenderLocalityCutoverApplyRequestV1,
    ) -> Result<RenderLocalityCutoverReceiptV1> {
        let report: RenderLocalityCutoverReportV1 = read_json_required(&request.report_path)?;
        validate_report(&report)?;
        let elapsed = now_unix_secs().saturating_sub(report.generated_at_unix_secs);
        if elapsed < report.min_quiet_secs {
            bail!(
                "render locality quiet window is incomplete: {elapsed}/{} seconds",
                report.min_quiet_secs
            );
        }
        let catalog = open_catalog(&request.layout)?;
        if catalog.epoch != report.catalog_epoch || sha256_json(&catalog)? != report.catalog_sha256
        {
            bail!("render locality cutover report is stale against the current catalog");
        }
        let render = RenderLocalityObservationsV1::open(
            request
                .layout
                .bro_home
                .join("render-locality-observations.json"),
        )?
        .snapshot();
        let checkout = CheckoutAccessObservations::open(
            request
                .layout
                .bro_home
                .join("checkout-access-observations.json"),
        )?
        .health();
        for row in &report.rows {
            if assigned_producer(&request.config, &row.scope)? != row.producer_id {
                bail!(
                    "render locality producer assignment changed for {}",
                    row.project_id
                );
            }
            let current_completions = required_completions(&render, row.project_id.as_str())?;
            if !same_completion_evidence(&current_completions, &row.completions) {
                bail!("render locality completion changed during the quiet window");
            }
            if render_checkout_counters(&checkout, row.project_id.as_str())
                != row.checkout_baselines
            {
                bail!(
                    "daemon-side render checkout access changed during the quiet window for {}",
                    row.project_id
                );
            }
        }
        let marker_path = request
            .layout
            .state_dir
            .join(RENDER_LOCALITY_CUTOVER_MARKER_FILE);
        if marker_path.exists() {
            bail!("render locality cutover marker already exists; verify it instead");
        }
        let report_sha256 = sha256_json(&report)?;
        let mut marker = RenderLocalityCutoverMarkerV1 {
            version: MARKER_VERSION,
            applied_at: request.applied_at,
            report_sha256,
            catalog_epoch: report.catalog_epoch,
            catalog_sha256: report.catalog_sha256,
            rows: report.rows,
            checksum_sha256: String::new(),
        };
        marker.checksum_sha256 = marker_checksum(&marker)?;
        write_json(&marker_path, &marker)?;
        Ok(RenderLocalityCutoverReceiptV1 {
            version: RECEIPT_VERSION,
            status: "applied".into(),
            marker_checksum_sha256: Some(marker.checksum_sha256),
            project_count: marker.rows.len() as u64,
            checkout_observation_sequence: checkout.sequence,
            render_observation_sequence: render.sequence,
        })
    }

    pub fn verify(
        request: RenderLocalityCutoverVerifyRequestV1,
    ) -> Result<RenderLocalityCutoverReceiptV1> {
        let path = request
            .layout
            .state_dir
            .join(RENDER_LOCALITY_CUTOVER_MARKER_FILE);
        let marker: RenderLocalityCutoverMarkerV1 = read_json_required(&path)?;
        validate_marker(&marker)?;
        let runtime = RenderLocalityCutoverRuntimeV1::open(&request.layout.state_dir)?;
        if runtime.project_ids().len() != marker.rows.len() {
            bail!("render locality runtime projection does not match the marker");
        }
        let checkout = CheckoutAccessObservations::open(
            request
                .layout
                .bro_home
                .join("checkout-access-observations.json"),
        )?
        .health();
        let render = RenderLocalityObservationsV1::open(
            request
                .layout
                .bro_home
                .join("render-locality-observations.json"),
        )?
        .snapshot();
        Ok(RenderLocalityCutoverReceiptV1 {
            version: RECEIPT_VERSION,
            status: "verified".into(),
            marker_checksum_sha256: Some(marker.checksum_sha256),
            project_count: marker.rows.len() as u64,
            checkout_observation_sequence: checkout.sequence,
            render_observation_sequence: render.sequence,
        })
    }
}

fn open_catalog(
    layout: &ProjectCatalogMigrationResolvedLayoutV1,
) -> Result<bbox_corpus_core::project_catalog::CatalogSnapshotV2> {
    Ok(ProjectCatalogStore::open_existing(layout.projects_path())?
        .snapshot()?
        .catalog()
        .as_ref()
        .clone())
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
        [] => bail!("no configured producer owns render scope {scope:?}"),
        _ => bail!("multiple configured producers own render scope {scope:?}"),
    }
}

fn required_completions(
    snapshot: &RenderLocalityObservationSnapshotV1,
    project_id: &str,
) -> Result<Vec<RenderLocalityCompletionV1>> {
    let mut completions = Vec::new();
    for view in [
        ProjectRenderViewV1::Published,
        ProjectRenderViewV1::Own,
        ProjectRenderViewV1::All,
    ] {
        let completion = snapshot
            .completions
            .iter()
            .find(|completion| completion.project_id == project_id && completion.view == view)
            .filter(|completion| {
                completion.all_providers
                    && !completion.dry_run
                    && completion.provider_count == 3
                    && completion.written_count == 3
                    && completion.refused_count == 0
            })
            .cloned()
            .with_context(|| {
                format!(
                    "successful all-provider render locality completion is missing for {project_id} {view:?}"
                )
            })?;
        completions.push(completion);
    }
    Ok(completions)
}

fn render_checkout_counters(
    health: &crate::checkout_access::CheckoutAccessHealth,
    project_id: &str,
) -> Vec<CheckoutAccessTargetCounter> {
    health
        .target_counters
        .iter()
        .filter(|counter| {
            counter.project_id == project_id
                && counter.kind == CheckoutAccessKind::RenderFileProvider
        })
        .cloned()
        .collect()
}

fn same_completion_evidence(
    current: &[RenderLocalityCompletionV1],
    baseline: &[RenderLocalityCompletionV1],
) -> bool {
    current.len() == baseline.len()
        && current.iter().zip(baseline).all(|(current, baseline)| {
            current.project_id == baseline.project_id
                && current.view == baseline.view
                && current.receipt_sha256 == baseline.receipt_sha256
                && current.all_providers == baseline.all_providers
                && current.dry_run == baseline.dry_run
                && current.provider_count == baseline.provider_count
                && current.written_count == baseline.written_count
                && current.refused_count == baseline.refused_count
        })
}

fn validate_report(report: &RenderLocalityCutoverReportV1) -> Result<()> {
    if report.version != REPORT_VERSION
        || report.min_quiet_secs < MIN_RENDER_LOCALITY_QUIET_SECS
        || report.rows.is_empty()
    {
        bail!("invalid render locality cutover report");
    }
    validate_sha256(&report.catalog_sha256)?;
    let mut seen = BTreeSet::new();
    for row in &report.rows {
        if !seen.insert(row.project_id.clone()) {
            bail!("invalid render locality cutover row");
        }
        validate_row(row)?;
    }
    Ok(())
}

fn validate_marker(marker: &RenderLocalityCutoverMarkerV1) -> Result<()> {
    if marker.version != MARKER_VERSION || marker.rows.is_empty() {
        bail!("invalid render locality cutover marker");
    }
    validate_sha256(&marker.report_sha256)?;
    validate_sha256(&marker.catalog_sha256)?;
    validate_sha256(&marker.checksum_sha256)?;
    if marker_checksum(marker)? != marker.checksum_sha256 {
        bail!("render locality cutover marker checksum mismatch");
    }
    let mut seen = BTreeSet::new();
    for row in &marker.rows {
        if !seen.insert(row.project_id.clone()) {
            bail!("duplicate render locality cutover project");
        }
        validate_row(row)?;
    }
    Ok(())
}

fn validate_row(row: &RenderLocalityCutoverRowV1) -> Result<()> {
    if row.producer_id.trim().is_empty()
        || row.producer_id.len() > 256
        || row.completions.len() != 3
        || row.completions.iter().any(|completion| {
            completion.project_id != row.project_id.as_str()
                || !completion.all_providers
                || completion.dry_run
                || completion.provider_count != 3
                || completion.written_count != 3
                || completion.refused_count != 0
        })
        || row.checkout_baselines.iter().any(|counter| {
            counter.project_id != row.project_id.as_str()
                || counter.kind != CheckoutAccessKind::RenderFileProvider
        })
    {
        bail!("invalid render locality cutover row");
    }
    let views = row
        .completions
        .iter()
        .map(|completion| completion.view)
        .collect::<BTreeSet<_>>();
    if views
        != BTreeSet::from([
            ProjectRenderViewV1::Published,
            ProjectRenderViewV1::Own,
            ProjectRenderViewV1::All,
        ])
    {
        bail!("render locality cutover row does not cover every view");
    }
    Ok(())
}

fn marker_checksum(marker: &RenderLocalityCutoverMarkerV1) -> Result<String> {
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(&(
        marker.version,
        &marker.applied_at,
        &marker.report_sha256,
        marker.catalog_epoch,
        &marker.catalog_sha256,
        &marker.rows,
    ))?)))
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
        bail!("render locality artifact exceeds its byte bound");
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
                bail!("render locality artifact exceeds its byte bound");
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
    use crate::checkout_access::{
        CheckoutAccessBroker, CheckoutAccessIntent, CheckoutAccessRequest,
        CheckoutAccessSourceLane, CheckoutAttachmentSelector, DenyCheckoutAccess,
    };
    use bbox_config::config::{self, CodeCollectionProducerConfig};
    use bbox_corpus_core::project_catalog::{CorpusProject, ProjectScope};
    use bbox_knowledge::knowledge::{
        Approval, Category, KnowledgeEntry, PROJECT_RENDER_TRANSPORT_SCOPE,
        PROJECT_RENDER_TRANSPORT_VERSION, Priority, ProjectRenderPlanV1, Scope, Status,
        execute_project_render_plan,
    };
    use std::sync::Arc;

    const PROJECT: &str = "p_00000000000000000000000000000001";

    fn test_config(root: &Path, scope: &PublishedScope) -> Config {
        let config_path = root.join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "[paths]\nstate_dir = {:?}\nvectors_dir = {:?}\n",
                root.join("live"),
                root.join("live").join("vectors")
            ),
        )
        .unwrap();
        let mut config = config::load_with(config::LoadOptions {
            config_path: Some(config_path),
            ..Default::default()
        })
        .unwrap();
        config
            .code_collection
            .producers
            .push(CodeCollectionProducerConfig {
                producer_id: "producer-a".into(),
                token_file: root.join("producer.token"),
                scopes: vec![scope.clone()],
            });
        config
    }

    fn render_entry() -> KnowledgeEntry {
        KnowledgeEntry {
            id: "render-cutover".into(),
            title: "Render cutover".into(),
            content: "render cutover positive control".into(),
            cluster: None,
            variants: Default::default(),
            category: Category::Convention,
            scope: Scope::Project,
            project: Some(PROJECT_RENDER_TRANSPORT_SCOPE.into()),
            project_id: Some(PROJECT.into()),
            providers: vec![],
            priority: Priority::Standard,
            weight: 100,
            status: Status::Active,
            approval: Approval::UserConfirmed,
            render: true,
            decay: false,
            review_at: None,
            supersedes: None,
            links: vec![],
            rationale: None,
            expires_at: None,
            source: "test".into(),
            created_at: "unix:1".into(),
            updated_at: "unix:1".into(),
            recall_count: 0,
            last_recalled: None,
        }
    }

    fn record_completions(
        layout: &ProjectCatalogMigrationResolvedLayoutV1,
        scope: &PublishedScope,
    ) {
        let observations = RenderLocalityObservationsV1::open(
            layout.bro_home.join("render-locality-observations.json"),
        )
        .unwrap();
        let checkout = tempfile::tempdir().unwrap();
        let root = checkout.path().canonicalize().unwrap();
        for view in [
            ProjectRenderViewV1::Published,
            ProjectRenderViewV1::Own,
            ProjectRenderViewV1::All,
        ] {
            let plan = ProjectRenderPlanV1 {
                version: PROJECT_RENDER_TRANSPORT_VERSION,
                project_id: PROJECT.into(),
                scope: scope.clone(),
                workspace_id: "workspace".into(),
                provider: None,
                dry_run: false,
                view,
                requested_scope: "project".into(),
                entries: vec![render_entry()],
                diagnostics: None,
            };
            let execution = execute_project_render_plan(&plan, &root, scope, "workspace").unwrap();
            observations
                .record_completed(&plan, &execution.receipt)
                .unwrap();
        }
    }

    fn age_report(path: &Path) {
        let mut report: RenderLocalityCutoverReportV1 = read_json_required(path).unwrap();
        report.generated_at_unix_secs = 0;
        write_json(path, &report).unwrap();
    }

    #[test]
    fn cutover_requires_all_views_and_a_quiet_render_checkout_baseline() {
        let _guard = bbox_util::util::test_env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let scope = PublishedScope::try_new("repo-a", ".").unwrap();
        let config = test_config(&root, &scope);
        let layout = ProjectCatalogMigrationResolvedLayoutV1::from_rehearsal_root(
            &root.join("rehearsal"),
            &config,
        )
        .unwrap();
        std::fs::create_dir_all(&layout.bro_home).unwrap();
        let store = ProjectCatalogStore::initialize_empty(layout.projects_path()).unwrap();
        let project_id = ProjectId::parse(PROJECT).unwrap();
        let epoch = store.snapshot().unwrap().epoch();
        store
            .transact(epoch, |catalog, _attachments| {
                catalog.projects.insert(
                    project_id.clone(),
                    CorpusProject {
                        project_id: project_id.clone(),
                        scope: ProjectScope::Published(scope.clone()),
                        operator_aliases: Default::default(),
                        nominated_aliases: Default::default(),
                        display_name: "project".into(),
                        created_at: "unix:1".into(),
                        registered_at_compat: None,
                        repo_history: None,
                        languages: Default::default(),
                    },
                );
                Ok(())
            })
            .unwrap();

        record_completions(&layout, &scope);
        let report_path = root.join("render-cutover-report.json");
        let preflight = |age: bool| {
            ProjectCatalogRenderLocalityCutoverFacadeV1::preflight(
                RenderLocalityCutoverPreflightRequestV1 {
                    layout: layout.clone(),
                    config: config.clone(),
                    report_path: report_path.clone(),
                    project_ids: vec![project_id.clone()],
                    min_quiet_secs: MIN_RENDER_LOCALITY_QUIET_SECS,
                    generated_at: "unix:1".into(),
                },
            )
            .unwrap();
            if age {
                age_report(&report_path);
            }
        };
        preflight(false);
        let error = ProjectCatalogRenderLocalityCutoverFacadeV1::apply(
            RenderLocalityCutoverApplyRequestV1 {
                layout: layout.clone(),
                config: config.clone(),
                report_path: report_path.clone(),
                applied_at: "unix:2".into(),
            },
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("quiet window is incomplete"), "{error}");
        // Repeating an identical checkout-owned render is healthy traffic and
        // advances only observation sequence/time, not parity evidence.
        record_completions(&layout, &scope);
        age_report(&report_path);

        let checkout_observations = CheckoutAccessObservations::open(
            layout.bro_home.join("checkout-access-observations.json"),
        )
        .unwrap();
        let broker = CheckoutAccessBroker::new(Arc::new(DenyCheckoutAccess), checkout_observations);
        let _ = broker.acquire(CheckoutAccessRequest {
            project_id: PROJECT.into(),
            attachment: CheckoutAttachmentSelector::Selected,
            expected_scope: Some(scope.clone()),
            kind: CheckoutAccessKind::RenderFileProvider,
            intent: CheckoutAccessIntent::Write,
            source_lane: CheckoutAccessSourceLane::NativeAttachment,
        });
        let error = ProjectCatalogRenderLocalityCutoverFacadeV1::apply(
            RenderLocalityCutoverApplyRequestV1 {
                layout: layout.clone(),
                config: config.clone(),
                report_path: report_path.clone(),
                applied_at: "unix:2".into(),
            },
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("checkout access changed"), "{error}");

        preflight(true);
        let receipt = ProjectCatalogRenderLocalityCutoverFacadeV1::apply(
            RenderLocalityCutoverApplyRequestV1 {
                layout: layout.clone(),
                config,
                report_path,
                applied_at: "unix:3".into(),
            },
        )
        .unwrap();
        assert_eq!(receipt.status, "applied");
        assert_eq!(
            ProjectCatalogRenderLocalityCutoverFacadeV1::verify(
                RenderLocalityCutoverVerifyRequestV1 {
                    layout: layout.clone(),
                }
            )
            .unwrap()
            .status,
            "verified"
        );
        assert!(
            RenderLocalityCutoverRuntimeV1::open(&layout.state_dir)
                .unwrap()
                .transport_governed(PROJECT)
        );
    }
}
