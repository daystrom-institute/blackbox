//! Daemon ownership of the native transcript landing store's storage bounds:
//! the store root, the single reservation ledger for it, the reaper
//! schedule, and the storage observation doctor reads.
//!
//! The root is fixed when the daemon opens. Every transcript route, the
//! ledger, the reaper and doctor read it from here, never from live config,
//! so a configuration reload cannot split ingest from its admission and
//! reclamation; moving the store takes a restart.
//!
//! One daemon owns a store root (the instance lock covers the state root),
//! while routes open a fresh store handle per request, so the ledger lives
//! here rather than on a handle. The reaper runs at task start, on a
//! periodic timer, and after every native index pass (nudged by the index
//! writer once the pass's reader leases dropped). Runs are serialized by this
//! one task and execute on the blocking pool. Each run also samples
//! filesystem headroom, so doctor reports one consistent observation that
//! changes only when the reaper runs.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bbox_config::config::TranscriptSourceConfig;
use bbox_transcript_source_store::{
    GenerationPhase, ReapReport, ReaperPolicy, ReservePolicy, SourceStorageHealth,
    StorageAdmission, TranscriptSourceStore, nearest_existing_ancestor,
};
use serde::Serialize;

/// What one filesystem probe reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FsProbe {
    pub(crate) device: u64,
    pub(crate) total: u64,
    pub(crate) available: u64,
}

/// A store root whose filesystem headroom is sampled.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct StorageRoot {
    pub(crate) label: String,
    pub(crate) path: PathBuf,
    /// The directory actually probed: the root itself, or its nearest
    /// existing ancestor (the parent first, for a file root).
    #[serde(skip)]
    pub(crate) probe_dir: PathBuf,
}

impl StorageRoot {
    pub(crate) fn new(label: impl Into<String>, path: PathBuf, directory: bool) -> Self {
        let start = if directory {
            path.as_path()
        } else {
            path.parent().unwrap_or(&path)
        };
        Self {
            label: label.into(),
            probe_dir: nearest_existing_ancestor(start),
            path,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ProbedRoot {
    pub(crate) root: StorageRoot,
    pub(crate) probe: Result<FsProbe, String>,
}

/// One reaper run's view of storage: filesystem headroom sampled with the
/// run, and every source directory the run measured, granted or not.
#[derive(Debug, Clone)]
pub(crate) struct StorageObservation {
    pub(crate) sampled_at_unix: u64,
    pub(crate) roots: Vec<ProbedRoot>,
    pub(crate) transcript_root: ProbedRoot,
    pub(crate) sources: BTreeMap<String, SourceStorageHealth>,
}

pub(crate) fn probe_filesystem(path: &Path) -> std::io::Result<FsProbe> {
    let space = bbox_transcript_source_store::probe_filesystem(path)?;
    #[cfg(unix)]
    let device = {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path)?.dev()
    };
    // Without device ids every probe is its own filesystem.
    #[cfg(not(unix))]
    let device = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        path.hash(&mut hasher);
        hasher.finish()
    };
    Ok(FsProbe {
        device,
        total: space.total,
        available: space.available,
    })
}

fn probed(root: &StorageRoot) -> ProbedRoot {
    ProbedRoot {
        root: root.clone(),
        probe: probe_filesystem(&root.probe_dir).map_err(|error| error.to_string()),
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

pub(crate) struct TranscriptSourceRuntime {
    root: PathBuf,
    admission: Arc<StorageAdmission>,
    policy: ReaperPolicy,
    interval: Duration,
    nudge: Arc<tokio::sync::Notify>,
    /// Instance-lock roots whose filesystems doctor reports, beside the
    /// transcript store root itself.
    probe_roots: Vec<StorageRoot>,
    observation: Mutex<Option<StorageObservation>>,
}

impl TranscriptSourceRuntime {
    /// The daemon's runtime: the store under the opened state directory,
    /// and every instance-lock root as a headroom probe.
    pub(crate) fn from_config(cfg: &crate::config::Config) -> Self {
        let probe_roots = super::instance_lock::instance_lock_roots(cfg)
            .into_iter()
            .map(|root| StorageRoot::new(root.label, root.path.clone(), root.is_directory()))
            .collect();
        Self::new(
            cfg.paths.state_dir.join("transcript-sources"),
            &cfg.transcript_source,
            probe_roots,
        )
    }

    pub(crate) fn new(
        root: PathBuf,
        config: &TranscriptSourceConfig,
        probe_roots: Vec<StorageRoot>,
    ) -> Self {
        let reserve = ReservePolicy::from_config(config.min_free_bytes);
        Self {
            admission: Arc::new(StorageAdmission::new(root.clone(), reserve)),
            root,
            policy: ReaperPolicy {
                staging_expiry: Duration::from_secs(config.staging_expiry_secs),
                staging_budget_bytes: config.staging_budget_bytes,
            },
            interval: Duration::from_secs(config.reaper_interval_secs),
            nudge: Arc::new(tokio::sync::Notify::new()),
            probe_roots,
            observation: Mutex::new(None),
        }
    }

    /// The store root every transcript surface uses, fixed at open.
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn reserve_policy(&self) -> ReservePolicy {
        self.admission.policy()
    }

    pub(crate) fn interval(&self) -> Duration {
        self.interval
    }

    /// Ingest handle on the runtime root, admitting through the shared ledger.
    pub(crate) fn ingest_store(&self) -> anyhow::Result<TranscriptSourceStore> {
        Ok(TranscriptSourceStore::open(&self.root)?.with_admission(self.admission.clone()))
    }

    /// Control and read handle on the runtime root; never admits.
    pub(crate) fn store(&self) -> anyhow::Result<TranscriptSourceStore> {
        TranscriptSourceStore::open(&self.root)
    }

    #[cfg(test)]
    pub(crate) fn admission_root(&self) -> &Path {
        self.admission.root()
    }

    /// Hook for the index writer: never blocks, coalesces into one pending
    /// run when the reaper is busy.
    pub(crate) fn pass_hook(&self) -> bbox_corpus_index::index::NativeSourcePassHook {
        let nudge = self.nudge.clone();
        Arc::new(move || nudge.notify_one())
    }

    /// The latest reaper run's storage observation; `None` before the first.
    pub(crate) fn observation(&self) -> Option<StorageObservation> {
        self.observation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// One reaper run, then a headroom sample, recorded together. Blocking
    /// filesystem work: call it on the blocking pool.
    pub(crate) fn run_once(&self) -> anyhow::Result<ReapReport> {
        let result =
            TranscriptSourceStore::for_read(&self.root).reap(&self.policy, SystemTime::now());
        let transcript_root = probed(&StorageRoot::new(
            "transcript source store",
            self.root.clone(),
            true,
        ));
        let mut roots = self.probe_roots.iter().map(probed).collect::<Vec<_>>();
        roots.push(transcript_root.clone());
        let mut observation = self
            .observation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A run that could not enumerate the store keeps the sources it last
        // measured; headroom is always the fresh sample.
        let sources = match &result {
            Ok(report) => report
                .scopes
                .iter()
                .map(|scope| (scope.connector_source_id.clone(), scope.health.clone()))
                .collect(),
            Err(_) => observation
                .as_ref()
                .map(|previous| previous.sources.clone())
                .unwrap_or_default(),
        };
        *observation = Some(StorageObservation {
            sampled_at_unix: unix_now(),
            roots,
            transcript_root,
            sources,
        });
        result
    }
}

/// One structured line per run: what was visited and removed, whether each
/// source's generation phase ran or found the reader lease held, and timing.
fn log_run(report: &ReapReport, elapsed: Duration) {
    let totals = report.totals();
    tracing::info!(
        sources = report.scopes.len(),
        streams_visited = totals.streams_visited,
        absorbed_chunk_files = totals.absorbed_chunks.files,
        absorbed_chunk_bytes = totals.absorbed_chunks.bytes,
        expired_chunk_files = totals.expired_chunks.files,
        expired_chunk_bytes = totals.expired_chunks.bytes,
        temporary_files = totals.temporary_files.files,
        temporary_bytes = totals.temporary_files.bytes,
        budget_evicted_files = totals.budget_evicted_chunks.files,
        budget_evicted_bytes = totals.budget_evicted_chunks.bytes,
        unpublished_streams_removed = totals.unpublished_streams.files,
        unpublished_stream_bytes = totals.unpublished_streams.bytes,
        non_current_generation_files = totals.non_current_generations.files,
        non_current_generation_bytes = totals.non_current_generations.bytes,
        contact_temporary_files = totals.contact_temporaries.files,
        contact_temporary_bytes = totals.contact_temporaries.bytes,
        generation_phase_ran = report.generation_phases(GenerationPhase::Ran),
        generation_phase_lease_held = report.generation_phases(GenerationPhase::LeaseHeld),
        errors = report.error_count(),
        first_error = report
            .scopes
            .iter()
            .find_map(|scope| scope.errors.first())
            .map(String::as_str)
            .unwrap_or(""),
        duration_ms = elapsed.as_millis() as u64,
        "native transcript source reaper run"
    );
}

/// Spawn the reaper task. Its first run starts immediately (daemon start).
pub(crate) fn spawn(runtime: Arc<TranscriptSourceRuntime>) {
    tokio::spawn(async move {
        loop {
            let worker = runtime.clone();
            let started = Instant::now();
            match tokio::task::spawn_blocking(move || worker.run_once()).await {
                Ok(Ok(report)) => log_run(&report, started.elapsed()),
                Ok(Err(error)) => {
                    tracing::warn!(%error, "native transcript source reaper run failed");
                }
                Err(error) => {
                    tracing::warn!(%error, "native transcript source reaper worker failed");
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(runtime.interval) => {}
                _ = runtime.nudge.notified() => {}
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_pass_nudge_wakes_the_reaper_before_its_interval() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store_root = root.join("transcript-sources");
        let scope_root = store_root.join("csrc_0123456789abcdef");
        std::fs::create_dir_all(&scope_root).unwrap();
        let runtime = Arc::new(TranscriptSourceRuntime::new(
            store_root,
            &TranscriptSourceConfig {
                reaper_interval_secs: 24 * 60 * 60,
                ..TranscriptSourceConfig::default()
            },
            Vec::new(),
        ));
        spawn(runtime.clone());
        let health = scope_root.join("health.json");
        let wait_for = |since: Option<std::time::SystemTime>| {
            let health = health.clone();
            async move {
                for _ in 0..200 {
                    if let Ok(metadata) = std::fs::metadata(&health) {
                        let modified = metadata.modified().unwrap();
                        if since.is_none_or(|since| modified > since) {
                            return modified;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                panic!("the reaper did not record a run");
            }
        };
        // The start-up run measures the source.
        let first = wait_for(None).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        (runtime.pass_hook())();
        wait_for(Some(first)).await;
    }
}
