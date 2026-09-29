//! Offline project-render overlap report, quiet-window gate, and runtime cut.
//!
//! Applying the marker requires exact checkout-owned render receipts for all
//! three visibility views and an unchanged daemon `RenderFileProvider`
//! checkout baseline for a nontrivial quiet window.
//!
//! The ceremony is re-runnable. With a marker installed, preflight carries
//! every row whose project is still in the catalog, drops rows whose project
//! left it, and proves only the explicitly selected projects. Apply supersedes
//! the reviewed predecessor with the union of carried and proved rows, or
//! removes the marker when no row remains, because the marker format the
//! daemon loads admits no empty row set.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use bbox_config::config::Config;
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::json_store::{
    acquire_store_lock_nofollow, atomic_write_json_locked, with_store_lock,
};
use bbox_corpus_core::project_catalog::{CatalogSnapshotV2, ProjectId, ProjectScope};
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

/// Why a predecessor marker row is not carried into the next marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RenderLocalityDroppedRowReasonV1 {
    /// The row's project no longer exists in the catalog, so no render can
    /// resolve it and no re-cutover can ever make it current.
    ProjectAbsentFromCatalog,
}

/// One predecessor marker row the next marker omits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderLocalityDroppedRowV1 {
    pub project_id: ProjectId,
    pub scope: PublishedScope,
    pub producer_id: String,
    pub reason: RenderLocalityDroppedRowReasonV1,
}

/// A reviewed preflight. `rows` holds the projects this run proves; with a
/// predecessor marker, `carried_forward_rows` and `dropped_rows` partition
/// the predecessor's remaining rows, and the next marker covers `rows` plus
/// `carried_forward_rows`.
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predecessor_marker_checksum: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub carried_forward_rows: Vec<RenderLocalityCutoverRowV1>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dropped_rows: Vec<RenderLocalityDroppedRowV1>,
    /// Published catalog projects the next marker would still not cover.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uncovered_projects: Vec<ProjectId>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predecessor_marker_checksum_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub carried_forward_row_count: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub dropped_row_count: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub uncovered_project_count: u64,
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

/// Path-free identity of the validated marker a runtime was opened from.
/// `checksum_sha256` is the value the apply and verify receipts report as
/// `marker_checksum_sha256`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RenderLocalityCutoverMarkerIdentityV1 {
    pub applied_at: String,
    pub report_sha256: String,
    pub catalog_epoch: u64,
    pub catalog_sha256: String,
    pub checksum_sha256: String,
}

#[derive(Debug, Clone, Default)]
pub struct RenderLocalityCutoverRuntimeV1 {
    rows: BTreeMap<ProjectId, RenderLocalityCutoverRowV1>,
    /// `Some` exactly when a validated marker was loaded.
    marker: Option<RenderLocalityCutoverMarkerIdentityV1>,
}

impl RenderLocalityCutoverRuntimeV1 {
    pub fn open(state_dir: &Path) -> Result<Self> {
        let path = state_dir.join(RENDER_LOCALITY_CUTOVER_MARKER_FILE);
        let Some(marker) = read_json_optional::<RenderLocalityCutoverMarkerV1>(&path)? else {
            return Ok(Self::default());
        };
        Self::from_marker(marker)
    }

    fn from_marker(marker: RenderLocalityCutoverMarkerV1) -> Result<Self> {
        validate_marker(&marker)?;
        Ok(Self {
            marker: Some(RenderLocalityCutoverMarkerIdentityV1 {
                applied_at: marker.applied_at,
                report_sha256: marker.report_sha256,
                catalog_epoch: marker.catalog_epoch,
                catalog_sha256: marker.catalog_sha256,
                checksum_sha256: marker.checksum_sha256,
            }),
            rows: marker
                .rows
                .into_iter()
                .map(|row| (row.project_id.clone(), row))
                .collect(),
        })
    }

    /// Identity of the loaded marker; `None` when no marker was installed.
    pub fn marker_identity(&self) -> Option<&RenderLocalityCutoverMarkerIdentityV1> {
        self.marker.as_ref()
    }

    /// Governed rows in project-id order.
    pub fn rows(&self) -> impl Iterator<Item = &RenderLocalityCutoverRowV1> {
        self.rows.values()
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
        Self::governed_rows_for_test(&[(
            project_id,
            PublishedScope::try_new("test", ".").unwrap(),
            "test",
        )])
    }

    /// A runtime loaded from a checksummed marker governing exactly these
    /// `(project_id, scope, producer_id)` rows, through the same validation
    /// startup applies.
    #[cfg(feature = "test-support")]
    pub fn governed_rows_for_test(rows: &[(&str, PublishedScope, &str)]) -> Self {
        let rows = rows
            .iter()
            .map(|(project_id, scope, producer_id)| {
                let project_id = ProjectId::parse(*project_id).expect("valid test project id");
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
                    issued_at_ms: None,
                })
                .collect();
                RenderLocalityCutoverRowV1 {
                    project_id,
                    scope: scope.clone(),
                    producer_id: (*producer_id).into(),
                    completions,
                    checkout_baselines: vec![],
                }
            })
            .collect();
        let mut marker = RenderLocalityCutoverMarkerV1 {
            version: MARKER_VERSION,
            applied_at: "2026-01-01T00:00:00Z".into(),
            report_sha256: "c".repeat(64),
            catalog_epoch: 1,
            catalog_sha256: "d".repeat(64),
            rows,
            checksum_sha256: String::new(),
        };
        marker.checksum_sha256 = marker_checksum(&marker).expect("test marker checksum");
        Self::from_marker(marker).expect("valid test marker")
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
        let predecessor = load_marker(&request.layout.state_dir)?;
        if request.project_ids.is_empty() && predecessor.is_none() {
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
        let (carried_forward_rows, dropped_rows) =
            partition_predecessor_rows(predecessor.as_ref(), &catalog, &selected);
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
        let uncovered_projects = uncovered_projects(&catalog, &rows, &carried_forward_rows);
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
            predecessor_marker_checksum: predecessor.map(|marker| marker.checksum_sha256),
            carried_forward_rows,
            dropped_rows,
            uncovered_projects,
        };
        validate_report(&report)?;
        write_json(&request.report_path, &report)?;
        Ok(RenderLocalityCutoverReceiptV1 {
            version: RECEIPT_VERSION,
            status: "preflight_clean".into(),
            marker_checksum_sha256: None,
            project_count: (report.rows.len() + report.carried_forward_rows.len()) as u64,
            checkout_observation_sequence: report.checkout_observation_sequence,
            render_observation_sequence: report.render_observation_sequence,
            predecessor_marker_checksum_sha256: report.predecessor_marker_checksum.clone(),
            carried_forward_row_count: report.carried_forward_rows.len() as u64,
            dropped_row_count: report.dropped_rows.len() as u64,
            uncovered_project_count: report.uncovered_projects.len() as u64,
        })
    }

    pub fn apply(
        request: RenderLocalityCutoverApplyRequestV1,
    ) -> Result<RenderLocalityCutoverReceiptV1> {
        let report: RenderLocalityCutoverReportV1 = read_json_required(&request.report_path)?;
        validate_report(&report)?;
        // The quiet window measures the projects this run proves. Carried
        // rows keep the evidence their original apply accepted.
        let elapsed = now_unix_secs().saturating_sub(report.generated_at_unix_secs);
        if !report.rows.is_empty() && elapsed < report.min_quiet_secs {
            bail!(
                "render locality quiet window is incomplete: {elapsed}/{} seconds",
                report.min_quiet_secs
            );
        }
        let catalog_store = ProjectCatalogStore::open_existing(request.layout.projects_path())?;
        let _catalog_lock = acquire_store_lock_nofollow(request.layout.projects_path())?;
        let catalog = catalog_store.snapshot()?.catalog().as_ref().clone();
        if catalog.epoch != report.catalog_epoch || sha256_json(&catalog)? != report.catalog_sha256
        {
            bail!("render locality cutover report is stale against the current catalog");
        }
        let predecessor = load_marker(&request.layout.state_dir)?;
        if predecessor.as_ref().map(|marker| &marker.checksum_sha256)
            != report.predecessor_marker_checksum.as_ref()
        {
            bail!("render locality cutover marker changed since preflight; run a new preflight");
        }
        let proved = report
            .rows
            .iter()
            .map(|row| row.project_id.clone())
            .collect::<BTreeSet<_>>();
        let (carried_forward_rows, dropped_rows) =
            partition_predecessor_rows(predecessor.as_ref(), &catalog, &proved);
        if carried_forward_rows != report.carried_forward_rows
            || dropped_rows != report.dropped_rows
        {
            bail!(
                "reviewed carry-forward or dropped render locality rows do not match the current predecessor"
            );
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
        let report_sha256 = sha256_json(&report)?;
        let rows = successor_rows(&report.rows, &report.carried_forward_rows);
        let installed = if rows.is_empty() {
            // Every predecessor row was dropped and nothing new is proved.
            // The daemon refuses an empty marker, so the successor is the
            // absent marker, which governs no project.
            remove_marker(&marker_path)?;
            None
        } else {
            let mut marker = RenderLocalityCutoverMarkerV1 {
                version: MARKER_VERSION,
                applied_at: request.applied_at,
                report_sha256,
                catalog_epoch: report.catalog_epoch,
                catalog_sha256: report.catalog_sha256.clone(),
                rows,
                checksum_sha256: String::new(),
            };
            marker.checksum_sha256 = marker_checksum(&marker)?;
            validate_marker(&marker)?;
            write_json(&marker_path, &marker)?;
            Some(marker)
        };
        Ok(RenderLocalityCutoverReceiptV1 {
            version: RECEIPT_VERSION,
            status: if installed.is_some() {
                "applied"
            } else {
                "retired"
            }
            .into(),
            project_count: installed
                .as_ref()
                .map_or(0, |marker| marker.rows.len() as u64),
            marker_checksum_sha256: installed.map(|marker| marker.checksum_sha256),
            checkout_observation_sequence: checkout.sequence,
            render_observation_sequence: render.sequence,
            predecessor_marker_checksum_sha256: report.predecessor_marker_checksum,
            carried_forward_row_count: report.carried_forward_rows.len() as u64,
            dropped_row_count: report.dropped_rows.len() as u64,
            uncovered_project_count: report.uncovered_projects.len() as u64,
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
            predecessor_marker_checksum_sha256: None,
            carried_forward_row_count: 0,
            dropped_row_count: 0,
            uncovered_project_count: 0,
        })
    }
}

fn open_catalog(layout: &ProjectCatalogMigrationResolvedLayoutV1) -> Result<CatalogSnapshotV2> {
    Ok(ProjectCatalogStore::open_existing(layout.projects_path())?
        .snapshot()?
        .catalog()
        .as_ref()
        .clone())
}

fn load_marker(state_dir: &Path) -> Result<Option<RenderLocalityCutoverMarkerV1>> {
    let path = state_dir.join(RENDER_LOCALITY_CUTOVER_MARKER_FILE);
    let Some(marker) = read_json_optional::<RenderLocalityCutoverMarkerV1>(&path)? else {
        return Ok(None);
    };
    validate_marker(&marker)?;
    Ok(Some(marker))
}

/// Split the predecessor's rows that this run does not re-prove into rows
/// carried forward unchanged and rows dropped because their project left the
/// catalog. A carried row keeps the completions its original apply accepted:
/// once governed, losing the producer, binding, source, or receipt never
/// reopens the daemon adapter (RL-D5 in the render locality design), so the
/// row needs no re-proof. A row for a catalog project is carried whatever its
/// current scope.
fn partition_predecessor_rows(
    predecessor: Option<&RenderLocalityCutoverMarkerV1>,
    catalog: &CatalogSnapshotV2,
    proved: &BTreeSet<ProjectId>,
) -> (
    Vec<RenderLocalityCutoverRowV1>,
    Vec<RenderLocalityDroppedRowV1>,
) {
    let mut carried = Vec::new();
    let mut dropped = Vec::new();
    for row in predecessor.into_iter().flat_map(|marker| &marker.rows) {
        if proved.contains(&row.project_id) {
            continue;
        }
        if catalog.projects.contains_key(&row.project_id) {
            carried.push(row.clone());
        } else {
            dropped.push(RenderLocalityDroppedRowV1 {
                project_id: row.project_id.clone(),
                scope: row.scope.clone(),
                producer_id: row.producer_id.clone(),
                reason: RenderLocalityDroppedRowReasonV1::ProjectAbsentFromCatalog,
            });
        }
    }
    (carried, dropped)
}

fn uncovered_projects(
    catalog: &CatalogSnapshotV2,
    rows: &[RenderLocalityCutoverRowV1],
    carried: &[RenderLocalityCutoverRowV1],
) -> Vec<ProjectId> {
    let covered = rows
        .iter()
        .chain(carried)
        .map(|row| &row.project_id)
        .collect::<BTreeSet<_>>();
    catalog
        .projects
        .values()
        .filter(|project| matches!(project.scope, ProjectScope::Published(_)))
        .filter(|project| !covered.contains(&project.project_id))
        .map(|project| project.project_id.clone())
        .collect()
}

fn successor_rows(
    proved: &[RenderLocalityCutoverRowV1],
    carried: &[RenderLocalityCutoverRowV1],
) -> Vec<RenderLocalityCutoverRowV1> {
    proved
        .iter()
        .chain(carried)
        .map(|row| (row.project_id.clone(), row.clone()))
        .collect::<BTreeMap<_, _>>()
        .into_values()
        .collect()
}

// Runs only in the offline cutover command, never on a tokio worker.
#[allow(clippy::disallowed_methods)]
fn remove_marker(path: &Path) -> Result<()> {
    with_store_lock(path, || {
        std::fs::remove_file(path)?;
        std::fs::File::open(path.parent().context("marker path has no parent")?)?.sync_all()?;
        Ok(())
    })
}

fn is_zero(value: &u64) -> bool {
    *value == 0
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
    if report.version != REPORT_VERSION || report.min_quiet_secs < MIN_RENDER_LOCALITY_QUIET_SECS {
        bail!("invalid render locality cutover report");
    }
    validate_sha256(&report.catalog_sha256)?;
    match &report.predecessor_marker_checksum {
        // A first cutover proves at least one project and has no
        // predecessor rows to carry or drop.
        None => {
            if report.rows.is_empty()
                || !report.carried_forward_rows.is_empty()
                || !report.dropped_rows.is_empty()
            {
                bail!("invalid render locality cutover report");
            }
        }
        Some(checksum) => validate_sha256(checksum)?,
    }
    let mut seen = BTreeSet::new();
    for row in report.rows.iter().chain(&report.carried_forward_rows) {
        if !seen.insert(row.project_id.clone()) {
            bail!("invalid render locality cutover row");
        }
        validate_row(row)?;
    }
    for dropped in &report.dropped_rows {
        if !seen.insert(dropped.project_id.clone()) {
            bail!("invalid render locality cutover dropped row");
        }
    }
    for project_id in &report.uncovered_projects {
        if !seen.insert(project_id.clone()) {
            bail!("invalid render locality cutover uncovered project");
        }
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
        Category, KnowledgeEntry, PROJECT_RENDER_TRANSPORT_SCOPE, PROJECT_RENDER_TRANSPORT_VERSION,
        Priority, ProjectRenderPlanV1, Scope, execute_project_render_plan,
    };
    use std::sync::Arc;

    const PROJECT: &str = "p_00000000000000000000000000000001";

    fn test_config(root: &Path, scope: &PublishedScope) -> Config {
        test_config_for(root, std::slice::from_ref(scope))
    }

    fn test_config_for(root: &Path, scopes: &[PublishedScope]) -> Config {
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
                token_files: Vec::new(),
                scopes: scopes.to_vec(),
                claim_scopes: Default::default(),
                auto_publish: false,
            });
        config
    }

    fn render_entry_for(project_id: &str) -> KnowledgeEntry {
        KnowledgeEntry {
            render_placement: Default::default(),
            id: "render-cutover".into(),
            title: "Render cutover".into(),
            content: "render cutover positive control".into(),
            cluster: None,
            category: Category::Convention,
            scope: Scope::Project,
            project: Some(PROJECT_RENDER_TRANSPORT_SCOPE.into()),
            project_id: Some(project_id.into()),
            providers: vec![],
            priority: Priority::Standard,
            render: true,
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
        record_completions_with(layout, scope, false);
    }

    fn record_completions_with(
        layout: &ProjectCatalogMigrationResolvedLayoutV1,
        scope: &PublishedScope,
        incomplete: bool,
    ) {
        record_completions_for(layout, PROJECT, scope, incomplete);
    }

    fn record_completions_for(
        layout: &ProjectCatalogMigrationResolvedLayoutV1,
        project_id: &str,
        scope: &PublishedScope,
        incomplete: bool,
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
                project_id: project_id.into(),
                scope: scope.clone(),
                workspace_id: "workspace".into(),
                producer: None,
                provider: None,
                dry_run: false,
                view,
                requested_scope: "project".into(),
                entries: vec![render_entry_for(project_id)],
                diagnostics: None,
            };
            let mut receipt = execute_project_render_plan(&plan, &root, scope, "workspace")
                .unwrap()
                .receipt;
            receipt.incomplete = incomplete;
            let issued_at_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            observations
                .record_completed(&plan, &receipt, issued_at_ms)
                .unwrap();
        }
    }

    fn age_report(path: &Path) {
        let mut report: RenderLocalityCutoverReportV1 = read_json_required(path).unwrap();
        report.generated_at_unix_secs = 0;
        write_json(path, &report).unwrap();
    }

    /// An applier that wrote every output but could not confirm completion
    /// returns an incomplete receipt whose dispositions all read written.
    /// Such receipts must never become positive-control evidence.
    #[test]
    fn incomplete_receipts_never_satisfy_the_positive_control_gate() {
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

        record_completions_with(&layout, &scope, true);
        let preflight = ProjectCatalogRenderLocalityCutoverFacadeV1::preflight(
            RenderLocalityCutoverPreflightRequestV1 {
                layout: layout.clone(),
                config: config.clone(),
                report_path: root.join("render-cutover-report.json"),
                project_ids: vec![project_id.clone()],
                min_quiet_secs: MIN_RENDER_LOCALITY_QUIET_SECS,
                generated_at: "unix:1".into(),
            },
        );
        assert!(preflight.is_err(), "incomplete receipts satisfied the gate");
        let observations = RenderLocalityObservationsV1::open(
            layout.bro_home.join("render-locality-observations.json"),
        )
        .unwrap()
        .snapshot();
        assert!(observations.completions.is_empty());
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
        let runtime = RenderLocalityCutoverRuntimeV1::open(&layout.state_dir).unwrap();
        assert!(runtime.transport_governed(PROJECT));
        let identity = runtime.marker_identity().expect("loaded marker identity");
        assert_eq!(
            Some(&identity.checksum_sha256),
            receipt.marker_checksum_sha256.as_ref()
        );
        assert_eq!(identity.applied_at, "unix:3");
        assert_eq!(runtime.rows().count() as u64, receipt.project_count);
    }

    const CARRIED_PROJECT: &str = "p_00000000000000000000000000000002";
    const NEW_PROJECT: &str = "p_00000000000000000000000000000003";
    const UNCOVERED_PROJECT: &str = "p_00000000000000000000000000000004";
    const RETIRED_PROJECT_A: &str = "p_000000000000000000000000000000a1";
    const RETIRED_PROJECT_B: &str = "p_000000000000000000000000000000a2";

    /// The marker fields a daemon built before re-runnable cutovers accepts;
    /// the marker denies unknown fields, so a successor must add none.
    const DEPLOYED_MARKER_FIELDS: [&str; 7] = [
        "applied_at",
        "catalog_epoch",
        "catalog_sha256",
        "checksum_sha256",
        "report_sha256",
        "rows",
        "version",
    ];

    struct RecutFixture {
        _directory: tempfile::TempDir,
        root: PathBuf,
        layout: ProjectCatalogMigrationResolvedLayoutV1,
        config: Config,
    }

    fn scope_for(project_id: &str) -> PublishedScope {
        PublishedScope::try_new(format!("repo-{}", &project_id[30..]), ".").unwrap()
    }

    fn predecessor_row(project_id: &str) -> RenderLocalityCutoverRowV1 {
        let completions = [
            ProjectRenderViewV1::Published,
            ProjectRenderViewV1::Own,
            ProjectRenderViewV1::All,
        ]
        .into_iter()
        .enumerate()
        .map(|(index, view)| RenderLocalityCompletionV1 {
            project_id: project_id.to_string(),
            view,
            receipt_sha256: "e".repeat(64),
            all_providers: true,
            dry_run: false,
            provider_count: 3,
            written_count: 3,
            refused_count: 0,
            sequence: index as u64 + 1,
            observed_at_unix_secs: 1,
            issued_at_ms: None,
        })
        .collect();
        RenderLocalityCutoverRowV1 {
            project_id: ProjectId::parse(project_id).unwrap(),
            scope: scope_for(project_id),
            producer_id: "producer-a".into(),
            completions,
            checkout_baselines: Vec::new(),
        }
    }

    fn project_ids(rows: &[RenderLocalityCutoverRowV1]) -> Vec<&str> {
        rows.iter().map(|row| row.project_id.as_str()).collect()
    }

    impl RecutFixture {
        /// A catalog of Published projects, each with its own scope owned by
        /// `producer-a`.
        fn new(projects: &[&str]) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().canonicalize().unwrap();
            let scopes = projects
                .iter()
                .map(|project_id| scope_for(project_id))
                .collect::<Vec<_>>();
            let config = {
                let _guard = bbox_util::util::test_env_lock();
                test_config_for(&root, &scopes)
            };
            let layout = ProjectCatalogMigrationResolvedLayoutV1::from_rehearsal_root(
                root.join("rehearsal"),
                &config,
            )
            .unwrap();
            std::fs::create_dir_all(&layout.bro_home).unwrap();
            let store = ProjectCatalogStore::initialize_empty(layout.projects_path()).unwrap();
            let epoch = store.snapshot().unwrap().epoch();
            store
                .transact(epoch, |catalog, _attachments| {
                    for (project_id, scope) in projects.iter().zip(&scopes) {
                        let project_id = ProjectId::parse(*project_id).unwrap();
                        catalog.projects.insert(
                            project_id.clone(),
                            CorpusProject {
                                project_id,
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
                    }
                    Ok(())
                })
                .unwrap();
            Self {
                _directory: directory,
                root,
                layout,
                config,
            }
        }

        fn marker_path(&self) -> PathBuf {
            self.layout
                .state_dir
                .join(RENDER_LOCALITY_CUTOVER_MARKER_FILE)
        }

        fn report_path(&self) -> PathBuf {
            self.root.join("render-cutover-report.json")
        }

        /// Install a checksummed predecessor whose rows carry evidence a
        /// first apply accepted. None of it is in the current observations.
        fn install_predecessor(&self, project_ids: &[&str]) -> RenderLocalityCutoverMarkerV1 {
            let mut rows = project_ids
                .iter()
                .map(|project_id| predecessor_row(project_id))
                .collect::<Vec<_>>();
            rows.sort_by(|left, right| left.project_id.cmp(&right.project_id));
            let mut marker = RenderLocalityCutoverMarkerV1 {
                version: MARKER_VERSION,
                applied_at: "unix:0".into(),
                report_sha256: "c".repeat(64),
                catalog_epoch: 1,
                catalog_sha256: "d".repeat(64),
                rows,
                checksum_sha256: String::new(),
            };
            marker.checksum_sha256 = marker_checksum(&marker).unwrap();
            write_json(&self.marker_path(), &marker).unwrap();
            marker
        }

        fn preflight(&self, project_ids: &[&str]) -> Result<RenderLocalityCutoverReceiptV1> {
            ProjectCatalogRenderLocalityCutoverFacadeV1::preflight(
                RenderLocalityCutoverPreflightRequestV1 {
                    layout: self.layout.clone(),
                    config: self.config.clone(),
                    report_path: self.report_path(),
                    project_ids: project_ids
                        .iter()
                        .map(|project_id| ProjectId::parse(*project_id).unwrap())
                        .collect(),
                    min_quiet_secs: MIN_RENDER_LOCALITY_QUIET_SECS,
                    generated_at: "unix:1".into(),
                },
            )
        }

        fn report(&self) -> RenderLocalityCutoverReportV1 {
            read_json_required(&self.report_path()).unwrap()
        }

        fn apply(&self) -> Result<RenderLocalityCutoverReceiptV1> {
            ProjectCatalogRenderLocalityCutoverFacadeV1::apply(
                RenderLocalityCutoverApplyRequestV1 {
                    layout: self.layout.clone(),
                    config: self.config.clone(),
                    report_path: self.report_path(),
                    applied_at: "unix:2".into(),
                },
            )
        }

        /// The installed marker as raw JSON and as the daemon startup gate
        /// opens it.
        fn installed(&self) -> (serde_json::Value, RenderLocalityCutoverMarkerV1) {
            let bytes = std::fs::read(self.marker_path()).unwrap();
            let json = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap();
            let marker = serde_json::from_slice(&bytes).unwrap();
            (json, marker)
        }
    }

    #[test]
    fn recut_carries_live_rows_drops_retired_rows_and_proves_selected_projects() {
        let fixture = RecutFixture::new(&[CARRIED_PROJECT, NEW_PROJECT, UNCOVERED_PROJECT]);
        let predecessor =
            fixture.install_predecessor(&[CARRIED_PROJECT, RETIRED_PROJECT_A, RETIRED_PROJECT_B]);
        record_completions_for(&fixture.layout, NEW_PROJECT, &scope_for(NEW_PROJECT), false);

        let receipt = fixture.preflight(&[NEW_PROJECT]).unwrap();
        assert_eq!(receipt.status, "preflight_clean");
        assert_eq!(receipt.project_count, 2);
        assert_eq!(receipt.carried_forward_row_count, 1);
        assert_eq!(receipt.dropped_row_count, 2);
        assert_eq!(receipt.uncovered_project_count, 1);
        assert_eq!(
            receipt.predecessor_marker_checksum_sha256.as_ref(),
            Some(&predecessor.checksum_sha256)
        );
        let report = fixture.report();
        assert_eq!(project_ids(&report.rows), [NEW_PROJECT]);
        // The carried row keeps the evidence its original apply accepted,
        // although no current completion exists for it.
        assert_eq!(
            report.carried_forward_rows,
            vec![predecessor.rows[0].clone()]
        );
        assert_eq!(
            report
                .dropped_rows
                .iter()
                .map(|row| (row.project_id.as_str(), row.reason))
                .collect::<Vec<_>>(),
            [
                (
                    RETIRED_PROJECT_A,
                    RenderLocalityDroppedRowReasonV1::ProjectAbsentFromCatalog
                ),
                (
                    RETIRED_PROJECT_B,
                    RenderLocalityDroppedRowReasonV1::ProjectAbsentFromCatalog
                ),
            ]
        );
        assert_eq!(
            serde_json::to_value(&report.dropped_rows[0]).unwrap()["reason"],
            "project_absent_from_catalog"
        );
        assert_eq!(
            report
                .uncovered_projects
                .iter()
                .map(ProjectId::as_str)
                .collect::<Vec<_>>(),
            [UNCOVERED_PROJECT]
        );

        // The quiet window still gates the projects this run proves.
        let error = fixture.apply().unwrap_err().to_string();
        assert!(error.contains("quiet window is incomplete"), "{error}");
        age_report(&fixture.report_path());

        // Dropped rows are reviewed, predecessor-bound evidence.
        let reviewed = std::fs::read(fixture.report_path()).unwrap();
        let mut tampered = fixture.report();
        tampered.dropped_rows.clear();
        write_json(&fixture.report_path(), &tampered).unwrap();
        let error = fixture.apply().unwrap_err().to_string();
        assert!(
            error.contains("do not match the current predecessor"),
            "{error}"
        );

        // A predecessor that changed after preflight refuses.
        std::fs::write(fixture.report_path(), &reviewed).unwrap();
        fixture.install_predecessor(&[CARRIED_PROJECT, RETIRED_PROJECT_A]);
        let error = fixture.apply().unwrap_err().to_string();
        assert!(error.contains("changed since preflight"), "{error}");
        fixture.install_predecessor(&[CARRIED_PROJECT, RETIRED_PROJECT_A, RETIRED_PROJECT_B]);

        let applied = fixture.apply().unwrap();
        assert_eq!(applied.status, "applied");
        let (json, marker) = fixture.installed();
        assert_eq!(
            json.as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(DEPLOYED_MARKER_FIELDS)
        );
        assert_eq!(project_ids(&marker.rows), [CARRIED_PROJECT, NEW_PROJECT]);
        assert_eq!(marker.rows[0], predecessor.rows[0]);
        assert_eq!(marker.rows[1], report.rows[0]);
        assert_eq!(marker.checksum_sha256, marker_checksum(&marker).unwrap());
        assert_eq!(
            marker.report_sha256,
            sha256_json(&fixture.report()).unwrap()
        );
        assert_eq!(applied.marker_checksum_sha256, Some(marker.checksum_sha256));
        assert_eq!(applied.project_count, 2);
        assert_eq!(
            applied.predecessor_marker_checksum_sha256,
            Some(predecessor.checksum_sha256)
        );
        assert_eq!(applied.carried_forward_row_count, 1);
        assert_eq!(applied.dropped_row_count, 2);

        let runtime = RenderLocalityCutoverRuntimeV1::open(&fixture.layout.state_dir).unwrap();
        assert!(runtime.transport_governed(CARRIED_PROJECT));
        assert!(runtime.transport_governed(NEW_PROJECT));
        for ungoverned in [UNCOVERED_PROJECT, RETIRED_PROJECT_A, RETIRED_PROJECT_B] {
            assert!(!runtime.transport_governed(ungoverned));
        }
        let verified = ProjectCatalogRenderLocalityCutoverFacadeV1::verify(
            RenderLocalityCutoverVerifyRequestV1 {
                layout: fixture.layout.clone(),
            },
        )
        .unwrap();
        assert_eq!(verified.status, "verified");
        assert_eq!(verified.project_count, 2);

        // The applied marker is the next run's predecessor.
        let receipt = fixture.preflight(&[]).unwrap();
        assert_eq!(receipt.carried_forward_row_count, 2);
        assert_eq!(receipt.dropped_row_count, 0);
    }

    #[test]
    fn recut_whose_every_predecessor_row_is_retired_removes_the_marker() {
        let fixture = RecutFixture::new(&[UNCOVERED_PROJECT]);
        let predecessor = fixture.install_predecessor(&[RETIRED_PROJECT_A, RETIRED_PROJECT_B]);

        let receipt = fixture.preflight(&[]).unwrap();
        assert_eq!(receipt.status, "preflight_clean");
        assert_eq!(receipt.project_count, 0);
        assert_eq!(receipt.dropped_row_count, 2);
        assert_eq!(receipt.uncovered_project_count, 1);
        let report = fixture.report();
        assert!(report.rows.is_empty() && report.carried_forward_rows.is_empty());

        // Nothing is proved, so no quiet window applies.
        let applied = fixture.apply().unwrap();
        assert_eq!(applied.status, "retired");
        assert_eq!(applied.marker_checksum_sha256, None);
        assert_eq!(applied.project_count, 0);
        assert_eq!(
            applied.predecessor_marker_checksum_sha256,
            Some(predecessor.checksum_sha256)
        );
        assert!(!fixture.marker_path().exists());
        let runtime = RenderLocalityCutoverRuntimeV1::open(&fixture.layout.state_dir).unwrap();
        assert!(runtime.project_ids().is_empty());

        // With no marker left, the next cutover is a first cutover again.
        let error = fixture.preflight(&[]).unwrap_err().to_string();
        assert!(
            error.contains("at least one explicit project id"),
            "{error}"
        );
    }

    #[test]
    fn first_cutover_keeps_its_report_shape_and_refuses_a_marker_installed_since() {
        let fixture = RecutFixture::new(&[NEW_PROJECT]);
        let error = fixture.preflight(&[]).unwrap_err().to_string();
        assert!(
            error.contains("at least one explicit project id"),
            "{error}"
        );

        record_completions_for(&fixture.layout, NEW_PROJECT, &scope_for(NEW_PROJECT), false);
        let receipt = fixture.preflight(&[NEW_PROJECT]).unwrap();
        assert_eq!(receipt.project_count, 1);
        let report_json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(fixture.report_path()).unwrap()).unwrap();
        for absent in [
            "predecessor_marker_checksum",
            "carried_forward_rows",
            "dropped_rows",
            "uncovered_projects",
        ] {
            assert!(report_json.get(absent).is_none(), "{absent}");
        }
        let receipt_json = serde_json::to_value(&receipt).unwrap();
        for absent in [
            "predecessor_marker_checksum_sha256",
            "carried_forward_row_count",
            "dropped_row_count",
            "uncovered_project_count",
        ] {
            assert!(receipt_json.get(absent).is_none(), "{absent}");
        }
        age_report(&fixture.report_path());

        fixture.install_predecessor(&[NEW_PROJECT]);
        let error = fixture.apply().unwrap_err().to_string();
        assert!(error.contains("changed since preflight"), "{error}");
        std::fs::remove_file(fixture.marker_path()).unwrap();

        let applied = fixture.apply().unwrap();
        assert_eq!(applied.status, "applied");
        assert_eq!(applied.predecessor_marker_checksum_sha256, None);
        let (json, marker) = fixture.installed();
        assert_eq!(
            json.as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(DEPLOYED_MARKER_FIELDS)
        );
        assert_eq!(project_ids(&marker.rows), [NEW_PROJECT]);
        assert!(
            RenderLocalityCutoverRuntimeV1::open(&fixture.layout.state_dir)
                .unwrap()
                .transport_governed(NEW_PROJECT)
        );
    }
}
