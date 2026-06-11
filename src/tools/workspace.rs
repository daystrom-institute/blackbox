use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use tantivy::Term;
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, TermQuery};
use tantivy::schema::IndexRecordOption;

use crate::index::TranscriptIndex;
use crate::knowledge::KnowledgeListParams;
use crate::notes::{NoteListParams, NoteParams};
use crate::orchestration::providers::dispatch_path_env;
use crate::server::state::{BlackboxServer, SharedState};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::schemars;
use rmcp::{tool, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

// ── Public router ─────────────────────────────────────────────────────

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::workspace_tools()
}

// ── Constants ─────────────────────────────────────────────────────────

const OUTPUT_CAP_BYTES: usize = 32 * 1024;
const DEFAULT_TIMEOUT_SECS: u64 = 60;
const DEFAULT_LOG_LIMIT: u32 = 20;

#[derive(Debug)]
struct WorkBashEvent {
    stream: &'static str,
    chunk: String,
}

// ── Sensitive-path guard ──────────────────────────────────────────────

fn is_sensitive_path(p: &str) -> bool {
    let lower = p.to_lowercase();
    let name = Path::new(p)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(p)
        .to_lowercase();
    name == ".env"
        || name.starts_with(".env.")
        || lower.contains("/.env")
        || name.contains("credentials")
        || lower.contains("/.aws/")
        || lower.contains("/.gcp/")
        || name.ends_with(".pem")
        || name.ends_with(".p12")
        || name.ends_with(".pfx")
        || name.ends_with(".jks")
        || name.starts_with("keystore")
        || name.starts_with("id_rsa")
        || name.starts_with("id_ed25519")
        || name.starts_with("id_ecdsa")
        || name.starts_with("id_dsa")
        || lower.contains("secret")
        || lower.contains("token")
        || lower.contains("service-account")
        || lower.contains("service_account")
}

// ── Simple glob match (supports * only) ──────────────────────────────

fn glob_matches(pattern: &str, s: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if !pattern.contains('*') {
        return s == pattern;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut cursor = 0usize;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        let rest = &s[cursor..];
        if i == 0 {
            if !rest.starts_with(part) {
                return false;
            }
            cursor += part.len();
        } else if i == parts.len() - 1 {
            return s.ends_with(part);
        } else {
            match rest.find(part) {
                Some(pos) => cursor += pos + part.len(),
                None => return false,
            }
        }
    }
    true
}

// ── Helper: first stored text from a Tantivy doc ─────────────────────

fn doc_text(doc: &tantivy::TantivyDocument, field: tantivy::schema::Field) -> String {
    doc.get_first(field)
        .and_then(|v| match v {
            tantivy::schema::OwnedValue::Str(s) => Some(s.as_str()),
            _ => None,
        })
        .unwrap_or("")
        .to_string()
}

// ── Helper: cap subprocess output ────────────────────────────────────

fn cap_output(s: &str) -> (String, bool) {
    if s.len() <= OUTPUT_CAP_BYTES {
        return (s.to_string(), false);
    }
    let suffix = "\n[... truncated to 32KB by work_bash output cap]";
    let target = OUTPUT_CAP_BYTES.saturating_sub(suffix.len());
    let mut out = String::new();
    for ch in s.chars() {
        if out.len() + ch.len_utf8() > target {
            break;
        }
        out.push(ch);
    }
    out.push_str(suffix);
    (out, true)
}

fn read_capped_stream<R: std::io::Read + Send + 'static>(
    mut reader: R,
    stream: &'static str,
    progress_tx: Option<tokio::sync::mpsc::UnboundedSender<WorkBashEvent>>,
) -> thread::JoinHandle<(String, bool)> {
    thread::spawn(move || {
        let mut out = Vec::new();
        let mut truncated = false;
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if let Some(tx) = &progress_tx {
                        let _ = tx.send(WorkBashEvent {
                            stream,
                            chunk: String::from_utf8_lossy(&buf[..n]).to_string(),
                        });
                    }
                    let remaining = OUTPUT_CAP_BYTES.saturating_sub(out.len());
                    if remaining > 0 {
                        let keep = remaining.min(n);
                        out.extend_from_slice(&buf[..keep]);
                    }
                    if n > remaining {
                        truncated = true;
                    }
                }
                Err(_) => break,
            }
        }
        let mut text = String::from_utf8_lossy(&out).to_string();
        if truncated {
            text.push_str("\n[... truncated to 32KB by work_bash output cap]");
        }
        (text, truncated)
    })
}

#[cfg(unix)]
fn configure_child_process_group(cmd: &mut Command) {
    unsafe {
        cmd.pre_exec(|| {
            if libc::setpgid(0, 0) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
}

#[cfg(not(unix))]
fn configure_child_process_group(_cmd: &mut Command) {}

#[cfg(unix)]
fn kill_child_process_group(pid: u32) {
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_child_process_group(_pid: u32) {}

// ── Helper: resolve repo path ─────────────────────────────────────────

fn resolve_repo(repo: Option<&str>) -> anyhow::Result<PathBuf> {
    let start = match repo {
        Some(r) if !r.is_empty() => PathBuf::from(r),
        _ => std::env::current_dir()?,
    };
    if !start.exists() {
        anyhow::bail!("`repo` path does not exist: {}", start.display());
    }
    let root = git_run(&start, &["rev-parse", "--show-toplevel"])?;
    Ok(PathBuf::from(root.trim()))
}

// ── Helper: run git command in repo, return stdout ────────────────────

// false positive: called from work_* handlers' run_blocking closures.
#[allow(clippy::disallowed_methods)]
fn git_run(repo: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|e| anyhow::anyhow!("git exec failed: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("git error ({}): {}", out.status, stderr.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

// ── Helper: run subprocess with timeout ──────────────────────────────
// Returns (exit_code, stdout, stderr, was_truncated, timed_out)

// false positive: called from work_* handlers' run_blocking closures.
#[allow(clippy::disallowed_methods)]
fn run_with_timeout(
    command: &str,
    cwd: &Path,
    timeout_secs: u64,
    progress_tx: Option<tokio::sync::mpsc::UnboundedSender<WorkBashEvent>>,
) -> anyhow::Result<(i32, String, String, bool, bool)> {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .env("PATH", dispatch_path_env())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_child_process_group(&mut cmd);

    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn command: {e}"))?;
    let child_pid = child.id();

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("failed to capture stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("failed to capture stderr"))?;

    let stdout_reader = read_capped_stream(stdout, "stdout", progress_tx.clone());
    let stderr_reader = read_capped_stream(stderr, "stderr", progress_tx);

    let (status_tx, status_rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = status_tx.send(child.wait());
    });

    let mut timed_out = false;
    let status = match status_rx.recv_timeout(Duration::from_secs(timeout_secs)) {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => return Err(e.into()),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            timed_out = true;
            kill_child_process_group(child_pid);
            match status_rx.recv_timeout(Duration::from_secs(5)) {
                Ok(Ok(status)) => status,
                Ok(Err(e)) => return Err(e.into()),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    anyhow::bail!(
                        "command timed out after {timeout_secs}s and did not exit after kill"
                    )
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    anyhow::bail!("command wait thread disconnected after timeout")
                }
            }
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            anyhow::bail!("command wait thread disconnected")
        }
    };

    let (stdout, trunc_out) = stdout_reader
        .join()
        .unwrap_or_else(|_| ("".to_string(), false));
    let (stderr, trunc_err) = stderr_reader
        .join()
        .unwrap_or_else(|_| ("".to_string(), false));

    Ok((
        if timed_out {
            -1
        } else {
            status.code().unwrap_or(-1)
        },
        stdout,
        stderr,
        trunc_out || trunc_err,
        timed_out,
    ))
}

// ── Parameter structs ─────────────────────────────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkToolCallsParams {
    /// Filter by MCP server name (e.g. "blackbox"). Empty = all servers.
    #[serde(default)]
    pub server: Option<String>,
    /// Exact tool name filter.
    #[serde(default)]
    pub tool_name: Option<String>,
    /// Glob pattern matched against tool names within the selected server scope.
    /// Supports `*` wildcard (e.g. `work_git_*`). Applied after Tantivy retrieval.
    #[serde(default)]
    pub glob_pattern: Option<String>,
    /// Filter by tool kind: read | write | edit | bash | mcp | unknown
    #[serde(default)]
    pub tool_kind: Option<String>,
    /// Substring match on tool_target (file path, cwd, repo).
    #[serde(default)]
    pub tool_target: Option<String>,
    /// Filter by project path (cwd recorded at call time).
    #[serde(default)]
    pub project: Option<String>,
    /// ISO 8601 lower bound on timestamp (lexical compare).
    #[serde(default)]
    pub since: Option<String>,
    /// Max rows to return (default 20, max 100).
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkSmartReadParams {
    /// Absolute or project-relative path to the file.
    pub file_path: String,
    /// Append bbox context (knowledge, notes). Default: true.
    #[serde(default)]
    pub enrich: Option<bool>,
    /// 0-based line offset to start reading from.
    #[serde(default)]
    pub offset: Option<u64>,
    /// Max lines to return (default 2000).
    #[serde(default)]
    pub limit: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkBashParams {
    /// Shell command to run via `sh -c`.
    pub command: String,
    /// Explicit working directory (required; do not infer from ambient context).
    pub cwd: String,
    /// Optional dispatch task ID for correlation.
    // kept: public tool param accepted via JSON schema; correlation hook wired in follow-up
    #[allow(dead_code)]
    #[serde(default)]
    pub task_id: Option<String>,
    /// Command timeout in seconds (default 60, max 300).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkGitStatusParams {
    /// Absolute path to git repository root (required).
    #[serde(default)]
    pub repo: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkGitLogParams {
    /// Absolute path to git repository root (required).
    #[serde(default)]
    pub repo: Option<String>,
    /// Number of commits to return (default 20, max 200).
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkGitDiffParams {
    /// Absolute path to git repository root (required).
    #[serde(default)]
    pub repo: Option<String>,
    /// If true, diff only staged (index) changes; otherwise diff working tree.
    #[serde(default)]
    pub staged: Option<bool>,
    /// Optional path to restrict the diff to.
    #[serde(default)]
    pub path: Option<String>,
    /// Include untracked files as new-file patches. Useful for closeout review
    /// before staging newly created files.
    #[serde(default)]
    pub include_untracked: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkGitShowParams {
    /// Absolute path to git repository root (required).
    #[serde(default)]
    pub repo: Option<String>,
    /// Commit SHA to show.
    pub sha: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkGitCommitParams {
    /// Absolute path to git repository root (required).
    #[serde(default)]
    pub repo: Option<String>,
    /// Commit message.
    pub message: String,
    /// Specific files to stage and commit. If omitted, stages all tracked modifications.
    #[serde(default)]
    pub files: Option<Vec<String>>,
    /// Dispatch task ID; when provided, emits bbox_note(kind=done) on success.
    #[serde(default)]
    pub task_id: Option<String>,
}

// ── Tool implementations ──────────────────────────────────────────────

fn impl_work_tool_calls(idx: &TranscriptIndex, p: &WorkToolCallsParams) -> anyhow::Result<String> {
    let searcher = idx.searcher();
    let fields = idx.field_handles();

    let mut clauses: Vec<(Occur, Box<dyn tantivy::query::Query>)> = vec![(
        Occur::Must,
        Box::new(TermQuery::new(
            Term::from_field_text(fields.doc_type, "tool_call"),
            IndexRecordOption::Basic,
        )),
    )];

    if let Some(s) = p.server.as_deref().filter(|s| !s.is_empty()) {
        clauses.push((
            Occur::Must,
            Box::new(TermQuery::new(
                Term::from_field_text(fields.tool_server, s),
                IndexRecordOption::Basic,
            )),
        ));
    }

    if let Some(n) = p.tool_name.as_deref().filter(|n| !n.is_empty()) {
        clauses.push((
            Occur::Must,
            Box::new(TermQuery::new(
                Term::from_field_text(fields.tool_name, n),
                IndexRecordOption::Basic,
            )),
        ));
    }

    if let Some(k) = p.tool_kind.as_deref().filter(|k| !k.is_empty()) {
        clauses.push((
            Occur::Must,
            Box::new(TermQuery::new(
                Term::from_field_text(fields.tool_kind, k),
                IndexRecordOption::Basic,
            )),
        ));
    }

    let limit = p.limit.unwrap_or(20).min(100) as usize;
    let query = BooleanQuery::new(clauses);
    let top_docs = searcher.search(&query, &TopDocs::with_limit((limit * 20).max(limit)))?;

    if top_docs.is_empty() {
        return Ok(
            "No tool-call documents found. Run bbox_reindex to populate tool-call records from existing transcripts.".to_string()
        );
    }

    let mut rows: Vec<Value> = Vec::new();
    for (_score, addr) in top_docs {
        let doc = searcher.doc(addr)?;
        rows.push(json!({
            "server":       doc_text(&doc, fields.tool_server),
            "tool_name":    doc_text(&doc, fields.tool_name),
            "tool_kind":    doc_text(&doc, fields.tool_kind),
            "tool_target":  doc_text(&doc, fields.tool_target),
            "tool_outcome": doc_text(&doc, fields.tool_outcome),
            "session_id":   doc_text(&doc, fields.session_id),
            "project":      doc_text(&doc, fields.project),
            "timestamp":    doc_text(&doc, fields.timestamp),
            "task_id":      doc_text(&doc, fields.task_id),
        }));
    }

    // Post-filters (applied after Tantivy retrieval)
    if let Some(ref glob) = p.glob_pattern {
        rows.retain(|r| glob_matches(glob, r["tool_name"].as_str().unwrap_or("")));
    }
    if let Some(ref target) = p.tool_target {
        rows.retain(|r| {
            r["tool_target"]
                .as_str()
                .unwrap_or("")
                .contains(target.as_str())
        });
    }
    if let Some(ref project) = p.project {
        rows.retain(|r| {
            r["project"]
                .as_str()
                .unwrap_or("")
                .contains(project.as_str())
        });
    }
    if let Some(ref since) = p.since {
        rows.retain(|r| {
            let ts = r["timestamp"].as_str().unwrap_or("");
            !ts.is_empty() && ts >= since.as_str()
        });
    }
    rows.truncate(limit);

    Ok(serde_json::to_string_pretty(&json!({
        "count": rows.len(),
        "rows": rows,
    }))?)
}

// false positive: called from work_smart_read's run_blocking closure.
#[allow(clippy::disallowed_methods)]
fn impl_work_smart_read(state: &SharedState, p: &WorkSmartReadParams) -> anyhow::Result<String> {
    let path = Path::new(&p.file_path);
    if !path.exists() {
        anyhow::bail!("file not found: {}", p.file_path);
    }
    if !path.is_file() {
        anyhow::bail!("path is not a file: {}", p.file_path);
    }

    let raw = std::fs::read_to_string(path)?;
    let lines: Vec<&str> = raw.lines().collect();
    let total = lines.len();

    let offset = p.offset.unwrap_or(0) as usize;
    let window = p.limit.unwrap_or(2000) as usize;
    let end = (offset + window).min(total);
    let slice = if offset <= total {
        &lines[offset..end]
    } else {
        &[]
    };

    let mut out = String::new();
    for (i, line) in slice.iter().enumerate() {
        out.push_str(&format!("{}\t{}\n", offset + i + 1, line));
    }

    if total > end {
        out.push_str(&format!(
            "\n[{} lines shown of {}; pass offset={} to continue]\n",
            end - offset,
            total,
            end
        ));
    }

    let enrich = p.enrich.unwrap_or(true);
    if !enrich {
        return Ok(out);
    }

    // ── Bounded enrichment ──────────────────────────────────────────
    out.push_str("\n── Enrichment ──────────────────────────────────────────────\n");

    // Related knowledge entries
    let kb_result = state.kb.write().list(&KnowledgeListParams {
        query: Some(p.file_path.clone()),
        limit: Some(3),
        ..Default::default()
    });
    match kb_result {
        Ok(kb) if !kb.contains("No entries found") && !kb.trim().is_empty() => {
            out.push_str("\n## Related knowledge\n");
            out.push_str(&kb);
        }
        _ => {}
    }

    // Unresolved notes mentioning this path
    let notes_result = state.notes.read().list(&NoteListParams {
        id: None,
        query: Some(p.file_path.clone()),
        resolution: Some("unresolved".to_string()),
        limit: Some(5),
        kind: None,
        project: None,
        task_id: None,
        session_id: None,
        thread_id: None,
        bro: None,
        since: None,
        include_addressed: None,
        full: None,
    });
    match notes_result {
        Ok(notes) if !notes.trim().is_empty() && !notes.contains("No notes found") => {
            out.push_str("\n## Unresolved notes\n");
            out.push_str(&notes);
        }
        _ => {}
    }

    Ok(out)
}

fn impl_work_bash(
    p: &WorkBashParams,
    progress_tx: Option<tokio::sync::mpsc::UnboundedSender<WorkBashEvent>>,
) -> anyhow::Result<String> {
    let cwd = Path::new(&p.cwd);
    if !cwd.exists() {
        anyhow::bail!("`cwd` does not exist: {}", p.cwd);
    }
    if !cwd.is_dir() {
        anyhow::bail!("`cwd` is not a directory: {}", p.cwd);
    }

    let timeout = p.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS).min(300);
    let (exit_code, stdout, stderr, truncated, timed_out) =
        run_with_timeout(&p.command, cwd, timeout, progress_tx)?;

    let outcome = if timed_out {
        "timeout"
    } else if exit_code == 0 {
        if truncated { "truncated" } else { "success" }
    } else {
        "error"
    };

    Ok(serde_json::to_string_pretty(&json!({
        "command": p.command,
        "cwd": p.cwd,
        "exit_code": exit_code,
        "outcome": outcome,
        "stdout": stdout,
        "stderr": stderr,
        "truncated": truncated,
        "timed_out": timed_out,
    }))?)
}

fn work_bash_progress_notification(
    token: &rmcp::model::ProgressToken,
    progress: f64,
    event: WorkBashEvent,
) -> anyhow::Result<rmcp::model::ServerNotification> {
    let message = serde_json::to_string(&json!({
        "stream": event.stream,
        "chunk": event.chunk,
    }))?;
    Ok(rmcp::model::ServerNotification::ProgressNotification(
        rmcp::model::Notification::new(rmcp::model::ProgressNotificationParam {
            progress_token: token.clone(),
            progress,
            total: None,
            message: Some(message),
        }),
    ))
}

async fn impl_work_bash_streaming(
    p: WorkBashParams,
    context: rmcp::service::RequestContext<rmcp::RoleServer>,
) -> anyhow::Result<String> {
    let progress_token = context.meta.get_progress_token();
    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel::<WorkBashEvent>();
    let use_progress = progress_token.is_some();
    let worker_tx = use_progress.then_some(progress_tx);
    let handle = tokio::task::spawn_blocking(move || impl_work_bash(&p, worker_tx));
    tokio::pin!(handle);

    let mut progress_idx = 0f64;
    loop {
        tokio::select! {
            event = progress_rx.recv(), if use_progress => {
                if let (Some(token), Some(event)) = (&progress_token, event) {
                    progress_idx += 1.0;
                    let _ = context.peer
                        .send_notification(work_bash_progress_notification(token, progress_idx, event)?)
                        .await;
                }
            }
            result = &mut handle => {
                let output = result.map_err(|e| anyhow::anyhow!("work_bash worker panicked: {e}"))??;
                if let Some(token) = &progress_token {
                    while let Ok(event) = progress_rx.try_recv() {
                        progress_idx += 1.0;
                        let _ = context.peer
                            .send_notification(work_bash_progress_notification(token, progress_idx, event)?)
                            .await;
                    }
                }
                return Ok(output);
            }
        }
    }
}

fn impl_work_git_status(repo_path: &Path) -> anyhow::Result<String> {
    // Branch info
    let branch = git_run(repo_path, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .or_else(|_| git_run(repo_path, &["rev-parse", "--abbrev-ref", "HEAD"]))
        .unwrap_or_else(|_| "unknown".to_string());
    let branch = branch.trim().to_string();

    // Porcelain status
    let status_raw = git_run(repo_path, &["status", "--porcelain=v1"])?;

    let mut staged: Vec<Value> = Vec::new();
    let mut unstaged: Vec<Value> = Vec::new();
    let mut untracked: Vec<Value> = Vec::new();

    for line in status_raw.lines() {
        if line.len() < 3 {
            continue;
        }
        let index_status = &line[..1];
        let worktree_status = &line[1..2];
        let path = line[3..].to_string();

        match (index_status, worktree_status) {
            ("?", "?") => untracked.push(json!({ "path": path })),
            _ => {
                if index_status != " " && index_status != "?" {
                    staged.push(json!({ "path": path, "status": index_status }));
                }
                if worktree_status != " " && worktree_status != "?" {
                    unstaged.push(json!({ "path": path, "status": worktree_status }));
                }
            }
        }
    }

    Ok(serde_json::to_string_pretty(&json!({
        "branch": branch,
        "staged": staged,
        "unstaged": unstaged,
        "untracked": untracked,
        "clean": staged.is_empty() && unstaged.is_empty() && untracked.is_empty(),
    }))?)
}

fn impl_work_git_log(repo_path: &Path, limit: u32) -> anyhow::Result<String> {
    let limit_str = limit.to_string();
    let raw = git_run(
        repo_path,
        &[
            "log",
            &format!("-n{}", limit_str),
            "--format=%H%x1f%P%x1f%an%x1f%ae%x1f%ai%x1f%s",
        ],
    )?;

    let mut commits: Vec<Value> = Vec::new();
    for line in raw.lines() {
        let parts: Vec<&str> = line.splitn(6, '\x1f').collect();
        if parts.len() < 6 {
            continue;
        }
        commits.push(json!({
            "sha":          parts[0],
            "parents":      parts[1].split_whitespace().collect::<Vec<_>>(),
            "author_name":  parts[2],
            "author_email": parts[3],
            "date":         parts[4],
            "subject":      parts[5],
        }));
    }

    Ok(serde_json::to_string_pretty(&json!({
        "count": commits.len(),
        "commits": commits,
    }))?)
}

fn impl_work_git_diff(
    repo_path: &Path,
    staged: bool,
    path_filter: Option<&str>,
    include_untracked: bool,
) -> anyhow::Result<String> {
    let mut args: Vec<&str> = vec!["diff"];
    if staged {
        args.push("--staged");
    }
    if let Some(pf) = path_filter {
        args.push("--");
        args.push(pf);
    }

    let mut raw = git_run(repo_path, &args)?;
    let mut untracked_count = 0usize;
    if include_untracked {
        let untracked = git_untracked_paths(repo_path, path_filter)?;
        untracked_count = untracked.len();
        for path in untracked {
            let patch = git_diff_untracked_file(repo_path, &path)?;
            if !patch.is_empty() {
                if !raw.is_empty() && !raw.ends_with('\n') {
                    raw.push('\n');
                }
                raw.push_str(&patch);
            }
        }
    }
    let (output, truncated) = cap_output(&raw);

    Ok(serde_json::to_string_pretty(&json!({
        "staged": staged,
        "path_filter": path_filter,
        "include_untracked": include_untracked,
        "untracked_count": untracked_count,
        "diff": output,
        "truncated": truncated,
    }))?)
}

fn git_untracked_paths(repo_path: &Path, path_filter: Option<&str>) -> anyhow::Result<Vec<String>> {
    let mut args = vec!["ls-files", "--others", "--exclude-standard", "-z"];
    if let Some(path) = path_filter {
        args.push("--");
        args.push(path);
    }
    let raw = git_run(repo_path, &args)?;
    Ok(raw
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .collect())
}

// false positive: called from work_* handlers' run_blocking closures.
#[allow(clippy::disallowed_methods)]
fn git_diff_untracked_file(repo_path: &Path, path: &str) -> anyhow::Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["diff", "--no-index", "--", "/dev/null", path])
        .output()
        .map_err(|e| anyhow::anyhow!("git exec failed: {e}"))?;
    let code = out.status.code().unwrap_or(1);
    if code != 0 && code != 1 {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("git error ({}): {}", out.status, stderr.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn impl_work_git_show(repo_path: &Path, sha: &str) -> anyhow::Result<String> {
    // Sanitise SHA: must be hex-only to prevent injection
    if sha.is_empty() || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        anyhow::bail!("invalid sha: must be a hex string");
    }

    let raw = git_run(repo_path, &["show", sha])?;
    let (output, truncated) = cap_output(&raw);

    Ok(serde_json::to_string_pretty(&json!({
        "sha": sha,
        "output": output,
        "truncated": truncated,
    }))?)
}

fn impl_work_git_commit(
    state: &SharedState,
    repo_path: &Path,
    p: &WorkGitCommitParams,
) -> anyhow::Result<String> {
    if p.message.trim().is_empty() {
        anyhow::bail!("commit message cannot be empty");
    }

    // Reject sensitive files before staging
    if let Some(ref files) = p.files {
        for f in files {
            if is_sensitive_path(f) {
                anyhow::bail!("refusing to commit sensitive file: {f}");
            }
        }
        for f in files {
            git_run(repo_path, &["add", "--", f])?;
        }
    } else {
        git_run(repo_path, &["add", "-u"])?;
    }

    // Check that there's something staged
    let staged_check = git_run(repo_path, &["diff", "--cached", "--name-only"])?;
    if staged_check.trim().is_empty() {
        anyhow::bail!("nothing staged to commit");
    }

    let commit_out = git_run(repo_path, &["commit", "-m", &p.message])?;
    let sha = git_run(repo_path, &["rev-parse", "HEAD"])
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    // Emit bbox_note(kind=done) when a task_id is supplied
    if let Some(ref task_id) = p.task_id {
        let note_body = format!(
            "committed: {} (sha: {}, repo: {})",
            p.message.lines().next().unwrap_or(""),
            &sha[..sha.len().min(12)],
            repo_path.display()
        );
        let _ = state.notes.write().create(&NoteParams {
            kind: "done".to_string(),
            body: note_body,
            task_id: Some(task_id.clone()),
            session_id: None,
            project: Some(repo_path.to_string_lossy().to_string()),
            thread_id: None,
            provider: None,
            bro: None,
        });
    }

    Ok(serde_json::to_string_pretty(&json!({
        "sha": sha,
        "message": p.message,
        "repo": repo_path.display().to_string(),
        "output": commit_out.trim(),
        "done_note_emitted": p.task_id.is_some(),
    }))?)
}

// ── Tool router ───────────────────────────────────────────────────────

#[tool_router(router = workspace_tools)]
impl BlackboxServer {
    #[tool(
        name = "work_tool_calls",
        description = "Query indexed workspace tool-call records by server, tool_name, glob_pattern, tool_kind, target, project, and time. Rows preserve (server, tool_name) identity."
    )]
    pub(crate) async fn work_tool_calls(
        &self,
        Parameters(p): Parameters<WorkToolCallsParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("work_tool_calls", move || {
            if server.state.idx.read().is_empty() {
                server
                    .state
                    .idx
                    .write()
                    .build_index(false)
                    .map_err(|e| anyhow::anyhow!("auto-index failed: {e}"))?;
            }
            impl_work_tool_calls(&server.state.idx.read(), &p)
        })
        .await
    }

    #[tool(
        name = "work_smart_read",
        description = "Read a file with stable line numbers and optional bounded bbox overlays. Use offset/limit to window large files; set enrich=false for plain reads."
    )]
    pub(crate) async fn work_smart_read(
        &self,
        Parameters(p): Parameters<WorkSmartReadParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("work_smart_read", move || {
            impl_work_smart_read(&server.state, &p)
        })
        .await
    }

    #[tool(
        name = "work_bash",
        description = "Run a shell command in an explicit cwd with streaming progress chunks, 32KB stdout/stderr final caps, and timeout handling."
    )]
    pub(crate) async fn work_bash(
        &self,
        Parameters(p): Parameters<WorkBashParams>,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> CallToolResult {
        match impl_work_bash_streaming(p, context).await {
            Ok(text) => Self::ok_text(&text),
            Err(e) => Self::err_text(&format!("Error: {e:#}")),
        }
    }

    #[tool(
        name = "work_git_status",
        description = "Structured git status for a repository: branch, staged/unstaged/untracked files, and clean flag."
    )]
    pub(crate) async fn work_git_status(
        &self,
        Parameters(p): Parameters<WorkGitStatusParams>,
    ) -> CallToolResult {
        Self::run_blocking("work_git_status", move || {
            let repo = resolve_repo(p.repo.as_deref())?;
            impl_work_git_status(&repo)
        })
        .await
    }

    #[tool(
        name = "work_git_log",
        description = "Structured git commit log with sha, parents, author, date, and subject. Default limit is 20, max 200."
    )]
    pub(crate) async fn work_git_log(
        &self,
        Parameters(p): Parameters<WorkGitLogParams>,
    ) -> CallToolResult {
        Self::run_blocking("work_git_log", move || {
            let repo = resolve_repo(p.repo.as_deref())?;
            let limit = p.limit.unwrap_or(DEFAULT_LOG_LIMIT).min(200);
            impl_work_git_log(&repo, limit)
        })
        .await
    }

    #[tool(
        name = "work_git_diff",
        description = "Structured git diff for working tree or staged changes, optionally path-restricted, with 32KB output cap. Set include_untracked=true to include untracked files as new-file patches."
    )]
    pub(crate) async fn work_git_diff(
        &self,
        Parameters(p): Parameters<WorkGitDiffParams>,
    ) -> CallToolResult {
        Self::run_blocking("work_git_diff", move || {
            let repo = resolve_repo(p.repo.as_deref())?;
            impl_work_git_diff(
                &repo,
                p.staged.unwrap_or(false),
                p.path.as_deref(),
                p.include_untracked.unwrap_or(false),
            )
        })
        .await
    }

    #[tool(
        name = "work_git_show",
        description = "Show one commit by hex SHA with metadata and diff, capped at 32KB."
    )]
    pub(crate) async fn work_git_show(
        &self,
        Parameters(p): Parameters<WorkGitShowParams>,
    ) -> CallToolResult {
        Self::run_blocking("work_git_show", move || {
            let repo = resolve_repo(p.repo.as_deref())?;
            impl_work_git_show(&repo, &p.sha)
        })
        .await
    }

    #[tool(
        name = "work_git_commit",
        description = "Stage and commit files, rejecting sensitive paths before staging. Omitting files stages tracked modifications only; never pushes."
    )]
    pub(crate) async fn work_git_commit(
        &self,
        Parameters(p): Parameters<WorkGitCommitParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("work_git_commit", move || {
            let repo = resolve_repo(p.repo.as_deref())?;
            impl_work_git_commit(&server.state, &repo, &p)
        })
        .await
    }
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── glob_matches ─────────────────────────────────────────────────

    #[test]
    fn glob_exact_match() {
        assert!(glob_matches("work_git_status", "work_git_status"));
        assert!(!glob_matches("work_git_status", "work_git_log"));
    }

    #[test]
    fn glob_star_suffix() {
        assert!(glob_matches("work_git_*", "work_git_status"));
        assert!(glob_matches("work_git_*", "work_git_commit"));
        assert!(!glob_matches("work_git_*", "work_smart_read"));
    }

    #[test]
    fn glob_star_prefix() {
        assert!(glob_matches("*_commit", "work_git_commit"));
        assert!(!glob_matches("*_commit", "work_git_status"));
    }

    #[test]
    fn glob_star_wildcard() {
        assert!(glob_matches("*", "anything"));
        assert!(glob_matches("*", ""));
    }

    #[test]
    fn glob_interior_star() {
        assert!(glob_matches("work_*_commit", "work_git_commit"));
        assert!(!glob_matches("work_*_commit", "work_git_status"));
    }

    // ── is_sensitive_path ────────────────────────────────────────────

    #[test]
    fn sensitive_env_file() {
        assert!(is_sensitive_path(".env"));
        assert!(is_sensitive_path("/home/user/.env"));
        assert!(is_sensitive_path(".env.production"));
    }

    #[test]
    fn sensitive_certs() {
        assert!(is_sensitive_path("server.pem"));
        assert!(is_sensitive_path("client.p12"));
        assert!(is_sensitive_path("cert.pfx"));
    }

    #[test]
    fn sensitive_keys() {
        assert!(is_sensitive_path("id_rsa"));
        assert!(is_sensitive_path("id_ed25519.pub"));
    }

    #[test]
    fn sensitive_credentials() {
        assert!(is_sensitive_path("credentials.json"));
        assert!(is_sensitive_path("/home/user/.aws/credentials"));
    }

    #[test]
    fn not_sensitive_source_files() {
        assert!(!is_sensitive_path("src/main.rs"));
        assert!(!is_sensitive_path("README.md"));
        assert!(!is_sensitive_path("Cargo.toml"));
    }

    // ── cap_output ───────────────────────────────────────────────────

    #[test]
    fn cap_output_short_unchanged() {
        let s = "hello world";
        let (out, truncated) = cap_output(s);
        assert_eq!(out, s);
        assert!(!truncated);
    }

    #[test]
    fn cap_output_long_truncated() {
        let long = "x".repeat(OUTPUT_CAP_BYTES + 1000);
        let (out, truncated) = cap_output(&long);
        assert!(truncated);
        assert!(out.len() <= OUTPUT_CAP_BYTES + 100); // suffix adds a few bytes
        assert!(out.contains("truncated"));
    }

    // ── work_bash runner ────────────────────────────────────────────

    #[test]
    fn work_bash_captures_stdout_and_stderr() {
        let tmp = tempfile::tempdir().unwrap();
        let (exit_code, stdout, stderr, truncated, timed_out) =
            run_with_timeout("printf out; printf err >&2", tmp.path(), 5, None).unwrap();
        assert_eq!(exit_code, 0);
        assert_eq!(stdout, "out");
        assert_eq!(stderr, "err");
        assert!(!truncated);
        assert!(!timed_out);
    }

    #[test]
    fn work_bash_timeout_returns_partial_output() {
        let tmp = tempfile::tempdir().unwrap();
        let (exit_code, stdout, _stderr, _truncated, timed_out) =
            run_with_timeout("printf start; sleep 5; printf end", tmp.path(), 1, None).unwrap();
        assert_eq!(exit_code, -1);
        assert_eq!(stdout, "start");
        assert!(timed_out);
    }

    #[test]
    fn work_bash_emits_progress_chunks() {
        let tmp = tempfile::tempdir().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (_exit_code, stdout, _stderr, _truncated, timed_out) =
            run_with_timeout("printf one; printf two", tmp.path(), 5, Some(tx)).unwrap();
        assert_eq!(stdout, "onetwo");
        assert!(!timed_out);

        let mut chunks = String::new();
        while let Ok(event) = rx.try_recv() {
            assert_eq!(event.stream, "stdout");
            chunks.push_str(&event.chunk);
        }
        assert_eq!(chunks, "onetwo");
    }

    #[test]
    fn work_bash_progress_notification_preserves_stream_and_chunk() {
        let token = rmcp::model::ProgressToken(rmcp::model::NumberOrString::String("tok".into()));
        let event = WorkBashEvent {
            stream: "stderr",
            chunk: "tail line\n".to_string(),
        };

        let notification = work_bash_progress_notification(&token, 2.0, event).unwrap();
        let rmcp::model::ServerNotification::ProgressNotification(notification) = notification
        else {
            panic!("expected progress notification");
        };

        assert_eq!(notification.params.progress_token, token);
        assert_eq!(notification.params.progress, 2.0);
        assert_eq!(notification.params.total, None);
        let message: serde_json::Value =
            serde_json::from_str(notification.params.message.as_deref().unwrap()).unwrap();
        assert_eq!(message["stream"], "stderr");
        assert_eq!(message["chunk"], "tail line\n");
    }

    // ── git_commit sensitive guard (no live git) ─────────────────────

    #[test]
    fn commit_rejects_sensitive_files() {
        let tmp = tempfile::tempdir().unwrap();
        // Provide a valid repo path so resolve_repo passes, but files are rejected first
        let params = WorkGitCommitParams {
            repo: Some(tmp.path().to_string_lossy().to_string()),
            message: "test".to_string(),
            files: Some(vec![".env".to_string(), "README.md".to_string()]),
            task_id: None,
        };
        let state = SharedState::for_test(tmp.path());
        let result = impl_work_git_commit(&state, tmp.path(), &params);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("sensitive"),
            "expected sensitive-file error, got: {msg}"
        );
    }

    // ── work_tool_calls on empty index ───────────────────────────────

    #[test]
    fn tool_calls_empty_index_returns_message() {
        let tmp = tempfile::tempdir().unwrap();
        let idx = TranscriptIndex::open_or_create(
            &tmp.path().join("idx"),
            Vec::new(),
            None,
            tmp.path().join("projects.json"),
            tmp.path().join("kb.json"),
            tmp.path().join("threads.json"),
            tmp.path().join("roadmap.json"),
        )
        .unwrap();

        let p = WorkToolCallsParams {
            server: None,
            tool_name: None,
            glob_pattern: None,
            tool_kind: None,
            tool_target: None,
            project: None,
            since: None,
            limit: None,
        };

        let result = impl_work_tool_calls(&idx, &p).unwrap();
        assert!(result.contains("No tool-call documents found"));
    }

    // ── git_status parse on clean repo ───────────────────────────────

    #[test]
    fn git_status_parses_clean_repo() {
        use std::process::Command;

        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();

        // Init a clean git repo
        if Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(repo)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            let result = impl_work_git_status(repo).unwrap();
            let v: Value = serde_json::from_str(&result).unwrap();
            assert_eq!(v["clean"], true);
            assert_eq!(v["branch"].as_str().unwrap_or(""), "main");
        }
        // If git init fails (CI without git), the test is a no-op
    }

    #[test]
    fn git_diff_can_include_untracked_files() {
        use std::process::Command;

        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();

        if !Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(repo)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return;
        }

        Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(repo)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test User"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::fs::write(repo.join("tracked.txt"), "base\n").unwrap();
        Command::new("git")
            .args(["add", "tracked.txt"])
            .current_dir(repo)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(repo)
            .output()
            .unwrap();

        std::fs::write(repo.join("new.txt"), "hello\n").unwrap();

        let result = impl_work_git_diff(repo, false, None, true).unwrap();
        let v: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["include_untracked"], true);
        assert_eq!(v["untracked_count"], 1);
        let diff = v["diff"].as_str().unwrap();
        assert!(diff.contains("diff --git"), "{diff}");
        assert!(diff.contains("new.txt"), "{diff}");
        assert!(diff.contains("+hello"), "{diff}");
    }

    #[test]
    fn work_bash_uses_dispatch_path_for_user_local_bins() {
        let tmp = tempfile::tempdir().unwrap();
        let mut env = crate::util::TestEnvGuard::new();
        env.set("HOME", tmp.path());
        env.remove("BRO_EXTRA_PATH");
        env.set("PATH", "/usr/bin:/bin");

        let local_bin = tmp.path().join(".local").join("bin");
        std::fs::create_dir_all(&local_bin).unwrap();
        let fake = local_bin.join("fake-rtk");
        std::fs::write(&fake, "#!/bin/sh\necho fake-ok\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&fake).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&fake, perms).unwrap();
        }

        let (exit_code, stdout, stderr, _, _) =
            run_with_timeout("fake-rtk", tmp.path(), 5, None).unwrap();
        assert_eq!(exit_code, 0, "{stderr}");
        assert_eq!(stdout, "fake-ok\n");
    }
}
