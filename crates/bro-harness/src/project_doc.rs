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
use std::path::Component;
use std::path::{Path, PathBuf};

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
    scope: &Path,
    origin: &Path,
    documents: &mut Vec<InstructionDocument>,
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
    documents.push(InstructionDocument::new(
        canonical.clone(),
        scope.to_path_buf(),
        origin.to_path_buf(),
        body.clone(),
    ));

    let mut sections = vec![body.clone()];
    for mention in extract_at_mentions(&body) {
        let Some(include) = resolve_include(&canonical, &mention) else {
            continue;
        };
        sections.extend(read_doc_tree(
            &include,
            loaded_paths,
            loaded,
            depth + 1,
            scope,
            origin,
            documents,
        ));
    }
    sections
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectDocOverlay {
    pub(crate) text: String,
    pub(crate) loaded_paths: Vec<PathBuf>,
    pub(crate) documents: Vec<InstructionDocument>,
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
    let mut documents = Vec::new();
    let mut loaded: Vec<String> = Vec::new();
    let mut loaded_paths: HashSet<PathBuf> = HashSet::new();

    // Global scope: $CODEX_HOME/AGENTS.md (+ override), uncapped.
    if let Some(home) = codex_home {
        for name in [AGENTS_FILE, AGENTS_OVERRIDE_FILE] {
            let p = home.join(name);
            sections.extend(read_doc_tree(
                &p,
                &mut loaded_paths,
                &mut loaded,
                0,
                cwd,
                &p,
                &mut documents,
            ));
        }
    }

    // Project scope: repo AGENTS.md, git root → cwd. Never truncate
    // instructions here; a large overlay is the operator's context decision.
    let mut project: Vec<String> = Vec::new();
    for p in project_agents_paths(cwd, project_doc_files) {
        project.extend(read_doc_tree(
            &p,
            &mut loaded_paths,
            &mut loaded,
            0,
            p.parent().unwrap_or(cwd),
            &p,
            &mut documents,
        ));
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
    origins: std::collections::BTreeSet<PathBuf>,
    occurrence: u64,
    delivered_generation: Option<u64>,
}

#[derive(Debug, Default)]
struct InstructionLedger {
    explicit_origins: std::collections::BTreeSet<(PathBuf, PathBuf)>,
    active: Vec<ActiveInstruction>,
    observed_paths: std::collections::BTreeSet<PathBuf>,
    next_occurrence: u64,
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
                origins: std::collections::BTreeSet::from([document.origin.clone()]),
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
}

/// Session-owned version ledger. Shell and remote tools are explicit escape
/// hatches; this policy only admits tool-owned structured filesystem scopes.
#[derive(Debug, Clone)]
pub(crate) struct ScopedProjectDocs {
    root: PathBuf,
    names: Vec<String>,
    ledger: std::sync::Arc<std::sync::Mutex<InstructionLedger>>,
}

impl Default for ScopedProjectDocs {
    fn default() -> Self {
        Self::with_names(PathBuf::from("/"), None, vec![AGENTS_FILE.into()])
    }
}

impl ScopedProjectDocs {
    pub(crate) fn new(root: PathBuf, startup: Option<&ProjectDocOverlay>) -> Self {
        Self::with_names(root, startup, project_doc_files())
    }

    fn with_names(root: PathBuf, startup: Option<&ProjectDocOverlay>, names: Vec<String>) -> Self {
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
            ledger: std::sync::Arc::new(std::sync::Mutex::new(ledger)),
        }
    }

    /// Register global candidates even when discovery could not read them.
    /// An explicit system override skips this startup enrollment.
    pub(crate) fn enroll_global_candidates(&self) {
        if let Some(home) = codex_home() {
            self.enroll_global_candidates_at(&home);
        }
    }

    fn enroll_global_candidates_at(&self, home: &Path) {
        let mut ledger = self.ledger.lock().expect("instruction ledger poisoned");
        for name in [AGENTS_FILE, AGENTS_OVERRIDE_FILE] {
            ledger
                .explicit_origins
                .insert((home.join(name), self.root.clone()));
        }
    }

    /// Taking a batch grants nothing. Append its exact text to authoritative
    /// transport input before acknowledging this exact batch.
    pub(crate) fn pending_batch(&self, generation: u64) -> Option<InstructionBatch> {
        let ledger = self.ledger.lock().expect("instruction ledger poisoned");
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

    pub(crate) fn acknowledge(&self, batch: &InstructionBatch) {
        let mut ledger = self.ledger.lock().expect("instruction ledger poisoned");
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
    }

    /// Compaction replaces instruction-bearing history. An acknowledgment
    /// already in flight cannot revive a version from the previous history.
    pub(crate) fn invalidate_delivery(&self) {
        let mut ledger = self.ledger.lock().expect("instruction ledger poisoned");
        for index in 0..ledger.active.len() {
            ledger.next_occurrence += 1;
            ledger.active[index].occurrence = ledger.next_occurrence;
            ledger.active[index].delivered_generation = None;
        }
    }

    pub(crate) fn active_documents(&self) -> Vec<InstructionDocument> {
        let ledger = self.ledger.lock().expect("instruction ledger poisoned");
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
        self.ledger
            .lock()
            .expect("instruction ledger poisoned")
            .observed_paths
            .remove(&self.root);
    }

    pub(crate) fn observed_paths(&self) -> Vec<PathBuf> {
        self.ledger
            .lock()
            .expect("instruction ledger poisoned")
            .observed_paths
            .iter()
            .cloned()
            .collect()
    }

    pub(crate) fn restore_observed_paths(&self, paths: Vec<PathBuf>) {
        self.ledger
            .lock()
            .expect("instruction ledger poisoned")
            .observed_paths
            .extend(paths);
    }

    /// Restore graph origins/scopes only, never proof of delivery. Refresh
    /// rereads current bytes before the host prepares its first resumed request.
    pub(crate) async fn restore_documents(
        &self,
        documents: Vec<InstructionDocument>,
    ) -> Result<(), String> {
        {
            let mut ledger = self.ledger.lock().expect("instruction ledger poisoned");
            for document in documents {
                ledger.observe(document);
            }
        }
        self.invalidate_delivery();
        self.refresh().await
    }

    /// Rebuild complete include graphs and rewalk visited ancestry before each
    /// provider boundary. Removed edges revoke old instructions; new AGENTS in
    /// already visited scopes are discovered without another tool invocation.
    pub(crate) async fn refresh(&self) -> Result<(), String> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut ledger = this.ledger.lock().expect("instruction ledger poisoned");
            this.refresh_blocking(&mut ledger)
        })
        .await
        .map_err(|error| format!("instruction refresh task failed: {error}"))?
    }

    // All filesystem discovery runs on the owning blocking executor, with the
    // session mutex serializing observation and exact version acknowledgment.
    #[allow(clippy::disallowed_methods)]
    fn refresh_blocking(&self, ledger: &mut InstructionLedger) -> Result<(), String> {
        let root = canonical_existing_ancestor(&self.root)?;
        let mut origins = ledger.explicit_origins.clone();
        for path in &ledger.observed_paths {
            let canonical = canonical_existing_ancestor(path)?;
            for touched in [path, &canonical] {
                for directory in instruction_ancestry(&root, touched) {
                    for name in &self.names {
                        origins.insert((directory.join(name), directory.clone()));
                    }
                }
            }
        }
        for active in &ledger.active {
            for origin in &active.origins {
                origins.insert((origin.clone(), active.document.scope.clone()));
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
            match std::fs::symlink_metadata(&origin) {
                Ok(_) => {
                    if let Err(error) = read_instruction_tree(
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
        ledger.reconcile(documents, errors.is_empty());
        errors.into_iter().next().map_or(Ok(()), Err)
    }

    fn check_blocking(
        &self,
        request: bro_tools::InstructionPaths,
        generation: u64,
    ) -> Result<(), bro_tools::ToolResult> {
        let mut ledger = self.ledger.lock().expect("instruction ledger poisoned");
        let root = canonical_existing_ancestor(&self.root).map_err(instruction_read_error)?;
        let mut touched = Vec::new();
        for path in request.paths {
            let lexical = normalize_lexical(&if path.is_absolute() {
                path
            } else {
                root.join(path)
            });
            let canonical =
                canonical_existing_ancestor(&lexical).map_err(instruction_read_error)?;
            touched.push(lexical);
            touched.push(canonical);
        }
        ledger.observed_paths.extend(touched.iter().cloned());
        self.refresh_blocking(&mut ledger)
            .map_err(instruction_read_error)?;
        if request.access == bro_tools::InstructionAccess::Read {
            return Ok(());
        }
        let blocked: Vec<_> = ledger
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
            .collect::<std::collections::BTreeSet<_>>()
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

#[async_trait::async_trait]
impl bro_tools::InstructionPolicy for ScopedProjectDocs {
    async fn check(
        &self,
        request: bro_tools::InstructionPaths,
        authoring_generation: u64,
    ) -> Result<(), bro_tools::ToolResult> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.check_blocking(request, authoring_generation))
            .await
            .map_err(|error| {
                bro_tools::ToolResult::Error(format!("instruction discovery task failed: {error}"))
            })?
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
        "{RIDER_OPEN}\nHost-discovered instructions apply to the named scopes. These exact document versions are authoritative user context.\n\n{sections}\n{RIDER_CLOSE}"
    )
}

// Blocking executor only. Canonicalize a new target through its nearest
// existing parent, retaining absent suffixes without dropping symlink ancestry.
#[allow(clippy::disallowed_methods)]
fn canonical_existing_ancestor(path: &Path) -> Result<PathBuf, String> {
    let mut cursor = path;
    let mut suffix = Vec::new();
    loop {
        match std::fs::symlink_metadata(cursor) {
            Ok(_) => {
                let mut canonical = std::fs::canonicalize(cursor)
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

fn instruction_ancestry(root: &Path, touched: &Path) -> Vec<PathBuf> {
    let directory = if touched.is_dir() {
        touched
    } else {
        touched.parent().unwrap_or(touched)
    };
    let in_workspace = touched.starts_with(root);
    let boundary = root
        .ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
        .unwrap_or(root);
    let mut ancestors = Vec::new();
    for ancestor in directory.ancestors() {
        ancestors.push(ancestor.to_path_buf());
        if (in_workspace && ancestor == boundary)
            || (!in_workspace && ancestor.join(".git").exists())
        {
            break;
        }
    }
    ancestors.reverse();
    ancestors
}

// Blocking executor only. Missing optional AGENTS candidates are filtered by
// the caller; a selected document or allowed explicit include must be readable.
#[allow(clippy::disallowed_methods)]
fn read_instruction_tree(
    path: &Path,
    scope: &Path,
    origin: &Path,
    visited: &mut HashSet<PathBuf>,
    documents: &mut Vec<InstructionDocument>,
    depth: usize,
) -> Result<(), String> {
    let canonical =
        std::fs::canonicalize(path).map_err(|error| format!("{}: {error}", path.display()))?;
    if !visited.insert(canonical.clone()) {
        return Ok(());
    }
    if depth > MAX_INCLUDE_DEPTH {
        return Err(format!(
            "{}: instruction include depth exceeds {MAX_INCLUDE_DEPTH}",
            path.display()
        ));
    }
    let body = std::fs::read_to_string(&canonical)
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
            read_instruction_tree(&candidate, scope, origin, visited, documents, depth + 1)?;
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
