use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result};
use rmcp::schemars;
use serde::{Deserialize, Serialize};

// ── MCP parameter structs ─────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NoteParams {
    /// One of: dispute, assumption, surprise, followup, blocked, learned, done
    pub kind: String,
    /// Short note body (1–3 sentences)
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
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NoteListParams {
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
    /// Include notes whose resolution is "addressed" (default: false)
    #[serde(default)]
    pub include_addressed: Option<bool>,
    /// Render full note bodies. Default false → bodies are previewed at 200
    /// chars with an ellipsis to keep the response under the MCP cap. Set
    /// true when you need the complete body (e.g. structured `done` summaries
    /// or multi-line `dispute` rationales).
    #[serde(default)]
    pub full: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NoteResolveParams {
    /// Note ID. Canonical form is `note-<8 hex>` (e.g. `note-a1b2c3d4`) — the
    /// exact string returned by `bbox_note` and listed by `bbox_notes` /
    /// `bbox_inbox`. The bare 8-hex suffix (`a1b2c3d4`) is accepted as a
    /// fallback for ergonomics, but prefer the canonical form.
    #[schemars(regex(pattern = r"^(note-)?[0-9a-f]{8}$"))]
    pub id: String,
    /// One of: unresolved, acknowledged, addressed
    pub resolution: String,
    /// Optional resolution note
    #[serde(default)]
    pub note: Option<String>,
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

#[derive(Debug, Serialize, Deserialize)]
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

// ── Store operations ───────────────────────────────────────────────

pub struct Notes {
    store_path: PathBuf,
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
        let hash = d.as_nanos() ^ 0x9e3779b97f4a7c15;
        format!("note-{:08x}", hash as u32)
    }

    /// Immutable slice of all stored notes — used by cross-store
    /// aggregators (inbox) that can't go through the MCP layer.
    pub fn all(&self) -> &[Note] {
        &self.store.notes
    }

    pub fn rename_project_refs(&mut self, old_project: &str, new_project: &str) -> Result<usize> {
        let path = self.store_path.clone();
        crate::json_store::with_store_lock(&path, || {
            self.reload()?;
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
                self.save()?;
                for note in &self.store.notes {
                    if note.project.as_deref() == Some(new_project) {
                        crate::embed_queue::enqueue_note(note);
                    }
                }
            }
            Ok(updated)
        })
    }

    // ── bbox_note (create) ─────────────────────────────────────────

    pub fn create(&mut self, p: &NoteParams) -> Result<String> {
        let path = self.store_path.clone();
        crate::json_store::with_store_lock(&path, || {
            self.reload()?;
            self.create_locked(p)
        })
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
        self.save()?;
        crate::embed_queue::enqueue_note(&note);

        Ok(format!("Note {id} recorded (kind={})", kind.as_ref()))
    }

    // ── bbox_note_resolve ──────────────────────────────────────────

    pub fn resolve(&mut self, p: &NoteResolveParams) -> Result<String> {
        let path = self.store_path.clone();
        crate::json_store::with_store_lock(&path, || {
            self.reload()?;
            self.resolve_locked(p)
        })
    }

    fn resolve_locked(&mut self, p: &NoteResolveParams) -> Result<String> {
        let resolution = NoteResolution::from_str(&p.resolution).map_err(|_| {
            anyhow::anyhow!(
                "Unknown resolution: {}. Use: unresolved, acknowledged, addressed",
                p.resolution
            )
        })?;

        // Canonical IDs are `note-<8hex>`. Accept the bare suffix as a
        // fallback — agents sometimes strip the prefix treating it as display
        // decoration; fail loudly rather than silently on true misses.
        let needle = p.id.as_str();
        let note = self
            .store
            .notes
            .iter_mut()
            .find(|n| n.id == needle || n.id.strip_prefix("note-") == Some(needle))
            .with_context(|| {
                format!(
                    "Note not found: {} (expected `note-<8hex>`, e.g. `note-a1b2c3d4`)",
                    p.id
                )
            })?;

        let now = Self::now_iso();
        note.resolution = resolution;
        note.updated_at = now.clone();
        note.resolved_at = if matches!(resolution, NoteResolution::Unresolved) {
            None
        } else {
            Some(now)
        };
        if let Some(txt) = p.note.as_deref() {
            note.resolution_note = Some(txt.to_string());
        }

        self.save()?;
        Ok(format!("Note {} → {}", p.id, resolution.as_ref()))
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

        let include_addressed = p.include_addressed.unwrap_or(false);
        let full = p.full.unwrap_or(false);
        let limit = p.limit.unwrap_or(50).max(1) as usize;

        let query_lower = p.query.as_deref().map(|s| s.to_lowercase());
        let project_lower = p.project.as_deref().map(|s| s.to_lowercase());

        let mut results: Vec<&Note> = self
            .store
            .notes
            .iter()
            .filter(|n| {
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
                if let Some(pl) = &project_lower {
                    let nproj = n.project.as_deref().unwrap_or("").to_lowercase();
                    if !nproj.contains(pl) {
                        return false;
                    }
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
    use tempfile::tempdir;

    fn mk_store() -> (tempfile::TempDir, Notes) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("notes.json");
        let notes = Notes::open(&path).unwrap();
        (dir, notes)
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
                task_id: None,
                thread_id: None,
                provider: Some("claude".into()),
                bro: Some("executor".into()),
            })
            .unwrap();
        assert!(r.contains("dispute"));

        let out = notes
            .list(&NoteListParams {
                kind: Some("dispute".into()),
                project: None,
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
    fn unknown_kind_rejected() {
        let (_tmp, mut notes) = mk_store();
        let e = notes
            .create(&NoteParams {
                kind: "ponder".into(),
                body: "x".into(),
                session_id: None,
                project: None,
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
                task_id: None,
                thread_id: None,
                provider: None,
                bro: None,
            })
            .unwrap();
        let id = notes.store.notes[0].id.clone();

        notes
            .resolve(&NoteResolveParams {
                id: id.clone(),
                resolution: "acknowledged".into(),
                note: Some("will investigate next round".into()),
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
                kind: None,
                project: None,
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
                id: id.clone(),
                resolution: "addressed".into(),
                note: None,
            })
            .unwrap();

        let out = notes
            .list(&NoteListParams {
                kind: None,
                project: None,
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
                kind: None,
                project: None,
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
                id: bare,
                resolution: "addressed".into(),
                note: None,
            })
            .unwrap();

        assert_eq!(notes.store.notes[0].resolution, NoteResolution::Addressed);
    }

    #[test]
    fn resolve_unknown_id_errors_with_format_hint() {
        let (_tmp, mut notes) = mk_store();
        let e = notes
            .resolve(&NoteResolveParams {
                id: "does-not-exist".into(),
                resolution: "addressed".into(),
                note: None,
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
                task_id: None,
                thread_id: None,
                provider: None,
                bro: None,
            })
            .unwrap();

        let out = notes
            .list(&NoteListParams {
                kind: None,
                project: None,
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
                task_id: None,
                thread_id: None,
                provider: None,
                bro: None,
            })
            .unwrap();

        let preview = notes
            .list(&NoteListParams {
                kind: None,
                project: None,
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
                kind: None,
                project: None,
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

    #[test]
    fn roundtrip_persists() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("notes.json");
        {
            let mut notes = Notes::open(&path).unwrap();
            notes
                .create(&NoteParams {
                    kind: "learned".into(),
                    body: "repo uses bb:managed markers".into(),
                    session_id: None,
                    project: Some("/repo/x".into()),
                    task_id: None,
                    thread_id: None,
                    provider: None,
                    bro: None,
                })
                .unwrap();
        }
        let notes = Notes::open(&path).unwrap();
        assert_eq!(notes.store.notes.len(), 1);
        assert_eq!(notes.store.notes[0].kind, NoteKind::Learned);
    }
}
