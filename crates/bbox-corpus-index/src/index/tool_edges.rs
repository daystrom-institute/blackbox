use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use sha2::{Digest, Sha256};

use super::project_files;
use bbox_chunker::{EdgeConfidence, EdgeProvenance};
use bbox_corpus_core::entity_ref::EntityRef;
use bbox_edge_sidecar::edge_sidecar::Edge;
use bro_transcript::{self as parser, ParsedEvent, ToolCallInfo, ToolCallKind};

/// How many distinct sessions an unresolvable-path diagnostic names before
/// it stops growing. The diagnostic must stay bounded regardless of corpus
/// size, so the count keeps rising while the sample set does not.
const MAX_UNRESOLVABLE_SAMPLES: usize = 8;

pub struct ToolEdgeContext {
    projects: Vec<ToolEdgeProjectAccess>,
    edges_dir: PathBuf,
    emit_sidecars: bool,
    pending_edges: std::sync::Mutex<Vec<(String, Edge)>>,
    /// Session-cwd → resolved base project id memo (gap-72fd5932). Distinct
    /// cwds are few relative to session files, and resolution can git-probe,
    /// so memoize per reindex pass.
    base_project_cache: std::sync::Mutex<BTreeMap<String, Option<String>>>,
    unresolvable: std::sync::Mutex<ToolEdgePathDiagnostics>,
}

/// Bounded record of tool-call path events that no authorized local root
/// resolves (plan section 9, tool/transcript-edge row).
///
/// A remote-only project contributes no local root to the pass, so its
/// transcript path events cannot be attributed. They are skipped, never
/// re-identified against some other project whose root happens to contain a
/// same-named path, and counted here so the skip is observable rather than
/// silent.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ToolEdgePathDiagnostics {
    /// Total skipped path events. Saturating: a diagnostic must not panic a
    /// reindex pass.
    pub unresolvable_path_events: u64,
    /// Bounded sample of the sessions that produced them.
    pub sample_session_ids: std::collections::BTreeSet<String>,
}

impl ToolEdgePathDiagnostics {
    pub fn is_empty(&self) -> bool {
        self.unresolvable_path_events == 0
    }

    fn record(&mut self, session_id: &str) {
        self.unresolvable_path_events = self.unresolvable_path_events.saturating_add(1);
        if self.sample_session_ids.len() < MAX_UNRESOLVABLE_SAMPLES {
            self.sample_session_ids.insert(session_id.to_string());
        }
    }
}

#[derive(Debug, Default)]
pub struct ToolEdgePublishBundle {
    edges_dir: PathBuf,
    grouped: BTreeMap<String, Vec<Edge>>,
}

impl ToolEdgePublishBundle {
    pub fn is_empty(&self) -> bool {
        self.grouped.is_empty()
    }

    pub fn publish(self) -> Result<usize> {
        let mut written = 0;
        for (project_id, edges) in self.grouped {
            bbox_edge_sidecar::edge_sidecar::append_observed_edges(
                &self.edges_dir,
                &project_id,
                &edges,
            )?;
            written += edges.len();
        }
        Ok(written)
    }
}

/// Pure project identity plus the filesystem authority the daemon/indexing
/// boundary already validated (plan section 4.15).
///
/// The carrier deliberately holds no `ProjectRecord`: the lease that proved
/// these roots lives in the upper `bbox-indexing` layer, and putting a
/// path-bearing record here would give this lower crate a second, unleased
/// way to name a checkout. `local_root` and `git_root` are ephemeral for the
/// duration of one pass and are valid only while that lease is alive.
#[derive(Debug, Clone)]
pub struct ToolEdgeProjectAccess {
    pub project_id: String,
    pub local_root: PathBuf,
    pub git_root: Option<PathBuf>,
}

impl ToolEdgeContext {
    pub fn with_project_access(
        projects: Vec<ToolEdgeProjectAccess>,
        edges_dir: PathBuf,
        emit_sidecars: bool,
    ) -> Self {
        Self {
            projects,
            edges_dir,
            emit_sidecars,
            pending_edges: std::sync::Mutex::default(),
            base_project_cache: std::sync::Mutex::default(),
            unresolvable: std::sync::Mutex::default(),
        }
    }

    /// The bounded unresolvable-path diagnostic accumulated so far.
    pub fn path_diagnostics(&self) -> ToolEdgePathDiagnostics {
        self.unresolvable
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn record_unresolvable_path(&self, event: &ParsedEvent) {
        self.unresolvable
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .record(&event.session_id);
    }

    /// Single-project context for backfill use — restricts edge resolution
    /// to the given project so unrelated transcript paths are cheap to skip.
    pub fn for_project_access(project: ToolEdgeProjectAccess, edges_dir: PathBuf) -> Self {
        Self::with_project_access(vec![project], edges_dir, true)
    }

    /// Resolve a session cwd to the registered base project's id, memoized
    /// across the pass (gap-72fd5932). `None` for empty cwds and paths no
    /// registered project owns.
    pub fn base_project_id_for_cwd(&self, cwd: &str) -> Option<String> {
        if cwd.is_empty() {
            return None;
        }
        let mut cache = self
            .base_project_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(hit) = cache.get(cwd) {
            return hit.clone();
        }
        let resolved = fs::canonicalize(cwd)
            .ok()
            .and_then(|cwd| self.project_for_absolute_path(&cwd))
            .map(|(access, _)| access.project_id.clone());
        cache.insert(cwd.to_string(), resolved.clone());
        resolved
    }

    pub fn emit_event_edges(
        &self,
        event: &ParsedEvent,
        provider: &str,
        line_offset: u64,
        event_idx: u32,
    ) -> Result<usize> {
        if !self.emit_sidecars {
            return Ok(0);
        }
        let Some(tool_call) = event.tool_call.as_ref() else {
            return Ok(0);
        };
        match tool_call.kind {
            ToolCallKind::Read | ToolCallKind::Write | ToolCallKind::Edit => {
                self.emit_file_tool_edge(event, provider, line_offset, event_idx, tool_call)
            }
            ToolCallKind::Bash => {
                self.emit_bash_tool_edge(event, provider, line_offset, event_idx, tool_call)
            }
        }
    }

    /// Build edges for a transcript event without writing them. Used by
    /// backfill paths that collect all edges first and then write with dedup.
    pub fn build_event_edges(
        &self,
        event: &ParsedEvent,
        provider: &str,
        line_offset: u64,
        event_idx: u32,
    ) -> Result<Option<Edge>> {
        let Some(tool_call) = event.tool_call.as_ref() else {
            return Ok(None);
        };
        match tool_call.kind {
            ToolCallKind::Read | ToolCallKind::Write | ToolCallKind::Edit => {
                self.build_file_tool_edge(event, provider, line_offset, event_idx, tool_call)
            }
            ToolCallKind::Bash => {
                self.build_bash_tool_edge(event, provider, line_offset, event_idx, tool_call)
            }
        }
    }

    /// Detach the observed edges accumulated during this pass into an explicit
    /// final-publication bundle. The caller publishes it only while holding the
    /// checkout publication guard that covers the contributing roots.
    pub fn take_publish_bundle(&self) -> ToolEdgePublishBundle {
        let mut pending = self
            .pending_edges
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut grouped = BTreeMap::<String, Vec<Edge>>::new();
        for (project_id, edge) in pending.drain(..) {
            grouped.entry(project_id).or_default().push(edge);
        }
        ToolEdgePublishBundle {
            edges_dir: self.edges_dir.clone(),
            grouped,
        }
    }

    /// Standalone lower-crate indexing has no checkout lease lifecycle. Daemon
    /// callers use `take_publish_bundle` and publish under their authority
    /// fence instead.
    pub fn publish_pending_edges(&self) -> Result<usize> {
        self.take_publish_bundle().publish()
    }

    fn build_file_tool_edge(
        &self,
        event: &ParsedEvent,
        provider: &str,
        line_offset: u64,
        event_idx: u32,
        tool_call: &ToolCallInfo,
    ) -> Result<Option<Edge>> {
        let Some(raw_path) = parser::tool_call_file_path(tool_call) else {
            return Ok(None);
        };
        let Some((access, root, absolute_path)) = self.resolve_project_path(event, raw_path) else {
            // Never re-identified against another project: an unattributable
            // path event is counted and dropped (plan section 9).
            self.record_unresolvable_path(event);
            tracing::debug!(
                path = raw_path,
                cwd = event.cwd.as_deref().unwrap_or(""),
                "skipping tool-call file edge outside registered projects"
            );
            return Ok(None);
        };
        // The anchor is a project-relative path by contract. `absolute_path`
        // came from `project_for_absolute_path`, so it is under `root`; the
        // refusal is the guard against a future caller weakening that.
        let Some(relative_anchor) = normalized_relative_anchor(&root, &absolute_path) else {
            self.record_unresolvable_path(event);
            tracing::debug!(
                path = %absolute_path.display(),
                "skipping tool-call edge; path is not relative to its authorized root"
            );
            return Ok(None);
        };
        let bytes = match fs::read(&absolute_path) {
            Ok(bytes) => bytes,
            Err(err) => {
                tracing::debug!(path = %absolute_path.display(), error = %err, "skipping tool-call edge for unreadable file");
                return Ok(None);
            }
        };
        let byte_range = byte_range_for_tool(tool_call, &bytes);
        let Some(target) = project_files::resolve_current_chunk_entity(
            &access.project_id,
            &root,
            &absolute_path,
            byte_range,
        )?
        else {
            tracing::debug!(path = %absolute_path.display(), "skipping tool-call edge; current chunk target unresolved");
            return Ok(None);
        };
        let source = EntityRef::Transcript {
            provider: provider.to_string(),
            session_id: event.session_id.clone(),
            line_offset,
            event_idx,
        };
        Ok(Some(Edge {
            source,
            kind: match tool_call.kind {
                ToolCallKind::Read => "READ_FILE",
                ToolCallKind::Write | ToolCallKind::Edit => "EDITED_FILE",
                ToolCallKind::Bash => unreachable!(),
            }
            .to_string(),
            target,
            provenance: EdgeProvenance::Explicit,
            // The target points at the current chunk containing the byte range
            // when transcripts are reindexed. The historical identity lives in
            // anchor.* metadata and is resolved by bbox_blame via git blame.
            confidence: EdgeConfidence::Heuristic,
            metadata: anchor_metadata(
                event,
                tool_call,
                &access.project_id,
                &relative_anchor,
                access.git_root.as_deref(),
                byte_range,
                &bytes,
            ),
            // Left None deliberately: `access.project_id` is the indexing
            // lane's id, not durable catalog authority. Only the Phase 6
            // backfill stamps a catalog project onto an edge row (Q-E1).
            project_id: None,
        }))
    }

    fn build_bash_tool_edge(
        &self,
        event: &ParsedEvent,
        provider: &str,
        line_offset: u64,
        event_idx: u32,
        tool_call: &ToolCallInfo,
    ) -> Result<Option<Edge>> {
        let Some((access, _root)) = self.project_for_cwd(event) else {
            self.record_unresolvable_path(event);
            tracing::debug!(
                cwd = event.cwd.as_deref().unwrap_or(""),
                "skipping bash tool edge outside registered projects"
            );
            return Ok(None);
        };
        let source = EntityRef::Transcript {
            provider: provider.to_string(),
            session_id: event.session_id.clone(),
            line_offset,
            event_idx,
        };
        Ok(Some(Edge {
            source,
            kind: "RAN_BASH".to_string(),
            target: EntityRef::BashCall {
                session: event.session_id.clone(),
                turn: line_offset_to_turn(line_offset, event_idx),
            },
            provenance: EdgeProvenance::Explicit,
            confidence: EdgeConfidence::Exact,
            metadata: bash_metadata(event, tool_call, &access.project_id, line_offset),
            // Left None deliberately: `access.project_id` is the indexing
            // lane's id, not durable catalog authority. Only the Phase 6
            // backfill stamps a catalog project onto an edge row (Q-E1).
            project_id: None,
        }))
    }

    fn emit_file_tool_edge(
        &self,
        event: &ParsedEvent,
        provider: &str,
        line_offset: u64,
        event_idx: u32,
        tool_call: &ToolCallInfo,
    ) -> Result<usize> {
        let Some(edge) =
            self.build_file_tool_edge(event, provider, line_offset, event_idx, tool_call)?
        else {
            return Ok(0);
        };
        let project_id = match &edge.target {
            bbox_corpus_core::entity_ref::EntityRef::ProjectFile { project_id, .. }
            | bbox_corpus_core::entity_ref::EntityRef::ProjectFileV2 { project_id, .. } => {
                project_id.clone()
            }
            _ => return Ok(0),
        };
        self.pending_edges
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push((project_id, edge));
        Ok(1)
    }

    fn emit_bash_tool_edge(
        &self,
        event: &ParsedEvent,
        provider: &str,
        line_offset: u64,
        event_idx: u32,
        tool_call: &ToolCallInfo,
    ) -> Result<usize> {
        let Some((access, _root)) = self.project_for_cwd(event) else {
            self.record_unresolvable_path(event);
            tracing::debug!(
                cwd = event.cwd.as_deref().unwrap_or(""),
                "skipping bash tool edge outside registered projects"
            );
            return Ok(0);
        };
        let source = EntityRef::Transcript {
            provider: provider.to_string(),
            session_id: event.session_id.clone(),
            line_offset,
            event_idx,
        };
        let edge = Edge {
            source,
            kind: "RAN_BASH".to_string(),
            target: EntityRef::BashCall {
                session: event.session_id.clone(),
                turn: line_offset_to_turn(line_offset, event_idx),
            },
            provenance: EdgeProvenance::Explicit,
            confidence: EdgeConfidence::Exact,
            metadata: bash_metadata(event, tool_call, &access.project_id, line_offset),
            // Left None deliberately: `access.project_id` is the indexing
            // lane's id, not durable catalog authority. Only the Phase 6
            // backfill stamps a catalog project onto an edge row (Q-E1).
            project_id: None,
        };
        let project_id = access.project_id.clone();
        self.pending_edges
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push((project_id, edge));
        Ok(1)
    }

    // index-build path; runs on the IndexWriterActor / reindex thread.
    #[allow(clippy::disallowed_methods)]
    fn resolve_project_path<'a>(
        &'a self,
        event: &ParsedEvent,
        raw_path: &str,
    ) -> Option<(&'a ToolEdgeProjectAccess, PathBuf, PathBuf)> {
        let raw = Path::new(raw_path);
        let absolute = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            let cwd = event.cwd.as_deref()?;
            Path::new(cwd).join(raw)
        };
        let absolute = fs::canonicalize(absolute).ok()?;
        let (project, root) = self.project_for_absolute_path(&absolute)?;
        Some((project, root, absolute))
    }

    fn project_for_cwd(&self, event: &ParsedEvent) -> Option<(&ToolEdgeProjectAccess, PathBuf)> {
        let cwd = event.cwd.as_deref()?;
        let cwd = fs::canonicalize(cwd).ok()?;
        self.project_for_absolute_path(&cwd)
    }

    fn project_for_absolute_path(
        &self,
        absolute: &Path,
    ) -> Option<(&ToolEdgeProjectAccess, PathBuf)> {
        self.projects
            .iter()
            .filter_map(|access| {
                absolute
                    .starts_with(&access.local_root)
                    .then_some((access, access.local_root.clone()))
            })
            .max_by_key(|(_access, root)| root.as_os_str().len())
    }
}

/// The project-relative anchor for a path inside an authorized root, with
/// separators normalized so the emitted edge is stable across hosts.
///
/// `None` when the path is not under the root: the edge must then be
/// skipped, never anchored to an absolute host path.
fn normalized_relative_anchor(root: &Path, absolute_path: &Path) -> Option<String> {
    let relative = absolute_path.strip_prefix(root).ok()?;
    if relative.as_os_str().is_empty() {
        return None;
    }
    let normalized = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    (!normalized.is_empty()).then_some(normalized)
}

fn anchor_metadata(
    event: &ParsedEvent,
    tool_call: &ToolCallInfo,
    project_id: &str,
    relative_anchor: &str,
    git_root: Option<&Path>,
    byte_range: Option<(u64, u64)>,
    bytes: &[u8],
) -> BTreeMap<String, String> {
    let mut metadata = BTreeMap::new();
    metadata.insert("anchor.file_path".to_string(), relative_anchor.to_string());
    metadata.insert("anchor.project_id".to_string(), project_id.to_string());
    if let Some((start, end)) = byte_range {
        metadata.insert("anchor.byte_start".to_string(), start.to_string());
        metadata.insert("anchor.byte_end".to_string(), end.to_string());
    }
    metadata.insert("anchor.content_hash_at_edit".to_string(), sha256_hex(bytes));
    if let Some(commit_sha) = git_root.and_then(bbox_corpus_core::git::current_head) {
        metadata.insert("anchor.commit_sha_at_edit".to_string(), commit_sha);
    }
    metadata.insert(
        "anchor.edit_timestamp".to_string(),
        event
            .timestamp
            .clone()
            .unwrap_or_else(bbox_corpus_core::util::now_iso),
    );
    metadata.insert("tool.name".to_string(), tool_call.name.clone());
    if let Some(id) = &tool_call.tool_use_id {
        metadata.insert("tool.id".to_string(), id.clone());
    }
    metadata
}

fn bash_metadata(
    event: &ParsedEvent,
    tool_call: &ToolCallInfo,
    project_id: &str,
    line_offset: u64,
) -> BTreeMap<String, String> {
    let mut metadata = BTreeMap::new();
    metadata.insert("anchor.project_id".to_string(), project_id.to_string());
    metadata.insert(
        "anchor.edit_timestamp".to_string(),
        event
            .timestamp
            .clone()
            .unwrap_or_else(bbox_corpus_core::util::now_iso),
    );
    metadata.insert("tool.name".to_string(), tool_call.name.clone());
    if let Some(id) = &tool_call.tool_use_id {
        metadata.insert("tool.id".to_string(), id.clone());
    }
    if let Some(cwd) = &event.cwd {
        metadata.insert("cwd".to_string(), cwd.clone());
    }
    metadata.insert(
        "turn_source_line_offset".to_string(),
        line_offset.to_string(),
    );
    if let Some(command) = parser::tool_call_command(tool_call) {
        metadata.insert("command".to_string(), command.to_string());
    }
    metadata
}

fn byte_range_for_tool(tool_call: &ToolCallInfo, bytes: &[u8]) -> Option<(u64, u64)> {
    match tool_call.kind {
        ToolCallKind::Read => read_byte_range(tool_call, bytes.len()),
        ToolCallKind::Write => Some((0, bytes.len() as u64)),
        ToolCallKind::Edit => edit_byte_range(tool_call, bytes),
        ToolCallKind::Bash => None,
    }
}

fn read_byte_range(tool_call: &ToolCallInfo, file_len: usize) -> Option<(u64, u64)> {
    let offset = tool_call.input.get("offset").and_then(|v| v.as_u64());
    let limit = tool_call.input.get("limit").and_then(|v| v.as_u64());
    match (offset, limit) {
        (Some(start), Some(limit)) => Some((start, start.saturating_add(limit))),
        (Some(start), None) => Some((start, file_len as u64)),
        (None, Some(limit)) => Some((0, limit)),
        (None, None) => Some((0, file_len as u64)),
    }
}

fn edit_byte_range(tool_call: &ToolCallInfo, bytes: &[u8]) -> Option<(u64, u64)> {
    let old_string = tool_call.input.get("old_string")?.as_str()?;
    let content = std::str::from_utf8(bytes).ok()?;
    let start = content.find(old_string)? as u64;
    Some((start, start + old_string.len() as u64))
}

fn line_offset_to_turn(line_offset: u64, event_idx: u32) -> u32 {
    // BashCall refs only have a u32 turn slot, while transcript locations are
    // `(line_offset: u64, event_idx: u32)`. This truncates a SHA-256 tuple hash
    // to 32 bits, so collisions are possible but rare at current per-session
    // volumes; the source transcript ref remains in RAN_BASH edge metadata.
    let mut hasher = Sha256::new();
    hasher.update(line_offset.to_be_bytes());
    hasher.update(event_idx.to_be_bytes());
    let digest = hasher.finalize();
    u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]])
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bro_transcript::{MessageRole, ParsedEvent, ToolCallInfo, ToolCallKind};
    use serde_json::json;

    #[test]
    fn edit_byte_range_uses_old_string_position() {
        let tool_call = ToolCallInfo {
            kind: ToolCallKind::Edit,
            name: "Edit".into(),
            tool_use_id: None,
            input: json!({"old_string": "second"}),
        };

        assert_eq!(
            edit_byte_range(&tool_call, b"first\nsecond\nthird"),
            Some((6, 12))
        );
    }

    #[test]
    fn read_byte_range_uses_offset_and_limit() {
        let tool_call = ToolCallInfo {
            kind: ToolCallKind::Read,
            name: "Read".into(),
            tool_use_id: None,
            input: json!({"offset": 10, "limit": 20}),
        };

        assert_eq!(read_byte_range(&tool_call, 100), Some((10, 30)));
    }

    #[test]
    fn disabled_context_does_not_emit_sidecar_edges() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolEdgeContext {
            projects: Vec::new(),
            edges_dir: dir.path().to_path_buf(),
            emit_sidecars: false,
            pending_edges: Default::default(),
            base_project_cache: Default::default(),
            unresolvable: Default::default(),
        };
        let event = ParsedEvent {
            role: MessageRole::ToolUse,
            content: String::new(),
            session_id: "sess-1".into(),
            timestamp: None,
            git_branch: None,
            is_subagent: false,
            agent_slug: None,
            cwd: Some("/tmp".into()),
            tool_call: Some(ToolCallInfo {
                kind: ToolCallKind::Bash,
                name: "Bash".into(),
                tool_use_id: None,
                input: json!({"command": "echo hi"}),
            }),
        };

        assert_eq!(ctx.emit_event_edges(&event, "claude", 42, 0).unwrap(), 0);
        assert!(fs::read_dir(dir.path()).unwrap().next().is_none());
    }

    #[test]
    fn line_offset_to_turn_has_no_collisions_for_synthetic_session() {
        let mut seen = std::collections::HashSet::new();
        for idx in 0..10_000u32 {
            let line_offset = u64::from(idx) * 137;
            let event_idx = idx % 5;
            assert!(
                seen.insert(line_offset_to_turn(line_offset, event_idx)),
                "unexpected turn collision at synthetic event {idx}"
            );
        }
    }

    #[test]
    fn explicit_local_root_resolves_edges_without_a_record_or_git_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let source = root.join("src.rs");
        fs::write(&source, "pub fn visible() {}\n").unwrap();
        let ctx = ToolEdgeContext::with_project_access(
            vec![ToolEdgeProjectAccess {
                project_id: "project-1".into(),
                local_root: root.clone(),
                git_root: None,
            }],
            root.join("edges"),
            true,
        );
        let event = ParsedEvent {
            role: MessageRole::ToolUse,
            content: String::new(),
            session_id: "sess-1".into(),
            timestamp: None,
            git_branch: None,
            is_subagent: false,
            agent_slug: None,
            cwd: Some(root.to_string_lossy().into_owned()),
            tool_call: Some(ToolCallInfo {
                kind: ToolCallKind::Read,
                name: "Read".into(),
                tool_use_id: None,
                input: json!({"file_path": source}),
            }),
        };

        let edge = ctx
            .build_event_edges(&event, "claude", 10, 0)
            .unwrap()
            .expect("authorized local root resolves the file");
        assert_eq!(edge.metadata["anchor.file_path"], "src.rs");
        assert!(!edge.metadata.contains_key("anchor.commit_sha_at_edit"));

        assert_eq!(ctx.emit_event_edges(&event, "claude", 10, 0).unwrap(), 1);
        let observed = root.join("edges/observed/project-1.jsonl");
        assert!(
            !observed.exists(),
            "observed edges must remain staged before final publication"
        );
        let publication = ctx.take_publish_bundle();
        assert!(!publication.is_empty());
        publication.publish().unwrap();
        assert!(observed.exists());
        assert!(
            ctx.path_diagnostics().is_empty(),
            "an attributable path event is not a diagnostic"
        );
    }

    fn read_event(session_id: &str, cwd: &Path, file_path: &Path) -> ParsedEvent {
        ParsedEvent {
            role: MessageRole::ToolUse,
            content: String::new(),
            session_id: session_id.into(),
            timestamp: None,
            git_branch: None,
            is_subagent: false,
            agent_slug: None,
            cwd: Some(cwd.to_string_lossy().into_owned()),
            tool_call: Some(ToolCallInfo {
                kind: ToolCallKind::Read,
                name: "Read".into(),
                tool_use_id: None,
                input: json!({ "file_path": file_path }),
            }),
        }
    }

    /// A remote-only project contributes no local root, so its transcript
    /// path events are unattributable. They must be counted and dropped, and
    /// in particular must NOT be re-identified against the one project that
    /// does have a root in this pass.
    #[test]
    fn unresolved_path_events_are_diagnosed_and_never_reidentified() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let attached = root.join("attached");
        let remote = root.join("remote");
        fs::create_dir_all(&attached).unwrap();
        fs::create_dir_all(&remote).unwrap();
        let remote_source = remote.join("src.rs");
        fs::write(&remote_source, "pub fn elsewhere() {}\n").unwrap();

        let ctx = ToolEdgeContext::with_project_access(
            vec![ToolEdgeProjectAccess {
                project_id: "attached-project".into(),
                local_root: attached.clone(),
                git_root: None,
            }],
            root.join("edges"),
            true,
        );
        let event = read_event("sess-remote", &remote, &remote_source);

        assert!(
            ctx.build_event_edges(&event, "claude", 10, 0)
                .unwrap()
                .is_none()
        );
        assert_eq!(ctx.emit_event_edges(&event, "claude", 10, 0).unwrap(), 0);
        assert!(
            ctx.take_publish_bundle().is_empty(),
            "an unattributable path event must not be re-identified onto the attached project"
        );

        let diagnostics = ctx.path_diagnostics();
        assert!(!diagnostics.is_empty());
        assert_eq!(diagnostics.unresolvable_path_events, 2);
        assert_eq!(
            diagnostics.sample_session_ids,
            std::collections::BTreeSet::from(["sess-remote".to_string()])
        );
    }

    /// The diagnostic must stay bounded no matter how large the corpus is:
    /// the count keeps rising, the sample set does not.
    #[test]
    fn unresolvable_path_diagnostic_sample_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let outside = root.join("outside");
        fs::create_dir_all(&outside).unwrap();
        let source = outside.join("src.rs");
        fs::write(&source, "pub fn elsewhere() {}\n").unwrap();
        let ctx = ToolEdgeContext::with_project_access(Vec::new(), root.join("edges"), true);

        for index in 0..(MAX_UNRESOLVABLE_SAMPLES * 3) {
            let event = read_event(&format!("sess-{index}"), &outside, &source);
            assert_eq!(ctx.emit_event_edges(&event, "claude", 10, 0).unwrap(), 0);
        }

        let diagnostics = ctx.path_diagnostics();
        assert_eq!(
            diagnostics.unresolvable_path_events,
            (MAX_UNRESOLVABLE_SAMPLES * 3) as u64
        );
        assert_eq!(
            diagnostics.sample_session_ids.len(),
            MAX_UNRESOLVABLE_SAMPLES
        );
    }

    #[test]
    fn anchor_file_path_is_the_normalized_project_relative_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let nested = root.join("crates").join("inner");
        fs::create_dir_all(&nested).unwrap();
        let source = nested.join("src.rs");
        fs::write(&source, "pub fn nested() {}\n").unwrap();
        let ctx = ToolEdgeContext::with_project_access(
            vec![ToolEdgeProjectAccess {
                project_id: "project-1".into(),
                local_root: root.clone(),
                git_root: None,
            }],
            root.join("edges"),
            true,
        );

        let edge = ctx
            .build_event_edges(&read_event("sess-1", &root, &source), "claude", 10, 0)
            .unwrap()
            .expect("nested file resolves under the authorized root");
        assert_eq!(edge.metadata["anchor.file_path"], "crates/inner/src.rs");
        assert_eq!(edge.metadata["anchor.project_id"], "project-1");
    }

    #[test]
    fn normalized_relative_anchor_refuses_paths_outside_the_root() {
        let root = Path::new("/authorized/root");
        assert_eq!(
            normalized_relative_anchor(root, Path::new("/authorized/root/a/b.rs")),
            Some("a/b.rs".to_string())
        );
        assert_eq!(normalized_relative_anchor(root, root), None);
        assert_eq!(
            normalized_relative_anchor(root, Path::new("/elsewhere/b.rs")),
            None
        );
    }
}
