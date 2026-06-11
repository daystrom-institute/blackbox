use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use sha2::{Digest, Sha256};

use super::{ReindexConfig, project_files};
use crate::chunker::{EdgeConfidence, EdgeProvenance};
use crate::edge_index::Edge;
use crate::entity_ref::EntityRef;
use crate::parser::{self, ParsedEvent, ToolCallInfo, ToolCallKind};
use bbox_corpus_core::project_record::{ProjectRecord, load_project_records};

pub struct ToolEdgeContext {
    projects: Vec<ProjectRecord>,
    edges_dir: PathBuf,
    emit_sidecars: bool,
}

impl ToolEdgeContext {
    pub(super) fn from_config(config: &ReindexConfig, emit_sidecars: bool) -> Result<Self> {
        Ok(Self {
            projects: load_project_records(&config.projects_path)?,
            edges_dir: crate::edge_index::edges_dir_from_projects_path(&config.projects_path),
            emit_sidecars,
        })
    }

    /// Single-project context for backfill use — restricts edge resolution
    /// to the given project so unrelated transcript paths are cheap to skip.
    pub(super) fn for_project(project: ProjectRecord, edges_dir: PathBuf) -> Self {
        Self {
            projects: vec![project],
            edges_dir,
            emit_sidecars: true,
        }
    }

    pub(super) fn emit_event_edges(
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
    pub(super) fn build_event_edges(
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
        let Some((project, root, absolute_path)) = self.resolve_project_path(event, raw_path)
        else {
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
            project,
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
                project,
                &root,
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
        let Some((project, _root)) = self.project_for_cwd(event) else {
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
            metadata: bash_metadata(event, tool_call, project, line_offset),
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
            crate::entity_ref::EntityRef::ProjectFile { project_id, .. }
            | crate::entity_ref::EntityRef::ProjectFileV2 { project_id, .. } => project_id.clone(),
            _ => return Ok(0),
        };
        crate::edge_index::append_observed_edges(&self.edges_dir, &project_id, &[edge])?;
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
        let Some((project, _root)) = self.project_for_cwd(event) else {
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
            metadata: bash_metadata(event, tool_call, project, line_offset),
        };
        crate::edge_index::append_observed_edges(&self.edges_dir, &project.project_id, &[edge])?;
        Ok(1)
    }

    // index-build path; runs on the IndexWriterActor / reindex thread.
    #[allow(clippy::disallowed_methods)]
    fn resolve_project_path<'a>(
        &'a self,
        event: &ParsedEvent,
        raw_path: &str,
    ) -> Option<(&'a ProjectRecord, PathBuf, PathBuf)> {
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

    fn project_for_cwd(&self, event: &ParsedEvent) -> Option<(&ProjectRecord, PathBuf)> {
        let cwd = event.cwd.as_deref()?;
        let cwd = fs::canonicalize(cwd).ok()?;
        self.project_for_absolute_path(&cwd)
    }

    fn project_for_absolute_path(&self, absolute: &Path) -> Option<(&ProjectRecord, PathBuf)> {
        self.projects
            .iter()
            .filter_map(|project| {
                let root = fs::canonicalize(&project.canonical_path).ok()?;
                absolute.starts_with(&root).then_some((project, root))
            })
            .max_by_key(|(_project, root)| root.as_os_str().len())
    }
}

fn anchor_metadata(
    event: &ParsedEvent,
    tool_call: &ToolCallInfo,
    project: &ProjectRecord,
    root: &Path,
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
    if let Some(commit_sha) = crate::git::current_head(root) {
        metadata.insert("anchor.commit_sha_at_edit".to_string(), commit_sha);
    }
    metadata.insert(
        "anchor.edit_timestamp".to_string(),
        event.timestamp.clone().unwrap_or_else(bbox_corpus_core::util::now_iso),
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
        event.timestamp.clone().unwrap_or_else(bbox_corpus_core::util::now_iso),
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
    use crate::parser::{MessageRole, ParsedEvent, ToolCallInfo, ToolCallKind};
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
}
