use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::Result;
use sha2::{Digest, Sha256};
use tantivy::{IndexWriter, TantivyDocument, Term};

use super::{FieldHandles, FileMeta};
use crate::projects::ProjectRegistry;
use bbox_corpus_core::entity_ref::EntityRef;
use bbox_threads::threads::{Thread, ThreadRecord, Threads};

fn thread_entity_id(thread_id: &str) -> String {
    EntityRef::Thread {
        thread_id: thread_id.to_string(),
    }
    .to_string()
}

fn thread_content(thread: &Thread) -> String {
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
    if let Some(origin) = thread.origin {
        fields.push(format!("origin: {}", origin.as_ref()));
    }
    if let Some(handoff_doc) = &thread.handoff_doc {
        fields.push(format!("handoff_doc:\n{handoff_doc}"));
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
            fields.push(format!(
                "{} {} {}",
                session.provider,
                session.session_id,
                session.name.as_deref().unwrap_or("")
            ));
        }
    }
    if !thread.edges.is_empty() {
        fields.push("thread_edges:".to_string());
        for edge in &thread.edges {
            fields.push(format!(
                "{} -> {}:{} {}",
                edge.kind.as_ref(),
                edge.target_type.as_ref(),
                edge.target,
                edge.note.as_deref().unwrap_or("")
            ));
        }
    }
    fields.join("\n")
}

fn content_hash(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn build_thread_doc(thread: &Thread, threads_path: &Path, f: FieldHandles) -> TantivyDocument {
    let content = thread_content(thread);
    let mut doc = TantivyDocument::new();
    doc.add_text(f.doc_type, "thread");
    doc.add_text(
        f.parser_version,
        bbox_corpus_core::entity_ref::PARSER_VERSION,
    );
    doc.add_text(f.content, &content);
    doc.add_text(f.entity_id, thread_entity_id(&thread.id));
    doc.add_text(f.account, "blackbox");
    doc.add_text(f.project, &thread.project);
    doc.add_text(f.role, "thread");
    doc.add_text(f.file_path, threads_path.to_string_lossy());
    doc.add_text(
        f.path_tokens,
        format!(
            "thread {} {} {} {}",
            thread.id,
            thread.name.as_deref().unwrap_or(""),
            thread.topic,
            thread.project
        ),
    );
    doc.add_text(f.chunk_kind, "thread");
    doc.add_text(f.symbol, thread.name.as_deref().unwrap_or(&thread.topic));
    doc.add_text(f.symbol_exact, &thread.id);
    doc.add_text(f.chunk_hash, content_hash(&content));
    doc.add_text(f.timestamp, &thread.last_activity);
    doc
}

fn record_content(record: &ThreadRecord, project_dir: &Path) -> String {
    let mut fields = vec![
        "entity: thread".to_string(),
        format!("thread_id: {}", record.id),
        format!("topic: {}", record.topic),
        format!("project: {}", project_dir.display()),
        format!("status: {}", record.status.as_ref()),
    ];
    if let Some(kind) = record.kind {
        fields.push(format!("kind: {}", kind.as_ref()));
    }
    if let Some(promoted_to) = &record.promoted_to {
        fields.push(format!("promoted_to: {promoted_to}"));
    }
    if !record.notes.is_empty() {
        fields.push("inline_notes:".to_string());
        for (idx, note) in record.notes.iter().enumerate() {
            fields.push(format!("note {}:\n{}", idx + 1, note));
        }
    }
    fields.join("\n")
}

fn build_record_doc(
    record: &ThreadRecord,
    project_dir: &Path,
    record_path: &Path,
    f: FieldHandles,
) -> TantivyDocument {
    let content = record_content(record, project_dir);
    let mut doc = TantivyDocument::new();
    // Indexed as a `thread` so a committed record is found exactly like the
    // live thread it snapshots (same entity_id); on a clone it is the only copy.
    doc.add_text(f.doc_type, "thread");
    doc.add_text(
        f.parser_version,
        bbox_corpus_core::entity_ref::PARSER_VERSION,
    );
    doc.add_text(f.content, &content);
    doc.add_text(f.entity_id, thread_entity_id(&record.id));
    doc.add_text(f.account, "blackbox");
    doc.add_text(f.project, project_dir.to_string_lossy());
    doc.add_text(f.role, "thread");
    doc.add_text(f.file_path, record_path.to_string_lossy());
    doc.add_text(
        f.path_tokens,
        format!(
            "thread {} {} {}",
            record.id,
            record.topic,
            project_dir.display()
        ),
    );
    doc.add_text(f.chunk_kind, "thread");
    doc.add_text(f.symbol, &record.topic);
    doc.add_text(f.symbol_exact, &record.id);
    doc.add_text(f.chunk_hash, content_hash(&content));
    doc.add_text(f.timestamp, &record.resolved_at);
    doc
}

/// Index committed `.bbox/record/` thread snapshots so resolved/promoted
/// investigations are searchable on a clone where the live thread store doesn't
/// carry them. Deduped against the live thread store by id — on the origin
/// machine the live thread is already indexed, so its record is skipped.
pub fn reindex_project_records_standalone(
    projects_path: &Path,
    threads_path: &Path,
    f: FieldHandles,
    writer: &mut IndexWriter,
) -> Result<u64> {
    let live_ids: HashSet<String> = if threads_path.exists() {
        Threads::open(threads_path)?
            .all()
            .iter()
            .map(|t| t.id.clone())
            .collect()
    } else {
        HashSet::new()
    };
    let roots: Vec<PathBuf> = ProjectRegistry::load_records(projects_path)
        .unwrap_or_default()
        .into_iter()
        .map(|r| PathBuf::from(r.canonical_path))
        .collect();
    let mut docs = 0u64;
    for root in &roots {
        for record in bbox_threads::threads::load_repo_records(root) {
            if live_ids.contains(&record.id) {
                continue;
            }
            let record_path = root
                .join(".bbox")
                .join("record")
                .join(format!("{}.json", record.id));
            // Delete-before-add keeps reindex idempotent per record file.
            writer.delete_term(Term::from_field_text(
                f.file_path,
                record_path.to_string_lossy().as_ref(),
            ));
            writer.add_document(build_record_doc(&record, root, &record_path, f))?;
            docs += 1;
        }
    }
    Ok(docs)
}

pub fn reindex_threads_store_standalone(
    threads_path: &Path,
    f: FieldHandles,
    writer: &mut IndexWriter,
    meta: &mut HashMap<String, FileMeta>,
) -> Result<u64> {
    if !threads_path.exists() {
        return Ok(0);
    }
    let path_str = threads_path.to_string_lossy().to_string();
    let file_meta = fs::metadata(threads_path)?;
    let mtime = file_meta.modified()?.duration_since(UNIX_EPOCH)?.as_secs();
    if super::reindex::should_skip_file(&path_str, mtime, file_meta.len(), meta) {
        return Ok(0);
    }
    if meta.contains_key(&path_str) {
        writer.delete_term(Term::from_field_text(f.file_path, &path_str));
    }
    let threads = Threads::open(threads_path)?;
    for thread in threads.all() {
        writer.add_document(build_thread_doc(thread, threads_path, f))?;
    }
    meta.insert(
        path_str,
        FileMeta {
            mtime,
            size: file_meta.len(),
            mat_version: None,
        },
    );
    Ok(threads.all().len() as u64)
}

/// Apply a full thread-store replacement to an already-held writer (no
/// commit). `threads` is a point-in-time snapshot of every thread.
pub fn apply_threads_store_upsert(
    writer: &mut IndexWriter,
    f: FieldHandles,
    threads_path: &Path,
    threads: &[Thread],
) -> Result<()> {
    let path_str = threads_path.to_string_lossy().to_string();
    writer.delete_term(Term::from_field_text(f.file_path, &path_str));
    for thread in threads {
        writer.add_document(build_thread_doc(thread, threads_path, f))?;
    }
    Ok(())
}

/// Apply a single-thread upsert to an already-held writer (no commit).
pub fn apply_thread_upsert(
    writer: &mut IndexWriter,
    f: FieldHandles,
    threads_path: &Path,
    thread: &Thread,
) -> Result<()> {
    writer.delete_term(Term::from_field_text(
        f.entity_id,
        &thread_entity_id(&thread.id),
    ));
    writer.add_document(build_thread_doc(thread, threads_path, f))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::build_schema;
    use bbox_threads::threads::{ThreadKind, ThreadStatus};

    fn first_text(doc: &TantivyDocument, field: tantivy::schema::Field) -> String {
        doc.get_first(field)
            .and_then(|value| match value {
                tantivy::schema::OwnedValue::Str(value) => Some(value.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    #[test]
    fn thread_doc_contains_handoff_notes_and_edges() {
        let (_schema, fields) = build_schema();
        let thread = Thread {
            id: "thread-abc12345".into(),
            name: Some("thread-name".into()),
            topic: "Investigate rich thread content".into(),
            project: "/repo".into(),
            record_dir: None,
            status: ThreadStatus::Active,
            kind: Some(ThreadKind::Investigation),
            origin: None,
            sessions: Vec::new(),
            handoff_doc: Some("handoff marker".into()),
            notes: vec!["inline note marker".into()],
            edges: vec![bbox_threads::threads::ThreadEdge {
                kind: bbox_threads::threads::EdgeKind::RelatesTo,
                target: "thread-def67890".into(),
                target_type: bbox_threads::threads::EdgeTarget::Thread,
                note: Some("edge note marker".into()),
                created_at: "2026-05-06T00:00:00Z".into(),
            }],
            promoted_to: None,
            created_at: "2026-05-06T00:00:00Z".into(),
            last_activity: "2026-05-06T00:00:00Z".into(),
            resolved_at: None,
        };
        let doc = build_thread_doc(&thread, Path::new("/tmp/threads.json"), fields);
        assert_eq!(first_text(&doc, fields.doc_type), "thread");
        assert_eq!(first_text(&doc, fields.entity_id), "thread:thread-abc12345");
        let content = first_text(&doc, fields.content);
        assert!(content.contains("handoff marker"));
        assert!(content.contains("inline note marker"));
        assert!(content.contains("relates_to -> thread:thread-def67890 edge note marker"));
    }

    fn make_record() -> ThreadRecord {
        ThreadRecord {
            id: "thread-rec00001".into(),
            topic: "audit dispatch".into(),
            status: ThreadStatus::Resolved,
            kind: Some(ThreadKind::Investigation),
            promoted_to: None,
            notes: vec!["RECORD_NOTE_MARKER".into()],
            created_at: "2026-01-01T00:00:00Z".into(),
            resolved_at: "2026-01-02T00:00:00Z".into(),
        }
    }

    fn write_committed_record(repo_root: &Path, record: &ThreadRecord) {
        let rec_dir = repo_root.join(".bbox").join("record");
        std::fs::create_dir_all(&rec_dir).unwrap();
        std::fs::write(
            rec_dir.join(format!("{}.json", record.id)),
            serde_json::to_string(record).unwrap(),
        )
        .unwrap();
    }

    fn register(projects_path: &Path, repo_root: &Path) {
        let mut reg = ProjectRegistry::open(projects_path).unwrap();
        reg.register_path(repo_root).unwrap();
        // register_path is memory-only post-persister-conversion; the
        // standalone reindex reads projects.json from disk, so flush the
        // snapshot explicitly (the production path persists via the actor).
        bbox_corpus_core::json_store::atomic_write_json_locked(
            projects_path,
            &<ProjectRegistry as bbox_stores::store_persister::StoreSnapshot>::snapshot(&reg)
                .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn project_records_index_when_no_live_thread() {
        let central = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let repo_root = repo.path().canonicalize().unwrap();
        let record = make_record();
        write_committed_record(&repo_root, &record);

        let projects_path = central.path().join("projects.json");
        register(&projects_path, &repo_root);
        let threads_path = central.path().join("threads.json"); // absent → no live ids

        let (schema, fields) = build_schema();
        let index = tantivy::Index::create_in_ram(schema);
        let mut writer: IndexWriter = index.writer(15_000_000).unwrap();
        let docs =
            reindex_project_records_standalone(&projects_path, &threads_path, fields, &mut writer)
                .unwrap();
        assert_eq!(
            docs, 1,
            "committed record should index when no live thread carries it"
        );
    }

    #[test]
    fn project_records_skip_when_live_thread_present() {
        let central = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let repo_root = repo.path().canonicalize().unwrap();
        let record = make_record();
        write_committed_record(&repo_root, &record);

        let projects_path = central.path().join("projects.json");
        register(&projects_path, &repo_root);

        // Live thread store carries the same id (origin machine): record skipped.
        let threads_path = central.path().join("threads.json");
        let live = Thread {
            id: record.id.clone(),
            name: None,
            topic: record.topic.clone(),
            project: repo_root.to_string_lossy().into_owned(),
            record_dir: None,
            status: ThreadStatus::Resolved,
            kind: None,
            origin: None,
            sessions: Vec::new(),
            handoff_doc: None,
            notes: Vec::new(),
            edges: Vec::new(),
            promoted_to: None,
            created_at: "2026-01-01T00:00:00Z".into(),
            last_activity: "2026-01-02T00:00:00Z".into(),
            resolved_at: Some("2026-01-02T00:00:00Z".into()),
        };
        std::fs::write(
            &threads_path,
            serde_json::to_string(&bbox_threads::threads::ThreadStore {
                version: 1,
                threads: vec![live],
            })
            .unwrap(),
        )
        .unwrap();

        let (schema, fields) = build_schema();
        let index = tantivy::Index::create_in_ram(schema);
        let mut writer: IndexWriter = index.writer(15_000_000).unwrap();
        let docs =
            reindex_project_records_standalone(&projects_path, &threads_path, fields, &mut writer)
                .unwrap();
        assert_eq!(
            docs, 0,
            "record must be skipped when the live thread is already indexed"
        );
    }
}
