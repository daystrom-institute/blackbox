use std::sync::OnceLock;

use parking_lot::RwLock;
use sha2::{Digest, Sha256};

use crate::embed::queue::{EmbedQueueHandle, EmbedRequest, EmbedStatusResponse};
use crate::embed::{Bucket, queue};
use bbox_chunker::Chunk;
use bbox_corpus_core::entity_ref::EntityRef;
use bbox_knowledge::knowledge::KnowledgeEntry;
use bbox_threads::notes::Note;
use bbox_threads::threads::Thread;

static GLOBAL_QUEUE: OnceLock<RwLock<Option<EmbedQueueHandle>>> = OnceLock::new();
const THREAD_EMBED_TEXT_MAX_BYTES: usize = 24 * 1024;

fn queue_slot() -> &'static RwLock<Option<EmbedQueueHandle>> {
    GLOBAL_QUEUE.get_or_init(|| RwLock::new(None))
}

pub fn install(handle: EmbedQueueHandle) {
    *queue_slot().write() = Some(handle);
}

pub fn shutdown() {
    if let Some(handle) = queue_slot().read().clone() {
        handle.shutdown();
    }
}

pub fn status_response() -> EmbedStatusResponse {
    queue_slot()
        .read()
        .as_ref()
        .map(EmbedQueueHandle::status)
        .unwrap_or_else(|| EmbedStatusResponse {
            routes: Default::default(),
        })
}

pub fn enqueue_knowledge(entry: &KnowledgeEntry, entity_id: &str, chunk_hash: &str) -> bool {
    enqueue(EmbedRequest {
        bucket: Bucket::Knowledge,
        project_id: None,
        entity_id: entity_id.to_string(),
        chunk_hash: chunk_hash.to_string(),
        text: format!("{}\n\n{}", entry.title, entry.content),
    })
}

pub fn tombstone_knowledge(entity_id: &str) {
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

pub fn enqueue_roadmap(
    item: &bbox_stores::roadmap::RoadmapItem,
    entity_id: &str,
    chunk_hash: &str,
) -> bool {
    enqueue(EmbedRequest {
        bucket: Bucket::Knowledge, // reuses knowledge bucket for vector search
        project_id: None,
        entity_id: entity_id.to_string(),
        chunk_hash: chunk_hash.to_string(),
        text: format!("{}\n\n{}", item.title, item.body),
    })
}

pub fn tombstone_roadmap(entity_id: &str) {
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
pub fn register_index_embed_hooks() {
    bbox_indexing::index::embed_hook::register_embed_hooks(
        bbox_indexing::index::embed_hook::EmbedHooks {
            project_file: enqueue_project_file_hook,
            git_message: enqueue_git_message_hook,
        },
    );
}

fn enqueue_project_file_hook(chunk: &Chunk, entity_id: &str) {
    let _ = enqueue_project_file(chunk, entity_id);
}

fn enqueue_git_message_hook(entity_id: &str, chunk_hash: &str, message: &str) {
    let _ = enqueue_git_message(entity_id, chunk_hash, message);
}

pub fn enqueue_project_file(chunk: &Chunk, entity_id: &str) -> bool {
    let bucket = if chunk.language.is_some() || chunk.chunk_kind == "code_block" {
        Bucket::Code
    } else {
        Bucket::Docs
    };
    enqueue_project_file_as(chunk, entity_id, bucket)
}

pub fn enqueue_project_file_as(chunk: &Chunk, entity_id: &str, bucket: Bucket) -> bool {
    enqueue(EmbedRequest {
        bucket,
        project_id: Some(chunk.project_id.clone()),
        entity_id: entity_id.to_string(),
        chunk_hash: chunk.chunk_hash.clone(),
        text: chunk.content.clone(),
    })
}

pub fn enqueue_git_message(entity_id: &str, chunk_hash: &str, message: &str) -> bool {
    enqueue(EmbedRequest {
        bucket: Bucket::GitMessage,
        project_id: None,
        entity_id: entity_id.to_string(),
        chunk_hash: chunk_hash.to_string(),
        text: message.to_string(),
    })
}

pub fn enqueue_note(note: &Note) -> bool {
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
    })
}

pub fn enqueue_note_hook(note: &Note) {
    let _ = enqueue_note(note);
}

pub fn enqueue_thread(thread: &Thread) -> bool {
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
    })
}

pub fn enqueue_thread_hook(thread: &Thread) {
    let _ = enqueue_thread(thread);
}

pub fn enqueue_transcript(
    provider: &str,
    session_id: &str,
    byte_offset: u64,
    content: &str,
    chunk_hash: &str,
) -> bool {
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
    })
}

// Entity-id construction for project-file chunks lives with the index
// engine (`index::embed_hook`); re-exported here for the embed-side callers.
pub use bbox_indexing::index::embed_hook::project_file_entity_id;

/// Parse an `agent_embed:<name>:v<version>:<component>` vector entity id
/// into its plain parts. The agent-typed wrapper lives in the daemon's
/// embed runtime; this layer stays free of orchestration types.
pub fn parse_agent_component_entity_id_parts(entity_id: &str) -> Option<(String, u32, String)> {
    let rest = entity_id.strip_prefix("agent_embed:")?;
    let (agent_part, component_part) = rest.rsplit_once(':')?;
    let (name, version_part) = agent_part.rsplit_once(":v")?;
    let version = version_part.parse::<u32>().ok()?;
    Some((name.to_string(), version, component_part.to_string()))
}

pub fn content_hash(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub fn note_chunk_hash(note: &Note) -> String {
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

pub fn thread_chunk_hash(thread: &Thread) -> String {
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

pub fn enqueue(request: queue::EmbedRequest) -> bool {
    let route = request.bucket.as_str();
    let entity_id = request.entity_id.clone();
    let chunk_hash = request.chunk_hash.clone();
    if let Some(queue) = queue_slot().read().as_ref() {
        let accepted = queue.enqueue(request);
        if !accepted {
            tracing::debug!(
                route,
                entity_id,
                chunk_hash,
                "embedding enqueue skipped or route unavailable"
            );
        }
        accepted
    } else {
        tracing::debug!(
            route,
            entity_id,
            chunk_hash,
            "embedding queue not installed; accepted enqueue as no-op"
        );
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use bbox_threads::threads::{
        EdgeKind, EdgeTarget, SessionLink, Thread, ThreadEdge, ThreadKind, ThreadStatus,
    };

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
}
