use crate::knowledge::KnowledgeEntry;

pub(crate) fn enqueue_knowledge(entry: &KnowledgeEntry, entity_id: &str, chunk_hash: &str) {
    tracing::debug!(
        route = "knowledge",
        entity_id,
        chunk_hash,
        bytes = entry.content.len(),
        "embedding queue unavailable until E2; accepted enqueue stub"
    );
}

pub(crate) fn tombstone_knowledge(entity_id: &str) {
    tracing::debug!(
        route = "knowledge",
        entity_id,
        "embedding queue unavailable until E2; accepted tombstone stub"
    );
}

pub(crate) fn enqueue_git_message(entity_id: &str, chunk_hash: &str, message: &str) {
    tracing::debug!(
        route = "git_message",
        entity_id,
        chunk_hash,
        bytes = message.len(),
        "embedding queue unavailable until E2; accepted enqueue stub"
    );
}
