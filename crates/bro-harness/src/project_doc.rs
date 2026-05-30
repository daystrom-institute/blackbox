//! Codex-equivalent AGENTS.md overlay discovery.
//!
//! When the harness is launched WITHOUT a `--system-prompt` override — the flag
//! is *absent*, not the empty-string suppress sentinel — it builds its base
//! system prompt the same way Codex assembles project docs: a global
//! `$CODEX_HOME/AGENTS.md` (+ `AGENTS.override.md`) followed by the repo's
//! `AGENTS.md` files walked from the git root down to the cwd.
//!
//! `AGENTS.md` is the provider-agnostic project-doc filename. The harness backs
//! Claude-compatible providers but reads `AGENTS.md` (not `CLAUDE.md`) so a
//! single project doc serves both Codex and harness dispatches.
//!
//! Three-state `--system-prompt` semantics (resolved in `agent_loop::build`):
//!   * non-empty string  ⇒ explicit override; this discovery is skipped.
//!   * empty string `""`  ⇒ explicit suppress (no overlay) — mirrors Codex
//!     `project_doc_max_bytes=0` + the AGENTS-omitting `CODEX_HOME` overlay.
//!   * absent (`None`)    ⇒ not overridden ⇒ this Codex-style overlay.

use std::path::{Path, PathBuf};

/// Codex's default `project_doc_max_bytes`. Caps the *project* doc chain only;
/// the global `$CODEX_HOME` instructions are uncapped, matching Codex.
const DEFAULT_PROJECT_DOC_MAX_BYTES: usize = 32 * 1024;
const AGENTS_FILE: &str = "AGENTS.md";
const AGENTS_OVERRIDE_FILE: &str = "AGENTS.override.md";

fn project_doc_max_bytes() -> usize {
    std::env::var("BRO_HARNESS_PROJECT_DOC_MAX_BYTES")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_PROJECT_DOC_MAX_BYTES)
}

/// Resolve `$CODEX_HOME`, defaulting to `~/.codex` — the same base the daemon's
/// Codex arm uses (`orchestration::brofile`).
fn codex_home() -> Option<PathBuf> {
    if let Ok(h) = std::env::var("CODEX_HOME") {
        let h = h.trim();
        if !h.is_empty() {
            return Some(PathBuf::from(h));
        }
    }
    dirs::home_dir().map(|h| h.join(".codex"))
}

/// Collect repo `AGENTS.md` paths walking from `cwd` up to (and including) the
/// git root, ordered outermost-first (git root → cwd) so the most-specific doc
/// lands last. If `cwd` is not inside a git repo, only `cwd/AGENTS.md` is
/// considered — Codex does not walk arbitrarily up the filesystem outside a
/// repo.
fn project_agents_paths(cwd: &Path) -> Vec<PathBuf> {
    let mut git_root: Option<PathBuf> = None;
    let mut probe = Some(cwd);
    while let Some(d) = probe {
        if d.join(".git").exists() {
            git_root = Some(d.to_path_buf());
            break;
        }
        probe = d.parent();
    }

    let mut chain: Vec<PathBuf> = Vec::new();
    match git_root {
        Some(root) => {
            let mut d = Some(cwd);
            while let Some(cur) = d {
                let candidate = cur.join(AGENTS_FILE);
                if candidate.is_file() {
                    chain.push(candidate);
                }
                if cur == root {
                    break;
                }
                d = cur.parent();
            }
            chain.reverse();
        }
        None => {
            let candidate = cwd.join(AGENTS_FILE);
            if candidate.is_file() {
                chain.push(candidate);
            }
        }
    }
    chain
}

fn read_nonempty(path: &Path) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(s) if !s.trim().is_empty() => Some(s),
        Ok(_) => None,
        Err(e) => {
            tracing::debug!(path = %path.display(), error = %e, "skip unreadable AGENTS doc");
            None
        }
    }
}

fn truncate_on_char_boundary(s: &mut String, cap: usize) {
    if s.len() <= cap {
        return;
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}

/// Assemble the Codex-equivalent overlay from the process cwd + `$CODEX_HOME`.
/// Returns `None` when no AGENTS docs exist, in which case the caller sends no
/// system prompt (identical to the prior provider-defaults behavior).
pub(crate) fn discover() -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    assemble(&cwd, codex_home().as_deref())
}

/// Pure assembly seam: explicit `cwd` and `codex_home` make this testable
/// without touching process-global env/cwd.
fn assemble(cwd: &Path, codex_home: Option<&Path>) -> Option<String> {
    let mut sections: Vec<String> = Vec::new();
    let mut loaded: Vec<String> = Vec::new();

    // Global scope: $CODEX_HOME/AGENTS.md (+ override), uncapped.
    if let Some(home) = codex_home {
        for name in [AGENTS_FILE, AGENTS_OVERRIDE_FILE] {
            let p = home.join(name);
            if let Some(body) = read_nonempty(&p) {
                sections.push(body);
                loaded.push(p.display().to_string());
            }
        }
    }

    // Project scope: repo AGENTS.md, git root → cwd; the joined chain is capped
    // per Codex `project_doc_max_bytes`.
    let mut project: Vec<String> = Vec::new();
    for p in project_agents_paths(cwd) {
        if let Some(body) = read_nonempty(&p) {
            project.push(body);
            loaded.push(p.display().to_string());
        }
    }
    if !project.is_empty() {
        let mut joined = project.join("\n\n");
        let cap = project_doc_max_bytes();
        if joined.len() > cap {
            truncate_on_char_boundary(&mut joined, cap);
            tracing::warn!(
                cap,
                "project AGENTS.md overlay truncated to BRO_HARNESS_PROJECT_DOC_MAX_BYTES"
            );
        }
        sections.push(joined);
    }

    if sections.is_empty() {
        return None;
    }
    tracing::info!(files = ?loaded, "loaded AGENTS.md overlay as base system prompt");
    Some(sections.join("\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bh-pd-{}", uuid::Uuid::new_v4().simple()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, body).unwrap();
    }

    #[test]
    fn none_when_no_docs() {
        let root = scratch();
        write(&root.join(".git").join("HEAD"), "ref: refs/heads/main\n");
        let cwd = root.join("crate").join("sub");
        fs::create_dir_all(&cwd).unwrap();
        assert!(assemble(&cwd, None).is_none());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn walks_git_root_to_cwd_outermost_first() {
        let root = scratch();
        write(&root.join(".git").join("HEAD"), "ref: refs/heads/main\n");
        write(&root.join(AGENTS_FILE), "ROOT-DOC");
        let cwd = root.join("crate").join("sub");
        write(&cwd.join(AGENTS_FILE), "LEAF-DOC");

        let out = assemble(&cwd, None).expect("docs present");
        let root_at = out.find("ROOT-DOC").unwrap();
        let leaf_at = out.find("LEAF-DOC").unwrap();
        assert!(root_at < leaf_at, "git-root doc must precede cwd doc");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn no_walk_outside_git_repo() {
        // No .git anywhere: only the cwd-level AGENTS.md is read, parents ignored.
        let root = scratch();
        write(&root.join(AGENTS_FILE), "PARENT-DOC");
        let cwd = root.join("child");
        write(&cwd.join(AGENTS_FILE), "CHILD-DOC");

        let out = assemble(&cwd, None).expect("cwd doc present");
        assert!(out.contains("CHILD-DOC"));
        assert!(!out.contains("PARENT-DOC"), "must not walk up outside a repo");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn global_precedes_project_and_includes_override() {
        let home = scratch();
        write(&home.join(AGENTS_FILE), "GLOBAL-DOC");
        write(&home.join(AGENTS_OVERRIDE_FILE), "OVERRIDE-DOC");

        let root = scratch();
        write(&root.join(".git").join("HEAD"), "ref: refs/heads/main\n");
        write(&root.join(AGENTS_FILE), "PROJECT-DOC");

        let out = assemble(&root, Some(&home)).expect("docs present");
        let g = out.find("GLOBAL-DOC").unwrap();
        let o = out.find("OVERRIDE-DOC").unwrap();
        let p = out.find("PROJECT-DOC").unwrap();
        assert!(g < o && o < p, "order: global, override, then project");
        fs::remove_dir_all(&home).ok();
        fs::remove_dir_all(&root).ok();
    }
}
