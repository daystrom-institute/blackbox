use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use sha2::{Digest, Sha256};

use bbox_chunker::{EdgeConfidence, EdgeProvenance};
use bbox_corpus_core::entity_ref::EntityRef;
use bbox_edge_sidecar::edge_sidecar::Edge;
use bro_transcript::{self as parser, ParsedEvent, ToolCallInfo, ToolCallKind};

/// How many distinct sessions an unresolvable-cwd diagnostic names before it
/// stops growing. The diagnostic must stay bounded regardless of corpus
/// size, so the count keeps rising while the sample set does not.
const MAX_UNRESOLVABLE_SAMPLES: usize = 8;

/// Observed tool-call edges for one reindex pass: one `RAN_BASH` edge per
/// shell tool call whose session cwd lies under an authorized project root.
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

/// Bounded record of tool-call events whose cwd no authorized local root
/// resolves (plan section 9, tool/transcript-edge row).
///
/// A remote-only project contributes no local root to the pass, so its
/// transcript events cannot be attributed. They are skipped, never
/// re-identified against some other project whose root happens to contain a
/// same-named path, and counted here so the skip is observable rather than
/// silent.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ToolEdgePathDiagnostics {
    /// Total skipped events. Saturating: a diagnostic must not panic a
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

/// Pure project identity plus one validated root for transcript attribution.
///
/// The carrier deliberately holds no `ProjectRecord`. A local root is valid
/// only while its upper-layer checkout lease is alive. A collected project's
/// attachment path serves strictly as a lexical transcript namespace: nothing
/// reads that checkout.
#[derive(Clone)]
pub struct ToolEdgeProjectAccess {
    pub project_id: String,
    pub source: ToolEdgeProjectSource,
}

#[derive(Clone)]
pub enum ToolEdgeProjectSource {
    Local { local_root: PathBuf },
    Collected { transcript_root: PathBuf },
}

impl ToolEdgeProjectAccess {
    pub fn local(project_id: impl Into<String>, local_root: PathBuf) -> Self {
        Self {
            project_id: project_id.into(),
            source: ToolEdgeProjectSource::Local { local_root },
        }
    }

    pub fn collected(project_id: impl Into<String>, transcript_root: PathBuf) -> Self {
        Self {
            project_id: project_id.into(),
            source: ToolEdgeProjectSource::Collected { transcript_root },
        }
    }
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

    /// The bounded unresolvable-cwd diagnostic accumulated so far.
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
    /// to the given project so unrelated transcript events are cheap to skip.
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
        let resolved = self
            .project_for_cwd_path(cwd)
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
        let Some((project_id, edge)) =
            self.build_attributed_edge(event, provider, line_offset, event_idx)
        else {
            return Ok(0);
        };
        self.pending_edges
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push((project_id, edge));
        Ok(1)
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
        Ok(self
            .build_attributed_edge(event, provider, line_offset, event_idx)
            .map(|(_, edge)| edge))
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

    /// The observed edge for one shell tool call and the project it belongs
    /// to. Every other tool call contributes no edge.
    fn build_attributed_edge(
        &self,
        event: &ParsedEvent,
        provider: &str,
        line_offset: u64,
        event_idx: u32,
    ) -> Option<(String, Edge)> {
        let tool_call = event.tool_call.as_ref()?;
        if tool_call.kind != ToolCallKind::Bash {
            return None;
        }
        let Some((access, _root)) = self.project_for_cwd(event) else {
            self.record_unresolvable_path(event);
            tracing::debug!(
                cwd = event.cwd.as_deref().unwrap_or(""),
                "skipping bash tool edge outside registered projects"
            );
            return None;
        };
        let edge = Edge {
            source: EntityRef::Transcript {
                provider: provider.to_string(),
                session_id: event.session_id.clone(),
                line_offset,
                event_idx,
            },
            kind: "RAN_BASH".to_string(),
            target: EntityRef::BashCall {
                session: event.session_id.clone(),
                turn: line_offset_to_turn(line_offset, event_idx),
            },
            provenance: EdgeProvenance::Explicit,
            confidence: EdgeConfidence::Exact,
            metadata: bash_metadata(event, tool_call, line_offset),
            // Left None deliberately: `access.project_id` is the indexing
            // lane's id, not durable catalog authority. Only the Phase 6
            // backfill stamps a catalog project onto an edge row (Q-E1).
            project_id: None,
        };
        Some((access.project_id.clone(), edge))
    }

    fn project_for_cwd(&self, event: &ParsedEvent) -> Option<(&ToolEdgeProjectAccess, PathBuf)> {
        self.project_for_cwd_path(event.cwd.as_deref()?)
    }

    // Index-build path; runs on the IndexWriterActor / reindex thread.
    #[allow(clippy::disallowed_methods)]
    fn project_for_cwd_path(&self, cwd: &str) -> Option<(&ToolEdgeProjectAccess, PathBuf)> {
        let local = fs::canonicalize(cwd)
            .ok()
            .and_then(|cwd| self.project_for_absolute_path(&cwd, true));
        let collected = normalize_lexical_absolute(Path::new(cwd))
            .and_then(|cwd| self.project_for_absolute_path(&cwd, false));
        most_specific_project(local, collected)
    }

    fn project_for_absolute_path(
        &self,
        absolute: &Path,
        local: bool,
    ) -> Option<(&ToolEdgeProjectAccess, PathBuf)> {
        self.projects
            .iter()
            .filter_map(|access| {
                let root = match &access.source {
                    ToolEdgeProjectSource::Local { local_root } if local => local_root,
                    ToolEdgeProjectSource::Collected { transcript_root } if !local => {
                        transcript_root
                    }
                    _ => return None,
                };
                absolute.starts_with(root).then_some((access, root.clone()))
            })
            .max_by_key(|(_access, root)| root.as_os_str().len())
    }
}

fn most_specific_project<'a>(
    left: Option<(&'a ToolEdgeProjectAccess, PathBuf)>,
    right: Option<(&'a ToolEdgeProjectAccess, PathBuf)>,
) -> Option<(&'a ToolEdgeProjectAccess, PathBuf)> {
    match (left, right) {
        (Some(left), Some(right)) => {
            if left.1.as_os_str().len() >= right.1.as_os_str().len() {
                Some(left)
            } else {
                Some(right)
            }
        }
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn normalize_lexical_absolute(path: &Path) -> Option<PathBuf> {
    use std::path::Component;

    if !path.is_absolute() {
        return None;
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            Component::Normal(value) => normalized.push(value),
        }
    }
    Some(normalized)
}

fn bash_metadata(
    event: &ParsedEvent,
    tool_call: &ToolCallInfo,
    line_offset: u64,
) -> BTreeMap<String, String> {
    let mut metadata = BTreeMap::new();
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

#[cfg(test)]
mod tests {
    use super::*;
    use bro_transcript::{MessageRole, ParsedEvent, ToolCallInfo, ToolCallKind};
    use serde_json::json;

    fn tool_event(
        session_id: &str,
        cwd: &Path,
        kind: ToolCallKind,
        name: &str,
        input: serde_json::Value,
    ) -> ParsedEvent {
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
                kind,
                name: name.into(),
                tool_use_id: Some("tool-1".into()),
                input,
            }),
        }
    }

    fn bash_event(session_id: &str, cwd: &Path) -> ParsedEvent {
        tool_event(
            session_id,
            cwd,
            ToolCallKind::Bash,
            "Bash",
            json!({"command": "cargo check"}),
        )
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
        let event = bash_event("sess-1", Path::new("/tmp"));

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
    fn explicit_local_root_resolves_bash_edges_without_a_record() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let ctx = ToolEdgeContext::with_project_access(
            vec![ToolEdgeProjectAccess::local("project-1", root.clone())],
            root.join("edges"),
            true,
        );
        let event = bash_event("sess-1", &root);

        let edge = ctx
            .build_event_edges(&event, "claude", 10, 0)
            .unwrap()
            .expect("authorized local root resolves the bash call");
        assert_eq!(edge.kind, "RAN_BASH");
        assert_eq!(edge.metadata["command"], "cargo check");
        assert_eq!(edge.metadata["cwd"], root.to_string_lossy());
        assert!(
            edge.metadata.keys().all(|key| !key.starts_with("anchor.")),
            "bash edges carry no file anchor metadata: {:?}",
            edge.metadata
        );

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
            "an attributable event is not a diagnostic"
        );
    }

    /// Read, Write and Edit tool calls produce no observed edge and are not
    /// counted as unresolvable, whether or not the file lies under an
    /// authorized root.
    #[test]
    fn file_tool_calls_emit_no_edges() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let source = root.join("src.rs");
        fs::write(&source, "pub fn visible() {}\n").unwrap();
        let ctx = ToolEdgeContext::with_project_access(
            vec![ToolEdgeProjectAccess::local("project-1", root.clone())],
            root.join("edges"),
            true,
        );

        for (kind, name, input) in [
            (ToolCallKind::Read, "Read", json!({"file_path": source})),
            (
                ToolCallKind::Write,
                "Write",
                json!({"file_path": source, "content": "x"}),
            ),
            (
                ToolCallKind::Edit,
                "Edit",
                json!({"file_path": source, "old_string": "visible", "new_string": "hidden"}),
            ),
        ] {
            let event = tool_event("sess-1", &root, kind, name, input);
            assert!(
                ctx.build_event_edges(&event, "claude", 10, 0)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(ctx.emit_event_edges(&event, "claude", 10, 0).unwrap(), 0);
        }
        assert!(ctx.take_publish_bundle().is_empty());
        assert!(ctx.path_diagnostics().is_empty());
        assert!(!root.join("edges").exists());
    }

    #[test]
    fn collected_transcript_root_attributes_bash_calls_without_reading_the_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let transcript_root = root.join("checkout-that-does-not-exist");
        let ctx = ToolEdgeContext::with_project_access(
            vec![ToolEdgeProjectAccess::collected(
                "project-1",
                transcript_root.clone(),
            )],
            root.join("edges"),
            true,
        );
        let event = bash_event("sess-collected", &transcript_root.join("crates"));

        let edge = ctx
            .build_event_edges(&event, "claude", 10, 0)
            .unwrap()
            .expect("the collected transcript namespace attributes the bash call");
        assert_eq!(edge.kind, "RAN_BASH");
        assert_eq!(
            ctx.base_project_id_for_cwd(transcript_root.to_str().unwrap()),
            Some("project-1".into())
        );
        assert!(
            !transcript_root.exists(),
            "collected attribution must not materialize or read the checkout"
        );
    }

    /// A remote-only project contributes no local root, so its transcript
    /// events are unattributable. They must be counted and dropped, and in
    /// particular must NOT be re-identified against the one project that
    /// does have a root in this pass.
    #[test]
    fn unresolved_events_are_diagnosed_and_never_reidentified() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let attached = root.join("attached");
        let remote = root.join("remote");
        fs::create_dir_all(&attached).unwrap();
        fs::create_dir_all(&remote).unwrap();

        let ctx = ToolEdgeContext::with_project_access(
            vec![ToolEdgeProjectAccess::local(
                "attached-project",
                attached.clone(),
            )],
            root.join("edges"),
            true,
        );
        let event = bash_event("sess-remote", &remote);

        assert!(
            ctx.build_event_edges(&event, "claude", 10, 0)
                .unwrap()
                .is_none()
        );
        assert_eq!(ctx.emit_event_edges(&event, "claude", 10, 0).unwrap(), 0);
        assert!(
            ctx.take_publish_bundle().is_empty(),
            "an unattributable event must not be re-identified onto the attached project"
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
    fn unresolvable_diagnostic_sample_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let outside = root.join("outside");
        fs::create_dir_all(&outside).unwrap();
        let ctx = ToolEdgeContext::with_project_access(Vec::new(), root.join("edges"), true);

        for index in 0..(MAX_UNRESOLVABLE_SAMPLES * 3) {
            let event = bash_event(&format!("sess-{index}"), &outside);
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
    fn nested_cwd_resolves_to_the_most_specific_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let nested = root.join("crates").join("inner");
        fs::create_dir_all(&nested).unwrap();
        let ctx = ToolEdgeContext::with_project_access(
            vec![
                ToolEdgeProjectAccess::local("outer", root.clone()),
                ToolEdgeProjectAccess::local("inner", nested.clone()),
            ],
            root.join("edges"),
            true,
        );

        assert_eq!(
            ctx.base_project_id_for_cwd(nested.to_str().unwrap()),
            Some("inner".into())
        );
        assert_eq!(
            ctx.base_project_id_for_cwd(root.join("crates").to_str().unwrap()),
            Some("outer".into())
        );
    }
}
