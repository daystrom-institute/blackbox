//! Offline blame-locality overlap report, quiet-window gate, and runtime cut.
//!
//! The marker is monotonic no-fallback authority. Applying it requires both
//! path and entity positive controls through the authenticated operator route,
//! equal local/legacy response checksums, and a nontrivial quiet window with
//! no new daemon-side `Blame` checkout observation for every selected project.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use bbox_config::config::Config;
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::json_store::{atomic_write_json_locked, with_store_lock};
use bbox_corpus_core::project_catalog::{ProjectId, ProjectScope};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::blame_locality_observations::{
    BlameLocalityAuthorityV1, BlameLocalityComparisonV1, BlameLocalityObservationSnapshotV1,
    BlameLocalityObservationsV1, BlameLocalityOutcomeV1, BlameLocalityTargetV1,
};
use crate::checkout_access::{
    CheckoutAccessKind, CheckoutAccessObservations, CheckoutAccessTargetCounter,
};
use crate::project_catalog_migration::ProjectCatalogMigrationResolvedLayoutV1;
use crate::project_catalog_store::ProjectCatalogStore;

const REPORT_VERSION: u32 = 1;
const MARKER_VERSION: u32 = 1;
const RECEIPT_VERSION: u32 = 1;
pub const MIN_BLAME_LOCALITY_QUIET_SECS: u64 = 5 * 60;
pub const BLAME_LOCALITY_CUTOVER_MARKER_FILE: &str = "blame-locality-cutover-marker.json";
const MAX_ARTIFACT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlameLocalityCutoverRowV1 {
    pub project_id: ProjectId,
    pub scope: PublishedScope,
    pub producer_id: String,
    pub path_comparison: BlameLocalityComparisonV1,
    pub entity_comparison: BlameLocalityComparisonV1,
    pub checkout_baselines: Vec<CheckoutAccessTargetCounter>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlameLocalityCutoverReportV1 {
    pub version: u32,
    pub generated_at: String,
    pub generated_at_unix_secs: u64,
    pub min_quiet_secs: u64,
    pub catalog_epoch: u64,
    pub catalog_sha256: String,
    pub checkout_observation_sequence: u64,
    pub blame_observation_sequence: u64,
    pub rows: Vec<BlameLocalityCutoverRowV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlameLocalityCutoverMarkerV1 {
    pub version: u32,
    pub applied_at: String,
    pub report_sha256: String,
    pub catalog_epoch: u64,
    pub catalog_sha256: String,
    pub rows: Vec<BlameLocalityCutoverRowV1>,
    pub checksum_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlameLocalityCutoverReceiptV1 {
    pub version: u32,
    pub status: String,
    pub marker_checksum_sha256: Option<String>,
    pub project_count: u64,
    pub checkout_observation_sequence: u64,
    pub blame_observation_sequence: u64,
}

pub struct BlameLocalityCutoverPreflightRequestV1 {
    pub layout: ProjectCatalogMigrationResolvedLayoutV1,
    pub config: Config,
    pub report_path: PathBuf,
    pub project_ids: Vec<ProjectId>,
    pub min_quiet_secs: u64,
    pub generated_at: String,
}

pub struct BlameLocalityCutoverApplyRequestV1 {
    pub layout: ProjectCatalogMigrationResolvedLayoutV1,
    pub config: Config,
    pub report_path: PathBuf,
    pub applied_at: String,
}

pub struct BlameLocalityCutoverVerifyRequestV1 {
    pub layout: ProjectCatalogMigrationResolvedLayoutV1,
}

#[derive(Debug, Clone, Default)]
pub struct BlameLocalityCutoverRuntimeV1 {
    rows: BTreeMap<ProjectId, BlameLocalityCutoverRowV1>,
}

impl BlameLocalityCutoverRuntimeV1 {
    pub fn open(state_dir: &Path) -> Result<Self> {
        let path = state_dir.join(BLAME_LOCALITY_CUTOVER_MARKER_FILE);
        let Some(marker) = read_json_optional::<BlameLocalityCutoverMarkerV1>(&path)? else {
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
}

pub struct ProjectCatalogBlameLocalityCutoverFacadeV1;

impl ProjectCatalogBlameLocalityCutoverFacadeV1 {
    pub fn preflight(
        request: BlameLocalityCutoverPreflightRequestV1,
    ) -> Result<BlameLocalityCutoverReceiptV1> {
        if request.min_quiet_secs < MIN_BLAME_LOCALITY_QUIET_SECS {
            bail!(
                "blame locality quiet window must be at least {MIN_BLAME_LOCALITY_QUIET_SECS} seconds"
            );
        }
        if request.project_ids.is_empty() {
            bail!("blame locality cutover requires at least one explicit project id");
        }
        let selected = request.project_ids.into_iter().collect::<BTreeSet<_>>();
        let catalog = open_catalog(&request.layout)?;
        let catalog_sha256 = sha256_json(&catalog)?;
        let blame = BlameLocalityObservationsV1::open(
            request
                .layout
                .bro_home
                .join("blame-locality-observations.json"),
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
                bail!("blame locality cutover requires a Published project: {project_id}");
            };
            let producer_id = assigned_producer(&request.config, scope)?;
            require_completed(&blame, project_id.as_str(), BlameLocalityTargetV1::Path)?;
            require_completed(&blame, project_id.as_str(), BlameLocalityTargetV1::Entity)?;
            let path_comparison =
                require_equal_comparison(&blame, project_id.as_str(), BlameLocalityTargetV1::Path)?;
            let entity_comparison = require_equal_comparison(
                &blame,
                project_id.as_str(),
                BlameLocalityTargetV1::Entity,
            )?;
            let checkout_baselines = checkout
                .target_counters
                .iter()
                .filter(|counter| {
                    counter.project_id == project_id.as_str()
                        && counter.kind == CheckoutAccessKind::Blame
                })
                .cloned()
                .collect();
            rows.push(BlameLocalityCutoverRowV1 {
                project_id,
                scope: scope.clone(),
                producer_id,
                path_comparison,
                entity_comparison,
                checkout_baselines,
            });
        }
        let report = BlameLocalityCutoverReportV1 {
            version: REPORT_VERSION,
            generated_at: request.generated_at,
            generated_at_unix_secs: now_unix_secs(),
            min_quiet_secs: request.min_quiet_secs,
            catalog_epoch: catalog.epoch,
            catalog_sha256,
            checkout_observation_sequence: checkout.sequence,
            blame_observation_sequence: blame.sequence,
            rows,
        };
        validate_report(&report)?;
        write_json(&request.report_path, &report)?;
        Ok(BlameLocalityCutoverReceiptV1 {
            version: RECEIPT_VERSION,
            status: "preflight_clean".into(),
            marker_checksum_sha256: None,
            project_count: report.rows.len() as u64,
            checkout_observation_sequence: report.checkout_observation_sequence,
            blame_observation_sequence: report.blame_observation_sequence,
        })
    }

    pub fn apply(
        request: BlameLocalityCutoverApplyRequestV1,
    ) -> Result<BlameLocalityCutoverReceiptV1> {
        let report: BlameLocalityCutoverReportV1 = read_json_required(&request.report_path)?;
        validate_report(&report)?;
        let elapsed = now_unix_secs().saturating_sub(report.generated_at_unix_secs);
        if elapsed < report.min_quiet_secs {
            bail!(
                "blame locality quiet window is incomplete: {elapsed}/{} seconds",
                report.min_quiet_secs
            );
        }
        let catalog = open_catalog(&request.layout)?;
        if catalog.epoch != report.catalog_epoch || sha256_json(&catalog)? != report.catalog_sha256
        {
            bail!("blame locality cutover report is stale against the current catalog");
        }
        let blame = BlameLocalityObservationsV1::open(
            request
                .layout
                .bro_home
                .join("blame-locality-observations.json"),
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
                    "blame locality producer assignment changed for {}",
                    row.project_id
                );
            }
            let path = require_equal_comparison(
                &blame,
                row.project_id.as_str(),
                BlameLocalityTargetV1::Path,
            )?;
            let entity = require_equal_comparison(
                &blame,
                row.project_id.as_str(),
                BlameLocalityTargetV1::Entity,
            )?;
            if path != row.path_comparison || entity != row.entity_comparison {
                bail!("blame locality comparison changed during the quiet window");
            }
            let current = checkout
                .target_counters
                .iter()
                .filter(|counter| {
                    counter.project_id == row.project_id.as_str()
                        && counter.kind == CheckoutAccessKind::Blame
                })
                .cloned()
                .collect::<Vec<_>>();
            if current != row.checkout_baselines {
                bail!(
                    "daemon-side blame checkout access changed during the quiet window for {}",
                    row.project_id
                );
            }
        }
        let marker_path = request
            .layout
            .state_dir
            .join(BLAME_LOCALITY_CUTOVER_MARKER_FILE);
        if marker_path.exists() {
            bail!("blame locality cutover marker already exists; verify it instead");
        }
        let report_sha256 = sha256_json(&report)?;
        let mut marker = BlameLocalityCutoverMarkerV1 {
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
        Ok(BlameLocalityCutoverReceiptV1 {
            version: RECEIPT_VERSION,
            status: "applied".into(),
            marker_checksum_sha256: Some(marker.checksum_sha256),
            project_count: marker.rows.len() as u64,
            checkout_observation_sequence: checkout.sequence,
            blame_observation_sequence: blame.sequence,
        })
    }

    pub fn verify(
        request: BlameLocalityCutoverVerifyRequestV1,
    ) -> Result<BlameLocalityCutoverReceiptV1> {
        let path = request
            .layout
            .state_dir
            .join(BLAME_LOCALITY_CUTOVER_MARKER_FILE);
        let marker: BlameLocalityCutoverMarkerV1 = read_json_required(&path)?;
        validate_marker(&marker)?;
        let runtime = BlameLocalityCutoverRuntimeV1::open(&request.layout.state_dir)?;
        if runtime.project_ids().len() != marker.rows.len() {
            bail!("blame locality runtime projection does not match the marker");
        }
        let checkout = CheckoutAccessObservations::open(
            request
                .layout
                .bro_home
                .join("checkout-access-observations.json"),
        )?
        .health();
        let blame = BlameLocalityObservationsV1::open(
            request
                .layout
                .bro_home
                .join("blame-locality-observations.json"),
        )?
        .snapshot();
        Ok(BlameLocalityCutoverReceiptV1 {
            version: RECEIPT_VERSION,
            status: "verified".into(),
            marker_checksum_sha256: Some(marker.checksum_sha256),
            project_count: marker.rows.len() as u64,
            checkout_observation_sequence: checkout.sequence,
            blame_observation_sequence: blame.sequence,
        })
    }
}

fn open_catalog(
    layout: &ProjectCatalogMigrationResolvedLayoutV1,
) -> Result<bbox_corpus_core::project_catalog::CatalogSnapshotV2> {
    Ok(ProjectCatalogStore::open_existing(layout.projects_path())?
        .snapshot()?
        .catalog()
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
        [] => bail!("no configured producer owns blame scope {scope:?}"),
        _ => bail!("multiple configured producers own blame scope {scope:?}"),
    }
}

fn require_completed(
    snapshot: &BlameLocalityObservationSnapshotV1,
    project_id: &str,
    target: BlameLocalityTargetV1,
) -> Result<()> {
    if snapshot.counters.iter().any(|counter| {
        counter.project_id == project_id
            && counter.authority == BlameLocalityAuthorityV1::Operator
            && counter.target == target
            && counter.outcome == BlameLocalityOutcomeV1::Completed
            && counter.count > 0
    }) {
        Ok(())
    } else {
        bail!("operator blame positive control is missing for {project_id} {target:?}")
    }
}

fn require_equal_comparison(
    snapshot: &BlameLocalityObservationSnapshotV1,
    project_id: &str,
    target: BlameLocalityTargetV1,
) -> Result<BlameLocalityComparisonV1> {
    snapshot
        .comparisons
        .iter()
        .find(|comparison| comparison.project_id == project_id && comparison.target == target)
        .filter(|comparison| comparison.equal)
        .cloned()
        .with_context(|| format!("equal blame comparison is missing for {project_id} {target:?}"))
}

fn validate_report(report: &BlameLocalityCutoverReportV1) -> Result<()> {
    if report.version != REPORT_VERSION
        || report.min_quiet_secs < MIN_BLAME_LOCALITY_QUIET_SECS
        || report.rows.is_empty()
    {
        bail!("invalid blame locality cutover report");
    }
    validate_sha256(&report.catalog_sha256)?;
    let mut seen = BTreeSet::new();
    for row in &report.rows {
        if !seen.insert(row.project_id.clone()) {
            bail!("invalid blame locality cutover row");
        }
        validate_row(row)?;
    }
    Ok(())
}

fn validate_marker(marker: &BlameLocalityCutoverMarkerV1) -> Result<()> {
    if marker.version != MARKER_VERSION || marker.rows.is_empty() {
        bail!("invalid blame locality cutover marker");
    }
    validate_sha256(&marker.report_sha256)?;
    validate_sha256(&marker.catalog_sha256)?;
    validate_sha256(&marker.checksum_sha256)?;
    if marker_checksum(marker)? != marker.checksum_sha256 {
        bail!("blame locality cutover marker checksum mismatch");
    }
    let mut seen = BTreeSet::new();
    for row in &marker.rows {
        if !seen.insert(row.project_id.clone()) {
            bail!("duplicate blame locality cutover project");
        }
        validate_row(row)?;
    }
    Ok(())
}

fn validate_row(row: &BlameLocalityCutoverRowV1) -> Result<()> {
    if row.producer_id.trim().is_empty()
        || row.producer_id.len() > 256
        || row.path_comparison.project_id != row.project_id.as_str()
        || row.path_comparison.target != BlameLocalityTargetV1::Path
        || !row.path_comparison.equal
        || row.entity_comparison.project_id != row.project_id.as_str()
        || row.entity_comparison.target != BlameLocalityTargetV1::Entity
        || !row.entity_comparison.equal
        || row.checkout_baselines.iter().any(|counter| {
            counter.project_id != row.project_id.as_str()
                || counter.kind != CheckoutAccessKind::Blame
        })
    {
        bail!("invalid blame locality cutover row");
    }
    for comparison in [&row.path_comparison, &row.entity_comparison] {
        validate_sha256(&comparison.local_response_sha256)?;
        validate_sha256(&comparison.legacy_response_sha256)?;
        if comparison.local_response_sha256 != comparison.legacy_response_sha256 {
            bail!("invalid equal blame locality comparison");
        }
    }
    Ok(())
}

fn marker_checksum(marker: &BlameLocalityCutoverMarkerV1) -> Result<String> {
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
        bail!("blame locality artifact exceeds its byte bound");
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
                bail!("blame locality artifact exceeds its byte bound");
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

    fn age_report(path: &Path) {
        let mut report: BlameLocalityCutoverReportV1 = read_json_required(path).unwrap();
        report.generated_at_unix_secs = 0;
        write_json(path, &report).unwrap();
    }

    #[test]
    fn marker_checksum_is_strict_and_runtime_is_monotonic() {
        let comparison = |target| BlameLocalityComparisonV1 {
            project_id: "project".into(),
            target,
            local_response_sha256: "a".repeat(64),
            legacy_response_sha256: "a".repeat(64),
            equal: true,
            sequence: 1,
            observed_at_unix_secs: 1,
        };
        let row = BlameLocalityCutoverRowV1 {
            project_id: ProjectId::parse("project").unwrap(),
            scope: PublishedScope::try_new("repo", ".").unwrap(),
            producer_id: "producer".into(),
            path_comparison: comparison(BlameLocalityTargetV1::Path),
            entity_comparison: comparison(BlameLocalityTargetV1::Entity),
            checkout_baselines: Vec::new(),
        };
        let mut marker = BlameLocalityCutoverMarkerV1 {
            version: MARKER_VERSION,
            applied_at: "now".into(),
            report_sha256: "b".repeat(64),
            catalog_epoch: 1,
            catalog_sha256: "c".repeat(64),
            rows: vec![row],
            checksum_sha256: String::new(),
        };
        marker.checksum_sha256 = marker_checksum(&marker).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        write_json(
            &tmp.path().join(BLAME_LOCALITY_CUTOVER_MARKER_FILE),
            &marker,
        )
        .unwrap();
        let runtime = BlameLocalityCutoverRuntimeV1::open(tmp.path()).unwrap();
        assert!(runtime.transport_governed("project"));

        marker.rows[0].producer_id = "changed".into();
        write_json(
            &tmp.path().join(BLAME_LOCALITY_CUTOVER_MARKER_FILE),
            &marker,
        )
        .unwrap();
        assert!(BlameLocalityCutoverRuntimeV1::open(tmp.path()).is_err());
    }

    #[test]
    fn cutover_requires_a_quiet_checkout_baseline_then_installs_exact_marker() {
        let _guard = bbox_util::util::test_env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let scope = PublishedScope::try_new("repo-a", ".").unwrap();
        let config = test_config(&root, &scope);
        let rehearsal = root.join("rehearsal");
        let layout =
            ProjectCatalogMigrationResolvedLayoutV1::from_rehearsal_root(&rehearsal, &config)
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

        let blame = BlameLocalityObservationsV1::open(
            layout.bro_home.join("blame-locality-observations.json"),
        )
        .unwrap();
        for target in [BlameLocalityTargetV1::Path, BlameLocalityTargetV1::Entity] {
            blame
                .record_completed(PROJECT, BlameLocalityAuthorityV1::Operator, target)
                .unwrap();
            blame
                .record_comparison(PROJECT, target, &"a".repeat(64), &"a".repeat(64))
                .unwrap();
        }

        let report_path = root.join("blame-cutover-report.json");
        let preflight = || {
            ProjectCatalogBlameLocalityCutoverFacadeV1::preflight(
                BlameLocalityCutoverPreflightRequestV1 {
                    layout: layout.clone(),
                    config: config.clone(),
                    report_path: report_path.clone(),
                    project_ids: vec![project_id.clone()],
                    min_quiet_secs: MIN_BLAME_LOCALITY_QUIET_SECS,
                    generated_at: "unix:1".into(),
                },
            )
            .unwrap();
            age_report(&report_path);
        };
        preflight();

        let checkout_observations = CheckoutAccessObservations::open(
            layout.bro_home.join("checkout-access-observations.json"),
        )
        .unwrap();
        let broker = CheckoutAccessBroker::new(Arc::new(DenyCheckoutAccess), checkout_observations);
        let _ = broker.acquire(CheckoutAccessRequest {
            project_id: PROJECT.into(),
            attachment: CheckoutAttachmentSelector::Selected,
            expected_scope: Some(scope.clone()),
            kind: CheckoutAccessKind::Blame,
            intent: CheckoutAccessIntent::Read,
            source_lane: CheckoutAccessSourceLane::NativeAttachment,
        });
        let error =
            ProjectCatalogBlameLocalityCutoverFacadeV1::apply(BlameLocalityCutoverApplyRequestV1 {
                layout: layout.clone(),
                config: config.clone(),
                report_path: report_path.clone(),
                applied_at: "unix:2".into(),
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("checkout access changed"), "{error}");

        preflight();
        let receipt =
            ProjectCatalogBlameLocalityCutoverFacadeV1::apply(BlameLocalityCutoverApplyRequestV1 {
                layout: layout.clone(),
                config,
                report_path,
                applied_at: "unix:3".into(),
            })
            .unwrap();
        assert_eq!(receipt.status, "applied");
        let verified = ProjectCatalogBlameLocalityCutoverFacadeV1::verify(
            BlameLocalityCutoverVerifyRequestV1 {
                layout: layout.clone(),
            },
        )
        .unwrap();
        assert_eq!(verified.status, "verified");
        let runtime = BlameLocalityCutoverRuntimeV1::open(&layout.state_dir).unwrap();
        assert!(runtime.transport_governed(PROJECT));
    }
}
