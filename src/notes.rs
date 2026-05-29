use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result};
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── MCP parameter structs ─────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NoteParams {
    /// One of: dispute, assumption, surprise, followup, blocked, learned, done
    pub kind: String,
    /// Short note body (1–3 sentences). For substrate gap reports, pass a
    /// `blackbox.gap_note.v1` JSON object here with `kind="followup"`; required
    /// fields are `type`, `title`, `gap_kind`, `domain`, and
    /// `wanted_capability`.
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

// ── Gap-note view ─────────────────────────────────────────────────

const GAP_NOTE_TYPE: &str = "blackbox.gap_note.v1";
const GAP_NOTE_FIELD_TYPE: &str = "type";
const GAP_NOTE_FIELD_TITLE: &str = "title";
const GAP_NOTE_FIELD_GAP_KIND: &str = "gap_kind";
const GAP_NOTE_FIELD_DOMAIN: &str = "domain";
const GAP_NOTE_FIELD_IMPACT: &str = "impact";
const GAP_NOTE_FIELD_BLOCKING_LEVEL: &str = "blocking_level";
const GAP_NOTE_FIELD_DEDUPE_KEY: &str = "dedupe_key";
const GAP_NOTE_FIELD_WANTED_CAPABILITY: &str = "wanted_capability";

const GAP_NOTE_KINDS: &[&str] = &[
    "packet_ast",
    "tooling",
    "agent",
    "workflow",
    "refactor_primitive",
    "mcp_surface",
    "ontology",
    "eval_coverage",
    "docs_runbook",
];
const GAP_NOTE_IMPACTS: &[&str] = &["low", "medium", "high", "critical"];
const GAP_NOTE_BLOCKING_LEVELS: &[&str] = &[
    "none",
    "workaround_available",
    "blocks_task",
    "blocks_class_of_work",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum GapImpact {
    Low,
    Medium,
    High,
    Critical,
}

impl GapImpact {
    pub fn parse(value: Option<&str>) -> Self {
        match value {
            Some("critical") => Self::Critical,
            Some("high") => Self::High,
            Some("low") => Self::Low,
            Some("medium") | None => Self::Medium,
            Some(_) => Self::Medium,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
        }
    }
}

pub struct GapNoteView<'a> {
    pub note: &'a Note,
    pub title: String,
    pub gap_kind: Option<String>,
    pub domain: Option<String>,
    pub impact: GapImpact,
    pub blocking_level: Option<String>,
    pub dedupe_key: Option<String>,
    pub wanted_capability: Option<String>,
}

impl<'a> GapNoteView<'a> {
    pub fn parse(note: &'a Note) -> Option<Self> {
        if note.kind != NoteKind::Followup {
            return None;
        }

        let value = serde_json::from_str::<Value>(&note.body).ok()?;
        let object = value.as_object()?;
        if object.get(GAP_NOTE_FIELD_TYPE).and_then(Value::as_str) != Some(GAP_NOTE_TYPE) {
            return None;
        }

        Some(Self {
            note,
            title: string_field(object, GAP_NOTE_FIELD_TITLE)
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| truncate_chars(&note.body, 120)),
            gap_kind: string_field(object, GAP_NOTE_FIELD_GAP_KIND),
            domain: string_field(object, GAP_NOTE_FIELD_DOMAIN),
            impact: GapImpact::parse(object.get(GAP_NOTE_FIELD_IMPACT).and_then(Value::as_str)),
            blocking_level: string_field(object, GAP_NOTE_FIELD_BLOCKING_LEVEL),
            dedupe_key: string_field(object, GAP_NOTE_FIELD_DEDUPE_KEY),
            wanted_capability: string_field(object, GAP_NOTE_FIELD_WANTED_CAPABILITY),
        })
    }

    #[cfg(test)]
    pub fn to_json_value(&self) -> Value {
        let mut object = serde_json::Map::new();
        object.insert(
            GAP_NOTE_FIELD_TYPE.to_owned(),
            Value::String(GAP_NOTE_TYPE.to_owned()),
        );
        object.insert(
            GAP_NOTE_FIELD_TITLE.to_owned(),
            Value::String(self.title.clone()),
        );
        if let Some(value) = &self.gap_kind {
            object.insert(
                GAP_NOTE_FIELD_GAP_KIND.to_owned(),
                Value::String(value.clone()),
            );
        }
        if let Some(value) = &self.domain {
            object.insert(
                GAP_NOTE_FIELD_DOMAIN.to_owned(),
                Value::String(value.clone()),
            );
        }
        object.insert(
            GAP_NOTE_FIELD_IMPACT.to_owned(),
            Value::String(self.impact.as_str().to_owned()),
        );
        if let Some(value) = &self.blocking_level {
            object.insert(
                GAP_NOTE_FIELD_BLOCKING_LEVEL.to_owned(),
                Value::String(value.clone()),
            );
        }
        if let Some(value) = &self.dedupe_key {
            object.insert(
                GAP_NOTE_FIELD_DEDUPE_KEY.to_owned(),
                Value::String(value.clone()),
            );
        }
        if let Some(value) = &self.wanted_capability {
            object.insert(
                GAP_NOTE_FIELD_WANTED_CAPABILITY.to_owned(),
                Value::String(value.clone()),
            );
        }
        Value::Object(object)
    }
}

fn string_field(object: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

fn validate_gap_note_submission(kind: NoteKind, body: &str) -> Result<()> {
    let trimmed = body.trim();
    let mentions_gap_note = trimmed.contains(GAP_NOTE_TYPE);
    let parsed = match serde_json::from_str::<Value>(trimmed) {
        Ok(value) => value,
        Err(err) => {
            if mentions_gap_note {
                anyhow::bail!("invalid {GAP_NOTE_TYPE} JSON body: {err}");
            }
            return Ok(());
        }
    };

    let Some(object) = parsed.as_object() else {
        if mentions_gap_note {
            anyhow::bail!("{GAP_NOTE_TYPE} body must be a JSON object");
        }
        return Ok(());
    };

    let Some(note_type) = object.get(GAP_NOTE_FIELD_TYPE).and_then(Value::as_str) else {
        if mentions_gap_note {
            anyhow::bail!("{GAP_NOTE_TYPE} body must include `type`");
        }
        return Ok(());
    };

    if note_type != GAP_NOTE_TYPE {
        return Ok(());
    }
    if kind != NoteKind::Followup {
        anyhow::bail!("{GAP_NOTE_TYPE} reports must use kind=\"followup\"");
    }

    let required = [
        GAP_NOTE_FIELD_TITLE,
        GAP_NOTE_FIELD_GAP_KIND,
        GAP_NOTE_FIELD_DOMAIN,
        GAP_NOTE_FIELD_WANTED_CAPABILITY,
    ];
    let missing: Vec<&str> = required
        .iter()
        .copied()
        .filter(|field| string_field(object, field).is_none())
        .collect();
    if !missing.is_empty() {
        anyhow::bail!(
            "{GAP_NOTE_TYPE} missing required field(s): {}",
            missing.join(", ")
        );
    }

    validate_gap_note_enum(object, GAP_NOTE_FIELD_GAP_KIND, GAP_NOTE_KINDS)?;
    validate_gap_note_enum(object, GAP_NOTE_FIELD_IMPACT, GAP_NOTE_IMPACTS)?;
    validate_gap_note_enum(
        object,
        GAP_NOTE_FIELD_BLOCKING_LEVEL,
        GAP_NOTE_BLOCKING_LEVELS,
    )?;

    if let Some(dedupe_key) = string_field(object, GAP_NOTE_FIELD_DEDUPE_KEY) {
        let segments: Vec<&str> = dedupe_key.split('/').collect();
        if segments.len() < 3 || segments.iter().any(|segment| segment.trim().is_empty()) {
            anyhow::bail!("{GAP_NOTE_TYPE} dedupe_key must use `<gap_kind>/<domain>/<slug>`");
        }
    }

    Ok(())
}

fn validate_gap_note_enum(
    object: &serde_json::Map<String, Value>,
    field: &str,
    allowed: &[&str],
) -> Result<()> {
    let Some(value) = string_field(object, field) else {
        return Ok(());
    };
    if allowed.contains(&value.as_str()) {
        Ok(())
    } else {
        anyhow::bail!(
            "{GAP_NOTE_TYPE} field `{field}` must be one of: {}",
            allowed.join(", ")
        )
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
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
        validate_gap_note_submission(kind, &p.body)?;

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

        let include_addressed = p.include_addressed.unwrap_or(p.id.is_some());
        let full = p.full.unwrap_or(false);
        let limit = p.limit.unwrap_or(50).max(1) as usize;

        let id_filter = p.id.as_deref().map(str::to_ascii_lowercase);
        let query_lower = p.query.as_deref().map(|s| s.to_lowercase());
        let project_lower = p.project.as_deref().map(|s| s.to_lowercase());

        let mut results: Vec<&Note> = self
            .store
            .notes
            .iter()
            .filter(|n| {
                if let Some(id) = id_filter.as_deref() {
                    let needle = id.strip_prefix("note-").unwrap_or(id);
                    if n.id != id && n.id.strip_prefix("note-") != Some(needle) {
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

    #[test]
    fn workload_retro_prompt_names_only_valid_gap_kinds() {
        // The retro probe (orchestration::WORKLOAD_RETRO_PROMPT) instructs
        // bros to file gap notes with a specific gap_kind. Those tokens
        // must stay in sync with the validator's GAP_NOTE_KINDS — a
        // mismatch makes every retro bbox_note call fail validation. This
        // guard fails loudly if either side drifts.
        let prompt = crate::orchestration::WORKLOAD_RETRO_PROMPT;
        for kind in [
            "mcp_surface",
            "tooling",
            "workflow",
            "agent",
            "docs_runbook",
            "refactor_primitive",
            "ontology",
            "eval_coverage",
        ] {
            assert!(
                prompt.contains(kind),
                "retro prompt no longer names gap_kind {kind:?}"
            );
            assert!(
                GAP_NOTE_KINDS.contains(&kind),
                "retro prompt names gap_kind {kind:?} that is not in GAP_NOTE_KINDS"
            );
        }
    }

    fn mk_store() -> (tempfile::TempDir, Notes) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("notes.json");
        let notes = Notes::open(&path).unwrap();
        (dir, notes)
    }

    fn followup_note(body: &str) -> Note {
        Note {
            id: "note-00000001".into(),
            kind: NoteKind::Followup,
            body: body.into(),
            task_id: None,
            session_id: None,
            project: Some("/repo/x".into()),
            thread_id: None,
            provider: None,
            bro: None,
            resolution: NoteResolution::Unresolved,
            created_at: "2026-05-12T00:00:00Z".into(),
            updated_at: "2026-05-12T00:00:00Z".into(),
            resolved_at: None,
            resolution_note: None,
        }
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
                id: None,
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
    fn list_filters_by_exact_id_with_bare_suffix_fallback() {
        let (_tmp, mut notes) = mk_store();
        notes
            .create(&NoteParams {
                kind: "done".into(),
                body: "target body".into(),
                session_id: None,
                project: None,
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
                task_id: None,
                thread_id: None,
                provider: None,
                bro: None,
            })
            .unwrap();
        let other_id = notes.store.notes[1].id.clone();

        notes
            .resolve(&NoteResolveParams {
                id: target_id.clone(),
                resolution: "addressed".into(),
                note: None,
            })
            .unwrap();

        let by_canonical_id = notes
            .list(&NoteListParams {
                id: Some(target_id.clone()),
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
        assert!(by_canonical_id.contains("target body"));
        assert!(!by_canonical_id.contains("other body"));

        let bare_id = target_id.strip_prefix("note-").unwrap().to_string();
        let by_bare_id = notes
            .list(&NoteListParams {
                id: Some(bare_id),
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
        assert!(by_bare_id.contains("target body"));

        let id_as_query = notes
            .list(&NoteListParams {
                id: None,
                kind: None,
                project: None,
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
    fn gap_note_compact_json_body_parses() {
        let note = followup_note(
            r#"{"type":"blackbox.gap_note.v1","title":"Packet AST cannot express regex","gap_kind":"packet_ast","domain":"review-policy","impact":"high","blocking_level":"workaround_available","dedupe_key":"packet_ast/review-policy/regex","wanted_capability":"regex matching"}"#,
        );

        let view = GapNoteView::parse(&note).unwrap();

        assert_eq!(view.note.id, "note-00000001");
        assert_eq!(view.title, "Packet AST cannot express regex");
        assert_eq!(view.gap_kind.as_deref(), Some("packet_ast"));
        assert_eq!(view.domain.as_deref(), Some("review-policy"));
        assert_eq!(view.impact, GapImpact::High);
        assert_eq!(view.blocking_level.as_deref(), Some("workaround_available"));
        assert_eq!(
            view.dedupe_key.as_deref(),
            Some("packet_ast/review-policy/regex")
        );
        assert_eq!(view.wanted_capability.as_deref(), Some("regex matching"));
    }

    #[test]
    fn gap_note_view_roundtrips_normalized_json_body() {
        let body = serde_json::json!({
            "type": "blackbox.gap_note.v1",
            "title": "Packet AST cannot express regex",
            "gap_kind": "packet_ast",
            "domain": "review-policy",
            "impact": "high",
            "blocking_level": "workaround_available",
            "dedupe_key": "packet_ast/review-policy/regex",
            "wanted_capability": "regex matching"
        })
        .to_string();
        let note = followup_note(&body);

        let view = GapNoteView::parse(&note).unwrap();
        let regenerated = view.to_json_value();
        let regenerated_body = serde_json::to_string(&regenerated).unwrap();
        let reparsed_note = followup_note(&regenerated_body);
        let reparsed = GapNoteView::parse(&reparsed_note).unwrap();

        assert_eq!(regenerated["type"].as_str(), Some(GAP_NOTE_TYPE));
        assert_eq!(reparsed.to_json_value(), regenerated);
    }

    #[test]
    fn gap_note_pretty_json_body_parses() {
        let body = serde_json::to_string_pretty(&serde_json::json!({
            "type": "blackbox.gap_note.v1",
            "title": "Need synonym matching",
            "impact": "critical"
        }))
        .unwrap();
        let note = followup_note(&body);

        let view = GapNoteView::parse(&note).unwrap();

        assert_eq!(view.title, "Need synonym matching");
        assert_eq!(view.impact, GapImpact::Critical);
    }

    #[test]
    fn gap_note_missing_type_is_ignored() {
        let note = followup_note(r#"{"title":"Missing type","impact":"high"}"#);

        assert!(GapNoteView::parse(&note).is_none());
    }

    #[test]
    fn gap_note_malformed_json_is_ignored() {
        let note = followup_note(r#"{"type":"blackbox.gap_note.v1""#);

        assert!(GapNoteView::parse(&note).is_none());
    }

    #[test]
    fn gap_note_unknown_impact_ranks_as_medium() {
        let note = followup_note(
            r#"{"type":"blackbox.gap_note.v1","title":"Unknown impact","impact":"urgent"}"#,
        );

        let view = GapNoteView::parse(&note).unwrap();

        assert_eq!(view.impact, GapImpact::Medium);
    }

    #[test]
    fn create_accepts_valid_gap_note_followup() {
        let (_tmp, mut notes) = mk_store();
        let body = serde_json::json!({
            "type": "blackbox.gap_note.v1",
            "title": "Need section extract",
            "gap_kind": "refactor_primitive",
            "domain": "rust",
            "wanted_capability": "Extract a bounded Rust section.",
            "impact": "high",
            "blocking_level": "workaround_available",
            "dedupe_key": "refactor_primitive/rust/section-extract"
        })
        .to_string();

        notes
            .create(&NoteParams {
                kind: "followup".into(),
                body,
                session_id: None,
                project: None,
                task_id: None,
                thread_id: None,
                provider: None,
                bro: None,
            })
            .unwrap();
    }

    #[test]
    fn create_rejects_malformed_gap_note_body() {
        let (_tmp, mut notes) = mk_store();
        let err = notes
            .create(&NoteParams {
                kind: "followup".into(),
                body: r#"{"type":"blackbox.gap_note.v1""#.into(),
                session_id: None,
                project: None,
                task_id: None,
                thread_id: None,
                provider: None,
                bro: None,
            })
            .unwrap_err()
            .to_string();

        assert!(err.contains("invalid blackbox.gap_note.v1 JSON body"));
    }

    #[test]
    fn create_rejects_gap_note_missing_required_fields() {
        let (_tmp, mut notes) = mk_store();
        let err = notes
            .create(&NoteParams {
                kind: "followup".into(),
                body: r#"{"type":"blackbox.gap_note.v1","title":"Need thing"}"#.into(),
                session_id: None,
                project: None,
                task_id: None,
                thread_id: None,
                provider: None,
                bro: None,
            })
            .unwrap_err()
            .to_string();

        assert!(err.contains("missing required field"));
        assert!(err.contains("gap_kind"));
        assert!(err.contains("domain"));
        assert!(err.contains("wanted_capability"));
    }

    #[test]
    fn create_rejects_gap_note_with_wrong_kind() {
        let (_tmp, mut notes) = mk_store();
        let body = serde_json::json!({
            "type": "blackbox.gap_note.v1",
            "title": "Need section extract",
            "gap_kind": "refactor_primitive",
            "domain": "rust",
            "wanted_capability": "Extract a bounded Rust section."
        })
        .to_string();

        let err = notes
            .create(&NoteParams {
                kind: "surprise".into(),
                body,
                session_id: None,
                project: None,
                task_id: None,
                thread_id: None,
                provider: None,
                bro: None,
            })
            .unwrap_err()
            .to_string();

        assert!(err.contains("must use kind=\"followup\""));
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
                id: None,
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
                id: None,
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
                id: None,
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
                id: None,
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
                id: None,
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
                id: None,
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
