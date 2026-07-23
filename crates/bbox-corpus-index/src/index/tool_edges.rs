use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use sha2::{Digest, Sha256};

use super::project_files;
use bbox_chunker::{EdgeConfidence, EdgeProvenance};
use bbox_corpus_core::entity_ref::EntityRef;
use bbox_corpus_core::project_record::ProjectRecord;
use bbox_edge_sidecar::edge_sidecar::Edge;
use bro_transcript::{self as parser, ParsedEvent, ToolCallInfo, ToolCallKind};

pub struct ToolEdgeContext {
    projects: Vec<ToolEdgeProjectAccess>,
    edges_dir: PathBuf,
    emit_sidecars: bool,
    pending_edges: std::sync::Mutex<Vec<(String, Edge)>>,
    /// Session-cwd → resolved base project id memo (gap-72fd5932). Distinct
    /// cwds are few relative to session files, and resolution can git-probe,
    /// so memoize per reindex pass.
    base_project_cache: std::sync::Mutex<BTreeMap<String, Option<String>>>,
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

/// Filesystem authority already validated by the daemon/indexing boundary.
/// This lower corpus crate may consume these roots, but it may not discover a
/// checkout from `ProjectRecord::canonical_path` or probe an alternate
/// worktree on its own.
#[derive(Debug, Clone)]
pub struct ToolEdgeProjectAccess {
    pub project: ProjectRecord,
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
        }
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
            .map(|(access, _)| access.project.project_id.clone());
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
            tracing::debug!(
                path = raw_path,
                cwd = event.cwd.as_deref().unwrap_or(""),
                "skipping tool-call file edge outside registered projects"
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
            &access.project,
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
                &access.project,
                &root,
                access.git_root.as_deref(),
                &absolute_path,
                byte_range,
                &bytes,
            ),
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
            metadata: bash_metadata(event, tool_call, &access.project, line_offset),
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
            metadata: bash_metadata(event, tool_call, &access.project, line_offset),
        };
        self.pending_edges
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push((access.project.project_id.clone(), edge));
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

fn anchor_metadata(
    event: &ParsedEvent,
    tool_call: &ToolCallInfo,
    project: &ProjectRecord,
    root: &Path,
    git_root: Option<&Path>,
    absolute_path: &Path,
    byte_range: Option<(u64, u64)>,
    bytes: &[u8],
) -> BTreeMap<String, String> {
    let rel_path = absolute_path.strip_prefix(root).unwrap_or(absolute_path);
    let mut metadata = BTreeMap::new();
    metadata.insert(
        "anchor.file_path".to_string(),
        rel_path.to_string_lossy().to_string(),
    );
    metadata.insert("anchor.project_id".to_string(), project.project_id.clone());
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
    project: &ProjectRecord,
    line_offset: u64,
) -> BTreeMap<String, String> {
    let mut metadata = BTreeMap::new();
    metadata.insert("anchor.project_id".to_string(), project.project_id.clone());
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
    fn explicit_local_root_works_with_bogus_record_path_and_without_git_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let source = root.join("src.rs");
        fs::write(&source, "pub fn visible() {}\n").unwrap();
        let project = ProjectRecord {
            project_id: "project-1".into(),
            repo_id: None,
            canonical_path: "/unavailable/not-authority".into(),
            registered_at: "2026-01-01T00:00:00Z".into(),
            is_git_repo: true,
            languages: Default::default(),
            aliases: Default::default(),
        };
        let ctx = ToolEdgeContext::with_project_access(
            vec![ToolEdgeProjectAccess {
                project,
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
        let observed = root.join("edges").join("project-1.jsonl");
        assert!(
            !observed.exists(),
            "observed edges must remain staged before final publication"
        );
        let publication = ctx.take_publish_bundle();
        assert!(!publication.is_empty());
        publication.publish().unwrap();
        assert!(observed.exists());
    }
}
