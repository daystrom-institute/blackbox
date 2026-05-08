//! Shared utilities. Keep this module small — only utilities that
//! genuinely have multiple callers should live here. One-callers
//! belong with their owner.

use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT_BLACKBOX_MCP_NAME: &str = "blackbox";

#[cfg(test)]
static TEST_ENV_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();

#[cfg(test)]
pub(crate) fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    TEST_ENV_LOCK
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap()
}

/// ISO-8601 UTC timestamp with second precision and a trailing `Z`.
/// Canonical format for every timestamp written into bbox stores
/// (knowledge, threads, notes, tool_docs). Hoisted from per-store
/// `Self::now_iso()` duplicates so the format stays consistent.
pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

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
    env_path("BLACKBOX_PACKETS_DIR").unwrap_or_else(|| blackbox_state_dir(home))
}

pub fn blackbox_artifacts_dir(home: &Path) -> PathBuf {
    env_path("BLACKBOX_ARTIFACTS_DIR").unwrap_or_else(|| blackbox_state_dir(home).join("artifacts"))
}

/// Provider-neutral global memory file. Lives under `~/.blackbox/`
/// (parallel to `~/.codex/`, `~/.gemini/`) — *not* `~/.claude-shared/`,
/// which is claude-specific multi-account state. Each provider's global
/// memory file `@imports` (or, for opencode, lists in its `instructions`
/// config) this path so they share one canonical body of guidance.
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

fn migrate_legacy_path(old: &Path, new: &Path) -> anyhow::Result<bool> {
    if !old.exists() || new.exists() {
        return Ok(false);
    }
    if let Some(parent) = new.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(old, new)?;
    Ok(true)
}

pub fn migrate_legacy_defaults(home: &Path) -> anyhow::Result<Vec<String>> {
    let mut moved = Vec::new();

    if env_path("BLACKBOX_KNOWLEDGE_PATH").is_none() {
        let old = home.join(".claude-shared").join("blackbox-knowledge.json");
        let new = blackbox_knowledge_path(home);
        if migrate_legacy_path(&old, &new)? {
            moved.push(format!("knowledge: {} -> {}", old.display(), new.display()));
        }
    }

    if env_path("BLACKBOX_THREADS_PATH").is_none() {
        let old = home.join(".claude-shared").join("blackbox-threads.json");
        let new = blackbox_threads_path(home);
        if migrate_legacy_path(&old, &new)? {
            moved.push(format!("threads: {} -> {}", old.display(), new.display()));
        }
    }

    if env_path("BLACKBOX_NOTES_PATH").is_none() {
        let old = home.join(".claude-shared").join("blackbox-notes.json");
        let new = blackbox_notes_path(home);
        if migrate_legacy_path(&old, &new)? {
            moved.push(format!("notes: {} -> {}", old.display(), new.display()));
        }
    }

    if env_path("TRANSCRIPT_SEARCH_INDEX_PATH").is_none() {
        let old = home.join(".claude-shared").join("transcript-index");
        let new = blackbox_index_path(home);
        if migrate_legacy_path(&old, &new)? {
            moved.push(format!("index: {} -> {}", old.display(), new.display()));
        }
    }

    if env_path("BRO_HOME").is_none() {
        let old = home.join(".bro");
        let new = bro_home_dir(home);
        if migrate_legacy_path(&old, &new)? {
            moved.push(format!("bro-home: {} -> {}", old.display(), new.display()));
        }
    }

    if env_path("BLACKBOX_GLOBAL_COMMON_MD").is_none() {
        let old = home.join(".claude-shared").join("BLACKBOX.md");
        let new = blackbox_global_common_md_path(home);
        if migrate_legacy_path(&old, &new)? {
            moved.push(format!(
                "blackbox-md: {} -> {}",
                old.display(),
                new.display()
            ));
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
        let dir = tempdir().unwrap();
        let home = dir.path();
        std::env::remove_var("BLACKBOX_STATE_DIR");
        std::env::remove_var("BLACKBOX_KNOWLEDGE_PATH");
        std::env::remove_var("BLACKBOX_THREADS_PATH");
        std::env::remove_var("BLACKBOX_NOTES_PATH");
        std::env::remove_var("BLACKBOX_PINS_PATH");
        std::env::remove_var("TRANSCRIPT_SEARCH_INDEX_PATH");
        std::env::remove_var("BRO_HOME");

        let state = blackbox_state_dir(home);
        assert!(state.ends_with(".local/state/blackbox") || state.ends_with("blackbox"));
        assert!(blackbox_knowledge_path(home).ends_with("blackbox-knowledge.json"));
        assert!(blackbox_threads_path(home).ends_with("blackbox-threads.json"));
        assert!(blackbox_notes_path(home).ends_with("blackbox-notes.json"));
        assert!(blackbox_pins_path(home).ends_with("blackbox-pins.json"));
        assert!(blackbox_index_path(home).ends_with("blackbox/index"));
        assert!(bro_home_dir(home).ends_with("blackbox/bro"));
    }

    #[test]
    fn migrates_legacy_defaults_when_new_targets_absent() {
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

        std::env::remove_var("BLACKBOX_STATE_DIR");
        std::env::remove_var("BLACKBOX_KNOWLEDGE_PATH");
        std::env::remove_var("BLACKBOX_THREADS_PATH");
        std::env::remove_var("BLACKBOX_NOTES_PATH");
        std::env::remove_var("BLACKBOX_PINS_PATH");
        std::env::remove_var("TRANSCRIPT_SEARCH_INDEX_PATH");
        std::env::remove_var("BRO_HOME");
        std::env::set_var("XDG_STATE_HOME", home.join(".local/state"));
        std::env::set_var("XDG_DATA_HOME", home.join(".local/share"));

        let moved = migrate_legacy_defaults(home).unwrap();
        assert_eq!(moved.len(), 5);
        assert!(blackbox_knowledge_path(home).exists());
        assert!(blackbox_threads_path(home).exists());
        assert!(blackbox_notes_path(home).exists());
        assert!(blackbox_index_path(home).exists());
        assert!(bro_home_dir(home).join("tasks.json").exists());
        assert!(!old_shared.join("blackbox-knowledge.json").exists());
        assert!(!old_bro.exists());
    }
}
