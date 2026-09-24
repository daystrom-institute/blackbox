//! Scoped filesystem instruction discovery and delivery receipts.
//!
//! An absent `--system-prompt` enables startup discovery. Provider composition
//! decides whether those instructions enter user context or the leading system
//! section. Discovery assembles a global
//! `$CODEX_HOME/AGENTS.md` (+ `AGENTS.override.md`) followed by the repo's
//! project instruction docs walked from the git root down to the cwd. The
//! default project candidates are `AGENTS.override.md`, then `AGENTS.md`, with
//! one selected file per directory. Sandbox/beta sessions can set
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
//!
//! Every filesystem call runs through [`crate::instruction_io`] under one
//! deadline per operation. Startup discovery is best effort: a timeout discards
//! the partial result and the ledger keeps its root and global candidates
//! enrolled, so the first boundary refresh reads strictly. Refresh, resume, and
//! structured checks fail closed. Workers only compute detached scans; the
//! awaiting caller commits a scan under one short ledger critical section, and
//! only when no other ledger change landed since its snapshot.

use crate::instruction_io::{
    self, InstructionFs, InstructionTimeout, IoFailure, Phase, Probe, WorkerSlot,
};
use std::collections::{BTreeSet, HashSet};
use std::path::Component;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Large-overlay warning threshold. This never truncates instructions; it only
/// catches unexpectedly huge project-doc chains. The default must stay above
/// this repo's standard AGENTS/PROJECT/BLACKBOX hierarchy.
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
        .unwrap_or_else(default_project_doc_files)
}

fn default_project_doc_files() -> Vec<String> {
    vec![AGENTS_OVERRIDE_FILE.to_string(), AGENTS_FILE.to_string()]
}

fn selected_project_doc(
    probe: &Probe,
    directory: &Path,
    names: &[String],
) -> std::io::Result<Option<PathBuf>> {
    for name in names {
        let candidate = directory.join(name);
        match probe.metadata(&candidate) {
            Ok(metadata) if metadata.is_file => return Ok(Some(candidate)),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(None)
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

/// Session settings that shape discovery, resolved on the caller's session
/// context. Worker threads have no task-local session env, so they receive
/// this immutable copy instead of reading settings themselves.
#[derive(Debug, Clone)]
struct DiscoveryConfig {
    codex_home: Option<PathBuf>,
    names: Vec<String>,
    warn_bytes: usize,
    timeout: Duration,
}

impl DiscoveryConfig {
    fn from_session() -> Self {
        Self {
            codex_home: codex_home(),
            names: project_doc_files(),
            warn_bytes: project_doc_warn_bytes(),
            timeout: instruction_io::session_timeout(),
        }
    }
}

/// Collect repo `AGENTS.md` paths walking from `cwd` up to (and including) the
/// git root, ordered outermost-first (git root → cwd) so the most-specific doc
/// lands last. If `cwd` is not inside a git repo, only `cwd/AGENTS.md` is
/// considered — Codex does not walk arbitrarily up the filesystem outside a
/// repo.
fn project_agents_paths(probe: &Probe, cwd: &Path, names: &[String]) -> Vec<PathBuf> {
    let mut git_root: Option<PathBuf> = None;
    let mut cursor = Some(cwd);
    while let Some(d) = cursor {
        if probe.exists(&d.join(".git")) {
            git_root = Some(d.to_path_buf());
            break;
        }
        cursor = d.parent();
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
        if let Ok(Some(candidate)) = selected_project_doc(probe, &dir, names) {
            chain.push(candidate);
        }
    }
    chain
}

fn read_nonempty(probe: &Probe, path: &Path) -> Option<String> {
    match probe.read_to_string(path) {
        Ok(s) if !s.trim().is_empty() => Some(s),
        Ok(_) => None,
        Err(e) => {
            tracing::debug!(path = %path.display(), error = %e, "skip unreadable AGENTS doc");
            None
        }
    }
}

fn canonical_file(probe: &Probe, path: &Path) -> Option<PathBuf> {
    let canonical = probe.canonicalize(path).ok()?;
    probe.is_file(&canonical).then_some(canonical)
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
            | "README.md"
    ) {
        return true;
    }
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("md" | "markdown" | "txt")
    )
}

fn resolve_include(probe: &Probe, referrer: &Path, mention: &str) -> Option<PathBuf> {
    let raw = mention.strip_prefix('@')?;
    if raw.is_empty() {
        return None;
    }
    let candidate = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        referrer.parent()?.join(raw)
    };
    let canonical = canonical_file(probe, &candidate)?;
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

struct DocTree<'a> {
    probe: &'a Probe,
    loaded_paths: HashSet<PathBuf>,
    loaded: Vec<String>,
    documents: Vec<InstructionDocument>,
}

impl DocTree<'_> {
    fn read(&mut self, path: &Path, depth: usize, scope: &Path, origin: &Path) -> Vec<String> {
        if depth > MAX_INCLUDE_DEPTH {
            tracing::warn!(
                path = %path.display(),
                max_depth = MAX_INCLUDE_DEPTH,
                "skipping nested AGENTS include beyond max depth"
            );
            return Vec::new();
        }
        let Some(canonical) = canonical_file(self.probe, path) else {
            return Vec::new();
        };
        if !self.loaded_paths.insert(canonical.clone()) {
            return Vec::new();
        }
        let Some(body) = read_nonempty(self.probe, &canonical) else {
            return Vec::new();
        };
        self.loaded.push(canonical.display().to_string());
        self.documents.push(InstructionDocument::new(
            canonical.clone(),
            scope.to_path_buf(),
            origin.to_path_buf(),
            body.clone(),
        ));

        let mut sections = vec![body.clone()];
        for mention in extract_at_mentions(&body) {
            let Some(include) = resolve_include(self.probe, &canonical, &mention) else {
                continue;
            };
            sections.extend(self.read(&include, depth + 1, scope, origin));
        }
        sections
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectDocOverlay {
    pub(crate) text: String,
    pub(crate) loaded_paths: Vec<PathBuf>,
    pub(crate) documents: Vec<InstructionDocument>,
}

/// Assemble the Codex-equivalent overlay from an explicit `cwd` and
/// `codex_home`. Returns `None` when no AGENTS docs exist, in which case the
/// caller sends no system prompt (identical to the prior provider-defaults
/// behavior).
fn assemble(
    probe: &Probe,
    cwd: &Path,
    codex_home: Option<&Path>,
    project_doc_files: &[String],
    project_doc_warn_bytes: usize,
) -> Option<ProjectDocOverlay> {
    let mut sections: Vec<String> = Vec::new();
    let mut tree = DocTree {
        probe,
        loaded_paths: HashSet::new(),
        loaded: Vec::new(),
        documents: Vec::new(),
    };

    // Global scope: $CODEX_HOME/AGENTS.md (+ override), uncapped.
    if let Some(home) = codex_home {
        for name in [AGENTS_FILE, AGENTS_OVERRIDE_FILE] {
            let p = home.join(name);
            sections.extend(tree.read(&p, 0, cwd, &p));
        }
    }

    // Project scope: repo AGENTS.md, git root → cwd. Never truncate
    // instructions here; a large overlay is the operator's context decision.
    let mut project: Vec<String> = Vec::new();
    for p in project_agents_paths(probe, cwd, project_doc_files) {
        project.extend(tree.read(&p, 0, p.parent().unwrap_or(cwd), &p));
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
    let DocTree {
        loaded, documents, ..
    } = tree;
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
        documents,
    })
}

/// Exact host-discovered instruction version, with its top-level discovery
/// origin. Includes inherit scope; removals are explicit authoritative updates.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct InstructionDocument {
    pub(crate) path: PathBuf,
    pub(crate) sha256: String,
    pub(crate) scope: PathBuf,
    pub(crate) origin: PathBuf,
    pub(crate) body: String,
    pub(crate) revoked: bool,
}

impl InstructionDocument {
    fn new(path: PathBuf, scope: PathBuf, origin: PathBuf, body: String) -> Self {
        Self {
            path,
            scope,
            origin,
            sha256: bbox_refactor::sha256_hex(body.as_bytes()),
            body,
            revoked: false,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct InstructionBatch {
    pub(crate) generation: u64,
    pub(crate) text: String,
    pub(crate) documents: Vec<InstructionDocument>,
    occurrences: Vec<u64>,
}

#[derive(Debug)]
struct ActiveInstruction {
    document: InstructionDocument,
    origins: BTreeSet<PathBuf>,
    occurrence: u64,
    delivered_generation: Option<u64>,
}

#[derive(Debug, Default)]
struct InstructionLedger {
    explicit_origins: BTreeSet<(PathBuf, PathBuf)>,
    active: Vec<ActiveInstruction>,
    observed_paths: BTreeSet<PathBuf>,
    next_occurrence: u64,
    /// Advances on every ledger change. A scan commits only against the
    /// revision it was planned from.
    revision: u64,
}

impl InstructionLedger {
    fn observe(&mut self, document: InstructionDocument) {
        if let Some(active) = self.active.iter_mut().find(|active| {
            active.document.path == document.path && active.document.scope == document.scope
        }) {
            active.origins.insert(document.origin.clone());
            if active.document.sha256 != document.sha256
                || active.document.body != document.body
                || active.document.revoked != document.revoked
            {
                self.next_occurrence += 1;
                active.occurrence = self.next_occurrence;
                active.document = document;
                active.delivered_generation = None;
            }
        } else {
            self.next_occurrence += 1;
            self.active.push(ActiveInstruction {
                origins: BTreeSet::from([document.origin.clone()]),
                document,
                occurrence: self.next_occurrence,
                delivered_generation: None,
            });
        }
    }

    fn reconcile(&mut self, documents: Vec<InstructionDocument>, complete: bool) {
        if complete {
            let removed: Vec<_> = self
                .active
                .iter()
                .filter(|active| {
                    !active.document.revoked
                        && !documents.iter().any(|document| {
                            document.path == active.document.path
                                && document.scope == active.document.scope
                        })
                })
                .map(|active| InstructionDocument {
                    body: String::new(),
                    sha256: bbox_refactor::sha256_hex(b""),
                    revoked: true,
                    ..active.document.clone()
                })
                .collect();
            for document in removed {
                self.observe(document);
            }
        }
        for document in documents {
            self.observe(document);
        }
    }

    fn admit(
        &self,
        access: bro_tools::InstructionAccess,
        touched: &[PathBuf],
        generation: u64,
    ) -> Result<(), bro_tools::ToolResult> {
        if access == bro_tools::InstructionAccess::Read {
            return Ok(());
        }
        let blocked: Vec<_> = self
            .active
            .iter()
            .filter(|active| {
                touched
                    .iter()
                    .any(|path| path.starts_with(&active.document.scope))
                    && active
                        .delivered_generation
                        .is_none_or(|delivered| delivered > generation)
            })
            .map(|active| active.document.path.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if blocked.is_empty() {
            return Ok(());
        }
        Err(bro_tools::ToolResult::Error(serde_json::json!({
            "error": "instructions_required", "paths": blocked,
            "message": "No filesystem effects were performed. Covering instructions are queued for an authoritative model boundary. Retrying in this same batch or cell cannot acknowledge them."
        }).to_string()))
    }
}

/// Ledger state a scan reads, copied under a short lock.
struct ScanPlan {
    root: PathBuf,
    names: Vec<String>,
    explicit_origins: BTreeSet<(PathBuf, PathBuf)>,
    observed_paths: BTreeSet<PathBuf>,
    /// `(origin, scope)` for every origin of every active document.
    active_origins: Vec<(PathBuf, PathBuf)>,
}

impl ScanPlan {
    fn capture(root: &Path, names: &[String], ledger: &InstructionLedger) -> Self {
        Self {
            root: root.to_path_buf(),
            names: names.to_vec(),
            explicit_origins: ledger.explicit_origins.clone(),
            observed_paths: ledger.observed_paths.clone(),
            active_origins: ledger
                .active
                .iter()
                .flat_map(|active| {
                    active
                        .origins
                        .iter()
                        .map(|origin| (origin.clone(), active.document.scope.clone()))
                })
                .collect(),
        }
    }
}

/// A completed read graph. Non-empty `errors` means it is incomplete: its
/// successful observations may be applied, but it must not cause removals.
struct Scan {
    documents: Vec<InstructionDocument>,
    errors: Vec<String>,
}

/// Rebuild complete include graphs and rewalk visited ancestry. Removed edges
/// revoke old instructions; new AGENTS in already visited scopes are discovered
/// without another tool invocation. An `Err` means nothing may be reconciled.
fn scan(probe: &Probe, plan: &ScanPlan) -> Result<Scan, String> {
    let root = canonical_existing_ancestor(probe, &plan.root)?;
    let mut origins = plan.explicit_origins.clone();
    let mut directories = BTreeSet::new();
    for path in &plan.observed_paths {
        let canonical = canonical_existing_ancestor(probe, path)?;
        for touched in [path, &canonical] {
            directories.extend(instruction_ancestry(probe, &root, touched));
        }
    }
    // Restored origins preserve previously visited scopes, but project
    // candidates must be selected again: an override can appear or vanish.
    for (origin, scope) in &plan.active_origins {
        if plan
            .explicit_origins
            .contains(&(origin.clone(), scope.clone()))
        {
            continue;
        }
        if origin
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| plan.names.iter().any(|candidate| candidate == name))
        {
            if let Some(directory) = origin.parent() {
                directories.insert(directory.to_path_buf());
            }
        } else {
            origins.insert((origin.clone(), scope.clone()));
        }
    }
    for directory in directories {
        if let Some(candidate) = selected_project_doc(probe, &directory, &plan.names)
            .map_err(|error| format!("{}: {error}", directory.display()))?
        {
            origins.insert((candidate, directory));
        }
    }
    let mut documents = Vec::new();
    let mut errors = Vec::new();
    let mut origins: Vec<_> = origins.into_iter().collect();
    origins.sort_by(|left, right| {
        left.1
            .components()
            .count()
            .cmp(&right.1.components().count())
            .then_with(|| left.1.cmp(&right.1))
            .then_with(|| left.0.cmp(&right.0))
    });
    for (origin, scope) in origins {
        match probe.symlink_metadata(&origin) {
            Ok(()) => {
                if let Err(error) = read_instruction_tree(
                    probe,
                    &origin,
                    &scope,
                    &origin,
                    &mut HashSet::new(),
                    &mut documents,
                    0,
                ) {
                    errors.push(error);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => errors.push(format!("{}: {error}", origin.display())),
        }
    }
    Ok(Scan { documents, errors })
}

/// A structured tool's touched paths (lexical and canonical) plus the scan of
/// the ledger with those paths observed.
struct CheckScan {
    touched: Vec<PathBuf>,
    scan: Result<Scan, String>,
}

fn check_scan(probe: &Probe, mut plan: ScanPlan, paths: Vec<PathBuf>) -> Result<CheckScan, String> {
    let root = canonical_existing_ancestor(probe, &plan.root)?;
    let mut touched = Vec::new();
    for path in paths {
        let lexical = normalize_lexical(&if path.is_absolute() {
            path
        } else {
            root.join(path)
        });
        let canonical = canonical_existing_ancestor(probe, &lexical)?;
        touched.push(lexical);
        touched.push(canonical);
    }
    plan.observed_paths.extend(touched.iter().cloned());
    Ok(CheckScan {
        touched,
        scan: scan(probe, &plan),
    })
}

/// Session-owned version ledger. Shell and remote tools are explicit escape
/// hatches; this policy only admits tool-owned structured filesystem scopes.
#[derive(Clone)]
pub(crate) struct ScopedProjectDocs {
    root: PathBuf,
    names: Vec<String>,
    timeout: Duration,
    slot: Arc<WorkerSlot>,
    diagnostics: Option<crate::emit::Emitter>,
    ledger: Arc<std::sync::Mutex<InstructionLedger>>,
}

impl std::fmt::Debug for ScopedProjectDocs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScopedProjectDocs")
            .field("root", &self.root)
            .field("names", &self.names)
            .field("timeout", &self.timeout)
            .field("slot", &self.slot)
            .finish_non_exhaustive()
    }
}

impl Default for ScopedProjectDocs {
    fn default() -> Self {
        Self::with_names(PathBuf::from("/"), None, default_project_doc_files())
    }
}

impl ScopedProjectDocs {
    /// Apply the same startup discovery/suppression contract to every session.
    /// Settings are captured here, on the session's context. Startup discovery
    /// is best effort within the instruction budget; the root and global
    /// candidates stay enrolled either way.
    pub(crate) async fn for_session(
        root: PathBuf,
        system_prompt: Option<&str>,
        fs: Arc<dyn InstructionFs>,
        diagnostics: Option<crate::emit::Emitter>,
    ) -> Self {
        let config = DiscoveryConfig::from_session();
        let docs = Self::with_io(
            root,
            None,
            config.names.clone(),
            config.timeout,
            fs,
            diagnostics,
        );
        if system_prompt.is_some() {
            docs.suppress_startup_discovery();
            return docs;
        }
        if let Some(home) = &config.codex_home {
            docs.enroll_global_candidates_at(home);
        }
        let root = docs.root.clone();
        let startup = docs
            .bounded(
                Phase::Startup,
                move |_| (root.clone(), config.clone()),
                |probe, (root, config)| {
                    assemble(
                        probe,
                        &root,
                        config.codex_home.as_deref(),
                        &config.names,
                        config.warn_bytes,
                    )
                },
                |ledger, overlay| {
                    for document in overlay.into_iter().flat_map(|overlay| overlay.documents) {
                        ledger.observe(document);
                    }
                },
            )
            .await;
        if let Err(IoFailure::Worker(error)) = startup {
            tracing::warn!(%error, "startup instruction discovery failed");
        }
        docs
    }

    #[cfg(test)]
    pub(crate) fn new(root: PathBuf, startup: Option<&ProjectDocOverlay>) -> Self {
        Self::with_names(root, startup, project_doc_files())
    }

    fn with_names(root: PathBuf, startup: Option<&ProjectDocOverlay>, names: Vec<String>) -> Self {
        Self::with_io(
            root,
            startup,
            names,
            instruction_io::session_timeout(),
            Arc::new(instruction_io::StdFs),
            None,
        )
    }

    fn with_io(
        root: PathBuf,
        startup: Option<&ProjectDocOverlay>,
        names: Vec<String>,
        timeout: Duration,
        fs: Arc<dyn InstructionFs>,
        diagnostics: Option<crate::emit::Emitter>,
    ) -> Self {
        let mut ledger = InstructionLedger::default();
        ledger.observed_paths.insert(root.clone());
        if let Some(startup) = startup {
            for document in &startup.documents {
                ledger.observe(document.clone());
            }
        }
        Self {
            root,
            names,
            timeout,
            slot: Arc::new(WorkerSlot::new(fs)),
            diagnostics,
            ledger: Arc::new(std::sync::Mutex::new(ledger)),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, InstructionLedger> {
        self.ledger.lock().expect("instruction ledger poisoned")
    }

    /// Mutate the ledger under its lock and advance its revision.
    fn change<R>(&self, mutate: impl FnOnce(&mut InstructionLedger) -> R) -> R {
        let mut ledger = self.lock();
        let result = mutate(&mut ledger);
        ledger.revision += 1;
        result
    }

    /// Register global candidates even when discovery could not read them.
    /// An explicit system override skips this startup enrollment.
    fn enroll_global_candidates_at(&self, home: &Path) {
        self.change(|ledger| {
            for name in [AGENTS_FILE, AGENTS_OVERRIDE_FILE] {
                ledger
                    .explicit_origins
                    .insert((home.join(name), self.root.clone()));
            }
        });
    }

    /// Taking a batch grants nothing. Append its exact text to authoritative
    /// transport input before acknowledging this exact batch.
    pub(crate) fn pending_batch(&self, generation: u64) -> Option<InstructionBatch> {
        let ledger = self.lock();
        let pending: Vec<_> = ledger
            .active
            .iter()
            .filter(|active| active.delivered_generation.is_none())
            .collect();
        if pending.is_empty() {
            return None;
        }
        let documents: Vec<_> = pending
            .iter()
            .map(|active| active.document.clone())
            .collect();
        Some(InstructionBatch {
            generation,
            text: render_instruction_documents(&documents),
            documents,
            occurrences: pending.iter().map(|active| active.occurrence).collect(),
        })
    }

    /// A replacement system section needs all current versions, including
    /// explicit revocations, rather than only the newly changed documents.
    pub(crate) fn snapshot_batch(&self, generation: u64) -> InstructionBatch {
        let ledger = self.lock();
        let documents: Vec<_> = ledger
            .active
            .iter()
            .map(|active| active.document.clone())
            .collect();
        InstructionBatch {
            generation,
            text: render_instruction_documents(&documents),
            documents,
            occurrences: ledger
                .active
                .iter()
                .map(|active| active.occurrence)
                .collect(),
        }
    }

    pub(crate) fn acknowledge(&self, batch: &InstructionBatch) {
        self.change(|ledger| {
            for (document, occurrence) in batch.documents.iter().zip(&batch.occurrences) {
                if let Some(active) = ledger.active.iter_mut().find(|active| {
                    active.document.path == document.path && active.document.scope == document.scope
                }) && active.occurrence == *occurrence
                    && active.document == *document
                {
                    active.delivered_generation = Some(
                        active
                            .delivered_generation
                            .map_or(batch.generation, |previous| previous.min(batch.generation)),
                    );
                }
            }
        });
    }

    /// Compaction replaces instruction-bearing history. An acknowledgment
    /// already in flight cannot revive a version from the previous history.
    pub(crate) fn invalidate_delivery(&self) {
        self.change(|ledger| {
            for index in 0..ledger.active.len() {
                ledger.next_occurrence += 1;
                ledger.active[index].occurrence = ledger.next_occurrence;
                ledger.active[index].delivered_generation = None;
            }
        });
    }

    pub(crate) fn active_documents(&self) -> Vec<InstructionDocument> {
        let ledger = self.lock();
        ledger
            .active
            .iter()
            .flat_map(|active| {
                active
                    .origins
                    .iter()
                    .map(|origin| InstructionDocument {
                        origin: origin.clone(),
                        ..active.document.clone()
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// An explicit system prompt suppresses automatic startup discovery. Later
    /// structured file accesses still opt their actual paths into scoped checks.
    pub(crate) fn suppress_startup_discovery(&self) {
        self.change(|ledger| ledger.observed_paths.remove(&self.root));
    }

    pub(crate) fn observed_paths(&self) -> Vec<PathBuf> {
        self.lock().observed_paths.iter().cloned().collect()
    }

    pub(crate) fn restore_observed_paths(&self, paths: Vec<PathBuf>) {
        self.change(|ledger| ledger.observed_paths.extend(paths));
    }

    /// Restore graph origins/scopes only, never proof of delivery. Refresh
    /// rereads current bytes before the host prepares its first resumed request.
    pub(crate) async fn restore_documents(
        &self,
        documents: Vec<InstructionDocument>,
    ) -> Result<(), String> {
        self.change(|ledger| {
            for document in documents {
                ledger.observe(document);
            }
        });
        self.invalidate_delivery();
        self.refresh_in(Phase::Resume).await
    }

    /// Strictly reread every scope before each provider boundary.
    pub(crate) async fn refresh(&self) -> Result<(), String> {
        self.refresh_in(Phase::Refresh).await
    }

    async fn refresh_in(&self, phase: Phase) -> Result<(), String> {
        self.bounded(
            phase,
            |ledger| ScanPlan::capture(&self.root, &self.names, ledger),
            |probe, plan| scan(probe, &plan),
            |ledger, scanned| {
                let Scan { documents, errors } = scanned?;
                ledger.reconcile(documents, errors.is_empty());
                errors.into_iter().next().map_or(Ok(()), Err)
            },
        )
        .await
        .map_err(|failure| failure.message())?
    }

    /// Run one instruction operation under a single deadline. Admission, the
    /// worker's filesystem calls, and conflict retries share the budget. A
    /// scan commits only when the ledger revision still matches its plan;
    /// otherwise the result is discarded and the operation replans.
    async fn bounded<P, T, R>(
        &self,
        phase: Phase,
        plan: impl Fn(&InstructionLedger) -> P,
        job: impl Fn(&Probe, P) -> T + Clone + Send + 'static,
        mut commit: impl FnMut(&mut InstructionLedger, T) -> R,
    ) -> Result<R, IoFailure>
    where
        P: Send + 'static,
        T: Send + 'static,
    {
        let budget = self.timeout;
        let deadline = tokio::time::Instant::now() + budget;
        let outcome = async {
            let mut admission = self.slot.admit(phase, budget, deadline).await?;
            let mut conflicts = 0;
            loop {
                let (planned, revision) = {
                    let ledger = self.lock();
                    (plan(&ledger), ledger.revision)
                };
                let job = job.clone();
                let (value, returned) = self
                    .slot
                    .run(
                        admission,
                        phase,
                        budget,
                        deadline,
                        conflicts,
                        move |probe| job(probe, planned),
                    )
                    .await?;
                admission = returned;
                let mut ledger = self.lock();
                if ledger.revision == revision {
                    let result = commit(&mut ledger, value);
                    ledger.revision += 1;
                    return Ok(result);
                }
                drop(ledger);
                conflicts += 1;
                if tokio::time::Instant::now() >= deadline {
                    return Err(IoFailure::Timeout(InstructionTimeout {
                        phase,
                        budget,
                        attempt: self.slot.active_attempt(),
                        waiting_for_admission: false,
                        conflicts,
                    }));
                }
            }
        }
        .await;
        if let Err(IoFailure::Timeout(timeout)) = &outcome {
            self.report_timeout(timeout);
        }
        outcome
    }

    fn report_timeout(&self, timeout: &InstructionTimeout) {
        tracing::warn!(
            phase = timeout.phase.as_str(),
            operation = timeout.operation(),
            path = ?timeout.path(),
            budget_ms = timeout.budget_ms(),
            "{}",
            timeout.message()
        );
        if let Some(emitter) = &self.diagnostics {
            emitter.instruction_read_timeout(timeout);
        }
    }
}

#[async_trait::async_trait]
impl bro_tools::InstructionPolicy for ScopedProjectDocs {
    async fn check(
        &self,
        request: bro_tools::InstructionPaths,
        authoring_generation: u64,
    ) -> Result<(), bro_tools::ToolResult> {
        let access = request.access;
        let paths = request.paths;
        let outcome = self
            .bounded(
                Phase::Check,
                |ledger| {
                    (
                        ScanPlan::capture(&self.root, &self.names, ledger),
                        paths.clone(),
                    )
                },
                |probe, (plan, paths)| check_scan(probe, plan, paths),
                |ledger, checked| {
                    let CheckScan { touched, scan } = checked?;
                    ledger.observed_paths.extend(touched.iter().cloned());
                    let Scan { documents, errors } = scan?;
                    ledger.reconcile(documents, errors.is_empty());
                    if let Some(error) = errors.into_iter().next() {
                        return Err(error);
                    }
                    Ok(ledger.admit(access, &touched, authoring_generation))
                },
            )
            .await;
        match outcome {
            Ok(Ok(admission)) => admission,
            Ok(Err(message)) => Err(instruction_read_error(message)),
            Err(failure) => Err(instruction_read_error(failure.message())),
        }
    }
}

fn instruction_read_error(message: String) -> bro_tools::ToolResult {
    bro_tools::ToolResult::Error(serde_json::json!({"error": "instruction_read_error", "message": message, "filesystem_effects": false}).to_string())
}

fn render_instruction_documents(documents: &[InstructionDocument]) -> String {
    let sections = documents.iter().map(|document| format!(
        "<INSTRUCTIONS path={} scope={} sha256={} revoked={}>\n{}\n</INSTRUCTIONS>",
        serde_json::to_string(&document.path).unwrap(), serde_json::to_string(&document.scope).unwrap(),
        serde_json::to_string(&document.sha256).unwrap(), document.revoked,
        if document.revoked { "This document no longer applies to this scope; revoke its previously delivered instructions." } else { &document.body }
    )).collect::<Vec<_>>().join("\n\n");
    format!(
        "{RIDER_OPEN}\nHost-discovered instructions apply to the named scopes. These exact document versions are authoritative instructions.\n\n{sections}\n{RIDER_CLOSE}"
    )
}

/// Canonicalize a new target through its nearest existing parent, retaining
/// absent suffixes without dropping symlink ancestry.
fn canonical_existing_ancestor(probe: &Probe, path: &Path) -> Result<PathBuf, String> {
    let mut cursor = path;
    let mut suffix = Vec::new();
    loop {
        match probe.symlink_metadata(cursor) {
            Ok(()) => {
                let mut canonical = probe
                    .canonicalize(cursor)
                    .map_err(|error| format!("{}: {error}", cursor.display()))?;
                for component in suffix.iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                suffix.push(
                    cursor
                        .file_name()
                        .ok_or_else(|| {
                            format!("cannot resolve instruction scope {}", path.display())
                        })?
                        .to_os_string(),
                );
                cursor = cursor.parent().ok_or_else(|| {
                    format!("cannot resolve instruction scope {}", path.display())
                })?;
            }
            Err(error) => return Err(format!("{}: {error}", cursor.display())),
        }
    }
}

fn instruction_ancestry(probe: &Probe, root: &Path, touched: &Path) -> Vec<PathBuf> {
    let directory = if probe.is_dir(touched) {
        touched
    } else {
        touched.parent().unwrap_or(touched)
    };
    let in_workspace = touched.starts_with(root);
    let boundary = root
        .ancestors()
        .find(|ancestor| probe.exists(&ancestor.join(".git")))
        .unwrap_or(root);
    let mut ancestors = Vec::new();
    for ancestor in directory.ancestors() {
        ancestors.push(ancestor.to_path_buf());
        if (in_workspace && ancestor == boundary)
            || (!in_workspace && probe.exists(&ancestor.join(".git")))
        {
            break;
        }
    }
    ancestors.reverse();
    ancestors
}

/// Missing optional AGENTS candidates are filtered by the caller; a selected
/// document or allowed explicit include must be readable.
fn read_instruction_tree(
    probe: &Probe,
    path: &Path,
    scope: &Path,
    origin: &Path,
    visited: &mut HashSet<PathBuf>,
    documents: &mut Vec<InstructionDocument>,
    depth: usize,
) -> Result<(), String> {
    let canonical = probe
        .canonicalize(path)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    if !visited.insert(canonical.clone()) {
        return Ok(());
    }
    if depth > MAX_INCLUDE_DEPTH {
        return Err(format!(
            "{}: instruction include depth exceeds {MAX_INCLUDE_DEPTH}",
            path.display()
        ));
    }
    let body = probe
        .read_to_string(&canonical)
        .map_err(|error| format!("{}: {error}", canonical.display()))?;
    documents.push(InstructionDocument::new(
        canonical.clone(),
        scope.to_path_buf(),
        origin.to_path_buf(),
        body.clone(),
    ));
    for mention in extract_at_mentions(&body) {
        let raw = mention.strip_prefix('@').unwrap_or(&mention);
        let candidate = if Path::new(raw).is_absolute() {
            PathBuf::from(raw)
        } else {
            canonical.parent().unwrap().join(raw)
        };
        if is_allowed_instruction_doc(&candidate) {
            read_instruction_tree(
                probe,
                &candidate,
                scope,
                origin,
                visited,
                documents,
                depth + 1,
            )?;
        }
    }
    Ok(())
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

#[cfg(test)]
#[path = "project_doc_bounded_tests.rs"]
mod bounded_tests;

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
        default_project_doc_files()
    }

    fn assemble(
        cwd: &Path,
        codex_home: Option<&Path>,
        project_doc_files: &[String],
        project_doc_warn_bytes: usize,
    ) -> Option<ProjectDocOverlay> {
        super::assemble(
            &Probe::std(),
            cwd,
            codex_home,
            project_doc_files,
            project_doc_warn_bytes,
        )
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
    fn startup_selects_one_project_candidate_per_directory_and_keeps_global_additive() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let project = root.join("project");
        let home = root.join("home");
        write(&project.join(".git/HEAD"), "ref: refs/heads/main\n");
        write(&project.join("AGENTS.md"), "SHADOWED_ROOT");
        write(&project.join("AGENTS.override.md"), "ROOT_OVERRIDE");
        write(&project.join("child/AGENTS.override.md"), "CHILD_OVERRIDE");
        write(&home.join("AGENTS.md"), "GLOBAL_BASE");
        write(&home.join("AGENTS.override.md"), "GLOBAL_ADDITION");
        let overlay = assemble(
            &project.join("child"),
            Some(&home),
            &default_docs(),
            usize::MAX,
        )
        .unwrap();
        let bodies: Vec<_> = overlay
            .documents
            .iter()
            .map(|document| document.body.as_str())
            .collect();
        assert_eq!(
            bodies,
            vec![
                "GLOBAL_BASE",
                "GLOBAL_ADDITION",
                "ROOT_OVERRIDE",
                "CHILD_OVERRIDE"
            ]
        );

        // Explicit alternate filenames replace the default candidate list.
        write(&project.join("RULES_PRIMARY.md"), "PRIMARY");
        write(&project.join("RULES_FALLBACK.md"), "FALLBACK");
        let names = vec!["RULES_PRIMARY.md".into(), "RULES_FALLBACK.md".into()];
        let overlay = assemble(&project, None, &names, usize::MAX).unwrap();
        assert_eq!(overlay.documents.len(), 1);
        assert_eq!(overlay.documents[0].body, "PRIMARY");
    }

    #[tokio::test]
    async fn override_appearance_and_removal_revoke_prior_candidates_and_gate_effects() {
        use bro_tools::{InstructionAccess, InstructionPolicy};
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        write(&root.join(".git/HEAD"), "ref: refs/heads/main\n");
        write(&root.join("AGENTS.md"), "BASE_RULE");
        write(&root.join("child/AGENTS.md"), "CHILD_BASE_RULE");
        let startup = assemble(&root.join("child"), None, &default_docs(), usize::MAX).unwrap();
        let ledger =
            ScopedProjectDocs::with_names(root.join("child"), Some(&startup), default_docs());
        ledger.refresh().await.unwrap();
        let initial = ledger.pending_batch(1).unwrap();
        assert_eq!(initial.documents.len(), 2);
        ledger.acknowledge(&initial);
        let target = root.join("child/value.txt");
        ledger
            .check(request(target.clone(), InstructionAccess::Mutate), 1)
            .await
            .unwrap();

        write(&root.join("AGENTS.override.md"), "OVERRIDE_RULE");
        assert!(
            ledger
                .check(request(target.clone(), InstructionAccess::Mutate), 1)
                .await
                .is_err()
        );
        let replacement = ledger.pending_batch(2).unwrap();
        assert!(
            replacement
                .documents
                .iter()
                .any(|doc| doc.path == root.join("AGENTS.md") && doc.revoked)
        );
        assert!(
            replacement
                .documents
                .iter()
                .any(|doc| doc.body == "OVERRIDE_RULE" && !doc.revoked)
        );
        assert!(
            !replacement
                .documents
                .iter()
                .any(|doc| doc.body == "CHILD_BASE_RULE")
        );
        ledger.acknowledge(&replacement);
        ledger
            .check(request(target.clone(), InstructionAccess::Mutate), 2)
            .await
            .unwrap();
        ledger.refresh().await.unwrap();
        assert!(
            ledger.pending_batch(3).is_none(),
            "old origin must not revive the shadowed candidate"
        );

        fs::remove_file(root.join("AGENTS.override.md")).unwrap();
        assert!(
            ledger
                .check(request(target.clone(), InstructionAccess::Mutate), 2)
                .await
                .is_err()
        );
        let fallback = ledger.pending_batch(3).unwrap();
        assert!(
            fallback
                .documents
                .iter()
                .any(|doc| doc.path == root.join("AGENTS.override.md") && doc.revoked)
        );
        assert!(
            fallback
                .documents
                .iter()
                .any(|doc| doc.body == "BASE_RULE" && !doc.revoked)
        );
        ledger.acknowledge(&fallback);
        ledger
            .check(request(target, InstructionAccess::Mutate), 3)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn refreshing_global_override_preserves_additive_scope() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let project = root.join("project");
        let home = root.join("home");
        fs::create_dir(&project).unwrap();
        write(&home.join("AGENTS.md"), "GLOBAL_BASE");
        write(&home.join("AGENTS.override.md"), "GLOBAL_ADDITION");
        let startup = assemble(&project, Some(&home), &default_docs(), usize::MAX).unwrap();
        let ledger = ScopedProjectDocs::with_names(project.clone(), Some(&startup), default_docs());
        ledger.enroll_global_candidates_at(&home);
        ledger.refresh().await.unwrap();
        let batch = ledger.pending_batch(1).unwrap();
        assert_eq!(batch.documents.len(), 2);
        assert!(
            batch
                .documents
                .iter()
                .all(|doc| doc.scope == project && !doc.revoked)
        );
        assert!(batch.text.contains("GLOBAL_BASE"));
        assert!(batch.text.contains("GLOBAL_ADDITION"));
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

    fn guarded_context(
        root: &Path,
        ledger: std::sync::Arc<ScopedProjectDocs>,
        generation: u64,
    ) -> bro_tools::ToolCx {
        bro_tools::ToolCx {
            tool_observations: Default::default(),
            instruction_generation: generation,
            instruction_policy: Some(ledger),
            root: root.to_path_buf(),
            cancellation: Default::default(),
            output_budget: 16 * 1024,
            safety: std::sync::Arc::new(bro_tools::SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: Default::default(),
            shell_sessions: Default::default(),
            edits: Default::default(),
            session_env: Default::default(),
            child_env: Default::default(),
            shell_env: Default::default(),
            tool_arg_defaults: Default::default(),
        }
    }

    fn request(path: PathBuf, access: bro_tools::InstructionAccess) -> bro_tools::InstructionPaths {
        bro_tools::InstructionPaths {
            paths: vec![path],
            access,
        }
    }

    fn scoped(root: &Path) -> std::sync::Arc<ScopedProjectDocs> {
        std::sync::Arc::new(ScopedProjectDocs::with_names(
            root.to_path_buf(),
            None,
            default_docs(),
        ))
    }

    #[tokio::test]
    async fn read_then_mutation_waits_for_delivery_and_old_cell_never_borrows_it() {
        use bro_tools::{InstructionPolicy, Tool};
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        write(&root.join("child/AGENTS.md"), "CHILD-DOC exact body\n");
        write(&root.join("child/file.txt"), "before\n");
        let ledger = scoped(&root);
        let cx = guarded_context(&root, ledger.clone(), 1);
        let read = bro_tools::file_read::FileRead;
        let result = bro_tools::call_tool_with_arg_defaults(
            &read,
            read.name(),
            serde_json::json!({"file_path":"child/file.txt"}),
            &cx,
        )
        .await;
        assert!(!result.is_error());
        let batch = ledger.pending_batch(2).unwrap();
        assert!(batch.text.contains("CHILD-DOC exact body\n"));
        let edit = bro_tools::workspace::FileEdit;
        let args = serde_json::json!({"file_path":"child/file.txt", "old_string":"before", "new_string":"after"});
        for _ in 0..2 {
            let result =
                bro_tools::call_tool_with_arg_defaults(&edit, edit.name(), args.clone(), &cx).await;
            assert!(
                matches!(result, bro_tools::ToolResult::Error(error) if error.contains("instructions_required"))
            );
            assert_eq!(fs::read(root.join("child/file.txt")).unwrap(), b"before\n");
        }
        ledger.acknowledge(&batch);
        assert!(
            ledger
                .check(
                    request(
                        root.join("child/file.txt"),
                        bro_tools::InstructionAccess::Mutate
                    ),
                    1
                )
                .await
                .is_err()
        );
        let mut next = cx.clone();
        next.instruction_generation = 2;
        let result = bro_tools::call_tool_with_arg_defaults(&edit, edit.name(), args, &next).await;
        assert!(!result.is_error(), "{result:?}");
        assert_eq!(fs::read(root.join("child/file.txt")).unwrap(), b"after\n");
    }

    #[tokio::test]
    async fn first_create_and_failed_read_queue_docs_without_effects() {
        use bro_tools::Tool;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        write(
            &root.join("child/AGENTS.md"),
            "Create only after reading me.",
        );
        let ledger = scoped(&root);
        let cx = guarded_context(&root, ledger.clone(), 1);
        let read = bro_tools::file_read::FileRead;
        let result = bro_tools::call_tool_with_arg_defaults(
            &read,
            read.name(),
            serde_json::json!({"file_path":"child/missing/file.txt"}),
            &cx,
        )
        .await;
        assert!(result.is_error());
        assert!(ledger.pending_batch(2).is_some());
        let write = bro_tools::workspace::FileWrite;
        let result = bro_tools::call_tool_with_arg_defaults(
            &write,
            write.name(),
            serde_json::json!({"file_path":"child/missing/file.txt", "content":"new"}),
            &cx,
        )
        .await;
        assert!(
            matches!(result, bro_tools::ToolResult::Error(error) if error.contains("instructions_required"))
        );
        assert!(!root.join("child/missing").exists());
    }

    #[tokio::test]
    async fn host_default_path_is_guarded_before_write() {
        use bro_tools::Tool;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        write(&root.join("child/AGENTS.md"), "Nested scope.");
        let ledger = scoped(&root);
        let mut cx = guarded_context(&root, ledger.clone(), 1);
        cx.tool_arg_defaults = std::sync::Arc::new(
            bro_tools::ToolArgDefaults::parse_map(std::collections::BTreeMap::from([(
                "default:file_write.file_path".into(),
                "child/new.txt".into(),
            )]))
            .unwrap(),
        );
        let tool = bro_tools::workspace::FileWrite;
        let result = bro_tools::call_tool_with_arg_defaults(
            &tool,
            tool.name(),
            serde_json::json!({"content":"new"}),
            &cx,
        )
        .await;
        assert!(
            matches!(result, bro_tools::ToolResult::Error(error) if error.contains("instructions_required"))
        );
        assert!(!root.join("child/new.txt").exists());
    }

    #[tokio::test]
    async fn patch_move_checks_destination_before_any_hunk_effects() {
        use bro_tools::Tool;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        write(&root.join("source.txt"), "old\n");
        write(
            &root.join("destination/AGENTS.md"),
            "Destination instructions.",
        );
        let ledger = scoped(&root);
        let mut cx = guarded_context(&root, ledger.clone(), 1);
        let tool = bro_tools::workspace::ApplyPatch;
        let args = serde_json::json!({"source":"*** Begin Patch\n*** Add File: first.txt\n+first\n*** Update File: source.txt\n*** Move to: destination/new.txt\n@@\n-old\n+new\n*** End Patch"});
        let result =
            bro_tools::call_tool_with_arg_defaults(&tool, tool.name(), args.clone(), &cx).await;
        assert!(
            matches!(result, bro_tools::ToolResult::Error(error) if error.contains("instructions_required"))
        );
        assert!(!root.join("first.txt").exists());
        assert!(!root.join("destination/new.txt").exists());
        assert_eq!(fs::read(root.join("source.txt")).unwrap(), b"old\n");
        ledger.acknowledge(&ledger.pending_batch(2).unwrap());
        cx.instruction_generation = 2;
        let result = bro_tools::call_tool_with_arg_defaults(&tool, tool.name(), args, &cx).await;
        assert!(!result.is_error(), "{result:?}");
        assert_eq!(
            fs::read(root.join("destination/new.txt")).unwrap(),
            b"new\n"
        );
    }

    #[tokio::test]
    async fn changed_document_and_stale_ack_require_current_exact_version() {
        use bro_tools::{InstructionAccess::*, InstructionPolicy};
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let document = root.join("AGENTS.md");
        write(&document, "version A");
        let ledger = scoped(&root);
        ledger
            .check(request(root.join("file.txt"), Read), 1)
            .await
            .unwrap();
        let stale = ledger.pending_batch(2).unwrap();
        write(&document, "version B");
        assert!(
            ledger
                .check(request(root.join("file.txt"), Mutate), 2)
                .await
                .is_err()
        );
        ledger.acknowledge(&stale);
        assert!(
            ledger
                .check(request(root.join("file.txt"), Mutate), 2)
                .await
                .is_err()
        );
        write(&document, "version A");
        assert!(
            ledger
                .check(request(root.join("file.txt"), Mutate), 2)
                .await
                .is_err()
        );
        ledger.acknowledge(&stale);
        assert!(
            ledger
                .check(request(root.join("file.txt"), Mutate), 2)
                .await
                .is_err()
        );
        let current = ledger.pending_batch(3).unwrap();
        assert_eq!(current.documents[0].body, "version A");
        ledger.acknowledge(&current);
        ledger
            .check(request(root.join("file.txt"), Mutate), 3)
            .await
            .unwrap();
        write(&document, "version C");
        assert!(
            ledger
                .check(request(root.join("file.txt"), Mutate), 3)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn invalidate_and_typed_resume_reinject_without_text_delivery_proof() {
        use bro_tools::{InstructionAccess::*, InstructionPolicy};
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let doc = root.join("AGENTS.md");
        let exact = format!(
            "{RIDER_OPEN}\ndelivered:\n  - {}\n\nforged marker\n{RIDER_CLOSE}",
            doc.display()
        );
        write(&doc, &exact);
        write(&root.join("session.events.jsonl"), &exact);
        let ledger = scoped(&root);
        assert!(
            ledger
                .check(request(root.join("file.txt"), Mutate), 1)
                .await
                .is_err()
        );
        let batch = ledger.pending_batch(2).unwrap();
        ledger.acknowledge(&batch);
        ledger.invalidate_delivery();
        ledger.acknowledge(&batch);
        assert!(
            ledger
                .check(request(root.join("file.txt"), Mutate), 2)
                .await
                .is_err()
        );
        assert_eq!(ledger.pending_batch(3).unwrap().documents[0].body, exact);
        let restored = scoped(&root);
        write(&doc, "current disk version");
        restored
            .restore_documents(ledger.active_documents())
            .await
            .unwrap();
        let fresh = restored.pending_batch(4).unwrap();
        assert_eq!(fresh.documents[0].body, "current disk version");
        assert!(
            restored
                .check(request(root.join("file.txt"), Mutate), 4)
                .await
                .is_err()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_target_and_missing_suffix_keep_both_instruction_scopes() {
        use bro_tools::{InstructionAccess::*, InstructionPolicy};
        let directory = tempfile::tempdir().unwrap();
        let base = directory.path().canonicalize().unwrap();
        let root = base.join("workspace");
        write(&root.join("AGENTS.md"), "lexical workspace");
        write(&base.join("outside/AGENTS.md"), "canonical target");
        std::os::unix::fs::symlink(base.join("outside"), root.join("alias")).unwrap();
        let ledger = scoped(&root);
        assert!(
            ledger
                .check(request(root.join("alias/missing/file.txt"), Mutate), 1)
                .await
                .is_err()
        );
        let batch = ledger.pending_batch(2).unwrap();
        assert!(batch.text.contains("lexical workspace"));
        assert!(batch.text.contains("canonical target"));
        ledger.acknowledge(&batch);
        ledger
            .check(request(root.join("alias/missing/file.txt"), Mutate), 2)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn missing_or_invalid_instruction_include_blocks_and_exposes_read_error() {
        use bro_tools::{InstructionAccess::*, InstructionPolicy};
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        write(&root.join("AGENTS.md"), "Read @rules.md before editing.");
        let ledger = scoped(&root);
        let failure = ledger
            .check(request(root.join("new.txt"), Mutate), 1)
            .await
            .unwrap_err();
        assert!(
            matches!(failure, bro_tools::ToolResult::Error(error) if error.contains("instruction_read_error") && error.contains("rules.md"))
        );
        write(&root.join("rules.md"), "Included rules.");
        assert!(
            ledger
                .check(request(root.join("new.txt"), Mutate), 1)
                .await
                .is_err()
        );
        ledger.acknowledge(&ledger.pending_batch(2).unwrap());
        ledger
            .check(request(root.join("new.txt"), Mutate), 2)
            .await
            .unwrap();
        fs::write(root.join("rules.md"), [0xff]).unwrap();
        assert!(
            ledger
                .check(request(root.join("new.txt"), Mutate), 2)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn removing_include_or_document_revokes_without_permanent_read_failure() {
        use bro_tools::{InstructionAccess::*, InstructionPolicy};
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        write(&root.join("AGENTS.md"), "Read @rules.md.");
        write(&root.join("rules.md"), "Old included rule.");
        let ledger = scoped(&root);
        ledger
            .check(request(root.join("file.txt"), Read), 1)
            .await
            .unwrap();
        ledger.acknowledge(&ledger.pending_batch(2).unwrap());
        write(&root.join("AGENTS.md"), "Include was removed.");
        ledger.refresh().await.unwrap();
        let batch = ledger.pending_batch(3).unwrap();
        assert!(
            batch
                .documents
                .iter()
                .any(|doc| doc.path == root.join("rules.md") && doc.revoked)
        );
        assert!(!batch.text.contains("Old included rule."));
        ledger
            .check(request(root.join("file.txt"), Read), 2)
            .await
            .unwrap();
        assert!(
            ledger
                .check(request(root.join("file.txt"), Mutate), 2)
                .await
                .is_err()
        );
        ledger.acknowledge(&batch);
        ledger
            .check(request(root.join("file.txt"), Mutate), 3)
            .await
            .unwrap();
        fs::remove_file(root.join("AGENTS.md")).unwrap();
        ledger.refresh().await.unwrap();
        let batch = ledger.pending_batch(4).unwrap();
        assert!(
            batch
                .documents
                .iter()
                .any(|doc| doc.path == root.join("AGENTS.md") && doc.revoked)
        );
        ledger.acknowledge(&batch);
        ledger
            .check(request(root.join("file.txt"), Mutate), 4)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn boundary_refresh_finds_new_documents_in_prior_empty_scopes() {
        use bro_tools::{InstructionAccess::*, InstructionPolicy};
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let ledger = scoped(&root);
        ledger
            .check(request(root.join("child/future/file.txt"), Read), 1)
            .await
            .unwrap();
        assert!(ledger.pending_batch(2).is_none());
        let restored = scoped(&root);
        restored.restore_observed_paths(ledger.observed_paths());
        write(&root.join("child/AGENTS.md"), "New scope.");
        restored.refresh().await.unwrap();
        assert_eq!(
            restored.pending_batch(2).unwrap().documents[0].body,
            "New scope."
        );
        assert!(
            restored
                .check(request(root.join("child/future/file.txt"), Mutate), 1)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn parallel_mutations_cannot_deliver_each_others_instructions() {
        use bro_tools::Tool;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        write(&root.join("child/AGENTS.md"), "Parallel scope.");
        let ledger = scoped(&root);
        let cx = guarded_context(&root, ledger.clone(), 1);
        let tool = bro_tools::workspace::FileWrite;
        let (first, second) = tokio::join!(
            bro_tools::call_tool_with_arg_defaults(
                &tool,
                tool.name(),
                serde_json::json!({"file_path":"child/a.txt", "content":"a"}),
                &cx
            ),
            bro_tools::call_tool_with_arg_defaults(
                &tool,
                tool.name(),
                serde_json::json!({"file_path":"child/b.txt", "content":"b"}),
                &cx
            ),
        );
        assert!(first.is_error());
        assert!(second.is_error());
        assert!(!root.join("child/a.txt").exists());
        assert!(!root.join("child/b.txt").exists());
        assert_eq!(ledger.pending_batch(2).unwrap().documents.len(), 1);
    }
    #[tokio::test]
    async fn global_candidates_are_strict_even_when_initial_discovery_skipped_them() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let home = root.join("global");
        std::fs::create_dir(&home).unwrap();
        std::fs::write(home.join(AGENTS_FILE), [0xff, 0xfe]).unwrap();
        let ledger = ScopedProjectDocs::with_names(root.clone(), None, vec![AGENTS_FILE.into()]);
        ledger.enroll_global_candidates_at(&home);
        assert!(ledger.refresh().await.is_err());
        std::fs::write(home.join(AGENTS_FILE), "STRICT_GLOBAL").unwrap();
        ledger.refresh().await.unwrap();
        assert!(
            ledger
                .pending_batch(1)
                .unwrap()
                .text
                .contains("STRICT_GLOBAL")
        );
        let other = root.join("later-global");
        std::fs::create_dir(&other).unwrap();
        ledger.enroll_global_candidates_at(&other);
        ledger.refresh().await.unwrap();
        std::fs::write(other.join(AGENTS_OVERRIDE_FILE), "NEW_GLOBAL").unwrap();
        ledger.refresh().await.unwrap();
        assert!(ledger.pending_batch(2).unwrap().text.contains("NEW_GLOBAL"));
    }
}
