use std::sync::OnceLock;

use anyhow::Result;
use parking_lot::RwLock;

use crate::chunker::Chunk;
use crate::embed::queue::{EmbedQueueHandle, EmbedRequest, EmbedStatusResponse};
use crate::embed::{Bucket, queue};
use crate::entity_ref::EntityRef;
use crate::knowledge::KnowledgeEntry;

static GLOBAL_QUEUE: OnceLock<RwLock<Option<EmbedQueueHandle>>> = OnceLock::new();

fn queue_slot() -> &'static RwLock<Option<EmbedQueueHandle>> {
    GLOBAL_QUEUE.get_or_init(|| RwLock::new(None))
}

pub(crate) fn install(handle: EmbedQueueHandle) {
    *queue_slot().write() = Some(handle);
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

pub(crate) fn project_file_entity_id(chunk: &Chunk) -> String {
    EntityRef::ProjectFile {
        project_id: chunk.project_id.clone(),
        rel_path_hash: chunk.rel_path_hash.clone(),
        chunk_hash: chunk.chunk_hash.clone(),
        occurrence_idx: chunk.occurrence_idx,
    }
    .to_string()
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
