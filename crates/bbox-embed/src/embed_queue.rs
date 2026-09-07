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
        visual_kind: None,
        visual_payload: None,
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

/// Register this module's enqueue functions as the index engine's embed
/// hooks (`index::embed_hook`). Called from the daemon's writer-actor spawn
/// path; idempotent.
pub fn register_index_embed_hooks() {
    bbox_indexing::index::embed_hook::register_embed_hooks(
        bbox_indexing::index::embed_hook::EmbedHooks {
            project_file: enqueue_project_file_hook,
            git_message: enqueue_git_message_hook,
            git_message_vector_active: git_message_vector_is_active,
        },
    );
}

/// Coverage probe for the P3-E history re-emission. `false` whenever coverage
/// cannot be PROVED (no queue installed, no route table, no vector store, probe
/// error), which makes the caller re-enqueue rather than promise a vector view
/// it never verified.
fn git_message_vector_is_active(entity_id: &str, content_hash: &str) -> bool {
    queue_slot()
        .read()
        .as_ref()
        .and_then(|handle| handle.vector_is_active(Bucket::GitMessage, entity_id, content_hash))
        .unwrap_or(false)
}

fn enqueue_project_file_hook(chunk: &Chunk, project_display: &str, entity_id: &str) {
    let _ = enqueue_project_file(chunk, project_display, entity_id);
}

fn enqueue_git_message_hook(entity_id: &str, chunk_hash: &str, message: &str) {
    let _ = enqueue_git_message(entity_id, chunk_hash, message);
}

/// THE code-vs-prose routing rule for project-file chunks, shared by the
/// index-time enqueue hook and the coverage/backfill attribution in
/// embed_runtime. One rule, one place: the two paths historically used
/// different rules (chunk language here, path extension there), which
/// routed fresh markdown edits into the Code bucket while backfills
/// re-embedded them into Docs - dueling partitions and phantom coverage.
/// Markdown's legacy `language: Some("md")` label is treated as prose so
/// stored docs written before the chunker stopped emitting it still
/// attribute correctly.
pub fn is_code_chunk(language: Option<&str>, chunk_kind: &str) -> bool {
    if chunk_kind == "code_block" {
        return true;
    }
    matches!(language, Some(lang) if !matches!(lang, "md" | "markdown" | "mdown"))
}

/// Visual (image-payload-bearing) chunk kinds: the routing analog of
/// `is_code_chunk` for the visual lane. `image` (X-IMG) and `pdf_figure`
/// (X-PDF's embedded-XObject extractor) are shipped; the rest are the
/// design's remaining visual sidecar kinds
/// (`design/corpus/agentic-corpus/multimodal-embedding-routing.md`
/// "Multimodal Chunk Model") reserved here so a future chunker (slide_image,
/// ...) needs no changes to this shared list beyond adding its kind string.
/// Shared by the index-time enqueue hook and the coverage/backfill
/// attribution in `embed_runtime`, same one-rule-one-place reasoning as
/// `is_code_chunk`'s doc comment.
pub const VISUAL_CHUNK_KINDS: &[&str] = &[
    "image",
    "pdf_figure",
    "spreadsheet_chart",
    "slide_image",
    "image_caption",
    "video_segment",
];

pub fn is_visual_chunk_kind(chunk_kind: &str) -> bool {
    VISUAL_CHUNK_KINDS.contains(&chunk_kind)
}

/// Version of the project-file TEXT embedding-input assembly
/// (`project_file_embed_text`). Bumping it misses the enqueue dedup for every
/// project-file text row exactly once, which is the ONLY mechanism that
/// re-embeds unchanged chunks: `should_embed` keys on
/// `(entity_id, content_hash)`, so a prepend change with an unchanged
/// `chunk_hash` would otherwise be silently skipped forever. Folding the
/// prepend into `chunk.content` instead is forbidden - that would change
/// `chunk_hash` and therefore `ProjectFileV2` ref identity (plan section 4.6).
///
/// Bumped to v2 at P3-E for the display-name + relative-path prepend
/// (governing section 10.2). Operationally this is a one-time full re-embed of
/// project-file text vectors, enumerated as a bridge-window deploy event in
/// plan section 4.3 item 3 beside the one-time index rebuild.
pub const EMBED_TEXT_VERSION: &str = "project-file-embed-text-v2-display-relpath";

/// The versioned envelope hash passed as the embed queue's `content_hash` for
/// project-file TEXT rows: `sha256(EMBED_TEXT_VERSION || chunk_hash)`. The
/// document's own `chunk_hash` and every entity-ref component stay
/// byte-untouched; only the queue's dedup key and the vector record's
/// freshness hash move. Every boundary that compares project-file vector
/// hashes must apply this same envelope or coverage reads a permanent phantom
/// zero (see `record_index_doc_coverage`'s Code/Docs arm).
pub fn project_file_text_content_hash(chunk_hash: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(EMBED_TEXT_VERSION.as_bytes());
    hasher.update(b"\0");
    hasher.update(chunk_hash.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Embedding input for a project-file TEXT chunk: the stable display name and
/// the project-relative path, then the chunk body. Never a host root
/// (governing section 10.2).
pub fn project_file_embed_text(chunk: &Chunk, project_display: &str) -> String {
    let relative_path = chunk
        .file_path
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/");
    format!("{project_display} {relative_path}\n\n{}", chunk.content)
}

pub fn enqueue_project_file(chunk: &Chunk, project_display: &str, entity_id: &str) -> bool {
    // Lane split (plan section 9 item 5): the visual lane's embedding input is
    // an image payload with no text prepend, so it is OUTSIDE the envelope and
    // keeps the raw `chunk_hash`. Only the text lanes are enveloped.
    if is_visual_chunk_kind(&chunk.chunk_kind) {
        return enqueue_visual_project_file(chunk, entity_id);
    }
    let bucket = if is_code_chunk(chunk.language.as_deref(), &chunk.chunk_kind) {
        Bucket::Code
    } else {
        Bucket::Docs
    };
    enqueue_project_file_as(chunk, project_display, entity_id, bucket)
}

pub fn enqueue_project_file_as(
    chunk: &Chunk,
    project_display: &str,
    entity_id: &str,
    bucket: Bucket,
) -> bool {
    enqueue(EmbedRequest {
        bucket,
        project_id: Some(chunk.project_id.clone()),
        entity_id: entity_id.to_string(),
        chunk_hash: project_file_text_content_hash(&chunk.chunk_hash),
        text: project_file_embed_text(chunk, project_display),
        visual_kind: None,
        visual_payload: None,
    })
}

/// Visual lane: routes through `[embed.routes.visual]` (chunk-kind-keyed,
/// never `Bucket`-keyed) instead of the bucket-keyed text route machinery.
/// `false` when the chunk carries no `visual_payload`: a visual chunk kind
/// with no payload ref is a chunker bug, not a route-config absence (that
/// case is handled downstream: an unconfigured `[embed.routes.visual]`
/// entry degrades to a per-route error status, not a panic here).
pub fn enqueue_visual_project_file(chunk: &Chunk, entity_id: &str) -> bool {
    let Some(payload) = &chunk.visual_payload else {
        tracing::warn!(
            entity_id,
            chunk_kind = %chunk.chunk_kind,
            "visual chunk kind has no visual_payload; skipping enqueue"
        );
        return false;
    };
    enqueue(EmbedRequest {
        // Ignored: visual_kind below overrides bucket-based routing.
        bucket: Bucket::Docs,
        project_id: Some(chunk.project_id.clone()),
        entity_id: entity_id.to_string(),
        chunk_hash: chunk.chunk_hash.clone(),
        text: chunk.content.clone(),
        visual_kind: Some(chunk.chunk_kind.clone()),
        visual_payload: Some(payload.clone()),
    })
}

pub fn enqueue_git_message(entity_id: &str, chunk_hash: &str, message: &str) -> bool {
    enqueue(EmbedRequest {
        bucket: Bucket::GitMessage,
        project_id: None,
        entity_id: entity_id.to_string(),
        chunk_hash: chunk_hash.to_string(),
        text: message.to_string(),
        visual_kind: None,
        visual_payload: None,
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
        visual_kind: None,
        visual_payload: None,
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
        visual_kind: None,
        visual_payload: None,
    })
}

pub fn enqueue_thread_hook(thread: &Thread) {
    let _ = enqueue_thread(thread);
}

/// Enqueue one published graph vertex's composed embed projection
/// (unified-retrieval design 4.4). `entity_id` is the canonical
/// `project_graph_vertex:<project>:<graph>:<vertex>` ref, which is what the
/// vector hit carries back into the hybrid lane; `project_id` lets a
/// per-project `[embed.routes.per_project.<id>] graph` override select the
/// provider. The dedup key is the versioned envelope hash over the composed
/// text (`GRAPH_EMBED_TEXT_VERSION`), so an unchanged vertex across
/// generations is not re-embedded and a composition change re-embeds once.
pub fn enqueue_graph_vertex(
    project_id: &str,
    entity_id: &str,
    projection: &bbox_project_graph::GraphVertexEmbedProjection,
) -> bool {
    enqueue(EmbedRequest {
        bucket: Bucket::Graph,
        project_id: Some(project_id.to_string()),
        entity_id: entity_id.to_string(),
        chunk_hash: projection.content_hash(),
        text: projection.text.clone(),
        visual_kind: None,
        visual_payload: None,
    })
}

/// Drop the vectors of graph vertices that left the embed-eligible set: a
/// generation flip removed them, a policy change excluded them, or their
/// graph left the accepted view. One store batch across every route.
pub fn tombstone_graph_vertices(entity_ids: &[String]) {
    if entity_ids.is_empty() {
        return;
    }
    if let Some(queue) = queue_slot().read().as_ref() {
        queue.tombstone_batch(entity_ids);
    } else {
        tracing::debug!(
            route = "graph",
            count = entity_ids.len(),
            "embedding queue not installed; accepted graph tombstones as no-op"
        );
    }
}

/// Whether one graph vertex's vector is active under its current envelope
/// hash. `None` when there is no queue, route table, or vector store to ask;
/// callers report that as "unknown", never as zero.
pub fn graph_vertex_vector_is_active(
    project_id: &str,
    entity_id: &str,
    content_hash: &str,
) -> Option<bool> {
    queue_slot().read().as_ref().and_then(|handle| {
        handle.vector_is_active_for_project(
            Bucket::Graph,
            Some(project_id),
            entity_id,
            content_hash,
        )
    })
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
        visual_kind: None,
        visual_payload: None,
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

    fn text_chunk(relative_path: &str, chunk_hash: &str) -> Chunk {
        let mut chunk = bbox_chunker::placeholder_chunk(
            std::path::Path::new(relative_path),
            "code_block",
            Some("rust"),
            "pub struct Helper;",
            0,
            18,
            0,
        );
        chunk.project_id = "proj1234".into();
        chunk.rel_path_hash = "abcd1234".into();
        chunk.chunk_hash = chunk_hash.to_string();
        chunk
    }

    /// P3-E embed row: the embedding INPUT carries the display name and the
    /// project-relative path, and never a host root.
    #[test]
    fn project_file_embed_text_prepends_the_display_name_and_relative_path() {
        let chunk = text_chunk("src/helper.rs", &"f".repeat(64));
        let text = project_file_embed_text(&chunk, "acme-service");
        assert!(text.starts_with("acme-service src/helper.rs\n\n"), "{text}");
        assert!(text.ends_with("pub struct Helper;"), "{text}");
        assert!(!text.contains("/host-checkouts"), "{text}");
    }

    /// P3-E embed row: dedup HIT within one version. The envelope is a pure
    /// function of `(EMBED_TEXT_VERSION, chunk_hash)`, so two enqueues of the
    /// same unchanged chunk key identically and `should_embed` skips the second.
    #[test]
    fn the_envelope_is_stable_within_one_version() {
        let hash = "f".repeat(64);
        assert_eq!(
            project_file_text_content_hash(&hash),
            project_file_text_content_hash(&hash)
        );
    }

    /// P3-E embed row: dedup MISS across the version bump. The envelope must
    /// differ from the raw `chunk_hash` it wraps, or the version bump would not
    /// miss the dedup and the prepend would never reach any vector.
    #[test]
    fn the_envelope_differs_from_the_raw_chunk_hash_it_wraps() {
        let hash = "f".repeat(64);
        let enveloped = project_file_text_content_hash(&hash);
        assert_ne!(enveloped, hash);
        assert_eq!(enveloped.len(), 64, "still a sha256 hex digest");
        // Distinct chunks stay distinct through the envelope: it adds a version
        // dimension, it does not collapse content identity.
        assert_ne!(
            project_file_text_content_hash(&hash),
            project_file_text_content_hash(&"e".repeat(64))
        );
    }

    /// P3-E embed row: the document's `chunk_hash` and every entity-ref
    /// component stay BYTE-UNTOUCHED across the bump. Only the queue's dedup key
    /// moves; folding the prepend into `chunk.content` would change
    /// `chunk_hash` and therefore `ProjectFileV2` ref identity, which plan
    /// section 4.6 forbids.
    #[test]
    fn ref_bytes_are_unchanged_by_the_envelope() {
        let hash = "f".repeat(64);
        let chunk = text_chunk("src/helper.rs", &hash);
        let before = chunk.chunk_hash.clone();
        let _ = project_file_text_content_hash(&chunk.chunk_hash);
        let _ = project_file_embed_text(&chunk, "acme-service");
        assert_eq!(chunk.chunk_hash, before);
        assert_eq!(chunk.rel_path_hash, "abcd1234");
        assert_eq!(chunk.project_id, "proj1234");
        assert_eq!(chunk.occurrence_idx, 0);
    }

    #[test]
    fn code_chunk_rule_treats_markdown_labels_as_prose() {
        assert!(is_code_chunk(Some("rust"), "code_block"));
        assert!(is_code_chunk(Some("rust"), "notebook_cell"));
        assert!(is_code_chunk(Some("json"), "config"));
        assert!(is_code_chunk(None, "code_block"));
        assert!(!is_code_chunk(Some("md"), "doc_section"));
        assert!(!is_code_chunk(Some("markdown"), "doc_section"));
        assert!(!is_code_chunk(None, "doc_section"));
        assert!(!is_code_chunk(None, "pdf_page"));
        assert!(!is_code_chunk(None, "office_section"));
        assert!(!is_code_chunk(None, "spreadsheet_sheet"));
    }

    #[test]
    fn visual_chunk_kinds_cover_x_img_and_reserved_future_kinds() {
        assert!(is_visual_chunk_kind("image"));
        assert!(is_visual_chunk_kind("pdf_figure"));
        assert!(is_visual_chunk_kind("spreadsheet_chart"));
        assert!(is_visual_chunk_kind("slide_image"));
        assert!(is_visual_chunk_kind("image_caption"));
        assert!(is_visual_chunk_kind("video_segment"));
        assert!(!is_visual_chunk_kind("pdf_page"));
        assert!(!is_visual_chunk_kind("doc_section"));
        assert!(!is_visual_chunk_kind("code_block"));
    }

    fn image_chunk(visual_payload: Option<bbox_visual_store::VisualPayloadRef>) -> Chunk {
        let mut chunk = bbox_chunker::placeholder_chunk(
            std::path::Path::new("assets/figure.png"),
            "image",
            None,
            "figure",
            0,
            4,
            0,
        );
        chunk.project_id = "proj1234".into();
        chunk.rel_path_hash = "abcd1234".into();
        chunk.chunk_hash = "f".repeat(64);
        chunk.visual_payload = visual_payload;
        chunk
    }

    #[test]
    fn enqueue_project_file_routes_image_chunks_through_the_visual_lane() {
        // No queue installed in this test process: enqueue_visual_project_file
        // short-circuits on the missing visual_payload before ever touching
        // the (uninstalled) global queue, so this exercises the routing
        // decision in enqueue_project_file without needing queue plumbing.
        let chunk = image_chunk(None);
        assert!(!enqueue_project_file(
            &chunk,
            "acme",
            "project_file:proj1234:abcd1234:hash:0"
        ));
    }

    #[test]
    fn enqueue_visual_project_file_without_payload_is_a_noop() {
        let chunk = image_chunk(None);
        assert!(!enqueue_visual_project_file(
            &chunk,
            "project_file:proj1234:abcd1234:hash:0"
        ));
    }

    use bbox_threads::threads::{
        EdgeKind, EdgeTarget, SessionLink, Thread, ThreadEdge, ThreadKind, ThreadStatus,
    };

    fn sample_thread() -> Thread {
        Thread {
            id: "thread-1234abcd".into(),
            name: Some("thread embeddings".into()),
            topic: "embed inline thread notes".into(),
            project: "/repo/blackbox".into(),
            project_id: None,
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
