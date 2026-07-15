//! Workspace tools: file, shell, and git. Ported from daystrom-mk2
//! `Daystrom.Worker/Tools/{FileTools,ShellTools,GitWorkspaceTools}`.
//!
//! File paths are normalized — relative paths join `cx.root`, absolute
//! paths are accepted as-is. There is no containment boundary here: the
//! harness has no other sandboxing machinery, and `shell` already escapes
//! any lexical `cx.root` with `git -C`, `find`, `tee`, `sed -i`, etc.
//! Shell commands pass through [`SafetyPolicy::deny_command`]; `git_commit`
//! additionally rejects staged sensitive files.

use crate::tool::{FreeformGrammar, Tool, ToolAnnotations, ToolCx, ToolResult, schema_for};
use async_trait::async_trait;
use globset::{Glob as GlobPattern, GlobBuilder};
use ignore::WalkBuilder;
use regex::Regex;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Effective workspace root. Normally this is the harness launch root. After
/// `exit_worktree(publish)` removes a managed fleet worktree, the harness
/// process may continue running for retros/cleanup while `cx.root` points at a
/// deleted path. In that one managed case, transition read/shell/git tools back
/// to the base repository advertised by the fleet env.
pub(crate) fn effective_root(root: &Path) -> PathBuf {
    let base_repo = std::env::var_os("BRO_FLEET_BASE_REPO").map(PathBuf::from);
    let worktree_root = std::env::var_os("BRO_FLEET_WORKTREE_ROOT").map(PathBuf::from);
    effective_root_with_env(root, base_repo.as_deref(), worktree_root.as_deref())
}

fn effective_root_with_env(
    root: &Path,
    base_repo: Option<&Path>,
    worktree_root: Option<&Path>,
) -> PathBuf {
    let root_norm = normalize_lexical(root);
    if root_norm.exists() {
        return root_norm;
    }
    let Some(base_repo) = base_repo.filter(|path| path.exists()) else {
        return root_norm;
    };
    let Some(worktree_root) = worktree_root else {
        return root_norm;
    };
    let worktree_root = normalize_lexical(worktree_root);
    if root_norm.starts_with(&worktree_root) {
        normalize_lexical(base_repo)
    } else {
        root_norm
    }
}

#[derive(Deserialize, JsonSchema)]
struct SandboxStatusInput {
    /// Optional root to inspect. Use the `cwd` returned by enter_worktree to
    /// inspect a managed worktree after entering it. Omit to inspect the
    /// harness launch root.
    #[serde(default)]
    root: Option<String>,
    /// Number of dirty git status entries to include. Default 12.
    #[serde(default)]
    status_limit: Option<usize>,
}

pub struct SandboxStatus;

#[async_trait]
impl Tool for SandboxStatus {
    fn name(&self) -> &str {
        "sandbox_status"
    }

    fn description(&self) -> &str {
        "Return a compact sandbox grounding manifest: launch/effective root, git/worktree identity, selected project-doc env, redacted session env, and process env keys visible to shell children. Pass root=<enter_worktree cwd> to inspect a managed worktree after entering it."
    }

    fn input_schema(&self) -> Value {
        schema_for::<SandboxStatusInput>()
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            ..Default::default()
        }
    }

    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let args: SandboxStatusInput = match serde_json::from_value(input) {
            Ok(args) => args,
            Err(e) => return ToolResult::Error(format!("bad input: {e}")),
        };
        // git_status_manifest shells out via sync `Command::output` — keep
        // the child-process wait off the runtime workers.
        let cx = cx.clone();
        crate::tool::call_blocking(move || ToolResult::from_result(sandbox_status(&cx, args))).await
    }
}

fn sandbox_status(cx: &ToolCx, args: SandboxStatusInput) -> anyhow::Result<Value> {
    sandbox_status_manifest(cx, args.root.as_deref(), args.status_limit)
}

pub(crate) fn sandbox_status_manifest(
    cx: &ToolCx,
    root: Option<&str>,
    status_limit: Option<usize>,
) -> anyhow::Result<Value> {
    let (root, root_source) = match root {
        Some(raw) if !raw.trim().is_empty() => (normalize_lexical(&PathBuf::from(raw)), "explicit"),
        _ => (effective_root(&cx.root), "launch"),
    };
    if !root.exists() {
        anyhow::bail!("root does not exist: {}", root.display());
    }
    let session_env = redact_env(&cx.session_env);
    let process_env = visible_process_env();
    let shell_path = shell_path_manifest();
    let tool_resolution = tool_resolution_manifest(["rtk", "rg", "git", "cargo", "rustc"]);
    Ok(json!({
        "launch_root": cx.root,
        "inspected_root": root,
        "root_source": root_source,
        "git": git_status_manifest(&root, status_limit.unwrap_or(12)),
        "fleet_worktree": {
            "base_repo": std::env::var("BRO_FLEET_BASE_REPO").ok(),
            "parent_worktree": std::env::var("BRO_FLEET_PARENT_WORKTREE").ok(),
            "worktree_root": std::env::var("BRO_FLEET_WORKTREE_ROOT").ok(),
            "worktree_branch": std::env::var("BRO_FLEET_WORKTREE_BRANCH").ok(),
        },
        "project_docs": {
            "selected": cx.session_env
                .get("BRO_HARNESS_PROJECT_DOC_FILES")
                .cloned()
                .or_else(|| std::env::var("BRO_HARNESS_PROJECT_DOC_FILES").ok()),
            "max_bytes": cx.session_env
                .get("BRO_HARNESS_PROJECT_DOC_MAX_BYTES")
                .cloned()
                .or_else(|| std::env::var("BRO_HARNESS_PROJECT_DOC_MAX_BYTES").ok()),
        },
        "session_env": session_env,
        "process_env_visible_to_shell": process_env,
        "shell_path": shell_path,
        "tool_resolution": tool_resolution,
        "notes": [
            "session_env values are task-local daemon config; sensitive values are redacted and are not inherited by shell children",
            "process_env_visible_to_shell is the subset expected to be inherited by shell tools",
            "tool_resolution checks common operator/project commands against PATH as seen by this harness process; MCP workspace shell tools may have their own path augmentation",
        ],
    }))
}

fn git_status_manifest(root: &Path, status_limit: usize) -> Value {
    json!({
        "toplevel": git_capture(root, &["rev-parse", "--show-toplevel"]),
        "branch": git_capture(root, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "head": git_capture(root, &["rev-parse", "--short=12", "HEAD"]),
        "status": git_status_summary(root, status_limit),
    })
}

fn git_status_summary(root: &Path, status_limit: usize) -> Value {
    let Some(raw) = git_capture(root, &["status", "--short", "--branch"]) else {
        return Value::Null;
    };
    let mut lines = raw.lines();
    let branch = lines.next().unwrap_or_default().to_string();
    let entries: Vec<String> = lines.map(str::to_string).collect();
    let dirty_count = entries.len();
    let limit = status_limit.max(1);
    json!({
        "branch_line": branch,
        "dirty_count": dirty_count,
        "entries": entries.iter().take(limit).cloned().collect::<Vec<_>>(),
        "truncated": dirty_count > limit,
    })
}

// called from sandbox_status's call_blocking closure (wave 13).
#[allow(clippy::disallowed_methods)]
fn git_capture(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

fn visible_process_env() -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for key in [
        "PATH",
        "BRO_FLEET_BASE_REPO",
        "BRO_FLEET_PARENT_WORKTREE",
        "BRO_FLEET_WORKTREE_ROOT",
        "BRO_FLEET_WORKTREE_BRANCH",
        "BRO_HARNESS_PROJECT_DOC_FILES",
        "BRO_HARNESS_PROJECT_DOC_MAX_BYTES",
    ] {
        if let Ok(value) = std::env::var(key) {
            out.insert(key.to_string(), value);
        }
    }
    out
}

fn shell_path_manifest() -> Value {
    let raw = std::env::var_os("PATH").unwrap_or_default();
    let entries: Vec<String> = std::env::split_paths(&raw)
        .map(|path| path.display().to_string())
        .collect();
    let entry_count = entries.len();
    let limit = 24;
    json!({
        "entry_count": entry_count,
        "entries": entries.iter().take(limit).cloned().collect::<Vec<_>>(),
        "truncated": entry_count > limit,
    })
}

fn tool_resolution_manifest<const N: usize>(bins: [&str; N]) -> BTreeMap<String, Value> {
    bins.into_iter()
        .map(|bin| {
            (
                bin.to_string(),
                match resolve_bin_on_path(bin) {
                    Some(path) => json!({"available": true, "path": path.display().to_string()}),
                    None => json!({"available": false, "path": Value::Null}),
                },
            )
        })
        .collect()
}

fn resolve_bin_on_path(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn redact_env(env: &BTreeMap<String, String>) -> BTreeMap<String, Value> {
    env.iter()
        .map(|(key, value)| {
            let redacted = !is_public_session_env_key(key);
            (
                key.clone(),
                json!({
                    "present": true,
                    "value": if redacted { Value::String("<redacted>".to_string()) } else { Value::String(value.clone()) },
                    "redacted": redacted,
                    "visible_to_shell": std::env::var(key).is_ok_and(|process_value| process_value == *value),
                }),
            )
        })
        .collect()
}

fn is_public_session_env_key(key: &str) -> bool {
    [
        "BRO_HARNESS_CHAT_REASONING",
        "BRO_HARNESS_PROJECT_DOC_FILES",
        "BRO_HARNESS_PROJECT_DOC_MAX_BYTES",
        "BRO_HARNESS_PROVIDER",
        "BRO_HARNESS_TRANSPORT",
        "BRO_HOME",
    ]
    .contains(&key)
}

/// Resolve a caller-supplied path to a normalized absolute path. Relative
/// paths are joined against the effective worktree root (the launch root or,
/// after `exit_worktree(publish)` removed a managed fleet worktree, the base
/// repository advertised by the fleet env); absolute paths are returned as-is
/// after lexical normalization.
///
/// **There is no containment check.** The harness has no other sandboxing
/// machinery, and `shell` already escapes any lexical `cx.root` boundary with
/// `git -C`, absolute paths, `find`, `sed -i`, `tee`, etc. The pretense of
/// confining the structured file tools to `cx.root` was a speed bump on
/// `file_read`/`file_edit`/`file_write`/`code.*` that the agent routinely
/// bypassed via shell in two or three calls per file (gap-e0ae3e7d,
/// friction/2026-June-13-0840pm-worktree-containment-issues.md). Callers that
/// need a real containment boundary must layer one below the file tools
/// (process-level sandbox), not in them.
pub fn resolve_in_root(root: &Path, rel: &str) -> anyhow::Result<PathBuf> {
    let root = effective_root(root);
    let joined = if Path::new(rel).is_absolute() {
        PathBuf::from(rel)
    } else {
        root.join(rel)
    };
    Ok(normalize_lexical(&joined))
}

/// Resolve a read-only file path. Relative paths join the effective worktree
/// root; absolute paths are accepted as-is. Explicit `@...` file mentions get
/// Codex-style handling on top of that: `@relative/path` strips the marker
/// and resolves inside the root, while `@/abs/path.md` can read instruction
/// docs outside the root. Harness-owned dump files are also readable by
/// absolute path so oversized tool-result riders can point at a lossless
/// recovery path. This is intentionally not used by write/edit/shell tools
/// (those go through [`resolve_in_root`]).
fn resolve_read_path(root: &Path, raw: &str) -> anyhow::Result<PathBuf> {
    let Some(stripped) = raw.strip_prefix('@') else {
        let path = Path::new(raw);
        if path.is_absolute() {
            let normalized = normalize_lexical(path);
            if is_allowed_harness_dump(&normalized) {
                return Ok(normalized);
            }
        }
        return resolve_in_root(root, raw);
    };
    if stripped.is_empty() {
        anyhow::bail!("empty @ file mention");
    }
    let path = Path::new(stripped);
    if path.is_absolute() {
        let normalized = normalize_lexical(path);
        if is_allowed_external_instruction_doc(&normalized) || is_allowed_harness_dump(&normalized)
        {
            return Ok(normalized);
        }
        anyhow::bail!("external @ file mention is not an allowed instruction doc: {raw}");
    }
    resolve_in_root(root, stripped)
}

fn is_allowed_harness_dump(path: &Path) -> bool {
    harness_dump_roots()
        .into_iter()
        .any(|root| path.starts_with(root))
}

fn harness_dump_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(home) = std::env::var("BRO_HOME") {
        roots.push(normalize_lexical(
            &PathBuf::from(home).join("harness-dumps"),
        ));
    }
    roots.push(normalize_lexical(
        &std::env::temp_dir().join("bro-harness-dumps"),
    ));
    roots
}

fn is_allowed_external_instruction_doc(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if matches!(
        name,
        "AGENTS.md"
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

fn normalize_lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        use std::path::Component::*;
        match comp {
            ParentDir => {
                out.pop();
            }
            CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Resolve an optional base subdir (default root) and a per-walk file cap.
fn walk_base(root: &Path, rel: Option<&str>) -> anyhow::Result<PathBuf> {
    resolve_in_root(root, rel.unwrap_or("."))
}

/// Directory names that are pruned from every recursive workspace walk
/// (`glob`, `content_search`). These are build/dependency/VCS trees that an
/// agent never wants to grep and that can be enormous — a single Cargo
/// `target/` in this very repo is 30 GB / 65k files. Pruning them by name is
/// defense-in-depth that does not depend on any ignore file being present.
const PRUNE_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".direnv",
    ".venv",
    "venv",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    "dist",
    "build",
    ".next",
    ".turbo",
    ".gradle",
    ".cargo",
];

/// A recursive directory walker hardened against runaway traversals.
///
/// Two protections layered together:
///   1. `require_git(false)` — honor `.gitignore` even when the walk root is not
///      a *recognized* git repository. This is essential inside a linked git
///      **worktree**, whose `.git` is a file (a `gitdir:` pointer), not a
///      directory: `ignore`'s default git detection misses it, so without this
///      it silently skips `.gitignore` and descends straight into the gitignored
///      `target/`. (That is exactly how the in-process harness wedged the daemon
///      — every `glob`/`content_search` walked 30 GB of build output.)
///   2. `filter_entry` hard-prunes [`PRUNE_DIRS`] by name, so even a directory
///      with no `.gitignore` at all (or one that doesn't list `target/`) can't
///      trigger a catastrophic walk. Only directories are pruned; files that
///      happen to share a name are still visited.
fn hardened_walk(base: &Path) -> ignore::Walk {
    WalkBuilder::new(base)
        .require_git(false)
        .filter_entry(|e| {
            if e.file_type().is_some_and(|t| t.is_dir()) {
                let name = e.file_name().to_str().unwrap_or_default();
                !PRUNE_DIRS.contains(&name)
            } else {
                true
            }
        })
        .build()
}

/// Read a file as UTF-8, returning None for binary/oversized/unreadable.
// called from content_search's call_blocking closure (wave 13).
#[allow(clippy::disallowed_methods)]
fn read_text_capped(path: &Path, max_bytes: u64) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > max_bytes {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

// ---------------------------------------------------------------------------
// file_read
// ---------------------------------------------------------------------------

/// Default cap on lines returned by a single `file_read` when no explicit
/// `max_lines` is given. Keeps an unbounded read from flooding context.
const FILE_READ_DEFAULT_MAX_LINES: usize = 2000;

#[derive(Deserialize, JsonSchema)]
struct FileReadInput {
    /// Path to the file. Relative paths resolve against the worktree root;
    /// absolute paths are accepted as-is. `@relative/path` is accepted as a
    /// file mention; `@/absolute/instruction.md` is accepted for read-only
    /// instruction docs outside the worktree.
    file_path: String,
    /// 1-based start line (inclusive). Omit to read from the beginning.
    start_line: Option<usize>,
    /// 1-based end line (inclusive). Omit to read to EOF.
    end_line: Option<usize>,
    /// Cap on the number of lines returned (default 2000). The read stops once
    /// this many in-range lines have been collected; a truncation marker is
    /// appended so the caller knows there is more.
    max_lines: Option<usize>,
    /// When true, prefix each returned line with its 1-based file line number
    /// (`<n>\t<text>`, cat -n style), so line ranges in follow-up edits are
    /// unambiguous. Default false.
    #[serde(default)]
    line_numbers: bool,
}

pub struct FileRead;

#[async_trait]
impl Tool for FileRead {
    fn name(&self) -> &str {
        "file_read"
    }
    fn description(&self) -> &str {
        "Read a UTF-8 text file in the worktree. Supports explicit @file mentions: @relative/path resolves inside the worktree, and @/absolute/instruction.md can read instruction docs outside it. Absolute harness dump paths from oversized tool-result riders are also readable. Optionally restrict to a 1-based [start_line, end_line] range. Returns at most max_lines lines (default 2000); the read stops early at the range/cap rather than loading the whole file. Set line_numbers=true to prefix each line with its 1-based number."
    }
    fn input_schema(&self) -> Value {
        schema_for::<FileReadInput>()
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            ..Default::default()
        }
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        use tokio::io::AsyncBufReadExt;

        let args: FileReadInput = match serde_json::from_value(input) {
            Ok(a) => a,
            Err(e) => return ToolResult::Error(format!("bad input: {e}")),
        };
        let path = match resolve_read_path(&cx.root, &args.file_path) {
            Ok(p) => p,
            Err(e) => return ToolResult::Error(e.to_string()),
        };
        let file = match tokio::fs::File::open(&path).await {
            Ok(f) => f,
            Err(e) => return ToolResult::Error(format!("read {}: {e}", args.file_path)),
        };

        // 1-based inclusive [start, end]; line numbers are 1-based here.
        let start = args.start_line.unwrap_or(1).max(1);
        let end = args.end_line.unwrap_or(usize::MAX);
        if start > end {
            return ToolResult::Error(format!("start_line {start} is after end_line {end}"));
        }
        let max_lines = args.max_lines.unwrap_or(FILE_READ_DEFAULT_MAX_LINES);

        let mut reader = tokio::io::BufReader::new(file).lines();
        let mut collected: Vec<String> = Vec::new();
        let mut lineno = 0usize;
        let mut more_after_cap = false;
        loop {
            let line = match reader.next_line().await {
                Ok(Some(l)) => l,
                Ok(None) => break,
                Err(e) => return ToolResult::Error(format!("read {}: {e}", args.file_path)),
            };
            lineno += 1;
            if lineno < start {
                continue;
            }
            if lineno > end {
                break;
            }
            if collected.len() == max_lines {
                // There is at least one more in-range line we are not returning.
                more_after_cap = true;
                break;
            }
            if args.line_numbers {
                collected.push(format!("{lineno}\t{line}"));
            } else {
                collected.push(line);
            }
        }

        let mut out = collected.join("\n");
        if more_after_cap {
            let next = start + max_lines;
            out.push_str(&format!(
                "\n[truncated at max_lines={max_lines}; continue with start_line={next}]"
            ));
        }
        ToolResult::Text(out)
    }
}

// ---------------------------------------------------------------------------
// file_write
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
struct FileWriteInput {
    /// Path to write. Relative paths resolve against the worktree root;
    /// absolute paths are accepted as-is. Parent dirs are created.
    file_path: String,
    /// Full new contents of the file.
    #[serde(default)]
    content: Option<String>,
}

pub struct FileWrite;

#[async_trait]
impl Tool for FileWrite {
    fn name(&self) -> &str {
        "file_write"
    }
    fn description(&self) -> &str {
        "Create or overwrite a file in the worktree with the given contents."
    }
    fn input_schema(&self) -> Value {
        schema_for::<FileWriteInput>()
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            destructive: true,
            ..Default::default()
        }
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let args: FileWriteInput = match serde_json::from_value(input) {
            Ok(a) => a,
            Err(e) => return ToolResult::Error(format!("bad input: {e}")),
        };
        let content = match args.content {
            Some(c) => c,
            None => return ToolResult::Error("file_write needs content".into()),
        };
        let path = match resolve_in_root(&cx.root, &args.file_path) {
            Ok(p) => p,
            Err(e) => return ToolResult::Error(e.to_string()),
        };
        if let Some(parent) = path.parent()
            && let Err(e) = tokio::fs::create_dir_all(parent).await
        {
            return ToolResult::Error(format!("mkdir {}: {e}", parent.display()));
        }
        // Capture the pre-image BEFORE the write so the post-write edit event
        // carries both ends. A missing file (fresh write) → empty pre-image.
        let pre_image = tokio::fs::read(&path).await.unwrap_or_default();
        match tokio::fs::write(&path, content.as_bytes()).await {
            Ok(()) => {
                record_edit(cx, &path, &pre_image, content.as_bytes());
                ToolResult::Json(json!({"ok": true, "bytes": content.len()}))
            }
            Err(e) => ToolResult::Error(format!("write {}: {e}", args.file_path)),
        }
    }
}

/// Push an `EditEvent` onto `cx.edits` after a successful file mutation.
/// Lock-poisoning is swallowed (logged), since the diagnostics substrate must
/// never fail a successful filesystem mutation.
fn record_edit(cx: &ToolCx, path: &Path, pre: &[u8], post: &[u8]) {
    match cx.edits.lock() {
        Ok(mut sink) => sink.push(crate::edits::EditEvent::from_bytes(
            path.to_path_buf(),
            pre,
            post,
        )),
        Err(_) => tracing::warn!(
            "edits sink poisoned; dropping {} edit event",
            path.display()
        ),
    }
}

// ---------------------------------------------------------------------------
// list_dir
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
struct ListDirInput {
    /// Directory to list. Relative paths resolve against the worktree root;
    /// absolute paths are accepted as-is. Defaults to root.
    path: Option<String>,
}

pub struct ListDir;

#[async_trait]
impl Tool for ListDir {
    fn name(&self) -> &str {
        "list_dir"
    }
    fn description(&self) -> &str {
        "List the immediate entries of a directory in the worktree."
    }
    fn input_schema(&self) -> Value {
        schema_for::<ListDirInput>()
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            ..Default::default()
        }
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let args: ListDirInput =
            serde_json::from_value(input).unwrap_or(ListDirInput { path: None });
        let dir = match resolve_in_root(&cx.root, args.path.as_deref().unwrap_or(".")) {
            Ok(p) => p,
            Err(e) => return ToolResult::Error(e.to_string()),
        };
        let mut rd = match tokio::fs::read_dir(&dir).await {
            Ok(rd) => rd,
            Err(e) => return ToolResult::Error(format!("read_dir {}: {e}", dir.display())),
        };
        let mut entries = Vec::new();
        while let Ok(Some(ent)) = rd.next_entry().await {
            let is_dir = ent.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
            entries.push(json!({
                "name": ent.file_name().to_string_lossy(),
                "is_dir": is_dir,
            }));
        }
        ToolResult::Json(json!({ "entries": entries }))
    }
}

// ---------------------------------------------------------------------------
// git tools (read-only via shell)
// ---------------------------------------------------------------------------

async fn git(cx: &ToolCx, args: &[&str]) -> ToolResult {
    let root = effective_root(&cx.root);
    let out = tokio::process::Command::new("git")
        .args(args)
        .current_dir(&root)
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => {
            ToolResult::Text(String::from_utf8_lossy(&o.stdout).into_owned())
        }
        Ok(o) => ToolResult::Error(String::from_utf8_lossy(&o.stderr).into_owned()),
        Err(e) => ToolResult::Error(format!("git {args:?}: {e}")),
    }
}

macro_rules! read_git_tool {
    ($ty:ident, $name:literal, $desc:literal, $argv:expr) => {
        pub struct $ty;
        #[async_trait]
        impl Tool for $ty {
            fn name(&self) -> &str {
                $name
            }
            fn description(&self) -> &str {
                $desc
            }
            fn input_schema(&self) -> Value {
                json!({"type": "object", "properties": {}})
            }
            fn annotations(&self) -> ToolAnnotations {
                ToolAnnotations {
                    read_only: true,
                    ..Default::default()
                }
            }
            async fn call(&self, _input: Value, cx: &ToolCx) -> ToolResult {
                git(cx, $argv).await
            }
        }
    };
}

read_git_tool!(
    GitStatus,
    "git_status",
    "Show `git status --short`.",
    &["status", "--short"]
);
read_git_tool!(
    GitLog,
    "git_log",
    "Show recent commits (`git log --oneline -20`).",
    &["log", "--oneline", "-20"]
);

#[derive(Deserialize, JsonSchema)]
struct GitDiffInput {
    /// Include untracked files as new-file patches.
    include_untracked: Option<bool>,
}

pub struct GitDiff;

#[async_trait]
impl Tool for GitDiff {
    fn name(&self) -> &str {
        "git_diff"
    }
    fn description(&self) -> &str {
        "Show the unstaged working-tree diff. Set include_untracked=true to include untracked files as new-file patches."
    }
    fn input_schema(&self) -> Value {
        schema_for::<GitDiffInput>()
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            ..Default::default()
        }
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let args: GitDiffInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => return ToolResult::Error(e.to_string()),
        };
        if args.include_untracked.unwrap_or(false) {
            git_diff_include_untracked(cx).await
        } else {
            git(cx, &["diff"]).await
        }
    }
}

async fn git_diff_include_untracked(cx: &ToolCx) -> ToolResult {
    let root = effective_root(&cx.root);
    let mut diff = match git_stdout(&root, &["diff"]).await {
        Ok(diff) => diff,
        Err(e) => return ToolResult::Error(e),
    };
    let raw_untracked =
        match git_stdout(&root, &["ls-files", "--others", "--exclude-standard", "-z"]).await {
            Ok(raw) => raw,
            Err(e) => return ToolResult::Error(e),
        };
    for path in raw_untracked.split('\0').filter(|path| !path.is_empty()) {
        match git_no_index_new_file(&root, path).await {
            Ok(patch) if !patch.is_empty() => {
                if !diff.is_empty() && !diff.ends_with('\n') {
                    diff.push('\n');
                }
                diff.push_str(&patch);
            }
            Ok(_) => {}
            Err(e) => return ToolResult::Error(e),
        }
    }
    ToolResult::Text(diff)
}

async fn git_stdout(root: &Path, args: &[&str]) -> Result<String, String> {
    let out = tokio::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .await
        .map_err(|e| format!("git {args:?}: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).into_owned())
    }
}

async fn git_no_index_new_file(root: &Path, path: &str) -> Result<String, String> {
    let out = tokio::process::Command::new("git")
        .args(["diff", "--no-index", "--", "/dev/null", path])
        .current_dir(root)
        .output()
        .await
        .map_err(|e| format!("git diff --no-index {path}: {e}"))?;
    let code = out.status.code().unwrap_or(1);
    if code == 0 || code == 1 {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).into_owned())
    }
}

#[derive(Deserialize, JsonSchema)]
struct GitShowInput {
    /// Commit-ish to show (default HEAD).
    rev: Option<String>,
}

pub struct GitShow;

#[async_trait]
impl Tool for GitShow {
    fn name(&self) -> &str {
        "git_show"
    }
    fn description(&self) -> &str {
        "Show a commit (`git show <rev>`, default HEAD)."
    }
    fn input_schema(&self) -> Value {
        schema_for::<GitShowInput>()
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            ..Default::default()
        }
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let args: GitShowInput =
            serde_json::from_value(input).unwrap_or(GitShowInput { rev: None });
        let rev = args.rev.unwrap_or_else(|| "HEAD".into());
        git(cx, &["show", &rev]).await
    }
}

// ---------------------------------------------------------------------------
// git_commit (guarded)
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
struct GitCommitInput {
    /// Commit message.
    message: String,
    /// Explicit pathspecs to stage (relative to root). Required — we never
    /// `git add .`, matching the operator's data-safety rule.
    paths: Vec<String>,
}

pub struct GitCommit;

#[async_trait]
impl Tool for GitCommit {
    fn name(&self) -> &str {
        "git_commit"
    }
    fn description(&self) -> &str {
        "Stage the named paths and commit. Refuses to stage sensitive files (.env, *.pem, id_rsa, credentials, ...). Never stages with `.`; paths must be explicit."
    }
    fn input_schema(&self) -> Value {
        schema_for::<GitCommitInput>()
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            destructive: true,
            ..Default::default()
        }
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let args: GitCommitInput = match serde_json::from_value(input) {
            Ok(a) => a,
            Err(e) => return ToolResult::Error(format!("bad input: {e}")),
        };
        if args.paths.is_empty() {
            return ToolResult::Error("paths must be explicit; refusing to `git add .`".into());
        }
        for p in &args.paths {
            if cx.safety.is_sensitive_path(Path::new(p)) {
                return ToolResult::Error(format!("refused: {p} looks like a secret/credential"));
            }
        }
        let mut add_args = vec!["add", "--"];
        add_args.extend(args.paths.iter().map(String::as_str));
        let staged = git(cx, &add_args).await;
        if staged.is_error() {
            return staged;
        }
        git(cx, &["commit", "-m", &args.message]).await
    }
}

// ---------------------------------------------------------------------------
// file_edit
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
struct FileEditInput {
    /// Path to the file. Relative paths resolve against the worktree root;
    /// absolute paths are accepted as-is.
    file_path: String,
    /// Exact text to find. Must be unique in the file unless `replace_all`.
    old_string: String,
    /// Replacement text.
    new_string: String,
    /// Replace every occurrence instead of requiring a unique match.
    #[serde(default)]
    replace_all: bool,
}

pub struct FileEdit;

#[async_trait]
impl Tool for FileEdit {
    fn name(&self) -> &str {
        "file_edit"
    }
    fn description(&self) -> &str {
        "Replace an exact string in a file. Fails if old_string is absent, or present more than once when replace_all is false."
    }
    fn input_schema(&self) -> Value {
        schema_for::<FileEditInput>()
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            destructive: true,
            ..Default::default()
        }
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let args: FileEditInput = match serde_json::from_value(input) {
            Ok(a) => a,
            Err(e) => return ToolResult::Error(format!("bad input: {e}")),
        };
        if args.old_string == args.new_string {
            return ToolResult::Error("old_string and new_string are identical".into());
        }
        let path = match resolve_in_root(&cx.root, &args.file_path) {
            Ok(p) => p,
            Err(e) => return ToolResult::Error(e.to_string()),
        };
        let body = match tokio::fs::read_to_string(&path).await {
            Ok(b) => b,
            Err(e) => return ToolResult::Error(format!("read {}: {e}", args.file_path)),
        };
        let count = body.matches(&args.old_string).count();
        if count == 0 {
            return ToolResult::Error("old_string not found".into());
        }
        if count > 1 && !args.replace_all {
            return ToolResult::Error(format!(
                "old_string occurs {count} times; pass replace_all or add context to make it unique"
            ));
        }
        let updated = if args.replace_all {
            body.replace(&args.old_string, &args.new_string)
        } else {
            body.replacen(&args.old_string, &args.new_string, 1)
        };
        match tokio::fs::write(&path, updated.as_bytes()).await {
            Ok(()) => {
                record_edit(cx, &path, body.as_bytes(), updated.as_bytes());
                ToolResult::Json(
                    json!({"ok": true, "replacements": count.min(if args.replace_all { count } else { 1 })}),
                )
            }
            Err(e) => ToolResult::Error(format!("write {}: {e}", args.file_path)),
        }
    }
}

// ---------------------------------------------------------------------------
// content_search (ripgrep-style; gitignore-aware)
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema, Default, Clone, Copy, PartialEq)]
#[serde(rename_all = "snake_case")]
enum SearchMode {
    /// `relpath:line:text` for every matching line (default).
    #[default]
    Content,
    /// One line per file that contains at least one match.
    Files,
    /// `relpath:count` per matching file, plus a total.
    Count,
}

#[derive(Deserialize, JsonSchema)]
struct ContentSearchInput {
    /// Regular expression to search for.
    pattern: String,
    /// Subdirectory to search, relative to root (default: whole worktree).
    path: Option<String>,
    /// Optional glob to restrict files by name (e.g. "*.rs").
    glob: Option<String>,
    /// Max results to return (default 80). Counts matching lines in `content`
    /// mode, matching files in `files`/`count` mode.
    max_results: Option<usize>,
    /// Output shape: `content` (relpath:line:text), `files` (matching paths),
    /// or `count` (per-file match counts). Defaults to `content`.
    #[serde(default)]
    mode: SearchMode,
    /// In `content` mode, also emit this many context lines before and after
    /// each match (like ripgrep `-C`). Ignored in other modes. Default 0.
    context_lines: Option<usize>,
    /// Case-insensitive matching (like ripgrep `-i`). Default false.
    #[serde(default)]
    case_insensitive: bool,
}

pub struct ContentSearch;

const CONTENT_SEARCH_DEFAULT_MAX_RESULTS: usize = 80;
const CONTENT_SEARCH_HARD_MAX_RESULTS: usize = 5000;
const CONTENT_SEARCH_OUTPUT_BYTE_CAP: usize = 24_000;

fn push_search_line(hits: &mut Vec<String>, output_bytes: &mut usize, line: String) -> bool {
    let next = *output_bytes + line.len() + usize::from(!hits.is_empty());
    if next > CONTENT_SEARCH_OUTPUT_BYTE_CAP {
        hits.push(format!(
            "[truncated near {CONTENT_SEARCH_OUTPUT_BYTE_CAP} bytes of output]"
        ));
        return false;
    }
    *output_bytes = next;
    hits.push(line);
    true
}

fn content_search_refinement_hint(args: &ContentSearchInput, cap: usize) -> String {
    let path_hint = args.path.as_deref().unwrap_or("<subdir>");
    let glob_hint = args.glob.as_deref().unwrap_or("*.rs");
    format!(
        "[refine: narrow path=\"{path_hint}\" and glob=\"{glob_hint}\", use mode=\"files\" or mode=\"count\" to size the hit set first, lower max_results for a compact sample, or raise max_results up to {CONTENT_SEARCH_HARD_MAX_RESULTS} for a deliberate exhaustive search; current result cap {cap}, byte cap {CONTENT_SEARCH_OUTPUT_BYTE_CAP}]"
    )
}

#[async_trait]
impl Tool for ContentSearch {
    fn name(&self) -> &str {
        "content_search"
    }
    fn description(&self) -> &str {
        "Search file contents by regex across the worktree (respects .gitignore). Returns compact relpath:line:text results by default, capped with explicit truncation/refinement hints. Optionally restrict by subdir and filename glob; set mode (content|files|count), context_lines, case_insensitive, and max_results."
    }
    fn input_schema(&self) -> Value {
        schema_for::<ContentSearchInput>()
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            ..Default::default()
        }
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        // Sync tree walk + capped reads over the whole worktree — the single
        // heaviest builtin; keep it off the runtime workers.
        let cx = cx.clone();
        crate::tool::call_blocking(move || {
            let args: ContentSearchInput = match serde_json::from_value(input) {
                Ok(a) => a,
                Err(e) => return ToolResult::Error(format!("bad input: {e}")),
            };
            let re = match regex::RegexBuilder::new(&args.pattern)
                .case_insensitive(args.case_insensitive)
                .build()
            {
                Ok(r) => r,
                Err(e) => return ToolResult::Error(format!("bad regex: {e}")),
            };
            let name_matcher = match args.glob.as_deref() {
                Some(g) => match GlobPattern::new(g) {
                    Ok(gp) => Some(gp.compile_matcher()),
                    Err(e) => return ToolResult::Error(format!("bad glob: {e}")),
                },
                None => None,
            };
            let base = match walk_base(&cx.root, args.path.as_deref()) {
                Ok(p) => p,
                Err(e) => return ToolResult::Error(e.to_string()),
            };
            let root = effective_root(&cx.root);
            let cap = args
                .max_results
                .unwrap_or(CONTENT_SEARCH_DEFAULT_MAX_RESULTS)
                .min(CONTENT_SEARCH_HARD_MAX_RESULTS);
            let ctx = args.context_lines.unwrap_or(0).min(50);

            let mut hits: Vec<String> = Vec::new();
            let mut output_bytes = 0usize;
            let mut truncated = false;
            // `files`/`count` modes count files; `content` counts lines.
            let mut total_matches = 0usize;
            let mut matched_files = 0usize;
            'walk: for entry in hardened_walk(&base).flatten() {
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    continue;
                }
                let p = entry.path();
                if let Some(m) = &name_matcher {
                    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    if !m.is_match(name) {
                        continue;
                    }
                }
                let Some(text) = read_text_capped(p, 2_000_000) else {
                    continue;
                };
                let rel = p.strip_prefix(&root).unwrap_or(p).display().to_string();
                let lines: Vec<&str> = text.lines().collect();

                match args.mode {
                    SearchMode::Files => {
                        if lines.iter().any(|l| re.is_match(l)) {
                            if !push_search_line(&mut hits, &mut output_bytes, rel) {
                                truncated = true;
                                break 'walk;
                            }
                            if hits.len() >= cap {
                                hits.push(format!("[truncated at {cap} files]"));
                                truncated = true;
                                break 'walk;
                            }
                        }
                    }
                    SearchMode::Count => {
                        let n = lines.iter().filter(|l| re.is_match(l)).count();
                        if n > 0 {
                            total_matches += n;
                            matched_files += 1;
                            if !push_search_line(&mut hits, &mut output_bytes, format!("{rel}:{n}"))
                            {
                                truncated = true;
                                break 'walk;
                            }
                            if hits.len() >= cap {
                                hits.push(format!("[truncated at {cap} files]"));
                                truncated = true;
                                break 'walk;
                            }
                        }
                    }
                    SearchMode::Content => {
                        for (i, line) in lines.iter().enumerate() {
                            if re.is_match(line) {
                                if ctx > 0 {
                                    let lo = i.saturating_sub(ctx);
                                    let hi = (i + ctx).min(lines.len().saturating_sub(1));
                                    for (j, ctx_line) in lines[lo..=hi].iter().enumerate() {
                                        let n = lo + j + 1;
                                        let sep = if lo + j == i { ':' } else { '-' };
                                        if !push_search_line(
                                            &mut hits,
                                            &mut output_bytes,
                                            format!("{rel}:{n}{sep}{ctx_line}"),
                                        ) {
                                            truncated = true;
                                            break 'walk;
                                        }
                                    }
                                    if !push_search_line(&mut hits, &mut output_bytes, "--".into())
                                    {
                                        truncated = true;
                                        break 'walk;
                                    }
                                } else {
                                    if !push_search_line(
                                        &mut hits,
                                        &mut output_bytes,
                                        format!("{rel}:{}:{}", i + 1, line),
                                    ) {
                                        truncated = true;
                                        break 'walk;
                                    }
                                }
                                total_matches += 1;
                                if total_matches >= cap {
                                    hits.push(format!("[truncated at {cap} matches]"));
                                    truncated = true;
                                    break 'walk;
                                }
                            }
                        }
                    }
                }
            }

            if hits.is_empty() {
                return ToolResult::Text("no matches".into());
            }
            if args.mode == SearchMode::Count {
                hits.push(format!(
                    "[total: {total_matches} matches across {matched_files} files]"
                ));
            }
            if truncated {
                hits.push(content_search_refinement_hint(&args, cap));
            }
            let output = hits.join("\n");
            ToolResult::Text(output)
        })
        .await
    }
}

// ---------------------------------------------------------------------------
// glob
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema, Default, Clone, Copy, PartialEq)]
#[serde(rename_all = "snake_case")]
enum GlobSort {
    /// Most-recently-modified first (default) — matches "recently edited" use.
    #[default]
    Mtime,
    /// Lexicographic path order.
    Name,
}

#[derive(Deserialize, JsonSchema)]
struct GlobInput {
    /// Glob pattern relative to the base dir (e.g. "**/*.rs", "src/*.toml").
    pattern: String,
    /// Base dir relative to root (default root).
    path: Option<String>,
    /// Result ordering: `mtime` (most recent first, default) or `name`.
    #[serde(default)]
    sort: GlobSort,
    /// Max paths to return after sorting (default 1000). A truncation marker is
    /// appended when more matched, so a capped result is never silent.
    max_results: Option<usize>,
}

/// Hard ceiling on matches collected before sort/cap — a memory backstop
/// independent of the user-facing `max_results`.
const GLOB_SCAN_CEILING: usize = 20_000;

/// Compile a glob for matching paths **relative to the walk base**.
///
/// Two deliberate choices, learned from a live failure where `glob {pattern:
/// "**/*fleet*"}` returned the entire tree: the matcher was tested against the
/// ABSOLUTE path, and the worktree prefix (`.../bro/fleet/worktrees/...`) itself
/// contained "fleet", so every file matched.
///   - `literal_separator(true)` so `*` does not cross `/` — standard glob
///     semantics (cf. codex `protocol/src/permissions.rs::build_glob_matcher`,
///     Apache-2.0); use `**` to span directories.
///   - the caller matches ONLY the base-relative path (never the absolute
///     path), so a base-dir prefix can never leak into the match.
fn relpath_glob(pattern: &str) -> Result<globset::GlobMatcher, globset::Error> {
    Ok(GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()?
        .compile_matcher())
}

pub struct Glob;

#[async_trait]
impl Tool for Glob {
    fn name(&self) -> &str {
        "glob"
    }
    fn description(&self) -> &str {
        "Find files matching a glob pattern under the worktree (respects .gitignore). Returns relative paths as ONE newline-delimited STRING (not an array — in code-mode cells use `result.split(\"\\n\")`), capped at max_results (default 1000) with a truncation marker. NOTE: results are sorted by modification time (newest first) by DEFAULT; pass sort=\"name\" for lexicographic order."
    }
    fn input_schema(&self) -> Value {
        schema_for::<GlobInput>()
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            ..Default::default()
        }
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        // Sync tree walk with per-entry stat (mtime sort) — keep it off the
        // runtime workers.
        let cx = cx.clone();
        crate::tool::call_blocking(move || {
            let args: GlobInput = match serde_json::from_value(input) {
                Ok(a) => a,
                Err(e) => return ToolResult::Error(format!("bad input: {e}")),
            };
            let matcher = match relpath_glob(&args.pattern) {
                Ok(m) => m,
                Err(e) => return ToolResult::Error(format!("bad glob: {e}")),
            };
            // A slash-less pattern (e.g. "*.rs", "Cargo.toml") is also matched
            // against the file NAME at any depth — ripgrep/fd `-g` ergonomics — so
            // an agent doesn't have to write "**/" for the common "find files of
            // this kind anywhere" case. Patterns with a separator stay full-path.
            let match_basename = !args.pattern.contains('/');
            let base = match walk_base(&cx.root, args.path.as_deref()) {
                Ok(p) => p,
                Err(e) => return ToolResult::Error(e.to_string()),
            };
            let root = effective_root(&cx.root);
            // Collect (relpath, mtime) so we can order before formatting.
            let mut out: Vec<(String, std::time::SystemTime)> = Vec::new();
            for entry in hardened_walk(&base).flatten() {
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    continue;
                }
                let p = entry.path();
                let rel = p.strip_prefix(&base).unwrap_or(p);
                let name_hit = match_basename
                    && p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| matcher.is_match(n));
                if matcher.is_match(rel) || name_hit {
                    let mtime = entry
                        .metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .unwrap_or(std::time::UNIX_EPOCH);
                    out.push((
                        p.strip_prefix(&root).unwrap_or(p).display().to_string(),
                        mtime,
                    ));
                    if out.len() >= GLOB_SCAN_CEILING {
                        break;
                    }
                }
            }
            match args.sort {
                // Most recent first; tie-break by path for determinism.
                GlobSort::Mtime => out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0))),
                GlobSort::Name => out.sort_by(|a, b| a.0.cmp(&b.0)),
            }
            if out.is_empty() {
                return ToolResult::Text("no files matched".into());
            }
            // Cap AFTER sorting, so the returned slice is the true top-N by the
            // chosen order (not an arbitrary walk-order prefix).
            let cap = args.max_results.unwrap_or(1000);
            let total = out.len();
            let mut lines: Vec<String> = out.into_iter().take(cap).map(|(rel, _)| rel).collect();
            if total > lines.len() {
                lines.push(format!(
                    "[showing {} of {total} matches; raise max_results for more]",
                    lines.len()
                ));
            }
            ToolResult::Text(lines.join("\n"))
        })
        .await
    }
}

// ---------------------------------------------------------------------------
// smart_read
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
struct SmartReadInput {
    /// Path to the file. Relative paths resolve against the worktree root;
    /// absolute paths are accepted as-is. `@relative/path` is accepted as a
    /// file mention; `@/absolute/instruction.md` is accepted for read-only
    /// instruction docs outside the worktree.
    file_path: String,
    /// Line count above which the file is outlined instead of returned whole
    /// (default 400).
    max_full_lines: Option<usize>,
}

pub struct SmartRead;

#[async_trait]
impl Tool for SmartRead {
    fn name(&self) -> &str {
        "smart_read"
    }
    fn description(&self) -> &str {
        "Read a file; small files are returned whole, large files are summarized as a definition outline (with line numbers) plus a head sample, so you can then file_read specific ranges. Supports @file mention syntax and absolute harness dump paths with the same read-only external carveouts as file_read."
    }
    fn input_schema(&self) -> Value {
        schema_for::<SmartReadInput>()
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            ..Default::default()
        }
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let args: SmartReadInput = match serde_json::from_value(input) {
            Ok(a) => a,
            Err(e) => return ToolResult::Error(format!("bad input: {e}")),
        };
        let path = match resolve_read_path(&cx.root, &args.file_path) {
            Ok(p) => p,
            Err(e) => return ToolResult::Error(e.to_string()),
        };
        let body = match tokio::fs::read_to_string(&path).await {
            Ok(b) => b,
            Err(e) => return ToolResult::Error(format!("read {}: {e}", args.file_path)),
        };
        let lines: Vec<&str> = body.lines().collect();
        let threshold = args.max_full_lines.unwrap_or(400);
        if lines.len() <= threshold {
            return ToolResult::Text(body);
        }
        // Outline: lines that look like definitions/headers.
        let def = definition_regex();
        let mut outline: Vec<String> = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            if def.is_match(line) {
                outline.push(format!("{}: {}", i + 1, line.trim_end()));
            }
        }
        let head: String = lines
            .iter()
            .take(40)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        let summary = format!(
            "[smart_read: {} lines — outlined (use file_read with start_line/end_line for detail)]\n\n\
             === head (lines 1-40) ===\n{head}\n\n\
             === outline ({} definitions) ===\n{}",
            lines.len(),
            outline.len(),
            if outline.is_empty() {
                "(no recognizable definitions)".to_string()
            } else {
                outline.join("\n")
            }
        );
        ToolResult::Text(summary)
    }
}

fn definition_regex() -> Regex {
    static SRC: &str = r"^\s*(pub\s+)?(async\s+)?(fn|struct|enum|trait|impl|mod|const|static|type|class|def|function|interface|export|public|private|protected|func|package)\b|^\s*#\[|^#{1,6}\s";
    use std::sync::OnceLock;
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(SRC).expect("valid definition regex"))
        .clone()
}

// ---------------------------------------------------------------------------
// apply_patch
// ---------------------------------------------------------------------------

/// The codex `*** Begin Patch` editor, exposed as a freeform/grammar-constrained
/// tool. Only meaningful on transports that honor the lark grammar (Responses);
/// the harness drops it elsewhere via the grammar-transport rule, so it never
/// degrades to an unconstrained JSON-string editor competing with `file_edit`.
pub struct ApplyPatch;

#[async_trait]
impl Tool for ApplyPatch {
    fn name(&self) -> &str {
        "apply_patch"
    }
    fn description(&self) -> &str {
        "Edit files with a `*** Begin Patch` / `*** End Patch` envelope of \
         `*** Add File:` / `*** Update File:` / `*** Delete File:` (and optional \
         `*** Move to:`) hunks; update lines are prefixed ' ' (context), '+' \
         (add), or '-' (remove). This is a FREEFORM tool — emit the patch text \
         directly, do not wrap it in JSON. Paths are relative to the worktree \
         root; absolute paths are accepted as-is."
    }
    fn input_schema(&self) -> Value {
        // JSON-function fallback shape. The freeform/grammar channel delivers
        // the patch as raw text (mapped to `source`); the fallback accepts it
        // under `source` too. The tool is only registered on grammar-capable
        // transports, so the fallback is effectively unused.
        json!({
            "type": "object",
            "properties": {
                "source": {
                    "type": "string",
                    "description": "The full apply_patch envelope text."
                }
            },
            "required": ["source"]
        })
    }
    fn freeform_grammar(&self) -> Option<FreeformGrammar> {
        Some(FreeformGrammar {
            syntax: "lark".to_string(),
            definition: bro_apply_patch::APPLY_PATCH_LARK_GRAMMAR.to_string(),
        })
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: false,
            destructive: true,
        }
    }
    // The fs reads below run inside the call_blocking closure; clippy's
    // disallowed_methods is syntactic and cannot see the blocking context.
    #[allow(clippy::disallowed_methods)]
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        // Sync multi-file pre-image reads + patch application — keep off the
        // runtime workers (round-2 invariant audit, thread-935b467d).
        let cx = cx.clone();
        crate::tool::call_blocking(move || {
            // The custom_tool_call freeform channel maps raw text to `source`; also
            // accept `patch`/`input` for the JSON-function fallback.
            let patch_text = input
                .get("source")
                .or_else(|| input.get("patch"))
                .or_else(|| input.get("input"))
                .and_then(|v| v.as_str());
            let patch_text = match patch_text {
                Some(s) if !s.trim().is_empty() => s,
                _ => {
                    return ToolResult::Error(
                        "apply_patch expects the patch envelope text (as `source`)".to_string(),
                    );
                }
            };

            // Snapshot pre-images of every source path the patch touches, so applied
            // changes feed the edit-diagnostics sink the same way file_edit does.
            let parsed = match bro_apply_patch::parse_patch(patch_text) {
                Ok(p) => p,
                Err(e) => return ToolResult::Error(format!("apply_patch parse error: {e}")),
            };
            let mut pre_images: std::collections::HashMap<PathBuf, Vec<u8>> =
                std::collections::HashMap::new();
            for hunk in &parsed.hunks {
                let src = match hunk {
                    bro_apply_patch::Hunk::AddFile { path, .. } => path,
                    bro_apply_patch::Hunk::DeleteFile { path } => path,
                    bro_apply_patch::Hunk::UpdateFile { path, .. } => path,
                };
                let abs = cx.root.join(src);
                pre_images.insert(src.clone(), std::fs::read(&abs).unwrap_or_default());
            }

            let outcome = match bro_apply_patch::apply_patch(patch_text, &cx.root) {
                Ok(o) => o,
                Err(e) => return ToolResult::Error(format!("apply_patch failed: {e}")),
            };

            use bro_apply_patch::FileAction;
            let mut summary = Vec::with_capacity(outcome.changes.len());
            for ch in &outcome.changes {
                let abs = cx.root.join(&ch.path);
                match ch.action {
                    FileAction::Added | FileAction::Updated => {
                        let pre = pre_images.get(&ch.path).cloned().unwrap_or_default();
                        let post = std::fs::read(&abs).unwrap_or_default();
                        record_edit(&cx, &abs, &pre, &post);
                    }
                    FileAction::Deleted => {
                        let pre = pre_images.get(&ch.path).cloned().unwrap_or_default();
                        record_edit(&cx, &abs, &pre, &[]);
                    }
                    FileAction::Moved => {
                        // Old path removed, new path created.
                        if let Some(from) = &ch.moved_from {
                            let from_abs = cx.root.join(from);
                            let pre = pre_images.get(from).cloned().unwrap_or_default();
                            record_edit(&cx, &from_abs, &pre, &[]);
                            let post = std::fs::read(&abs).unwrap_or_default();
                            record_edit(&cx, &abs, &[], &post);
                        }
                    }
                }
                let verb = match ch.action {
                    FileAction::Added => "added",
                    FileAction::Updated => "updated",
                    FileAction::Deleted => "deleted",
                    FileAction::Moved => "moved",
                };
                match (&ch.moved_from, ch.action) {
                    (Some(from), FileAction::Moved) => summary.push(format!(
                        "{verb} {} -> {}",
                        from.display(),
                        ch.path.display()
                    )),
                    _ => summary.push(format!("{verb} {}", ch.path.display())),
                }
            }

            ToolResult::Text(format!(
                "Applied patch ({} change{}):\n{}",
                summary.len(),
                if summary.len() == 1 { "" } else { "s" },
                summary.join("\n")
            ))
        })
        .await
    }
}

#[cfg(test)]
// These synchronous filesystem fixtures build and inspect isolated tempdir
// workspaces directly; no application Tokio worker executes them.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_relative_and_absolute_paths() {
        let root = PathBuf::from("/work/repo");
        // Relative paths join against the root, with `..` components
        // collapsed lexically — no containment check, no rejection. The pop
        // walks past the root on the way out (matches the existing
        // `normalize_lexical` semantics), so `../etc/passwd` lands one level
        // up, not at the filesystem root.
        assert_eq!(
            resolve_in_root(&root, "src/main.rs").unwrap(),
            PathBuf::from("/work/repo/src/main.rs")
        );
        assert_eq!(
            resolve_in_root(&root, "src/../other/x.rs").unwrap(),
            PathBuf::from("/work/repo/other/x.rs")
        );
        assert_eq!(
            resolve_in_root(&root, "../etc/passwd").unwrap(),
            PathBuf::from("/work/etc/passwd")
        );
        // Absolute paths are returned normalized, including ones that don't
        // live under the worktree root.
        assert_eq!(
            resolve_in_root(&root, "/work/repo/src/x").unwrap(),
            PathBuf::from("/work/repo/src/x")
        );
        assert_eq!(
            resolve_in_root(&root, "/etc/passwd").unwrap(),
            PathBuf::from("/etc/passwd")
        );
    }

    #[test]
    fn deleted_managed_worktree_root_transitions_to_base_repo() {
        let base_dir = tempfile::tempdir().unwrap();
        let worktree_root = tempfile::tempdir().unwrap();
        let base = base_dir.path().canonicalize().unwrap();
        let fleet_root = worktree_root.path().canonicalize().unwrap();
        let removed = fleet_root.join("repo").join("task-123");

        assert_eq!(
            effective_root_with_env(&removed, Some(&base), Some(&fleet_root)),
            base
        );
    }

    #[test]
    fn deleted_unmanaged_root_does_not_transition_to_base_repo() {
        let base_dir = tempfile::tempdir().unwrap();
        let worktree_root = tempfile::tempdir().unwrap();
        let removed = tempfile::tempdir().unwrap().path().join("task-123");
        let base = base_dir.path().canonicalize().unwrap();
        let fleet_root = worktree_root.path().canonicalize().unwrap();

        assert_eq!(
            effective_root_with_env(&removed, Some(&base), Some(&fleet_root)),
            normalize_lexical(&removed)
        );
    }

    #[test]
    fn read_paths_accept_at_instruction_docs() {
        let root_dir = tempfile::tempdir().unwrap();
        let root = root_dir.path().canonicalize().unwrap();
        let project_doc = root.join("PROJECT.md");
        std::fs::write(&project_doc, "project instructions\n").unwrap();

        assert_eq!(
            resolve_read_path(&root, "@PROJECT.md").unwrap(),
            project_doc
        );

        let external_dir = tempfile::tempdir().unwrap();
        let external_root = external_dir.path().canonicalize().unwrap();
        let blackbox_doc = external_root.join("BLACKBOX.md");
        std::fs::write(&blackbox_doc, "global instructions\n").unwrap();

        assert_eq!(
            resolve_read_path(&root, &format!("@{}", blackbox_doc.display())).unwrap(),
            blackbox_doc
        );
        assert!(resolve_read_path(&root, "@/etc/passwd").is_err());
        assert!(resolve_read_path(&root, "@").is_err());
    }

    fn cx_at(root: &Path) -> ToolCx {
        ToolCx {
            invocation_id: None,
            root: root.to_path_buf(),
            safety: std::sync::Arc::new(crate::safety::SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: std::sync::Arc::new(std::sync::Mutex::new(crate::todo::TodoList::default())),
            shell_sessions: std::sync::Arc::new(std::sync::Mutex::new(
                crate::shell::ShellSessions::default(),
            )),
            edits: std::sync::Arc::new(std::sync::Mutex::new(crate::edits::EditSink::default())),
            session_env: std::sync::Arc::new(std::collections::BTreeMap::new()),
            tool_arg_defaults: std::sync::Arc::new(crate::tool_defaults::ToolArgDefaults::default()),
            shell_env: std::sync::Arc::new(Default::default()),
        }
    }

    #[tokio::test]
    async fn sandbox_status_reports_project_docs_and_redacts_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let mut cx = cx_at(dir.path());
        cx.session_env = std::sync::Arc::new(BTreeMap::from([
            (
                "BRO_HARNESS_PROJECT_DOC_FILES".to_string(),
                "AGENTS_BETA.md".to_string(),
            ),
            (
                "ANTHROPIC_AUTH_TOKEN".to_string(),
                "secret-token".to_string(),
            ),
            ("CUSTOM_BEARER".to_string(), "opaque-secret".to_string()),
        ]));

        let out = SandboxStatus
            .call(
                json!({"root": dir.path().display().to_string(), "status_limit": 2}),
                &cx,
            )
            .await;
        let ToolResult::Json(v) = out else {
            panic!("expected json");
        };
        assert_eq!(v["root_source"], "explicit");
        assert_eq!(v["inspected_root"], dir.path().display().to_string());
        assert_eq!(v["project_docs"]["selected"], "AGENTS_BETA.md");
        assert_eq!(
            v["session_env"]["ANTHROPIC_AUTH_TOKEN"]["value"],
            "<redacted>"
        );
        assert_eq!(
            v["session_env"]["BRO_HARNESS_PROJECT_DOC_FILES"]["value"],
            "AGENTS_BETA.md"
        );
        assert_eq!(v["session_env"]["CUSTOM_BEARER"]["value"], "<redacted>");
    }

    #[tokio::test]
    async fn file_edit_requires_unique_match_then_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.txt");
        std::fs::write(&f, "x = 1\nx = 1\ny = 2\n").unwrap();
        let cx = cx_at(dir.path());

        // non-unique without replace_all → error
        let r = FileEdit
            .call(
                json!({"file_path":"a.txt","old_string":"x = 1","new_string":"x = 9"}),
                &cx,
            )
            .await;
        assert!(r.is_error(), "non-unique edit should fail: {r:?}");

        // unique edit succeeds
        let r = FileEdit
            .call(
                json!({"file_path":"a.txt","old_string":"y = 2","new_string":"y = 5"}),
                &cx,
            )
            .await;
        assert!(!r.is_error(), "unique edit should succeed: {r:?}");
        assert!(std::fs::read_to_string(&f).unwrap().contains("y = 5"));

        // replace_all rewrites both
        let r = FileEdit
            .call(json!({"file_path":"a.txt","old_string":"x = 1","new_string":"x = 0","replace_all":true}), &cx)
            .await;
        assert!(!r.is_error(), "replace_all should succeed: {r:?}");
        let body = std::fs::read_to_string(&f).unwrap();
        assert_eq!(body.matches("x = 0").count(), 2);
    }

    #[tokio::test]
    async fn file_read_range_and_max_lines_cap() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("big.txt");
        let body: String = (1..=10).map(|n| format!("line{n}\n")).collect();
        std::fs::write(&f, body).unwrap();
        let cx = cx_at(dir.path());

        // explicit range
        let r = FileRead
            .call(
                json!({"file_path":"big.txt","start_line":3,"end_line":5}),
                &cx,
            )
            .await;
        match r {
            ToolResult::Text(t) => assert_eq!(t, "line3\nline4\nline5"),
            other => panic!("expected text, got {other:?}"),
        }

        // max_lines cap emits a truncation marker with a resumable start_line
        let r = FileRead
            .call(json!({"file_path":"big.txt","max_lines":4}), &cx)
            .await;
        match r {
            ToolResult::Text(t) => {
                assert!(t.starts_with("line1\nline2\nline3\nline4"), "got: {t}");
                assert!(t.contains("max_lines=4"), "got: {t}");
                assert!(t.contains("start_line=5"), "got: {t}");
            }
            other => panic!("expected text, got {other:?}"),
        }

        // line_numbers prefixes the true 1-based file line number, even mid-range
        let r = FileRead
            .call(
                json!({"file_path":"big.txt","start_line":3,"end_line":4,"line_numbers":true}),
                &cx,
            )
            .await;
        match r {
            ToolResult::Text(t) => assert_eq!(t, "3\tline3\n4\tline4"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn file_read_allows_external_at_instruction_doc() {
        let root_dir = tempfile::tempdir().unwrap();
        let root = root_dir.path().canonicalize().unwrap();
        let external_dir = tempfile::tempdir().unwrap();
        let external_root = external_dir.path().canonicalize().unwrap();
        let doc = external_root.join("BLACKBOX.md");
        std::fs::write(&doc, "global blackbox instructions\n").unwrap();
        let cx = cx_at(&root);

        let r = FileRead
            .call(json!({"file_path": format!("@{}", doc.display())}), &cx)
            .await;
        match r {
            ToolResult::Text(t) => assert_eq!(t, "global blackbox instructions"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn file_read_allows_harness_dump_absolute_path() {
        let root_dir = tempfile::tempdir().unwrap();
        let root = root_dir.path().canonicalize().unwrap();
        let dump_root = std::env::temp_dir()
            .join("bro-harness-dumps")
            .join(format!("workspace-read-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dump_root);
        let dump = dump_root.join("tool-tu_1.txt");
        std::fs::create_dir_all(dump.parent().unwrap()).unwrap();
        std::fs::write(&dump, "full spilled payload\n").unwrap();
        let cx = cx_at(&root);

        let r = FileRead
            .call(json!({"file_path": dump.display().to_string()}), &cx)
            .await;
        match r {
            ToolResult::Text(t) => assert_eq!(t, "full spilled payload"),
            other => panic!("expected text, got {other:?}"),
        }

        let r = FileRead
            .call(
                json!({"file_path": root.parent().unwrap().join("x.txt").display().to_string()}),
                &cx,
            )
            .await;
        assert!(
            r.is_error(),
            "absolute non-dump path should remain confined: {r:?}"
        );
        let _ = std::fs::remove_dir_all(&dump_root);
    }

    #[tokio::test]
    async fn file_edit_still_rejects_external_at_instruction_doc() {
        let root_dir = tempfile::tempdir().unwrap();
        let root = root_dir.path().canonicalize().unwrap();
        let external_dir = tempfile::tempdir().unwrap();
        let external_root = external_dir.path().canonicalize().unwrap();
        let doc = external_root.join("BLACKBOX.md");
        std::fs::write(&doc, "global blackbox instructions\n").unwrap();
        let cx = cx_at(&root);

        let r = FileEdit
            .call(
                json!({
                    "file_path": format!("@{}", doc.display()),
                    "old_string": "global",
                    "new_string": "changed"
                }),
                &cx,
            )
            .await;
        assert!(r.is_error(), "file_edit should remain confined: {r:?}");
    }

    #[tokio::test]
    async fn content_search_modes_and_context() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.rs"),
            "one\nhit here\nthree\nhit again\nfive\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("b.rs"), "nothing\n").unwrap();
        let cx = cx_at(dir.path());

        // files mode → just the path, once
        let r = ContentSearch
            .call(json!({"pattern":"hit","mode":"files"}), &cx)
            .await;
        match r {
            ToolResult::Text(t) => {
                assert!(t.contains("a.rs") && !t.contains("b.rs"));
                assert!(
                    !t.contains(':'),
                    "files mode should not emit line nums: {t}"
                );
            }
            other => panic!("expected text, got {other:?}"),
        }

        // count mode → per-file count + total
        let r = ContentSearch
            .call(json!({"pattern":"hit","mode":"count"}), &cx)
            .await;
        match r {
            ToolResult::Text(t) => {
                assert!(t.contains("a.rs:2"), "got: {t}");
                assert!(t.contains("total: 2 matches across 1 files"), "got: {t}");
            }
            other => panic!("expected text, got {other:?}"),
        }

        // content mode with context_lines=1 brackets each hit with - separators
        let r = ContentSearch
            .call(json!({"pattern":"hit again","context_lines":1}), &cx)
            .await;
        match r {
            ToolResult::Text(t) => {
                assert!(t.contains("a.rs:3-three"), "before-context: {t}");
                assert!(t.contains("a.rs:4:hit again"), "match line: {t}");
                assert!(t.contains("a.rs:5-five"), "after-context: {t}");
            }
            other => panic!("expected text, got {other:?}"),
        }

        // case_insensitive: uppercase pattern matches lowercase content
        let r = ContentSearch
            .call(
                json!({"pattern":"HIT","mode":"files","case_insensitive":true}),
                &cx,
            )
            .await;
        match r {
            ToolResult::Text(t) => assert!(t.contains("a.rs"), "case-insensitive: {t}"),
            other => panic!("expected text, got {other:?}"),
        }
        // ...and does NOT match without the flag
        let r = ContentSearch
            .call(json!({"pattern":"HIT","mode":"files"}), &cx)
            .await;
        match r {
            ToolResult::Text(t) => assert_eq!(t, "no matches", "case-sensitive default: {t}"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn content_search_truncation_includes_refinement_hint() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.rs"),
            "hit one\nhit two\nhit three\nhit four\n",
        )
        .unwrap();
        let cx = cx_at(dir.path());

        let r = ContentSearch
            .call(json!({"pattern":"hit","max_results":2}), &cx)
            .await;
        match r {
            ToolResult::Text(t) => {
                assert!(t.contains("[truncated at 2 matches]"), "got: {t}");
                assert!(t.contains("[refine:"), "got: {t}");
                assert!(t.contains("mode=\"files\""), "got: {t}");
            }
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn glob_sorts_by_mtime_vs_name() {
        // `aaa.rs` is lexically first but OLDER; `zzz.rs` is newer. This makes
        // the two orderings disagree, so each assertion actually proves its sort.
        let dir = tempfile::tempdir().unwrap();
        let aaa = dir.path().join("aaa.rs");
        let zzz = dir.path().join("zzz.rs");
        std::fs::write(&aaa, "x\n").unwrap();
        std::fs::write(&zzz, "y\n").unwrap();
        let earlier = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&aaa)
            .unwrap()
            .set_modified(earlier)
            .unwrap();
        let cx = cx_at(dir.path());

        // default mtime → newest (zzz) first
        let r = Glob.call(json!({"pattern":"*.rs"}), &cx).await;
        match r {
            ToolResult::Text(t) => {
                let lines: Vec<&str> = t.lines().collect();
                assert_eq!(lines, vec!["zzz.rs", "aaa.rs"], "mtime order: {t}");
            }
            other => panic!("expected text, got {other:?}"),
        }

        // explicit name → lexicographic (aaa first)
        let r = Glob
            .call(json!({"pattern":"*.rs","sort":"name"}), &cx)
            .await;
        match r {
            ToolResult::Text(t) => {
                let lines: Vec<&str> = t.lines().collect();
                assert_eq!(lines, vec!["aaa.rs", "zzz.rs"], "name order: {t}");
            }
            other => panic!("expected text, got {other:?}"),
        }

        // max_results caps AFTER sort + emits a truncation marker
        let r = Glob
            .call(json!({"pattern":"*.rs","sort":"name","max_results":1}), &cx)
            .await;
        match r {
            ToolResult::Text(t) => {
                let lines: Vec<&str> = t.lines().collect();
                assert_eq!(lines[0], "aaa.rs", "capped slice is top-by-sort: {t}");
                assert!(
                    lines.len() == 2 && lines[1].contains("of 2 matches"),
                    "marker: {t}"
                );
            }
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn glob_matches_relative_path_not_absolute_prefix() {
        // Regression (live bug): a glob must match the BASE-RELATIVE path only.
        // A base dir whose own absolute path contains the pattern's literal
        // substring — here "fleet", as in `.../bro/fleet/worktrees/...` — must
        // NOT make every file match. Before the fix, the matcher was also tested
        // against the absolute path, so `**/*fleet*` returned the whole tree.
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("fleet_worktrees");
        let nested = base.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(base.join("AGENTS.md"), "x\n").unwrap();
        std::fs::write(base.join("plain.rs"), "x\n").unwrap();
        std::fs::write(base.join("fleet_tui.rs"), "x\n").unwrap();
        std::fs::write(nested.join("deep.rs"), "x\n").unwrap();
        let cx = cx_at(&base);

        // `**/*fleet*` matches only the fleet-named file — NOT every file under a
        // base whose absolute path contains "fleet".
        let r = Glob
            .call(json!({"pattern": "**/*fleet*", "sort": "name"}), &cx)
            .await;
        match r {
            ToolResult::Text(t) => {
                let lines: Vec<&str> = t.lines().collect();
                assert_eq!(lines, vec!["fleet_tui.rs"], "only the fleet file: {t}");
            }
            other => panic!("expected text, got {other:?}"),
        }

        // A slash-less pattern matches the basename at ANY depth (fd/rg `-g`).
        let r = Glob
            .call(json!({"pattern": "*.rs", "sort": "name"}), &cx)
            .await;
        match r {
            ToolResult::Text(t) => {
                let lines: Vec<&str> = t.lines().collect();
                assert_eq!(
                    lines,
                    vec!["fleet_tui.rs", "nested/deep.rs", "plain.rs"],
                    "slash-less pattern is recursive by basename: {t}"
                );
            }
            other => panic!("expected text, got {other:?}"),
        }

        // A separator'd pattern is anchored: `*.rs` under no subdir prefix does
        // not reach into `nested/`, but `**/*.rs` does.
        let r = Glob
            .call(json!({"pattern": "nested/*.rs", "sort": "name"}), &cx)
            .await;
        match r {
            ToolResult::Text(t) => {
                let lines: Vec<&str> = t.lines().collect();
                assert_eq!(lines, vec!["nested/deep.rs"], "anchored subdir: {t}");
            }
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn apply_patch_tool_applies_records_edits_and_exposes_grammar() {
        let dir = tempfile::tempdir().unwrap();
        let cx = cx_at(dir.path());

        // The grammar is exposed so the harness offers it on grammar transports.
        let g = ApplyPatch.freeform_grammar().expect("grammar present");
        assert_eq!(g.syntax, "lark");
        assert!(g.definition.contains("*** Begin Patch"));

        let r = ApplyPatch
            .call(
                json!({"source": "*** Begin Patch\n*** Add File: a.txt\n+hello\n*** End Patch"}),
                &cx,
            )
            .await;
        match r {
            ToolResult::Text(t) => assert!(t.contains("added a.txt"), "{t}"),
            other => panic!("expected text, got {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "hello\n"
        );
        // The mutation was recorded for the edit-diagnostics sink.
        assert!(!cx.edits.lock().unwrap().is_empty());

        // Missing patch text is a clean error, not a panic.
        let r = ApplyPatch.call(json!({}), &cx).await;
        assert!(matches!(r, ToolResult::Error(_)));
    }

    #[tokio::test]
    async fn content_search_and_glob_find_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("alpha.rs"), "fn target() {}\nlet z = 1;\n").unwrap();
        std::fs::write(dir.path().join("beta.md"), "no match here\n").unwrap();
        let cx = cx_at(dir.path());

        let r = ContentSearch
            .call(json!({"pattern": "fn target", "glob": "*.rs"}), &cx)
            .await;
        match r {
            ToolResult::Text(t) => {
                assert!(t.contains("alpha.rs:1:"), "expected hit, got: {t}");
                assert!(!t.contains("beta.md"));
            }
            other => panic!("expected text, got {other:?}"),
        }

        let r = Glob.call(json!({"pattern": "*.rs"}), &cx).await;
        match r {
            ToolResult::Text(t) => assert!(t.contains("alpha.rs") && !t.contains("beta.md")),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn glob_and_search_prune_heavy_dirs_without_gitignore() {
        // Regression: a recursive walk must never descend into build/VCS trees
        // like `target/` or `node_modules/`, EVEN when no .gitignore is present
        // (e.g. a non-git dir, or a git worktree whose `.git` file defeats
        // ignore-crate git detection). This is what wedged the in-process daemon
        // by walking a 30 GB `target/`.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep.rs"), "fn wanted() {}\n").unwrap();
        for heavy in ["target", "node_modules"] {
            let sub = dir.path().join(heavy);
            std::fs::create_dir_all(&sub).unwrap();
            std::fs::write(sub.join("buried.rs"), "fn wanted() {}\n").unwrap();
        }
        // No .gitignore written on purpose — pruning must not depend on one.
        let cx = cx_at(dir.path());

        let r = Glob.call(json!({"pattern": "**/*.rs"}), &cx).await;
        match r {
            ToolResult::Text(t) => {
                assert!(t.contains("keep.rs"), "expected keep.rs, got: {t}");
                assert!(
                    !t.contains("buried.rs"),
                    "heavy dirs must be pruned, got: {t}"
                );
            }
            other => panic!("expected text, got {other:?}"),
        }

        let r = ContentSearch
            .call(json!({"pattern": "fn wanted", "mode": "files"}), &cx)
            .await;
        match r {
            ToolResult::Text(t) => assert!(
                !t.contains("buried.rs"),
                "content_search must prune heavy dirs, got: {t}"
            ),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn file_write_and_edit_record_edit_events() {
        use crate::slice_core::sha256_hex;

        let dir = tempfile::tempdir().unwrap();
        // Canonicalize the tempdir root — macOS reports /var/folders but
        // canonicalizes to /private/var/folders, and our absolute paths come
        // back through normalize_lexical without that prefix. Comparing
        // against the canonical root keeps macOS and Linux honest.
        let root = dir.path().canonicalize().unwrap();
        let cx = cx_at(&root);

        // 1) Fresh file_write: empty pre-image, post-image matches content.
        let new_body = "first contents\n";
        let r = FileWrite
            .call(json!({"file_path": "fresh.txt", "content": new_body}), &cx)
            .await;
        assert!(!r.is_error(), "{r:?}");

        // 2) file_edit on top of that file: pre-image is the prior write.
        let r = FileEdit
            .call(
                json!({"file_path": "fresh.txt", "old_string": "first", "new_string": "second"}),
                &cx,
            )
            .await;
        assert!(!r.is_error(), "{r:?}");

        // 3) file_write that overwrites an existing file: pre-image carries
        //    the previous bytes.
        let overwrite = "third contents\n";
        let r = FileWrite
            .call(json!({"file_path": "fresh.txt", "content": overwrite}), &cx)
            .await;
        assert!(!r.is_error(), "{r:?}");

        let events = cx.edits.lock().unwrap().drain();
        assert_eq!(
            events.len(),
            3,
            "expected one event per mutation: {events:?}"
        );

        let expected_path = root.join("fresh.txt");
        let empty_sha = sha256_hex(b"");
        let first_sha = sha256_hex(new_body.as_bytes());
        let edited_body = "second contents\n";
        let edited_sha = sha256_hex(edited_body.as_bytes());
        let overwrite_sha = sha256_hex(overwrite.as_bytes());

        // (a) fresh file_write
        assert_eq!(events[0].path, expected_path);
        assert!(
            events[0].pre_image.is_empty(),
            "fresh write: empty pre-image"
        );
        assert_eq!(events[0].pre_sha256, empty_sha);
        assert_eq!(events[0].post_sha256, first_sha);

        // (b) file_edit
        assert_eq!(events[1].path, expected_path);
        assert_eq!(events[1].pre_image, new_body.as_bytes());
        assert_eq!(events[1].pre_sha256, first_sha);
        assert_eq!(events[1].post_sha256, edited_sha);

        // (c) overwriting file_write
        assert_eq!(events[2].path, expected_path);
        assert_eq!(events[2].pre_image, edited_body.as_bytes());
        assert_eq!(events[2].pre_sha256, edited_sha);
        assert_eq!(events[2].post_sha256, overwrite_sha);

        // drain() resets the sink.
        assert!(cx.edits.lock().unwrap().is_empty());
    }
}
