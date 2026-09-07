use std::fs;
use std::path::{Path, PathBuf};

use std::str::FromStr;

use anyhow::{Context, Result};
use rmcp::schemars;
use serde::{Deserialize, Serialize};

use bbox_corpus_core::project_selector::project_scope_matches;
use bbox_stores::store_persister::StoreSnapshot;

// ── embed-sink hook ────────────────────────────────────────────────
//
// The daemon registers `embed_queue::enqueue_thread` here at SharedState
// construction (mirrors `index::embed_hook`). Inverting the dependency
// keeps this store below the embedding pipeline in the crate DAG; before
// registration (or in tests without one) embed scheduling is a no-op,
// matching the uninstalled-queue behavior of `embed_queue::enqueue`.
static THREAD_EMBED_HOOK: std::sync::OnceLock<fn(&Thread)> = std::sync::OnceLock::new();

/// Register the embed sink for thread mutations. Idempotent; first
/// registration wins.
pub fn register_thread_embed_hook(hook: fn(&Thread)) {
    let _ = THREAD_EMBED_HOOK.set(hook);
}

fn enqueue_thread_embed(thread: &Thread) {
    if let Some(hook) = THREAD_EMBED_HOOK.get() {
        hook(thread);
    }
}

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
    /// Internal, not part of the MCP schema: the resolving authority's
    /// project id. Set by the daemon adapter from the resolver, never
    /// accepted from the wire, so identity cannot be caller-asserted.
    #[serde(skip)]
    pub project_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
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
    /// Project id from the resolver. When both this and a row carry an
    /// id, the id decides and the path predicate is not consulted.
    #[serde(default)]
    pub project_id: Option<String>,
    /// Internal, not part of the MCP schema: historical path keys the
    /// host-local `LegacyPathBinding` ledger maps to this query's project
    /// (plan §8.2 catalog-mode arm), so path-only rows written before
    /// attachment relocation stay visible. Empty on the bridge, which has no
    /// ledger. Set by the daemon adapter, never accepted from the wire.
    #[serde(skip)]
    #[schemars(skip)]
    pub project_ledger_paths: Vec<String>,
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
    /// Resolving authority's project id, stamped on write. Absent on rows
    /// written before the catalog cut: those stay on the path lane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// Capture live thread rows that retain the legacy literal project selector.
pub fn capture_project_catalog_owner_snapshot(
    store_path: &Path,
    limits: bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotLimitsV1,
) -> std::result::Result<
    bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotV1,
    bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotError,
> {
    use bbox_corpus_core::project_catalog_snapshot::{
        LegacyProjectSelectorKindV1, OwnerSnapshotRowV1, capture_json_owner,
    };

    capture_json_owner(
        store_path,
        "thread",
        "thread:central-json",
        limits,
        |bytes| {
            let store: ThreadStore = serde_json::from_slice(bytes).map_err(|_| ())?;
            Ok(store
                .threads
                .into_iter()
                .filter_map(|thread| {
                    let selector = thread.project.trim().to_string();
                    (!selector.is_empty()).then(|| {
                        OwnerSnapshotRowV1::legacy_selector(
                            thread.id,
                            LegacyProjectSelectorKindV1::Project,
                            selector,
                        )
                    })
                })
                .collect())
        },
    )
}

/// Stamp one thread row with its stable project id, the write-back inverse of
/// [`capture_project_catalog_owner_snapshot`]. Idempotent: a row already
/// carrying this exact id reports `AlreadyStamped` without writing.
pub fn stamp_project_catalog_owner_row(
    store_path: &Path,
    source_row_id: &str,
    expected_members: &bbox_corpus_core::project_catalog_snapshot::LegacySelectorMembersV1,
    project_id: &str,
    limits: bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotLimitsV1,
) -> std::result::Result<
    bbox_corpus_core::project_catalog_snapshot::OwnerRowStampOutcomeV1,
    bbox_corpus_core::project_catalog_snapshot::OwnerRowStampError,
> {
    bbox_corpus_core::project_catalog_snapshot::ensure_singleton_member_evidence(
        source_row_id,
        expected_members,
    )?;
    use bbox_corpus_core::project_catalog_snapshot::{stamp_json_array_row, stamp_json_owner_row};

    stamp_json_owner_row(
        store_path,
        "thread",
        "thread:central-json",
        limits,
        |bytes| stamp_json_array_row(bytes, "threads", "id", source_row_id, project_id),
    )
}

/// Read the stable project ids of MANY central thread rows, the VERIFY half of
/// [`stamp_project_catalog_owner_row`].
///
/// Read-only by construction: the backfill's verify proves that the rows an
/// applied plan claims to have stamped really carry the project id the ledger
/// binds them to, and a verify that could write would be proving its own work.
/// Batched over the whole requested set, so verifying this owner costs ONE
/// locked capture and answers every row from ONE durable snapshot.
pub fn read_project_catalog_owner_rows(
    store_path: &Path,
    rows: &bbox_corpus_core::project_catalog_snapshot::OwnerRowRequestV1,
    limits: bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotLimitsV1,
) -> std::result::Result<
    bbox_corpus_core::project_catalog_snapshot::OwnerRowBatchV1,
    bbox_corpus_core::project_catalog_snapshot::OwnerRowStampError,
> {
    bbox_corpus_core::project_catalog_snapshot::ensure_singleton_member_evidence_batch(rows)?;
    let source_row_ids = &rows
        .keys()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    use bbox_corpus_core::project_catalog_snapshot::{
        read_json_array_rows_project_id, read_json_owner_rows,
    };

    read_json_owner_rows(
        store_path,
        "thread",
        "thread:central-json",
        limits,
        |bytes| read_json_array_rows_project_id(bytes, "threads", "id", source_row_ids),
    )
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
    bbox_corpus_core::json_store::atomic_write_json_locked(&path, &record)?;
    Ok(Some((root, path)))
}

/// Load every committed thread record under `<project_dir>/.bbox/record/`.
/// These are durable snapshots of settled threads that travel with the repo;
/// on a clone (where the live thread store doesn't carry them) they are the
/// only trace of past investigations.
pub fn load_repo_records(project_dir: &Path) -> Vec<ThreadRecord> {
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
    store: ThreadStore,
    /// Where this store was loaded from — surfaced in lookup diagnostics so
    /// a caller chasing a "Thread not found" for an id another surface just
    /// listed can tell which store answered (gap-518d7215: list/resolve
    /// divergence is a two-stores symptom, not a lookup bug).
    store_path: PathBuf,
}

impl StoreSnapshot for Threads {
    type Snapshot = ThreadStore;

    fn snapshot(&self) -> Result<Self::Snapshot> {
        Ok(self.store.clone())
    }
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
            store,
            store_path: store_path.to_path_buf(),
        })
    }

    /// One-line store-identity breadcrumb for lookup diagnostics: which
    /// store file answered and how much it holds. Lookup is global by id —
    /// `project` never narrows it — so a miss for an id some listing just
    /// returned means that listing came from a DIFFERENT store/daemon, not
    /// that a scope filter excluded it.
    fn store_identity(&self) -> String {
        format!(
            "store {} holds {} thread(s)",
            self.store_path.display(),
            self.store.threads.len()
        )
    }

    fn now_iso() -> String {
        bbox_util::util::now_iso()
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
            for thread in &self.store.threads {
                if thread.project == new_project {
                    enqueue_thread_embed(thread);
                }
            }
        }
        Ok(updated)
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
                message: serde_json::to_string(&self.thread_get_page(p, None, 20, 0)?)?,
                changed_thread: None,
                changed_edges: false,
            });
        }
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
            project_id: p.project_id.clone(),
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
        enqueue_thread_embed(&thread);

        Ok(ThreadMutation {
            message: format!("Thread created: {} — \"{}\"", id, topic),
            changed_thread: Some(thread),
            changed_edges,
        })
    }

    /// The id/name finder shared by the get paths. Accepts bare `<8hex>` as
    /// a fallback for canonical `thread-<8hex>` — matches the schema regex
    /// and the NoteResolveParams policy.
    fn find_thread<'a>(&'a self, p: &ThreadParams) -> Result<&'a Thread> {
        let thread = if let Some(id) = p.id.as_deref() {
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
        thread.context("Thread not found")
    }

    /// Canonical thread id for an exact-read scope: binds the content-bound
    /// cursor to the thread identity even when the caller selected by name.
    pub fn thread_ref(&self, p: &ThreadParams) -> Result<String> {
        self.find_thread(p).map(|thread| thread.id.clone())
    }

    /// Exact thread-note read (audit A04 recovery): 1-based index, matching
    /// the `index` field of the `detail=notes` page rows.
    pub fn thread_note(&self, p: &ThreadParams, index: usize) -> Result<(String, String)> {
        let thread = self.find_thread(p)?;
        let note = thread.notes.get(index.wrapping_sub(1)).ok_or_else(|| {
            anyhow::anyhow!(
                "thread {} has no note at index {index} (1-based; it holds {})",
                thread.id,
                thread.notes.len()
            )
        })?;
        Ok((thread.id.clone(), note.clone()))
    }

    /// Exact handoff-doc read (audit A04 recovery).
    pub fn thread_handoff(&self, p: &ThreadParams) -> Result<(String, String)> {
        let thread = self.find_thread(p)?;
        let doc = thread
            .handoff_doc
            .as_deref()
            .filter(|doc| !doc.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("thread {} has no handoff doc", thread.id))?;
        Ok((thread.id.clone(), doc.to_string()))
    }

    /// Exact caller-facing metadata. Internal record storage paths stay private;
    /// note bodies have their own stable per-note reader.
    pub fn thread_metadata(&self, p: &ThreadParams) -> Result<(String, serde_json::Value)> {
        let thread = self.find_thread(p)?;
        let mut value = serde_json::to_value(thread)?;
        let object = value.as_object_mut().expect("thread object");
        object.remove("record_dir");
        object.remove("notes");
        object.insert("notes_count".into(), serde_json::json!(thread.notes.len()));
        Ok((thread.id.clone(), value))
    }

    /// Bounded thread get (audit A04): the default summary carries counts
    /// and 200-char previews only — never the full session/edge/note
    /// history, which was a 23KB+ unbounded dump. History lives behind
    /// `detail=notes|sessions|edges` pages; exact recovery reads
    /// (`detail=note`, `detail=handoff`) are paged by the daemon adapter
    /// through the content-bound body cursor.
    pub fn thread_get_page(
        &self,
        p: &ThreadParams,
        detail: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<serde_json::Value> {
        use bbox_corpus_core::response_page as page_lib;
        let thread = self.find_thread(p)?;
        let mut rows = match detail {
            None | Some("summary") => return Ok(self.thread_summary_value(thread)),
            Some("notes") => thread
                .notes
                .iter()
                .enumerate()
                .map(|(i, note)| {
                    let mut row = serde_json::json!({
                        "index": i + 1,
                        "note": note,
                    });
                    page_lib::preview_field(&mut row, "note", 200);
                    row
                })
                .collect::<Vec<_>>(),
            Some("sessions") => thread
                .sessions
                .iter()
                .map(|session| {
                    let mut row = serde_json::json!({
                        "session_id": session.session_id,
                        "provider": session.provider,
                        "linked_at": session.linked_at,
                    });
                    if let Some(name) = &session.name {
                        row["name"] = serde_json::json!(name);
                    }
                    for field in ["name", "session_id", "provider"] {
                        page_lib::preview_field(&mut row, field, 200);
                    }
                    row
                })
                .collect::<Vec<_>>(),
            Some("edges") => thread
                .edges
                .iter()
                .map(|edge| {
                    let mut row = serde_json::json!({
                        "kind": edge.kind,
                        "target": edge.target,
                        "target_type": edge.target_type,
                        "created_at": edge.created_at,
                    });
                    if let Some(note) = &edge.note {
                        row["note"] = serde_json::json!(note);
                    }
                    page_lib::preview_field(&mut row, "note", 200);
                    page_lib::preview_field(&mut row, "target", 200);
                    row
                })
                .collect::<Vec<_>>(),
            Some("note" | "handoff") => anyhow::bail!(
                "detail={detail:?} is an exact read served with note_index/cursor paging; pass it through the bbox_thread adapter"
            ),
            Some(other) => {
                anyhow::bail!("unknown detail: {other} (use notes, sessions, edges, note, handoff)")
            }
        };
        // Rows are already ordered by their stable append position; the sort
        // keeps the invariant explicit if the collector ever changes.
        rows.sort_by_key(|row| row["index"].as_u64().unwrap_or_default());
        let field = detail.expect("detail page");
        let mut page = page_lib::collection_page(rows, field, Some(limit), Some(offset))?;
        page["order"] = serde_json::json!(format!("{field}_append_index_asc"));
        page["pagination"] = serde_json::json!(
            "append_only_offset: existing rows keep their index; new rows append; re-query from offset 0 after continuing a thread"
        );
        page["detail_hint"] = serde_json::json!(if field == "notes" {
            format!(
                "bbox_thread(action=get,id={},detail=note,note_index=<1-based>)",
                thread.id
            )
        } else {
            format!(
                "bbox_thread(action=get,id={},detail=metadata); follow body.next_cursor with cursor",
                thread.id
            )
        });
        Ok(page)
    }

    /// The bounded default summary: identity fields, counts, and previews.
    fn thread_summary_value(&self, thread: &Thread) -> serde_json::Value {
        use bbox_corpus_core::response_page as page_lib;
        let mut row = serde_json::json!({
            "id": thread.id,
            "topic": thread.topic,
            "status": thread.status,
            "created_at": thread.created_at,
            "last_activity": thread.last_activity,
            "counts": {
                "sessions": thread.sessions.len(),
                "notes": thread.notes.len(),
                "edges": thread.edges.len(),
            },
        });
        if let Some(name) = &thread.name {
            row["name"] = serde_json::json!(name);
        }
        if let Some(kind) = thread.kind {
            row["kind"] = serde_json::json!(kind);
        }
        if let Some(origin) = thread.origin {
            row["origin"] = serde_json::json!(origin);
        }
        if let Some(id) = &thread.project_id {
            row["project_id"] = serde_json::json!(id);
        } else if !thread.project.is_empty() {
            row["project_selector"] = serde_json::json!(thread.project);
        }
        if let Some(resolved) = &thread.resolved_at {
            row["resolved_at"] = serde_json::json!(resolved);
        }
        if let Some(promoted) = &thread.promoted_to {
            row["promoted_to"] = serde_json::json!(promoted);
        }
        if let Some(latest) = thread.notes.last() {
            row["latest_note_index"] = serde_json::json!(thread.notes.len());
            row["latest_note"] = serde_json::json!(latest);
        }
        if let Some(doc) = &thread.handoff_doc {
            row["handoff_doc"] = serde_json::json!(doc);
        }
        page_lib::preview_field(&mut row, "topic", 200);
        page_lib::preview_field(&mut row, "name", 200);
        page_lib::preview_field(&mut row, "latest_note", 200);
        page_lib::preview_field(&mut row, "handoff_doc", 200);
        page_lib::preview_field(&mut row, "project_selector", 200);
        page_lib::preview_field(&mut row, "promoted_to", 200);
        serde_json::json!({
            "thread": row,
            "pagination": "summary is bounded; history pages are append-only offsets",
            "detail_hint": format!(
                "bbox_thread(action=get,id={},detail=notes|sessions|edges|note(note_index=N)|handoff|metadata); metadata recovers topic, name, sessions and edges",
                thread.id
            ),
        })
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
        enqueue_thread_embed(&thread_for_embed);

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
                "Thread not found: {id} (expected `thread-<8hex>`, e.g. `thread-7f01324e`). \
                 Lookup is global by id — `project` never narrows it; {}. \
                 If a listing just returned this id, that listing came from a different store/daemon.",
                self.store_identity()
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
            anyhow::bail!(
                "Thread not found: {name}. Lookup is global by name — \
                 `project` never narrows it; {}.",
                self.store_identity()
            );
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

        enqueue_thread_embed(&thread_for_embed);

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

        // Workflow-origin threads are transient arc scaffolding: their
        // audit trail is the arc event log, not a committed snapshot.
        // Resolving them (typically discarding stale wf-* exhaust) must
        // not emit repo-owned .bbox/record files for reviewers to wade
        // through (gap-8500c221). Promotion still writes a record — that
        // is the operator deliberately elevating the thread to durable.
        let record_rider = if thread_for_embed.origin == Some(ThreadOrigin::Workflow) {
            None
        } else {
            match write_thread_record(&thread_for_embed) {
                Ok(Some((root, path))) => Some(bbox_util::util::repo_artifact_rider(
                    &root.to_string_lossy(),
                    &path,
                )),
                Ok(None) => None,
                Err(e) => {
                    tracing::warn!("thread record write for {id}: {e:#}");
                    None
                }
            }
        };
        enqueue_thread_embed(&thread_for_embed);

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

        let record_rider = match write_thread_record(&thread_for_embed) {
            Ok(Some((root, path))) => Some(bbox_util::util::repo_artifact_rider(
                &root.to_string_lossy(),
                &path,
            )),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!("thread record write for {id}: {e:#}");
                None
            }
        };
        enqueue_thread_embed(&thread_for_embed);

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

        enqueue_thread_embed(&thread_for_embed);

        Ok(ThreadMutation {
            message: format!("Thread {id} renamed to \"{new_name}\" (topic: {topic})"),
            changed_thread: Some(thread_for_embed),
            changed_edges: false,
        })
    }

    // ── blackbox_thread_list (query) ───────────────────────────────

    fn matching_threads(&self, p: &ThreadListParams) -> Result<Vec<&Thread>> {
        let status_filter = p
            .status
            .as_deref()
            .map(ThreadStatus::from_str)
            .transpose()
            .map_err(|_| {
                anyhow::anyhow!("Unknown thread status. Use: open, active, resolved, promoted")
            })?;
        let project_filter = p.project.as_deref();
        let project_id_filter = p.project_id.as_deref();
        let ledger_lower: Vec<String> = p
            .project_ledger_paths
            .iter()
            .map(|path| path.to_lowercase())
            .collect();
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
            if let Some(target) = &status_filter {
                if thread.status != *target {
                    continue;
                }
            } else if !include_resolved {
                // Default: exclude resolved and promoted
                if thread.status == ThreadStatus::Resolved
                    || thread.status == ThreadStatus::Promoted
                {
                    continue;
                }
            }

            // Project filter. Dual-read (plan §8.2): ids on both sides decide,
            // whatever the paths say; either side missing an id keeps the path
            // predicate. The ledger arm is catalog-mode only and matches a
            // path-only row still keyed under a historical path of this
            // project.
            if (project_filter.is_some() || project_id_filter.is_some())
                && !project_scope_matches(thread.project_id.as_deref(), project_id_filter, || {
                    let row_project = thread.project.to_lowercase();
                    project_filter
                        .is_some_and(|filter| row_project.contains(&filter.to_lowercase()))
                        || ledger_lower
                            .iter()
                            .any(|historical| row_project.contains(historical.as_str()))
                })
            {
                continue;
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

        results.sort_by(|a, b| {
            b.last_activity
                .cmp(&a.last_activity)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(results)
    }

    /// Bounded MCP discovery view. Full thread context stays on the exact get.
    pub fn thread_list_page(
        &self,
        p: &ThreadListParams,
        limit: usize,
        offset: usize,
    ) -> Result<serde_json::Value> {
        let results = self.matching_threads(p)?;
        let total = results.len();
        let limit = limit.clamp(1, 100);
        let threads: Vec<_> = results
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|thread| {
                let topic: String = thread.topic.chars().take(200).collect();
                let mut row = serde_json::json!({
                    "id": thread.id, "topic": topic, "status": thread.status,
                    "last_activity": thread.last_activity,
                });
                if topic.len() < thread.topic.len() {
                    row["topic_truncated"] = serde_json::json!(true);
                }
                if let Some(name) = &thread.name {
                    row["name"] = serde_json::json!(name);
                }
                if let Some(kind) = thread.kind {
                    row["kind"] = serde_json::json!(kind);
                }
                if let Some(origin) = thread.origin {
                    row["origin"] = serde_json::json!(origin);
                }
                if let Some(id) = &thread.project_id {
                    row["project_id"] = serde_json::json!(id);
                } else if !thread.project.is_empty() {
                    row["project_selector"] = serde_json::json!(thread.project);
                }
                bbox_corpus_core::response_page::preview_field(&mut row, "name", 200);
                row
            })
            .collect();
        let next_offset = offset.saturating_add(threads.len());
        bbox_corpus_core::response_page::bound_page(
            serde_json::json!({
                "threads": threads, "total": total, "offset": offset, "limit": limit,
                "next_offset": (next_offset < total).then_some(next_offset),
                "order": "last_activity_desc,id_asc",
                "pagination": "live_offset: thread activity reorders rows between pages; re-query from offset 0 after mutating threads",
                "detail_hint": "bbox_thread(action=get,id=<id>) for a bounded summary; detail=notes|sessions|edges|note|handoff for paged history",
            }),
            "threads",
        )
    }

    pub fn thread_list(&self, p: &ThreadListParams) -> Result<String> {
        let mut results = self.matching_threads(p)?;
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if results.is_empty() {
            // Differentiate filter-miss from empty store: a caller staring at
            // an unexpected empty listing needs to know whether their filters
            // excluded everything or this store simply isn't the one that
            // holds their threads (gap-518d7215).
            return Ok(if self.store.threads.is_empty() {
                format!(
                    "No threads found (store {} is empty).",
                    self.store_path.display()
                )
            } else {
                format!(
                    "No threads found ({}; filters matched none).",
                    self.store_identity()
                )
            });
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
    use bbox_stores::store_persister::StorePersister;
    use parking_lot::RwLock;
    use std::sync::Arc;
    use tempfile::tempdir;

    #[test]
    fn thread_summary_pages_are_bounded_and_do_not_expand_history() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut store = Threads::open(&root.join("threads.json")).unwrap();
        for i in (0..105).rev() {
            store.store.threads.push(serde_json::from_value(serde_json::json!({
                "id": format!("thread-{i:08x}"), "topic": "界".repeat(300), "project": "",
                "status": "open", "sessions": [], "notes": ["internal detail".repeat(1000)],
                "handoff_doc": "/private/owner/handoff.md", "created_at": "2026-01-01T00:00:00Z",
                "last_activity": "2026-01-01T00:00:00Z",
            })).unwrap());
        }
        let p = ThreadListParams::default();
        let first = store.thread_list_page(&p, 1000, 0).unwrap();
        let returned = first["threads"].as_array().unwrap().len();
        assert!(returned > 0 && returned <= 100);
        assert_eq!(first["next_offset"], returned);
        assert!(
            serde_json::to_vec(&first).unwrap().len()
                <= bbox_corpus_core::response_page::PAGE_BUDGET_BYTES
        );
        assert_eq!(first["total"], 105);
        assert_eq!(first["threads"][0]["id"], "thread-00000000");
        assert_eq!(
            first["threads"][0]["topic"]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            200
        );
        assert_eq!(first["threads"][0]["topic_truncated"], true);
        assert!(!first.to_string().contains("handoff.md"));
        assert!(!first.to_string().contains("internal detail"));
        let last = store.thread_list_page(&p, 100, 100).unwrap();
        assert_eq!(last["threads"].as_array().unwrap().len(), 5);
        assert_eq!(last["threads"][0]["id"], "thread-00000064");
        assert!(last["next_offset"].is_null());
        let empty = store.thread_list_page(&p, 20, usize::MAX).unwrap();
        assert_eq!(empty["threads"], serde_json::json!([]));
        assert_eq!(empty["total"], 105);
    }

    #[test]
    fn thread_summary_pages_apply_id_only_scope_and_legacy_ledger() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut store = Threads::open(&root.join("threads.json")).unwrap();
        for (i, (project_id, project)) in [
            (Some("11111111"), "/repo/relocated"),
            (Some("22222222"), "/repo/old-target"),
            (None, "/repo/old-target"),
            (None, "/repo/other"),
        ]
        .into_iter()
        .enumerate()
        {
            store.store.threads.push(serde_json::from_value(serde_json::json!({
                "id": format!("thread-{i:08x}"), "topic": "scoped", "project": project, "project_id": project_id,
                "status": "open", "sessions": [], "created_at": "2026-01-01T00:00:00Z",
                "last_activity": "2026-01-01T00:00:00Z",
            })).unwrap());
        }
        let mut p = ThreadListParams {
            project_id: Some("11111111".into()),
            project_ledger_paths: vec!["/repo/old-target".into()],
            ..Default::default()
        };
        let by_id = store.thread_list_page(&p, 20, 0).unwrap();
        assert_eq!(by_id["total"], 2);
        assert_eq!(by_id["threads"][0]["id"], "thread-00000000");
        assert_eq!(by_id["threads"][1]["id"], "thread-00000002");
        p.project = Some("/repo/current".into());
        assert_eq!(
            store.thread_list_page(&p, 20, 0).unwrap()["threads"],
            by_id["threads"]
        );
        p.project = None;
        p.project_id = Some("33333333".into());
        p.project_ledger_paths.clear();
        assert_eq!(store.thread_list_page(&p, 20, 0).unwrap()["total"], 0);
    }

    fn params(action: &str) -> ThreadParams {
        ThreadParams {
            action: action.into(),
            topic: None,
            project: None,
            project_id: None,
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

    #[tokio::test]
    async fn threads_round_trip_through_persister() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store_path = root.join("threads.json");
        let threads = Arc::new(RwLock::new(Threads::open(&store_path).unwrap()));
        let persister = StorePersister::spawn(
            "threads-test-roundtrip",
            threads.clone(),
            store_path.clone(),
        );

        let created = threads
            .write()
            .thread(&ThreadParams {
                action: "open".into(),
                topic: Some("persister-backed thread".into()),
                project: Some(root.to_string_lossy().into_owned()),
                project_id: None,
                name: Some("persisted".into()),
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
        let thread_id = created.split_whitespace().nth(2).unwrap().to_string();
        persister.request_durable().await.unwrap();

        let reopened = Threads::open(&store_path).unwrap();
        let listed = reopened
            .thread_list(&ThreadListParams {
                status: None,
                project: Some(root.to_string_lossy().into_owned()),
                project_id: None,
                project_ledger_paths: Vec::new(),
                name: None,
                min_idle_days: None,
                include_resolved: None,
                kind: None,
                include_workflows: None,
            })
            .unwrap();
        assert!(listed.contains(&thread_id));
        assert!(listed.contains("persister-backed thread"));
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

    /// Audit A04: the default get is a bounded summary — counts and 200-char
    /// previews only, never the full history dump that reached 23KB+.
    #[test]
    fn thread_get_summary_is_bounded_with_counts_and_previews() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut threads = Threads::open(&root.join("threads.json")).unwrap();
        let id = open_thread_id(&mut threads, "bounded get", "/repo/x");
        for i in 0..50 {
            threads
                .thread(&ThreadParams {
                    id: Some(id.clone()),
                    note: Some(format!("note {i}: {}", "詳細 🦀".repeat(120))),
                    ..params("continue")
                })
                .unwrap();
        }
        threads
            .thread(&ThreadParams {
                id: Some(id.clone()),
                session_id: Some("sess-1".into()),
                provider: Some("claude".into()),
                ..params("continue")
            })
            .unwrap();
        threads
            .thread(&ThreadParams {
                id: Some(id.clone()),
                handoff_doc: Some("/owner/handoff.md".into()),
                ..params("continue")
            })
            .unwrap();

        let page = threads
            .thread_get_page(
                &ThreadParams {
                    id: Some(id.clone()),
                    ..params("get")
                },
                None,
                20,
                0,
            )
            .unwrap();
        assert!(
            serde_json::to_vec(&page).unwrap().len() <= 4 * 1024,
            "summary must stay bounded: {page}"
        );
        let row = &page["thread"];
        assert_eq!(row["id"], id);
        assert_eq!(row["counts"]["notes"], 50);
        assert_eq!(row["counts"]["sessions"], 1);
        assert_eq!(row["counts"]["edges"], 0);
        assert_eq!(row["latest_note_index"], 50);
        assert!(
            row["latest_note"].as_str().unwrap().len() <= 200,
            "preview bounded"
        );
        assert_eq!(row["latest_note_truncated"], true);
        assert_eq!(row["handoff_doc"], "/owner/handoff.md");
        assert!(page["detail_hint"].as_str().unwrap().contains("detail="));

        // Bounded even with a huge topic: previewed, flagged.
        let mut wide = params("open");
        wide.topic = Some("トピック".repeat(2000));
        let created = threads.thread(&wide).unwrap();
        let wide_id = created.split_whitespace().nth(2).unwrap().to_string();
        let page = threads
            .thread_get_page(
                &ThreadParams {
                    id: Some(wide_id),
                    ..params("get")
                },
                None,
                20,
                0,
            )
            .unwrap();
        assert!(
            page["thread"]["topic"].as_str().unwrap().len() <= 200,
            "topic preview bounded"
        );
        assert_eq!(page["thread"]["topic_truncated"], true);
    }

    /// Audit A04: history pages are bounded, ordered by append index, and
    /// labeled append-only.
    #[test]
    fn thread_get_detail_pages_note_history_in_append_order() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut threads = Threads::open(&root.join("threads.json")).unwrap();
        let id = open_thread_id(&mut threads, "paged history", "/repo/x");
        for i in 1..=5 {
            threads
                .thread(&ThreadParams {
                    id: Some(id.clone()),
                    note: Some(format!("history note {i}: {}", "本文".repeat(150))),
                    ..params("continue")
                })
                .unwrap();
        }

        let page = threads
            .thread_get_page(
                &ThreadParams {
                    id: Some(id.clone()),
                    ..params("get")
                },
                Some("notes"),
                2,
                0,
            )
            .unwrap();
        assert_eq!(page["total"], 5);
        assert_eq!(page["count"], 2);
        assert_eq!(page["next_offset"], 2);
        assert_eq!(page["notes"][0]["index"], 1);
        assert_eq!(page["notes"][1]["index"], 2);
        assert_eq!(page["notes"][0]["note_truncated"], true);
        assert!(
            page["pagination"]
                .as_str()
                .unwrap()
                .starts_with("append_only_offset")
        );

        let next = threads
            .thread_get_page(
                &ThreadParams {
                    id: Some(id.clone()),
                    ..params("get")
                },
                Some("notes"),
                2,
                2,
            )
            .unwrap();
        assert_eq!(next["notes"][0]["index"], 3);
        assert_eq!(next["notes"][1]["index"], 4);
        assert_eq!(next["next_offset"], 4);

        // Sessions and edges pages exist and stay bounded.
        let sessions = threads
            .thread_get_page(
                &ThreadParams {
                    id: Some(id.clone()),
                    ..params("get")
                },
                Some("sessions"),
                20,
                0,
            )
            .unwrap();
        assert_eq!(sessions["total"], 0);
        let edges = threads
            .thread_get_page(
                &ThreadParams {
                    id: Some(id.clone()),
                    ..params("get")
                },
                Some("edges"),
                20,
                0,
            )
            .unwrap();
        assert_eq!(edges["total"], 0);

        // Unknown detail names the valid set.
        let err = threads
            .thread_get_page(
                &ThreadParams {
                    id: Some(id),
                    ..params("get")
                },
                Some("bogus"),
                20,
                0,
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("unknown detail"),
            "invalid detail must error: {err}"
        );
    }

    /// Audit A04 exact recovery: the getters return complete stored text for
    /// the adapter's content-bound pages.
    #[test]
    fn thread_exact_getters_return_complete_text() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut threads = Threads::open(&root.join("threads.json")).unwrap();
        let id = open_thread_id(&mut threads, "exact reads", "/repo/x");
        let note_text = format!("exact note {}", "正確 🦀".repeat(300));
        threads
            .thread(&ThreadParams {
                id: Some(id.clone()),
                note: Some(note_text.clone()),
                ..params("continue")
            })
            .unwrap();
        threads
            .thread(&ThreadParams {
                id: Some(id.clone()),
                handoff_doc: Some("/owner/handoff.md".into()),
                ..params("continue")
            })
            .unwrap();

        let (by_id, note) = threads
            .thread_note(
                &ThreadParams {
                    id: Some(id.clone()),
                    ..params("get")
                },
                1,
            )
            .unwrap();
        assert_eq!(by_id, id);
        assert_eq!(note.len(), note_text.len());

        // Name selection binds to the canonical id.
        threads
            .thread(&ThreadParams {
                id: Some(id.clone()),
                name: Some("named-thread".into()),
                ..params("rename")
            })
            .unwrap();
        let (by_name, _) = threads
            .thread_note(
                &ThreadParams {
                    name: Some("named-thread".into()),
                    ..params("get")
                },
                1,
            )
            .unwrap();
        assert_eq!(by_name, id);

        let handoff = threads
            .thread_handoff(&ThreadParams {
                id: Some(id.clone()),
                ..params("get")
            })
            .unwrap();
        assert_eq!(handoff, (id.clone(), "/owner/handoff.md".to_string()));

        // Out-of-range and missing-handoff are explicit errors.
        let err = threads
            .thread_note(
                &ThreadParams {
                    id: Some(id.clone()),
                    ..params("get")
                },
                2,
            )
            .unwrap_err();
        assert!(err.to_string().contains("no note at index 2"), "{err}");
        let id2 = open_thread_id(&mut threads, "no handoff", "/repo/x");
        let err = threads
            .thread_handoff(&ThreadParams {
                id: Some(id2),
                ..params("get")
            })
            .unwrap_err();
        assert!(err.to_string().contains("no handoff doc"), "{err}");
    }

    /// Not-found and empty-list responses carry store identity, so a caller
    /// whose listing came from a DIFFERENT daemon/store (gap-518d7215) can
    /// see the divergence instead of retrying with project params that the
    /// global-by-id lookup never consults.
    #[test]
    fn lookup_miss_and_empty_list_surface_store_identity() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store_path = root.join("threads.json");
        let mut threads = Threads::open(&store_path).unwrap();

        let err = threads
            .thread(&ThreadParams {
                id: Some("thread-deadbeef".into()),
                project: Some("/repo/x".into()),
                ..params("resolve")
            })
            .unwrap_err()
            .to_string();
        assert!(err.contains("thread-deadbeef"), "{err}");
        assert!(err.contains("`project` never narrows it"), "{err}");
        assert!(err.contains(store_path.to_str().unwrap()), "{err}");
        assert!(err.contains("holds 0 thread(s)"), "{err}");

        // Empty store vs filters-matched-none are distinguishable.
        let empty = threads
            .thread_list(&ThreadListParams {
                ..Default::default()
            })
            .unwrap();
        assert!(empty.contains("is empty"), "{empty}");
        assert!(empty.contains(store_path.to_str().unwrap()), "{empty}");

        open_thread_id(&mut threads, "real topic", "/repo/x");
        let filtered = threads
            .thread_list(&ThreadListParams {
                project: Some("/repo/other".into()),
                ..Default::default()
            })
            .unwrap();
        assert!(filtered.contains("filters matched none"), "{filtered}");
        assert!(filtered.contains("holds 1 thread(s)"), "{filtered}");
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
    fn resolve_skips_record_for_workflow_origin_threads() {
        // gap-8500c221: discarding transient wf-* arc scaffolding must
        // not leave repo-owned .bbox/record files behind.
        let dir = tempdir().unwrap();
        let repo = tempdir().unwrap();
        let repo_root = repo.path().canonicalize().unwrap();
        let mut threads = Threads::open(&dir.path().join("threads.json")).unwrap();

        let created = threads
            .thread(&ThreadParams {
                topic: Some("workflow arc: nightly-eval".into()),
                project: Some(repo_root.to_string_lossy().into_owned()),
                origin: Some("workflow".into()),
                ..params("open")
            })
            .unwrap();
        let id = created.split_whitespace().nth(2).unwrap().to_string();

        threads
            .thread(&ThreadParams {
                id: Some(id.clone()),
                note: Some("stale arc exhaust, discarding".into()),
                ..params("resolve")
            })
            .unwrap();

        let record_path = repo_root
            .join(".bbox")
            .join("record")
            .join(format!("{id}.json"));
        assert!(
            !record_path.exists(),
            "workflow-origin thread must not snapshot to {}",
            record_path.display()
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
        let rider = bbox_util::util::repo_artifact_rider(root, path);
        assert!(rider.contains(".bbox/record/thread-abc.json"));
        assert!(rider.contains("git add .bbox/record/thread-abc.json"));
        assert!(
            !rider.contains("/repo/x/.bbox"),
            "must be repo-relative, not absolute"
        );
    }

    fn set_last_activity(threads: &mut Threads, thread_id: &str, last_activity: &str) {
        let thread = threads
            .store
            .threads
            .iter_mut()
            .find(|t| t.id == thread_id)
            .unwrap();
        thread.last_activity = last_activity.to_string();
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
                project_id: None,
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
                project_id: None,
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
                project_id: None,
                project_ledger_paths: Vec::new(),
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
                project_id: None,
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
        set_last_activity(&mut threads, &thread_id, "2026-01-01T00:00:00Z");

        let out = threads
            .thread_list(&ThreadListParams {
                status: None,
                project: None,
                project_id: None,
                project_ledger_paths: Vec::new(),
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
                project_id: None,
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
                project_id: None,
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
                project_id: None,
                project_ledger_paths: Vec::new(),
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
                project_id: None,
                project_ledger_paths: Vec::new(),
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

    // ── Dual-read (plan §8.2) ────────────────────────────────────────────

    fn dual_read_thread(id: &str, project: &str, project_id: Option<&str>) -> Thread {
        Thread {
            id: id.into(),
            name: None,
            topic: "dual read topic".into(),
            project: project.into(),
            project_id: project_id.map(str::to_string),
            record_dir: None,
            status: ThreadStatus::Open,
            kind: None,
            origin: None,
            sessions: Vec::new(),
            handoff_doc: None,
            notes: Vec::new(),
            edges: Vec::new(),
            promoted_to: None,
            created_at: "2026-07-24T00:00:00Z".into(),
            last_activity: "2026-07-24T00:00:00Z".into(),
            resolved_at: None,
        }
    }

    #[test]
    fn thread_row_without_project_id_decodes_and_round_trips() {
        let legacy = serde_json::json!({
            "id": "thread-legacy",
            "topic": "t",
            "project": "/repo/old",
            "status": "open",
            "sessions": [],
            "created_at": "2026-07-24T00:00:00Z",
            "last_activity": "2026-07-24T00:00:00Z"
        });
        let thread: Thread = serde_json::from_value(legacy).unwrap();
        assert_eq!(thread.project_id, None);
        assert!(
            serde_json::to_value(&thread)
                .unwrap()
                .get("project_id")
                .is_none()
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("threads.json");
        let mut threads = Threads::open(&path).unwrap();
        threads.store.threads.push(thread);
        std::fs::write(&path, serde_json::to_string(&threads.store).unwrap()).unwrap();
        let reopened = Threads::open(&path).unwrap();
        assert_eq!(reopened.store.threads.len(), 1);
        assert_eq!(reopened.store.threads[0].project_id, None);
    }

    #[test]
    fn thread_project_id_match_wins_over_a_different_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut threads = Threads::open(&dir.path().join("threads.json")).unwrap();
        threads.store.threads.push(dual_read_thread(
            "thread-aaaaaaaa",
            "/repo/old",
            Some("abc12345"),
        ));

        let out = threads
            .thread_list(&ThreadListParams {
                project: Some("/repo/relocated".into()),
                project_id: Some("abc12345".into()),
                ..Default::default()
            })
            .unwrap();
        assert!(out.contains("thread-aaaaaaaa"), "id arm must match: {out}");
    }

    #[test]
    fn thread_without_ids_falls_back_to_the_exact_path_arm() {
        let dir = tempfile::tempdir().unwrap();
        let mut threads = Threads::open(&dir.path().join("threads.json")).unwrap();
        threads
            .store
            .threads
            .push(dual_read_thread("thread-bbbbbbbb", "/repo/old", None));

        let miss = threads
            .thread_list(&ThreadListParams {
                project: Some("/repo/relocated".into()),
                project_id: Some("abc12345".into()),
                ..Default::default()
            })
            .unwrap();
        assert!(
            !miss.contains("thread-bbbbbbbb"),
            "path arm must decide: {miss}"
        );

        let hit = threads
            .thread_list(&ThreadListParams {
                project: Some("/repo/old".into()),
                ..Default::default()
            })
            .unwrap();
        assert!(
            hit.contains("thread-bbbbbbbb"),
            "path arm must match: {hit}"
        );
    }

    #[test]
    fn thread_mismatched_ids_hide_the_row_at_the_same_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut threads = Threads::open(&dir.path().join("threads.json")).unwrap();
        threads.store.threads.push(dual_read_thread(
            "thread-cccccccc",
            "/repo/old",
            Some("abc12345"),
        ));

        // Same path key, different ids: the id decides against the row, so a
        // path reused after a retire-and-add cannot leak the old rows.
        let out = threads
            .thread_list(&ThreadListParams {
                project: Some("/repo/old".into()),
                project_id: Some("def67890".into()),
                ..Default::default()
            })
            .unwrap();
        assert!(
            !out.contains("thread-cccccccc"),
            "id mismatch must hide: {out}"
        );
    }

    #[test]
    fn thread_ledger_paths_match_a_path_only_row_under_a_historical_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut threads = Threads::open(&dir.path().join("threads.json")).unwrap();
        threads
            .store
            .threads
            .push(dual_read_thread("thread-dddddddd", "/repo/old", None));

        // Catalog-mode ledger arm: the relocated project queries by its
        // current key, and the ledger's historical key still reaches the row.
        let hit = threads
            .thread_list(&ThreadListParams {
                project: Some("/repo/relocated".into()),
                project_ledger_paths: vec!["/repo/old".into()],
                ..Default::default()
            })
            .unwrap();
        assert!(
            hit.contains("thread-dddddddd"),
            "ledger arm must match: {hit}"
        );

        // Bridge mode carries no ledger paths, so the historical row stays
        // invisible to the relocated key.
        let miss = threads
            .thread_list(&ThreadListParams {
                project: Some("/repo/relocated".into()),
                ..Default::default()
            })
            .unwrap();
        assert!(
            !miss.contains("thread-dddddddd"),
            "no ledger path must not match: {miss}"
        );
    }
}

// ── Project-catalog row stamping (P6-B) ────────────────────────────────────

#[cfg(test)]
mod owner_row_stamping {
    use super::*;
    use bbox_corpus_core::project_catalog_snapshot::{
        OWNER_ROW_ABSENT, OWNER_ROW_PROJECT_ID_CONFLICT, OWNER_SOURCE_MISSING,
        OwnerRowStampOutcomeV1, OwnerSnapshotLimitsV1,
    };

    /// Two threads plus a field this binary does not model, so every test also
    /// witnesses preservation of data the compiled schema cannot see.
    fn write_fixture(store_path: &Path) {
        std::fs::write(
            store_path,
            br#"{
  "version": 1,
  "threads": [
    {
      "id": "thread-0001",
      "project": "/legacy/path/one",
      "future_field": {"kept": true}
    },
    {
      "id": "thread-0002",
      "project": "/legacy/path/two"
    }
  ]
}
"#,
        )
        .unwrap();
    }

    fn stamp(
        store_path: &Path,
        row: &str,
        project_id: &str,
    ) -> std::result::Result<
        OwnerRowStampOutcomeV1,
        bbox_corpus_core::project_catalog_snapshot::OwnerRowStampError,
    > {
        stamp_project_catalog_owner_row(
            store_path,
            row,
            &bbox_corpus_core::project_catalog_snapshot::singleton_selector_members(row),
            project_id,
            OwnerSnapshotLimitsV1::default(),
        )
    }

    fn read_row(store_path: &Path, row: &str) -> serde_json::Value {
        let document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(store_path).unwrap()).unwrap();
        document["threads"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["id"] == row)
            .cloned()
            .unwrap()
    }

    fn fixture_store(dir: &tempfile::TempDir) -> std::path::PathBuf {
        let store_path = dir.path().canonicalize().unwrap().join("threads.json");
        write_fixture(&store_path);
        store_path
    }

    #[test]
    fn a_fresh_row_takes_the_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = fixture_store(&dir);

        assert_eq!(
            stamp(&store_path, "thread-0001", "a1b2c3d4").unwrap(),
            OwnerRowStampOutcomeV1::Stamped
        );

        let row = read_row(&store_path, "thread-0001");
        assert_eq!(row["project_id"], "a1b2c3d4");
        // The legacy selector is RETAINED: dual-read still resolves through it
        // until the later path-fallback removal gate.
        assert_eq!(row["project"], "/legacy/path/one");
        // A field this binary does not model survives the write-back.
        assert_eq!(row["future_field"]["kept"], true);
        // Stamping one row must not touch its neighbours.
        assert!(
            read_row(&store_path, "thread-0002")
                .get("project_id")
                .is_none()
        );
    }

    /// Re-applying a torn backfill must complete, not double-write.
    #[test]
    fn restamping_the_same_id_is_an_idempotent_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = fixture_store(&dir);

        stamp(&store_path, "thread-0001", "a1b2c3d4").unwrap();
        let after_first = std::fs::read(&store_path).unwrap();

        assert_eq!(
            stamp(&store_path, "thread-0001", "a1b2c3d4").unwrap(),
            OwnerRowStampOutcomeV1::AlreadyStamped
        );
        // Byte-identical: the second stamp elided the write entirely.
        assert_eq!(std::fs::read(&store_path).unwrap(), after_first);
    }

    /// Never a silent overwrite: a row bound to another project refuses.
    #[test]
    fn a_conflicting_id_refuses_and_leaves_the_row_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = fixture_store(&dir);

        stamp(&store_path, "thread-0001", "a1b2c3d4").unwrap();
        let before = std::fs::read(&store_path).unwrap();

        let error = stamp(&store_path, "thread-0001", "99998888").unwrap_err();
        assert_eq!(error.code, OWNER_ROW_PROJECT_ID_CONFLICT);
        assert_eq!(
            read_row(&store_path, "thread-0001")["project_id"],
            "a1b2c3d4"
        );
        assert_eq!(std::fs::read(&store_path).unwrap(), before);
    }

    /// Absence is a refusal, never a success: a resolution naming a row this
    /// store does not have must not report progress.
    #[test]
    fn an_absent_row_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = fixture_store(&dir);

        let error = stamp(&store_path, "row-does-not-exist", "a1b2c3d4").unwrap_err();
        assert_eq!(error.code, OWNER_ROW_ABSENT);
    }

    /// An absent SOURCE is likewise a refusal, and must not create a store.
    #[test]
    fn an_absent_source_refuses_without_creating_it() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().canonicalize().unwrap().join("threads.json");

        let error = stamp(&store_path, "thread-0001", "a1b2c3d4").unwrap_err();
        assert_eq!(error.code, OWNER_SOURCE_MISSING);
        assert!(!store_path.exists());
    }
}
