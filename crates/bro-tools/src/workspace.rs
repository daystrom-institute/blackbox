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
/// escapes (`..`, absolute paths outside root, symlink traversal). Shared with
/// the `shell` module so shell `cwd` resolution uses the same confinement.
pub(crate) fn resolve_in_root(root: &Path, rel: &str) -> anyhow::Result<PathBuf> {
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

/// Resolve a read-only file path. Normal worktree confinement still applies,
/// but explicit `@...` file mentions get Codex-style handling: `@relative/path`
/// strips the marker and resolves inside the worktree, while `@/abs/path.md`
/// can read instruction docs outside the worktree. Harness-owned dump files are
/// also readable by absolute path so oversized tool-result riders can point at
/// a lossless recovery path. This is intentionally not used by write/edit/shell
/// tools.
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

/// Default cap on lines returned by a single `file_read` when no explicit
/// `max_lines` is given. Keeps an unbounded read from flooding context.
const FILE_READ_DEFAULT_MAX_LINES: usize = 2000;

#[derive(Deserialize, JsonSchema)]
struct FileReadInput {
    /// Path to the file, relative to the worktree root. `@relative/path` is
    /// accepted as a file mention; `@/absolute/instruction.md` is accepted for
    /// read-only instruction docs outside the worktree.
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
    /// Ref ABI (chaining): when set, the read range is stored into this
    /// clipboard register INSTEAD of being returned. The response carries
    /// metadata (register, hashes, counts, preview_head) but not the content,
    /// so a follow-up clip_paste / file_write{from} moves the bytes without
    /// them ever entering your context. Ignores `line_numbers` (registers hold
    /// raw text). See design/bro-harness/bro-harness-tool-chaining.md.
    #[serde(default)]
    into: Option<String>,
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
            if args.line_numbers && args.into.is_none() {
                collected.push(format!("{lineno}\t{line}"));
            } else {
                collected.push(line);
            }
        }

        // Ref ABI: stash into a register instead of returning the content.
        if let Some(register) = &args.into {
            let text = collected.join("\n");
            let range = format!(
                "lines {}-{}",
                start,
                if end == usize::MAX {
                    lineno
                } else {
                    end.min(lineno)
                }
            );
            let provenance = crate::clipboard::Provenance {
                path: args.file_path.clone(),
                file_sha256: String::new(),
                range,
            };
            let mut clip = cx.clipboard.lock().unwrap();
            let evicted = match clip.put_slice(register, text, Some(provenance)) {
                Ok(e) => e,
                Err(e) => return ToolResult::Error(e.to_string()),
            };
            let mut meta = clip.meta_of(register).unwrap_or(Value::Null);
            crate::clipboard::annotate_evicted(&mut meta, evicted);
            if more_after_cap && let Some(obj) = meta.as_object_mut() {
                obj.insert("truncated_at_max_lines".into(), json!(max_lines));
            }
            return ToolResult::Json(meta);
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
    /// Path to write, relative to the worktree root. Parent dirs are created.
    file_path: String,
    /// Full new contents of the file. Provide this OR `from`, not both.
    #[serde(default)]
    content: Option<String>,
    /// Ref ABI (chaining): write the contents of this clipboard register
    /// instead of inlining `content`, so the bytes never pass through your
    /// context. See design/bro-harness/bro-harness-tool-chaining.md.
    #[serde(default)]
    from: Option<String>,
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
        // Resolve the body from exactly one of `content` / `from`.
        let content = match (args.content, &args.from) {
            (Some(_), Some(_)) => {
                return ToolResult::Error("provide content OR from, not both".into());
            }
            (Some(c), None) => c,
            (None, Some(register)) => match cx.clipboard.lock().unwrap().consume_text(register) {
                Ok(t) => t,
                Err(e) => return ToolResult::Error(e),
            },
            (None, None) => return ToolResult::Error("file_write needs content or from".into()),
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
read_git_tool!(
    GitDiff,
    "git_diff",
    "Show the unstaged working-tree diff.",
    &["diff"]
);

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
    /// Max results to return (default 200). Counts matching lines in `content`
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
    /// Ref ABI (chaining): when set, the result is stored into this clipboard
    /// register instead of being returned, so a large match set never costs
    /// context tokens. The response carries metadata + preview; consume it with
    /// clip_paste / file_write{from} / clip_peek. See
    /// design/bro-harness/bro-harness-tool-chaining.md.
    #[serde(default)]
    into: Option<String>,
}

pub struct ContentSearch;

#[async_trait]
impl Tool for ContentSearch {
    fn name(&self) -> &str {
        "content_search"
    }
    fn description(&self) -> &str {
        "Search file contents by regex across the worktree (respects .gitignore). Returns relpath:line:text. Optionally restrict by subdir and filename glob; set mode (content|files|count), context_lines, and case_insensitive. Ref ABI (chaining): set `into=\"<reg>\"` to stash a large match set into a clipboard register INSTEAD of returning it — the matches never cost context tokens, and you then narrow with clip_grep/clip_slice or consume with clip_paste / file_write{from} / shell_run{stdin_from}."
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
        let cap = args.max_results.unwrap_or(200).min(5000);
        let ctx = args.context_lines.unwrap_or(0).min(50);

        let mut hits: Vec<String> = Vec::new();
        // `files`/`count` modes count files; `content` counts lines.
        let mut total_matches = 0usize;
        let mut matched_files = 0usize;
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
            let rel = p.strip_prefix(&cx.root).unwrap_or(p).display().to_string();
            let lines: Vec<&str> = text.lines().collect();

            match args.mode {
                SearchMode::Files => {
                    if lines.iter().any(|l| re.is_match(l)) {
                        hits.push(rel);
                        if hits.len() >= cap {
                            hits.push(format!("[truncated at {cap} files]"));
                            break 'walk;
                        }
                    }
                }
                SearchMode::Count => {
                    let n = lines.iter().filter(|l| re.is_match(l)).count();
                    if n > 0 {
                        total_matches += n;
                        matched_files += 1;
                        hits.push(format!("{rel}:{n}"));
                        if hits.len() >= cap {
                            hits.push(format!("[truncated at {cap} files]"));
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
                                    hits.push(format!("{rel}:{n}{sep}{ctx_line}"));
                                }
                                hits.push("--".into());
                            } else {
                                hits.push(format!("{rel}:{}:{}", i + 1, line));
                            }
                            total_matches += 1;
                            if total_matches >= cap {
                                hits.push(format!("[truncated at {cap} matches]"));
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
        let output = hits.join("\n");
        // Ref ABI: stash the match set into a register instead of returning it.
        if let Some(register) = &args.into {
            return crate::clipboard::deposit_tool_result(&cx.clipboard, register, output);
        }
        ToolResult::Text(output)
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

pub struct Glob;

#[async_trait]
impl Tool for Glob {
    fn name(&self) -> &str {
        "glob"
    }
    fn description(&self) -> &str {
        "Find files matching a glob pattern under the worktree (respects .gitignore). Returns relative paths, capped at max_results (default 1000) with a truncation marker. NOTE: results are sorted by modification time (newest first) by DEFAULT; pass sort=\"name\" for lexicographic order."
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
        // Collect (relpath, mtime) so we can order before formatting.
        let mut out: Vec<(String, std::time::SystemTime)> = Vec::new();
        for entry in WalkBuilder::new(&base).build().flatten() {
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let p = entry.path();
            let rel = p.strip_prefix(&base).unwrap_or(p);
            if matcher.is_match(rel) || matcher.is_match(p) {
                let mtime = entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .unwrap_or(std::time::UNIX_EPOCH);
                out.push((
                    p.strip_prefix(&cx.root).unwrap_or(p).display().to_string(),
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
    }
}

// ---------------------------------------------------------------------------
// smart_read
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
struct SmartReadInput {
    /// Path to the file, relative to the worktree root. `@relative/path` is
    /// accepted as a file mention; `@/absolute/instruction.md` is accepted for
    /// read-only instruction docs outside the worktree.
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
            root: root.to_path_buf(),
            safety: std::sync::Arc::new(crate::safety::SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: std::sync::Arc::new(std::sync::Mutex::new(crate::todo::TodoList::default())),
            shell_sessions: std::sync::Arc::new(std::sync::Mutex::new(
                crate::shell::ShellSessions::default(),
            )),
            clipboard: std::sync::Arc::new(std::sync::Mutex::new(
                crate::clipboard::Registers::default(),
            )),
            edits: std::sync::Arc::new(std::sync::Mutex::new(crate::edits::EditSink::default())),
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
    async fn content_search_into_deposits_register_without_returning_matches() {
        let dir = tempfile::tempdir().unwrap();
        // Enough matches that the 3-line preview_head is a strict subset of the
        // full set — so the late match proves the body never round-trips.
        let body_src: String = (1..=8).map(|n| format!("hit number {n}\n")).collect();
        std::fs::write(dir.path().join("a.rs"), &body_src).unwrap();
        let cx = cx_at(dir.path());

        let r = ContentSearch
            .call(json!({"pattern":"hit","mode":"content","into":"m"}), &cx)
            .await;

        // The response is metadata + bounded preview, NOT the full match set.
        match r {
            ToolResult::Json(v) => {
                assert_eq!(v["register"], "m");
                assert_eq!(v["kind"], "tool_result");
                assert_eq!(v["line_count"], 8);
                // A late match is past the 3-line preview, so it must be absent
                // from the returned payload — the ref-ABI win.
                assert!(
                    !v.to_string().contains("hit number 8"),
                    "full match set must not round-trip through the response: {v}"
                );
            }
            other => panic!("expected json metadata, got {other:?}"),
        }

        // The full match set lives server-side in the register, addressable
        // downstream by clip_grep/clip_paste/file_write{from}/etc.
        let stored = cx
            .clipboard
            .lock()
            .unwrap()
            .consume_text("m")
            .expect("register populated");
        assert!(stored.contains("a.rs:1:hit number 1"), "got: {stored}");
        assert!(stored.contains("a.rs:8:hit number 8"), "got: {stored}");
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
