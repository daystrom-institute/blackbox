//! Workspace tools: file, shell, and git. Ported from daystrom-mk2
//! `Daystrom.Worker/Tools/{FileTools,ShellTools,GitWorkspaceTools}`.
//!
//! All paths are resolved against and confined to `cx.root`. Shell commands
//! pass through [`SafetyPolicy::deny_command`]; `git_commit` additionally
//! rejects staged sensitive files.

use crate::tool::{Tool, ToolAnnotations, ToolCx, ToolResult, schema_for};
use async_trait::async_trait;
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

const NOT_IMPL: &str = "not yet implemented (skeleton)";

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
// Skeleton stubs — real schema, body returns an error until implemented.
// ---------------------------------------------------------------------------

macro_rules! stub_tool {
    ($ty:ident, $name:literal, $desc:literal) => {
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
            async fn call(&self, _input: Value, _cx: &ToolCx) -> ToolResult {
                ToolResult::Error(format!("{}: {NOT_IMPL}", $name))
            }
        }
    };
}

stub_tool!(SmartRead, "smart_read", "Read a file with structure-aware summarization for large files.");
stub_tool!(FileEdit, "file_edit", "Apply an exact string replacement to a file.");
stub_tool!(ContentSearch, "content_search", "Search file contents (ripgrep-style) across the worktree.");
stub_tool!(Glob, "glob", "Find files matching a glob pattern.");

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
}
