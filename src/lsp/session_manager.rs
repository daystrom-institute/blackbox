//! Lazy, reusable LSP session pool.
//!
//! ## Lifecycle
//!
//! * **Spawn**: First call to [`LspSessionManager::with_session`] for a
//!   `(project_root, Language)` key spawns the backend (`jdtls`,
//!   `rust-analyzer`, ...) as a child process, sends LSP `initialize`,
//!   *blocks until the actual response arrives* (hard-fail at
//!   `BLACKBOX_JDTLS_INIT_TIMEOUT_SECS`, default 60), then sends
//!   `initialized`.
//! * **Reuse**: Every subsequent call for the same key reuses the
//!   already-initialized child. The closure receives a fresh
//!   [`LspClient`] wrapper that allocates per-call ids and correlates
//!   responses on the shared stdio.
//! * **Evict**: A background tick (`BLACKBOX_LSP_EVICT_TICK_SECS`,
//!   default 60) drops sessions that have been idle longer than
//!   `BLACKBOX_LSP_IDLE_SECS` (default 600). Each evicted session
//!   gets a best-effort `shutdown`/`exit` then a `kill` if the child
//!   doesn't exit promptly.
//! * **Resilience**: If a request fails because the child closed
//!   stdout or stdin write returned EOF, the session is dropped from
//!   the map; the next call respawns. Callers see the failure for the
//!   in-flight request but don't need to remember broken-state.
//!
//! ## Concurrency
//!
//! Each session is wrapped in a `parking_lot::Mutex`. Two requests
//! against the same `(root, Language)` serialize on that mutex —
//! correct because LSP stdio is fundamentally sequential. Different
//! `(root, Language)` pairs run in parallel.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use lsp_types::{
    notification::{Exit, Initialized, Notification},
    request::{Initialize, Request, Shutdown},
    ClientCapabilities, CodeActionClientCapabilities, CodeActionKind, CodeActionKindLiteralSupport,
    CodeActionLiteralSupport, InitializeParams, ResourceOperationKind,
    TextDocumentClientCapabilities, WorkspaceClientCapabilities, WorkspaceEditClientCapabilities,
    WorkspaceFolder,
};
use parking_lot::Mutex;
use reqwest::Url;
use serde_json::Value;

use crate::projects::Language;

/// Shared LSP session pool. Cheap to clone; the live state lives
/// behind an `Arc<Mutex>`.
#[derive(Clone)]
pub struct LspSessionManager {
    inner: Arc<ManagerInner>,
}

struct ManagerInner {
    sessions: Mutex<HashMap<SessionKey, Arc<Mutex<Session>>>>,
    /// Set on shutdown so the eviction thread bails out and
    /// `with_session` refuses to spawn new backends.
    shutting_down: AtomicBool,
    config: Config,
}

#[derive(Clone, Debug)]
struct Config {
    idle_timeout: Duration,
    request_timeout: Duration,
    jdtls_init_timeout: Duration,
    rust_analyzer_init_timeout: Duration,
    jdtls_bin: Option<String>,
    rust_analyzer_bin: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            idle_timeout: Duration::from_secs(env_u64("BLACKBOX_LSP_IDLE_SECS", 600)),
            request_timeout: Duration::from_secs(env_u64(
                "BLACKBOX_JDTLS_TIMEOUT_SECS",
                30,
            )),
            jdtls_init_timeout: Duration::from_secs(env_u64("BLACKBOX_JDTLS_INIT_TIMEOUT_SECS", 60)),
            rust_analyzer_init_timeout: Duration::from_secs(env_u64(
                "BLACKBOX_RUST_ANALYZER_INIT_TIMEOUT_SECS",
                60,
            )),
            jdtls_bin: std::env::var("BLACKBOX_JDTLS_BIN").ok().filter(|s| !s.trim().is_empty()),
            rust_analyzer_bin: std::env::var("BLACKBOX_RUST_ANALYZER_BIN")
                .ok()
                .filter(|s| !s.trim().is_empty()),
        }
    }
}

impl Config {
    fn from_lsp_config(cfg: &blackbox::config::LspConfig) -> Self {
        Self {
            idle_timeout: Duration::from_secs(cfg.idle_timeout_secs),
            request_timeout: Duration::from_secs(cfg.request_timeout_secs),
            jdtls_init_timeout: Duration::from_secs(cfg.jdtls_init_timeout_secs),
            rust_analyzer_init_timeout: Duration::from_secs(cfg.rust_analyzer_init_timeout_secs),
            jdtls_bin: cfg.jdtls_bin.clone(),
            rust_analyzer_bin: cfg.rust_analyzer_bin.clone(),
        }
    }
}

/// Per-language `initialize` timeout. JDTLS and rust-analyzer both
/// pay multi-second cold-start (workspace import, crate graph build);
/// each gets its own knob with a 60s default.
fn init_timeout_for(language: Language, config: &Config) -> Duration {
    match language {
        Language::Java => config.jdtls_init_timeout,
        Language::Rust => config.rust_analyzer_init_timeout,
    }
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct SessionKey {
    project_root: PathBuf,
    language: Language,
}

struct Session {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    next_id: u64,
    last_used: Instant,
    /// Pending responses keyed by the client-side id. `read_response`
    /// drains arbitrary server messages until it sees its own id and
    /// stashes any others here for the matching request to consume.
    pending: HashMap<u64, Value>,
}

/// Errors a single LSP request can produce. `Broken` is the marker
/// the manager looks for to drop and respawn the session.
#[derive(Debug)]
pub enum LspError {
    Broken(anyhow::Error),
    Other(anyhow::Error),
}

impl std::fmt::Display for LspError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LspError::Broken(e) => write!(f, "lsp session broken: {e:#}"),
            LspError::Other(e) => write!(f, "lsp request failed: {e:#}"),
        }
    }
}

impl std::error::Error for LspError {}

/// Per-call wrapper handed to the `with_session` closure. Allocates
/// request ids from the shared session counter and correlates
/// responses against the pending map. Drops the lock when the
/// closure returns.
pub struct LspClient<'a> {
    session: &'a mut Session,
    config: &'a Config,
}

impl<'a> LspClient<'a> {
    pub fn next_id(&mut self) -> u64 {
        let id = self.session.next_id;
        self.session.next_id += 1;
        id
    }

    pub fn send_request<R: Request>(&mut self, params: &R::Params) -> Result<u64, LspError> {
        let id = self.next_id();
        write_request(&mut self.session.stdin, R::METHOD, id, params)
            .map_err(LspError::Broken)?;
        Ok(id)
    }

    pub fn send_notification<N: Notification>(&mut self, params: &N::Params) -> Result<(), LspError> {
        write_notification(&mut self.session.stdin, N::METHOD, params).map_err(LspError::Broken)
    }

    pub fn read_response<R: Request>(&mut self, id: u64) -> Result<R::Result, LspError> {
        let value = read_response(self.session, id, self.config.request_timeout)?;
        if let Some(error) = value.get("error") {
            return Err(LspError::Other(anyhow!(
                "LSP server returned error: {error}"
            )));
        }
        let result = value
            .get("result")
            .ok_or_else(|| LspError::Other(anyhow!("LSP response missing result")))?;
        serde_json::from_value(result.clone())
            .map_err(|e| LspError::Other(anyhow!("decoding LSP result: {e}")))
    }

    /// Drain server messages until we see a `textDocument/publishDiagnostics`
    /// notification for `uri`, or the timeout elapses. Used after a
    /// `didOpen` to wait for the language server to have actually analyzed
    /// the file — indexing is lazy in rust-analyzer, so the immediate next
    /// request can otherwise see "no references found" even on a long-warm
    /// session. Stashes any responses-by-id seen along the way into the
    /// pending map; ignores other notifications. On timeout, returns
    /// quietly so the caller can proceed (the worst case is that the
    /// caller's request fails the same way it would have before).
    pub fn wait_for_diagnostics(&mut self, uri: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            if Instant::now() >= deadline {
                return;
            }
            let value = match read_message(&mut self.session.reader) {
                Ok(v) => v,
                Err(_) => return,
            };
            if let Some(other) = value.get("id").and_then(Value::as_u64) {
                self.session.pending.insert(other, value);
                continue;
            }
            if value.get("method").and_then(Value::as_str)
                == Some("textDocument/publishDiagnostics")
            {
                let matched = value
                    .get("params")
                    .and_then(|p| p.get("uri"))
                    .and_then(Value::as_str)
                    == Some(uri);
                if matched {
                    return;
                }
            }
        }
    }
}

impl LspSessionManager {
    pub fn new() -> Self {
        Self::with_config(Config::default())
    }

    pub fn with_lsp_config(cfg: &blackbox::config::LspConfig) -> Self {
        Self::with_config(Config::from_lsp_config(cfg))
    }

    fn with_config(config: Config) -> Self {
        let inner = Arc::new(ManagerInner {
            sessions: Mutex::new(HashMap::new()),
            shutting_down: AtomicBool::new(false),
            config,
        });
        let mgr = LspSessionManager { inner };
        mgr.spawn_evictor();
        mgr
    }

    fn spawn_evictor(&self) {
        let weak = Arc::downgrade(&self.inner);
        let tick = Duration::from_secs(env_u64("BLACKBOX_LSP_EVICT_TICK_SECS", 60));
        std::thread::Builder::new()
            .name("blackbox-lsp-evictor".into())
            .spawn(move || loop {
                std::thread::sleep(tick);
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                if inner.shutting_down.load(Ordering::Relaxed) {
                    return;
                }
                let idle = inner.config.idle_timeout;
                let now = Instant::now();
                let stale: Vec<SessionKey> = {
                    let sessions = inner.sessions.lock();
                    sessions
                        .iter()
                        .filter_map(|(k, sess)| {
                            // try_lock so we never block on an in-use session.
                            sess.try_lock().and_then(|s| {
                                if now.duration_since(s.last_used) > idle {
                                    Some(k.clone())
                                } else {
                                    None
                                }
                            })
                        })
                        .collect()
                };
                for key in stale {
                    let removed = inner.sessions.lock().remove(&key);
                    if let Some(arc) = removed {
                        if let Some(mut sess) = arc.try_lock() {
                            tracing::info!(
                                project = %key.project_root.display(),
                                language = ?key.language,
                                "evicting idle LSP session"
                            );
                            shutdown_session(&mut sess);
                        }
                    }
                }
            })
            .expect("spawn lsp evictor thread");
    }

    /// Run a closure against a (lazily spawned) LSP session for the
    /// given project+language. Subsequent calls reuse the same child.
    /// On [`LspError::Broken`] the session is dropped from the pool;
    /// the next call respawns.
    pub fn with_session<F, R>(
        &self,
        project_dir: &Path,
        language: Language,
        f: F,
    ) -> Result<R>
    where
        F: FnOnce(LspClient<'_>) -> Result<R, LspError>,
    {
        if self.inner.shutting_down.load(Ordering::Relaxed) {
            bail!("LSP session manager is shutting down");
        }
        let canonical = std::fs::canonicalize(project_dir)
            .with_context(|| format!("canonicalizing {}", project_dir.display()))?;
        let key = SessionKey {
            project_root: canonical,
            language,
        };

        // One execution attempt; on Broken (including stale-child
        // detection) we drop the entry and the caller can retry by
        // simply calling again. Retrying inline would require
        // forcing `F: FnMut` or cloning, both ugly for the typical
        // single-shot refactor case. The pool already respawns on
        // the next call, which is what callers actually want.
        let arc = {
            let mut sessions = self.inner.sessions.lock();
            if let Some(existing) = sessions.get(&key).cloned() {
                existing
            } else {
                let new = spawn_session(&key, &self.inner.config)?;
                let arc = Arc::new(Mutex::new(new));
                sessions.insert(key.clone(), arc.clone());
                arc
            }
        };

        let mut guard = arc.lock();
        // The child may have died between the previous call and now
        // (idle eviction race, OOM-killer, etc.). Detect via try_wait
        // and respawn before invoking the closure.
        if let Ok(Some(_status)) = guard.child.try_wait() {
            drop(guard);
            self.inner.sessions.lock().remove(&key);
            // Tail-recurse once. f hasn't been touched.
            return self.with_session(project_dir, language, f);
        }

        let client = LspClient {
            session: &mut *guard,
            config: &self.inner.config,
        };
        match f(client) {
            Ok(r) => {
                guard.last_used = Instant::now();
                Ok(r)
            }
            Err(LspError::Broken(err)) => {
                tracing::warn!(
                    project = %key.project_root.display(),
                    language = ?key.language,
                    error = %err,
                    "LSP session broken; dropping"
                );
                let _ = guard.child.kill();
                drop(guard);
                self.inner.sessions.lock().remove(&key);
                Err(err)
            }
            Err(LspError::Other(err)) => Err(err),
        }
    }

    /// Tear down every live session. Invoked on daemon shutdown.
    pub fn shutdown_all(&self) {
        self.inner.shutting_down.store(true, Ordering::Relaxed);
        let mut sessions = self.inner.sessions.lock();
        for (key, arc) in sessions.drain() {
            if let Some(mut sess) = arc.try_lock() {
                tracing::info!(
                    project = %key.project_root.display(),
                    language = ?key.language,
                    "shutting down LSP session"
                );
                shutdown_session(&mut sess);
            }
        }
    }

    /// Test/diagnostic hook: count of currently pooled sessions.
    #[cfg(test)]
    pub fn session_count(&self) -> usize {
        self.inner.sessions.lock().len()
    }
}

impl Default for LspSessionManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Try to send `shutdown` + `exit` then wait briefly for the child;
/// kill if it lingers. Errors are swallowed because eviction is a
/// best-effort path.
fn shutdown_session(sess: &mut Session) {
    let id = sess.next_id;
    sess.next_id += 1;
    let _ = write_request(&mut sess.stdin, Shutdown::METHOD, id, &());
    let _ = write_notification(&mut sess.stdin, Exit::METHOD, &());
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if matches!(sess.child.try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = sess.child.kill();
    let _ = sess.child.wait();
}

fn spawn_session(key: &SessionKey, config: &Config) -> Result<Session> {
    let argv = launch_argv(key.language, config);
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning LSP backend `{}`", argv.join(" ")))?;
    let stdin = child.stdin.take().context("child stdin")?;
    let stdout = child.stdout.take().context("child stdout")?;
    let mut session = Session {
        child,
        stdin,
        reader: BufReader::new(stdout),
        next_id: 1,
        last_used: Instant::now(),
        pending: HashMap::new(),
    };

    let init_params = build_init_params(&key.project_root)?;
    let init_id = session.next_id;
    session.next_id += 1;
    write_request(&mut session.stdin, Initialize::METHOD, init_id, &init_params)
        .context("sending initialize")?;
    let init_timeout = init_timeout_for(key.language, config);
    let value = read_response(&mut session, init_id, init_timeout).map_err(|e| {
        anyhow!(
            "LSP `initialize` did not return within {init_timeout:?}: {e}"
        )
    })?;
    if let Some(error) = value.get("error") {
        bail!("LSP `initialize` failed: {error}");
    }
    write_notification(
        &mut session.stdin,
        Initialized::METHOD,
        &lsp_types::InitializedParams {},
    )
    .context("sending initialized notification")?;
    if matches!(key.language, Language::Rust) {
        // rust-analyzer's `initialize` returns BEFORE `cargo metadata` and
        // workspace indexing finish, so the very first textDocument request
        // against a fresh session reliably comes back "no references". Drain
        // notifications until rust-analyzer either signals ready
        // (`experimental/serverStatus { quiescent: true }`) or emits its
        // first `textDocument/publishDiagnostics` (broader fallback for
        // builds that don't ship the experimental notification). Bounded by
        // BLACKBOX_RUST_ANALYZER_READY_TIMEOUT_SECS (default 30s); on
        // timeout we proceed anyway and let the request fail naturally —
        // the session manager isn't a stop-the-world critical path.
        let ready_timeout = Duration::from_secs(env_u64(
            "BLACKBOX_RUST_ANALYZER_READY_TIMEOUT_SECS",
            30,
        ));
        wait_for_rust_analyzer_ready(&mut session, ready_timeout);
    }
    tracing::info!(
        project = %key.project_root.display(),
        language = ?key.language,
        "spawned + initialized LSP session"
    );
    Ok(session)
}

/// Block until rust-analyzer signals it has finished workspace indexing
/// (or until `timeout` elapses). Stashes any responses-by-id encountered
/// along the way into `session.pending` so a subsequent `read_response`
/// call won't lose them.
fn wait_for_rust_analyzer_ready(session: &mut Session, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let now = Instant::now();
        if now >= deadline {
            tracing::debug!(
                "rust-analyzer didn't signal ready within {timeout:?}; proceeding"
            );
            return;
        }
        let value = match read_message(&mut session.reader) {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!("rust-analyzer ready-wait read error: {e}");
                return;
            }
        };
        if let Some(other) = value.get("id").and_then(Value::as_u64) {
            // Server-sent response to a request we made — stash it for the
            // matching caller.
            session.pending.insert(other, value);
            continue;
        }
        let method = value.get("method").and_then(Value::as_str).unwrap_or("");
        match method {
            "experimental/serverStatus" => {
                let quiescent = value
                    .get("params")
                    .and_then(|p| p.get("quiescent"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if quiescent {
                    tracing::debug!("rust-analyzer reported quiescent");
                    return;
                }
            }
            "textDocument/publishDiagnostics" => {
                // First diagnostics implies rust-analyzer has analyzed at
                // least one file — workspace is loaded enough to answer.
                tracing::debug!("rust-analyzer emitted publishDiagnostics");
                return;
            }
            _ => {
                // Other notifications (window/showMessage, $/progress, etc.)
                // are noise for our purposes. Drop and continue.
            }
        }
    }
}

fn launch_argv(language: Language, config: &Config) -> Vec<String> {
    match language {
        Language::Java => {
            let bin = config.jdtls_bin.clone().unwrap_or_else(|| "jdtls".to_string());
            vec![bin]
        }
        Language::Rust => {
            let bin = config.rust_analyzer_bin.clone().unwrap_or_else(|| "rust-analyzer".to_string());
            vec![bin]
        }
    }
}

fn build_init_params(project_root: &Path) -> Result<InitializeParams> {
    let root_uri = Url::from_directory_path(project_root)
        .map_err(|_| anyhow!("failed to convert {} to file URL", project_root.display()))?;
    Ok(InitializeParams {
        process_id: Some(std::process::id()),
        root_uri: Some(root_uri.clone()),
        capabilities: ClientCapabilities {
            workspace: Some(WorkspaceClientCapabilities {
                workspace_edit: Some(WorkspaceEditClientCapabilities {
                    document_changes: Some(true),
                    resource_operations: Some(vec![
                        ResourceOperationKind::Create,
                        ResourceOperationKind::Rename,
                        ResourceOperationKind::Delete,
                    ]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            text_document: Some(TextDocumentClientCapabilities {
                code_action: Some(CodeActionClientCapabilities {
                    code_action_literal_support: Some(CodeActionLiteralSupport {
                        code_action_kind: CodeActionKindLiteralSupport {
                            value_set: vec![CodeActionKind::SOURCE_ORGANIZE_IMPORTS
                                .as_str()
                                .to_string()],
                        },
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        },
        workspace_folders: Some(vec![WorkspaceFolder {
            uri: root_uri,
            name: "blackbox-lsp".to_string(),
        }]),
        ..Default::default()
    })
}

fn write_request<P: serde::Serialize>(
    stdin: &mut ChildStdin,
    method: &str,
    id: u64,
    params: &P,
) -> Result<()> {
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    let body = serde_json::to_vec(&req)?;
    write!(stdin, "Content-Length: {}\r\n\r\n", body.len())?;
    stdin.write_all(&body)?;
    stdin.flush()?;
    Ok(())
}

fn write_notification<P: serde::Serialize>(
    stdin: &mut ChildStdin,
    method: &str,
    params: &P,
) -> Result<()> {
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    });
    let body = serde_json::to_vec(&req)?;
    write!(stdin, "Content-Length: {}\r\n\r\n", body.len())?;
    stdin.write_all(&body)?;
    stdin.flush()?;
    Ok(())
}

/// Drain server messages until we see the response matching `id`.
/// Server-pushed notifications/requests are ignored; responses with
/// other ids are stashed in the pending map for the matching caller.
/// Honours a per-call deadline so a wedged backend doesn't block the
/// caller forever.
fn read_response(session: &mut Session, id: u64, timeout: Duration) -> Result<Value, LspError> {
    if let Some(stashed) = session.pending.remove(&id) {
        return Ok(stashed);
    }
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() >= deadline {
            return Err(LspError::Broken(anyhow!(
                "timed out waiting for LSP response id={id} (>{timeout:?})"
            )));
        }
        let value = match read_message(&mut session.reader) {
            Ok(v) => v,
            Err(e) => return Err(LspError::Broken(e)),
        };
        match value.get("id").and_then(Value::as_u64) {
            Some(other) if other == id => return Ok(value),
            Some(other) => {
                session.pending.insert(other, value);
            }
            None => {
                // Server-pushed notification / request with no id we
                // care about. Ignore.
            }
        }
    }
}

fn read_message<R: BufRead + Read>(reader: &mut R) -> Result<Value> {
    let mut content_length = None;
    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            bail!("LSP server closed stdout");
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(value) = trimmed.strip_prefix("Content-Length:") {
            content_length = Some(value.trim().parse::<usize>()?);
        }
    }
    let len = content_length.context("LSP message missing Content-Length")?;
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body)?;
    Ok(serde_json::from_slice(&body)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manager_starts_empty_and_shuts_down_idempotently() {
        let m = LspSessionManager::new();
        assert_eq!(m.session_count(), 0);
        m.shutdown_all();
        m.shutdown_all();
    }

    #[test]
    fn refuses_session_after_shutdown() {
        let m = LspSessionManager::new();
        m.shutdown_all();
        let dir = tempfile::tempdir().unwrap();
        let res = m.with_session::<_, ()>(dir.path(), Language::Java, |_| Ok(()));
        assert!(res.is_err());
    }

    #[test]
    fn rust_analyzer_alias_no_longer_used() {
        let _guard = blackbox::util::test_env_lock();
        let orig_prefixed = std::env::var("BLACKBOX_RUST_ANALYZER_BIN").ok();

        std::env::remove_var("BLACKBOX_RUST_ANALYZER_BIN");
        std::env::set_var("RUST_ANALYZER_BIN", "alias-path");

        let config = Config::default();
        assert_eq!(config.rust_analyzer_bin, None,
            "RUST_ANALYZER_BIN should not be accepted after Phase 5");

        std::env::remove_var("RUST_ANALYZER_BIN");
        if let Some(v) = orig_prefixed { std::env::set_var("BLACKBOX_RUST_ANALYZER_BIN", v); } else { std::env::remove_var("BLACKBOX_RUST_ANALYZER_BIN"); }
    }
}

