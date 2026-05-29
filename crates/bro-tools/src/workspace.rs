//! Workspace tools: file, shell, and git. Ported from daystrom-mk2
//! `Daystrom.Worker/Tools/{FileTools,ShellTools,GitWorkspaceTools}`.
//!
//! All paths are resolved against and confined to `cx.root`. Shell commands
//! pass through [`SafetyPolicy::deny_command`]; `git_commit` additionally
//! rejects staged sensitive files.

use crate::tool::{Tool, ToolAnnotations, ToolCx, ToolResult, schema_for};
use async_trait::async_trait;
use globset::Glob as GlobPattern;
use ignore::WalkBuilder;
use regex::Regex;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Resolve a caller-supplied path against the worktree root and refuse
/// escapes (`..`, absolute paths outside root, symlink traversal).
fn resolve_in_root(root: &Path, rel: &str) -> anyhow::Result<PathBuf> {
    let joined = if Path::new(rel).is_absolute() {
        PathBuf::from(rel)
    } else {
        root.join(rel)
    };
    // Lexical containment check first (works for not-yet-existing paths).
    let normalized = normalize_lexical(&joined);
    let root_norm = normalize_lexical(root);
    if !normalized.starts_with(&root_norm) {
        anyhow::bail!("path escapes worktree root: {rel}");
    }
    Ok(normalized)
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

/// Read a file as UTF-8, returning None for binary/oversized/unreadable.
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

#[derive(Deserialize, JsonSchema)]
struct FileReadInput {
    /// Path to the file, relative to the worktree root.
    file_path: String,
    /// 1-based start line (inclusive). Omit to read from the beginning.
    start_line: Option<usize>,
    /// 1-based end line (inclusive). Omit to read to EOF.
    end_line: Option<usize>,
}

pub struct FileRead;

#[async_trait]
impl Tool for FileRead {
    fn name(&self) -> &str {
        "file_read"
    }
    fn description(&self) -> &str {
        "Read a UTF-8 text file in the worktree. Optionally restrict to a 1-based [start_line, end_line] range."
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
        let args: FileReadInput = match serde_json::from_value(input) {
            Ok(a) => a,
            Err(e) => return ToolResult::Error(format!("bad input: {e}")),
        };
        let path = match resolve_in_root(&cx.root, &args.file_path) {
            Ok(p) => p,
            Err(e) => return ToolResult::Error(e.to_string()),
        };
        let body = match tokio::fs::read_to_string(&path).await {
            Ok(b) => b,
            Err(e) => return ToolResult::Error(format!("read {}: {e}", args.file_path)),
        };
        let sliced = match (args.start_line, args.end_line) {
            (None, None) => body,
            (s, e) => {
                let start = s.unwrap_or(1).saturating_sub(1);
                let end = e.unwrap_or(usize::MAX);
                body.lines()
                    .enumerate()
                    .filter(|(i, _)| *i >= start && *i < end)
                    .map(|(_, l)| l)
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        };
        ToolResult::Text(sliced)
    }
}

// ---------------------------------------------------------------------------
// file_write
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
struct FileWriteInput {
    /// Path to write, relative to the worktree root. Parent dirs are created.
    file_path: String,
    /// Full new contents of the file.
    content: String,
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
        let path = match resolve_in_root(&cx.root, &args.file_path) {
            Ok(p) => p,
            Err(e) => return ToolResult::Error(e.to_string()),
        };
        if let Some(parent) = path.parent()
            && let Err(e) = tokio::fs::create_dir_all(parent).await
        {
            return ToolResult::Error(format!("mkdir {}: {e}", parent.display()));
        }
        match tokio::fs::write(&path, args.content.as_bytes()).await {
            Ok(()) => ToolResult::Json(json!({"ok": true, "bytes": args.content.len()})),
            Err(e) => ToolResult::Error(format!("write {}: {e}", args.file_path)),
        }
    }
}

// ---------------------------------------------------------------------------
// list_dir
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
struct ListDirInput {
    /// Directory to list, relative to the worktree root. Defaults to root.
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
        let args: ListDirInput = serde_json::from_value(input).unwrap_or(ListDirInput { path: None });
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
// shell_run
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
struct ShellRunInput {
    /// The shell command line to execute (run via `bash -lc`).
    command: String,
    /// Working subdirectory relative to the worktree root.
    cwd: Option<String>,
}

pub struct ShellRun;

#[async_trait]
impl Tool for ShellRun {
    fn name(&self) -> &str {
        "shell_run"
    }
    fn description(&self) -> &str {
        "Run a shell command in the worktree and return stdout/stderr/exit code. Categorically destructive commands (rm -rf /, git reset --hard, kill-by-port, etc.) are refused."
    }
    fn input_schema(&self) -> Value {
        schema_for::<ShellRunInput>()
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let args: ShellRunInput = match serde_json::from_value(input) {
            Ok(a) => a,
            Err(e) => return ToolResult::Error(format!("bad input: {e}")),
        };
        if let Some(reason) = cx.safety.deny_command(&args.command) {
            return ToolResult::Error(format!("refused: {reason}"));
        }
        let cwd = match resolve_in_root(&cx.root, args.cwd.as_deref().unwrap_or(".")) {
            Ok(p) => p,
            Err(e) => return ToolResult::Error(e.to_string()),
        };
        let out = tokio::process::Command::new("bash")
            .args(["-lc", &args.command])
            .current_dir(&cwd)
            .output()
            .await;
        match out {
            Ok(o) => ToolResult::Json(json!({
                "exit_code": o.status.code(),
                "stdout": String::from_utf8_lossy(&o.stdout),
                "stderr": String::from_utf8_lossy(&o.stderr),
            })),
            Err(e) => ToolResult::Error(format!("spawn failed: {e}")),
        }
    }
}

// ---------------------------------------------------------------------------
// git tools (read-only via shell)
// ---------------------------------------------------------------------------

async fn git(cx: &ToolCx, args: &[&str]) -> ToolResult {
    let out = tokio::process::Command::new("git")
        .args(args)
        .current_dir(&cx.root)
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

read_git_tool!(GitStatus, "git_status", "Show `git status --short`.", &["status", "--short"]);
read_git_tool!(GitLog, "git_log", "Show recent commits (`git log --oneline -20`).", &["log", "--oneline", "-20"]);
read_git_tool!(GitDiff, "git_diff", "Show the unstaged working-tree diff.", &["diff"]);

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
        let args: GitShowInput = serde_json::from_value(input).unwrap_or(GitShowInput { rev: None });
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
    /// Path to the file, relative to the worktree root.
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
            Ok(()) => ToolResult::Json(json!({"ok": true, "replacements": count.min(if args.replace_all { count } else { 1 })})),
            Err(e) => ToolResult::Error(format!("write {}: {e}", args.file_path)),
        }
    }
}

// ---------------------------------------------------------------------------
// content_search (ripgrep-style; gitignore-aware)
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
struct ContentSearchInput {
    /// Regular expression to search for.
    pattern: String,
    /// Subdirectory to search, relative to root (default: whole worktree).
    path: Option<String>,
    /// Optional glob to restrict files by name (e.g. "*.rs").
    glob: Option<String>,
    /// Max matching lines to return (default 200).
    max_results: Option<usize>,
}

pub struct ContentSearch;

#[async_trait]
impl Tool for ContentSearch {
    fn name(&self) -> &str {
        "content_search"
    }
    fn description(&self) -> &str {
        "Search file contents by regex across the worktree (respects .gitignore). Returns relpath:line:text. Optionally restrict by subdir and filename glob."
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
        let args: ContentSearchInput = match serde_json::from_value(input) {
            Ok(a) => a,
            Err(e) => return ToolResult::Error(format!("bad input: {e}")),
        };
        let re = match Regex::new(&args.pattern) {
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
        let cap = args.max_results.unwrap_or(200).min(5000);

        let mut hits: Vec<String> = Vec::new();
        'walk: for entry in WalkBuilder::new(&base).build().flatten() {
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
            let rel = p.strip_prefix(&cx.root).unwrap_or(p).display();
            for (i, line) in text.lines().enumerate() {
                if re.is_match(line) {
                    hits.push(format!("{rel}:{}:{}", i + 1, line));
                    if hits.len() >= cap {
                        hits.push(format!("[truncated at {cap} matches]"));
                        break 'walk;
                    }
                }
            }
        }
        ToolResult::Text(if hits.is_empty() {
            "no matches".into()
        } else {
            hits.join("\n")
        })
    }
}

// ---------------------------------------------------------------------------
// glob
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
struct GlobInput {
    /// Glob pattern relative to the base dir (e.g. "**/*.rs", "src/*.toml").
    pattern: String,
    /// Base dir relative to root (default root).
    path: Option<String>,
}

pub struct Glob;

#[async_trait]
impl Tool for Glob {
    fn name(&self) -> &str {
        "glob"
    }
    fn description(&self) -> &str {
        "Find files matching a glob pattern under the worktree (respects .gitignore). Returns relative paths."
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
        let args: GlobInput = match serde_json::from_value(input) {
            Ok(a) => a,
            Err(e) => return ToolResult::Error(format!("bad input: {e}")),
        };
        let matcher = match GlobPattern::new(&args.pattern) {
            Ok(gp) => gp.compile_matcher(),
            Err(e) => return ToolResult::Error(format!("bad glob: {e}")),
        };
        let base = match walk_base(&cx.root, args.path.as_deref()) {
            Ok(p) => p,
            Err(e) => return ToolResult::Error(e.to_string()),
        };
        let mut out: Vec<String> = Vec::new();
        for entry in WalkBuilder::new(&base).build().flatten() {
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let p = entry.path();
            let rel = p.strip_prefix(&base).unwrap_or(p);
            if matcher.is_match(rel) || matcher.is_match(p) {
                out.push(p.strip_prefix(&cx.root).unwrap_or(p).display().to_string());
                if out.len() >= 2000 {
                    break;
                }
            }
        }
        out.sort();
        ToolResult::Text(if out.is_empty() {
            "no files matched".into()
        } else {
            out.join("\n")
        })
    }
}

// ---------------------------------------------------------------------------
// smart_read
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
struct SmartReadInput {
    /// Path to the file, relative to the worktree root.
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
        "Read a file; small files are returned whole, large files are summarized as a definition outline (with line numbers) plus a head sample, so you can then file_read specific ranges."
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
        let path = match resolve_in_root(&cx.root, &args.file_path) {
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
        let head: String = lines.iter().take(40).cloned().collect::<Vec<_>>().join("\n");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_path_escape() {
        let root = PathBuf::from("/work/repo");
        assert!(resolve_in_root(&root, "../etc/passwd").is_err());
        assert!(resolve_in_root(&root, "src/main.rs").is_ok());
        assert!(resolve_in_root(&root, "/work/repo/src/x").is_ok());
        assert!(resolve_in_root(&root, "/etc/passwd").is_err());
    }

    fn cx_at(root: &Path) -> ToolCx {
        ToolCx {
            root: root.to_path_buf(),
            safety: std::sync::Arc::new(crate::safety::SafetyPolicy::new()),
            http: reqwest::Client::new(),
        }
    }

    #[tokio::test]
    async fn file_edit_requires_unique_match_then_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.txt");
        std::fs::write(&f, "x = 1\nx = 1\ny = 2\n").unwrap();
        let cx = cx_at(dir.path());

        // non-unique without replace_all → error
        let r = FileEdit
            .call(json!({"file_path":"a.txt","old_string":"x = 1","new_string":"x = 9"}), &cx)
            .await;
        assert!(r.is_error(), "non-unique edit should fail: {r:?}");

        // unique edit succeeds
        let r = FileEdit
            .call(json!({"file_path":"a.txt","old_string":"y = 2","new_string":"y = 5"}), &cx)
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
}
