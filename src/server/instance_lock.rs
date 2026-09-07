//! The daemon's cross-process instance lock.
//!
//! R31F1. Every durable store below the state root is written by exactly one
//! daemon, but nothing in the startup chain enforced that. The listener bind
//! is the only exclusivity the process had, and it happens far too late:
//! `open_shared_state` opens the corpus index, runs local-activation
//! recovery, and takes the coordinator-held pin clear before `run` ever tries
//! to claim the port. A duplicate or leaked daemon therefore reached
//! reclamation paths that assume single-writer semantics, and the live
//! daemon's in-flight publication was the collateral: the second process
//! unlinked the temporary between the first's `openat` and its `renameat`,
//! and the first's reindex went down with an `ENOENT`.
//!
//! The fix is an advisory lock claimed BEFORE any shared store opens and held
//! for the process lifetime. A second daemon reaching the same state now
//! fails fast, at the lock, having mutated nothing.
//!
//! # R32F1: one lock per root, not one lock for the state root
//!
//! The first cut keyed exclusivity on `state_dir` alone, which is not the
//! daemon's identity. The transcript index defaults to the XDG *data* dir, so
//! two daemons with distinct `BLACKBOX_STATE_DIR` values share one Tantivy
//! index by default: same writer lock, and two reindex passes purging each
//! other's documents from different project catalogs. `BRO_HOME`, the packet
//! and artifact directories, and every JSON store carry their own independent
//! overrides with the same property.
//!
//! So the claim covers every mutable root the loaded config resolves, not the
//! state root alone. Roots are canonicalized, deduplicated, and reduced by
//! containment (a root nested under an already-claimed directory is already
//! covered by that directory's lock), then locked one by one. Refusal names
//! the specific contended root and lists every root this daemon claims, so
//! the operator learns that isolating a second daemon means giving each of
//! them a distinct value, not just `BLACKBOX_STATE_DIR`.
//!
//! # Lock placement
//!
//! The state root keeps its lock INSIDE the directory
//! (`<state_dir>/instance.lock`) — that is the shipped, documented path. Every
//! other root locks through a SIBLING (`<root>.instance.lock`), because those
//! roots are store directories with strict content policies: the packet store,
//! for one, refuses to enumerate a directory holding a non-canonical entry, so
//! a lock file dropped inside it would break the store it was meant to
//! protect.
//!
//! # Scope: daemon only, not the offline CLI
//!
//! `src/bin/blackbox.rs` deliberately does NOT take these locks. It never
//! opens the corpus index or the edge sidecar, so it cannot reach the
//! reclamation this lock exists to serialize, and the durable catalog state it
//! does mutate is already guarded at the right granularity by the per-store
//! advisory locks (`acquire_store_lock_nofollow`, the catalog migration
//! lock). Taking a process-lifetime exclusive root lock there would refuse
//! every offline `project-catalog list`/`get` against a live daemon, which
//! trades a real capability for no additional safety.

use bbox_corpus_core::json_store::open_lock_path_nofollow;
use fs2::FileExt;
use std::fs::File;
use std::path::{Path, PathBuf};

/// The lock file name inside the state root.
pub const INSTANCE_LOCK_NAME: &str = "instance.lock";
/// The suffix of a sibling lock file, for every root outside the state root.
pub const INSTANCE_LOCK_SUFFIX: &str = ".instance.lock";

/// The stable lock path for one daemon state root.
pub fn instance_lock_path(state_dir: &Path) -> PathBuf {
    state_dir.join(INSTANCE_LOCK_NAME)
}

/// Whether a claimed root is a directory (and therefore covers what nests
/// under it) or a single file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RootShape {
    Directory,
    File,
}

/// One mutable root this daemon claims for its lifetime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceRoot {
    /// Operator-facing name of the store living at this root.
    pub label: &'static str,
    /// The env var (or config key) that moves this root somewhere else.
    pub selector: &'static str,
    pub shape: RootShape,
    pub path: PathBuf,
    /// The state root holds its lock inside itself; see the module docs.
    lock_inside: bool,
}

impl InstanceRoot {
    pub fn state_root(path: PathBuf) -> Self {
        Self {
            label: "state root",
            selector: "BLACKBOX_STATE_DIR",
            shape: RootShape::Directory,
            path,
            lock_inside: true,
        }
    }

    pub fn directory(label: &'static str, selector: &'static str, path: PathBuf) -> Self {
        Self {
            label,
            selector,
            shape: RootShape::Directory,
            path,
            lock_inside: false,
        }
    }

    pub fn file(label: &'static str, selector: &'static str, path: PathBuf) -> Self {
        Self {
            label,
            selector,
            shape: RootShape::File,
            path,
            lock_inside: false,
        }
    }

    pub fn is_directory(&self) -> bool {
        matches!(self.shape, RootShape::Directory)
    }

    /// Where this root's advisory lock file lives.
    pub fn lock_path(&self) -> PathBuf {
        if self.lock_inside {
            return self.path.join(INSTANCE_LOCK_NAME);
        }
        match (self.path.parent(), self.path.file_name()) {
            (Some(parent), Some(name)) => {
                let mut sibling = name.to_os_string();
                sibling.push(INSTANCE_LOCK_SUFFIX);
                parent.join(sibling)
            }
            // A root with no parent (`/`) cannot carry a sibling; fall back to
            // the inside placement rather than inventing a path outside it.
            _ => self.path.join(INSTANCE_LOCK_NAME),
        }
    }

    fn with_path(&self, path: PathBuf) -> Self {
        Self {
            path,
            ..self.clone()
        }
    }
}

/// Every mutable root the loaded config resolves.
///
/// The rendered global memory files (`BLACKBOX_GLOBAL_*_MD`) are included even
/// though humans and non-daemon tools may edit them. The advisory claim only
/// coordinates blackboxd instances. Without it, a partially isolated daemon
/// can publish its incomplete store into the production guidance files.
///
/// Deliberately excluded, with reasons:
/// - the defaults / user memory directories and transcript source roots: read
///   surfaces, never written by the daemon.
/// - the rolling log directory: it derives from the platform home / state
///   directory rather than from config, and on platforms where `dirs` ignores
///   `XDG_STATE_HOME` (macOS) nothing but `$HOME` moves it. Claiming it would
///   refuse a second daemon that had given every CONFIGURED root a distinct
///   value, which is the isolation recipe the operator docs teach. A second
///   daemon still shares that path unless it also isolates `$HOME`; the docs
///   say so.
///
/// The vector store used to sit in that exclusion for the same reason. R33F1
/// moved it: it is now `paths.vectors_path`, a config-resolved root like any
/// other, so the platform-derived exclusion no longer applies to it and a
/// second daemon isolates it with `BLACKBOX_VECTORS_PATH` (or
/// `[paths].vectors_dir`) like everything else here.
pub fn instance_lock_roots(cfg: &crate::config::Config) -> Vec<InstanceRoot> {
    let paths = &cfg.paths;
    vec![
        InstanceRoot::state_root(paths.state_dir.clone()),
        InstanceRoot::directory(
            "transcript index",
            "TRANSCRIPT_SEARCH_INDEX_PATH",
            paths.index_path.clone(),
        ),
        InstanceRoot::directory(
            "vector store",
            "BLACKBOX_VECTORS_PATH",
            paths.vectors_path.clone(),
        ),
        InstanceRoot::directory("bro home", "BRO_HOME", paths.bro_home.clone()),
        InstanceRoot::directory(
            "rule packet store",
            "BLACKBOX_PACKETS_DIR",
            paths.packets_dir.clone(),
        ),
        InstanceRoot::directory(
            "artifact catalog",
            "BLACKBOX_ARTIFACTS_DIR",
            paths.artifacts_dir.clone(),
        ),
        InstanceRoot::directory(
            "backup directory",
            "BLACKBOX_STATE_DIR",
            paths.backup_dir.clone(),
        ),
        InstanceRoot::file(
            "knowledge store",
            "BLACKBOX_KNOWLEDGE_PATH",
            paths.knowledge_path.clone(),
        ),
        InstanceRoot::file(
            "global common render target",
            "BLACKBOX_GLOBAL_COMMON_MD",
            paths.global_common_md.clone(),
        ),
        InstanceRoot::file(
            "global Claude render target",
            "BLACKBOX_GLOBAL_CLAUDE_MD",
            paths.global_claude_md.clone(),
        ),
        InstanceRoot::file(
            "global Codex render target",
            "BLACKBOX_GLOBAL_CODEX_MD",
            paths.global_codex_md.clone(),
        ),
        InstanceRoot::file(
            "global Gemini render target",
            "BLACKBOX_GLOBAL_GEMINI_MD",
            paths.global_gemini_md.clone(),
        ),
        InstanceRoot::file("gap store", "BLACKBOX_GAPS_PATH", paths.gaps_path.clone()),
        InstanceRoot::file(
            "thread store",
            "BLACKBOX_THREADS_PATH",
            paths.threads_path.clone(),
        ),
        InstanceRoot::file(
            "notes store",
            "BLACKBOX_NOTES_PATH",
            paths.notes_path.clone(),
        ),
        InstanceRoot::file("pin store", "BLACKBOX_PINS_PATH", paths.pins_path.clone()),
        InstanceRoot::file(
            "project store",
            "BLACKBOX_PROJECTS_PATH",
            paths.projects_path.clone(),
        ),
    ]
}

/// Canonicalize the deepest existing ancestor and re-attach the rest, so two
/// spellings of one root (symlinked parent, `/var` vs `/private/var`) compare
/// equal even before the root exists.
fn canonical_root_path(path: &Path) -> PathBuf {
    let mut trailing: Vec<std::ffi::OsString> = Vec::new();
    let mut cursor = path.to_path_buf();
    loop {
        if let Ok(mut resolved) = cursor.canonicalize() {
            for component in trailing.iter().rev() {
                resolved.push(component);
            }
            return resolved;
        }
        let Some(name) = cursor.file_name().map(|name| name.to_os_string()) else {
            return path.to_path_buf();
        };
        let Some(parent) = cursor.parent().map(Path::to_path_buf) else {
            return path.to_path_buf();
        };
        if parent.as_os_str().is_empty() {
            return path.to_path_buf();
        }
        trailing.push(name);
        cursor = parent;
    }
}

/// Canonicalize, deduplicate, and drop every root already covered by a
/// claimed directory. Order-independent: the result is sorted by path, and
/// containment is decided against the kept set, not against arrival order.
pub fn reduce_instance_roots(roots: &[InstanceRoot]) -> Vec<InstanceRoot> {
    let mut canonical: Vec<InstanceRoot> = roots
        .iter()
        .map(|root| root.with_path(canonical_root_path(&root.path)))
        .collect();
    // Sorting by path puts every ancestor before its descendants, so a single
    // forward pass decides containment. Ties break on shape (a directory claim
    // wins over a file claim on the same path) then label, so the surviving
    // claim never depends on enumeration order.
    canonical.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then(left.shape.cmp(&right.shape))
            .then(left.label.cmp(right.label))
    });

    let mut kept: Vec<InstanceRoot> = Vec::new();
    for root in canonical {
        let covered = kept.iter().any(|held| {
            (held.is_directory() && root.path.starts_with(&held.path)) || held.path == root.path
        });
        if !covered {
            kept.push(root);
        }
    }
    kept
}

/// Why a daemon could not claim its roots.
#[derive(Debug)]
pub enum InstanceLockError {
    /// Another live process holds one of this daemon's roots. The
    /// overwhelmingly likely cause is a second daemon sharing that root.
    AlreadyHeld {
        root: InstanceRoot,
        path: PathBuf,
        claimed: Vec<InstanceRoot>,
    },
    /// The lock file itself could not be opened or locked.
    Unavailable {
        path: PathBuf,
        source: anyhow::Error,
    },
}

impl InstanceLockError {
    /// The stable code, so callers and tests can discriminate without
    /// matching on prose.
    pub fn code(&self) -> &'static str {
        match self {
            Self::AlreadyHeld { .. } => "error.daemon_instance_locked",
            Self::Unavailable { .. } => "error.daemon_instance_lock_unavailable",
        }
    }

    /// The lock file this daemon could not claim.
    pub fn path(&self) -> &Path {
        match self {
            Self::AlreadyHeld { path, .. } | Self::Unavailable { path, .. } => path,
        }
    }

    /// The contended root, when contention (not I/O) is the cause.
    pub fn root(&self) -> Option<&InstanceRoot> {
        match self {
            Self::AlreadyHeld { root, .. } => Some(root),
            Self::Unavailable { .. } => None,
        }
    }
}

fn render_claimed_roots(claimed: &[InstanceRoot]) -> String {
    claimed
        .iter()
        .map(|root| format!("{}={} ({})", root.label, root.path.display(), root.selector))
        .collect::<Vec<_>>()
        .join(", ")
}

impl std::fmt::Display for InstanceLockError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyHeld {
                root,
                path,
                claimed,
            } => write!(
                formatter,
                "another blackboxd already holds this daemon's {} at {} (instance lock {}). \
                 blackboxd claims every mutable root it resolves, not just the state root, \
                 so a second daemon needs a distinct value for each of them: {}. \
                 Stop the running instance, or give this one distinct roots, before starting it",
                root.label,
                root.path.display(),
                path.display(),
                render_claimed_roots(claimed)
            ),
            Self::Unavailable { path, source } => write!(
                formatter,
                "failed to claim the daemon instance lock at {}: {source:#}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for InstanceLockError {}

/// A held instance lock. Dropping it, or exiting the process by any route,
/// releases the advisory lock the kernel holds on the open file description.
#[derive(Debug)]
pub struct InstanceLockGuard {
    file: File,
    path: PathBuf,
}

impl InstanceLockGuard {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for InstanceLockGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

/// Every lock this daemon holds. Bound for the process lifetime by `run`.
#[derive(Debug)]
pub struct InstanceLockSet {
    guards: Vec<InstanceLockGuard>,
    roots: Vec<InstanceRoot>,
}

impl InstanceLockSet {
    /// The reduced set of roots actually claimed.
    pub fn roots(&self) -> &[InstanceRoot] {
        &self.roots
    }

    pub fn lock_paths(&self) -> impl Iterator<Item = &Path> {
        self.guards.iter().map(InstanceLockGuard::path)
    }

    /// Whether this set holds the lock file at `path`.
    pub fn holds_lock(&self, path: &Path) -> bool {
        self.lock_paths().any(|held| held == path)
    }

    /// Whether a claimed root covers `path`: the same root, or a claimed
    /// directory containing it. Canonicalizes, so a caller may pass the path
    /// exactly as its config resolved it.
    pub fn covers(&self, path: &Path) -> bool {
        let path = canonical_root_path(path);
        self.roots
            .iter()
            .any(|root| path == root.path || (root.is_directory() && path.starts_with(&root.path)))
    }
}

/// Claim every mutable root this daemon resolves, without blocking.
///
/// Non-blocking is the point: a duplicate daemon must refuse loudly and
/// immediately rather than queue behind a healthy one and then proceed into
/// shared-state mutation whenever that one exits. On refusal the guards taken
/// so far drop, releasing what this process had claimed.
pub fn acquire_instance_locks(
    roots: &[InstanceRoot],
) -> Result<InstanceLockSet, InstanceLockError> {
    let claimed = reduce_instance_roots(roots);
    let mut guards = Vec::with_capacity(claimed.len());
    for root in &claimed {
        guards.push(acquire_root(root, &claimed)?);
    }
    Ok(InstanceLockSet {
        guards,
        roots: claimed,
    })
}

/// Claim `<state_dir>/instance.lock` alone. The single-root entry point, kept
/// for callers and regressions that reason about the state root by itself.
pub fn acquire_instance_lock(state_dir: &Path) -> Result<InstanceLockGuard, InstanceLockError> {
    let root = InstanceRoot::state_root(state_dir.to_path_buf());
    let claimed = vec![root.clone()];
    acquire_root(&root, &claimed)
}

fn acquire_root(
    root: &InstanceRoot,
    claimed: &[InstanceRoot],
) -> Result<InstanceLockGuard, InstanceLockError> {
    let path = root.lock_path();
    let unavailable = |source: anyhow::Error| InstanceLockError::Unavailable {
        path: path.clone(),
        source,
    };

    let file = open_lock_path_nofollow(&path).map_err(&unavailable)?;
    let metadata = file
        .metadata()
        .map_err(|error| unavailable(anyhow::Error::new(error).context("inspect lock file")))?;
    if !metadata.file_type().is_file() {
        return Err(unavailable(anyhow::anyhow!(
            "instance lock is not a regular file"
        )));
    }

    match file.try_lock_exclusive() {
        Ok(()) => Ok(InstanceLockGuard { file, path }),
        Err(error) if is_lock_contended(&error) => Err(InstanceLockError::AlreadyHeld {
            root: root.clone(),
            path,
            claimed: claimed.to_vec(),
        }),
        Err(error) => Err(unavailable(anyhow::Error::new(error))),
    }
}

/// Contention reports as the platform's would-block error rather than a
/// distinct kind, so compare against the value `fs2` documents for it.
fn is_lock_contended(error: &std::io::Error) -> bool {
    let contended = fs2::lock_contended_error();
    error.raw_os_error() == contended.raw_os_error() || error.kind() == contended.kind()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bbox_edge_sidecar::snapshot::{
        clear_pending_local_activation_pins, pending_local_activation_pins_dir,
    };

    fn state_root(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().canonicalize().expect("canonicalize state root")
    }

    /// The shape the atomic pin writer mints between its create and its
    /// `renameat`: `.<leaf>.<pid>.<sequence>.tmp`, carrying a foreign pid.
    fn peer_temporary(pins_dir: &Path, project_id: &str, pid: u32) -> PathBuf {
        let path = pins_dir.join(format!(".{project_id}.json.{pid}.7.tmp"));
        std::fs::create_dir_all(pins_dir).expect("create the pin directory");
        std::fs::write(&path, b"{}").expect("mint the in-flight temporary");
        path
    }

    /// A config whose every root resolves below `root`, plus the two overrides
    /// a caller wants to vary. Loading through `config::load` is deliberate:
    /// the derivation under test is the one the daemon actually runs.
    fn config_for(
        env: &mut crate::util::TestEnvGuard,
        root: &Path,
        state_dir: &Path,
        index_path: &Path,
    ) -> crate::config::Config {
        env.set("BLACKBOX_CONFIG", root.join("absent-config.toml"));
        env.set("BLACKBOX_STATE_DIR", state_dir);
        env.set("TRANSCRIPT_SEARCH_INDEX_PATH", index_path);
        // The vector root defaults to the PLATFORM directory (R33F1), which
        // the live daemon on this host claims. Keep the fixture's below the
        // varied state root so the test neither contends with the host daemon
        // nor makes two fixture configurations share one vector store.
        env.set("BLACKBOX_VECTORS_PATH", state_dir.join("vectors"));
        env.set(
            "BLACKBOX_GLOBAL_COMMON_MD",
            state_dir.join("render").join("BLACKBOX.md"),
        );
        env.set(
            "BLACKBOX_GLOBAL_CLAUDE_MD",
            state_dir.join("render").join("CLAUDE.md"),
        );
        env.set(
            "BLACKBOX_GLOBAL_CODEX_MD",
            state_dir.join("render").join("AGENTS.md"),
        );
        env.set(
            "BLACKBOX_GLOBAL_GEMINI_MD",
            state_dir.join("render").join("GEMINI.md"),
        );
        for var in [
            "BRO_HOME",
            "BLACKBOX_PACKETS_DIR",
            "BLACKBOX_ARTIFACTS_DIR",
            "BLACKBOX_KNOWLEDGE_PATH",
            "BLACKBOX_GAPS_PATH",
            "BLACKBOX_THREADS_PATH",
            "BLACKBOX_NOTES_PATH",
            "BLACKBOX_PINS_PATH",
            "BLACKBOX_PROJECTS_PATH",
        ] {
            env.remove(var);
        }
        crate::config::load().expect("load the daemon config")
    }

    #[test]
    fn second_acquirer_refuses_while_the_first_holds_the_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = state_root(&dir);

        let first = acquire_instance_lock(&root).expect("first acquirer claims the root");
        assert_eq!(first.path(), instance_lock_path(&root));

        let error = acquire_instance_lock(&root).expect_err("second acquirer must refuse");
        assert_eq!(error.code(), "error.daemon_instance_locked");
        assert_eq!(error.path(), instance_lock_path(&root));
        let rendered = error.to_string();
        assert!(
            rendered.contains(&instance_lock_path(&root).display().to_string()),
            "the refusal must name the lock path: {rendered}"
        );
        assert!(
            rendered.contains("blackboxd"),
            "the refusal must name the likely duplicate-daemon cause: {rendered}"
        );
    }

    #[test]
    fn releasing_the_lock_lets_the_next_daemon_claim_the_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = state_root(&dir);

        let first = acquire_instance_lock(&root).expect("first acquirer claims the root");
        drop(first);
        let second = acquire_instance_lock(&root).expect("a released root is claimable again");
        assert_eq!(second.path(), instance_lock_path(&root));
    }

    /// The R31F1 regression, driven through the public surfaces.
    ///
    /// A live daemon is mid-publication: its temporary exists in the pin
    /// directory and its `renameat` has not run yet. A duplicate daemon boots
    /// on the same state root and would run local-activation recovery, whose
    /// clear takes the coordinator-held enumeration and unlinks every
    /// temporary it walks past. With the instance lock in front of the
    /// startup chain the duplicate never reaches that clear, so the peer's
    /// temporary survives and its rename still completes.
    #[test]
    fn a_duplicate_daemon_cannot_reclaim_a_peers_in_flight_pin_temporary() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = state_root(&dir);
        let edges_dir =
            crate::edge_index::edges_dir_from_projects_path(&root.join("projects.json"));
        let pins_dir = pending_local_activation_pins_dir(&edges_dir);

        // The live daemon owns the root and is mid-publication: its temporary
        // is on disk and its rename has not run.
        let live = acquire_instance_lock(&root).expect("the live daemon claims the root");
        let temporary = peer_temporary(&pins_dir, "peer-project", 424_242);

        // The duplicate daemon's startup. The instance lock refuses before
        // any shared store opens, so startup recovery and its coordinator-held
        // clear never run.
        let error = acquire_instance_lock(&root).expect_err("the duplicate daemon must refuse");
        assert_eq!(error.code(), "error.daemon_instance_locked");
        assert!(
            temporary.exists(),
            "the refused duplicate must leave the peer's in-flight temporary alone"
        );

        // The peer's rename completes, which an unlinked temporary would have
        // turned into an ENOENT failure that takes its reindex down.
        let published = pins_dir.join("peer-project.json");
        std::fs::rename(&temporary, &published).expect("the peer's rename must still succeed");
        assert!(published.exists());

        // The counterfactual, so the guard is not vacuous: reaching the clear
        // IS what destroys a foreign temporary. Only the process holding the
        // root may run it.
        let doomed = peer_temporary(&pins_dir, "second-project", 424_243);
        clear_pending_local_activation_pins(&edges_dir).expect("clear under the held root");
        assert!(
            !doomed.exists(),
            "the coordinator-held clear reclaims every temporary it walks past"
        );
        assert!(!published.exists(), "the clear also retracts the pin set");

        drop(live);
        acquire_instance_lock(&root).expect("a released root is claimable again");
    }

    #[test]
    fn nested_roots_ride_the_enclosing_directory_claim() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = state_root(&dir);
        let state_dir = root.join("state");
        let outside_index = root.join("elsewhere").join("index");

        let roots = vec![
            InstanceRoot::directory("bro home", "BRO_HOME", state_dir.join("bro")),
            InstanceRoot::state_root(state_dir.clone()),
            InstanceRoot::file(
                "knowledge store",
                "BLACKBOX_KNOWLEDGE_PATH",
                state_dir.join("blackbox-knowledge.json"),
            ),
            InstanceRoot::directory(
                "transcript index",
                "TRANSCRIPT_SEARCH_INDEX_PATH",
                outside_index.clone(),
            ),
        ];
        let reduced = reduce_instance_roots(&roots);
        let claimed: Vec<&Path> = reduced.iter().map(|root| root.path.as_path()).collect();
        assert_eq!(
            claimed,
            vec![outside_index.as_path(), state_dir.as_path()],
            "only the state root and the root outside it need their own lock"
        );

        // Enumeration order must not change the reduction.
        let mut shuffled = roots.clone();
        shuffled.reverse();
        assert_eq!(reduce_instance_roots(&shuffled), reduced);
    }

    #[test]
    fn duplicate_spellings_of_one_root_claim_one_lock() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = state_root(&dir);
        let state_dir = root.join("state");
        std::fs::create_dir_all(&state_dir).expect("create the state root");

        let reduced = reduce_instance_roots(&[
            InstanceRoot::state_root(state_dir.clone()),
            InstanceRoot::state_root(root.join(".").join("state")),
            InstanceRoot::directory("bro home", "BRO_HOME", state_dir.join("..").join("state")),
        ]);
        assert_eq!(reduced.len(), 1, "one path, one claim: {reduced:?}");
        assert_eq!(reduced[0].path, state_dir);
    }

    /// Locks for roots other than the state root live BESIDE the root, never
    /// inside it: the packet store refuses to enumerate a directory holding a
    /// non-canonical entry, so a lock file dropped inside would break the
    /// store this lock exists to protect.
    #[test]
    fn only_the_state_root_carries_its_lock_inside_itself() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = state_root(&dir);

        let state = InstanceRoot::state_root(root.join("state"));
        assert_eq!(state.lock_path(), root.join("state").join("instance.lock"));

        let packets = InstanceRoot::directory(
            "rule packet store",
            "BLACKBOX_PACKETS_DIR",
            root.join("pkt"),
        );
        assert_eq!(packets.lock_path(), root.join("pkt.instance.lock"));

        let knowledge = InstanceRoot::file(
            "knowledge store",
            "BLACKBOX_KNOWLEDGE_PATH",
            root.join("kb.json"),
        );
        assert_eq!(knowledge.lock_path(), root.join("kb.json.instance.lock"));

        std::fs::create_dir_all(root.join("pkt")).expect("create the packet store");
        let packets_lock = acquire_instance_locks(&[packets.clone()]).expect("claim the packets");
        assert!(
            std::fs::read_dir(root.join("pkt"))
                .expect("the packet directory exists")
                .next()
                .is_none(),
            "the claim must not put a file inside the store directory"
        );
        drop(packets_lock);
    }

    /// The R32F1 regression. Two configurations differ in `BLACKBOX_STATE_DIR`
    /// — the isolation knob the old refusal message named — but resolve the
    /// same transcript index, which is exactly what happens by default, since
    /// the index derives from the XDG data dir rather than the state root. The
    /// second daemon must refuse, naming the index.
    #[test]
    fn a_shared_index_refuses_a_second_daemon_with_its_own_state_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = state_root(&dir);
        let shared_index = root.join("shared-index");
        let mut env = crate::util::TestEnvGuard::new();

        let first = config_for(&mut env, &root, &root.join("state-a"), &shared_index);
        let first_roots = instance_lock_roots(&first);
        let held = acquire_instance_locks(&first_roots).expect("the first daemon claims its roots");
        assert!(
            held.holds_lock(&instance_lock_path(&canonical_root_path(
                &root.join("state-a")
            ))),
            "the state root is still claimed: {:?}",
            held.lock_paths().collect::<Vec<_>>()
        );

        let second = config_for(&mut env, &root, &root.join("state-b"), &shared_index);
        let second_roots = instance_lock_roots(&second);
        let error =
            acquire_instance_locks(&second_roots).expect_err("the shared index must refuse");

        assert_eq!(error.code(), "error.daemon_instance_locked");
        let contended = error.root().expect("contention names its root");
        assert_eq!(contended.label, "transcript index");
        assert_eq!(contended.path, canonical_root_path(&shared_index));
        assert_eq!(
            error.path(),
            InstanceRoot::directory(
                "transcript index",
                "TRANSCRIPT_SEARCH_INDEX_PATH",
                canonical_root_path(&shared_index)
            )
            .lock_path()
        );

        let rendered = error.to_string();
        assert!(
            rendered.contains("transcript index")
                && rendered.contains("TRANSCRIPT_SEARCH_INDEX_PATH"),
            "the refusal must name the contended root and the override that moves it: {rendered}"
        );
        assert!(
            rendered.contains("BLACKBOX_STATE_DIR"),
            "the refusal must list every claimed root, not just the contended one: {rendered}"
        );

        drop(held);
        acquire_instance_locks(&second_roots).expect("released roots are claimable again");
    }

    #[test]
    fn a_shared_global_render_target_refuses_an_otherwise_isolated_daemon() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = state_root(&dir);
        let shared_target = root.join("shared-render").join("BLACKBOX.md");
        let mut env = crate::util::TestEnvGuard::new();

        let mut first = config_for(
            &mut env,
            &root,
            &root.join("state-a"),
            &root.join("index-a"),
        );
        first.paths.global_common_md = shared_target.clone();
        let held = acquire_instance_locks(&instance_lock_roots(&first))
            .expect("the first daemon claims the shared render target");

        let mut second = config_for(
            &mut env,
            &root,
            &root.join("state-b"),
            &root.join("index-b"),
        );
        second.paths.global_common_md = shared_target.clone();
        let error = acquire_instance_locks(&instance_lock_roots(&second))
            .expect_err("the second daemon must not share a render target");

        assert_eq!(error.code(), "error.daemon_instance_locked");
        let contended = error.root().expect("contention names its root");
        assert_eq!(contended.label, "global common render target");
        assert_eq!(contended.path, canonical_root_path(&shared_target));
        let rendered = error.to_string();
        assert!(rendered.contains("BLACKBOX_GLOBAL_COMMON_MD"), "{rendered}");

        drop(held);
        acquire_instance_locks(&instance_lock_roots(&second))
            .expect("released render targets are claimable again");
    }

    /// Every root the daemon opens is claimed. The derivation is what R32F1
    /// turned on: a root missing here is a root two configurations can share.
    #[test]
    fn the_claim_covers_every_configured_mutable_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = state_root(&dir);
        let mut env = crate::util::TestEnvGuard::new();
        let cfg = config_for(&mut env, &root, &root.join("state"), &root.join("index"));
        let roots = instance_lock_roots(&cfg);

        let labels: Vec<&str> = roots.iter().map(|root| root.label).collect();
        for expected in [
            "state root",
            "transcript index",
            "vector store",
            "bro home",
            "rule packet store",
            "artifact catalog",
            "backup directory",
            "knowledge store",
            "global common render target",
            "global Claude render target",
            "global Codex render target",
            "global Gemini render target",
            "gap store",
            "thread store",
            "notes store",
            "pin store",
            "project store",
        ] {
            assert!(labels.contains(&expected), "{expected} must be claimed");
        }
        for root in &roots {
            assert!(
                !root.path.as_os_str().is_empty(),
                "{} resolved to an empty path",
                root.label
            );
        }
    }
}
