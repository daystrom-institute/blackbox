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
            let mut source = fs::File::open(old)?;
            let mut dest = fs::File::create(new)?;
            std::io::copy(&mut source, &mut dest)?;
            dest.sync_all()?;
            drop(source);
            drop(dest);
            fs::remove_file(old)?;
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

pub fn migrate_legacy_defaults(home: &Path) -> anyhow::Result<Vec<String>> {
    let mut moved = Vec::new();

    if env_path("BLACKBOX_KNOWLEDGE_PATH").is_none() {
        let old = home.join(".claude-shared").join("blackbox-knowledge.json");
        let new = blackbox_knowledge_path(home);
        if let LegacyMove::Moved { old, new } = migrate_legacy_file(&old, &new)? {
            moved.push(format!("knowledge: {} -> {}", old.display(), new.display()));
        }
    }

    if env_path("BLACKBOX_THREADS_PATH").is_none() {
        let old = home.join(".claude-shared").join("blackbox-threads.json");
        let new = blackbox_threads_path(home);
        if let LegacyMove::Moved { old, new } = migrate_legacy_file(&old, &new)? {
            moved.push(format!("threads: {} -> {}", old.display(), new.display()));
        }
    }

    if env_path("BLACKBOX_NOTES_PATH").is_none() {
        let old = home.join(".claude-shared").join("blackbox-notes.json");
        let new = blackbox_notes_path(home);
        if let LegacyMove::Moved { old, new } = migrate_legacy_file(&old, &new)? {
            moved.push(format!("notes: {} -> {}", old.display(), new.display()));
        }
    }

    if env_path("TRANSCRIPT_SEARCH_INDEX_PATH").is_none() {
        let old = home.join(".claude-shared").join("transcript-index");
        let new = blackbox_index_path(home);
        if let LegacyMove::Moved { old, new } = migrate_legacy_file(&old, &new)? {
            moved.push(format!("index: {} -> {}", old.display(), new.display()));
        }
    }

    if env_path("BLACKBOX_GLOBAL_COMMON_MD").is_none() {
        let old = home.join(".claude-shared").join("BLACKBOX.md");
        let new = blackbox_global_common_md_path(home);
        if let LegacyMove::Moved { old, new } = migrate_legacy_file(&old, &new)? {
            moved.push(format!(
                "blackbox-md: {} -> {}",
                old.display(),
                new.display()
            ));
        }
    }

    // Task 3: ~/.bro/ migration
    let old_bro = home.join(".bro");
    let new_bro = bro_home_dir(home);
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

    #[test]
    fn migrates_legacy_defaults_when_new_targets_absent() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let home = dir.path();
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
        unsafe {
            std::env::set_var("XDG_STATE_HOME", home.join(".local/state"));
        }
        unsafe {
            std::env::set_var("XDG_DATA_HOME", home.join(".local/share"));
        }

        let moved = migrate_legacy_defaults(home).unwrap();
        // Moved: knowledge, threads, notes, index, bro (tasks.json)
        // (claude render migration was removed in Phase 5)
        assert!(
            moved.len() >= 4,
            "expected >=4 moves, got {}: {:?}",
            moved.len(),
            moved
        );
        assert!(blackbox_knowledge_path(home).exists());
        assert!(blackbox_threads_path(home).exists());
        assert!(blackbox_notes_path(home).exists());
        assert!(blackbox_index_path(home).exists());
        assert!(bro_home_dir(home).join("tasks.json").exists());
        assert!(!old_shared.join("blackbox-knowledge.json").exists());
        assert!(!old_bro.exists());

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
        unsafe {
            std::env::remove_var("XDG_STATE_HOME");
        }
        unsafe {
            std::env::remove_var("XDG_DATA_HOME");
        }
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
