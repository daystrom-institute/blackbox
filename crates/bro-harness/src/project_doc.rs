//! Codex-equivalent AGENTS.md overlay discovery.
//!
//! When the harness is launched WITHOUT a `--system-prompt` override — the flag
//! is *absent*, not the empty-string suppress sentinel — it builds its base
//! system prompt the same way Codex assembles project docs: a global
//! `$CODEX_HOME/AGENTS.md` (+ `AGENTS.override.md`) followed by the repo's
//! project instruction docs walked from the git root down to the cwd. The
//! default project instruction doc is `AGENTS.md`; sandbox/beta sessions can set
//! `BRO_HARNESS_PROJECT_DOC_FILES=AGENTS_BETA.md` to load a different repo doc
//! without changing global Codex instructions.
//!
//! `AGENTS.md` is the provider-agnostic project-doc filename. The harness backs
//! Claude-compatible providers but reads `AGENTS.md` (not `CLAUDE.md`) so a
//! single project doc serves both Codex and harness dispatches.
//!
//! Three-state `--system-prompt` semantics (resolved in `agent_loop::build`):
//!   * non-empty string  ⇒ explicit override; this discovery is skipped.
//!   * empty string `""`  ⇒ explicit suppress (no overlay).
//!   * absent (`None`)    ⇒ not overridden ⇒ this Codex-style overlay.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Large-overlay warning threshold. This never truncates instructions; it only
/// catches unexpectedly huge project-doc chains. The default must stay above
/// this repo's standard AGENTS/PROJECT/BLACKBOX/RTK hierarchy.
const DEFAULT_PROJECT_DOC_WARN_BYTES: usize = 256 * 1024;
const MAX_INCLUDE_DEPTH: usize = 8;
const AGENTS_FILE: &str = "AGENTS.md";
const AGENTS_OVERRIDE_FILE: &str = "AGENTS.override.md";

fn project_doc_warn_bytes() -> usize {
    crate::transport::session_var("BRO_HARNESS_PROJECT_DOC_WARN_BYTES")
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_PROJECT_DOC_WARN_BYTES)
}

fn project_doc_files() -> Vec<String> {
    crate::transport::session_var("BRO_HARNESS_PROJECT_DOC_FILES")
        .as_deref()
        .map(parse_project_doc_files)
        .filter(|names| !names.is_empty())
        .unwrap_or_else(|| vec![AGENTS_FILE.to_string()])
}

fn parse_project_doc_files(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter(|s| !s.contains('/') && !s.contains('\\'))
        .map(ToString::to_string)
        .collect()
}

/// Resolve `$CODEX_HOME`, defaulting to `~/.codex` — the same base the daemon's
/// Codex arm uses (`orchestration::brofile`).
fn codex_home() -> Option<PathBuf> {
    if let Some(h) = crate::transport::session_var("CODEX_HOME") {
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
fn project_agents_paths(cwd: &Path, names: &[String]) -> Vec<PathBuf> {
    let mut git_root: Option<PathBuf> = None;
    let mut probe = Some(cwd);
    while let Some(d) = probe {
        if d.join(".git").exists() {
            git_root = Some(d.to_path_buf());
            break;
        }
        probe = d.parent();
    }

    let dirs = match git_root {
        Some(root) => {
            let mut dirs = Vec::new();
            let mut d = Some(cwd);
            while let Some(cur) = d {
                dirs.push(cur.to_path_buf());
                if cur == root {
                    break;
                }
                d = cur.parent();
            }
            dirs.reverse();
            dirs
        }
        None => vec![cwd.to_path_buf()],
    };
    let mut chain: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        for name in names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                chain.push(candidate);
            }
        }
    }
    chain
}

// one-time session-start project-doc read, before the loop serves turns.
#[allow(clippy::disallowed_methods)]
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

fn canonical_file(path: &Path) -> Option<PathBuf> {
    let canonical = path.canonicalize().ok()?;
    canonical.is_file().then_some(canonical)
}

fn is_allowed_instruction_doc(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if matches!(
        name,
        "AGENTS.md"
            | "AGENTS.override.md"
            | "BLACKBOX.md"
            | "CLAUDE.md"
            | "GEMINI.md"
            | "PROJECT.md"
            | "RTK.md"
            | "README.md"
    ) {
        return true;
    }
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("md" | "markdown" | "txt")
    )
}

fn resolve_include(referrer: &Path, mention: &str) -> Option<PathBuf> {
    let raw = mention.strip_prefix('@')?;
    if raw.is_empty() {
        return None;
    }
    let candidate = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        referrer.parent()?.join(raw)
    };
    let canonical = canonical_file(&candidate)?;
    is_allowed_instruction_doc(&canonical).then_some(canonical)
}

fn extract_at_mentions(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let chars: Vec<(usize, char)> = body.char_indices().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].1 != '@' {
            i += 1;
            continue;
        }
        let start = chars[i].0;
        let mut end = body.len();
        let mut j = i + 1;
        while j < chars.len() {
            let (idx, ch) = chars[j];
            if ch.is_whitespace() || matches!(ch, ')' | ']' | '}' | '>' | '`' | '"' | '\'' | ',') {
                end = idx;
                break;
            }
            j += 1;
        }
        let mention = body[start..end].trim_end_matches(['.', ';', ':']);
        if mention.len() > 1 {
            out.push(mention.to_string());
        }
        i = j.max(i + 1);
    }
    out
}

fn read_doc_tree(
    path: &Path,
    loaded_paths: &mut HashSet<PathBuf>,
    loaded: &mut Vec<String>,
    depth: usize,
) -> Vec<String> {
    if depth > MAX_INCLUDE_DEPTH {
        tracing::warn!(
            path = %path.display(),
            max_depth = MAX_INCLUDE_DEPTH,
            "skipping nested AGENTS include beyond max depth"
        );
        return Vec::new();
    }
    let Some(canonical) = canonical_file(path) else {
        return Vec::new();
    };
    if !loaded_paths.insert(canonical.clone()) {
        return Vec::new();
    }
    let Some(body) = read_nonempty(&canonical) else {
        return Vec::new();
    };
    loaded.push(canonical.display().to_string());

    let mut sections = vec![body.clone()];
    for mention in extract_at_mentions(&body) {
        let Some(include) = resolve_include(&canonical, &mention) else {
            continue;
        };
        sections.extend(read_doc_tree(&include, loaded_paths, loaded, depth + 1));
    }
    sections
}

/// Assemble the Codex-equivalent overlay from the process cwd + `$CODEX_HOME`.
/// Returns `None` when no AGENTS docs exist, in which case the caller sends no
/// system prompt (identical to the prior provider-defaults behavior).
pub(crate) fn discover(cwd: &Path) -> Option<String> {
    let project_doc_files = project_doc_files();
    assemble(
        cwd,
        codex_home().as_deref(),
        &project_doc_files,
        project_doc_warn_bytes(),
    )
}

/// Pure assembly seam: explicit `cwd` and `codex_home` make this testable
/// without touching process-global env/cwd.
fn assemble(
    cwd: &Path,
    codex_home: Option<&Path>,
    project_doc_files: &[String],
    project_doc_warn_bytes: usize,
) -> Option<String> {
    let mut sections: Vec<String> = Vec::new();
    let mut loaded: Vec<String> = Vec::new();
    let mut loaded_paths: HashSet<PathBuf> = HashSet::new();

    // Global scope: $CODEX_HOME/AGENTS.md (+ override), uncapped.
    if let Some(home) = codex_home {
        for name in [AGENTS_FILE, AGENTS_OVERRIDE_FILE] {
            let p = home.join(name);
            sections.extend(read_doc_tree(&p, &mut loaded_paths, &mut loaded, 0));
        }
    }

    // Project scope: repo AGENTS.md, git root → cwd. Never truncate
    // instructions here; a large overlay is the operator's context decision.
    let mut project: Vec<String> = Vec::new();
    for p in project_agents_paths(cwd, project_doc_files) {
        project.extend(read_doc_tree(&p, &mut loaded_paths, &mut loaded, 0));
    }
    if !project.is_empty() {
        let joined = project.join("\n\n");
        if project_doc_warn_bytes > 0 && joined.len() > project_doc_warn_bytes {
            tracing::warn!(
                bytes = joined.len(),
                warn_bytes = project_doc_warn_bytes,
                "project AGENTS.md overlay exceeds BRO_HARNESS_PROJECT_DOC_WARN_BYTES"
            );
        }
        sections.push(joined);
    }

    if sections.is_empty() {
        return None;
    }
    let manifest = format!(
        "[project-docs]\nselected: {}\nloaded:\n{}\n[/project-docs]",
        project_doc_files.join(", "),
        loaded
            .iter()
            .map(|path| format!("  - {path}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    sections.insert(0, manifest);
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

    fn default_docs() -> Vec<String> {
        vec![AGENTS_FILE.to_string()]
    }

    fn assemble_default(
        cwd: &Path,
        codex_home: Option<&Path>,
        project_doc_files: &[String],
    ) -> Option<String> {
        assemble(
            cwd,
            codex_home,
            project_doc_files,
            DEFAULT_PROJECT_DOC_WARN_BYTES,
        )
    }

    #[test]
    fn none_when_no_docs() {
        let root = scratch();
        write(&root.join(".git").join("HEAD"), "ref: refs/heads/main\n");
        let cwd = root.join("crate").join("sub");
        fs::create_dir_all(&cwd).unwrap();
        assert!(assemble_default(&cwd, None, &default_docs()).is_none());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn walks_git_root_to_cwd_outermost_first() {
        let root = scratch();
        write(&root.join(".git").join("HEAD"), "ref: refs/heads/main\n");
        write(&root.join(AGENTS_FILE), "ROOT-DOC");
        let cwd = root.join("crate").join("sub");
        write(&cwd.join(AGENTS_FILE), "LEAF-DOC");

        let out = assemble_default(&cwd, None, &default_docs()).expect("docs present");
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

        let out = assemble_default(&cwd, None, &default_docs()).expect("cwd doc present");
        assert!(out.contains("CHILD-DOC"));
        assert!(
            !out.contains("PARENT-DOC"),
            "must not walk up outside a repo"
        );
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

        let out = assemble_default(&root, Some(&home), &default_docs()).expect("docs present");
        let g = out.find("GLOBAL-DOC").unwrap();
        let o = out.find("OVERRIDE-DOC").unwrap();
        let p = out.find("PROJECT-DOC").unwrap();
        assert!(g < o && o < p, "order: global, override, then project");
        fs::remove_dir_all(&home).ok();
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn recursively_merges_at_mentioned_instruction_docs() {
        let root = scratch();
        write(&root.join(".git").join("HEAD"), "ref: refs/heads/main\n");
        write(
            &root.join(AGENTS_FILE),
            "ROOT-DOC\nRead @PROJECT.md and @docs/EXTRA.md",
        );
        write(&root.join("PROJECT.md"), "PROJECT-DOC\nSee @docs/NESTED.md");
        write(&root.join("docs").join("EXTRA.md"), "EXTRA-DOC");
        write(&root.join("docs").join("NESTED.md"), "NESTED-DOC");

        let out = assemble_default(&root, None, &default_docs()).expect("docs present");
        let root_at = out.find("ROOT-DOC").unwrap();
        let project_at = out.find("PROJECT-DOC").unwrap();
        let extra_at = out.find("EXTRA-DOC").unwrap();
        let nested_at = out.find("NESTED-DOC").unwrap();
        assert!(
            root_at < project_at && project_at < nested_at && root_at < extra_at,
            "includes should be appended after the referring doc: {out}"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn absolute_at_mentions_are_instruction_doc_only_and_deduped() {
        let root = scratch();
        write(&root.join(".git").join("HEAD"), "ref: refs/heads/main\n");
        let external = scratch();
        let blackbox = external.join("BLACKBOX.md");
        let secret = external.join("secret.json");
        write(&blackbox, "BLACKBOX-DOC");
        write(&secret, "SECRET");
        write(
            &root.join(AGENTS_FILE),
            &format!(
                "ROOT-DOC\n@{}\n@{}\n@{}",
                blackbox.display(),
                blackbox.display(),
                secret.display()
            ),
        );

        let out = assemble_default(&root, None, &default_docs()).expect("docs present");
        assert!(out.contains("BLACKBOX-DOC"));
        assert_eq!(out.matches("BLACKBOX-DOC").count(), 1);
        assert!(!out.contains("SECRET"));
        fs::remove_dir_all(&root).ok();
        fs::remove_dir_all(&external).ok();
    }

    #[test]
    fn alternate_project_doc_file_can_replace_agents_md() {
        let root = scratch();
        write(&root.join(".git").join("HEAD"), "ref: refs/heads/main\n");
        write(&root.join(AGENTS_FILE), "NORMAL-DOC");
        write(&root.join("AGENTS_BETA.md"), "BETA-DOC");

        let docs = vec!["AGENTS_BETA.md".to_string()];
        let out = assemble_default(&root, None, &docs).expect("docs present");
        assert!(out.contains("selected: AGENTS_BETA.md"));
        assert!(out.contains("BETA-DOC"));
        assert!(!out.contains("NORMAL-DOC"));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn project_docs_over_warning_threshold_are_not_truncated() {
        let root = scratch();
        write(&root.join(".git").join("HEAD"), "ref: refs/heads/main\n");
        let body = "LONG-DOC-".repeat(16);
        write(&root.join(AGENTS_FILE), &body);

        let out = assemble(&root, None, &default_docs(), 8).expect("docs present");
        assert!(
            out.contains(&body),
            "warning threshold must not mutate project instructions"
        );
        fs::remove_dir_all(&root).ok();
    }
}
