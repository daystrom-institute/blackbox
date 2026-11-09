//! Daemon ownership of the native transcript landing store's storage bounds:
//! the single reservation ledger for its root, and the reaper schedule.
//!
//! One daemon owns a store root (the instance lock covers the state root),
//! while routes open a fresh store handle per request, so the ledger lives
//! here rather than on a handle. The reaper runs at task start, on a
//! periodic timer, and after every native index pass (nudged by the index
//! writer once the pass's reader leases dropped). Runs are serialized by this
//! one task and execute on the blocking pool.
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use bbox_config::config::TranscriptSourceConfig;
use bbox_transcript_source_store::{
    GenerationPhase, ReapReport, ReaperPolicy, ReservePolicy, StorageAdmission,
    TranscriptSourceStore,
};

pub(crate) struct TranscriptSourceRuntime {
    root: PathBuf,
    admission: Arc<StorageAdmission>,
    policy: ReaperPolicy,
    interval: Duration,
    nudge: Arc<tokio::sync::Notify>,
}

impl TranscriptSourceRuntime {
    pub(crate) fn new(root: PathBuf, config: &TranscriptSourceConfig) -> Self {
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
        }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// The shared reservation ledger every ingest store handle admits through.
    pub(crate) fn admission(&self) -> Arc<StorageAdmission> {
        self.admission.clone()
    }

    pub(crate) fn reserve_policy(&self) -> ReservePolicy {
        self.admission.policy()
    }

    pub(crate) fn interval(&self) -> Duration {
        self.interval
    }

    /// Ingest handle for `root`, admitting through the shared ledger.
    pub(crate) fn ingest_store(
        &self,
        root: impl AsRef<Path>,
    ) -> anyhow::Result<TranscriptSourceStore> {
        Ok(TranscriptSourceStore::open(root)?.with_admission(self.admission()))
    }

    /// Hook for the index writer: never blocks, coalesces into one pending
    /// run when the reaper is busy.
    pub(crate) fn pass_hook(&self) -> bbox_corpus_index::index::NativeSourcePassHook {
        let nudge = self.nudge.clone();
        Arc::new(move || nudge.notify_one())
    }

    /// One reaper run. Blocking filesystem work: call it on the blocking pool.
    pub(crate) fn run_once(&self) -> anyhow::Result<ReapReport> {
        TranscriptSourceStore::for_read(&self.root).reap(&self.policy, SystemTime::now())
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
