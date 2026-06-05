use std::fs;
use std::path::{Path, PathBuf};

use std::str::FromStr;

use anyhow::{Context, Result};
use rmcp::schemars;
use serde::{Deserialize, Serialize};

// ── MCP parameter structs ─────────────────────────────────────────
//
// These are the typed inputs for the bbox_thread / bbox_thread_list
// MCP tools. They live here (next to their domain methods) rather
// than in `main.rs` so the server crate can own the schema alongside
// the behavior it drives.

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ThreadParams {
    /// get, open, continue, link, resolve, promote, rename
    pub action: String,
    #[serde(default)]
    pub name: Option<String>,
    /// Thread ID. Canonical form is `thread-<8 hex>` (e.g. `thread-7f01324e`)
    /// — the exact string returned by `bbox_thread(action="open")` and listed
    /// by `bbox_thread_list`. Required for `continue`, `resolve`, `rename`,
    /// `link`, `promote`; optional for `get` (friendly `name` works too).
    #[serde(default)]
    #[schemars(regex(pattern = r"^(thread-)?[0-9a-f]{8}$"))]
    pub id: Option<String>,
    #[serde(default)]
    pub topic: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub session_name: Option<String>,
    #[serde(default)]
    pub handoff_doc: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub target_type: Option<String>,
    #[serde(default)]
    pub edge: Option<String>,
    #[serde(default)]
    pub promoted_to: Option<String>,
    /// Thread kind (e.g. "work_item"). Optional; defaults to general.
    #[serde(default)]
    pub kind: Option<String>,
    /// Thread origin marker (e.g. "workflow"). Optional for normal/manual threads.
    #[serde(default)]
    pub origin: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ThreadListParams {
    /// Filter by lifecycle status: open, active, resolved, promoted.
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    /// Return only threads idle for at least this many days.
    #[serde(default)]
    pub min_idle_days: Option<u64>,
    #[serde(default)]
    pub include_resolved: Option<bool>,
    /// Filter by thread kind (e.g. "work_item")
    #[serde(default)]
    pub kind: Option<String>,
    /// Include workflow-origin threads. Defaults to false so workflow arc
    /// scaffolding does not dominate normal continuity scans.
    #[serde(default)]
    pub include_workflows: Option<bool>,
}

// ── Schema ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, strum::EnumString, strum::AsRefStr)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ThreadStatus {
    Open,
    Active,
    Resolved,
    /// graduated to graph (finding/inquiry/task)
    Promoted,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Serialize, Deserialize, strum::EnumString, strum::AsRefStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ThreadKind {
    /// Orchestrator-led propose → execute → review → refine loop
    WorkItem,
    /// Investigation or QC walk
    Investigation,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Serialize, Deserialize, strum::EnumString, strum::AsRefStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ThreadOrigin {
    /// Thread was opened by the workflow runtime for an arc.
    Workflow,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, strum::EnumString, strum::AsRefStr)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum EdgeKind {
    /// this thread was opened from another
    SpawnedFrom,
    /// this thread is blocked until target resolves
    BlockedBy,
    /// general relationship
    RelatesTo,
    /// this thread absorbs/replaces target
    Subsumes,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, strum::EnumString, strum::AsRefStr)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum EdgeTarget {
    Thread,
    Session,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadEdge {
    pub kind: EdgeKind,
    pub target: String, // thread ID or session ID
    #[serde(default = "EdgeTarget::default")]
    pub target_type: EdgeTarget,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub created_at: String,
}

impl EdgeTarget {
    fn default() -> Self {
        Self::Thread
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionLink {
    pub session_id: String,
    pub provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub linked_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Thread {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub topic: String,
    pub project: String,
    /// Directory the committed `.bbox/record/` snapshot is written into. When a
    /// project-scoped thread is opened from a managed fleet worktree, `project`
    /// holds the registered base (durable scope) while this holds the worktree
    /// path, so the record travels with the agent's branch. `None` → write into
    /// `project` (the common, non-worktree case). Internal: not a `ThreadParams`
    /// input; set by the `bbox_thread` adapter via worktree resolution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_dir: Option<String>,
    pub status: ThreadStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ThreadKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<ThreadOrigin>,
    pub sessions: Vec<SessionLink>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handoff_doc: Option<String>,
    #[serde(default)]
    pub notes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edges: Vec<ThreadEdge>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub promoted_to: Option<String>, // graph entity ref when promoted
    pub created_at: String,
    pub last_activity: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ThreadStore {
    pub version: u32,
    pub threads: Vec<Thread>,
}

pub struct ThreadMutation {
    pub message: String,
    pub changed_thread: Option<Thread>,
    pub changed_edges: bool,
}

impl ThreadStore {
    pub fn new() -> Self {
        Self {
            version: 1,
            threads: Vec::new(),
        }
    }
}

// ── Repo-owned thread records ──────────────────────────────────────
//
// A live thread is operational exhaust — high-churn, session/bro-bound — and
// stays in the host-local store. But once a thread is promoted or resolved it
// becomes a durable record of past activity that belongs with the code it
// explains, so we snapshot a scrubbed summary into the owning repo's
// `.bbox/record/<id>.json` (committed, travels with the checkout). Only the
// settled record travels; the churny live state never does.

/// Durable, committed snapshot of a settled thread. Deliberately omits the
/// identity-bearing live fields (sessions, edges, origin, handoff doc) — the
/// record is a portable summary, not a copy of host state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadRecord {
    pub id: String,
    pub topic: String,
    pub status: ThreadStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ThreadKind>,
    /// Graph entity ref this thread produced (set when promoted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promoted_to: Option<String>,
    /// Scrubbed investigation summary.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    pub created_at: String,
    pub resolved_at: String,
}

/// Replace the absolute home path with `~` so a committed record carries no
/// host-specific path. Split out from the home lookup so it is hermetically
/// testable without reading the real `$HOME`.
fn scrub_host_identity_with(s: &str, home: Option<&Path>) -> String {
    match home {
        Some(h) if !h.as_os_str().is_empty() => s.replace(h.to_string_lossy().as_ref(), "~"),
        _ => s.to_string(),
    }
}

/// Write a durable, host-scrubbed snapshot of a settled thread into the owning
/// repo's committed `.bbox/record/`. The write target is the first existing dir
/// of [`record_dir`, `project`]: a worktree-opened thread writes into the
/// worktree (so the record travels with the agent's branch), falling back to the
/// base `project` when the worktree is gone (e.g. removed before resolve) so the
/// record is never silently lost. Returns `(selected_root, record_path)` so the
/// caller can build a commit-this rider relative to the root actually used;
/// `Ok(None)` when no bound path exists on this host (nothing written).
fn write_thread_record(thread: &Thread) -> Result<Option<(PathBuf, PathBuf)>> {
    let candidates = [thread.record_dir.as_deref(), Some(thread.project.as_str())];
    let root = candidates
        .into_iter()
        .flatten()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .find(|p| p.is_dir());
    let Some(root) = root else {
        return Ok(None);
    };
    let dir = root.join(".bbox").join("record");
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    let home = dirs::home_dir();
    let scrub = |s: &str| scrub_host_identity_with(s, home.as_deref());
    let record = ThreadRecord {
        id: thread.id.clone(),
        topic: scrub(&thread.topic),
        status: thread.status.clone(),
        kind: thread.kind,
        promoted_to: thread.promoted_to.clone(),
        notes: thread.notes.iter().map(|n| scrub(n)).collect(),
        created_at: thread.created_at.clone(),
        resolved_at: thread.resolved_at.clone().unwrap_or_default(),
    };
    let path = dir.join(format!("{}.json", thread.id));
    crate::json_store::atomic_write_json_locked(&path, &record)?;
    Ok(Some((root, path)))
}

/// Load every committed thread record under `<project_dir>/.bbox/record/`.
/// These are durable snapshots of settled threads that travel with the repo;
/// on a clone (where the live thread store doesn't carry them) they are the
/// only trace of past investigations.
pub(crate) fn load_repo_records(project_dir: &Path) -> Vec<ThreadRecord> {
    let dir = project_dir.join(".bbox").join("record");
    let Ok(read) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for de in read.flatten() {
        let path = de.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<ThreadRecord>(&raw).ok())
        {
            Some(record) => out.push(record),
            None => tracing::warn!("skipping unreadable thread record {}", path.display()),
        }
    }
    out
}

// ── Store operations ───────────────────────────────────────────────

pub struct Threads {
    store_path: PathBuf,
    store: ThreadStore,
}

impl Threads {
    pub fn open(store_path: &Path) -> Result<Self> {
        let store = if store_path.exists() {
            let raw = fs::read_to_string(store_path)
                .with_context(|| format!("reading {}", store_path.display()))?;
            serde_json::from_str(&raw)
                .with_context(|| format!("parsing {}", store_path.display()))?
        } else {
            ThreadStore::new()
        };
        Ok(Self {
            store_path: store_path.to_path_buf(),
            store,
        })
    }

    fn save(&self) -> Result<()> {
        crate::json_store::atomic_write_json_locked(&self.store_path, &self.store)
    }

    pub fn reload(&mut self) -> Result<()> {
        if self.store_path.exists() {
            let raw = fs::read_to_string(&self.store_path)
                .with_context(|| format!("reading {}", self.store_path.display()))?;
            self.store = serde_json::from_str(&raw)
                .with_context(|| format!("parsing {}", self.store_path.display()))?;
        }
        Ok(())
    }

    fn now_iso() -> String {
        crate::util::now_iso()
    }

    fn gen_id() -> String {
        use std::time::SystemTime;
        let d = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let hash = d.as_nanos() ^ 0x517cc1b727220a95;
        format!("thread-{:08x}", hash as u32)
    }

    /// Immutable slice of all stored threads — used by cross-store
    /// aggregators (inbox) that can't go through the MCP layer.
    pub fn all(&self) -> &[Thread] {
        &self.store.threads
    }

    pub fn rename_project_refs(&mut self, old_project: &str, new_project: &str) -> Result<usize> {
        let path = self.store_path.clone();
        crate::json_store::with_store_lock(&path, || {
            self.reload()?;
            let mut updated = 0usize;
            let now = Self::now_iso();
            for thread in &mut self.store.threads {
                if thread.project == old_project {
                    thread.project = new_project.to_string();
                    thread.last_activity = now.clone();
                    updated += 1;
                }
            }
            if updated > 0 {
                self.save()?;
                for thread in &self.store.threads {
                    if thread.project == new_project {
                        crate::embed_queue::enqueue_thread(thread);
                    }
                }
            }
            Ok(updated)
        })
    }

    // ── blackbox_thread (CRUD) ─────────────────────────────────────

    pub fn thread(&mut self, p: &ThreadParams) -> Result<String> {
        Ok(self.thread_mutation(p, None)?.message)
    }

    /// `write_dir` is the committed-record write target resolved by the MCP
    /// adapter when `p.project` is a managed fleet worktree (the worktree path;
    /// `p.project` having been normalized to the registered base for scope). It
    /// is honored by open/resolve/promote so the record travels with the agent's
    /// branch. `None` for every non-worktree caller (the `thread()` wrapper and
    /// its many internal callers), preserving today's base-write behavior.
    pub fn thread_mutation(
        &mut self,
        p: &ThreadParams,
        write_dir: Option<&str>,
    ) -> Result<ThreadMutation> {
        if p.action == "get" {
            return Ok(ThreadMutation {
                message: self.thread_get(p)?,
                changed_thread: None,
                changed_edges: false,
            });
        }
        let path = self.store_path.clone();
        crate::json_store::with_store_lock(&path, || {
            self.reload()?;
            match p.action.as_str() {
                "open" => self.thread_open(p, write_dir),
                "continue" => self.thread_continue(p),
                "link" => self.thread_link(p),
                "resolve" => self.thread_resolve(p, write_dir),
                "promote" => self.thread_promote(p, write_dir),
                "rename" => self.thread_rename(p),
                other => anyhow::bail!(
                    "Unknown action: {other}. Use: get, open, continue, link, resolve, promote, rename"
                ),
            }
        })
    }

    fn thread_open(&mut self, p: &ThreadParams, write_dir: Option<&str>) -> Result<ThreadMutation> {
        let topic = p.topic.as_deref().context("'topic' is required")?;
        let project = p.project.as_deref().unwrap_or("");

        let now = Self::now_iso();
        let id = Self::gen_id();

        let mut sessions = Vec::new();
        if let Some(sid) = p.session_id.as_deref() {
            sessions.push(SessionLink {
                session_id: sid.to_string(),
                provider: p.provider.as_deref().unwrap_or("unknown").to_string(),
                name: p.session_name.clone(),
                linked_at: now.clone(),
            });
        }

        let notes = p.note.clone().into_iter().collect();

        let kind = p
            .kind
            .as_deref()
            .map(ThreadKind::from_str)
            .transpose()
            .map_err(|_| {
                anyhow::anyhow!(
                    "Unknown thread kind: {:?}. Use: work_item, investigation",
                    p.kind
                )
            })?;
        let origin = p
            .origin
            .as_deref()
            .map(ThreadOrigin::from_str)
            .transpose()
            .map_err(|_| anyhow::anyhow!("Unknown thread origin: {:?}. Use: workflow", p.origin))?;

        let thread = Thread {
            id: id.clone(),
            name: p.name.clone(),
            topic: topic.to_string(),
            project: project.to_string(),
            record_dir: write_dir.map(str::to_string),
            status: ThreadStatus::Open,
            kind,
            origin,
            sessions,
            handoff_doc: p.handoff_doc.clone(),
            notes,
            edges: Vec::new(),
            promoted_to: None,
            created_at: now.clone(),
            last_activity: now,
            resolved_at: None,
        };

        let changed_edges = !thread.sessions.is_empty();
        self.store.threads.push(thread.clone());
        self.save()?;
        crate::embed_queue::enqueue_thread(&thread);

        Ok(ThreadMutation {
            message: format!("Thread created: {} — \"{}\"", id, topic),
            changed_thread: Some(thread),
            changed_edges,
        })
    }

    fn thread_get(&self, p: &ThreadParams) -> Result<String> {
        let thread = if let Some(id) = p.id.as_deref() {
            // Accept bare `<8hex>` as fallback for canonical `thread-<8hex>`
            // — matches the schema regex and the NoteResolveParams policy.
            self.store
                .threads
                .iter()
                .find(|t| t.id == id || t.id.strip_prefix("thread-") == Some(id))
        } else if let Some(name) = p.name.as_deref() {
            let name_lower = name.to_lowercase();
            self.store.threads.iter().find(|t| {
                t.name
                    .as_ref()
                    .map(|n| n.to_lowercase() == name_lower)
                    .unwrap_or(false)
                    || t.id == name
            })
        } else {
            anyhow::bail!("'id' or 'name' is required for get");
        };

        let thread = thread.context("Thread not found")?;

        // Build a readable representation
        let mut out = String::new();
        out.push_str(&format!("# {} — {}\n", thread.id, thread.topic));
        if let Some(name) = &thread.name {
            out.push_str(&format!("Name: {}\n", name));
        }
        out.push_str(&format!("Status: {}\n", thread.status.as_ref()));
        if let Some(k) = thread.kind {
            out.push_str(&format!("Kind: {}\n", k.as_ref()));
        }
        if let Some(origin) = thread.origin {
            out.push_str(&format!("Origin: {}\n", origin.as_ref()));
        }
        out.push_str(&format!(
            "Project: {}\n",
            if thread.project.is_empty() {
                "-"
            } else {
                &thread.project
            }
        ));
        out.push_str(&format!("Created: {}\n", thread.created_at));
        out.push_str(&format!("Last activity: {}\n", thread.last_activity));
        if let Some(resolved) = &thread.resolved_at {
            out.push_str(&format!("Resolved: {}\n", resolved));
        }
        if let Some(doc) = &thread.handoff_doc {
            out.push_str(&format!("Handoff doc: {}\n", doc));
        }
        if let Some(promoted) = &thread.promoted_to {
            out.push_str(&format!("Promoted to: {}\n", promoted));
        }

        // Sessions
        if thread.sessions.is_empty() {
            out.push_str("\nSessions: none\n");
        } else {
            out.push_str(&format!("\nSessions ({}):\n", thread.sessions.len()));
            for s in &thread.sessions {
                let display = s.name.as_deref().unwrap_or(&s.session_id);
                out.push_str(&format!(
                    "  - {} ({}) linked {}\n",
                    display, s.provider, s.linked_at
                ));
            }
        }

        // Edges
        if !thread.edges.is_empty() {
            out.push_str(&format!("\nEdges ({}):\n", thread.edges.len()));
            for e in &thread.edges {
                let target_label = match e.target_type {
                    EdgeTarget::Thread => {
                        let name = self
                            .store
                            .threads
                            .iter()
                            .find(|t| t.id == e.target)
                            .and_then(|t| t.name.as_deref())
                            .unwrap_or("?");
                        format!("{} ({})", e.target, name)
                    }
                    EdgeTarget::Session => {
                        // Check if this session is linked on any thread for a friendly name
                        let name = self
                            .store
                            .threads
                            .iter()
                            .flat_map(|t| t.sessions.iter())
                            .find(|s| s.session_id == e.target)
                            .and_then(|s| s.name.as_deref());
                        match name {
                            Some(n) => {
                                format!("session:{} ({})", &e.target[..e.target.len().min(8)], n)
                            }
                            None => format!("session:{}", &e.target[..e.target.len().min(8)]),
                        }
                    }
                };
                out.push_str(&format!("  - {} → {}", e.kind.as_ref(), target_label));
                if let Some(note) = &e.note {
                    out.push_str(&format!(" — {}", note));
                }
                out.push('\n');
            }
        }

        // Notes
        if thread.notes.is_empty() {
            out.push_str("\nNotes: none\n");
        } else {
            out.push_str(&format!("\nNotes ({}):\n", thread.notes.len()));
            for (i, note) in thread.notes.iter().enumerate() {
                out.push_str(&format!("\n--- Note {} ---\n{}\n", i + 1, note));
            }
        }

        Ok(out)
    }

    fn thread_link(&mut self, p: &ThreadParams) -> Result<ThreadMutation> {
        let id = self.resolve_thread_id(p)?;
        let target = p
            .target
            .as_deref()
            .context("'target' is required (target thread or session ID)")?;
        let kind_str = p
            .edge
            .as_deref()
            .context("'edge' is required (spawned_from, blocked_by, relates_to, subsumes)")?;
        let kind = EdgeKind::from_str(kind_str).map_err(|_| {
            anyhow::anyhow!(
                "Unknown edge kind: {kind_str}. Use: spawned_from, blocked_by, relates_to, subsumes"
            )
        })?;

        let target_type_str = p.target_type.as_deref().unwrap_or("thread");
        let target_type = EdgeTarget::from_str(target_type_str).map_err(|_| {
            anyhow::anyhow!("Unknown target_type: {target_type_str}. Use: thread, session")
        })?;

        // Validate target exists (threads only — sessions are external, trust the caller)
        if target_type == EdgeTarget::Thread && !self.store.threads.iter().any(|t| t.id == target) {
            anyhow::bail!("Target thread {target} not found");
        }

        let thread = self
            .store
            .threads
            .iter_mut()
            .find(|t| t.id == id)
            .context("Source thread not found")?;

        // Check for duplicate edge
        if thread
            .edges
            .iter()
            .any(|e| e.kind == kind && e.target == target && e.target_type == target_type)
        {
            anyhow::bail!("Edge {kind_str} → {target} already exists");
        }

        let now = Self::now_iso();
        thread.edges.push(ThreadEdge {
            kind,
            target: target.to_string(),
            target_type,
            note: p.note.clone(),
            created_at: now.clone(),
        });
        thread.last_activity = now;

        let topic = thread.topic.clone();
        let thread_for_embed = thread.clone();
        self.save()?;
        crate::embed_queue::enqueue_thread(&thread_for_embed);

        Ok(ThreadMutation {
            message: format!("Thread {id} ({topic}) — added {kind_str} edge to {target}"),
            changed_thread: Some(thread_for_embed),
            changed_edges: true,
        })
    }

    /// Resolve a thread by `id` or `name` in the params. Accepts bare
    /// `<8hex>` as fallback for canonical `thread-<8hex>` and always
    /// returns the canonical stored form.
    fn resolve_thread_id(&self, p: &ThreadParams) -> Result<String> {
        if let Some(id) = p.id.as_deref() {
            if let Some(t) = self
                .store
                .threads
                .iter()
                .find(|t| t.id == id || t.id.strip_prefix("thread-") == Some(id))
            {
                return Ok(t.id.clone());
            }
            anyhow::bail!(
                "Thread not found: {id} (expected `thread-<8hex>`, e.g. `thread-7f01324e`)"
            );
        }
        if let Some(name) = p.name.as_deref() {
            let name_lower = name.to_lowercase();
            if let Some(t) = self.store.threads.iter().find(|t| {
                t.name
                    .as_ref()
                    .map(|n| n.to_lowercase() == name_lower)
                    .unwrap_or(false)
                    || t.id == name
            }) {
                return Ok(t.id.clone());
            }
            anyhow::bail!("Thread not found: {name}");
        }
        anyhow::bail!("'id' or 'name' is required");
    }

    fn thread_continue(&mut self, p: &ThreadParams) -> Result<ThreadMutation> {
        let id = self.resolve_thread_id(p)?;

        let thread = self
            .store
            .threads
            .iter_mut()
            .find(|t| t.id == id)
            .context("Thread not found")?;

        let now = Self::now_iso();

        let mut changed_edges = false;
        if let Some(sid) = p.session_id.as_deref() {
            thread.sessions.push(SessionLink {
                session_id: sid.to_string(),
                provider: p.provider.as_deref().unwrap_or("unknown").to_string(),
                name: p.session_name.clone(),
                linked_at: now.clone(),
            });
            changed_edges = true;
        }
        if let Some(note) = p.note.as_deref() {
            thread.notes.push(note.to_string());
        }
        if let Some(doc) = p.handoff_doc.as_deref() {
            thread.handoff_doc = Some(doc.to_string());
        }
        if let Some(name) = p.name.as_deref() {
            thread.name = Some(name.to_string());
        }

        thread.status = ThreadStatus::Active;
        thread.last_activity = now;
        let topic = thread.topic.clone();
        let thread_for_embed = thread.clone();

        self.save()?;
        crate::embed_queue::enqueue_thread(&thread_for_embed);

        Ok(ThreadMutation {
            message: format!("Thread {id} continued — \"{topic}\""),
            changed_thread: Some(thread_for_embed),
            changed_edges,
        })
    }

    fn thread_resolve(
        &mut self,
        p: &ThreadParams,
        write_dir: Option<&str>,
    ) -> Result<ThreadMutation> {
        let id = self.resolve_thread_id(p)?;

        let thread = self
            .store
            .threads
            .iter_mut()
            .find(|t| t.id == id)
            .context("Thread not found")?;

        let now = Self::now_iso();

        if let Some(note) = p.note.as_deref() {
            thread.notes.push(note.to_string());
        }

        // (Re)point the committed-record write target at the worktree the agent
        // is resolving from, so a thread opened from the base still snapshots
        // into the worktree branch at close-out.
        if let Some(wd) = write_dir {
            thread.record_dir = Some(wd.to_string());
        }

        thread.status = ThreadStatus::Resolved;
        thread.last_activity = now.clone();
        thread.resolved_at = Some(now);
        let topic = thread.topic.clone();
        let thread_for_embed = thread.clone();

        self.save()?;
        let record_rider = match write_thread_record(&thread_for_embed) {
            Ok(Some((root, path))) => Some(crate::util::repo_artifact_rider(
                &root.to_string_lossy(),
                &path,
            )),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!("thread record write for {id}: {e:#}");
                None
            }
        };
        crate::embed_queue::enqueue_thread(&thread_for_embed);

        let mut message = format!("Thread {id} resolved — \"{topic}\"");
        if let Some(rider) = record_rider {
            message.push_str(&rider);
        }
        Ok(ThreadMutation {
            message,
            changed_thread: Some(thread_for_embed),
            changed_edges: false,
        })
    }

    fn thread_promote(
        &mut self,
        p: &ThreadParams,
        write_dir: Option<&str>,
    ) -> Result<ThreadMutation> {
        let id = self.resolve_thread_id(p)?;
        let promoted_to = p
            .promoted_to
            .as_deref()
            .context("'promoted_to' is required (graph entity reference)")?;

        let thread = self
            .store
            .threads
            .iter_mut()
            .find(|t| t.id == id)
            .context("Thread not found")?;

        let now = Self::now_iso();

        if let Some(note) = p.note.as_deref() {
            thread.notes.push(note.to_string());
        }

        // (Re)point the committed-record write target at the worktree the agent
        // is promoting from (see thread_resolve).
        if let Some(wd) = write_dir {
            thread.record_dir = Some(wd.to_string());
        }

        thread.status = ThreadStatus::Promoted;
        thread.promoted_to = Some(promoted_to.to_string());
        thread.last_activity = now.clone();
        thread.resolved_at = Some(now);
        let topic = thread.topic.clone();
        let thread_for_embed = thread.clone();

        self.save()?;
        let record_rider = match write_thread_record(&thread_for_embed) {
            Ok(Some((root, path))) => Some(crate::util::repo_artifact_rider(
                &root.to_string_lossy(),
                &path,
            )),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!("thread record write for {id}: {e:#}");
                None
            }
        };
        crate::embed_queue::enqueue_thread(&thread_for_embed);

        let mut message = format!("Thread {id} promoted to {promoted_to} — \"{topic}\"");
        if let Some(rider) = record_rider {
            message.push_str(&rider);
        }
        Ok(ThreadMutation {
            message,
            changed_thread: Some(thread_for_embed),
            changed_edges: false,
        })
    }

    fn thread_rename(&mut self, p: &ThreadParams) -> Result<ThreadMutation> {
        // For rename, 'id' is lookup and 'name' is the new name.
        let id = p.id.as_deref().context("'id' is required for rename")?;
        let new_name = p.name.as_deref().context("'name' is required for rename")?;

        // Try to find by id directly, then fall back to id-as-name lookup
        let thread = self
            .store
            .threads
            .iter_mut()
            .find(|t| {
                t.id == id || t.name.as_deref().map(|n| n.to_lowercase()) == Some(id.to_lowercase())
            })
            .context("Thread not found")?;

        thread.name = Some(new_name.to_string());
        thread.last_activity = Self::now_iso();
        let topic = thread.topic.clone();
        let thread_for_embed = thread.clone();

        self.save()?;
        crate::embed_queue::enqueue_thread(&thread_for_embed);

        Ok(ThreadMutation {
            message: format!("Thread {id} renamed to \"{new_name}\" (topic: {topic})"),
            changed_thread: Some(thread_for_embed),
            changed_edges: false,
        })
    }

    // ── blackbox_thread_list (query) ───────────────────────────────

    pub fn thread_list(&self, p: &ThreadListParams) -> Result<String> {
        let status_filter = p.status.as_deref();
        let project_filter = p.project.as_deref();
        let name_filter = p.name.as_deref();
        let min_idle_days = p.min_idle_days;
        let include_resolved = p.include_resolved.unwrap_or(false);
        let include_workflows = p.include_workflows.unwrap_or(false);
        let kind_filter = p
            .kind
            .as_deref()
            .map(ThreadKind::from_str)
            .transpose()
            .map_err(|_| anyhow::anyhow!("Unknown thread kind: {:?}", p.kind))?;

        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let mut results: Vec<&Thread> = Vec::new();

        for thread in &self.store.threads {
            // Status filter
            if let Some(sf) = status_filter {
                if let Ok(target) = ThreadStatus::from_str(sf) {
                    if thread.status != target {
                        continue;
                    }
                } else {
                    anyhow::bail!(
                        "Unknown thread status: {sf}. Use: open, active, resolved, promoted"
                    );
                }
            } else if !include_resolved {
                // Default: exclude resolved and promoted
                if thread.status == ThreadStatus::Resolved
                    || thread.status == ThreadStatus::Promoted
                {
                    continue;
                }
            }

            // Project filter
            if let Some(pf) = project_filter {
                if !thread.project.to_lowercase().contains(&pf.to_lowercase()) {
                    continue;
                }
            }

            // Name filter
            if let Some(nf) = name_filter {
                let nf_lower = nf.to_lowercase();
                let name_matches = thread
                    .name
                    .as_ref()
                    .map(|n| n.to_lowercase().contains(&nf_lower))
                    .unwrap_or(false);
                let topic_matches = thread.topic.to_lowercase().contains(&nf_lower);
                if !name_matches && !topic_matches {
                    continue;
                }
            }

            // Workflow-origin threads are operational scaffolding. Hide them
            // from default continuity scans unless the caller explicitly opts in.
            if !include_workflows && thread.origin == Some(ThreadOrigin::Workflow) {
                continue;
            }

            // Idle-age filter
            if let Some(days) = min_idle_days {
                let age = self.thread_age_days(thread, now_secs);
                if age < days {
                    continue;
                }
            }

            // Kind filter
            if let Some(k) = kind_filter {
                if thread.kind != Some(k) {
                    continue;
                }
            }

            results.push(thread);
        }

        if results.is_empty() {
            return Ok("No threads found.".to_string());
        }

        // Sort by last_activity descending
        results.sort_by(|a, b| b.last_activity.cmp(&a.last_activity));

        let mut lines = Vec::new();
        for t in &results {
            let age = self.thread_age_days(t, now_secs);
            let age_str = if age == 0 {
                "today".to_string()
            } else {
                format!("{}d ago", age)
            };

            let sessions_str = if t.sessions.is_empty() {
                "no sessions".to_string()
            } else {
                let names: Vec<String> = t
                    .sessions
                    .iter()
                    .map(|s| match s.name.as_deref() {
                        Some(n) => n.to_string(),
                        None => s.session_id.chars().take(8).collect::<String>(),
                    })
                    .collect();
                names.join(", ")
            };

            let handoff = t.handoff_doc.as_deref().unwrap_or("-");
            let project = if t.project.is_empty() {
                "-"
            } else {
                t.project.rsplit('/').next().unwrap_or(&t.project)
            };

            let display_name = t.name.as_deref().unwrap_or("-");

            let edges_str = if t.edges.is_empty() {
                String::new()
            } else {
                let edge_parts: Vec<String> = t
                    .edges
                    .iter()
                    .map(|e| {
                        let label = match e.target_type {
                            EdgeTarget::Thread => self
                                .store
                                .threads
                                .iter()
                                .find(|t2| t2.id == e.target)
                                .and_then(|t2| t2.name.as_deref())
                                .unwrap_or("?")
                                .to_string(),
                            EdgeTarget::Session => {
                                let name = self
                                    .store
                                    .threads
                                    .iter()
                                    .flat_map(|t2| t2.sessions.iter())
                                    .find(|s| s.session_id == e.target)
                                    .and_then(|s| s.name.as_deref());
                                match name {
                                    Some(n) => format!("session:{}", n),
                                    None => {
                                        format!("session:{}", &e.target[..e.target.len().min(8)])
                                    }
                                }
                            }
                        };
                        format!("{}→{}", e.kind.as_ref(), label)
                    })
                    .collect();
                format!(" [{}]", edge_parts.join(", "))
            };

            lines.push(format!(
                "{} | {} | {} | {} | {} | {}{} | {} | {}",
                t.id,
                display_name,
                t.status.as_ref(),
                age_str,
                project,
                t.topic,
                edges_str,
                sessions_str,
                handoff,
            ));
        }

        let header = format!("{} thread(s)", results.len());
        Ok(format!("{}\n\n{}", header, lines.join("\n")))
    }

    fn thread_age_days(&self, thread: &Thread, now_secs: u64) -> u64 {
        // Parse ISO timestamp to approximate epoch seconds
        let ts = &thread.last_activity;
        if ts.len() < 10 {
            return 0;
        }
        let y: i64 = ts[0..4].parse().unwrap_or(2026);
        let m: u32 = ts[5..7].parse().unwrap_or(1);
        let d: u32 = ts[8..10].parse().unwrap_or(1);

        // Rough epoch calc
        let mut epoch_days: i64 = 0;
        for yr in 1970..y {
            epoch_days += if yr % 4 == 0 && (yr % 100 != 0 || yr % 400 == 0) {
                366
            } else {
                365
            };
        }
        let months = [
            31,
            if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
                29
            } else {
                28
            },
            31,
            30,
            31,
            30,
            31,
            31,
            30,
            31,
            30,
            31,
        ];
        for days in months.iter().take((m as usize - 1).min(11)) {
            epoch_days += *days as i64;
        }
        epoch_days += d as i64 - 1;

        let activity_secs = epoch_days as u64 * 86400;
        now_secs.saturating_sub(activity_secs) / 86400
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn params(action: &str) -> ThreadParams {
        ThreadParams {
            action: action.into(),
            topic: None,
            project: None,
            name: None,
            id: None,
            session_id: None,
            provider: None,
            session_name: None,
            handoff_doc: None,
            note: None,
            target: None,
            target_type: None,
            edge: None,
            promoted_to: None,
            kind: None,
            origin: None,
        }
    }

    fn open_thread_id(threads: &mut Threads, topic: &str, project: &str) -> String {
        let created = threads
            .thread(&ThreadParams {
                topic: Some(topic.into()),
                project: Some(project.into()),
                ..params("open")
            })
            .unwrap();
        created.split_whitespace().nth(2).unwrap().to_string()
    }

    #[test]
    fn resolve_writes_scrubbed_record_to_repo_bbox() {
        let dir = tempdir().unwrap();
        let repo = tempdir().unwrap();
        let repo_root = repo.path().canonicalize().unwrap();
        let mut threads = Threads::open(&dir.path().join("threads.json")).unwrap();

        let id = open_thread_id(
            &mut threads,
            "audit the dispatch path",
            &repo_root.to_string_lossy(),
        );
        threads
            .thread(&ThreadParams {
                id: Some(id.clone()),
                note: Some("found the bug in resolve_provider_pool".into()),
                ..params("resolve")
            })
            .unwrap();

        let record_path = repo_root
            .join(".bbox")
            .join("record")
            .join(format!("{id}.json"));
        assert!(
            record_path.exists(),
            "settled thread should snapshot to {}",
            record_path.display()
        );
        let rec: ThreadRecord =
            serde_json::from_str(&fs::read_to_string(&record_path).unwrap()).unwrap();
        assert_eq!(rec.status, ThreadStatus::Resolved);
        assert_eq!(rec.topic, "audit the dispatch path");
        assert!(
            rec.notes
                .iter()
                .any(|n| n.contains("resolve_provider_pool")),
            "investigation note should be recorded: {:?}",
            rec.notes
        );
    }

    #[test]
    fn promote_writes_record_with_graph_ref() {
        let dir = tempdir().unwrap();
        let repo = tempdir().unwrap();
        let repo_root = repo.path().canonicalize().unwrap();
        let mut threads = Threads::open(&dir.path().join("threads.json")).unwrap();

        let id = open_thread_id(
            &mut threads,
            "triad convergence",
            &repo_root.to_string_lossy(),
        );
        threads
            .thread(&ThreadParams {
                id: Some(id.clone()),
                promoted_to: Some("knowledge:abc12345".into()),
                ..params("promote")
            })
            .unwrap();

        let record_path = repo_root
            .join(".bbox")
            .join("record")
            .join(format!("{id}.json"));
        let rec: ThreadRecord =
            serde_json::from_str(&fs::read_to_string(&record_path).unwrap()).unwrap();
        assert_eq!(rec.status, ThreadStatus::Promoted);
        assert_eq!(rec.promoted_to.as_deref(), Some("knowledge:abc12345"));
    }

    #[test]
    fn thread_record_scrub_replaces_home_with_tilde() {
        assert_eq!(
            scrub_host_identity_with("/Users/me/repos/x said hi", Some(Path::new("/Users/me"))),
            "~/repos/x said hi"
        );
        // No home to anchor on → text is left untouched.
        assert_eq!(scrub_host_identity_with("/Users/me/x", None), "/Users/me/x");
    }

    #[test]
    fn resolve_writes_no_record_when_repo_absent() {
        // A thread whose owning repo isn't present on this host must not panic
        // or create stray directories — the record simply isn't written.
        let dir = tempdir().unwrap();
        let mut threads = Threads::open(&dir.path().join("threads.json")).unwrap();
        let id = open_thread_id(&mut threads, "x", "/nonexistent/repo/path");
        let msg = threads
            .thread(&ThreadParams {
                id: Some(id),
                ..params("resolve")
            })
            .unwrap();
        assert!(!Path::new("/nonexistent/repo/path").exists());
        // No project on this host → nothing written → no commit-this rider.
        assert!(
            !msg.contains(".bbox/record"),
            "no rider when the repo record was not written: {msg}"
        );
    }

    #[test]
    fn resolve_from_worktree_writes_record_into_worktree_not_base() {
        // A thread opened from the base (no write-dir) but resolved from a
        // worktree (write-dir set at close-out) must snapshot into the worktree —
        // so the record travels with the agent's branch — not the base repo.
        let dir = tempdir().unwrap();
        let base = tempdir().unwrap();
        let worktree = tempdir().unwrap();
        let base_root = base.path().canonicalize().unwrap();
        let worktree_root = worktree.path().canonicalize().unwrap();
        let wt = worktree_root.to_string_lossy().into_owned();
        let mut threads = Threads::open(&dir.path().join("threads.json")).unwrap();

        let id = open_thread_id(
            &mut threads,
            "audit the dispatch path",
            &base_root.to_string_lossy(),
        );
        let msg = threads
            .thread_mutation(
                &ThreadParams {
                    id: Some(id.clone()),
                    note: Some("found the bug".into()),
                    ..params("resolve")
                },
                Some(wt.as_str()),
            )
            .unwrap()
            .message;

        let in_worktree = worktree_root
            .join(".bbox")
            .join("record")
            .join(format!("{id}.json"));
        let in_base = base_root
            .join(".bbox")
            .join("record")
            .join(format!("{id}.json"));
        assert!(
            in_worktree.exists(),
            "record should land in the worktree: {}",
            in_worktree.display()
        );
        assert!(!in_base.exists(), "record must NOT land in the base repo");
        // Rider is worktree-relative and actionable from the worktree cwd.
        assert!(
            msg.contains("git add .bbox/record/") && msg.contains(&format!("{id}.json")),
            "rider should be worktree-relative: {msg}"
        );
    }

    #[test]
    fn resolve_falls_back_to_base_when_worktree_record_dir_is_gone() {
        // Opened from a worktree (record_dir stored), worktree later removed,
        // resolved with no write-dir → fall back to base rather than silent loss.
        let dir = tempdir().unwrap();
        let base = tempdir().unwrap();
        let base_root = base.path().canonicalize().unwrap();
        let gone = base_root.join("removed-worktree"); // never created → not a dir
        let mut threads = Threads::open(&dir.path().join("threads.json")).unwrap();

        // Open WITH a (now-absent) worktree write-dir, base also bound as scope.
        let open_msg = threads
            .thread_mutation(
                &ThreadParams {
                    topic: Some("x".into()),
                    project: Some(base_root.to_string_lossy().into_owned()),
                    ..params("open")
                },
                Some(gone.to_string_lossy().as_ref()),
            )
            .unwrap();
        let id = open_msg
            .message
            .split_whitespace()
            .nth(2)
            .unwrap()
            .to_string();

        let msg = threads
            .thread(&ThreadParams {
                id: Some(id.clone()),
                ..params("resolve")
            })
            .unwrap();
        assert!(
            base_root
                .join(".bbox")
                .join("record")
                .join(format!("{id}.json"))
                .exists(),
            "stale worktree record_dir should fall back to base, not silently drop"
        );
        assert!(
            msg.contains(".bbox/record/"),
            "rider expected on fallback write: {msg}"
        );
    }

    #[test]
    fn resolve_message_riders_the_record_path_for_commit() {
        let dir = tempdir().unwrap();
        let repo = tempdir().unwrap();
        let repo_root = repo.path().canonicalize().unwrap();
        let mut threads = Threads::open(&dir.path().join("threads.json")).unwrap();
        let id = open_thread_id(
            &mut threads,
            "tier the allocator",
            &repo_root.to_string_lossy(),
        );

        let msg = threads
            .thread(&ThreadParams {
                id: Some(id.clone()),
                ..params("resolve")
            })
            .unwrap();

        // The response surfaces the written record as repo-relative, with a
        // git-add hint, so the caller commits it instead of treating it as
        // untracked exhaust.
        let rel = format!(".bbox/record/{id}.json");
        assert!(
            msg.contains(&rel),
            "resolve message must name the record path repo-relative: {msg}"
        );
        assert!(
            msg.contains(&format!("git add {rel}")),
            "resolve message must hint the git add: {msg}"
        );
    }

    #[test]
    fn repo_artifact_rider_renders_relative_path_and_add_hint() {
        let root = "/repo/x";
        let path = Path::new("/repo/x/.bbox/record/thread-abc.json");
        let rider = crate::util::repo_artifact_rider(root, path);
        assert!(rider.contains(".bbox/record/thread-abc.json"));
        assert!(rider.contains("git add .bbox/record/thread-abc.json"));
        assert!(
            !rider.contains("/repo/x/.bbox"),
            "must be repo-relative, not absolute"
        );
    }

    fn set_last_activity(store_path: &Path, thread_id: &str, last_activity: &str) {
        let raw = fs::read_to_string(store_path).unwrap();
        let mut store: ThreadStore = serde_json::from_str(&raw).unwrap();
        let thread = store
            .threads
            .iter_mut()
            .find(|t| t.id == thread_id)
            .unwrap();
        thread.last_activity = last_activity.to_string();
        fs::write(store_path, serde_json::to_string_pretty(&store).unwrap()).unwrap();
    }

    #[test]
    fn thread_list_status_filters_lifecycle_not_idle_age() {
        let dir = tempdir().unwrap();
        let store_path = dir.path().join("threads.json");
        let mut threads = Threads::open(&store_path).unwrap();
        let created = threads
            .thread(&ThreadParams {
                action: "open".into(),
                topic: Some("new work".into()),
                project: Some("/repo/x".into()),
                name: Some("fresh".into()),
                id: None,
                session_id: None,
                provider: None,
                session_name: None,
                handoff_doc: None,
                note: None,
                target: None,
                target_type: None,
                edge: None,
                promoted_to: None,
                kind: None,
                origin: None,
            })
            .unwrap();
        let thread_id = created.split_whitespace().nth(2).unwrap().to_string();
        threads
            .thread(&ThreadParams {
                action: "continue".into(),
                id: Some(thread_id),
                name: None,
                topic: None,
                project: None,
                session_id: None,
                provider: None,
                session_name: None,
                handoff_doc: None,
                note: None,
                target: None,
                target_type: None,
                edge: None,
                promoted_to: None,
                kind: None,
                origin: None,
            })
            .unwrap();

        let out = threads
            .thread_list(&ThreadListParams {
                status: Some("active".into()),
                project: None,
                name: None,
                min_idle_days: None,
                include_resolved: None,
                kind: None,
                include_workflows: None,
            })
            .unwrap();

        assert!(out.contains("| active |"));
        assert!(out.contains("new work"));
    }

    #[test]
    fn thread_list_min_idle_days_filters_by_age() {
        let dir = tempdir().unwrap();
        let store_path = dir.path().join("threads.json");
        let mut threads = Threads::open(&store_path).unwrap();
        let created = threads
            .thread(&ThreadParams {
                action: "open".into(),
                topic: Some("old work".into()),
                project: Some("/repo/x".into()),
                name: Some("aged".into()),
                id: None,
                session_id: None,
                provider: None,
                session_name: None,
                handoff_doc: None,
                note: None,
                target: None,
                target_type: None,
                edge: None,
                promoted_to: None,
                kind: None,
                origin: None,
            })
            .unwrap();
        let thread_id = created.split_whitespace().nth(2).unwrap().to_string();
        set_last_activity(&store_path, &thread_id, "2026-01-01T00:00:00Z");
        let threads = Threads::open(&store_path).unwrap();

        let out = threads
            .thread_list(&ThreadListParams {
                status: None,
                project: None,
                name: None,
                min_idle_days: Some(7),
                include_resolved: None,
                kind: None,
                include_workflows: None,
            })
            .unwrap();

        assert!(out.contains("old work"));
    }

    #[test]
    fn thread_list_hides_workflow_origin_by_default() {
        let dir = tempdir().unwrap();
        let store_path = dir.path().join("threads.json");
        let mut threads = Threads::open(&store_path).unwrap();
        threads
            .thread(&ThreadParams {
                action: "open".into(),
                topic: Some("manual work".into()),
                project: Some("/repo/x".into()),
                name: Some("manual".into()),
                id: None,
                session_id: None,
                provider: None,
                session_name: None,
                handoff_doc: None,
                note: None,
                target: None,
                target_type: None,
                edge: None,
                promoted_to: None,
                kind: Some("work_item".into()),
                origin: None,
            })
            .unwrap();
        threads
            .thread(&ThreadParams {
                action: "open".into(),
                topic: Some("workflow arc: hidden".into()),
                project: Some("/repo/x".into()),
                name: Some("wf-hidden".into()),
                id: None,
                session_id: None,
                provider: None,
                session_name: None,
                handoff_doc: None,
                note: None,
                target: None,
                target_type: None,
                edge: None,
                promoted_to: None,
                kind: Some("work_item".into()),
                origin: Some("workflow".into()),
            })
            .unwrap();

        let default_out = threads
            .thread_list(&ThreadListParams {
                status: Some("open".into()),
                project: Some("/repo/x".into()),
                name: None,
                min_idle_days: None,
                include_resolved: None,
                kind: None,
                include_workflows: None,
            })
            .unwrap();
        assert!(default_out.contains("manual work"));
        assert!(!default_out.contains("workflow arc: hidden"));

        let explicit_out = threads
            .thread_list(&ThreadListParams {
                status: Some("open".into()),
                project: Some("/repo/x".into()),
                name: None,
                min_idle_days: None,
                include_resolved: None,
                kind: None,
                include_workflows: Some(true),
            })
            .unwrap();
        assert!(explicit_out.contains("manual work"));
        assert!(explicit_out.contains("workflow arc: hidden"));
    }
}
