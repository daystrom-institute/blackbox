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

use serde_json::Value;
use std::collections::HashSet;
use std::path::Component;
use std::path::{Path, PathBuf};

/// Large-overlay warning threshold. This never truncates instructions; it only
/// catches unexpectedly huge project-doc chains. The default must stay above
/// this repo's standard AGENTS/PROJECT/BLACKBOX/RTK hierarchy.
const DEFAULT_PROJECT_DOC_WARN_BYTES: usize = 256 * 1024;
const MAX_INCLUDE_DEPTH: usize = 8;
const AGENTS_FILE: &str = "AGENTS.md";
const AGENTS_OVERRIDE_FILE: &str = "AGENTS.override.md";
const RIDER_OPEN: &str = "<harness-project-docs>";
const RIDER_CLOSE: &str = "</harness-project-docs>";

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectDocOverlay {
    pub(crate) text: String,
    pub(crate) loaded_paths: Vec<PathBuf>,
}

/// Assemble the Codex-equivalent overlay from the process cwd + `$CODEX_HOME`.
/// Returns `None` when no AGENTS docs exist, in which case the caller sends no
/// system prompt (identical to the prior provider-defaults behavior).
pub(crate) fn discover(cwd: &Path) -> Option<ProjectDocOverlay> {
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
) -> Option<ProjectDocOverlay> {
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
    Some(ProjectDocOverlay {
        text: sections.join("\n\n"),
        loaded_paths: loaded.into_iter().map(PathBuf::from).collect(),
    })
}

#[derive(Debug, Default)]
pub(crate) struct ScopedProjectDocs {
    delivered: HashSet<PathBuf>,
}

impl ScopedProjectDocs {
    /// Build the live dedupe cache from startup docs plus any rider-delivered
    /// docs already present in the session event log. The event log is the
    /// durable source of truth for what the model saw; this set is only a
    /// runtime cache to avoid repeated scans.
    pub(crate) fn from_startup_and_event_log(
        startup_paths: impl IntoIterator<Item = PathBuf>,
        event_log_path: &Path,
    ) -> Self {
        let mut delivered: HashSet<PathBuf> = startup_paths.into_iter().collect();
        delivered.extend(delivered_paths_from_event_log(event_log_path));
        Self { delivered }
    }

    pub(crate) fn rider_for_tool_call(
        &mut self,
        root: &Path,
        tool_name: &str,
        args: &Value,
    ) -> Option<String> {
        let touched = touched_paths(root, tool_name, args);
        if touched.is_empty() {
            return None;
        }

        let doc_names = project_doc_files();
        let mut loaded_paths = self.delivered.clone();
        let mut newly_loaded = Vec::new();
        let mut sections = Vec::new();
        for touched_path in touched {
            let display_touched = display_path(root, &touched_path);
            for doc in scoped_agents_paths(root, &touched_path, &doc_names) {
                let Some(canonical) = canonical_file(&doc) else {
                    continue;
                };
                if self.delivered.contains(&canonical) || loaded_paths.contains(&canonical) {
                    continue;
                }
                let before = sections.len();
                sections.extend(read_doc_tree(
                    &canonical,
                    &mut loaded_paths,
                    &mut newly_loaded,
                    0,
                ));
                if sections.len() > before {
                    tracing::info!(
                        touched = %display_touched,
                        doc = %canonical.display(),
                        "attaching scoped AGENTS.md rider"
                    );
                }
            }
        }

        if sections.is_empty() {
            return None;
        }

        let mut canonical_new = Vec::new();
        for path in newly_loaded.into_iter().map(PathBuf::from) {
            if self.delivered.insert(path.clone()) {
                canonical_new.push(path);
            }
        }
        if canonical_new.is_empty() {
            return None;
        }

        Some(render_scoped_rider(root, &canonical_new, &sections))
    }
}

fn render_scoped_rider(root: &Path, loaded: &[PathBuf], sections: &[String]) -> String {
    let delivered = loaded
        .iter()
        .map(|path| format!("  - {}", path.display()))
        .collect::<Vec<_>>()
        .join("\n");
    let bodies = loaded
        .iter()
        .zip(sections.iter())
        .map(|(path, body)| {
            format!(
                "<INSTRUCTIONS path=\"{}\">\n{}\n</INSTRUCTIONS>",
                display_path(root, path),
                body
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    format!(
        "\n\n{RIDER_OPEN}\ndelivered:\n{delivered}\n\nAdditional AGENTS.md instructions now apply because this tool touched a covered path.\n\n{bodies}\n{RIDER_CLOSE}"
    )
}

fn touched_paths(root: &Path, tool_name: &str, args: &Value) -> Vec<PathBuf> {
    match tool_name {
        "file_read" | "smart_read" | "file_write" | "file_edit" => args
            .get("file_path")
            .and_then(Value::as_str)
            .and_then(|raw| workspace_path(root, raw))
            .into_iter()
            .collect(),
        "apply_patch" => apply_patch_paths(root, args),
        _ => Vec::new(),
    }
}

fn apply_patch_paths(root: &Path, args: &Value) -> Vec<PathBuf> {
    let Some(patch) = args
        .get("source")
        .or_else(|| args.get("patch"))
        .or_else(|| args.get("input"))
        .and_then(Value::as_str)
    else {
        return Vec::new();
    };
    let Ok(parsed) = bro_apply_patch::parse_patch(patch) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for hunk in parsed.hunks {
        match hunk {
            bro_apply_patch::Hunk::AddFile { path, .. }
            | bro_apply_patch::Hunk::DeleteFile { path } => {
                if let Some(path) = workspace_path(root, &path.to_string_lossy()) {
                    out.push(path);
                }
            }
            bro_apply_patch::Hunk::UpdateFile {
                path, move_path, ..
            } => {
                if let Some(path) = workspace_path(root, &path.to_string_lossy()) {
                    out.push(path);
                }
                if let Some(move_path) = move_path
                    && let Some(path) = workspace_path(root, &move_path.to_string_lossy())
                {
                    out.push(path);
                }
            }
        }
    }
    out
}

fn workspace_path(root: &Path, raw: &str) -> Option<PathBuf> {
    let raw = raw.strip_prefix('@').unwrap_or(raw);
    if raw.trim().is_empty() {
        return None;
    }
    let path = Path::new(raw);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let normalized = normalize_lexical(&joined);
    normalized.starts_with(root).then_some(normalized)
}

fn scoped_agents_paths(root: &Path, touched_path: &Path, names: &[String]) -> Vec<PathBuf> {
    let dir = if touched_path.is_dir() {
        touched_path.to_path_buf()
    } else {
        touched_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| root.to_path_buf())
    };
    let mut dirs = Vec::new();
    let mut cursor = Some(dir.as_path());
    while let Some(cur) = cursor {
        dirs.push(cur.to_path_buf());
        if cur == root {
            break;
        }
        cursor = cur.parent();
    }
    dirs.reverse();

    let mut paths = Vec::new();
    for dir in dirs {
        for name in names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                paths.push(candidate);
            }
        }
    }
    paths
}

fn display_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

fn normalize_lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

// one-time session resume scan, before the loop serves turns.
#[allow(clippy::disallowed_methods)]
fn delivered_paths_from_event_log(path: &Path) -> Vec<PathBuf> {
    let Ok(body) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    body.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|line| {
            line.pointer("/event/message/content")
                .and_then(Value::as_array)
                .cloned()
        })
        .flat_map(|blocks| {
            blocks
                .into_iter()
                .filter_map(|block| {
                    block
                        .get("content")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect::<Vec<_>>()
        })
        .flat_map(|content| delivered_paths_from_rider_text(&content))
        .collect()
}

fn delivered_paths_from_rider_text(text: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(RIDER_OPEN) {
        rest = &rest[start + RIDER_OPEN.len()..];
        let Some(end) = rest.find(RIDER_CLOSE) else {
            break;
        };
        let block = &rest[..end];
        let mut in_delivered_list = false;
        for line in block.lines() {
            let trimmed = line.trim();
            if trimmed == "delivered:" {
                in_delivered_list = true;
                continue;
            }
            if in_delivered_list && trimmed.is_empty() {
                break;
            }
            if !in_delivered_list {
                continue;
            }
            if let Some(path) = trimmed.strip_prefix("- ") {
                out.push(PathBuf::from(path.trim()));
            }
        }
        rest = &rest[end + RIDER_CLOSE.len()..];
    }
    out
}

#[cfg(test)]
// Filesystem fixtures intentionally exercise scoped project-document discovery.
#[allow(clippy::disallowed_methods)]
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
        .map(|overlay| overlay.text)
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
            out.text.contains(&body),
            "warning threshold must not mutate project instructions"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn scoped_rider_loads_child_doc_once_after_first_touch() {
        let root = scratch().canonicalize().unwrap();
        write(&root.join(".git").join("HEAD"), "ref: refs/heads/main\n");
        write(&root.join(AGENTS_FILE), "ROOT-DOC");
        let child = root.join("crates").join("thing");
        write(&child.join(AGENTS_FILE), "CHILD-DOC");
        write(&child.join("src").join("lib.rs"), "fn main() {}\n");
        let startup = assemble(&root, None, &default_docs(), DEFAULT_PROJECT_DOC_WARN_BYTES)
            .expect("startup docs");
        let mut scoped =
            ScopedProjectDocs::from_startup_and_event_log(startup.loaded_paths, &root.join("none"));

        let rider = scoped
            .rider_for_tool_call(
                &root,
                "file_read",
                &serde_json::json!({"file_path": "crates/thing/src/lib.rs"}),
            )
            .expect("child doc rider");
        assert!(rider.contains(RIDER_OPEN), "{rider}");
        assert!(rider.contains("CHILD-DOC"), "{rider}");
        assert!(!rider.contains("ROOT-DOC"), "{rider}");

        let again = scoped.rider_for_tool_call(
            &root,
            "smart_read",
            &serde_json::json!({"file_path": "crates/thing/src/other.rs"}),
        );
        assert_eq!(again, None);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn scoped_rider_ignores_shell_run() {
        let root = scratch().canonicalize().unwrap();
        write(&root.join(".git").join("HEAD"), "ref: refs/heads/main\n");
        let child = root.join("crates").join("thing");
        write(&child.join(AGENTS_FILE), "CHILD-DOC");
        let mut scoped = ScopedProjectDocs::default();

        let rider = scoped.rider_for_tool_call(
            &root,
            "shell_run",
            &serde_json::json!({"cmd": "cat crates/thing/src/lib.rs"}),
        );
        assert_eq!(rider, None);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn scoped_rider_extracts_apply_patch_paths() {
        let root = scratch().canonicalize().unwrap();
        write(&root.join(".git").join("HEAD"), "ref: refs/heads/main\n");
        let child = root.join("crates").join("thing");
        write(&child.join(AGENTS_FILE), "CHILD-DOC");
        let mut scoped = ScopedProjectDocs::default();
        let patch = "*** Begin Patch\n*** Add File: crates/thing/src/lib.rs\n+fn main() {}\n*** End Patch\n";

        let rider = scoped
            .rider_for_tool_call(&root, "apply_patch", &serde_json::json!({"source": patch}))
            .expect("apply_patch should load child doc");
        assert!(rider.contains("CHILD-DOC"), "{rider}");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn scoped_rider_reconstructs_delivered_paths_from_event_log() {
        let root = scratch().canonicalize().unwrap();
        write(&root.join(".git").join("HEAD"), "ref: refs/heads/main\n");
        let child = root.join("crates").join("thing");
        let agents = child.join(AGENTS_FILE);
        write(&agents, "CHILD-DOC");
        let rider =
            render_scoped_rider(&root, std::slice::from_ref(&agents), &["CHILD-DOC".into()]);
        let log_path = root.join("session.events.jsonl");
        write(
            &log_path,
            &serde_json::json!({
                "ts": "2026-01-01T00:00:00Z",
                "event": {
                    "type": "user",
                    "message": {
                        "content": [{
                            "type": "tool_result",
                            "content": rider,
                        }]
                    }
                }
            })
            .to_string(),
        );

        let mut scoped =
            ScopedProjectDocs::from_startup_and_event_log(Vec::<PathBuf>::new(), &log_path);
        let rider = scoped.rider_for_tool_call(
            &root,
            "file_read",
            &serde_json::json!({"file_path": "crates/thing/src/lib.rs"}),
        );
        assert_eq!(rider, None);
        fs::remove_dir_all(&root).ok();
    }
}
