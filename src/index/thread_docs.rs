use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::UNIX_EPOCH;

use anyhow::Result;
use sha2::{Digest, Sha256};
use tantivy::{Index, IndexWriter, TantivyDocument, Term};

use super::{FieldHandles, FileMeta};
use crate::entity_ref::EntityRef;
use crate::threads::{Thread, Threads};

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
    doc.add_text(f.parser_version, crate::entity_ref::PARSER_VERSION);
    doc.add_text(f.content, &content);
    doc.add_text(f.entity_id, thread_entity_id(&thread.id));
    doc.add_text(f.account, "blackbox");
    doc.add_text(f.project, &thread.project);
    doc.add_text(f.role, "thread");
    doc.add_text(f.file_path, &threads_path.to_string_lossy());
    doc.add_text(
        f.path_tokens,
        &format!(
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

pub(crate) fn reindex_threads_store_standalone(
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
        },
    );
    Ok(threads.all().len() as u64)
}

pub(crate) fn upsert_threads_store(
    index: &Index,
    f: FieldHandles,
    threads_path: &Path,
    threads: &Threads,
) -> Result<()> {
    let path_str = threads_path.to_string_lossy().to_string();
    let mut writer = index.writer(50_000_000)?;
    writer.delete_term(Term::from_field_text(f.file_path, &path_str));
    for thread in threads.all() {
        writer.add_document(build_thread_doc(thread, threads_path, f))?;
    }
    writer.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::build_schema;
    use crate::threads::{ThreadKind, ThreadStatus};

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
            status: ThreadStatus::Active,
            kind: Some(ThreadKind::Investigation),
            sessions: Vec::new(),
            handoff_doc: Some("handoff marker".into()),
            notes: vec!["inline note marker".into()],
            edges: vec![crate::threads::ThreadEdge {
                kind: crate::threads::EdgeKind::RelatesTo,
                target: "thread-def67890".into(),
                target_type: crate::threads::EdgeTarget::Thread,
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
}
