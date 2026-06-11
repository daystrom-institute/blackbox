use std::sync::OnceLock;

use anyhow::Result;
use parking_lot::RwLock;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::chunker::Chunk;
use crate::embed::queue::{EmbedQueueHandle, EmbedRequest, EmbedStatusResponse};
use crate::embed::{Bucket, queue};
use crate::entity_ref::EntityRef;
use crate::knowledge::KnowledgeEntry;
use crate::notes::Note;
use crate::notes::NoteParams;
use crate::orchestration::agents::types::{
    AgentEmbedding, AgentEmbeddingComponents, AgentManifest, AgentRef,
};
use crate::routing::RoutingVerdict;
use crate::server::dispatch::dispatch_routing_verdict_direct;
use crate::server::state::SharedState;
use crate::threads::Thread;

static GLOBAL_QUEUE: OnceLock<RwLock<Option<EmbedQueueHandle>>> = OnceLock::new();
static CONTRADICTION_STATE: OnceLock<RwLock<Option<std::sync::Arc<SharedState>>>> = OnceLock::new();
static CONTRADICTION_THRESHOLD: OnceLock<RwLock<f32>> = OnceLock::new();
const DEFAULT_TIER0_COSINE_THRESHOLD: f32 = 0.85;
const THREAD_EMBED_TEXT_MAX_BYTES: usize = 24 * 1024;

fn queue_slot() -> &'static RwLock<Option<EmbedQueueHandle>> {
    GLOBAL_QUEUE.get_or_init(|| RwLock::new(None))
}

pub(crate) fn install(handle: EmbedQueueHandle) {
    *queue_slot().write() = Some(handle);
}

pub(crate) fn shutdown() {
    if let Some(handle) = queue_slot().read().clone() {
        handle.shutdown();
    }
}

pub(crate) fn install_contradiction_state(state: std::sync::Arc<SharedState>) {
    *CONTRADICTION_STATE
        .get_or_init(|| RwLock::new(None))
        .write() = Some(state);
}

pub(crate) fn install_contradiction_threshold(threshold: f32) {
    *CONTRADICTION_THRESHOLD
        .get_or_init(|| RwLock::new(DEFAULT_TIER0_COSINE_THRESHOLD))
        .write() = threshold.clamp(0.0, 1.0);
}

fn contradiction_threshold() -> f32 {
    *CONTRADICTION_THRESHOLD
        .get_or_init(|| RwLock::new(DEFAULT_TIER0_COSINE_THRESHOLD))
        .read()
}

pub(crate) fn status_response() -> EmbedStatusResponse {
    queue_slot()
        .read()
        .as_ref()
        .map(EmbedQueueHandle::status)
        .unwrap_or_else(|| EmbedStatusResponse {
            routes: Default::default(),
        })
}

pub(crate) fn status_response_for_buckets(
    state: &SharedState,
    buckets: &[Bucket],
) -> Result<EmbedStatusResponse> {
    let mut response = status_response();
    let coverage = crate::embed::route_coverage(state, buckets)?;
    for (route, counts) in coverage {
        let status = response.routes.entry(route).or_default();
        status.session_indexed_count = Some(status.indexed_count);
        status.indexed_count = counts.indexed_count;
        status.source_count = Some(counts.source_count);
        status.coverage_ratio = if counts.source_count == 0 {
            None
        } else {
            Some(counts.indexed_count as f32 / counts.source_count as f32)
        };
    }
    queue::normalize_route_statuses(&mut response);
    Ok(response)
}

pub(crate) fn status_json_for_state(state: &SharedState) -> Result<String> {
    const STATUS_COVERAGE_BUCKETS: &[Bucket] = &[
        Bucket::Knowledge,
        Bucket::Code,
        Bucket::Docs,
        Bucket::GitMessage,
        Bucket::Notes,
        Bucket::Threads,
        Bucket::AgentManifest,
    ];
    Ok(serde_json::to_string_pretty(&status_response_for_buckets(
        state,
        STATUS_COVERAGE_BUCKETS,
    )?)?)
}

pub(crate) fn enqueue_knowledge(entry: &KnowledgeEntry, entity_id: &str, chunk_hash: &str) {
    enqueue(EmbedRequest {
        bucket: Bucket::Knowledge,
        project_id: None,
        entity_id: entity_id.to_string(),
        chunk_hash: chunk_hash.to_string(),
        text: format!("{}\n\n{}", entry.title, entry.content),
    });
}

pub(crate) fn tombstone_knowledge(entity_id: &str) {
    if let Some(queue) = queue_slot().read().as_ref() {
        queue.tombstone(entity_id);
    } else {
        tracing::debug!(
            route = "knowledge",
            entity_id,
            "embedding queue not installed; accepted tombstone as no-op"
        );
    }
}

pub(crate) fn enqueue_roadmap(
    item: &crate::roadmap::RoadmapItem,
    entity_id: &str,
    chunk_hash: &str,
) {
    enqueue(EmbedRequest {
        bucket: Bucket::Knowledge, // reuses knowledge bucket for vector search
        project_id: None,
        entity_id: entity_id.to_string(),
        chunk_hash: chunk_hash.to_string(),
        text: format!("{}\n\n{}", item.title, item.body),
    });
}

pub(crate) fn tombstone_roadmap(entity_id: &str) {
    if let Some(queue) = queue_slot().read().as_ref() {
        queue.tombstone(entity_id);
    } else {
        tracing::debug!(
            entity_id,
            "embedding queue not installed; accepted roadmap tombstone as no-op"
        );
    }
}

/// Register this module's enqueue functions as the index engine's embed
/// hooks (`index::embed_hook`). Called from the daemon's writer-actor spawn
/// path; idempotent.
pub(crate) fn register_index_embed_hooks() {
    crate::index::embed_hook::register_embed_hooks(crate::index::embed_hook::EmbedHooks {
        project_file: enqueue_project_file,
        git_message: enqueue_git_message,
    });
}

pub(crate) fn enqueue_project_file(chunk: &Chunk, entity_id: &str) {
    let bucket = if chunk.language.is_some() || chunk.chunk_kind == "code_block" {
        Bucket::Code
    } else {
        Bucket::Docs
    };
    enqueue(EmbedRequest {
        bucket,
        project_id: Some(chunk.project_id.clone()),
        entity_id: entity_id.to_string(),
        chunk_hash: chunk.chunk_hash.clone(),
        text: chunk.content.clone(),
    });
}

pub(crate) fn enqueue_git_message(entity_id: &str, chunk_hash: &str, message: &str) {
    enqueue(EmbedRequest {
        bucket: Bucket::GitMessage,
        project_id: None,
        entity_id: entity_id.to_string(),
        chunk_hash: chunk_hash.to_string(),
        text: message.to_string(),
    });
}

pub(crate) fn enqueue_note(note: &Note) {
    let entity_id = EntityRef::Note {
        note_id: note.id.clone(),
    }
    .to_string();
    enqueue(EmbedRequest {
        bucket: Bucket::Notes,
        project_id: None,
        entity_id,
        chunk_hash: note_chunk_hash(note),
        text: note_text(note),
    });
}

pub(crate) fn enqueue_thread(thread: &Thread) {
    let entity_id = EntityRef::Thread {
        thread_id: thread.id.clone(),
    }
    .to_string();
    enqueue(EmbedRequest {
        bucket: Bucket::Threads,
        project_id: None,
        entity_id,
        chunk_hash: thread_chunk_hash(thread),
        text: thread_text(thread),
    });
}

pub(crate) fn agent_manifest_embedding(
    agent: &AgentRef,
    manifest: &AgentManifest,
) -> AgentEmbedding {
    let route = crate::embed::EmbeddingRouter::load_default()
        .and_then(|router| router.route(Bucket::AgentManifest, None))
        .ok();
    let model = route
        .as_ref()
        .map(|route| route.model.clone())
        .unwrap_or_else(|| "unavailable".into());
    let vector_route = route.map(|route| route.vector_route_id());
    let primary = agent_component_entity_id(agent, AgentManifestComponent::Primary);
    let when_to_use = if manifest.when_to_use.is_empty() {
        None
    } else {
        Some(agent_component_entity_id(
            agent,
            AgentManifestComponent::WhenToUse,
        ))
    };
    let anti_patterns = if manifest.anti_patterns.is_empty() {
        None
    } else {
        Some(agent_component_entity_id(
            agent,
            AgentManifestComponent::AntiPatterns,
        ))
    };
    AgentEmbedding {
        model,
        computed_at: crate::util::now_iso(),
        vector_ref: primary.clone(),
        vector_route,
        components: AgentEmbeddingComponents {
            primary,
            when_to_use,
            anti_patterns,
        },
    }
}

pub(crate) fn enqueue_agent_manifest(agent: &AgentRef, manifest: &AgentManifest) {
    for component in agent_manifest_components(manifest) {
        enqueue(EmbedRequest {
            bucket: Bucket::AgentManifest,
            project_id: None,
            entity_id: agent_component_entity_id(agent, component.kind),
            chunk_hash: content_hash(&component.text),
            text: component.text,
        });
    }
}

pub(crate) fn agent_manifest_component_count(manifest: &AgentManifest) -> usize {
    agent_manifest_components(manifest).len()
}

pub(crate) fn agent_component_entity_id(
    agent: &AgentRef,
    component: AgentManifestComponent,
) -> String {
    format!(
        "agent_embed:{}:v{}:{}",
        agent.name,
        agent.version,
        component.as_str()
    )
}

pub(crate) fn parse_agent_component_entity_id(
    entity_id: &str,
) -> Option<(AgentRef, AgentManifestComponent)> {
    let rest = entity_id.strip_prefix("agent_embed:")?;
    let (agent_part, component_part) = rest.rsplit_once(':')?;
    let (name, version_part) = agent_part.rsplit_once(":v")?;
    let version = version_part.parse::<u32>().ok()?;
    Some((
        AgentRef {
            name: name.to_string(),
            version,
        },
        AgentManifestComponent::parse(component_part)?,
    ))
}

pub(crate) fn agent_component_hash(
    manifest: &AgentManifest,
    component: AgentManifestComponent,
) -> Option<String> {
    let text = agent_manifest_component_text(manifest, component)?;
    Some(content_hash(&text))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum AgentManifestComponent {
    Primary,
    WhenToUse,
    AntiPatterns,
}

impl AgentManifestComponent {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::WhenToUse => "when_to_use",
            Self::AntiPatterns => "anti_patterns",
        }
    }

    fn parse(input: &str) -> Option<Self> {
        match input {
            "primary" => Some(Self::Primary),
            "when_to_use" => Some(Self::WhenToUse),
            "anti_patterns" => Some(Self::AntiPatterns),
            _ => None,
        }
    }
}

struct AgentComponentText {
    kind: AgentManifestComponent,
    text: String,
}

fn agent_manifest_components(manifest: &AgentManifest) -> Vec<AgentComponentText> {
    let mut components = Vec::new();
    for kind in [
        AgentManifestComponent::Primary,
        AgentManifestComponent::WhenToUse,
        AgentManifestComponent::AntiPatterns,
    ] {
        if let Some(text) = agent_manifest_component_text(manifest, kind) {
            components.push(AgentComponentText { kind, text });
        }
    }
    components
}

fn agent_manifest_component_text(
    manifest: &AgentManifest,
    component: AgentManifestComponent,
) -> Option<String> {
    match component {
        AgentManifestComponent::Primary => Some(format!(
            "description: {}\nwhen_to_use:\n{}\nanti_patterns:\n{}",
            manifest.description,
            manifest.when_to_use.join("\n"),
            manifest.anti_patterns.join("\n")
        )),
        AgentManifestComponent::WhenToUse => {
            if manifest.when_to_use.is_empty() {
                None
            } else {
                Some(manifest.when_to_use.join("\n"))
            }
        }
        AgentManifestComponent::AntiPatterns => {
            if manifest.anti_patterns.is_empty() {
                None
            } else {
                Some(manifest.anti_patterns.join("\n"))
            }
        }
    }
}

pub(crate) fn enqueue_transcript(
    provider: &str,
    session_id: &str,
    byte_offset: u64,
    content: &str,
    chunk_hash: &str,
) {
    let entity_id = EntityRef::Transcript {
        provider: provider.to_string(),
        session_id: session_id.to_string(),
        line_offset: byte_offset,
        event_idx: 0,
    }
    .to_string();
    enqueue(EmbedRequest {
        bucket: Bucket::Transcripts,
        project_id: None,
        entity_id,
        chunk_hash: chunk_hash.to_string(),
        text: content.to_string(),
    });
}

// Entity-id construction for project-file chunks lives with the index
// engine (`index::embed_hook`); re-exported here for the embed-side callers.
pub(crate) use crate::index::embed_hook::project_file_entity_id;

pub(crate) fn content_hash(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub(crate) fn note_chunk_hash(note: &Note) -> String {
    let mut hasher = Sha256::new();
    hasher.update(note.id.as_bytes());
    hasher.update([0]);
    hasher.update(note.kind.as_ref().as_bytes());
    hasher.update([0]);
    hasher.update(note.body.as_bytes());
    hasher.update([0]);
    hasher.update(note.updated_at.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn note_text(note: &Note) -> String {
    let mut fields = vec![format!("kind: {}", note.kind.as_ref()), note.body.clone()];
    if let Some(project) = &note.project {
        fields.push(format!("project: {project}"));
    }
    if let Some(task_id) = &note.task_id {
        fields.push(format!("task: {task_id}"));
    }
    if let Some(thread_id) = &note.thread_id {
        fields.push(format!("thread: {thread_id}"));
    }
    if let Some(bro) = &note.bro {
        fields.push(format!("bro: {bro}"));
    }
    fields.join("\n")
}

pub(crate) fn thread_chunk_hash(thread: &Thread) -> String {
    let mut hasher = Sha256::new();
    hash_field(&mut hasher, "thread-v1");
    hash_field(&mut hasher, &thread.id);
    hash_field(&mut hasher, thread.name.as_deref().unwrap_or(""));
    hash_field(&mut hasher, &thread.topic);
    hash_field(&mut hasher, &thread.project);
    hash_field(&mut hasher, thread.status.as_ref());
    if let Some(kind) = thread.kind {
        hash_field(&mut hasher, kind.as_ref());
    } else {
        hash_field(&mut hasher, "");
    }
    hash_field(&mut hasher, thread.handoff_doc.as_deref().unwrap_or(""));
    hash_field(&mut hasher, &thread.notes.len().to_string());
    for note in &thread.notes {
        hash_field(&mut hasher, note);
    }
    hash_field(&mut hasher, &thread.sessions.len().to_string());
    for session in &thread.sessions {
        hash_field(&mut hasher, &session.session_id);
        hash_field(&mut hasher, &session.provider);
        hash_field(&mut hasher, session.name.as_deref().unwrap_or(""));
    }
    hash_field(&mut hasher, &thread.edges.len().to_string());
    for edge in &thread.edges {
        hash_field(&mut hasher, edge.kind.as_ref());
        hash_field(&mut hasher, edge.target_type.as_ref());
        hash_field(&mut hasher, &edge.target);
        hash_field(&mut hasher, edge.note.as_deref().unwrap_or(""));
    }
    format!("{:x}", hasher.finalize())
}

fn hash_field(hasher: &mut Sha256, value: &str) {
    hasher.update(value.len().to_le_bytes());
    hasher.update([0]);
    hasher.update(value.as_bytes());
    hasher.update([0]);
}

fn thread_text(thread: &Thread) -> String {
    let mut fields = vec![
        "entity: thread".to_string(),
        format!("thread_id: {}", thread.id),
        format!("topic: {}", thread.topic),
        format!("project: {}", thread.project),
        format!("status: {}", thread.status.as_ref()),
    ];
    if let Some(name) = &thread.name {
        fields.push(format!("name: {name}"));
    }
    if let Some(kind) = thread.kind {
        fields.push(format!("kind: {}", kind.as_ref()));
    }
    if let Some(doc) = thread.handoff_doc.as_deref().filter(|doc| !doc.is_empty()) {
        fields.push(format!("handoff_doc:\n{doc}"));
    }
    if !thread.notes.is_empty() {
        fields.push("inline_notes:".to_string());
        for (idx, note) in thread.notes.iter().enumerate() {
            fields.push(format!("note {}:\n{}", idx + 1, note));
        }
    }
    if !thread.sessions.is_empty() {
        fields.push("sessions:".to_string());
        for session in &thread.sessions {
            let mut line = format!(
                "- provider: {}; session_id: {}",
                session.provider, session.session_id
            );
            if let Some(name) = &session.name {
                line.push_str(&format!("; name: {name}"));
            }
            fields.push(line);
        }
    }
    if !thread.edges.is_empty() {
        fields.push("edges:".to_string());
        for edge in &thread.edges {
            let mut line = format!(
                "- kind: {}; target_type: {}; target: {}",
                edge.kind.as_ref(),
                edge.target_type.as_ref(),
                edge.target
            );
            if let Some(note) = &edge.note {
                line.push_str(&format!("; note: {note}"));
            }
            fields.push(line);
        }
    }
    truncate_middle(&fields.join("\n"), THREAD_EMBED_TEXT_MAX_BYTES)
}

fn truncate_middle(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    const MARKER: &str = "\n...[thread text truncated]...\n";
    if max_bytes <= MARKER.len() {
        return text[..floor_char_boundary(text, max_bytes)].to_string();
    }
    let available = max_bytes - MARKER.len();
    let head_len = floor_char_boundary(text, available / 2);
    let tail_start = ceil_char_boundary(text, text.len().saturating_sub(available - head_len));
    format!("{}{}{}", &text[..head_len], MARKER, &text[tail_start..])
}

fn floor_char_boundary(text: &str, mut idx: usize) -> usize {
    idx = idx.min(text.len());
    while idx > 0 && !text.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn ceil_char_boundary(text: &str, mut idx: usize) -> usize {
    idx = idx.min(text.len());
    while idx < text.len() && !text.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

fn enqueue(request: queue::EmbedRequest) {
    let route = request.bucket.as_str();
    let entity_id = request.entity_id.clone();
    let chunk_hash = request.chunk_hash.clone();
    if let Some(queue) = queue_slot().read().as_ref() {
        if !queue.enqueue(request) {
            tracing::debug!(
                route,
                entity_id,
                chunk_hash,
                "embedding enqueue skipped or route unavailable"
            );
        }
    } else {
        tracing::debug!(
            route,
            entity_id,
            chunk_hash,
            "embedding queue not installed; accepted enqueue as no-op"
        );
    }
}

pub(crate) fn maybe_detect_knowledge_contradiction(
    request: &EmbedRequest,
    vector_route: &str,
    vector: &[f32],
) {
    if request.bucket != Bucket::Knowledge {
        return;
    }
    let Some(state) = CONTRADICTION_STATE
        .get_or_init(|| RwLock::new(None))
        .read()
        .clone()
    else {
        return;
    };
    let Some(entry_a) = request.entity_id.strip_prefix("knowledge:") else {
        return;
    };
    let hits = match crate::vectors::search(vector_route, vector, 5) {
        Ok(hits) => hits,
        Err(err) => {
            tracing::debug!(error = %err, "knowledge contradiction nearest-neighbor scan failed");
            return;
        }
    };
    let kb = state.kb.read();
    let Some(source) = kb.entry(entry_a).cloned() else {
        return;
    };
    let threshold = contradiction_threshold();
    let Some((entry_b, cosine)) = hits.into_iter().find_map(|hit| {
        let cosine = 1.0 - hit.distance;
        if hit.id == request.entity_id || cosine < threshold {
            return None;
        }
        let id = hit.id.strip_prefix("knowledge:")?;
        let target = kb.entry(id)?.clone();
        if supersession_related(&source, &target) {
            return None;
        }
        Some((target, cosine))
    }) else {
        return;
    };
    drop(kb);

    let payload = json!({
        "entry_a": format!("knowledge:{}", source.id),
        "entry_b": format!("knowledge:{}", entry_b.id),
        "cosine": cosine,
        "vector_route": vector_route,
    });
    if state
        .workflow_registry
        .read()
        .contains_key("contradiction-review-arc")
    {
        let state_for_task = state.clone();
        let mut initial_vars = serde_json::Map::new();
        initial_vars.insert("entry_a".into(), json!(format!("knowledge:{}", source.id)));
        initial_vars.insert("entry_b".into(), json!(format!("knowledge:{}", entry_b.id)));
        initial_vars.insert("cosine".into(), json!(cosine));
        tokio::spawn(async move {
            let _ = dispatch_routing_verdict_direct(
                state_for_task,
                "contradiction-detected",
                RoutingVerdict::StartArc {
                    workflow: "contradiction-review-arc".into(),
                    initial_vars,
                },
                payload,
            )
            .await;
        });
    } else {
        let project = source.project.clone().or(entry_b.project.clone());
        let body = format!(
            "Tier-0 contradiction detected between knowledge:{} and knowledge:{} (cosine {:.3}), but contradiction-review-arc is not installed.",
            source.id, entry_b.id, cosine
        );
        if let Err(err) = state.notes.write().create(&NoteParams {
            kind: "surprise".into(),
            body,
            task_id: None,
            session_id: None,
            project,
            thread_id: None,
            provider: None,
            bro: None,
        }) {
            tracing::warn!(error = %err, "failed to surface contradiction fallback note");
        }
    }
}

fn supersession_related(a: &KnowledgeEntry, b: &KnowledgeEntry) -> bool {
    a.supersedes.as_deref() == Some(b.id.as_str()) || b.supersedes.as_deref() == Some(a.id.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::embed::{Bucket, EmbeddingRouter};
    use crate::entity_ref::EntityRef;
    use crate::server::state::SharedState;
    use crate::threads::{
        EdgeKind, EdgeTarget, SessionLink, Thread, ThreadEdge, ThreadKind, ThreadParams,
        ThreadStatus,
    };
    use crate::vectors::{VectorStore, install_test_global};

    fn sample_thread() -> Thread {
        Thread {
            id: "thread-1234abcd".into(),
            name: Some("thread embeddings".into()),
            topic: "embed inline thread notes".into(),
            project: "/repo/blackbox".into(),
            record_dir: None,
            status: ThreadStatus::Active,
            kind: Some(ThreadKind::WorkItem),
            origin: None,
            sessions: vec![SessionLink {
                session_id: "session-1".into(),
                provider: "claude".into(),
                name: Some("planner".into()),
                linked_at: "2026-05-06T00:00:00Z".into(),
            }],
            handoff_doc: Some("handoff body".into()),
            notes: vec!["first note".into()],
            edges: vec![ThreadEdge {
                kind: EdgeKind::RelatesTo,
                target: "thread-deadbeef".into(),
                target_type: EdgeTarget::Thread,
                note: Some("edge note".into()),
                created_at: "2026-05-06T00:00:00Z".into(),
            }],
            promoted_to: None,
            created_at: "2026-05-06T00:00:00Z".into(),
            last_activity: "2026-05-06T00:00:00Z".into(),
            resolved_at: None,
        }
    }

    #[test]
    fn thread_hash_ignores_activity_timestamp() {
        let a = sample_thread();
        let mut b = a.clone();
        b.last_activity = "2026-05-06T01:00:00Z".into();
        assert_eq!(thread_chunk_hash(&a), thread_chunk_hash(&b));
    }

    #[test]
    fn thread_hash_changes_when_inline_note_changes() {
        let a = sample_thread();
        let mut b = a.clone();
        b.notes.push("second note".into());
        assert_ne!(thread_chunk_hash(&a), thread_chunk_hash(&b));
    }

    #[test]
    fn thread_text_is_capped_and_keeps_head_and_tail() {
        let mut thread = sample_thread();
        thread.handoff_doc = Some(format!("start {}", "x".repeat(40_000)));
        thread.notes.push("tail marker".into());
        let text = thread_text(&thread);
        assert!(text.len() <= THREAD_EMBED_TEXT_MAX_BYTES);
        assert!(text.contains("thread_id: thread-1234abcd"));
        assert!(text.contains("thread text truncated"));
        assert!(text.contains("tail marker"));
    }
    #[test]
    fn embed_status_reports_thread_coverage_from_vector_store() {
        let tmp = tempfile::tempdir().unwrap();
        let state = SharedState::for_test(tmp.path());
        let vector_tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(VectorStore::open(vector_tmp.path()).unwrap());
        let _guard = install_test_global(store.clone());
        let created = state
            .threads
            .write()
            .thread(&ThreadParams {
                action: "open".into(),
                name: Some("coverage-thread".into()),
                id: None,
                topic: Some("status coverage thread".into()),
                project: Some("/repo".into()),
                session_id: None,
                provider: None,
                session_name: None,
                handoff_doc: Some("handoff marker".into()),
                note: Some("note marker".into()),
                target: None,
                target_type: None,
                edge: None,
                promoted_to: None,
                kind: Some("investigation".into()),
                origin: None,
            })
            .unwrap();
        let thread_id = regex::Regex::new(r"thread-[0-9a-f]{8}")
            .unwrap()
            .find(&created)
            .unwrap()
            .as_str()
            .to_string();
        let thread = state
            .threads
            .read()
            .all()
            .iter()
            .find(|thread| thread.id == thread_id)
            .unwrap()
            .clone();
        let route = EmbeddingRouter::default()
            .route(Bucket::Threads, None)
            .unwrap()
            .vector_route_id();
        let entity_id = EntityRef::Thread { thread_id }.to_string();
        store
            .upsert(
                &route,
                &entity_id,
                &thread_chunk_hash(&thread),
                vec![1.0, 0.0],
            )
            .unwrap();

        let status = status_response_for_buckets(&state, &[Bucket::Threads]).unwrap();
        let threads = status.routes.get("threads").unwrap();
        assert_eq!(threads.source_count, Some(1));
        assert_eq!(threads.indexed_count, 1);
        assert_eq!(threads.coverage_ratio, Some(1.0));
    }
}
