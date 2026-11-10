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
//! one task and execute on the blocking pool, and each run publishes its
//! source accounting as soon as it completes.
//!
//! Filesystem headroom is sampled by a separate task with at most one probe
//! worker in flight, on a detached thread: a stalled probe of an unrelated
//! mount skips later samples instead of piling up, and never delays
//! reclamation or runtime teardown at exit. Doctor
//! reports both cached observations with their own timestamps, so its body
//! changes only when one of them is replaced.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
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

/// One completed headroom sample of every probed store root.
#[derive(Debug, Clone)]
pub(crate) struct HeadroomSample {
    pub(crate) sampled_at_unix: u64,
    pub(crate) roots: Vec<ProbedRoot>,
    pub(crate) transcript_root: ProbedRoot,
}

/// Every source directory the latest completed reaper run measured, granted
/// or not.
#[derive(Debug, Clone)]
pub(crate) struct SourceMeasurements {
    pub(crate) measured_at_unix: u64,
    pub(crate) sources: BTreeMap<String, SourceStorageHealth>,
}

/// What doctor reads: the two cached observations, each `None` until its
/// first completion.
#[derive(Debug, Clone, Default)]
pub(crate) struct StorageObservation {
    pub(crate) headroom: Option<HeadroomSample>,
    pub(crate) sources: Option<SourceMeasurements>,
}

pub(crate) type HeadroomProbe = Arc<dyn Fn(&Path) -> std::io::Result<FsProbe> + Send + Sync>;

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

fn probed(probe: &HeadroomProbe, root: &StorageRoot) -> ProbedRoot {
    ProbedRoot {
        root: root.clone(),
        probe: probe(&root.probe_dir).map_err(|error| error.to_string()),
    }
}

/// Upper bound on the headroom sampling period; shorter when the reaper
/// interval is.
const MAX_HEADROOM_SAMPLE_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Clears the in-flight flag when a sampling worker ends, however it ends.
struct SamplingSlot(Arc<AtomicBool>);

impl Drop for SamplingSlot {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
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
    probe: HeadroomProbe,
    sampling: Arc<AtomicBool>,
    headroom: Mutex<Option<HeadroomSample>>,
    sources: Mutex<Option<SourceMeasurements>>,
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
            probe: Arc::new(probe_filesystem),
            sampling: Arc::new(AtomicBool::new(false)),
            headroom: Mutex::new(None),
            sources: Mutex::new(None),
        }
    }

    /// Replace the filesystem probe, so tests can stall or fake a mount.
    #[cfg(test)]
    pub(crate) fn with_probe(mut self, probe: HeadroomProbe) -> Self {
        self.probe = probe;
        self
    }

    fn headroom_sample_interval(&self) -> Duration {
        self.interval.min(MAX_HEADROOM_SAMPLE_INTERVAL)
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

    /// The cached headroom sample and source measurements.
    pub(crate) fn observation(&self) -> StorageObservation {
        StorageObservation {
            headroom: self
                .headroom
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            sources: self
                .sources
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        }
    }

    /// One reaper run, publishing its source accounting when it completes.
    /// Blocking filesystem work on the transcript store only: call it on the
    /// blocking pool.
    pub(crate) fn run_once(&self) -> anyhow::Result<ReapReport> {
        let result =
            TranscriptSourceStore::for_read(&self.root).reap(&self.policy, SystemTime::now());
        // A run that could not enumerate the store keeps the sources it last
        // measured.
        if let Ok(report) = &result {
            *self
                .sources
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(SourceMeasurements {
                measured_at_unix: unix_now(),
                sources: report
                    .scopes
                    .iter()
                    .map(|scope| (scope.connector_source_id.clone(), scope.health.clone()))
                    .collect(),
            });
        }
        result
    }

    /// Probe every store root and publish the sample. Blocking, and can stall
    /// on an unresponsive mount: the scheduler only reaches it through
    /// [`Self::request_headroom_sample`].
    pub(crate) fn sample_headroom_now(&self) {
        // Stamped before the first probe: a batch held up by a slow mount
        // must not make its earlier readings look fresh.
        let sampled_at_unix = unix_now();
        let transcript_root = probed(
            &self.probe,
            &StorageRoot::new("transcript source store", self.root.clone(), true),
        );
        let mut roots = self
            .probe_roots
            .iter()
            .map(|root| probed(&self.probe, root))
            .collect::<Vec<_>>();
        roots.push(transcript_root.clone());
        *self
            .headroom
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(HeadroomSample {
            sampled_at_unix,
            roots,
            transcript_root,
        });
    }

    /// Start one sampling worker unless one is still in flight. Never waits;
    /// returns whether a worker started.
    ///
    /// The worker is a detached OS thread, not blocking-pool work: the async
    /// runtime's teardown waits for blocking tasks it started, so a probe
    /// stalled on a hung mount there would hold daemon exit past its grace.
    /// The probes are read-only, so abandoning one at exit loses nothing.
    pub(crate) fn request_headroom_sample(self: &Arc<Self>) -> bool {
        if self.sampling.swap(true, Ordering::SeqCst) {
            tracing::debug!("filesystem headroom sample still in flight; skipping");
            return false;
        }
        let slot = SamplingSlot(self.sampling.clone());
        let runtime = self.clone();
        let spawned = std::thread::Builder::new()
            .name("bbox-storage-headroom".into())
            .spawn(move || {
                let _slot = slot;
                runtime.sample_headroom_now();
            });
        match spawned {
            Ok(_detached) => true,
            Err(error) => {
                // The closure, and the slot it owned, dropped with the failed
                // spawn, so the next tick may try again.
                tracing::warn!(%error, "could not start the filesystem headroom sampler thread");
                false
            }
        }
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

/// Spawn the reaper task and the independent headroom sampler. Both start
/// immediately (daemon start).
pub(crate) fn spawn(runtime: Arc<TranscriptSourceRuntime>) {
    let sampler = runtime.clone();
    tokio::spawn(async move {
        loop {
            sampler.request_headroom_sample();
            tokio::time::sleep(sampler.headroom_sample_interval()).await;
        }
    });
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

    async fn eventually(mut done: impl FnMut() -> bool, what: &str) {
        for _ in 0..400 {
            if done() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("timed out waiting for {what}");
    }

    /// An unrelated store mount whose probe hangs stalls only the headroom
    /// sampler: sweeps keep running and publishing accounting, further
    /// samples are skipped instead of queued, and the sample lands once the
    /// probe returns.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stalled_headroom_probe_never_delays_reclamation_or_piles_up() {
        use std::sync::atomic::AtomicUsize;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store_root = root.join("transcript-sources");
        std::fs::create_dir_all(store_root.join("csrc_0123456789abcdef")).unwrap();
        let stalled = root.join("stalled-mount");
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let calls = Arc::new(AtomicUsize::new(0));
        let probe: HeadroomProbe = {
            let stalled = stalled.clone();
            let calls = calls.clone();
            Arc::new(move |path: &Path| {
                calls.fetch_add(1, Ordering::SeqCst);
                if path == stalled {
                    release_rx.lock().unwrap().recv().unwrap();
                    return Ok(FsProbe {
                        device: 99,
                        total: 100,
                        available: 50,
                    });
                }
                probe_filesystem(path)
            })
        };
        let runtime = Arc::new(
            TranscriptSourceRuntime::new(
                store_root.clone(),
                &TranscriptSourceConfig {
                    reaper_interval_secs: 24 * 60 * 60,
                    ..TranscriptSourceConfig::default()
                },
                vec![StorageRoot {
                    label: "stalled mount".into(),
                    path: stalled.clone(),
                    probe_dir: stalled.clone(),
                }],
            )
            .with_probe(probe),
        );
        spawn(runtime.clone());

        eventually(
            || runtime.observation().sources.is_some(),
            "the start-up sweep to publish accounting",
        )
        .await;
        eventually(|| calls.load(Ordering::SeqCst) >= 2, "the sampler to stall").await;
        assert!(runtime.observation().headroom.is_none());
        assert!(
            !runtime.request_headroom_sample(),
            "a second sample is skipped while one is in flight"
        );

        // A later sweep, nudged by an index pass, still runs and publishes.
        std::fs::create_dir_all(store_root.join("csrc_fedcba9876543210")).unwrap();
        (runtime.pass_hook())();
        eventually(
            || {
                runtime
                    .observation()
                    .sources
                    .is_some_and(|measured| measured.sources.contains_key("csrc_fedcba9876543210"))
            },
            "the nudged sweep to publish the new source",
        )
        .await;
        assert!(runtime.observation().headroom.is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 2, "no probe piled up");

        release_tx.send(()).unwrap();
        eventually(
            || runtime.observation().headroom.is_some(),
            "the released sample to publish",
        )
        .await;
        let sample = runtime.observation().headroom.unwrap();
        assert!(
            sample
                .roots
                .iter()
                .any(|probed| probed.root.path == stalled && probed.probe.is_ok())
        );
        // The slot is free again; the next sample's stalled probe returns too.
        release_tx.send(()).unwrap();
        eventually(|| runtime.request_headroom_sample(), "the slot to free").await;
        eventually(
            || calls.load(Ordering::SeqCst) == 4 && !runtime.sampling.load(Ordering::SeqCst),
            "the follow-up sample to finish",
        )
        .await;
    }

    /// Tearing down the async runtime, as daemon exit does, completes while
    /// a headroom probe is still blocked on a hung mount.
    #[test]
    fn runtime_teardown_does_not_wait_for_a_blocked_headroom_probe() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let hung = root.join("hung-mount");
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
        let entered_tx = Arc::new(Mutex::new(entered_tx));
        let probe: HeadroomProbe = {
            let hung = hung.clone();
            Arc::new(move |path: &Path| {
                if path == hung {
                    entered_tx.lock().unwrap().send(()).unwrap();
                    let _ = release_rx.lock().unwrap().recv();
                }
                probe_filesystem(path)
            })
        };
        let runtime = Arc::new(
            TranscriptSourceRuntime::new(
                root.join("transcript-sources"),
                &TranscriptSourceConfig::default(),
                vec![StorageRoot {
                    label: "hung mount".into(),
                    path: hung.clone(),
                    probe_dir: hung,
                }],
            )
            .with_probe(probe),
        );
        let tokio_runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        tokio_runtime.block_on(async {
            assert!(runtime.request_headroom_sample());
        });
        entered_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the probe reached the hung mount");

        let (dropped_tx, dropped_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            drop(tokio_runtime);
            dropped_tx.send(()).unwrap();
        });
        let torn_down = dropped_rx.recv_timeout(Duration::from_secs(5));
        assert!(runtime.sampling.load(Ordering::SeqCst), "still in flight");
        release_tx.send(()).unwrap();
        torn_down.expect("runtime teardown waited on the blocked probe");
    }
}
