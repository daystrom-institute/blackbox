//! Shared utilities. Keep this module small — only utilities that
//! genuinely have multiple callers should live here. One-callers
//! belong with their owner.

use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT_BLACKBOX_MCP_NAME: &str = "blackbox";

/// Neutral rider appended to a tool response when a durable repo-owned file is
/// written into the working tree (a settled thread snapshot under
/// `.bbox/record/`, a project knowledge entry under `.bbox/knowledge/`, …), so
/// the caller knows to commit it rather than mistaking it for untracked
/// exhaust. `repo_root` is stripped to render the path repo-relative.
pub fn repo_artifact_rider(repo_root: &str, path: &Path) -> String {
    let rel = path
        .strip_prefix(repo_root)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| path.to_path_buf());
    let rel = rel.display();
    format!(
        "\n\nNote: wrote `{rel}` into the working tree — a durable repo-owned artifact \
         that travels with the repo. It is currently untracked; include it with your work \
         (`git add {rel}`)."
    )
}

// Not `#[cfg(test)]` gated: binary crates (e.g. `bro-slack`, `src/slack_bridge.rs`)
// reference this from their own test modules as `blackbox::util::test_env_lock`,
// which only resolves when the symbol is compiled into the lib even in non-test
// builds (cargo test --bin builds the lib as a normal dep). Lib modules use
// `crate::util::test_env_lock`. Cost: one Mutex in the binary; never touched in
// production.
static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Serializes every test that mutates process-global environment.
///
/// Rust runs tests multi-threaded, and `std::env::set_var`/`remove_var` mutate
/// shared process state, so any two env-touching tests can corrupt each other
/// unless they take this lock. INVARIANT: a test that calls `set_var`/`remove_var`
/// (directly or via [`TestEnvGuard`]) MUST hold this lock for the whole mutation
/// window. Poison-tolerant on purpose: a panicking env test must not cascade into
/// every later env test failing with a poisoned-mutex panic.
pub fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// RAII guard for env-mutating tests: holds [`test_env_lock`] AND restores every
/// touched variable to its prior value on drop. Prefer this over hand-rolled
/// save/restore blocks — it can't leak a variable into the next test even on
/// panic.
///
/// ```ignore
/// let mut env = TestEnvGuard::new();
/// env.set("BLACKBOX_STATE_DIR", tmp.path());
/// env.remove("BRO_HOME");
/// // ... assertions ...
/// // original environment restored when `env` drops
/// ```
// Like `TEST_ENV_LOCK` above, not `#[cfg(test)]` gated: consumer crates use
// this from their own test modules, where this crate compiles as a normal
// (non-test) dependency and `cfg(test)` is false.
pub struct TestEnvGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    saved: Vec<(std::ffi::OsString, Option<std::ffi::OsString>)>,
}

impl TestEnvGuard {
    pub fn new() -> Self {
        Self {
            _lock: test_env_lock(),
            saved: Vec::new(),
        }
    }

    /// Set `key=value`, remembering the prior value for restoration.
    pub fn set<K: AsRef<std::ffi::OsStr>, V: AsRef<std::ffi::OsStr>>(&mut self, key: K, value: V) {
        self.remember(key.as_ref());
        // Safe: the lock guarantees no other thread mutates env concurrently.
        unsafe { std::env::set_var(key, value) };
    }

    /// Remove `key`, remembering the prior value for restoration.
    pub fn remove<K: AsRef<std::ffi::OsStr>>(&mut self, key: K) {
        self.remember(key.as_ref());
        unsafe { std::env::remove_var(key) };
    }

    fn remember(&mut self, key: &std::ffi::OsStr) {
        // Only snapshot the first touch of each key so the original (pre-guard)
        // value is what gets restored.
        if !self.saved.iter().any(|(k, _)| k == key) {
            self.saved.push((key.to_os_string(), std::env::var_os(key)));
        }
    }
}

impl Default for TestEnvGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TestEnvGuard {
    fn drop(&mut self) {
        for (key, prior) in self.saved.drain(..).rev() {
            match prior {
                Some(value) => unsafe { std::env::set_var(&key, value) },
                None => unsafe { std::env::remove_var(&key) },
            }
        }
    }
}

/// ISO-8601 UTC timestamp with second precision and a trailing `Z`.
/// Canonical store-timestamp format; the implementation lives in
/// bbox-corpus-core so leaf crates (bbox-packets) share it.
pub use bbox_corpus_core::util::now_iso;

pub fn blackbox_mcp_name() -> String {
    std::env::var("BLACKBOX_MCP_NAME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_BLACKBOX_MCP_NAME.to_string())
}

pub fn blackbox_mcp_prefix() -> String {
    format!("mcp__{}__", blackbox_mcp_name())
}

fn xdg_state_dir(home: &Path) -> PathBuf {
    dirs::state_dir().unwrap_or_else(|| home.join(".local").join("state"))
}

fn xdg_data_dir(home: &Path) -> PathBuf {
    dirs::data_dir().unwrap_or_else(|| home.join(".local").join("share"))
}

fn env_path(var: &str) -> Option<PathBuf> {
    std::env::var(var)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
}

pub fn blackbox_state_dir(home: &Path) -> PathBuf {
    env_path("BLACKBOX_STATE_DIR").unwrap_or_else(|| xdg_state_dir(home).join("blackbox"))
}

pub fn blackbox_knowledge_path(home: &Path) -> PathBuf {
    env_path("BLACKBOX_KNOWLEDGE_PATH")
        .unwrap_or_else(|| blackbox_state_dir(home).join("blackbox-knowledge.json"))
}

pub fn blackbox_threads_path(home: &Path) -> PathBuf {
    env_path("BLACKBOX_THREADS_PATH")
        .unwrap_or_else(|| blackbox_state_dir(home).join("blackbox-threads.json"))
}

pub fn blackbox_roadmap_path(home: &Path) -> PathBuf {
    env_path("BLACKBOX_ROADMAP_PATH")
        .unwrap_or_else(|| blackbox_state_dir(home).join("blackbox-roadmap.json"))
}

pub fn blackbox_notes_path(home: &Path) -> PathBuf {
    env_path("BLACKBOX_NOTES_PATH")
        .unwrap_or_else(|| blackbox_state_dir(home).join("blackbox-notes.json"))
}

pub fn blackbox_pins_path(home: &Path) -> PathBuf {
    env_path("BLACKBOX_PINS_PATH")
        .unwrap_or_else(|| blackbox_state_dir(home).join("blackbox-pins.json"))
}

pub fn blackbox_projects_path(home: &Path) -> PathBuf {
    env_path("BLACKBOX_PROJECTS_PATH")
        .unwrap_or_else(|| blackbox_state_dir(home).join("projects.json"))
}

/// Rule-packets live as one-file-per-packet under a directory rather than
/// a single merged JSON. Each packet can be substantial (rank tables,
/// rule trees, provenance arrays) and the per-scope layout makes
/// `global` vs `project` cleanup trivial.
pub fn blackbox_packets_dir(home: &Path) -> PathBuf {
    env_path("BLACKBOX_PACKETS_DIR").unwrap_or_else(|| blackbox_state_dir(home).join("packets"))
}

pub fn blackbox_artifacts_dir(home: &Path) -> PathBuf {
    env_path("BLACKBOX_ARTIFACTS_DIR").unwrap_or_else(|| blackbox_state_dir(home).join("artifacts"))
}

/// Provider-neutral global memory file. Lives under `~/.blackbox/`
/// (parallel to `~/.codex/`, `~/.gemini/`) — *not* `~/.claude-shared/`,
/// which is claude-specific multi-account state. Each provider's global
/// memory files import this path so providers share one canonical body of
/// guidance.
pub fn blackbox_global_common_md_path(home: &Path) -> PathBuf {
    env_path("BLACKBOX_GLOBAL_COMMON_MD")
        .unwrap_or_else(|| home.join(".blackbox").join("BLACKBOX.md"))
}

pub fn bro_home_dir(home: &Path) -> PathBuf {
    env_path("BRO_HOME").unwrap_or_else(|| blackbox_state_dir(home).join("bro"))
}

/// Managed worktree parent roots recognized by the cockpit-facing daemon
/// surfaces (`bro_home/{fleet,agent}/worktrees`). One definition shared by the
/// daemon's `/control/closeout` validation, task-metadata projection, and the
/// write-side worktree gate in `bbox-indexing` — the roster must never
/// advertise a different policy than the request-time guards enforce.
pub fn cockpit_managed_worktree_roots() -> [PathBuf; 2] {
    let bro_home = bro_home_dir(&dirs::home_dir().unwrap_or_default());
    [
        bro_home.join("fleet").join("worktrees"),
        bro_home.join("agent").join("worktrees"),
    ]
}

pub fn blackbox_index_path(home: &Path) -> PathBuf {
    env_path("TRANSCRIPT_SEARCH_INDEX_PATH")
        .unwrap_or_else(|| xdg_data_dir(home).join("blackbox").join("index"))
}

pub fn blackbox_log_dir(home: &Path) -> PathBuf {
    blackbox_state_dir(home).join("logs")
}

#[derive(Debug, PartialEq, Eq)]
pub enum LegacyMove {
    Moved { old: PathBuf, new: PathBuf },
    SkippedMissing { old: PathBuf },
    SkippedDestinationExists { old: PathBuf, new: PathBuf },
}

/// The staging name a cross-device migration copies into before it renames
/// into place (R33F2).
///
/// The old fallback created the DESTINATION and streamed into it, so an
/// interrupted copy left a short file at the authoritative path: the next
/// startup saw the destination exist, skipped the migration, and the truncated
/// copy became the store. Copying into a sibling temporary and renaming means
/// a partial copy is never named as the authority; it is leftover debris the
/// next attempt overwrites.
pub fn cross_device_temp_path(new: &Path) -> PathBuf {
    let mut name = new
        .file_name()
        .map(std::ffi::OsString::from)
        .unwrap_or_default();
    name.push(".migrating.tmp");
    match new.parent() {
        Some(parent) => parent.join(name),
        None => PathBuf::from(name),
    }
}

/// The EXDEV fallback: copy into a sibling temporary, fsync, then rename it
/// onto the destination. `pub` so the interrupted-copy shape is testable
/// without injecting a real device boundary.
pub fn migrate_legacy_file_across_devices(old: &Path, new: &Path) -> anyhow::Result<()> {
    let temp = cross_device_temp_path(new);
    // Debris from an interrupted earlier attempt is overwritten, never adopted.
    let _ = fs::remove_file(&temp);
    let mut source = fs::File::open(old)?;
    let mut dest = fs::File::create(&temp)?;
    std::io::copy(&mut source, &mut dest)?;
    dest.sync_all()?;
    drop(source);
    drop(dest);
    fs::rename(&temp, new)?;
    fs::remove_file(old)?;
    Ok(())
}

pub fn migrate_legacy_file(old: &Path, new: &Path) -> anyhow::Result<LegacyMove> {
    if !old.exists() {
        return Ok(LegacyMove::SkippedMissing {
            old: old.to_path_buf(),
        });
    }
    if new.exists() {
        return Ok(LegacyMove::SkippedDestinationExists {
            old: old.to_path_buf(),
            new: new.to_path_buf(),
        });
    }

    if let Some(parent) = new.parent() {
        fs::create_dir_all(parent)?;
    }

    // Try atomic rename first
    if let Err(e) = fs::rename(old, new) {
        if e.raw_os_error() == Some(18) {
            // EXDEV: Cross-device link
            migrate_legacy_file_across_devices(old, new).map_err(|error| {
                error.context(format!(
                    "failed to copy {} to {}",
                    old.display(),
                    new.display()
                ))
            })?;
        } else {
            return Err(anyhow::anyhow!(e).context(format!(
                "failed to move {} to {}",
                old.display(),
                new.display()
            )));
        }
    }

    Ok(LegacyMove::Moved {
        old: old.to_path_buf(),
        new: new.to_path_buf(),
    })
}

pub fn resolve_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        dirs::home_dir().unwrap_or_default().join(rest)
    } else if path == "~" {
        dirs::home_dir().unwrap_or_default()
    } else {
        PathBuf::from(path)
    }
}

/// The name of the advisory lock guarding the one-time legacy migration,
/// placed next to the legacy source trees (`~/.claude-shared`, `~/.bro`).
pub const LEGACY_MIGRATION_LOCK_NAME: &str = ".blackbox-legacy-migration.lock";

/// A held claim on the legacy source tree. Dropping it, or exiting the
/// process by any route, releases the advisory lock the kernel holds on the
/// open file description.
#[derive(Debug)]
pub struct LegacyMigrationLock {
    file: fs::File,
    path: PathBuf,
}

impl LegacyMigrationLock {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for LegacyMigrationLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

/// The stable lock path for one legacy source tree.
pub fn legacy_migration_lock_path(home: &Path) -> PathBuf {
    home.join(LEGACY_MIGRATION_LOCK_NAME)
}

/// Claim the legacy source tree, without blocking.
///
/// R33F2: the migration's DESTINATIONS are now the daemon's resolved stores,
/// which sit inside roots it has already claimed. The one object two daemons
/// still share is the legacy SOURCE, since it hangs off `$HOME` and no daemon
/// owns `$HOME`. This serializes the one-time move over it: `Ok(None)` means
/// another process holds it, and the right response is to skip the migration
/// entirely — the holder either did it or is doing it, and no legacy sources
/// afterwards is the normal steady state.
pub fn try_lock_legacy_migration(home: &Path) -> anyhow::Result<Option<LegacyMigrationLock>> {
    use fs2::FileExt;

    let path = legacy_migration_lock_path(home);
    let file = bbox_corpus_core::json_store::open_lock_path_nofollow(&path)?;
    if !file.metadata()?.file_type().is_file() {
        anyhow::bail!(
            "legacy migration lock is not a regular file: {}",
            path.display()
        );
    }
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(LegacyMigrationLock { file, path })),
        Err(error) if is_lock_contended(&error) => Ok(None),
        Err(error) => Err(anyhow::Error::new(error)
            .context(format!("claiming the legacy source at {}", path.display()))),
    }
}

/// Contention reports as the platform's would-block error rather than a
/// distinct kind, so compare against the value `fs2` documents for it.
fn is_lock_contended(error: &std::io::Error) -> bool {
    let contended = fs2::lock_contended_error();
    error.raw_os_error() == contended.raw_os_error() || error.kind() == contended.kind()
}

/// Where the one-time legacy migration puts each store it moves.
///
/// R33F2: these are the caller's ALREADY-RESOLVED paths, not a second
/// derivation from env vars and `$HOME` defaults. Recomputing them here
/// ignored the config file, so a daemon isolated purely by
/// `[paths].state_dir` renamed shared legacy state into the production
/// default paths it had not claimed, racing the daemon that owns them.
#[derive(Debug, Clone)]
pub struct LegacyMigrationDestinations {
    pub knowledge_path: PathBuf,
    pub threads_path: PathBuf,
    pub notes_path: PathBuf,
    pub index_path: PathBuf,
    pub global_common_md: PathBuf,
    pub bro_home: PathBuf,
}

/// Move the legacy stores into this daemon's RESOLVED destinations, once.
///
/// Returns an empty list when another process holds the legacy source claim:
/// the migration is one-time and idempotent, so deferring to the holder loses
/// nothing.
pub fn migrate_legacy_defaults(
    home: &Path,
    destinations: &LegacyMigrationDestinations,
) -> anyhow::Result<Vec<String>> {
    let Some(_source_claim) = try_lock_legacy_migration(home)? else {
        tracing::info!(
            lock = %legacy_migration_lock_path(home).display(),
            "another process holds the legacy source; skipping the one-time migration"
        );
        return Ok(Vec::new());
    };

    let mut moved = Vec::new();

    for (label, old, new) in [
        (
            "knowledge",
            home.join(".claude-shared").join("blackbox-knowledge.json"),
            destinations.knowledge_path.clone(),
        ),
        (
            "threads",
            home.join(".claude-shared").join("blackbox-threads.json"),
            destinations.threads_path.clone(),
        ),
        (
            "notes",
            home.join(".claude-shared").join("blackbox-notes.json"),
            destinations.notes_path.clone(),
        ),
        (
            "index",
            home.join(".claude-shared").join("transcript-index"),
            destinations.index_path.clone(),
        ),
        (
            "blackbox-md",
            home.join(".claude-shared").join("BLACKBOX.md"),
            destinations.global_common_md.clone(),
        ),
    ] {
        if let LegacyMove::Moved { old, new } = migrate_legacy_file(&old, &new)? {
            moved.push(format!("{label}: {} -> {}", old.display(), new.display()));
        }
    }

    // Task 3: ~/.bro/ migration
    let old_bro = home.join(".bro");
    let new_bro = destinations.bro_home.clone();
    if old_bro.is_dir() {
        for entry in fs::read_dir(&old_bro)? {
            let entry = entry?;
            let old_path = entry.path();
            let name = old_path
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("invalid file name"))?;
            let new_path = new_bro.join(name);
            match migrate_legacy_file(&old_path, &new_path)? {
                LegacyMove::Moved { old, new } => {
                    moved.push(format!("bro: {} -> {}", old.display(), new.display()));
                }
                LegacyMove::SkippedDestinationExists { old, new } => {
                    tracing::warn!(
                        "Skipped migrating {} because {} already exists",
                        old.display(),
                        new.display()
                    );
                }
                _ => {}
            }
        }
        // If empty after migration, try to remove old dir
        if old_bro
            .read_dir()
            .map(|mut d| d.next().is_none())
            .unwrap_or(false)
        {
            let _ = fs::remove_dir(&old_bro);
        }
    }

    Ok(moved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn defaults_use_xdg_layout() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let home = dir.path();

        // Save and clear env vars
        let orig_state_dir = std::env::var("BLACKBOX_STATE_DIR").ok();
        let orig_knowledge = std::env::var("BLACKBOX_KNOWLEDGE_PATH").ok();
        let orig_threads = std::env::var("BLACKBOX_THREADS_PATH").ok();
        let orig_notes = std::env::var("BLACKBOX_NOTES_PATH").ok();
        let orig_pins = std::env::var("BLACKBOX_PINS_PATH").ok();
        let orig_index = std::env::var("TRANSCRIPT_SEARCH_INDEX_PATH").ok();
        let orig_bro_home = std::env::var("BRO_HOME").ok();
        unsafe {
            std::env::remove_var("BLACKBOX_STATE_DIR");
        }
        unsafe {
            std::env::remove_var("BLACKBOX_KNOWLEDGE_PATH");
        }
        unsafe {
            std::env::remove_var("BLACKBOX_THREADS_PATH");
        }
        unsafe {
            std::env::remove_var("BLACKBOX_NOTES_PATH");
        }
        unsafe {
            std::env::remove_var("BLACKBOX_PINS_PATH");
        }
        unsafe {
            std::env::remove_var("TRANSCRIPT_SEARCH_INDEX_PATH");
        }
        unsafe {
            std::env::remove_var("BRO_HOME");
        }

        let state = blackbox_state_dir(home);
        assert!(state.ends_with(".local/state/blackbox") || state.ends_with("blackbox"));
        assert!(blackbox_knowledge_path(home).ends_with("blackbox-knowledge.json"));
        assert!(blackbox_threads_path(home).ends_with("blackbox-threads.json"));
        assert!(blackbox_notes_path(home).ends_with("blackbox-notes.json"));
        assert!(blackbox_pins_path(home).ends_with("blackbox-pins.json"));
        assert!(blackbox_index_path(home).ends_with("blackbox/index"));
        assert!(bro_home_dir(home).ends_with("blackbox/bro"));

        // Restore
        if let Some(v) = orig_state_dir {
            unsafe { std::env::set_var("BLACKBOX_STATE_DIR", v) };
        } else {
            unsafe { std::env::remove_var("BLACKBOX_STATE_DIR") };
        }
        if let Some(v) = orig_knowledge {
            unsafe { std::env::set_var("BLACKBOX_KNOWLEDGE_PATH", v) };
        } else {
            unsafe { std::env::remove_var("BLACKBOX_KNOWLEDGE_PATH") };
        }
        if let Some(v) = orig_threads {
            unsafe { std::env::set_var("BLACKBOX_THREADS_PATH", v) };
        } else {
            unsafe { std::env::remove_var("BLACKBOX_THREADS_PATH") };
        }
        if let Some(v) = orig_notes {
            unsafe { std::env::set_var("BLACKBOX_NOTES_PATH", v) };
        } else {
            unsafe { std::env::remove_var("BLACKBOX_NOTES_PATH") };
        }
        if let Some(v) = orig_pins {
            unsafe { std::env::set_var("BLACKBOX_PINS_PATH", v) };
        } else {
            unsafe { std::env::remove_var("BLACKBOX_PINS_PATH") };
        }
        if let Some(v) = orig_index {
            unsafe { std::env::set_var("TRANSCRIPT_SEARCH_INDEX_PATH", v) };
        } else {
            unsafe { std::env::remove_var("TRANSCRIPT_SEARCH_INDEX_PATH") };
        }
        if let Some(v) = orig_bro_home {
            unsafe { std::env::set_var("BRO_HOME", v) };
        } else {
            unsafe { std::env::remove_var("BRO_HOME") };
        }
    }

    /// Every destination below one fixture root. R33F2: the migration takes
    /// its destinations from the caller's resolved configuration, so the test
    /// never has to mutate process environment to steer them.
    fn fixture_destinations(state: &Path, home: &Path) -> LegacyMigrationDestinations {
        LegacyMigrationDestinations {
            knowledge_path: state.join("blackbox-knowledge.json"),
            threads_path: state.join("blackbox-threads.json"),
            notes_path: state.join("blackbox-notes.json"),
            index_path: state.join("index"),
            global_common_md: home.join(".blackbox").join("BLACKBOX.md"),
            bro_home: state.join("bro"),
        }
    }

    fn write_legacy_tree(home: &Path) {
        let old_shared = home.join(".claude-shared");
        let old_bro = home.join(".bro");
        fs::create_dir_all(&old_shared).unwrap();
        fs::create_dir_all(&old_bro).unwrap();
        fs::write(old_shared.join("blackbox-knowledge.json"), "{}").unwrap();
        fs::write(old_shared.join("blackbox-threads.json"), "{}").unwrap();
        fs::write(old_shared.join("blackbox-notes.json"), "{}").unwrap();
        fs::create_dir_all(old_shared.join("transcript-index")).unwrap();
        fs::write(old_shared.join("transcript-index").join("meta"), "x").unwrap();
        fs::write(old_bro.join("tasks.json"), "[]").unwrap();
    }

    #[test]
    fn migrates_legacy_defaults_when_new_targets_absent() {
        let dir = tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        write_legacy_tree(&home);
        let destinations = fixture_destinations(&home.join("state"), &home);

        let moved = migrate_legacy_defaults(&home, &destinations).unwrap();
        // Moved: knowledge, threads, notes, index, bro (tasks.json)
        assert!(
            moved.len() >= 4,
            "expected >=4 moves, got {}: {:?}",
            moved.len(),
            moved
        );
        assert!(destinations.knowledge_path.exists());
        assert!(destinations.threads_path.exists());
        assert!(destinations.notes_path.exists());
        assert!(destinations.index_path.exists());
        assert!(destinations.bro_home.join("tasks.json").exists());
        assert!(
            !home
                .join(".claude-shared")
                .join("blackbox-knowledge.json")
                .exists()
        );
        assert!(!home.join(".bro").exists());
    }

    /// R33F2. Two daemons isolated purely by configuration share one `$HOME`,
    /// so they see the same legacy source tree. The claim on that source is
    /// what stops both from committing to the same one-time move: the loser
    /// migrates nothing, and the sources land in the winner's RESOLVED roots
    /// exactly once. The destinations differ per daemon, which is the whole
    /// point: recomputing them from env vars and `$HOME` sent both daemons at
    /// one production-default path neither had claimed.
    #[test]
    fn two_config_isolated_daemons_sharing_one_home_migrate_the_sources_once() {
        let dir = tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        write_legacy_tree(&home);
        let first = fixture_destinations(&home.join("state-a"), &home);
        let second = fixture_destinations(&home.join("state-b"), &home);

        // The first daemon is inside its migration, holding the source claim.
        let held = try_lock_legacy_migration(&home)
            .unwrap()
            .expect("the first daemon claims the legacy source");

        let skipped = migrate_legacy_defaults(&home, &second).unwrap();
        assert!(
            skipped.is_empty(),
            "the daemon that lost the source claim must migrate nothing: {skipped:?}"
        );
        assert!(!second.knowledge_path.exists());
        assert!(!second.bro_home.exists());
        assert!(
            home.join(".claude-shared")
                .join("blackbox-knowledge.json")
                .exists()
        );

        // The winner finishes and releases; the sources move exactly once,
        // into ITS resolved roots.
        drop(held);
        let moved = migrate_legacy_defaults(&home, &first).unwrap();
        assert!(moved.iter().any(|line| line.starts_with("knowledge:")));
        assert!(first.knowledge_path.exists());
        assert!(first.bro_home.join("tasks.json").exists());

        // The second daemon runs afterwards: the sources are gone, which is
        // the normal steady state, and nothing lands in its roots.
        let after = migrate_legacy_defaults(&home, &second).unwrap();
        assert!(after.is_empty(), "nothing is left to migrate: {after:?}");
        assert!(!second.knowledge_path.exists());
    }

    /// R33F2. An interrupted cross-device copy must never be mistaken for the
    /// migrated authority: the copy lands on a temporary name and is renamed
    /// into place, so debris at the temporary path is overwritten rather than
    /// adopted. Asserted through the naming rather than by injecting a real
    /// device boundary.
    #[test]
    fn a_cross_device_copy_stages_under_a_temporary_name() {
        let dir = tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let old = home.join("legacy.json");
        let new = home.join("destination.json");
        fs::write(&old, "legacy body").unwrap();

        let temp = cross_device_temp_path(&new);
        assert_ne!(temp, new, "the staging name is never the destination");
        assert_eq!(
            temp.parent(),
            new.parent(),
            "staging stays on the target device"
        );

        // Debris from an interrupted earlier copy, which the destination-exists
        // check above deliberately does not see.
        fs::write(&temp, "truncated debr").unwrap();
        assert!(
            !new.exists(),
            "a partial copy is never named as the authority"
        );

        migrate_legacy_file_across_devices(&old, &new).unwrap();
        assert_eq!(fs::read_to_string(&new).unwrap(), "legacy body");
        assert!(
            !temp.exists(),
            "the staging name does not survive the rename"
        );
        assert!(!old.exists(), "the source is removed only after the rename");
    }

    #[test]
    fn util_migrate_legacy_file_skips_destination_exists() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let home = dir.path();
        let old = home.join("old.txt");
        let new = home.join("new.txt");
        fs::write(&old, "old").unwrap();
        fs::write(&new, "new").unwrap();

        let res = migrate_legacy_file(&old, &new).unwrap();
        assert!(matches!(res, LegacyMove::SkippedDestinationExists { .. }));
        assert_eq!(fs::read_to_string(&old).unwrap(), "old");
        assert_eq!(fs::read_to_string(&new).unwrap(), "new");
    }

    #[test]
    fn packets_dir_default_is_state_packets() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let home = dir.path();

        // Save and clear env vars
        let orig_packets_dir = std::env::var("BLACKBOX_PACKETS_DIR").ok();
        unsafe {
            std::env::remove_var("BLACKBOX_PACKETS_DIR");
        }

        let packets_dir = blackbox_packets_dir(home);
        assert!(packets_dir == blackbox_state_dir(home).join("packets"));

        // Restore
        if let Some(v) = orig_packets_dir {
            unsafe { std::env::set_var("BLACKBOX_PACKETS_DIR", v) };
        } else {
            unsafe { std::env::remove_var("BLACKBOX_PACKETS_DIR") };
        }
    }
}

/// Debug-build marker for sanctioned blocking contexts (Phase 4 of
/// design/daemon-runtime/concurrency-model.md §5; invariants I1/I2).
///
/// Owner-actor threads (StorePersister, IndexWriterActor, storage GC, the
/// edge-rebuild watcher, …) enter this scope at thread start. The
/// constructor debug-asserts the thread is NOT a tokio runtime thread, so a
/// refactor that accidentally moves an actor body into an async context
/// panics in `cargo test --lib` / debug builds instead of silently blocking
/// runtime workers in production.
pub struct BlockingScope;

impl BlockingScope {
    pub fn enter() -> Self {
        debug_assert!(
            tokio::runtime::Handle::try_current().is_err(),
            "BlockingScope entered on a tokio runtime thread — sanctioned \
             blocking actors must run on dedicated OS threads or the blocking \
             pool (concurrency-model I2)"
        );
        BlockingScope
    }
}

#[cfg(test)]
mod blocking_scope_tests {
    use super::BlockingScope;

    #[test]
    fn enter_on_plain_thread_is_fine() {
        std::thread::spawn(|| {
            let _scope = BlockingScope::enter();
        })
        .join()
        .unwrap();
    }

    #[tokio::test]
    #[should_panic(expected = "BlockingScope entered on a tokio runtime thread")]
    async fn enter_on_runtime_thread_panics_in_debug() {
        let _scope = BlockingScope::enter();
    }
}
