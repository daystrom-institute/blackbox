//! Bounded filesystem I/O for instruction documents.
//!
//! Every instruction operation the harness awaits (startup discovery, resume
//! restoration, boundary refresh, structured-tool checks) captures one absolute
//! deadline at entry. Admission, filesystem traversal, and conflict retries all
//! spend that single budget. The kernel call itself may stay blocked; the
//! caller never waits for it past the deadline.
//!
//! Each ledger owns one worker slot. A worker is a detached dedicated thread
//! that holds the slot's admission permit until it actually returns, so a
//! stalled read occupies exactly one thread no matter how many callers retry,
//! and no runtime shutdown joins it. Workers return detached results; only
//! the awaiting caller may apply them, and a caller that has timed out or been
//! cancelled has dropped the receiver, so a late result is discarded.
//!
//! Before each potentially blocking call a worker publishes the operation and
//! path to the slot's progress cell. The cell lock is never held across I/O,
//! so a timeout can name the attempted path without touching the filesystem.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

pub(crate) const TIMEOUT_ENV: &str = "BRO_HARNESS_INSTRUCTION_READ_TIMEOUT_MS";
pub(crate) const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Positive, representable millisecond budgets only; anything else selects the
/// finite default.
pub(crate) fn parse_timeout(raw: Option<&str>) -> Duration {
    raw.and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|millis| *millis > 0)
        .map(Duration::from_millis)
        .filter(|budget| std::time::Instant::now().checked_add(*budget).is_some())
        .unwrap_or(DEFAULT_TIMEOUT)
}

pub(crate) fn session_timeout() -> Duration {
    parse_timeout(crate::transport::session_var(TIMEOUT_ENV).as_deref())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    Startup,
    Refresh,
    Check,
    Resume,
}

impl Phase {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Refresh => "refresh",
            Self::Check => "check",
            Self::Resume => "resume",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FsOp {
    Metadata,
    SymlinkMetadata,
    Canonicalize,
    Read,
}

impl FsOp {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Metadata => "metadata",
            Self::SymlinkMetadata => "symlink_metadata",
            Self::Canonicalize => "canonicalize",
            Self::Read => "read",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct FsMetadata {
    pub(crate) is_file: bool,
    pub(crate) is_dir: bool,
}

/// The filesystem calls instruction discovery makes. Tests inject gated
/// implementations so each probe kind can stall independently.
pub(crate) trait InstructionFs: Send + Sync + 'static {
    fn metadata(&self, path: &Path) -> std::io::Result<FsMetadata>;
    fn symlink_metadata(&self, path: &Path) -> std::io::Result<()>;
    fn canonicalize(&self, path: &Path) -> std::io::Result<PathBuf>;
    fn read_to_string(&self, path: &Path) -> std::io::Result<String>;
}

pub(crate) struct StdFs;

// Runs only on a dedicated instruction worker thread, never on a runtime worker.
#[allow(clippy::disallowed_methods)]
impl InstructionFs for StdFs {
    fn metadata(&self, path: &Path) -> std::io::Result<FsMetadata> {
        std::fs::metadata(path).map(|metadata| FsMetadata {
            is_file: metadata.is_file(),
            is_dir: metadata.is_dir(),
        })
    }

    fn symlink_metadata(&self, path: &Path) -> std::io::Result<()> {
        std::fs::symlink_metadata(path).map(drop)
    }

    fn canonicalize(&self, path: &Path) -> std::io::Result<PathBuf> {
        std::fs::canonicalize(path)
    }

    fn read_to_string(&self, path: &Path) -> std::io::Result<String> {
        std::fs::read_to_string(path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Attempt {
    pub(crate) operation: FsOp,
    pub(crate) path: PathBuf,
}

type Progress = Arc<Mutex<Option<Attempt>>>;

/// Filesystem handle given to worker jobs. Every call publishes its attempt
/// before it can block.
pub(crate) struct Probe {
    fs: Arc<dyn InstructionFs>,
    progress: Progress,
}

impl Probe {
    #[cfg(test)]
    pub(crate) fn std() -> Self {
        Self {
            fs: Arc::new(StdFs),
            progress: Progress::default(),
        }
    }

    fn publish(&self, operation: FsOp, path: &Path) {
        *self.progress.lock().expect("instruction progress poisoned") = Some(Attempt {
            operation,
            path: path.to_path_buf(),
        });
    }

    pub(crate) fn metadata(&self, path: &Path) -> std::io::Result<FsMetadata> {
        self.publish(FsOp::Metadata, path);
        self.fs.metadata(path)
    }

    pub(crate) fn exists(&self, path: &Path) -> bool {
        self.metadata(path).is_ok()
    }

    pub(crate) fn is_file(&self, path: &Path) -> bool {
        self.metadata(path).is_ok_and(|metadata| metadata.is_file)
    }

    pub(crate) fn is_dir(&self, path: &Path) -> bool {
        self.metadata(path).is_ok_and(|metadata| metadata.is_dir)
    }

    pub(crate) fn symlink_metadata(&self, path: &Path) -> std::io::Result<()> {
        self.publish(FsOp::SymlinkMetadata, path);
        self.fs.symlink_metadata(path)
    }

    pub(crate) fn canonicalize(&self, path: &Path) -> std::io::Result<PathBuf> {
        self.publish(FsOp::Canonicalize, path);
        self.fs.canonicalize(path)
    }

    pub(crate) fn read_to_string(&self, path: &Path) -> std::io::Result<String> {
        self.publish(FsOp::Read, path);
        self.fs.read_to_string(path)
    }
}

/// An instruction operation that missed its deadline. Carries only the
/// operation, path, and budget: never document bodies or environment values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InstructionTimeout {
    pub(crate) phase: Phase,
    pub(crate) budget: Duration,
    pub(crate) attempt: Option<Attempt>,
    /// The caller never started its own read: it queued behind the active
    /// worker, whose attempt is reported.
    pub(crate) waiting_for_admission: bool,
    /// Completed reads discarded because the ledger changed underneath them.
    pub(crate) conflicts: u32,
}

impl InstructionTimeout {
    pub(crate) fn path(&self) -> Option<&Path> {
        self.attempt.as_ref().map(|attempt| attempt.path.as_path())
    }

    pub(crate) fn operation(&self) -> Option<&'static str> {
        self.attempt
            .as_ref()
            .map(|attempt| attempt.operation.as_str())
    }

    pub(crate) fn budget_ms(&self) -> u64 {
        u64::try_from(self.budget.as_millis()).unwrap_or(u64::MAX)
    }

    pub(crate) fn reason(&self) -> String {
        let call = match &self.attempt {
            Some(attempt) => format!("{} {}", attempt.operation.as_str(), attempt.path.display()),
            None => "no filesystem call".to_owned(),
        };
        let mut reason = if self.waiting_for_admission {
            format!(
                "waited for admission behind the active instruction read ({call}); this operation did not start its own read"
            )
        } else if self.attempt.is_some() {
            format!("{call} did not return")
        } else {
            "the instruction worker had not started a filesystem call".to_owned()
        };
        if self.conflicts > 0 {
            reason.push_str(&format!(
                " after {} completed read(s) were discarded because instruction state changed",
                self.conflicts
            ));
        }
        reason
    }

    pub(crate) fn message(&self) -> String {
        format!(
            "instruction {} timed out after {} ms: {}",
            self.phase.as_str(),
            self.budget_ms(),
            self.reason()
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IoFailure {
    Timeout(InstructionTimeout),
    Worker(String),
}

impl IoFailure {
    pub(crate) fn message(&self) -> String {
        match self {
            Self::Timeout(timeout) => timeout.message(),
            Self::Worker(message) => message.clone(),
        }
    }
}

/// Admission to the single instruction-I/O slot of one ledger.
pub(crate) struct Admission {
    _permit: OwnedSemaphorePermit,
}

/// One active instruction-I/O worker per ledger, shared by every phase.
pub(crate) struct WorkerSlot {
    fs: Arc<dyn InstructionFs>,
    admission: Arc<Semaphore>,
    progress: Progress,
    live: Arc<AtomicUsize>,
    spawned: Arc<AtomicUsize>,
}

impl std::fmt::Debug for WorkerSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerSlot")
            .field("live", &self.live.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

struct LiveGuard(Arc<AtomicUsize>);

impl Drop for LiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl WorkerSlot {
    pub(crate) fn new(fs: Arc<dyn InstructionFs>) -> Self {
        Self {
            fs,
            admission: Arc::new(Semaphore::new(1)),
            progress: Progress::default(),
            live: Arc::default(),
            spawned: Arc::default(),
        }
    }

    pub(crate) fn active_attempt(&self) -> Option<Attempt> {
        self.progress
            .lock()
            .expect("instruction progress poisoned")
            .clone()
    }

    /// Wait asynchronously, within the caller's deadline, for the slot.
    pub(crate) async fn admit(
        &self,
        phase: Phase,
        budget: Duration,
        deadline: Instant,
    ) -> Result<Admission, IoFailure> {
        match tokio::time::timeout_at(deadline, self.admission.clone().acquire_owned()).await {
            Ok(Ok(permit)) => Ok(Admission { _permit: permit }),
            Ok(Err(_)) => Err(IoFailure::Worker(
                "instruction worker slot closed".to_owned(),
            )),
            Err(_) => Err(IoFailure::Timeout(InstructionTimeout {
                phase,
                budget,
                attempt: self.active_attempt(),
                waiting_for_admission: true,
                conflicts: 0,
            })),
        }
    }

    /// Run `job` on a detached worker thread that owns the admission until it
    /// returns. The admission comes back with the result only when the caller
    /// is still waiting; otherwise it is released when the thread exits.
    pub(crate) async fn run<T, J>(
        &self,
        admission: Admission,
        phase: Phase,
        budget: Duration,
        deadline: Instant,
        conflicts: u32,
        job: J,
    ) -> Result<(T, Admission), IoFailure>
    where
        T: Send + 'static,
        J: FnOnce(&Probe) -> T + Send + 'static,
    {
        *self.progress.lock().expect("instruction progress poisoned") = None;
        let probe = Probe {
            fs: self.fs.clone(),
            progress: self.progress.clone(),
        };
        let (sender, receiver) = tokio::sync::oneshot::channel();
        self.live.fetch_add(1, Ordering::SeqCst);
        let live = LiveGuard(self.live.clone());
        let spawned = std::thread::Builder::new()
            .name("bro-instruction-io".to_owned())
            .spawn(move || {
                let _live = live;
                let value = job(&probe);
                // A dropped receiver means the caller timed out or was
                // cancelled: the result and admission are discarded here.
                let _ = sender.send((value, admission));
            });
        if let Err(error) = spawned {
            return Err(IoFailure::Worker(format!(
                "instruction worker could not start: {error}"
            )));
        }
        self.spawned.fetch_add(1, Ordering::SeqCst);
        match tokio::time::timeout_at(deadline, receiver).await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(_)) => Err(IoFailure::Worker(
                "instruction worker exited without a result".to_owned(),
            )),
            Err(_) => Err(IoFailure::Timeout(InstructionTimeout {
                phase,
                budget,
                attempt: self.active_attempt(),
                waiting_for_admission: false,
                conflicts,
            })),
        }
    }

    #[cfg(test)]
    pub(crate) fn live_workers(&self) -> usize {
        self.live.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn spawned_workers(&self) -> usize {
        self.spawned.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! Gated filesystem for deadline tests. A stall blocks matching calls until
    //! the test releases the gate; `Drop` releases it so no test leaks a
    //! forever-blocked worker.
    use super::*;
    use std::sync::Condvar;

    type Rule = Box<dyn Fn(FsOp, &Path) -> bool + Send + Sync>;
    type Hook = Box<dyn Fn(FsOp, &Path) + Send + Sync>;

    #[derive(Default)]
    struct Gate {
        released: bool,
        stalled: usize,
    }

    #[derive(Default)]
    pub(crate) struct GatedFs {
        stall: Mutex<Option<Rule>>,
        delay: Mutex<Option<(Rule, Duration)>>,
        hook: Mutex<Option<Hook>>,
        gate: Mutex<Gate>,
        changed: Condvar,
        calls: Mutex<Vec<(FsOp, PathBuf)>>,
    }

    impl GatedFs {
        pub(crate) fn new() -> Arc<Self> {
            Arc::default()
        }

        /// Block matching calls until [`GatedFs::release`].
        pub(crate) fn stall_on(&self, operation: FsOp, path: PathBuf) {
            *self.stall.lock().unwrap() = Some(Box::new(move |op, candidate| {
                op == operation && candidate == path
            }));
        }

        pub(crate) fn delay_reads(&self, delay: Duration) {
            *self.delay.lock().unwrap() = Some((Box::new(|op, _| op == FsOp::Read), delay));
        }

        /// Run `hook` before each matching call, on the worker thread.
        pub(crate) fn on_call(&self, hook: impl Fn(FsOp, &Path) + Send + Sync + 'static) {
            *self.hook.lock().unwrap() = Some(Box::new(hook));
        }

        pub(crate) fn clear_hook(&self) {
            *self.hook.lock().unwrap() = None;
        }

        pub(crate) fn release(&self) {
            self.gate.lock().unwrap().released = true;
            self.changed.notify_all();
        }

        /// Wait until a worker is blocked on the stall.
        pub(crate) fn wait_stalled(&self) {
            let gate = self.gate.lock().unwrap();
            let (gate, timeout) = self
                .changed
                .wait_timeout_while(gate, Duration::from_secs(10), |gate| gate.stalled == 0)
                .unwrap();
            assert!(
                !timeout.timed_out() && gate.stalled > 0,
                "no worker stalled"
            );
        }

        pub(crate) fn calls(&self) -> Vec<(FsOp, PathBuf)> {
            self.calls.lock().unwrap().clone()
        }

        pub(crate) fn reads_of(&self, path: &Path) -> usize {
            self.calls()
                .iter()
                .filter(|(op, candidate)| *op == FsOp::Read && candidate == path)
                .count()
        }

        fn enter(&self, operation: FsOp, path: &Path) {
            self.calls
                .lock()
                .unwrap()
                .push((operation, path.to_path_buf()));
            if let Some(hook) = self.hook.lock().unwrap().as_ref() {
                hook(operation, path);
            }
            let delay = self
                .delay
                .lock()
                .unwrap()
                .as_ref()
                .filter(|(rule, _)| rule(operation, path))
                .map(|(_, delay)| *delay);
            if let Some(delay) = delay {
                std::thread::sleep(delay);
            }
            let stalls = self
                .stall
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|rule| rule(operation, path));
            if stalls {
                let mut gate = self.gate.lock().unwrap();
                gate.stalled += 1;
                self.changed.notify_all();
                let mut gate = self
                    .changed
                    .wait_while(gate, |gate| !gate.released)
                    .unwrap();
                gate.stalled -= 1;
            }
        }
    }

    impl Drop for GatedFs {
        fn drop(&mut self) {
            self.release();
        }
    }

    impl InstructionFs for GatedFs {
        fn metadata(&self, path: &Path) -> std::io::Result<FsMetadata> {
            self.enter(FsOp::Metadata, path);
            StdFs.metadata(path)
        }

        fn symlink_metadata(&self, path: &Path) -> std::io::Result<()> {
            self.enter(FsOp::SymlinkMetadata, path);
            StdFs.symlink_metadata(path)
        }

        fn canonicalize(&self, path: &Path) -> std::io::Result<PathBuf> {
            self.enter(FsOp::Canonicalize, path);
            StdFs.canonicalize(path)
        }

        fn read_to_string(&self, path: &Path) -> std::io::Result<String> {
            self.enter(FsOp::Read, path);
            StdFs.read_to_string(path)
        }
    }

    /// Wait (bounded) until every worker of `slot` has exited.
    pub(crate) fn wait_workers_exit(slot: &WorkerSlot) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while slot.live_workers() > 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "instruction worker did not exit"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_zero_and_overflowing_budgets_select_the_finite_default() {
        for raw in [
            None,
            Some(""),
            Some("abc"),
            Some("0"),
            Some("-5"),
            Some("1.5"),
            Some("18446744073709551616"),
        ] {
            assert_eq!(parse_timeout(raw), DEFAULT_TIMEOUT, "{raw:?}");
        }
        assert_eq!(parse_timeout(Some(" 250 ")), Duration::from_millis(250));
    }
}
