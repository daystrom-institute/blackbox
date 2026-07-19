//! In-process LSP client core for bro-harness.
//!
//! This crate is intentionally independent of the root `blackbox` crate. The
//! harness and daemon may share this crate, but the harness must not call back
//! into the daemon at runtime.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail};
use lsp_types::notification::{
    DidChangeTextDocument, DidOpenTextDocument, Initialized, Notification,
};
use lsp_types::request::{DocumentDiagnosticRequest, Initialize, Request, Shutdown};
use lsp_types::{
    ClientCapabilities, Diagnostic, DiagnosticClientCapabilities, DiagnosticServerCapabilities,
    DidChangeTextDocumentParams, DidOpenTextDocumentParams, DocumentDiagnosticParams,
    DocumentDiagnosticReport, DocumentDiagnosticReportResult, ExecuteCommandClientCapabilities,
    ExecuteCommandParams, FailureHandlingKind, FileRename, InitializeParams, InitializeResult,
    PartialResultParams, PublishDiagnosticsClientCapabilities, PublishDiagnosticsParams,
    RenameFilesParams, ResourceOperationKind, TextDocumentClientCapabilities,
    TextDocumentContentChangeEvent, TextDocumentIdentifier, TextDocumentItem, Url,
    VersionedTextDocumentIdentifier, WorkDoneProgressParams, WorkspaceClientCapabilities,
    WorkspaceEditClientCapabilities, WorkspaceFileOperationsClientCapabilities, WorkspaceFolder,
};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::time;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum Language {
    Rust,
    Java,
}

impl Language {
    pub fn language_id(self) -> &'static str {
        match self {
            Language::Rust => "rust",
            Language::Java => "java",
        }
    }

    fn launch_argv(self, config: &LspConfig) -> Vec<String> {
        match self {
            Language::Rust => vec![
                config
                    .rust_analyzer_bin
                    .clone()
                    .map(|path| path.display().to_string())
                    .or_else(|| env_string("BRO_LSP_RUST_ANALYZER_BIN"))
                    .or_else(|| env_string("BRO_RUST_ANALYZER_BIN"))
                    .or_else(|| env_string("BLACKBOX_RUST_ANALYZER_BIN"))
                    .unwrap_or_else(|| "rust-analyzer".to_string()),
            ],
            // JDTLS (Eclipse JDT.LS), launched via the `jdtls` wrapper script
            // that ships with the distribution — it owns the launcher jar,
            // config dir, and per-workspace `-data` directory, so the bare
            // command is the whole argv (mirrors the daemon's bbox-lsp
            // `launch_argv`). Env chain reuses BLACKBOX_JDTLS_BIN so an
            // operator with the daemon backend configured needs no new var.
            Language::Java => vec![
                config
                    .jdtls_bin
                    .clone()
                    .map(|path| path.display().to_string())
                    .or_else(|| env_string("BRO_LSP_JDTLS_BIN"))
                    .or_else(|| env_string("BLACKBOX_JDTLS_BIN"))
                    .unwrap_or_else(|| "jdtls".to_string()),
            ],
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessState {
    NotStarted,
    Initializing,
    Indexing,
    Ready,
    Failed,
}

impl ReadinessState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotStarted => "not_started",
            Self::Initializing => "initializing",
            Self::Indexing => "indexing",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ReadinessStatus {
    pub language: &'static str,
    pub state: ReadinessState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Debug)]
pub struct LspConfig {
    pub idle_timeout: Duration,
    pub request_timeout: Duration,
    pub init_timeout: Duration,
    pub ready_timeout: Duration,
    pub evict_tick: Duration,
    pub rust_analyzer_bin: Option<PathBuf>,
    /// JDTLS (`jdtls` wrapper) binary; defaults to the env chain
    /// (`BRO_LSP_JDTLS_BIN` → `BLACKBOX_JDTLS_BIN`) then PATH `jdtls`.
    pub jdtls_bin: Option<PathBuf>,
    /// JDTLS workspace-ready deadline. Gradle/Maven import is slow on a
    /// cold workspace, so this is independent of the rust-analyzer
    /// `ready_timeout`. Defaults to 60s (`BRO_LSP_JDTLS_READY_TIMEOUT_SECS`).
    pub jdtls_ready_timeout: Duration,
    /// Optional gradle version JDTLS uses for the Buildship import instead
    /// of the project wrapper — a workaround for gradle 9.x's configuration
    /// exclusive-lock hardening, which trips JDTLS's injected
    /// GradleAnnotationProcessorPatchPlugin and breaks the import. Pin a
    /// pre-9 gradle (e.g. "8.14.3") via `BRO_LSP_JDTLS_GRADLE_VERSION`.
    /// Defaults off (use the wrapper); opt in per project that needs it.
    pub jdtls_gradle_version: Option<String>,
}

impl Default for LspConfig {
    fn default() -> Self {
        Self {
            idle_timeout: Duration::from_secs(env_u64("BRO_LSP_IDLE_SECS", 600)),
            request_timeout: Duration::from_secs(env_u64("BRO_LSP_REQUEST_TIMEOUT_SECS", 30)),
            init_timeout: Duration::from_secs(env_u64("BRO_LSP_INIT_TIMEOUT_SECS", 60)),
            ready_timeout: Duration::from_secs(env_u64(
                "BRO_LSP_RUST_ANALYZER_READY_TIMEOUT_SECS",
                30,
            )),
            evict_tick: Duration::from_secs(env_u64("BRO_LSP_EVICT_TICK_SECS", 60)),
            rust_analyzer_bin: env_path("BRO_LSP_RUST_ANALYZER_BIN")
                .or_else(|| env_path("BRO_RUST_ANALYZER_BIN"))
                .or_else(|| env_path("BLACKBOX_RUST_ANALYZER_BIN")),
            jdtls_bin: env_path("BRO_LSP_JDTLS_BIN").or_else(|| env_path("BLACKBOX_JDTLS_BIN")),
            jdtls_ready_timeout: Duration::from_secs(env_u64(
                "BRO_LSP_JDTLS_READY_TIMEOUT_SECS",
                60,
            )),
            jdtls_gradle_version: env_string("BRO_LSP_JDTLS_GRADLE_VERSION"),
        }
    }
}

#[derive(Debug)]
pub enum Error {
    LspUnavailable {
        language: Language,
        command: String,
        source: String,
    },
    Superseded {
        uri: Url,
        requested_version: i32,
        current_version: Option<i32>,
    },
    DocumentNotOpen {
        uri: Url,
    },
    DocumentAlreadyOpen {
        uri: Url,
    },
    InvalidVersion {
        uri: Url,
        current_version: i32,
        new_version: i32,
    },
    Server {
        method: String,
        error: Value,
    },
    NotReady {
        state: ReadinessState,
        detail: Option<String>,
        waited: Duration,
    },
    Protocol(anyhow::Error),
}

impl Error {
    pub fn is_lsp_unavailable(&self) -> bool {
        matches!(self, Self::LspUnavailable { .. })
    }

    pub fn is_superseded(&self) -> bool {
        matches!(self, Self::Superseded { .. })
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::LspUnavailable {
                language,
                command,
                source,
            } => write!(
                f,
                "lsp_unavailable: {:?} backend `{command}` is unavailable: {source}",
                language
            ),
            Error::Superseded {
                uri,
                requested_version,
                current_version,
            } => write!(
                f,
                "diagnostics for {uri} version {requested_version} were superseded by {current_version:?}"
            ),
            Error::DocumentNotOpen { uri } => write!(f, "document is not open: {uri}"),
            Error::DocumentAlreadyOpen { uri } => write!(f, "document is already open: {uri}"),
            Error::InvalidVersion {
                uri,
                current_version,
                new_version,
            } => write!(
                f,
                "invalid document version for {uri}: current {current_version}, new {new_version}"
            ),
            Error::Server { method, error } => {
                write!(f, "LSP server returned error for {method}: {error}")
            }
            Error::NotReady {
                state,
                detail,
                waited,
            } => {
                write!(
                    f,
                    "server still {} after {}; retry or raise wait_ready_ms",
                    state.as_str(),
                    format_wait_duration(*waited)
                )?;
                if let Some(detail) = detail {
                    write!(f, " ({detail})")?;
                }
                Ok(())
            }
            Error::Protocol(err) => write!(f, "lsp protocol error: {err:#}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<anyhow::Error> for Error {
    fn from(value: anyhow::Error) -> Self {
        Self::Protocol(value)
    }
}

#[derive(Clone, Debug)]
pub struct OpenDocument {
    pub root: PathBuf,
    pub language: Language,
    pub uri: Url,
    pub version: i32,
}

#[derive(Clone)]
pub struct SessionPool {
    inner: Arc<PoolInner>,
}

struct PoolInner {
    config: LspConfig,
    sessions: Mutex<HashMap<SessionKey, Arc<Mutex<Session>>>>,
    evictor_started: AtomicBool,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct SessionKey {
    root: PathBuf,
    language: Language,
}

struct Session {
    language: Language,
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
    pending: HashMap<i64, Value>,
    documents: HashMap<Url, OpenDocumentState>,
    published: HashMap<Url, DiagnosticSnapshot>,
    pulled: HashMap<Url, DiagnosticSnapshot>,
    result_ids: HashMap<Url, String>,
    capabilities: InitializeResult,
    request_timeout: Duration,
    last_used: Instant,
    readiness: ReadinessTracker,
}

#[derive(Clone, Debug)]
struct ReadinessTracker {
    state: ReadinessState,
    detail: Option<String>,
    observed_readiness_signal: bool,
    active_progress: HashSet<String>,
    /// A server-specific status channel (rust-analyzer serverStatus, JDTLS
    /// language/status) has been observed. Once true, that channel owns
    /// readiness: generic progress pairs may no longer promote to Ready,
    /// because rust-analyzer emits short-lived progress streams (Fetching)
    /// long before it is quiescent.
    status_authority: bool,
    /// Ready was declared by the status channel itself. A non-quiescent /
    /// busy status demotes any progress-induced Ready, but never a
    /// status-declared one.
    ready_from_status: bool,
}

impl ReadinessTracker {
    fn initializing() -> Self {
        Self {
            state: ReadinessState::Initializing,
            detail: Some("initialize returned; waiting for server readiness".to_string()),
            observed_readiness_signal: false,
            active_progress: HashSet::new(),
            status_authority: false,
            ready_from_status: false,
        }
    }

    fn status(&self, language: Language) -> ReadinessStatus {
        ReadinessStatus {
            language: language.language_id(),
            state: self.state,
            detail: self.detail.clone(),
        }
    }

    fn mark_ready(&mut self, detail: impl Into<String>) {
        self.state = ReadinessState::Ready;
        self.detail = Some(detail.into());
        self.active_progress.clear();
    }

    fn mark_failed(&mut self, detail: impl Into<String>) {
        self.state = ReadinessState::Failed;
        self.detail = Some(detail.into());
        self.active_progress.clear();
    }

    fn mark_indexing(&mut self, detail: impl Into<String>) {
        if self.state != ReadinessState::Ready {
            self.state = ReadinessState::Indexing;
            self.detail = Some(detail.into());
        }
    }

    fn mark_ready_from_success(&mut self, detail: impl Into<String>) {
        if self.state != ReadinessState::Ready {
            self.mark_ready(detail);
        }
    }

    fn observe_notification(&mut self, language: Language, value: &Value) {
        let Some(method) = value.get("method").and_then(Value::as_str) else {
            return;
        };
        match method {
            "$/progress" => self.observe_progress(value),
            "experimental/serverStatus" if matches!(language, Language::Rust) => {
                self.observed_readiness_signal = true;
                self.observe_rust_analyzer_status(value);
            }
            "language/status" if matches!(language, Language::Java) => {
                self.observed_readiness_signal = true;
                self.observe_jdtls_status(value);
            }
            "language/eventNotification" if matches!(language, Language::Java) => {
                self.observed_readiness_signal = true;
                if jdtls_event_type(value) == Some(100) {
                    self.status_authority = true;
                    self.mark_ready("JDTLS Gradle import finished");
                    self.ready_from_status = true;
                }
            }
            _ => {}
        }
    }

    fn observe_progress(&mut self, value: &Value) {
        let Some(params) = value.get("params") else {
            return;
        };
        let token = params
            .get("token")
            .map(progress_token_key)
            .unwrap_or_else(|| "<anonymous>".to_string());
        let Some(progress_value) = params.get("value") else {
            return;
        };
        let kind = progress_value.get("kind").and_then(Value::as_str);
        let detail =
            progress_detail(progress_value).unwrap_or_else(|| "server progress".to_string());
        match kind {
            Some("begin") => {
                self.observed_readiness_signal = true;
                self.active_progress.insert(token);
                self.mark_indexing(detail);
            }
            Some("report") => {
                self.observed_readiness_signal = true;
                self.mark_indexing(detail);
            }
            Some("end") => {
                self.observed_readiness_signal = true;
                let tracked = self.active_progress.remove(&token);
                if tracked && self.active_progress.is_empty() && !self.status_authority {
                    self.mark_ready(detail);
                }
            }
            _ => {}
        }
    }

    /// Demote a progress-induced Ready when the status channel says the
    /// server is still busy. Never demotes a status-declared Ready.
    fn demote_to_indexing(&mut self, detail: impl Into<String>) {
        if self.state == ReadinessState::Ready && !self.ready_from_status {
            self.state = ReadinessState::Indexing;
        }
        self.mark_indexing(detail);
    }

    fn observe_rust_analyzer_status(&mut self, value: &Value) {
        self.status_authority = true;
        let Some(params) = value.get("params") else {
            return;
        };

        // rust-analyzer's experimental/serverStatus payload carries
        // `{ health: "ok"|"warning"|"error", quiescent: bool, message?: string }`.
        // Older variants used `status` ("error" / ...) instead; tolerate it as
        // a fallback alias for robustness, but `health` is the canonical field.
        let health = params
            .get("health")
            .and_then(Value::as_str)
            .map(str::to_ascii_lowercase);
        let legacy_status = params.get("status").and_then(Value::as_str);
        let is_error = health.as_deref() == Some("error")
            || legacy_status
                .map(|s| s.eq_ignore_ascii_case("error"))
                .unwrap_or(false);
        let is_warning = health.as_deref() == Some("warning");

        // health == "error" wins over quiescence: a server that reports
        // quiescent alongside an error is still failed, not ready.
        if is_error {
            let detail = params
                .get("message")
                .and_then(Value::as_str)
                .map(|message| format!("rust-analyzer reported error: {message}"))
                .unwrap_or_else(|| "rust-analyzer reported error".to_string());
            self.mark_failed(detail);
            return;
        }

        let quiescent = params
            .get("quiescent")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if quiescent {
            self.mark_ready("rust-analyzer reported quiescent");
            self.ready_from_status = true;
            return;
        }

        // health == "warning": do not fail. Record the message when we are
        // not yet Ready; never demote an existing status-declared Ready.
        if is_warning {
            if self.state != ReadinessState::Ready {
                let detail = params
                    .get("message")
                    .and_then(Value::as_str)
                    .map(|message| format!("rust-analyzer reported warning: {message}"))
                    .unwrap_or_else(|| "rust-analyzer reported warning".to_string());
                self.mark_indexing(detail);
            }
            return;
        }

        self.demote_to_indexing("rust-analyzer reported busy");
    }

    fn observe_jdtls_status(&mut self, value: &Value) {
        self.status_authority = true;
        let status_type = value
            .get("params")
            .and_then(|p| p.get("type"))
            .and_then(Value::as_str);
        match status_type {
            Some(status @ ("Started" | "ServiceReady")) => {
                self.mark_ready(format!("JDTLS language/status {status}"));
                self.ready_from_status = true;
            }
            Some("Error") => self.mark_failed("JDTLS language/status Error"),
            Some(status @ ("Starting" | "Busy")) => {
                self.demote_to_indexing(format!("JDTLS language/status {status}"));
            }
            Some("Message") => {
                self.mark_indexing("JDTLS language/status Message");
            }
            Some(other) => self.mark_indexing(format!("JDTLS language/status {other:?}")),
            None => {}
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReadyWait {
    Ready,
    ProbeAllowed,
}

#[derive(Clone, Debug)]
struct OpenDocumentState {
    version: i32,
}

#[derive(Clone, Debug)]
struct DiagnosticSnapshot {
    version: i32,
    diagnostics: Vec<Diagnostic>,
}

impl SessionPool {
    pub fn new(config: LspConfig) -> Self {
        let pool = Self {
            inner: Arc::new(PoolInner {
                config,
                sessions: Mutex::new(HashMap::new()),
                evictor_started: AtomicBool::new(false),
            }),
        };
        pool.spawn_evictor_if_possible();
        pool
    }

    pub fn config(&self) -> &LspConfig {
        &self.inner.config
    }

    pub async fn status(
        &self,
        root: impl Into<PathBuf>,
        language: Language,
    ) -> Result<ReadinessStatus> {
        let root = root.into();
        let root = root
            .canonicalize()
            .with_context(|| format!("canonicalizing project root {}", root.display()))?;
        let key = SessionKey { root, language };
        let sessions = self.inner.sessions.lock().await;
        let Some(session) = sessions.get(&key).cloned() else {
            return Ok(ReadinessStatus {
                language: language.language_id(),
                state: ReadinessState::NotStarted,
                detail: Some("no pooled language server session".to_string()),
            });
        };
        drop(sessions);
        let session = session.lock().await;
        Ok(session.readiness.status(language))
    }

    pub async fn open_document(
        &self,
        root: impl Into<PathBuf>,
        language: Language,
        path: impl Into<PathBuf>,
        version: i32,
        text: String,
    ) -> Result<OpenDocument> {
        let root = root.into();
        let root = root
            .canonicalize()
            .with_context(|| format!("canonicalizing project root {}", root.display()))?;
        let path = path.into();
        let path = path
            .canonicalize()
            .with_context(|| format!("canonicalizing document path {}", path.display()))?;
        let uri = path_to_uri(&path)?;
        let session = self.session(root.clone(), language).await?;
        let mut session = session.lock().await;
        session.last_used = Instant::now();
        if session.documents.contains_key(&uri) {
            return Err(Error::DocumentAlreadyOpen { uri });
        }
        session
            .send_notification::<DidOpenTextDocument>(&DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: language.language_id().to_string(),
                    version,
                    text,
                },
            })
            .await?;
        session
            .documents
            .insert(uri.clone(), OpenDocumentState { version });
        Ok(OpenDocument {
            root,
            language,
            uri,
            version,
        })
    }

    pub async fn apply_change(
        &self,
        doc: &mut OpenDocument,
        version: i32,
        text: String,
    ) -> Result<()> {
        let session = self.session(doc.root.clone(), doc.language).await?;
        let mut session = session.lock().await;
        session.last_used = Instant::now();
        let Some(state) = session.documents.get(&doc.uri) else {
            return Err(Error::DocumentNotOpen {
                uri: doc.uri.clone(),
            });
        };
        if version <= state.version {
            return Err(Error::InvalidVersion {
                uri: doc.uri.clone(),
                current_version: state.version,
                new_version: version,
            });
        }
        session
            .send_notification::<DidChangeTextDocument>(&DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier::new(doc.uri.clone(), version),
                content_changes: vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text,
                }],
            })
            .await?;
        session
            .documents
            .insert(doc.uri.clone(), OpenDocumentState { version });
        doc.version = version;
        Ok(())
    }

    pub async fn diagnostics(&self, doc: &OpenDocument, version: i32) -> Result<Vec<Diagnostic>> {
        self.diagnostics_for_uri(doc.root.clone(), doc.language, doc.uri.clone(), version)
            .await
    }

    pub async fn diagnostics_for_uri(
        &self,
        root: impl Into<PathBuf>,
        language: Language,
        uri: Url,
        version: i32,
    ) -> Result<Vec<Diagnostic>> {
        let root = root.into();
        let root = root
            .canonicalize()
            .with_context(|| format!("canonicalizing project root {}", root.display()))?;
        let session = self.session(root, language).await?;
        let mut session = session.lock().await;
        session.last_used = Instant::now();
        session.ensure_current_version(&uri, version)?;
        if session.supports_pull_diagnostics() {
            session.pull_diagnostics(uri, version).await
        } else {
            session.drain_published_diagnostics(uri, version).await
        }
    }

    /// `textDocument/rename` → the server-authored [`lsp_types::WorkspaceEdit`].
    ///
    /// Retries while the server is still warming (rust-analyzer answers
    /// `ContentModified` (-32801) or retrigger-flagged `ServerCancelled`
    /// (-32802) until the workspace is indexed), bounded by the configured
    /// request timeout. Fails closed on an unavailable server — callers
    /// (RX-V3) must never downgrade to a syntax-only approximation.
    pub async fn rename(
        &self,
        doc: &OpenDocument,
        position: lsp_types::Position,
        new_name: impl Into<String>,
        wait_ready: Duration,
    ) -> Result<lsp_types::WorkspaceEdit> {
        let session = self.session(doc.root.clone(), doc.language).await?;
        let mut session = session.lock().await;
        session.last_used = Instant::now();
        if !session.documents.contains_key(&doc.uri) {
            return Err(Error::DocumentNotOpen {
                uri: doc.uri.clone(),
            });
        }
        session
            .wait_until_ready(
                doc.language,
                <lsp_types::request::Rename as Request>::METHOD,
                wait_ready,
            )
            .await?;
        let params = lsp_types::RenameParams {
            text_document_position: lsp_types::TextDocumentPositionParams {
                text_document: TextDocumentIdentifier {
                    uri: doc.uri.clone(),
                },
                position,
            },
            new_name: new_name.into(),
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
        };
        let deadline = Instant::now() + session.request_timeout;
        loop {
            match session
                .send_request::<lsp_types::request::Rename>(&params)
                .await
            {
                Ok(Some(edit)) => {
                    // Older server builds may never emit the readiness signal
                    // we watch. A real edit-producing response proves this
                    // session can answer authoritatively, so it can promote the
                    // session to ready instead of leaving later calls blocked.
                    session
                        .readiness
                        .mark_ready_from_success("rename returned workspace edits");
                    return Ok(edit);
                }
                Ok(None) => {
                    return Err(Error::Server {
                        method: <lsp_types::request::Rename as Request>::METHOD.to_string(),
                        error: serde_json::json!({
                            "message": "server returned no rename edits (symbol not renameable at this position)"
                        }),
                    });
                }
                Err(Error::Server { method, error })
                    if method == <lsp_types::request::Rename as Request>::METHOD
                        && should_retry_while_warming(&error)
                        && Instant::now() < deadline =>
                {
                    time::sleep(Duration::from_millis(100)).await;
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// `workspace/willRenameFiles` -> server-authored edits that must be
    /// applied before the client performs the file rename.
    pub async fn will_rename_files(
        &self,
        root: impl Into<PathBuf>,
        language: Language,
        renames: Vec<(PathBuf, PathBuf)>,
    ) -> Result<Option<lsp_types::WorkspaceEdit>> {
        let requested_root = root.into();
        let root = requested_root
            .canonicalize()
            .with_context(|| format!("canonicalizing project root {}", requested_root.display()))?;
        let mut files = Vec::new();
        for (old_path, new_path) in renames {
            let old_path = old_path
                .canonicalize()
                .with_context(|| format!("canonicalizing rename source {}", old_path.display()))?;
            let new_path = if new_path.exists() {
                new_path.canonicalize().with_context(|| {
                    format!("canonicalizing rename target {}", new_path.display())
                })?
            } else if new_path.is_absolute() {
                match new_path.strip_prefix(&requested_root) {
                    Ok(rel) => absolutize_under_root(&root, rel)?,
                    Err(_) => absolutize_under_root(&root, &new_path)?,
                }
            } else {
                absolutize_under_root(&root, &new_path)?
            };
            files.push(FileRename {
                old_uri: path_to_uri(&old_path)?.to_string(),
                new_uri: path_to_uri(&new_path)?.to_string(),
            });
        }
        let session = self.session(root, language).await?;
        let mut session = session.lock().await;
        session.last_used = Instant::now();
        let params = RenameFilesParams { files };
        let deadline = Instant::now() + session.request_timeout;
        loop {
            match session
                .send_request::<lsp_types::request::WillRenameFiles>(&params)
                .await
            {
                Ok(edit) => return Ok(edit),
                Err(Error::Server { method, error })
                    if method == <lsp_types::request::WillRenameFiles as Request>::METHOD
                        && should_retry_while_warming(&error)
                        && Instant::now() < deadline =>
                {
                    time::sleep(Duration::from_millis(100)).await;
                }
                Err(err) => return Err(err),
            }
        }
    }

    pub async fn execute_command(
        &self,
        root: impl Into<PathBuf>,
        language: Language,
        command: impl Into<String>,
        arguments: Vec<Value>,
    ) -> Result<Option<Value>> {
        let root = root.into();
        let root = root
            .canonicalize()
            .with_context(|| format!("canonicalizing project root {}", root.display()))?;
        let session = self.session(root, language).await?;
        let mut session = session.lock().await;
        session.last_used = Instant::now();
        let params = ExecuteCommandParams {
            command: command.into(),
            arguments,
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
        };
        let deadline = Instant::now() + session.request_timeout;
        loop {
            match session
                .send_request::<lsp_types::request::ExecuteCommand>(&params)
                .await
            {
                Ok(result) => return Ok(result),
                Err(Error::Server { method, error })
                    if method == <lsp_types::request::ExecuteCommand as Request>::METHOD
                        && should_retry_while_warming(&error)
                        && Instant::now() < deadline =>
                {
                    time::sleep(Duration::from_millis(100)).await;
                }
                Err(err) => return Err(err),
            }
        }
    }

    pub async fn request_raw(
        &self,
        root: impl Into<PathBuf>,
        language: Language,
        method: impl Into<String>,
        params: Value,
    ) -> Result<Value> {
        let root = root.into();
        let root = root
            .canonicalize()
            .with_context(|| format!("canonicalizing project root {}", root.display()))?;
        let session = self.session(root, language).await?;
        let mut session = session.lock().await;
        session.last_used = Instant::now();
        let method = method.into();
        let deadline = Instant::now() + session.request_timeout;
        loop {
            match session.send_raw_request(&method, &params).await {
                Ok(result) => return Ok(result),
                Err(Error::Server {
                    method: err_method,
                    error,
                }) if err_method == method
                    && should_retry_while_warming(&error)
                    && Instant::now() < deadline =>
                {
                    time::sleep(Duration::from_millis(100)).await;
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// `textDocument/hover` at `position` — authoritative resolved info for
    /// the symbol under the cursor. This is the seam the harness uses for
    /// Java `var` resolution: JDTLS resolves cross-file receiver return
    /// types and generic parameters (e.g. jOOQ `Table<R>.newRecord()` → `R`)
    /// that the pure-bytes facts module cannot. Retries on the same warming
    /// codes as `rename`. `None` is a legitimate result (no hover info at
    /// the position), not an error — callers distinguish it from failure.
    pub async fn hover(
        &self,
        doc: &OpenDocument,
        position: lsp_types::Position,
        wait_ready: Duration,
    ) -> Result<Option<lsp_types::Hover>> {
        let session = self.session(doc.root.clone(), doc.language).await?;
        let mut session = session.lock().await;
        session.last_used = Instant::now();
        if !session.documents.contains_key(&doc.uri) {
            return Err(Error::DocumentNotOpen {
                uri: doc.uri.clone(),
            });
        }
        let ready_wait = session
            .wait_until_ready(
                doc.language,
                <lsp_types::request::HoverRequest as Request>::METHOD,
                wait_ready,
            )
            .await?;
        let params = lsp_types::HoverParams {
            text_document_position_params: lsp_types::TextDocumentPositionParams {
                text_document: TextDocumentIdentifier {
                    uri: doc.uri.clone(),
                },
                position,
            },
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
        };
        let deadline = Instant::now() + session.request_timeout;
        loop {
            match session
                .send_request::<lsp_types::request::HoverRequest>(&params)
                .await
            {
                Ok(hover) => {
                    let non_empty = hover
                        .as_ref()
                        .map(|h| !hover_text_is_empty(&h.contents))
                        .unwrap_or(false);
                    if non_empty {
                        // Older server builds may never emit the readiness
                        // signal we watch. Only non-empty hover contents prove
                        // readiness; an empty cold JDTLS hover is the gap this
                        // guard prevents.
                        session
                            .readiness
                            .mark_ready_from_success("hover returned non-empty contents");
                    } else if ready_wait == ReadyWait::ProbeAllowed {
                        return Err(Error::NotReady {
                            state: session.readiness.state,
                            detail: Some(
                                "no readiness signal observed; hover returned empty contents"
                                    .to_string(),
                            ),
                            waited: wait_ready,
                        });
                    }
                    return Ok(hover);
                }
                Err(Error::Server { method, error })
                    if method == <lsp_types::request::HoverRequest as Request>::METHOD
                        && should_retry_while_warming(&error)
                        && Instant::now() < deadline =>
                {
                    time::sleep(Duration::from_millis(100)).await;
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// `textDocument/references` at `position` — authoritative project-wide
    /// find-usages for the symbol under the cursor. JDTLS symmetric, RA
    /// primary. Returns an empty `Vec` (not an error) when the server has
    /// no references at the position; callers distinguish empty from
    /// failure by `is_err`. `include_declaration` toggles the LSP
    /// `context.includeDeclaration` knob. Retries on the same warming codes
    /// as `rename`/`hover`. Fails closed on an unavailable server — callers
    /// (RX-V3) must never downgrade to a syntax-only approximation.
    pub async fn references(
        &self,
        doc: &OpenDocument,
        position: lsp_types::Position,
        include_declaration: bool,
        wait_ready: Duration,
    ) -> Result<Vec<lsp_types::Location>> {
        let session = self.session(doc.root.clone(), doc.language).await?;
        let mut session = session.lock().await;
        session.last_used = Instant::now();
        if !session.documents.contains_key(&doc.uri) {
            return Err(Error::DocumentNotOpen {
                uri: doc.uri.clone(),
            });
        }
        session
            .wait_until_ready(
                doc.language,
                <lsp_types::request::References as Request>::METHOD,
                wait_ready,
            )
            .await?;
        let params = lsp_types::ReferenceParams {
            text_document_position: lsp_types::TextDocumentPositionParams {
                text_document: TextDocumentIdentifier {
                    uri: doc.uri.clone(),
                },
                position,
            },
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
            partial_result_params: PartialResultParams {
                partial_result_token: None,
            },
            context: lsp_types::ReferenceContext {
                include_declaration,
            },
        };
        let deadline = Instant::now() + session.request_timeout;
        loop {
            match session
                .send_request::<lsp_types::request::References>(&params)
                .await
            {
                Ok(locations) => {
                    // As with rename: a real edit/answer-producing response
                    // proves this session can answer authoritatively.
                    session
                        .readiness
                        .mark_ready_from_success("references returned a result");
                    return Ok(locations.unwrap_or_default());
                }
                Err(Error::Server { method, error })
                    if method == <lsp_types::request::References as Request>::METHOD
                        && should_retry_while_warming(&error)
                        && Instant::now() < deadline =>
                {
                    time::sleep(Duration::from_millis(100)).await;
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// `textDocument/codeAction` — retrieve code actions (assists, quick-fixes,
    /// refactors) at a given range. Readiness-gated (RX-V3 fail-closed); bounded
    /// (callers cap the returned list). Returns the raw `CodeActionOrCommand`
    /// list; filtering, snippet-guard, and command-guard belong to the harness
    /// binding.
    pub async fn code_action(
        &self,
        doc: &OpenDocument,
        range: lsp_types::Range,
        wait_ready: Duration,
    ) -> Result<Vec<lsp_types::CodeActionOrCommand>> {
        let session = self.session(doc.root.clone(), doc.language).await?;
        let mut session = session.lock().await;
        session.last_used = Instant::now();
        if !session.documents.contains_key(&doc.uri) {
            return Err(Error::DocumentNotOpen {
                uri: doc.uri.clone(),
            });
        }
        session
            .wait_until_ready(
                doc.language,
                <lsp_types::request::CodeActionRequest as Request>::METHOD,
                wait_ready,
            )
            .await?;
        let params = lsp_types::CodeActionParams {
            text_document: TextDocumentIdentifier {
                uri: doc.uri.clone(),
            },
            range,
            context: lsp_types::CodeActionContext {
                diagnostics: Vec::new(),
                ..Default::default()
            },
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
            partial_result_params: PartialResultParams {
                partial_result_token: None,
            },
        };
        let deadline = Instant::now() + session.request_timeout;
        loop {
            match session
                .send_request::<lsp_types::request::CodeActionRequest>(&params)
                .await
            {
                Ok(actions) => {
                    session
                        .readiness
                        .mark_ready_from_success("codeAction returned code actions");
                    return Ok(actions.unwrap_or_default());
                }
                Err(Error::Server { method, error })
                    if method
                        == <lsp_types::request::CodeActionRequest as Request>::METHOD
                        && should_retry_while_warming(&error)
                        && Instant::now() < deadline =>
                {
                    time::sleep(Duration::from_millis(100)).await;
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// `codeAction/resolve` — resolve a partially-populated `CodeAction` to
    /// its full form (with a complete `WorkspaceEdit`). Readiness-gated;
    /// fail-closed (RX-V3).
    pub async fn code_action_resolve(
        &self,
        root: impl Into<PathBuf>,
        language: Language,
        action: lsp_types::CodeAction,
    ) -> Result<lsp_types::CodeAction> {
        let root = root.into();
        let root = root.canonicalize().with_context(|| {
            format!("canonicalizing project root {}", root.display())
        })?;
        let session = self.session(root, language).await?;
        let mut session = session.lock().await;
        session.last_used = Instant::now();
        session
            .wait_until_ready(
                language,
                <lsp_types::request::CodeActionResolveRequest as Request>::METHOD,
                Duration::from_secs(30),
            )
            .await?;
        let params = action;
        let deadline = Instant::now() + session.request_timeout;
        loop {
            match session
                .send_request::<lsp_types::request::CodeActionResolveRequest>(&params)
                .await
            {
                Ok(resolved) => {
                    session
                        .readiness
                        .mark_ready_from_success("codeAction/resolve returned resolved action");
                    return Ok(resolved);
                }
                Err(Error::Server { method, error })
                    if method
                        == <lsp_types::request::CodeActionResolveRequest as Request>::METHOD
                        && should_retry_while_warming(&error)
                        && Instant::now() < deadline =>
                {
                    time::sleep(Duration::from_millis(100)).await;
                }
                Err(err) => return Err(err),
            }
        }
    }

    pub async fn shutdown_all(&self) {
        let mut sessions = self.inner.sessions.lock().await;
        let drained = sessions
            .drain()
            .map(|(_, session)| session)
            .collect::<Vec<_>>();
        drop(sessions);
        for session in drained {
            let mut session = session.lock().await;
            let _ = shutdown_session(&mut session).await;
        }
    }

    async fn session(&self, root: PathBuf, language: Language) -> Result<Arc<Mutex<Session>>> {
        self.spawn_evictor_if_possible();
        let key = SessionKey { root, language };
        let mut sessions = self.inner.sessions.lock().await;
        if let Some(session) = sessions.get(&key) {
            return Ok(session.clone());
        }
        let session = spawn_session(&key, &self.inner.config).await?;
        let session = Arc::new(Mutex::new(session));
        sessions.insert(key, session.clone());
        Ok(session)
    }

    fn spawn_evictor_if_possible(&self) {
        if self.inner.evictor_started.swap(true, Ordering::Relaxed) {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            self.inner.evictor_started.store(false, Ordering::Relaxed);
            return;
        };
        let inner = Arc::downgrade(&self.inner);
        let tick = self.inner.config.evict_tick;
        handle.spawn(async move {
            loop {
                time::sleep(tick).await;
                let Some(inner) = inner.upgrade() else {
                    return;
                };
                let idle = inner.config.idle_timeout;
                let now = Instant::now();
                let stale = {
                    let sessions = inner.sessions.lock().await;
                    sessions
                        .iter()
                        .filter_map(|(key, session)| {
                            session.try_lock().ok().and_then(|session| {
                                (now.duration_since(session.last_used) > idle).then(|| key.clone())
                            })
                        })
                        .collect::<Vec<_>>()
                };
                for key in stale {
                    if let Some(session) = inner.sessions.lock().await.remove(&key)
                        && let Ok(mut session) = session.try_lock()
                    {
                        let _ = shutdown_session(&mut session).await;
                    }
                }
            }
        });
    }
}

impl Default for SessionPool {
    fn default() -> Self {
        Self::new(LspConfig::default())
    }
}

impl Session {
    async fn wait_until_ready(
        &mut self,
        language: Language,
        method: &'static str,
        wait_ready: Duration,
    ) -> Result<ReadyWait> {
        if wait_ready.is_zero() || self.readiness.state == ReadinessState::Ready {
            return Ok(ReadyWait::Ready);
        }
        if self.readiness.state == ReadinessState::Failed {
            return Err(Error::NotReady {
                state: self.readiness.state,
                detail: self.readiness.detail.clone(),
                waited: Duration::ZERO,
            });
        }
        let deadline = Instant::now() + wait_ready;
        loop {
            if self.readiness.state == ReadinessState::Ready {
                return Ok(ReadyWait::Ready);
            }
            if self.readiness.state == ReadinessState::Failed || Instant::now() >= deadline {
                if !self.readiness.observed_readiness_signal
                    && self.readiness.state != ReadinessState::Failed
                {
                    return Ok(ReadyWait::ProbeAllowed);
                }
                return Err(Error::NotReady {
                    state: self.readiness.state,
                    detail: self
                        .readiness
                        .detail
                        .clone()
                        .or_else(|| Some(method.to_string())),
                    waited: wait_ready,
                });
            }
            let value = match self.read_message_before(deadline).await {
                Ok(value) => value,
                Err(_err) if Instant::now() >= deadline => {
                    if !self.readiness.observed_readiness_signal {
                        return Ok(ReadyWait::ProbeAllowed);
                    }
                    return Err(Error::NotReady {
                        state: self.readiness.state,
                        detail: self
                            .readiness
                            .detail
                            .clone()
                            .or_else(|| Some(method.to_string())),
                        waited: wait_ready,
                    });
                }
                Err(err) => return Err(err),
            };
            if server_request_method(&value).is_some() {
                self.answer_server_request(&value).await?;
                continue;
            }
            if let Some(id) = value.get("id").and_then(Value::as_i64) {
                self.pending.insert(id, value);
                continue;
            }
            let _ = self.handle_notification_for_language(language, value)?;
        }
    }

    fn ensure_current_version(&self, uri: &Url, requested_version: i32) -> Result<()> {
        let current_version = self.documents.get(uri).map(|doc| doc.version);
        if current_version == Some(requested_version) {
            Ok(())
        } else {
            Err(Error::Superseded {
                uri: uri.clone(),
                requested_version,
                current_version,
            })
        }
    }

    fn supports_pull_diagnostics(&self) -> bool {
        self.capabilities.capabilities.diagnostic_provider.is_some()
    }

    fn diagnostic_identifier(&self) -> Option<String> {
        match self
            .capabilities
            .capabilities
            .diagnostic_provider
            .as_ref()?
        {
            DiagnosticServerCapabilities::Options(options) => options.identifier.clone(),
            DiagnosticServerCapabilities::RegistrationOptions(options) => {
                options.diagnostic_options.identifier.clone()
            }
        }
    }

    async fn pull_diagnostics(&mut self, uri: Url, version: i32) -> Result<Vec<Diagnostic>> {
        let params = DocumentDiagnosticParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            identifier: self.diagnostic_identifier(),
            previous_result_id: self.result_ids.get(&uri).cloned(),
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
            partial_result_params: PartialResultParams {
                partial_result_token: None,
            },
        };
        let deadline = Instant::now() + self.request_timeout;
        let response = loop {
            self.ensure_current_version(&uri, version)?;
            match self
                .send_request::<DocumentDiagnosticRequest>(&params)
                .await
            {
                Ok(response) => break response,
                Err(Error::Server { method, error })
                    if method == DocumentDiagnosticRequest::METHOD
                        && should_retrigger_diagnostic_request(&error)
                        && Instant::now() < deadline =>
                {
                    time::sleep(Duration::from_millis(100)).await;
                }
                Err(err) => return Err(err),
            }
        };
        self.ensure_current_version(&uri, version)?;
        match response {
            DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Full(report)) => {
                let report = report.full_document_diagnostic_report;
                if let Some(result_id) = report.result_id {
                    self.result_ids.insert(uri.clone(), result_id);
                }
                let diagnostics = report.items;
                self.pulled.insert(
                    uri.clone(),
                    DiagnosticSnapshot {
                        version,
                        diagnostics: diagnostics.clone(),
                    },
                );
                self.ensure_current_version(&uri, version)?;
                Ok(diagnostics)
            }
            DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Unchanged(report)) => {
                self.result_ids.insert(
                    uri.clone(),
                    report.unchanged_document_diagnostic_report.result_id,
                );
                let Some(snapshot) = self.pulled.get(&uri).cloned() else {
                    return Err(
                        anyhow!("server returned unchanged diagnostics without a cache").into(),
                    );
                };
                let diagnostics = snapshot.diagnostics;
                self.pulled.insert(
                    uri.clone(),
                    DiagnosticSnapshot {
                        version,
                        diagnostics: diagnostics.clone(),
                    },
                );
                self.ensure_current_version(&uri, version)?;
                Ok(diagnostics)
            }
            DocumentDiagnosticReportResult::Partial(_) => {
                Err(anyhow!("partial diagnostic responses are not supported").into())
            }
        }
    }

    async fn drain_published_diagnostics(
        &mut self,
        uri: Url,
        version: i32,
    ) -> Result<Vec<Diagnostic>> {
        if let Some(snapshot) = self.published.get(&uri)
            && snapshot.version == version
        {
            return Ok(snapshot.diagnostics.clone());
        }
        let deadline = Instant::now() + self.request_timeout;
        loop {
            self.ensure_current_version(&uri, version)?;
            let value = self.read_message_before(deadline).await?;
            if server_request_method(&value).is_some() {
                self.answer_server_request(&value).await?;
                continue;
            }
            if let Some(id) = value.get("id").and_then(Value::as_i64) {
                self.pending.insert(id, value);
                continue;
            }
            let Some((published_uri, snapshot)) =
                self.handle_notification_for_language(self.language, value)?
            else {
                continue;
            };
            if published_uri != uri {
                continue;
            }
            if snapshot.version == version {
                return Ok(snapshot.diagnostics);
            }
            if snapshot.version > version {
                return Err(Error::Superseded {
                    uri,
                    requested_version: version,
                    current_version: Some(snapshot.version),
                });
            }
        }
    }

    async fn send_request<R: Request>(&mut self, params: &R::Params) -> Result<R::Result> {
        let id = self.next_id();
        write_request(&mut self.stdin, R::METHOD, id, params)
            .await
            .with_context(|| format!("sending request {}", R::METHOD))?;
        let value = self.read_response(R::METHOD, id).await?;
        let result = value
            .get("result")
            .ok_or_else(|| anyhow!("LSP response for {} missing result", R::METHOD))?;
        serde_json::from_value(result.clone())
            .with_context(|| format!("decoding LSP result for {}", R::METHOD))
            .map_err(Into::into)
    }

    async fn send_raw_request(&mut self, method: &str, params: &Value) -> Result<Value> {
        let id = self.next_id();
        write_request(&mut self.stdin, method, id, params)
            .await
            .with_context(|| format!("sending request {method}"))?;
        let value = self.read_response(method, id).await?;
        value
            .get("result")
            .cloned()
            .ok_or_else(|| anyhow!("LSP response for {method} missing result"))
            .map_err(Into::into)
    }

    async fn send_notification<N: Notification>(&mut self, params: &N::Params) -> Result<()> {
        write_notification(&mut self.stdin, N::METHOD, params)
            .await
            .with_context(|| format!("sending notification {}", N::METHOD))
            .map_err(Into::into)
    }

    fn next_id(&mut self) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    async fn read_response(&mut self, method: &str, id: i64) -> Result<Value> {
        if let Some(value) = self.pending.remove(&id) {
            return response_or_error(method, value);
        }
        let deadline = Instant::now() + self.request_timeout;
        loop {
            let value = self.read_message_before(deadline).await?;
            if server_request_method(&value).is_some() {
                self.answer_server_request(&value).await?;
                continue;
            }
            match value.get("id").and_then(Value::as_i64) {
                Some(other) if other == id => return response_or_error(method, value),
                Some(other) => {
                    self.pending.insert(other, value);
                }
                None => {
                    let _ = self.handle_notification_for_language(self.language, value)?;
                }
            }
        }
    }

    async fn read_message_before(&mut self, deadline: Instant) -> Result<Value> {
        let now = Instant::now();
        if now >= deadline {
            return Err(anyhow!("timed out waiting for LSP message").into());
        }
        let remaining = deadline.duration_since(now);
        time::timeout(remaining, read_message(&mut self.stdout))
            .await
            .map_err(|_| anyhow!("timed out waiting for LSP message"))?
            .map_err(Into::into)
    }

    /// Answer a server-initiated request so the server can proceed and the
    /// id never enters our response correlation. Known housekeeping requests
    /// get their protocol-shaped acks; anything else gets MethodNotFound.
    async fn answer_server_request(&mut self, value: &Value) -> Result<()> {
        let id = value.get("id").cloned().unwrap_or(Value::Null);
        let method = value.get("method").and_then(Value::as_str).unwrap_or("");
        let response = match method {
            "workspace/configuration" => {
                let count = value
                    .pointer("/params/items")
                    .and_then(Value::as_array)
                    .map(|items| items.len())
                    .unwrap_or(1);
                serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": vec![Value::Null; count] })
            }
            "window/workDoneProgress/create"
            | "client/registerCapability"
            | "client/unregisterCapability"
            | "window/showMessageRequest" => {
                serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": Value::Null })
            }
            "workspace/applyEdit" => {
                serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": { "applied": false } })
            }
            other => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32601,
                    "message": format!("bro-lsp client does not handle {other}"),
                }
            }),
        };
        write_message(&mut self.stdin, &response)
            .await
            .map_err(Into::into)
    }

    fn handle_notification_for_language(
        &mut self,
        language: Language,
        value: Value,
    ) -> Result<Option<(Url, DiagnosticSnapshot)>> {
        self.readiness.observe_notification(language, &value);
        if value.get("method").and_then(Value::as_str) != Some("textDocument/publishDiagnostics") {
            return Ok(None);
        }
        let params = value
            .get("params")
            .cloned()
            .ok_or_else(|| anyhow!("publishDiagnostics missing params"))?;
        let params: PublishDiagnosticsParams =
            serde_json::from_value(params).context("decoding publishDiagnostics params")?;
        let current_version = self
            .documents
            .get(&params.uri)
            .map(|doc| doc.version)
            .unwrap_or_default();
        let version = params.version.unwrap_or(current_version);
        let snapshot = DiagnosticSnapshot {
            version,
            diagnostics: params.diagnostics,
        };
        self.published.insert(params.uri.clone(), snapshot.clone());
        Ok(Some((params.uri, snapshot)))
    }
}

fn response_or_error(method: &str, value: Value) -> Result<Value> {
    if let Some(error) = value.get("error") {
        return Err(Error::Server {
            method: method.to_string(),
            error: error.clone(),
        });
    }
    Ok(value)
}

/// A message carrying BOTH `id` and `method` is a server-initiated request,
/// never a response. Server request ids are an independent numbering that
/// collides with client ids, so parking one in `pending` (or matching it as
/// a response) corrupts correlation: advertising `window.workDoneProgress`
/// made rust-analyzer send `window/workDoneProgress/create` and the very
/// next client request read it back as "missing result".
fn server_request_method(value: &Value) -> Option<&str> {
    if value.get("id").is_some() {
        value.get("method").and_then(Value::as_str)
    } else {
        None
    }
}

/// Whether a request error means "the server is still warming — try again":
/// `ContentModified` (-32801, rust-analyzer while indexing) or a
/// retrigger-flagged `ServerCancelled` (-32802).
fn should_retry_while_warming(error: &Value) -> bool {
    error.get("code").and_then(Value::as_i64) == Some(-32801)
        || should_retrigger_diagnostic_request(error)
}

fn should_retrigger_diagnostic_request(error: &Value) -> bool {
    error.get("code").and_then(Value::as_i64) == Some(-32802)
        && error
            .get("data")
            .and_then(|data| data.get("retriggerRequest"))
            .and_then(Value::as_bool)
            != Some(false)
}

fn progress_token_key(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string())
}

fn progress_detail(value: &Value) -> Option<String> {
    value
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| value.get("title").and_then(Value::as_str))
        .map(str::to_string)
}

fn jdtls_event_type(value: &Value) -> Option<u64> {
    value
        .get("params")
        .and_then(|p| p.get("eventType"))
        .and_then(Value::as_u64)
}

fn hover_text_is_empty(contents: &lsp_types::HoverContents) -> bool {
    use lsp_types::HoverContents;
    match contents {
        HoverContents::Scalar(s) => marked_string_is_empty(s),
        HoverContents::Array(items) => items.iter().all(marked_string_is_empty),
        HoverContents::Markup(markup) => markup.value.trim().is_empty(),
    }
}

fn marked_string_is_empty(value: &lsp_types::MarkedString) -> bool {
    match value {
        lsp_types::MarkedString::String(value) => value.trim().is_empty(),
        lsp_types::MarkedString::LanguageString(value) => value.value.trim().is_empty(),
    }
}

fn format_wait_duration(duration: Duration) -> String {
    if duration.as_millis() % 1000 == 0 {
        format!("{}s", duration.as_secs())
    } else {
        format!("{}ms", duration.as_millis())
    }
}

// rust-analyzer build data on lane checkouts: host target dir + shim-stripped
// spawn env. See design/refactor-tools/rust/rust-isolate-surface.md §8.6.
//
// On a lane checkout the cwd-keyed shim routes rust-analyzer's `cargo
// metadata` / `cargo check` children back into the Linux pod, and the
// pod-built `target/` holds ELF proc-macro `.so` files the host proc-macro
// server cannot load. Scrubbing the spawn env fixes both knobs (which cargo
// runs, where artifacts land) with no server-side surgery.

/// Detect a lane host-build session: the root canonicalizes under `~/lanes/`,
/// or `BRO_LSP_RA_HOST_BUILD=1` is set (covers non-lane NFS/sshfs layouts).
/// Rust-only at the call site; the flag is read here so detection stays
/// pure and unit-testable.
fn is_lane_host_build(root: &Path) -> bool {
    if let Some(value) = std::env::var("BRO_LSP_RA_HOST_BUILD")
        .ok()
        .map(|v| v.trim().to_string())
        && !value.is_empty()
        && value != "0"
    {
        return true;
    }
    let Some(home) = dirs::home_dir() else {
        return false;
    };
    let Ok(home) = home.canonicalize() else {
        return false;
    };
    let lanes_root = home.join("lanes");
    let Ok(root_canon) = root.canonicalize() else {
        return false;
    };
    root_canon.starts_with(&lanes_root)
}

/// Lane shim dir to strip from the child PATH. Default `~/.lane/shims`,
/// overridable via `BRO_LSP_LANE_SHIM_DIR`. Canonicalized when it exists so
/// the PATH-element comparison survives symlinks; falls back to the raw path
/// when it does not (the dir may legitimately be absent).
fn lane_shim_dir() -> PathBuf {
    let raw = env_path("BRO_LSP_LANE_SHIM_DIR")
        .or_else(|| dirs::home_dir().map(|h| h.join(".lane").join("shims")))
        .unwrap_or_else(|| PathBuf::from("~/.lane/shims"));
    raw.canonicalize().unwrap_or(raw)
}

/// Host-local per-root target dir for the lane host build.
/// `BRO_LSP_RA_TARGET_DIR` if set, else
/// `~/.cache/blackbox/ra-target/<sha256(canonical root)[:16]>`. Creates the
/// dir if missing. `root` is canonicalized before hashing so a checkout
/// reached via two paths lands on the same dir.
fn lane_host_build_target_dir(root: &Path) -> Result<PathBuf> {
    if let Some(explicit) = env_path("BRO_LSP_RA_TARGET_DIR") {
        if !explicit.exists() {
            // One-shot mkdir of a small cache leaf during session spawn; not a
            // sustained I/O loop, so the tokio-worker blocking-I/O lint does
            // not apply to this setup step.
            #[allow(clippy::disallowed_methods)]
            std::fs::create_dir_all(&explicit)
                .with_context(|| format!("creating ra target dir {}", explicit.display()))?;
        }
        return Ok(explicit);
    }
    let root_canon = root
        .canonicalize()
        .with_context(|| format!("canonicalizing session root {}", root.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(root_canon.as_os_str().to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    let cache = dirs::cache_dir()
        .ok_or_else(|| anyhow!("could not resolve host cache dir for ra target"))?;
    let target = cache.join("blackbox").join("ra-target").join(&hex);
    if !target.exists() {
        // One-shot mkdir of a small cache leaf during session spawn; not a
        // sustained I/O loop, so the tokio-worker blocking-I/O lint does
        // not apply to this setup step.
        #[allow(clippy::disallowed_methods)]
        std::fs::create_dir_all(&target)
            .with_context(|| format!("creating ra target dir {}", target.display()))?;
    }
    Ok(target)
}

/// Pure env-scrub for the lane host-build spawn. Given the inherited env
/// (`base_env`), returns a new env with:
/// - the lane shim dir removed from PATH (other entries preserved in order);
/// - CARGO_TARGET_DIR set to `target_dir`;
/// - RUSTC_WRAPPER and RUSTC_WORKSPACE_WRAPPER removed (a sccache lane shim
///   must not re-enter the pod).
///
/// `root` and `shim_dir` are accepted for testability and future extension;
/// only `target_dir` is currently consumed beyond PATH scrubbing.
fn lane_host_build_env(
    _root: &Path,
    base_env: &[(String, String)],
    shim_dir: &Path,
    target_dir: &Path,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::with_capacity(base_env.len() + 1);
    let mut path_seen = false;
    for (key, value) in base_env {
        if key == "RUSTC_WRAPPER" || key == "RUSTC_WORKSPACE_WRAPPER" {
            continue;
        }
        if key == "PATH" {
            path_seen = true;
            let scrubbed = scrub_lane_shim_from_path(value, shim_dir);
            out.push((key.clone(), scrubbed));
            continue;
        }
        out.push((key.clone(), value.clone()));
    }
    if !path_seen {
        // PATH was absent from the inherited env; do not synthesize one.
    }
    out.push((
        "CARGO_TARGET_DIR".to_string(),
        target_dir.display().to_string(),
    ));
    out
}

/// Remove every PATH element that equals `shim_dir`, preserving the order
/// and case of the surviving entries. Both sides are compared as raw bytes;
/// callers pass already-canonicalized paths so symlink differences do not
/// leak through.
fn scrub_lane_shim_from_path(path_value: &str, shim_dir: &Path) -> String {
    let shim_str = shim_dir.as_os_str().to_string_lossy();
    let shim_str = shim_str.as_ref();
    path_value
        .split(':')
        .filter(|entry| !entry.is_empty() && *entry != shim_str)
        .collect::<Vec<_>>()
        .join(":")
}

async fn spawn_session(key: &SessionKey, config: &LspConfig) -> Result<Session> {
    let argv = key.language.launch_argv(config);
    // A failed readiness wait (or any error path after spawn) must not
    // leave an orphaned language-server child behind. Observed in
    // practice: an errored isolate session left a rust-analyzer child
    // running. kill_on_drop guarantees the child dies with the Command
    // handle, which Session owns for its whole lifetime.
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if matches!(key.language, Language::Rust) && is_lane_host_build(&key.root) {
        let shim_dir = lane_shim_dir();
        let target_dir = lane_host_build_target_dir(&key.root)?;
        let base_env: Vec<(String, String)> = std::env::vars().collect();
        let scrubbed = lane_host_build_env(&key.root, &base_env, &shim_dir, &target_dir);
        command.env_clear().envs(scrubbed);
    }
    let mut child = command.spawn().map_err(|err| Error::LspUnavailable {
        language: key.language,
        command: argv.join(" "),
        source: err.to_string(),
    })?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("language server stdin unavailable"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("language server stdout unavailable"))?;
    let mut session = Session {
        child,
        stdin,
        stdout: BufReader::new(stdout),
        next_id: 1,
        pending: HashMap::new(),
        documents: HashMap::new(),
        published: HashMap::new(),
        pulled: HashMap::new(),
        result_ids: HashMap::new(),
        capabilities: InitializeResult {
            capabilities: Default::default(),
            server_info: None,
        },
        request_timeout: config.request_timeout,
        last_used: Instant::now(),
        readiness: ReadinessTracker::initializing(),
        language: key.language,
    };
    let init_id = session.next_id();
    let init_params = build_init_params(&key.root, key.language, config)?;
    write_request(
        &mut session.stdin,
        Initialize::METHOD,
        init_id,
        &init_params,
    )
    .await
    .context("sending initialize")?;
    let init_value = session
        .read_response_with_timeout(Initialize::METHOD, init_id, config.init_timeout)
        .await?;
    let init_result = init_value
        .get("result")
        .cloned()
        .ok_or_else(|| anyhow!("initialize response missing result"))?;
    session.capabilities =
        serde_json::from_value(init_result).context("decoding initialize result")?;
    session
        .send_notification::<Initialized>(&lsp_types::InitializedParams {})
        .await?;
    if matches!(key.language, Language::Rust) {
        wait_for_rust_analyzer_ready(&mut session, config.ready_timeout).await?;
    }
    if matches!(key.language, Language::Java) {
        // JDTLS's `initialize` returns immediately, then performs gradle /
        // maven / Buildship workspace import asynchronously. Drain the early
        // notifications into readiness state, then let guarded requests wait
        // and fail closed if the workspace is still indexing.
        wait_for_jdtls_ready(&mut session, config.jdtls_ready_timeout).await;
    }
    Ok(session)
}

impl Session {
    async fn read_response_with_timeout(
        &mut self,
        method: &str,
        id: i64,
        timeout: Duration,
    ) -> Result<Value> {
        if let Some(value) = self.pending.remove(&id) {
            return response_or_error(method, value);
        }
        let deadline = Instant::now() + timeout;
        loop {
            let value = self.read_message_before(deadline).await?;
            if server_request_method(&value).is_some() {
                self.answer_server_request(&value).await?;
                continue;
            }
            match value.get("id").and_then(Value::as_i64) {
                Some(other) if other == id => return response_or_error(method, value),
                Some(other) => {
                    self.pending.insert(other, value);
                }
                None => {
                    let _ = self.handle_notification_for_language(self.language, value)?;
                }
            }
        }
    }
}

fn build_init_params(
    root: &Path,
    language: Language,
    config: &LspConfig,
) -> anyhow::Result<InitializeParams> {
    let root_uri = Url::from_directory_path(root)
        .map_err(|_| anyhow!("failed to convert {} to file URL", root.display()))?;
    // JDTLS workaround: gradle 9.x hardened configuration resolution to
    // require an exclusive lock, which trips JDTLS's injected
    // GradleAnnotationProcessorPatchPlugin during the Buildship model fetch
    // (`annotationProcessor ... without an exclusive lock`) — breaking the
    // whole gradle import and leaving the classpath unresolved. Pinning
    // `java.import.gradle.version` makes JDTLS download + import with a
    // pre-9 gradle that doesn't have the hardening. Opt-in via env so it
    // imposes nothing on projects that import cleanly on their wrapper.
    let initialization_options = match (language, config.jdtls_gradle_version.as_ref()) {
        (Language::Java, Some(gv)) => Some(serde_json::json!({
            // JDTLS reads config under initializationOptions.settings.java.*
            // (the wrapper vscode-java uses), not a bare java.* key.
            "settings": { "java": { "import": { "gradle": {
                "version": gv,
                // Force the pinned version over the project wrapper.
                "wrapper": { "enabled": false }
            } } } }
        })),
        _ => None,
    };
    Ok(InitializeParams {
        process_id: Some(std::process::id()),
        root_uri: Some(root_uri.clone()),
        initialization_options,
        capabilities: ClientCapabilities {
            workspace: Some(WorkspaceClientCapabilities {
                apply_edit: Some(true),
                workspace_edit: Some(WorkspaceEditClientCapabilities {
                    document_changes: Some(true),
                    resource_operations: Some(vec![
                        ResourceOperationKind::Create,
                        ResourceOperationKind::Rename,
                        ResourceOperationKind::Delete,
                    ]),
                    failure_handling: Some(FailureHandlingKind::Transactional),
                    ..Default::default()
                }),
                file_operations: Some(WorkspaceFileOperationsClientCapabilities {
                    will_rename: Some(true),
                    did_rename: Some(true),
                    ..Default::default()
                }),
                execute_command: Some(ExecuteCommandClientCapabilities {
                    dynamic_registration: Some(true),
                }),
                workspace_folders: Some(true),
                ..Default::default()
            }),
            text_document: Some(TextDocumentClientCapabilities {
                publish_diagnostics: Some(PublishDiagnosticsClientCapabilities {
                    related_information: Some(true),
                    version_support: Some(true),
                    code_description_support: Some(true),
                    data_support: Some(true),
                    ..Default::default()
                }),
                diagnostic: Some(DiagnosticClientCapabilities {
                    dynamic_registration: Some(false),
                    related_document_support: Some(false),
                }),
                ..Default::default()
            }),
            // Readiness signals only flow when asked for: $/progress needs
            // window.workDoneProgress, and rust-analyzer sends
            // experimental/serverStatus only when the client declares
            // serverStatusNotification. Without these the readiness tracker
            // never leaves `initializing` and request-time waits burn their
            // full window (observed as a 120s cell rename).
            window: Some(lsp_types::WindowClientCapabilities {
                work_done_progress: Some(true),
                ..Default::default()
            }),
            experimental: Some(serde_json::json!({ "serverStatusNotification": true })),
            ..Default::default()
        },
        workspace_folders: Some(vec![WorkspaceFolder {
            uri: root_uri,
            name: "bro-lsp".to_string(),
        }]),
        ..Default::default()
    })
}

fn absolutize_under_root(root: &Path, path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            std::path::Component::RootDir => out.push(component.as_os_str()),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !out.pop() {
                    return Err(anyhow!(
                        "rename target escapes filesystem root: {}",
                        path.display()
                    )
                    .into());
                }
            }
            std::path::Component::Normal(part) => out.push(part),
        }
    }
    if out.starts_with(root) {
        Ok(out)
    } else {
        Err(anyhow!(
            "rename target {} is outside project root {}",
            out.display(),
            root.display()
        )
        .into())
    }
}

async fn wait_for_rust_analyzer_ready(session: &mut Session, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() >= deadline {
            return Ok(());
        }
        let value = match session.read_message_before(deadline).await {
            Ok(value) => value,
            Err(_) => return Ok(()),
        };
        if server_request_method(&value).is_some() {
            let _ = session.answer_server_request(&value).await;
            continue;
        }
        if let Some(id) = value.get("id").and_then(Value::as_i64) {
            session.pending.insert(id, value);
            continue;
        }
        let _ = session.handle_notification_for_language(Language::Rust, value)?;
        if session.readiness.state == ReadinessState::Ready {
            return Ok(());
        }
    }
}

/// Block until JDTLS signals it has finished workspace import (or until
/// `timeout` elapses). Stashes any responses-by-id into `session.pending`
/// so a later `read_response_with_timeout` won't lose them. On timeout we
/// return and let the caller observe the degraded cold-workspace behavior,
/// matching the daemon's `bbox-lsp` policy.
async fn wait_for_jdtls_ready(session: &mut Session, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() >= deadline {
            return;
        }
        let value = match session.read_message_before(deadline).await {
            Ok(value) => value,
            Err(_) => return,
        };
        if server_request_method(&value).is_some() {
            let _ = session.answer_server_request(&value).await;
            continue;
        }
        if let Some(id) = value.get("id").and_then(Value::as_i64) {
            session.pending.insert(id, value);
            continue;
        }
        let _ = session.handle_notification_for_language(Language::Java, value);
        if session.readiness.state == ReadinessState::Ready {
            return;
        }
    }
}

async fn shutdown_session(session: &mut Session) -> anyhow::Result<()> {
    let id = session.next_id();
    let _ = write_request(&mut session.stdin, Shutdown::METHOD, id, &()).await;
    let _ = write_notification(&mut session.stdin, "exit", &()).await;
    let _ = time::timeout(Duration::from_secs(2), session.child.wait()).await;
    let _ = session.child.kill().await;
    Ok(())
}

async fn write_request<P: Serialize>(
    stdin: &mut ChildStdin,
    method: &str,
    id: i64,
    params: &P,
) -> anyhow::Result<()> {
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    write_message(stdin, &request).await
}

async fn write_notification<P: Serialize>(
    stdin: &mut ChildStdin,
    method: &str,
    params: &P,
) -> anyhow::Result<()> {
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    });
    write_message(stdin, &request).await
}

async fn write_message(stdin: &mut ChildStdin, value: &Value) -> anyhow::Result<()> {
    let body = serde_json::to_vec(value)?;
    stdin
        .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
        .await?;
    stdin.write_all(&body).await?;
    stdin.flush().await?;
    Ok(())
}

async fn read_message(reader: &mut BufReader<ChildStdout>) -> anyhow::Result<Value> {
    let mut content_length = None;
    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line).await?;
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
    let mut body = vec![0; len];
    reader.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

pub fn path_to_uri(path: &Path) -> Result<Url> {
    Url::from_file_path(path)
        .map_err(|_| anyhow!("failed to convert {} to file URL", path.display()).into())
}

pub fn uri_to_path(uri: &Url) -> Result<PathBuf> {
    uri.to_file_path()
        .map_err(|_| anyhow!("failed to convert {uri} to local path").into())
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn env_path(key: &str) -> Option<PathBuf> {
    env_string(key).map(PathBuf::from)
}

fn env_string(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_positive_timeouts() {
        let config = LspConfig::default();
        assert!(config.idle_timeout > Duration::ZERO);
        assert!(config.request_timeout > Duration::ZERO);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn references_fails_closed_when_server_unavailable() {
        // No rust-analyzer binary is reachable; the pool cannot spawn a
        // session and `references` must surface `LspUnavailable` (the
        // binding-layer `render_lsp_error` hook turns this into the
        // lsp_unavailable prefix; RX-V3 fail-closed).
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
        let pool = SessionPool::new(LspConfig {
            rust_analyzer_bin: Some(dir.path().join("missing-rust-analyzer")),
            request_timeout: Duration::from_secs(2),
            init_timeout: Duration::from_secs(2),
            ..LspConfig::default()
        });
        let doc = pool
            .open_document(
                &root,
                Language::Rust,
                &root.join("src/lib.rs"),
                1,
                "pub fn f() {}\n".to_string(),
            )
            .await
            .expect_err("expected lsp_unavailable, got success");
        assert!(
            doc.is_lsp_unavailable(),
            "expected is_lsp_unavailable=true, got {doc:?}"
        );
        pool.shutdown_all().await;
    }

    #[test]
    fn jdtls_readiness_tracks_status_and_progress_notifications() {
        let mut readiness = ReadinessTracker::initializing();
        readiness.observe_notification(
            Language::Java,
            &serde_json::json!({
                "method": "language/status",
                "params": { "type": "Starting" }
            }),
        );
        assert_eq!(readiness.state, ReadinessState::Indexing);

        readiness.observe_notification(
            Language::Java,
            &serde_json::json!({
                "method": "$/progress",
                "params": {
                    "token": "jdtls-import",
                    "value": { "kind": "begin", "title": "Importing Gradle project" }
                }
            }),
        );
        assert_eq!(readiness.state, ReadinessState::Indexing);
        readiness.observe_notification(
            Language::Java,
            &serde_json::json!({
                "method": "$/progress",
                "params": {
                    "token": "jdtls-import",
                    "value": { "kind": "end", "message": "Import finished" }
                }
            }),
        );
        // language/status was observed, so the status channel owns readiness:
        // a progress pair may no longer promote. ServiceReady does.
        assert_eq!(readiness.state, ReadinessState::Indexing);
        readiness.observe_notification(
            Language::Java,
            &serde_json::json!({
                "method": "language/status",
                "params": { "type": "ServiceReady" }
            }),
        );
        assert_eq!(readiness.state, ReadinessState::Ready);

        let mut readiness = ReadinessTracker::initializing();
        readiness.observe_notification(
            Language::Java,
            &serde_json::json!({
                "method": "language/status",
                "params": { "type": "ServiceReady" }
            }),
        );
        assert_eq!(readiness.state, ReadinessState::Ready);

        let mut readiness = ReadinessTracker::initializing();
        readiness.observe_notification(
            Language::Java,
            &serde_json::json!({
                "method": "language/eventNotification",
                "params": { "eventType": 100 }
            }),
        );
        assert_eq!(readiness.state, ReadinessState::Ready);
    }

    #[test]
    fn rust_analyzer_readiness_tracks_status_and_progress_notifications() {
        let mut readiness = ReadinessTracker::initializing();
        readiness.observe_notification(
            Language::Rust,
            &serde_json::json!({
                "method": "experimental/serverStatus",
                "params": { "quiescent": false }
            }),
        );
        assert_eq!(readiness.state, ReadinessState::Indexing);
        readiness.observe_notification(
            Language::Rust,
            &serde_json::json!({
                "method": "experimental/serverStatus",
                "params": { "quiescent": true }
            }),
        );
        assert_eq!(readiness.state, ReadinessState::Ready);

        let mut readiness = ReadinessTracker::initializing();
        readiness.observe_notification(
            Language::Rust,
            &serde_json::json!({
                "method": "$/progress",
                "params": {
                    "token": "rust-analyzer/indexing",
                    "value": { "kind": "begin", "title": "Indexing" }
                }
            }),
        );
        assert_eq!(readiness.state, ReadinessState::Indexing);
        readiness.observe_notification(
            Language::Rust,
            &serde_json::json!({
                "method": "$/progress",
                "params": {
                    "token": "rust-analyzer/indexing",
                    "value": { "kind": "end", "message": "Indexing finished" }
                }
            }),
        );
        assert_eq!(readiness.state, ReadinessState::Ready);
    }

    #[test]
    fn rust_analyzer_early_progress_pair_cannot_outrank_busy_server_status() {
        // Live regression shape: rust-analyzer emits a short-lived progress
        // stream (Fetching) that begins and ends long before quiescence. The
        // premature Ready sent a rename at ~0.2s and the server answered
        // with no edits.
        let mut readiness = ReadinessTracker::initializing();
        readiness.observe_notification(
            Language::Rust,
            &serde_json::json!({
                "method": "$/progress",
                "params": {
                    "token": "rustAnalyzer/Fetching",
                    "value": { "kind": "begin", "title": "Fetching" }
                }
            }),
        );
        readiness.observe_notification(
            Language::Rust,
            &serde_json::json!({
                "method": "$/progress",
                "params": {
                    "token": "rustAnalyzer/Fetching",
                    "value": { "kind": "end" }
                }
            }),
        );
        // No status authority yet: the progress pair legitimately promotes.
        assert_eq!(readiness.state, ReadinessState::Ready);
        // The status channel arrives and says busy: demote.
        readiness.observe_notification(
            Language::Rust,
            &serde_json::json!({
                "method": "experimental/serverStatus",
                "params": { "quiescent": false }
            }),
        );
        assert_eq!(readiness.state, ReadinessState::Indexing);
        // Later progress pairs may no longer promote past the status channel.
        readiness.observe_notification(
            Language::Rust,
            &serde_json::json!({
                "method": "$/progress",
                "params": {
                    "token": "rustAnalyzer/Roots Scanned",
                    "value": { "kind": "begin", "title": "Roots Scanned" }
                }
            }),
        );
        readiness.observe_notification(
            Language::Rust,
            &serde_json::json!({
                "method": "$/progress",
                "params": {
                    "token": "rustAnalyzer/Roots Scanned",
                    "value": { "kind": "end" }
                }
            }),
        );
        assert_eq!(readiness.state, ReadinessState::Indexing);
        // Only quiescence promotes now, and it is sticky.
        readiness.observe_notification(
            Language::Rust,
            &serde_json::json!({
                "method": "experimental/serverStatus",
                "params": { "quiescent": true }
            }),
        );
        assert_eq!(readiness.state, ReadinessState::Ready);
        readiness.observe_notification(
            Language::Rust,
            &serde_json::json!({
                "method": "experimental/serverStatus",
                "params": { "quiescent": true }
            }),
        );
        assert_eq!(readiness.state, ReadinessState::Ready);
    }

    #[test]
    fn rust_analyzer_health_error_beats_quiescent() {
        // Real rust-analyzer sends `health` (canonical) and `quiescent`.
        // Ordering bug: quiescent used to short-circuit to Ready even when
        // the server reported an error, swallowing the failure entirely.
        let mut readiness = ReadinessTracker::initializing();
        readiness.observe_notification(
            Language::Rust,
            &serde_json::json!({
                "method": "experimental/serverStatus",
                "params": {
                    "health": "error",
                    "quiescent": true,
                    "message": "cargo check failed"
                }
            }),
        );
        assert_eq!(readiness.state, ReadinessState::Failed);
        assert!(
            readiness
                .detail
                .as_deref()
                .unwrap_or_default()
                .contains("cargo check failed"),
            "failure detail should carry the server message"
        );
    }

    #[test]
    fn rust_analyzer_health_error_without_quiescent_fails() {
        let mut readiness = ReadinessTracker::initializing();
        readiness.observe_notification(
            Language::Rust,
            &serde_json::json!({
                "method": "experimental/serverStatus",
                "params": { "health": "error", "quiescent": false }
            }),
        );
        assert_eq!(readiness.state, ReadinessState::Failed);
    }

    #[test]
    fn rust_analyzer_health_warning_is_not_failure() {
        let mut readiness = ReadinessTracker::initializing();
        readiness.observe_notification(
            Language::Rust,
            &serde_json::json!({
                "method": "experimental/serverStatus",
                "params": { "health": "warning", "quiescent": false }
            }),
        );
        assert_ne!(readiness.state, ReadinessState::Failed);
        assert!(
            readiness
                .detail
                .as_deref()
                .unwrap_or_default()
                .contains("rust-analyzer reported warning"),
            "warning detail should be recorded when not yet Ready"
        );
    }

    #[test]
    fn rust_analyzer_health_warning_does_not_demote_status_ready() {
        // A status-declared Ready stays Ready under a warning: warnings are
        // advisory and never demote a quiescent-ready server.
        let mut readiness = ReadinessTracker::initializing();
        readiness.observe_notification(
            Language::Rust,
            &serde_json::json!({
                "method": "experimental/serverStatus",
                "params": { "health": "ok", "quiescent": true }
            }),
        );
        assert_eq!(readiness.state, ReadinessState::Ready);
        readiness.observe_notification(
            Language::Rust,
            &serde_json::json!({
                "method": "experimental/serverStatus",
                "params": { "health": "warning", "quiescent": false }
            }),
        );
        assert_eq!(readiness.state, ReadinessState::Ready);
    }

    #[test]
    fn rust_analyzer_health_ok_quiescent_is_ready() {
        let mut readiness = ReadinessTracker::initializing();
        readiness.observe_notification(
            Language::Rust,
            &serde_json::json!({
                "method": "experimental/serverStatus",
                "params": { "health": "ok", "quiescent": true }
            }),
        );
        assert_eq!(readiness.state, ReadinessState::Ready);
    }

    #[test]
    fn rust_analyzer_legacy_status_error_alias_still_fails() {
        // `status` is tolerated as a fallback alias for servers that predate
        // the `health` field; an error there must still mark Failed.
        let mut readiness = ReadinessTracker::initializing();
        readiness.observe_notification(
            Language::Rust,
            &serde_json::json!({
                "method": "experimental/serverStatus",
                "params": { "status": "error" }
            }),
        );
        assert_eq!(readiness.state, ReadinessState::Failed);
    }

    #[test]
    fn lane_host_build_env_strips_shim_from_path_preserving_order() {
        let tmp = tempfile::tempdir().unwrap();
        let shim = tmp.path().join("shims");
        std::fs::create_dir_all(&shim).unwrap();
        let shim = shim.canonicalize().unwrap();
        let host_cargo = tmp.path().join("host-cargo");
        std::fs::create_dir_all(&host_cargo).unwrap();
        let host_cargo = host_cargo.canonicalize().unwrap();

        let path_value = format!("{}:{}:{}", shim.display(), host_cargo.display(), "/usr/bin");
        let target = tmp.path().join("ra-target");
        std::fs::create_dir_all(&target).unwrap();
        let target = target.canonicalize().unwrap();

        let base_env: Vec<(String, String)> = vec![
            ("PATH".to_string(), path_value),
            ("HOME".to_string(), "/home/someone".to_string()),
            ("RUSTC_WRAPPER".to_string(), "sccache".to_string()),
            ("RUSTC_WORKSPACE_WRAPPER".to_string(), "sccache".to_string()),
            (
                "RUSTUP_HOME".to_string(),
                "/home/someone/.rustup".to_string(),
            ),
        ];

        let scrubbed = lane_host_build_env(tmp.path(), &base_env, &shim, &target);

        let path_entry = scrubbed
            .iter()
            .find(|(k, _)| k == "PATH")
            .expect("PATH preserved")
            .1
            .clone();
        let entries: Vec<&str> = path_entry.split(':').collect();
        assert!(
            !entries.iter().any(|e| *e == shim.to_str().unwrap()),
            "shim dir must be removed from PATH"
        );
        assert_eq!(
            entries,
            vec![host_cargo.to_str().unwrap(), "/usr/bin"],
            "non-shim entries must survive in order"
        );

        let cargo_target = scrubbed
            .iter()
            .find(|(k, _)| k == "CARGO_TARGET_DIR")
            .expect("CARGO_TARGET_DIR set");
        assert_eq!(cargo_target.1, target.display().to_string());

        assert!(
            !scrubbed.iter().any(|(k, _)| k == "RUSTC_WRAPPER"),
            "RUSTC_WRAPPER must be removed"
        );
        assert!(
            !scrubbed.iter().any(|(k, _)| k == "RUSTC_WORKSPACE_WRAPPER"),
            "RUSTC_WORKSPACE_WRAPPER must be removed"
        );
        assert!(
            scrubbed
                .iter()
                .any(|(k, v)| k == "HOME" && v == "/home/someone"),
            "unrelated env vars must survive"
        );
    }

    #[test]
    fn lane_host_build_env_handles_absent_path() {
        // If the inherited env has no PATH at all, the scrubber must not
        // synthesize one; it still sets CARGO_TARGET_DIR and strips wrappers.
        let tmp = tempfile::tempdir().unwrap();
        let shim = tmp.path().join("shims");
        std::fs::create_dir_all(&shim).unwrap();
        let shim = shim.canonicalize().unwrap();
        let target = tmp.path().join("ra-target");
        let base_env: Vec<(String, String)> = vec![
            ("HOME".to_string(), "/h".to_string()),
            ("RUSTC_WRAPPER".to_string(), "sccache".to_string()),
        ];
        let scrubbed = lane_host_build_env(tmp.path(), &base_env, &shim, &target);
        assert!(!scrubbed.iter().any(|(k, _)| k == "PATH"));
        assert!(!scrubbed.iter().any(|(k, _)| k == "RUSTC_WRAPPER"));
        assert!(scrubbed.iter().any(|(k, _)| k == "CARGO_TARGET_DIR"));
    }

    #[test]
    fn lane_host_build_env_drops_all_shim_entries() {
        // A malformed PATH that lists the shim twice must drop both copies.
        let tmp = tempfile::tempdir().unwrap();
        let shim = tmp.path().join("shims");
        std::fs::create_dir_all(&shim).unwrap();
        let shim = shim.canonicalize().unwrap();
        let path_value = format!("{}:{}:{}", shim.display(), "/usr/bin", shim.display());
        let base_env = vec![("PATH".to_string(), path_value)];
        let target = tmp.path().join("t");
        let scrubbed = lane_host_build_env(tmp.path(), &base_env, &shim, &target);
        let path_entry = scrubbed
            .iter()
            .find(|(k, _)| k == "PATH")
            .unwrap()
            .1
            .clone();
        assert_eq!(path_entry, "/usr/bin");
    }

    #[test]
    fn non_lane_root_without_env_flag_is_not_detected() {
        // A random tempdir is not under ~/lanes/ and (when the operator has
        // not opted in via BRO_LSP_RA_HOST_BUILD) must not trigger the lane
        // host-build path. Skipped if the operator has the flag set, since
        // that makes the detection true regardless of root.
        if env_string("BRO_LSP_RA_HOST_BUILD")
            .map(|v| !v.is_empty() && v != "0")
            .unwrap_or(false)
        {
            // Operator opted in globally; this assertion is meaningless.
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        assert!(!is_lane_host_build(&root));
    }

    #[test]
    fn lane_host_build_target_dir_is_per_root_and_stable() {
        // Same canonical root produces the same host target dir; two distinct
        // roots produce two distinct dirs. The directory is created on first
        // call and reused on the second (no error on the existing path).
        let tmp = tempfile::tempdir().unwrap();
        let root_a = tmp.path().join("crate-a");
        std::fs::create_dir_all(&root_a).unwrap();
        let root_a = root_a.canonicalize().unwrap();
        let root_b = tmp.path().join("crate-b");
        std::fs::create_dir_all(&root_b).unwrap();
        let root_b = root_b.canonicalize().unwrap();

        let dir_a_first = lane_host_build_target_dir(&root_a).unwrap();
        let dir_a_again = lane_host_build_target_dir(&root_a).unwrap();
        let dir_b = lane_host_build_target_dir(&root_b).unwrap();

        assert_eq!(dir_a_first, dir_a_again, "same root must hash to same dir");
        assert_ne!(dir_a_first, dir_b, "distinct roots must hash apart");
        assert!(dir_a_first.exists());
        assert!(dir_b.exists());
        assert!(
            dir_a_first.display().to_string().contains("ra-target"),
            "target dir lives under the ra-target cache namespace"
        );
    }
}
