use std::fs;
use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result};
use rmcp::schemars;
use serde::{Deserialize, Serialize};

use bbox_corpus_core::project_selector::project_scope_matches;
use bbox_stores::store_persister::StoreSnapshot;

const NOTE_ID_PREFIX: &str = "note-";
const NOTE_ID_FORMAT_HINT: &str = "note-<8hex>";

// ── embed-sink hook ────────────────────────────────────────────────
//
// Same inversion as `threads::register_thread_embed_hook`: the daemon
// registers `embed_queue::enqueue_note` at SharedState construction;
// unregistered means embed scheduling is a no-op.
static NOTE_EMBED_HOOK: std::sync::OnceLock<fn(&Note)> = std::sync::OnceLock::new();

/// Register the embed sink for note mutations. Idempotent; first
/// registration wins.
pub fn register_note_embed_hook(hook: fn(&Note)) {
    let _ = NOTE_EMBED_HOOK.set(hook);
}

fn enqueue_note_embed(note: &Note) {
    if let Some(hook) = NOTE_EMBED_HOOK.get() {
        hook(note);
    }
}

// ── MCP parameter structs ─────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NoteParams {
    /// One of: dispute, assumption, surprise, followup, blocked, learned, done
    pub kind: String,
    /// Short note body (1–3 sentences). Substrate gap reports are NOT
    /// side-channel notes — file them with `bbox_gap` (see `sm-gap-notes` via
    /// `bbox_knowledge`), not here.
    pub body: String,
    /// Dispatch task ID — copy from the `task:` value in the ambient
    /// [scope] prefix to link this note to the dispatch. Stable across
    /// all providers regardless of when the provider emits its session
    /// ID.
    #[serde(default)]
    pub task_id: Option<String>,
    /// Session that produced this note (provider-native session ID)
    #[serde(default)]
    pub session_id: Option<String>,
    /// Project path
    #[serde(default)]
    pub project: Option<String>,
    /// Linked work-item thread ID. Canonical form is `thread-<8 hex>` (e.g.
    /// `thread-7f01324e`) — the exact string returned by `bbox_thread`
    /// / listed by `bbox_thread_list`. Copy verbatim from the `thread:` line
    /// of the ambient `[scope]` prefix when available.
    #[serde(default)]
    #[schemars(regex(pattern = r"^(thread-)?[0-9a-f]{8}$"))]
    pub thread_id: Option<String>,
    /// Provider (claude, codex, gemini, ...)
    #[serde(default)]
    pub provider: Option<String>,
    /// Named bro instance
    #[serde(default)]
    pub bro: Option<String>,
    /// Internal, not part of the MCP schema: the resolving authority's
    /// project id. Set by the daemon adapter from the resolver, never
    /// accepted from the wire, so identity cannot be caller-asserted.
    #[serde(skip)]
    pub project_id: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NoteListParams {
    /// Exact note ID. Canonical form is `note-<8 hex>` (e.g.
    /// `note-a1b2c3d4`). The bare 8-hex suffix is accepted as a fallback.
    #[serde(default)]
    #[schemars(regex(pattern = r"^(note-)?[0-9a-f]{8}$"))]
    pub id: Option<String>,
    /// Filter by kind
    #[serde(default)]
    pub kind: Option<String>,
    /// Filter by project substring
    #[serde(default)]
    pub project: Option<String>,
    /// Filter by dispatch task ID (the `task:` value in ambient scope)
    #[serde(default)]
    pub task_id: Option<String>,
    /// Filter by session ID (provider-native)
    #[serde(default)]
    pub session_id: Option<String>,
    /// Filter by thread ID
    #[serde(default)]
    pub thread_id: Option<String>,
    /// Filter by bro name
    #[serde(default)]
    pub bro: Option<String>,
    /// Filter by resolution: unresolved, acknowledged, addressed
    #[serde(default)]
    pub resolution: Option<String>,
    /// Free-text substring match on body
    #[serde(default)]
    pub query: Option<String>,
    /// ISO 8601: only notes created at or after this timestamp
    #[serde(default)]
    pub since: Option<String>,
    /// Max rows (default: 50)
    #[serde(default)]
    pub limit: Option<u64>,
    /// Include notes whose resolution is "addressed" (default: false for list
    /// views, true for exact `id` lookups)
    #[serde(default)]
    pub include_addressed: Option<bool>,
    /// Render full note bodies. Default false → bodies are previewed at 200
    /// chars with an ellipsis to keep the response under the MCP cap. Set
    /// true when you need the complete body (e.g. structured `done` summaries
    /// or multi-line `dispute` rationales).
    #[serde(default)]
    pub full: Option<bool>,
    /// Project id from the resolver. When both this and a row carry an
    /// id, the id decides and the path predicate is not consulted.
    #[serde(default)]
    pub project_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NoteResolveParams {
    /// Note ID for the single-note path. Canonical form is `note-<8 hex>` (e.g.
    /// `note-a1b2c3d4`) — the exact string returned by `bbox_note` and listed
    /// by `bbox_notes` / `bbox_inbox`. The bare 8-hex suffix (`a1b2c3d4`) is
    /// accepted as a fallback for ergonomics, but prefer the canonical form.
    #[serde(default)]
    #[schemars(regex(pattern = r"^(note-)?[0-9a-f]{8}$"))]
    pub id: Option<String>,
    /// Batch note IDs to resolve in one mutation and one durable persist. Use
    /// this when closing multiple notes from an inbox/round cleanup.
    #[serde(default)]
    #[schemars(length(min = 1))]
    pub ids: Vec<String>,
    /// One of: unresolved, acknowledged, addressed
    pub resolution: String,
    /// Optional resolution note
    #[serde(default)]
    pub note: Option<String>,
    /// Per-note resolution details keyed by note ID. Map keys also act as
    /// batch IDs, so `notes={"note-a1b2c3d4":"fixed"}` is enough to resolve
    /// that note with a distinct detail.
    #[serde(default)]
    pub notes: std::collections::BTreeMap<String, String>,
}

impl NoteResolveParams {
    fn requested_ids(&self) -> Result<Vec<String>> {
        let mut ids = Vec::new();
        if let Some(id) = self.id.as_deref().filter(|id| !id.trim().is_empty()) {
            ids.push(id.to_string());
        }
        ids.extend(self.ids.iter().filter(|id| !id.trim().is_empty()).cloned());
        ids.extend(
            self.notes
                .keys()
                .filter(|id| !id.trim().is_empty())
                .cloned(),
        );
        if ids.is_empty() {
            anyhow::bail!("Either 'id', 'ids', or 'notes' is required");
        }
        Ok(ids)
    }

    fn resolution_note_for(&self, requested_id: &str, canonical_id: &str) -> Option<&String> {
        let bare_id = canonical_id
            .strip_prefix(NOTE_ID_PREFIX)
            .unwrap_or(canonical_id);
        self.notes
            .get(requested_id)
            .or_else(|| self.notes.get(canonical_id))
            .or_else(|| self.notes.get(bare_id))
            .or(self.note.as_ref())
    }
}

struct ResolvedNoteTarget {
    requested_id: String,
    index: usize,
}

// ── Schema ─────────────────────────────────────────────────────────

#[derive(
    Debug, Clone, Copy, PartialEq, Serialize, Deserialize, strum::EnumString, strum::AsRefStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum NoteKind {
    /// Executor disagrees with brief or orchestrator premise
    Dispute,
    /// Ambiguity-resolving judgment call made while working
    Assumption,
    /// Expected X, found Y — premise drift signal
    Surprise,
    /// Out-of-scope work spotted, deferred
    Followup,
    /// Subtask blocked — reason included
    Blocked,
    /// Project-local convention discovered in situ
    Learned,
    /// Completion signal with one-line acceptance summary
    Done,
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::AsRefStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum NoteResolution {
    #[default]
    Unresolved,
    Acknowledged,
    Addressed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Note {
    pub id: String,
    pub kind: NoteKind,
    pub body: String,
    /// Dispatch task ID — the stable correlation key across providers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Resolving authority's project id, stamped on write. Absent on rows
    /// written before the catalog cut: those stay on the path lane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bro: Option<String>,
    #[serde(default)]
    pub resolution: NoteResolution,
    pub created_at: String,
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution_note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoteStore {
    pub version: u32,
    pub notes: Vec<Note>,
}

impl NoteStore {
    pub fn new() -> Self {
        Self {
            version: 1,
            notes: Vec::new(),
        }
    }
}

/// Capture note rows that retain the legacy literal project selector.
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

    capture_json_owner(store_path, "note", "note:central-json", limits, |bytes| {
        let store: NoteStore = serde_json::from_slice(bytes).map_err(|_| ())?;
        Ok(store
            .notes
            .into_iter()
            .filter_map(|note| {
                let selector = note.project?.trim().to_string();
                (!selector.is_empty()).then(|| {
                    OwnerSnapshotRowV1::legacy_selector(
                        note.id,
                        LegacyProjectSelectorKindV1::Project,
                        selector,
                    )
                })
            })
            .collect())
    })
}

impl StoreSnapshot for Notes {
    type Snapshot = NoteStore;

    fn snapshot(&self) -> Result<Self::Snapshot> {
        Ok(self.store.clone())
    }
}

// ── Store operations ───────────────────────────────────────────────

pub struct Notes {
    store: NoteStore,
}

impl Notes {
    pub fn open(store_path: &Path) -> Result<Self> {
        let store = if store_path.exists() {
            let raw = fs::read_to_string(store_path)
                .with_context(|| format!("reading {}", store_path.display()))?;
            serde_json::from_str(&raw)
                .with_context(|| format!("parsing {}", store_path.display()))?
        } else {
            NoteStore::new()
        };
        Ok(Self { store })
    }

    fn now_iso() -> String {
        bbox_util::util::now_iso()
    }

    fn gen_id() -> String {
        use std::time::SystemTime;
        let d = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let hash = d.as_nanos() ^ 0x9e3779b97f4a7c15;
        format!("note-{:08x}", hash as u32)
    }

    /// Immutable slice of all stored notes — used by cross-store
    /// aggregators (inbox) that can't go through the MCP layer.
    pub fn all(&self) -> &[Note] {
        &self.store.notes
    }

    pub fn rename_project_refs(&mut self, old_project: &str, new_project: &str) -> Result<usize> {
        let mut updated = 0usize;
        let now = Self::now_iso();
        for note in &mut self.store.notes {
            if note.project.as_deref() == Some(old_project) {
                note.project = Some(new_project.to_string());
                note.updated_at = now.clone();
                updated += 1;
            }
        }
        if updated > 0 {
            for note in &self.store.notes {
                if note.project.as_deref() == Some(new_project) {
                    enqueue_note_embed(note);
                }
            }
        }
        Ok(updated)
    }

    // ── bbox_note (create) ─────────────────────────────────────────

    pub fn create(&mut self, p: &NoteParams) -> Result<String> {
        self.create_locked(p)
    }

    fn create_locked(&mut self, p: &NoteParams) -> Result<String> {
        let kind = NoteKind::from_str(&p.kind).map_err(|_| {
            anyhow::anyhow!(
                "Unknown kind: {}. Use: dispute, assumption, surprise, followup, blocked, learned, done",
                p.kind
            )
        })?;
        if p.body.trim().is_empty() {
            anyhow::bail!("'body' is required and cannot be empty");
        }

        let now = Self::now_iso();
        let id = Self::gen_id();

        let note = Note {
            id: id.clone(),
            kind,
            body: p.body.clone(),
            task_id: p.task_id.clone(),
            session_id: p.session_id.clone(),
            project: p.project.clone(),
            project_id: p.project_id.clone(),
            thread_id: p.thread_id.clone(),
            provider: p.provider.clone(),
            bro: p.bro.clone(),
            resolution: NoteResolution::Unresolved,
            created_at: now.clone(),
            updated_at: now,
            resolved_at: None,
            resolution_note: None,
        };

        self.store.notes.push(note.clone());
        enqueue_note_embed(&note);

        Ok(format!("Note {id} recorded (kind={})", kind.as_ref()))
    }

    // ── bbox_note_resolve ──────────────────────────────────────────

    pub fn resolve(&mut self, p: &NoteResolveParams) -> Result<String> {
        self.resolve_locked(p)
    }

    fn resolve_locked(&mut self, p: &NoteResolveParams) -> Result<String> {
        let resolution = NoteResolution::from_str(&p.resolution).map_err(|_| {
            anyhow::anyhow!(
                "Unknown resolution: {}. Use: unresolved, acknowledged, addressed",
                p.resolution
            )
        })?;

        let requested_ids = p.requested_ids()?;
        let note_targets = self.resolve_targets(&requested_ids)?;
        let now = Self::now_iso();
        for target in &note_targets {
            let canonical_id = self.store.notes[target.index].id.clone();
            let resolution_note = p
                .resolution_note_for(&target.requested_id, &canonical_id)
                .cloned();
            let note = &mut self.store.notes[target.index];
            note.resolution = resolution;
            note.updated_at = now.clone();
            note.resolved_at = if matches!(resolution, NoteResolution::Unresolved) {
                None
            } else {
                Some(now.clone())
            };
            if let Some(txt) = resolution_note {
                note.resolution_note = Some(txt);
            }
        }

        if note_targets.len() == 1 {
            Ok(format!(
                "Note {} → {}",
                self.store.notes[note_targets[0].index].id,
                resolution.as_ref()
            ))
        } else {
            Ok(format!(
                "{} notes → {}",
                note_targets.len(),
                resolution.as_ref()
            ))
        }
    }

    fn resolve_targets(&self, requested_ids: &[String]) -> Result<Vec<ResolvedNoteTarget>> {
        let mut note_targets = Vec::with_capacity(requested_ids.len());
        for requested_id in requested_ids {
            let index = self.find_note_index(requested_id).with_context(|| {
                format!(
                    "Note not found: {} (expected `{}`, e.g. `note-a1b2c3d4`)",
                    requested_id, NOTE_ID_FORMAT_HINT
                )
            })?;
            if !note_targets
                .iter()
                .any(|target: &ResolvedNoteTarget| target.index == index)
            {
                note_targets.push(ResolvedNoteTarget {
                    requested_id: requested_id.clone(),
                    index,
                });
            }
        }
        Ok(note_targets)
    }

    fn find_note_index(&self, requested_id: &str) -> Option<usize> {
        // Canonical IDs are `note-<8hex>`. Accept the bare suffix as a
        // fallback — agents sometimes strip the prefix treating it as display
        // decoration; fail loudly rather than silently on true misses.
        let needle = requested_id
            .strip_prefix(NOTE_ID_PREFIX)
            .unwrap_or(requested_id);
        self.store
            .notes
            .iter()
            .position(|n| n.id == requested_id || n.id.strip_prefix(NOTE_ID_PREFIX) == Some(needle))
    }

    // ── bbox_notes (list) ──────────────────────────────────────────

    pub fn list(&self, p: &NoteListParams) -> Result<String> {
        let kind_filter = p
            .kind
            .as_deref()
            .map(NoteKind::from_str)
            .transpose()
            .map_err(|_| anyhow::anyhow!("Unknown kind filter: {:?}", p.kind))?;

        let resolution_filter = p
            .resolution
            .as_deref()
            .map(NoteResolution::from_str)
            .transpose()
            .map_err(|_| anyhow::anyhow!("Unknown resolution filter: {:?}", p.resolution))?;

        let include_addressed = p.include_addressed.unwrap_or(p.id.is_some());
        let full = p.full.unwrap_or(false);
        let limit = p.limit.unwrap_or(50).max(1) as usize;

        let id_filter = p.id.as_deref().map(str::to_ascii_lowercase);
        let query_lower = p.query.as_deref().map(|s| s.to_lowercase());
        let project_lower = p.project.as_deref().map(|s| s.to_lowercase());
        let project_id_filter = p.project_id.as_deref();

        let mut results: Vec<&Note> = self
            .store
            .notes
            .iter()
            .filter(|n| {
                if let Some(id) = id_filter.as_deref() {
                    let needle = id.strip_prefix(NOTE_ID_PREFIX).unwrap_or(id);
                    if n.id != id && n.id.strip_prefix(NOTE_ID_PREFIX) != Some(needle) {
                        return false;
                    }
                }
                if let Some(k) = kind_filter {
                    if n.kind != k {
                        return false;
                    }
                }
                if let Some(r) = resolution_filter {
                    if n.resolution != r {
                        return false;
                    }
                } else if !include_addressed && n.resolution == NoteResolution::Addressed {
                    return false;
                }
                if let Some(tid) = p.task_id.as_deref() {
                    if n.task_id.as_deref() != Some(tid) {
                        return false;
                    }
                }
                if let Some(sid) = p.session_id.as_deref() {
                    if n.session_id.as_deref() != Some(sid) {
                        return false;
                    }
                }
                if let Some(tid) = p.thread_id.as_deref() {
                    if n.thread_id.as_deref() != Some(tid) {
                        return false;
                    }
                }
                if let Some(bro) = p.bro.as_deref() {
                    if n.bro.as_deref() != Some(bro) {
                        return false;
                    }
                }
                // Dual-read (plan §8.2): ids on both sides decide, whatever the
                // paths say; either side missing an id keeps the path predicate.
                if let Some(pl) = &project_lower
                    && !project_scope_matches(n.project_id.as_deref(), project_id_filter, || {
                        n.project
                            .as_deref()
                            .unwrap_or("")
                            .to_lowercase()
                            .contains(pl)
                    })
                {
                    return false;
                }
                if let Some(q) = &query_lower {
                    if !n.body.to_lowercase().contains(q) {
                        return false;
                    }
                }
                if let Some(since) = p.since.as_deref() {
                    if n.created_at.as_str() < since {
                        return false;
                    }
                }
                true
            })
            .collect();

        if results.is_empty() {
            return Ok("No notes found.".to_string());
        }

        // Newest first
        results.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        results.truncate(limit);

        let mut out = String::new();
        out.push_str(&format!("{} note(s)\n\n", results.len()));
        for n in &results {
            let body_preview = if full || n.body.len() <= 200 {
                n.body.clone()
            } else {
                let mut end = 200;
                while !n.body.is_char_boundary(end) {
                    end -= 1;
                }
                format!("{}…", &n.body[..end])
            };
            let ctx_bits = [
                n.bro.as_deref().map(|b| format!("bro={b}")),
                n.provider.as_deref().map(|p| format!("provider={p}")),
                n.task_id
                    .as_deref()
                    .map(|t| format!("task={}", &t[..t.len().min(8)])),
                n.session_id
                    .as_deref()
                    .map(|s| format!("session={}", &s[..s.len().min(8)])),
                n.thread_id.as_deref().map(|t| format!("thread={t}")),
                n.project
                    .as_deref()
                    .and_then(|p| p.rsplit('/').next().map(|leaf| format!("project={leaf}"))),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");

            out.push_str(&format!(
                "{id}  [{kind}/{res}]  {ts}  {ctx}\n  {body}\n",
                id = n.id,
                kind = n.kind.as_ref(),
                res = n.resolution.as_ref(),
                ts = n.created_at,
                ctx = ctx_bits,
                body = body_preview,
            ));
            if let Some(rn) = &n.resolution_note {
                out.push_str(&format!("  ↳ {rn}\n"));
            }
            out.push('\n');
        }

        Ok(out)
    }
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use bbox_stores::store_persister::StorePersister;
    use fs2::FileExt;
    use parking_lot::RwLock;
    use std::sync::Arc;
    use tempfile::tempdir;

    fn mk_store() -> (tempfile::TempDir, Notes) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("notes.json");
        let notes = Notes::open(&path).unwrap();
        (dir, notes)
    }

    #[tokio::test]
    async fn create_and_resolve_round_trip_through_persister() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("notes.json");
        let notes = Arc::new(RwLock::new(Notes::open(&path).unwrap()));
        let persister = StorePersister::spawn("notes-test-roundtrip", notes.clone(), path.clone());

        notes
            .write()
            .create(&NoteParams {
                kind: "done".into(),
                body: "persisted through actor".into(),
                session_id: None,
                project: None,
                project_id: None,
                task_id: None,
                thread_id: None,
                provider: None,
                bro: None,
            })
            .unwrap();
        persister.request_durable().await.unwrap();

        let saved: NoteStore =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved.notes.len(), 1);
        assert_eq!(saved.notes[0].body, "persisted through actor");
        let id = saved.notes[0].id.clone();

        notes
            .write()
            .resolve(&NoteResolveParams {
                id: Some(id.clone()),
                ids: Vec::new(),
                resolution: "addressed".into(),
                note: Some("verified".into()),
                notes: Default::default(),
            })
            .unwrap();
        persister.request_durable().await.unwrap();

        let saved: NoteStore =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let note = saved.notes.iter().find(|note| note.id == id).unwrap();
        assert_eq!(note.resolution, NoteResolution::Addressed);
        assert_eq!(note.resolution_note.as_deref(), Some("verified"));
    }

    #[tokio::test]
    async fn reads_succeed_while_persistence_ack_is_pending() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("notes.json");
        let notes = Arc::new(RwLock::new(Notes::open(&path).unwrap()));
        let persister = StorePersister::spawn("notes-test-pending", notes.clone(), path.clone());

        notes
            .write()
            .create(&NoteParams {
                kind: "done".into(),
                body: "read before ack".into(),
                session_id: None,
                project: None,
                project_id: None,
                task_id: None,
                thread_id: None,
                provider: None,
                bro: None,
            })
            .unwrap();

        let lock_path = path.with_extension("json.lock");
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        lock_file.lock_exclusive().unwrap();

        let pending = {
            let persister = persister.clone();
            tokio::spawn(async move { persister.request_durable().await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        assert!(!pending.is_finished());

        let out = notes
            .read()
            .list(&NoteListParams {
                id: None,
                kind: None,
                project: None,
                project_id: None,
                session_id: None,
                task_id: None,
                thread_id: None,
                bro: None,
                resolution: None,
                query: Some("read before ack".into()),
                since: None,
                limit: None,
                include_addressed: None,
                full: None,
            })
            .unwrap();
        assert!(out.contains("read before ack"));

        lock_file.unlock().unwrap();
        pending.await.unwrap().unwrap();
    }

    #[test]
    fn create_and_list() {
        let (_tmp, mut notes) = mk_store();
        let r = notes
            .create(&NoteParams {
                kind: "dispute".into(),
                body: "brief conflates schemas".into(),
                session_id: Some("sess-abc".into()),
                project: Some("/repo/x".into()),
                project_id: None,
                task_id: None,
                thread_id: None,
                provider: Some("claude".into()),
                bro: Some("executor".into()),
            })
            .unwrap();
        assert!(r.contains("dispute"));

        let out = notes
            .list(&NoteListParams {
                id: None,
                kind: Some("dispute".into()),
                project: None,
                project_id: None,
                session_id: None,
                task_id: None,
                thread_id: None,
                bro: None,
                resolution: None,
                query: None,
                since: None,
                limit: None,
                include_addressed: None,
                full: None,
            })
            .unwrap();
        assert!(out.contains("brief conflates schemas"));
        assert!(out.contains("bro=executor"));
    }

    #[test]
    fn list_filters_by_exact_id_with_bare_suffix_fallback() {
        let (_tmp, mut notes) = mk_store();
        notes
            .create(&NoteParams {
                kind: "done".into(),
                body: "target body".into(),
                session_id: None,
                project: None,
                project_id: None,
                task_id: None,
                thread_id: None,
                provider: None,
                bro: None,
            })
            .unwrap();
        let target_id = notes.store.notes[0].id.clone();
        notes
            .create(&NoteParams {
                kind: "done".into(),
                body: "other body".into(),
                session_id: None,
                project: None,
                project_id: None,
                task_id: None,
                thread_id: None,
                provider: None,
                bro: None,
            })
            .unwrap();
        let other_id = notes.store.notes[1].id.clone();

        notes
            .resolve(&NoteResolveParams {
                id: Some(target_id.clone()),
                ids: Vec::new(),
                resolution: "addressed".into(),
                note: None,
                notes: Default::default(),
            })
            .unwrap();

        let by_canonical_id = notes
            .list(&NoteListParams {
                id: Some(target_id.clone()),
                kind: None,
                project: None,
                project_id: None,
                session_id: None,
                task_id: None,
                thread_id: None,
                bro: None,
                resolution: None,
                query: None,
                since: None,
                limit: None,
                include_addressed: None,
                full: None,
            })
            .unwrap();
        assert!(by_canonical_id.contains("target body"));
        assert!(!by_canonical_id.contains("other body"));

        let bare_id = target_id.strip_prefix("note-").unwrap().to_string();
        let by_bare_id = notes
            .list(&NoteListParams {
                id: Some(bare_id),
                kind: None,
                project: None,
                project_id: None,
                session_id: None,
                task_id: None,
                thread_id: None,
                bro: None,
                resolution: None,
                query: None,
                since: None,
                limit: None,
                include_addressed: None,
                full: None,
            })
            .unwrap();
        assert!(by_bare_id.contains("target body"));

        let id_as_query = notes
            .list(&NoteListParams {
                id: None,
                kind: None,
                project: None,
                project_id: None,
                session_id: None,
                task_id: None,
                thread_id: None,
                bro: None,
                resolution: None,
                query: Some(other_id),
                since: None,
                limit: None,
                include_addressed: None,
                full: None,
            })
            .unwrap();
        assert_eq!(id_as_query, "No notes found.");
    }

    #[test]
    fn unknown_kind_rejected() {
        let (_tmp, mut notes) = mk_store();
        let e = notes
            .create(&NoteParams {
                kind: "ponder".into(),
                body: "x".into(),
                session_id: None,
                project: None,
                project_id: None,
                task_id: None,
                thread_id: None,
                provider: None,
                bro: None,
            })
            .unwrap_err();
        assert!(e.to_string().contains("Unknown kind"));
    }

    #[test]
    fn empty_body_rejected() {
        let (_tmp, mut notes) = mk_store();
        let e = notes
            .create(&NoteParams {
                kind: "done".into(),
                body: "  ".into(),
                session_id: None,
                project: None,
                project_id: None,
                task_id: None,
                thread_id: None,
                provider: None,
                bro: None,
            })
            .unwrap_err();
        assert!(e.to_string().contains("body"));
    }

    #[test]
    fn resolve_transitions() {
        let (_tmp, mut notes) = mk_store();
        notes
            .create(&NoteParams {
                kind: "surprise".into(),
                body: "expected N, found M".into(),
                session_id: None,
                project: None,
                project_id: None,
                task_id: None,
                thread_id: None,
                provider: None,
                bro: None,
            })
            .unwrap();
        let id = notes.store.notes[0].id.clone();

        notes
            .resolve(&NoteResolveParams {
                id: Some(id.clone()),
                ids: Vec::new(),
                resolution: "acknowledged".into(),
                note: Some("will investigate next round".into()),
                notes: Default::default(),
            })
            .unwrap();

        let n = &notes.store.notes[0];
        assert_eq!(n.resolution, NoteResolution::Acknowledged);
        assert!(n.resolved_at.is_some());
        assert_eq!(
            n.resolution_note.as_deref(),
            Some("will investigate next round")
        );

        // Default list excludes addressed but includes acknowledged
        let out = notes
            .list(&NoteListParams {
                id: None,
                kind: None,
                project: None,
                project_id: None,
                session_id: None,
                task_id: None,
                thread_id: None,
                bro: None,
                resolution: None,
                query: None,
                since: None,
                limit: None,
                include_addressed: None,
                full: None,
            })
            .unwrap();
        assert!(out.contains(&id));

        notes
            .resolve(&NoteResolveParams {
                id: Some(id.clone()),
                ids: Vec::new(),
                resolution: "addressed".into(),
                note: None,
                notes: Default::default(),
            })
            .unwrap();

        let out = notes
            .list(&NoteListParams {
                id: None,
                kind: None,
                project: None,
                project_id: None,
                session_id: None,
                task_id: None,
                thread_id: None,
                bro: None,
                resolution: None,
                query: None,
                since: None,
                limit: None,
                include_addressed: None,
                full: None,
            })
            .unwrap();
        assert!(
            !out.contains(&id),
            "addressed note should be excluded by default"
        );

        let out_all = notes
            .list(&NoteListParams {
                id: None,
                kind: None,
                project: None,
                project_id: None,
                session_id: None,
                task_id: None,
                thread_id: None,
                bro: None,
                resolution: None,
                query: None,
                since: None,
                limit: None,
                include_addressed: Some(true),
                full: None,
            })
            .unwrap();
        assert!(out_all.contains(&id));
    }

    #[test]
    fn resolve_accepts_bare_hex_fallback() {
        let (_tmp, mut notes) = mk_store();
        notes
            .create(&NoteParams {
                kind: "done".into(),
                body: "task complete".into(),
                session_id: None,
                project: None,
                project_id: None,
                task_id: None,
                thread_id: None,
                provider: None,
                bro: None,
            })
            .unwrap();
        let full_id = notes.store.notes[0].id.clone();
        let bare = full_id.strip_prefix("note-").unwrap().to_string();
        assert_eq!(bare.len(), 8, "gen_id produces 8 hex chars");

        notes
            .resolve(&NoteResolveParams {
                id: Some(bare),
                ids: Vec::new(),
                resolution: "addressed".into(),
                note: None,
                notes: Default::default(),
            })
            .unwrap();

        assert_eq!(notes.store.notes[0].resolution, NoteResolution::Addressed);
    }

    #[test]
    fn resolve_batch_updates_multiple_notes_once() {
        let (_tmp, mut notes) = mk_store();
        for body in ["first", "second", "third"] {
            notes
                .create(&NoteParams {
                    kind: "done".into(),
                    body: body.into(),
                    session_id: None,
                    project: None,
                    project_id: None,
                    task_id: None,
                    thread_id: None,
                    provider: None,
                    bro: None,
                })
                .unwrap();
        }
        let first_id = notes.store.notes[0].id.clone();
        let second_bare = notes.store.notes[1]
            .id
            .strip_prefix(NOTE_ID_PREFIX)
            .unwrap()
            .to_string();

        let out = notes
            .resolve(&NoteResolveParams {
                id: Some(first_id),
                ids: vec![second_bare],
                resolution: "addressed".into(),
                note: Some("batch cleanup".into()),
                notes: Default::default(),
            })
            .unwrap();

        assert_eq!(out, "2 notes → addressed");
        assert_eq!(notes.store.notes[0].resolution, NoteResolution::Addressed);
        assert_eq!(notes.store.notes[1].resolution, NoteResolution::Addressed);
        assert_eq!(notes.store.notes[2].resolution, NoteResolution::Unresolved);
        assert_eq!(
            notes.store.notes[0].resolution_note.as_deref(),
            Some("batch cleanup")
        );
        assert_eq!(
            notes.store.notes[1].resolution_note.as_deref(),
            Some("batch cleanup")
        );
    }

    #[test]
    fn resolve_batch_validates_all_ids_before_mutating() {
        let (_tmp, mut notes) = mk_store();
        notes
            .create(&NoteParams {
                kind: "done".into(),
                body: "keep unresolved on error".into(),
                session_id: None,
                project: None,
                project_id: None,
                task_id: None,
                thread_id: None,
                provider: None,
                bro: None,
            })
            .unwrap();
        let valid_id = notes.store.notes[0].id.clone();

        let err = notes
            .resolve(&NoteResolveParams {
                id: None,
                ids: vec![valid_id, "note-deadbeef".into()],
                resolution: "addressed".into(),
                note: None,
                notes: Default::default(),
            })
            .unwrap_err();

        assert!(
            err.to_string().contains("note-deadbeef"),
            "error should name missing batch id: {err:#}"
        );
        assert_eq!(notes.store.notes[0].resolution, NoteResolution::Unresolved);
    }

    #[test]
    fn resolve_requires_id_or_ids() {
        let (_tmp, mut notes) = mk_store();
        let err = notes
            .resolve(&NoteResolveParams {
                id: None,
                ids: Vec::new(),
                resolution: "addressed".into(),
                note: None,
                notes: Default::default(),
            })
            .unwrap_err();
        assert!(err.to_string().contains("Either 'id', 'ids', or 'notes'"));
    }

    #[test]
    fn resolve_batch_accepts_per_id_resolution_notes() {
        let (_tmp, mut notes) = mk_store();
        for body in ["first", "second"] {
            notes
                .create(&NoteParams {
                    kind: "done".into(),
                    body: body.into(),
                    session_id: None,
                    project: None,
                    project_id: None,
                    task_id: None,
                    thread_id: None,
                    provider: None,
                    bro: None,
                })
                .unwrap();
        }
        let first_id = notes.store.notes[0].id.clone();
        let second_id = notes.store.notes[1].id.clone();
        let mut resolution_notes = std::collections::BTreeMap::new();
        resolution_notes.insert(first_id.clone(), "fixed first".into());
        resolution_notes.insert(second_id.clone(), "fixed second".into());

        let out = notes
            .resolve(&NoteResolveParams {
                id: None,
                ids: Vec::new(),
                resolution: "addressed".into(),
                note: None,
                notes: resolution_notes,
            })
            .unwrap();

        assert_eq!(out, "2 notes → addressed");
        assert_eq!(
            notes.store.notes[0].resolution_note.as_deref(),
            Some("fixed first")
        );
        assert_eq!(
            notes.store.notes[1].resolution_note.as_deref(),
            Some("fixed second")
        );
    }

    #[test]
    fn resolve_batch_map_notes_override_shared_note_by_id() {
        let (_tmp, mut notes) = mk_store();
        for body in ["first", "second"] {
            notes
                .create(&NoteParams {
                    kind: "done".into(),
                    body: body.into(),
                    session_id: None,
                    project: None,
                    project_id: None,
                    task_id: None,
                    thread_id: None,
                    provider: None,
                    bro: None,
                })
                .unwrap();
        }
        let first_id = notes.store.notes[0].id.clone();
        let second_id = notes.store.notes[1].id.clone();
        let second_bare = second_id.strip_prefix(NOTE_ID_PREFIX).unwrap().to_string();
        let mut resolution_notes = std::collections::BTreeMap::new();
        resolution_notes.insert(second_bare, "specific second".into());

        notes
            .resolve(&NoteResolveParams {
                id: Some(first_id),
                ids: vec![second_id],
                resolution: "addressed".into(),
                note: Some("shared fallback".into()),
                notes: resolution_notes,
            })
            .unwrap();

        assert_eq!(
            notes.store.notes[0].resolution_note.as_deref(),
            Some("shared fallback")
        );
        assert_eq!(
            notes.store.notes[1].resolution_note.as_deref(),
            Some("specific second")
        );
    }

    #[test]
    fn resolve_unknown_id_errors_with_format_hint() {
        let (_tmp, mut notes) = mk_store();
        let e = notes
            .resolve(&NoteResolveParams {
                id: Some("does-not-exist".into()),
                ids: Vec::new(),
                resolution: "addressed".into(),
                note: None,
                notes: Default::default(),
            })
            .unwrap_err();
        let msg = e.to_string();
        assert!(
            msg.contains("note-<8hex>"),
            "error should hint at format: {msg}"
        );
    }

    #[test]
    fn list_preview_handles_multibyte_boundary() {
        let (_tmp, mut notes) = mk_store();
        // Em-dash is 3 bytes; place one so byte 200 lands mid-char.
        let mut body = "x".repeat(198);
        body.push('—');
        body.push_str(&"y".repeat(50));
        notes
            .create(&NoteParams {
                kind: "done".into(),
                body,
                session_id: None,
                project: None,
                project_id: None,
                task_id: None,
                thread_id: None,
                provider: None,
                bro: None,
            })
            .unwrap();

        let out = notes
            .list(&NoteListParams {
                id: None,
                kind: None,
                project: None,
                project_id: None,
                session_id: None,
                task_id: None,
                thread_id: None,
                bro: None,
                resolution: None,
                query: None,
                since: None,
                limit: None,
                include_addressed: None,
                full: None,
            })
            .unwrap();
        assert!(out.contains('…'));
    }

    #[test]
    fn list_full_returns_untruncated_body() {
        let (_tmp, mut notes) = mk_store();
        let body = format!("{}END", "x".repeat(400));
        notes
            .create(&NoteParams {
                kind: "done".into(),
                body: body.clone(),
                session_id: None,
                project: None,
                project_id: None,
                task_id: None,
                thread_id: None,
                provider: None,
                bro: None,
            })
            .unwrap();

        let preview = notes
            .list(&NoteListParams {
                id: None,
                kind: None,
                project: None,
                project_id: None,
                session_id: None,
                task_id: None,
                thread_id: None,
                bro: None,
                resolution: None,
                query: None,
                since: None,
                limit: None,
                include_addressed: None,
                full: None,
            })
            .unwrap();
        assert!(preview.contains('…'));
        assert!(!preview.contains("END"));

        let full = notes
            .list(&NoteListParams {
                id: None,
                kind: None,
                project: None,
                project_id: None,
                session_id: None,
                task_id: None,
                thread_id: None,
                bro: None,
                resolution: None,
                query: None,
                since: None,
                limit: None,
                include_addressed: None,
                full: Some(true),
            })
            .unwrap();
        assert!(!full.contains('…'));
        assert!(full.contains("END"));
    }

    #[tokio::test]
    async fn roundtrip_persists() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("notes.json");
        let notes = Arc::new(RwLock::new(Notes::open(&path).unwrap()));
        let persister =
            StorePersister::spawn("notes-test-roundtrip-legacy", notes.clone(), path.clone());
        notes
            .write()
            .create(&NoteParams {
                kind: "learned".into(),
                body: "repo uses bb:managed markers".into(),
                session_id: None,
                project: Some("/repo/x".into()),
                project_id: None,
                task_id: None,
                thread_id: None,
                provider: None,
                bro: None,
            })
            .unwrap();
        persister.request_durable().await.unwrap();
        let notes = Notes::open(&path).unwrap();
        assert_eq!(notes.store.notes.len(), 1);
        assert_eq!(notes.store.notes[0].kind, NoteKind::Learned);
    }

    // ── Dual-read (plan §8.2) ────────────────────────────────────────────

    fn dual_read_note(id: &str, project: &str, project_id: Option<&str>) -> Note {
        Note {
            id: id.into(),
            kind: NoteKind::Learned,
            body: "dual read body".into(),
            task_id: None,
            session_id: None,
            project: Some(project.into()),
            project_id: project_id.map(str::to_string),
            thread_id: None,
            provider: None,
            bro: None,
            resolution: NoteResolution::Unresolved,
            resolution_note: None,
            created_at: "2026-07-24T00:00:00Z".into(),
            updated_at: "2026-07-24T00:00:00Z".into(),
            resolved_at: None,
        }
    }

    #[test]
    fn note_row_without_project_id_decodes_and_round_trips() {
        let legacy = serde_json::json!({
            "id": "note-legacy",
            "kind": "learned",
            "body": "b",
            "project": "/repo/old",
            "created_at": "2026-07-24T00:00:00Z",
            "updated_at": "2026-07-24T00:00:00Z"
        });
        let note: Note = serde_json::from_value(legacy).unwrap();
        assert_eq!(note.project_id, None);
        assert!(
            serde_json::to_value(&note)
                .unwrap()
                .get("project_id")
                .is_none()
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.json");
        let mut notes = Notes::open(&path).unwrap();
        notes.store.notes.push(note);
        std::fs::write(&path, serde_json::to_string(&notes.store).unwrap()).unwrap();
        let reopened = Notes::open(&path).unwrap();
        assert_eq!(reopened.store.notes.len(), 1);
        assert_eq!(reopened.store.notes[0].project_id, None);
    }

    #[test]
    fn note_project_id_match_wins_over_a_different_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut notes = Notes::open(&dir.path().join("notes.json")).unwrap();
        notes.store.notes.push(dual_read_note(
            "note-aaaaaaaa",
            "/repo/old",
            Some("abc12345"),
        ));

        let out = notes
            .list(&NoteListParams {
                project: Some("/repo/relocated".into()),
                project_id: Some("abc12345".into()),
                ..Default::default()
            })
            .unwrap();
        assert!(out.contains("note-aaaaaaaa"), "id arm must match: {out}");
    }

    #[test]
    fn note_without_ids_falls_back_to_the_exact_path_arm() {
        let dir = tempfile::tempdir().unwrap();
        let mut notes = Notes::open(&dir.path().join("notes.json")).unwrap();
        notes
            .store
            .notes
            .push(dual_read_note("note-bbbbbbbb", "/repo/old", None));

        let miss = notes
            .list(&NoteListParams {
                project: Some("/repo/relocated".into()),
                project_id: Some("abc12345".into()),
                ..Default::default()
            })
            .unwrap();
        assert!(
            !miss.contains("note-bbbbbbbb"),
            "path arm must decide: {miss}"
        );

        let hit = notes
            .list(&NoteListParams {
                project: Some("/repo/old".into()),
                ..Default::default()
            })
            .unwrap();
        assert!(hit.contains("note-bbbbbbbb"), "path arm must match: {hit}");
    }

    #[test]
    fn note_mismatched_ids_hide_the_row_at_the_same_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut notes = Notes::open(&dir.path().join("notes.json")).unwrap();
        notes.store.notes.push(dual_read_note(
            "note-cccccccc",
            "/repo/old",
            Some("abc12345"),
        ));

        // Same path key, different ids: the id decides against the row, so a
        // path reused after a retire-and-add cannot leak the old rows.
        let out = notes
            .list(&NoteListParams {
                project: Some("/repo/old".into()),
                project_id: Some("def67890".into()),
                ..Default::default()
            })
            .unwrap();
        assert!(
            !out.contains("note-cccccccc"),
            "id mismatch must hide: {out}"
        );
    }
}
