use std::sync::OnceLock;

use anyhow::Result;
use parking_lot::RwLock;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::chunker::Chunk;
use crate::embed::queue::{EmbedQueueHandle, EmbedRequest, EmbedStatusResponse};
use crate::embed::{queue, Bucket};
use crate::entity_ref::EntityRef;
use crate::knowledge::KnowledgeEntry;
use crate::notes::Note;
use crate::notes::NoteParams;
use crate::routing::RoutingVerdict;
use crate::SharedState;

static GLOBAL_QUEUE: OnceLock<RwLock<Option<EmbedQueueHandle>>> = OnceLock::new();
static CONTRADICTION_STATE: OnceLock<RwLock<Option<std::sync::Arc<SharedState>>>> =
    OnceLock::new();
static CONTRADICTION_THRESHOLD: OnceLock<RwLock<f32>> = OnceLock::new();
const DEFAULT_TIER0_COSINE_THRESHOLD: f32 = 0.85;

fn queue_slot() -> &'static RwLock<Option<EmbedQueueHandle>> {
    GLOBAL_QUEUE.get_or_init(|| RwLock::new(None))
}

pub(crate) fn install(handle: EmbedQueueHandle) {
    *queue_slot().write() = Some(handle);
}

pub(crate) fn install_contradiction_state(state: std::sync::Arc<SharedState>) {
    *CONTRADICTION_STATE.get_or_init(|| RwLock::new(None)).write() = Some(state);
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

pub(crate) fn status_json() -> Result<String> {
    Ok(serde_json::to_string_pretty(&status_response())?)
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

pub(crate) fn project_file_entity_id(chunk: &Chunk) -> String {
    EntityRef::ProjectFile {
        project_id: chunk.project_id.clone(),
        rel_path_hash: chunk.rel_path_hash.clone(),
        chunk_hash: chunk.chunk_hash.clone(),
        occurrence_idx: chunk.occurrence_idx,
    }
    .to_string()
}

pub(crate) fn content_hash(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn note_chunk_hash(note: &Note) -> String {
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
            let _ = crate::dispatch_routing_verdict_direct(
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
    a.supersedes.as_deref() == Some(b.id.as_str())
        || b.supersedes.as_deref() == Some(a.id.as_str())
}
