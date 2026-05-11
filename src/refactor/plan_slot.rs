// Plan-file slot policy (RX-F1b).
//
// All `output_path` writes and `bbox_refactor_apply(plan_path=…)` reads are
// confined to the daemon-owned directory:
//
//   $BLACKBOX_STATE_DIR/refactor/plans/
//
// Sibling directories are reserved for future phases:
//   $BLACKBOX_STATE_DIR/refactor/diagnostics/  — RX-F2a compiler-diagnostic capture
//   $BLACKBOX_STATE_DIR/refactor/runs/         — RX-F2b run-state scratch
//
// Absolute paths and relative paths that canonicalize outside the slot are
// rejected with error.bad_input(code=plan_path_outside_slot).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Resolve `$BLACKBOX_STATE_DIR` without requiring a pre-loaded `home` argument.
/// Mirrors `util::blackbox_state_dir` but self-contained for use from the
/// refactor module which may run before the full daemon config is available.
fn state_dir() -> Result<PathBuf> {
    if let Ok(v) = std::env::var("BLACKBOX_STATE_DIR") {
        if !v.trim().is_empty() {
            return Ok(PathBuf::from(v));
        }
    }
    let home = dirs::home_dir().context("cannot determine home directory")?;
    let state =
        dirs::state_dir().unwrap_or_else(|| home.join(".local").join("state"));
    Ok(state.join("blackbox"))
}

/// Create `$BLACKBOX_STATE_DIR/refactor/plans/` and return its canonical path.
pub fn ensure_plan_slot() -> Result<PathBuf> {
    let slot = state_dir()?.join("refactor").join("plans");
    fs::create_dir_all(&slot)
        .with_context(|| format!("creating plan slot directory {}", slot.display()))?;
    slot.canonicalize()
        .with_context(|| format!("canonicalizing plan slot {}", slot.display()))
}

/// Lexically normalize a path: process `.` and `..` without filesystem access.
/// This is used to detect slot-escape attempts before the path exists on disk.
fn normalize_path(path: &Path) -> PathBuf {
    let mut parts: Vec<std::path::Component<'_>> = Vec::new();
    for c in path.components() {
        match c {
            std::path::Component::ParentDir => {
                // Only pop a normal component — don't pop the root or a prefix.
                if matches!(parts.last(), Some(std::path::Component::Normal(_))) {
                    parts.pop();
                }
            }
            std::path::Component::CurDir => {}
            other => parts.push(other),
        }
    }
    parts.iter().collect()
}

fn reject_if_outside(normalized: &Path, slot: &Path, label: &str) -> Result<()> {
    if !normalized.starts_with(slot) {
        anyhow::bail!(
            "error.bad_input(code=plan_path_outside_slot): {} resolves to {} which is outside the plan slot {}",
            label,
            normalized.display(),
            slot.display()
        );
    }
    Ok(())
}

/// Resolve a relative `output_path` string to an absolute path inside the
/// plan slot.  Absolute paths and paths that escape the slot are rejected.
pub fn resolve_plan_write_path(output_path: &str) -> Result<PathBuf> {
    let p = Path::new(output_path);
    if p.is_absolute() {
        anyhow::bail!(
            "error.bad_input(code=plan_path_outside_slot): output_path must be a relative \
             filename, not an absolute path ({})",
            output_path
        );
    }
    // ensure_plan_slot() creates the directory and returns a canonical path.
    let slot = ensure_plan_slot()?;
    let normalized = normalize_path(&slot.join(p));
    reject_if_outside(&normalized, &slot, output_path)?;
    Ok(normalized)
}

/// Resolve a `plan_path` string (from `bbox_refactor_apply`) to an absolute
/// path inside the plan slot.  Absolute paths and slot-escaping paths are
/// rejected.
pub fn resolve_plan_read_path(plan_path: &str) -> Result<PathBuf> {
    let p = Path::new(plan_path);
    if p.is_absolute() {
        anyhow::bail!(
            "error.bad_input(code=plan_path_outside_slot): plan_path must be a relative \
             filename, not an absolute path ({})",
            plan_path
        );
    }
    // Use ensure_plan_slot() to get a canonical slot path (same as write side).
    // This creates the directory on first call; that is acceptable for the read
    // path because the slot must already exist if a plan file is being read.
    let slot = ensure_plan_slot()?;
    let normalized = normalize_path(&slot.join(p));
    reject_if_outside(&normalized, &slot, plan_path)?;
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    fn with_state_dir<F: FnOnce()>(dir: &Path, f: F) {
        let _lock = crate::util::test_env_lock();
        let old = env::var("BLACKBOX_STATE_DIR").ok();
        unsafe { env::set_var("BLACKBOX_STATE_DIR", dir) };
        f();
        match old {
            Some(v) => unsafe { env::set_var("BLACKBOX_STATE_DIR", v) },
            None => unsafe { env::remove_var("BLACKBOX_STATE_DIR") },
        }
    }

    #[test]
    fn slot_respects_env_override() {
        let tmp = tempfile::tempdir().unwrap();
        with_state_dir(tmp.path(), || {
            let slot = ensure_plan_slot().unwrap();
            assert!(slot.starts_with(tmp.path()));
            assert!(slot.ends_with("refactor/plans"));
            assert!(slot.exists());
        });
    }

    #[test]
    fn write_path_resolves_under_slot() {
        let tmp = tempfile::tempdir().unwrap();
        with_state_dir(tmp.path(), || {
            let resolved = resolve_plan_write_path("my-plan.json").unwrap();
            let slot = ensure_plan_slot().unwrap();
            assert!(resolved.starts_with(&slot));
            assert!(resolved.ends_with("my-plan.json"));
        });
    }

    #[test]
    fn write_path_rejects_absolute() {
        let tmp = tempfile::tempdir().unwrap();
        with_state_dir(tmp.path(), || {
            let err = resolve_plan_write_path("/tmp/evil.json").unwrap_err();
            assert!(err.to_string().contains("plan_path_outside_slot"));
        });
    }

    #[test]
    fn write_path_rejects_escape() {
        let tmp = tempfile::tempdir().unwrap();
        with_state_dir(tmp.path(), || {
            let err = resolve_plan_write_path("../../etc/passwd").unwrap_err();
            assert!(err.to_string().contains("plan_path_outside_slot"));
        });
    }

    #[test]
    fn read_path_rejects_escape() {
        let tmp = tempfile::tempdir().unwrap();
        with_state_dir(tmp.path(), || {
            let err = resolve_plan_read_path("../../foo.json").unwrap_err();
            assert!(err.to_string().contains("plan_path_outside_slot"));
        });
    }
}
