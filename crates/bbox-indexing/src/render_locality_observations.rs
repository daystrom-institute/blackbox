//! Durable path-free completion evidence for checkout-owned project renders.

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use bbox_corpus_core::json_store::atomic_write_bytes_locked;
use bbox_knowledge::knowledge::{
    ProjectRenderDispositionV1, ProjectRenderPlanV1, ProjectRenderReceiptV1, ProjectRenderViewV1,
};
use fs2::FileExt;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const OBSERVATION_VERSION: u32 = 1;
const MAX_ID_BYTES: usize = 256;
const MAX_COMPLETIONS: usize = 65_536;
const MAX_OBSERVATION_BYTES: usize = 16 * 1024 * 1024;
const MAX_WORKSPACE_ISSUANCES: usize = 4_096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderLocalityCompletionV1 {
    pub project_id: String,
    pub view: ProjectRenderViewV1,
    pub receipt_sha256: String,
    pub all_providers: bool,
    pub dry_run: bool,
    pub provider_count: u64,
    pub written_count: u64,
    pub refused_count: u64,
    pub sequence: u64,
    pub observed_at_unix_secs: u64,
    /// Daemon-clock issuance of the plan this completion applied. A row is
    /// never replaced by a completion of an older plan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issued_at_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderLocalityObservationSnapshotV1 {
    pub version: u32,
    pub sequence: u64,
    pub completions: Vec<RenderLocalityCompletionV1>,
}

impl Default for RenderLocalityObservationSnapshotV1 {
    fn default() -> Self {
        Self {
            version: OBSERVATION_VERSION,
            sequence: 0,
            completions: Vec::new(),
        }
    }
}

#[derive(Clone)]
pub struct RenderLocalityObservationsV1 {
    store_path: Option<Arc<PathBuf>>,
    state: Arc<Mutex<RenderLocalityObservationSnapshotV1>>,
    /// Workspace plans this daemon issued, as `(issued_at_ms, plan_sha256)`.
    /// In memory and bounded: a completion whose issuance is not held here
    /// cannot be ordered and is never evidence.
    workspace_issuances: Arc<Mutex<BTreeSet<(u64, String)>>>,
}

impl RenderLocalityObservationsV1 {
    pub fn open(store_path: impl Into<PathBuf>) -> Result<Self> {
        let store_path = store_path.into();
        let snapshot = load_snapshot(&store_path)?;
        Ok(Self {
            store_path: Some(Arc::new(store_path)),
            state: Arc::new(Mutex::new(snapshot)),
            workspace_issuances: Arc::default(),
        })
    }

    pub fn in_memory() -> Self {
        Self {
            store_path: None,
            state: Arc::new(Mutex::new(Default::default())),
            workspace_issuances: Arc::default(),
        }
    }

    pub fn snapshot(&self) -> RenderLocalityObservationSnapshotV1 {
        self.state.lock().clone()
    }

    /// Remember that this daemon issued the workspace plan `plan_sha256` at
    /// `issued_at_ms`. The issuance leaves the daemon beside the plan bytes
    /// and comes back with the completion; only a remembered one orders it.
    pub fn note_workspace_issuance(&self, plan_sha256: &str, issued_at_ms: u64) {
        let mut issuances = self.workspace_issuances.lock();
        issuances.insert((issued_at_ms, plan_sha256.to_owned()));
        while issuances.len() > MAX_WORKSPACE_ISSUANCES {
            issuances.pop_first();
        }
    }

    /// Whether this daemon issued the workspace plan `plan_sha256` at
    /// `issued_at_ms`.
    pub fn issued_workspace_plan(&self, plan_sha256: &str, issued_at_ms: u64) -> bool {
        self.workspace_issuances
            .lock()
            .contains(&(issued_at_ms, plan_sha256.to_owned()))
    }

    /// Persist one completion only after the daemon has independently
    /// reconstructed the current plan and validated every projection hash in
    /// the checkout owner's receipt.
    ///
    /// An incomplete receipt (an output may have been written but the owner
    /// could not confirm the application) is never completion evidence,
    /// whatever its dispositions say, so it is not recorded and returns
    /// `None`. Both the bound harness and the checkout-owner collector
    /// complete through this boundary.
    ///
    /// `issued_at_ms` is the daemon-clock issuance of the applied plan. A
    /// completion of a plan issued before the one already recorded for the
    /// same project and view is historical: it never replaces the newer
    /// evidence and returns `None`. So is a different receipt at the same
    /// issuance.
    pub fn record_completed(
        &self,
        plan: &ProjectRenderPlanV1,
        receipt: &ProjectRenderReceiptV1,
        issued_at_ms: u64,
    ) -> Result<Option<u64>> {
        plan.validate()?;
        receipt.validate_against_issued(plan, Some(issued_at_ms))?;
        if receipt.incomplete {
            return Ok(None);
        }
        let receipt_sha256 = format!("{:x}", Sha256::digest(serde_json::to_vec(receipt)?));
        let written_count = receipt
            .projections
            .iter()
            .filter(|projection| projection.disposition == ProjectRenderDispositionV1::Written)
            .count() as u64;
        let refused_count = receipt
            .projections
            .iter()
            .filter(|projection| {
                matches!(
                    projection.disposition,
                    ProjectRenderDispositionV1::Refused | ProjectRenderDispositionV1::DryRunRefused
                )
            })
            .count() as u64;
        self.mutate(|snapshot| {
            let key = (plan.project_id.as_str(), plan.view);
            let position = snapshot
                .completions
                .binary_search_by(|current| (current.project_id.as_str(), current.view).cmp(&key));
            // Issuances are allocated strictly increasing, so an equal one
            // is the same render reporting again; a different receipt at an
            // equal issuance cannot be ordered and never replaces the row.
            if let Ok(index) = position
                && let Some(recorded) = snapshot.completions[index].issued_at_ms
                && (recorded > issued_at_ms
                    || (recorded == issued_at_ms
                        && snapshot.completions[index].receipt_sha256 != receipt_sha256))
            {
                return Ok(None);
            }
            snapshot.sequence = snapshot
                .sequence
                .checked_add(1)
                .context("render locality observation sequence exhausted")?;
            let completion = RenderLocalityCompletionV1 {
                project_id: plan.project_id.clone(),
                view: plan.view,
                receipt_sha256,
                all_providers: plan.provider.is_none(),
                dry_run: plan.dry_run,
                provider_count: receipt.projections.len() as u64,
                written_count,
                refused_count,
                sequence: snapshot.sequence,
                observed_at_unix_secs: now_unix_secs(),
                issued_at_ms: Some(issued_at_ms),
            };
            match position {
                Ok(index) => snapshot.completions[index] = completion,
                Err(index) => snapshot.completions.insert(index, completion),
            }
            Ok(Some(snapshot.sequence))
        })
    }

    fn mutate<T>(
        &self,
        mutation: impl FnOnce(&mut RenderLocalityObservationSnapshotV1) -> Result<T>,
    ) -> Result<T> {
        let mut state = self.state.lock();
        let (next, sequence) = if let Some(store_path) = &self.store_path {
            with_store_lock(store_path, || {
                let mut next = load_snapshot(store_path)?;
                let sequence = mutation(&mut next)?;
                validate_snapshot(&next)?;
                let bytes = serde_json::to_vec(&next)?;
                if bytes.len() > MAX_OBSERVATION_BYTES {
                    bail!("render locality observations exceed their byte bound");
                }
                atomic_write_bytes_locked(store_path, &bytes)?;
                sync_parent_directory(store_path)?;
                Ok((next, sequence))
            })?
        } else {
            let mut next = state.clone();
            let sequence = mutation(&mut next)?;
            validate_snapshot(&next)?;
            (next, sequence)
        };
        *state = next;
        Ok(sequence)
    }
}

fn load_snapshot(path: &Path) -> Result<RenderLocalityObservationSnapshotV1> {
    match std::fs::read(path) {
        Ok(bytes) => {
            if bytes.len() > MAX_OBSERVATION_BYTES {
                bail!("render locality observation file exceeds its byte bound");
            }
            let snapshot: RenderLocalityObservationSnapshotV1 = serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing {}", path.display()))?;
            validate_snapshot(&snapshot)?;
            Ok(snapshot)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Default::default()),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

fn validate_snapshot(snapshot: &RenderLocalityObservationSnapshotV1) -> Result<()> {
    if snapshot.version != OBSERVATION_VERSION {
        bail!("unsupported render locality observation version");
    }
    if snapshot.completions.len() > MAX_COMPLETIONS {
        bail!("render locality observation cardinality exceeds its bound");
    }
    let mut previous = None;
    for completion in &snapshot.completions {
        validate_id(&completion.project_id)?;
        validate_sha256(&completion.receipt_sha256)?;
        if completion.sequence == 0
            || completion.sequence > snapshot.sequence
            || completion.provider_count == 0
            || completion.written_count + completion.refused_count > completion.provider_count
        {
            bail!("invalid render locality completion");
        }
        let key = (completion.project_id.as_str(), completion.view);
        if previous.is_some_and(|previous| previous >= key) {
            bail!("render locality completions are not strictly sorted");
        }
        previous = Some(key);
    }
    Ok(())
}

fn validate_id(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > MAX_ID_BYTES {
        bail!("invalid render locality project id");
    }
    Ok(())
}

fn validate_sha256(value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        bail!("invalid render locality receipt checksum");
    }
    Ok(())
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn with_store_lock<T>(path: &Path, operation: impl FnOnce() -> Result<T>) -> Result<T> {
    let parent = path.parent().context("observation path has no parent")?;
    std::fs::create_dir_all(parent)?;
    let lock_path = path.with_extension("lock");
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&lock_path)?;
    lock.lock_exclusive()?;
    let result = operation();
    let unlock = FileExt::unlock(&lock).context("unlocking render locality observations");
    match (result, unlock) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}

fn sync_parent_directory(path: &Path) -> Result<()> {
    let parent = path.parent().context("observation path has no parent")?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bbox_corpus_core::identity::PublishedScope;
    use bbox_knowledge::knowledge::{
        PROJECT_RENDER_TRANSPORT_VERSION, ProjectRenderProjectionReceiptV1,
    };

    fn plan(view: ProjectRenderViewV1) -> ProjectRenderPlanV1 {
        ProjectRenderPlanV1 {
            version: PROJECT_RENDER_TRANSPORT_VERSION,
            project_id: "project".into(),
            scope: PublishedScope::try_new("repo", ".").unwrap(),
            workspace_id: "workspace".into(),
            producer: None,
            provider: None,
            dry_run: false,
            view,
            requested_scope: "project".into(),
            entries: vec![],
            diagnostics: None,
        }
    }

    fn receipt(plan: &ProjectRenderPlanV1) -> ProjectRenderReceiptV1 {
        ProjectRenderReceiptV1 {
            version: PROJECT_RENDER_TRANSPORT_VERSION,
            project_id: plan.project_id.clone(),
            scope: plan.scope.clone(),
            workspace_id: plan.workspace_id.clone(),
            producer: None,
            project_doc_nonempty: false,
            incomplete: false,
            projections: ["claude", "agents", "gemini"]
                .into_iter()
                .map(|provider| ProjectRenderProjectionReceiptV1 {
                    provider: provider.into(),
                    file_name: match provider {
                        "claude" => "CLAUDE.md",
                        "agents" => "AGENTS.md",
                        _ => "GEMINI.md",
                    }
                    .into(),
                    disposition: ProjectRenderDispositionV1::Skipped,
                    projection_sha256: None,
                    projection_bytes: None,
                })
                .collect(),
        }
    }

    #[test]
    fn completions_are_durable_and_replace_by_view() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("render.json");
        let observations = RenderLocalityObservationsV1::open(&path).unwrap();
        for view in [
            ProjectRenderViewV1::Published,
            ProjectRenderViewV1::Own,
            ProjectRenderViewV1::All,
            ProjectRenderViewV1::Own,
        ] {
            let plan = plan(view);
            observations
                .record_completed(&plan, &receipt(&plan), 100)
                .unwrap();
        }

        // An incomplete receipt that reports every provider written is not
        // completion evidence.
        let plan = plan(ProjectRenderViewV1::Published);
        let mut incomplete = receipt(&plan);
        incomplete.incomplete = true;
        assert_eq!(
            observations
                .record_completed(&plan, &incomplete, 300)
                .unwrap(),
            None
        );

        let reopened = RenderLocalityObservationsV1::open(&path)
            .unwrap()
            .snapshot();
        assert_eq!(reopened.sequence, 4);
        assert_eq!(reopened.completions.len(), 3);
        assert_eq!(
            reopened
                .completions
                .iter()
                .find(|completion| completion.view == ProjectRenderViewV1::Own)
                .unwrap()
                .sequence,
            4
        );
    }

    #[test]
    fn a_completion_of_an_older_plan_never_replaces_newer_evidence() {
        let observations = RenderLocalityObservationsV1::in_memory();
        let plan = plan(ProjectRenderViewV1::Published);
        let newer = receipt(&plan);
        observations
            .record_completed(&plan, &newer, 200)
            .unwrap()
            .expect("the newer completion is recorded");
        let recorded = observations.snapshot();
        assert_eq!(
            observations
                .record_completed(&plan, &receipt(&plan), 150)
                .unwrap(),
            None
        );
        assert_eq!(observations.snapshot(), recorded);
        observations
            .record_completed(&plan, &receipt(&plan), 250)
            .unwrap()
            .expect("a later completion replaces the row");
    }

    /// Two renders cannot share an issuance, so a different receipt at the
    /// recorded issuance is never ordered after it: a written receipt at an
    /// equal clock reading never replaces a refusal.
    #[test]
    fn a_different_completion_at_an_equal_issuance_never_replaces_evidence() {
        let observations = RenderLocalityObservationsV1::in_memory();
        let plan = plan(ProjectRenderViewV1::Published);
        let mut written = receipt(&plan);
        written.project_doc_nonempty = true;
        written.projections = plan.expected_projections(true, Some(200)).unwrap();
        let mut refused = written.clone();
        refused.projections[0].disposition = ProjectRenderDispositionV1::Refused;
        observations
            .record_completed(&plan, &refused, 200)
            .unwrap()
            .expect("the refusal is recorded");
        let recorded = observations.snapshot();
        assert_eq!(recorded.completions[0].refused_count, 1);
        assert_eq!(
            observations.record_completed(&plan, &written, 200).unwrap(),
            None
        );
        assert_eq!(observations.snapshot(), recorded);
        // The same render reporting again stays recorded as itself.
        observations
            .record_completed(&plan, &refused, 200)
            .unwrap()
            .expect("an identical report is accepted");
        assert_eq!(observations.snapshot().completions[0].refused_count, 1);
    }
}
