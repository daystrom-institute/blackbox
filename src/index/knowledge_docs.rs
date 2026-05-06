use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::UNIX_EPOCH;

use anyhow::Result;
use sha2::{Digest, Sha256};
use tantivy::schema::Term;
use tantivy::{Index, IndexWriter, TantivyDocument};

use super::{FieldHandles, FileMeta};
use crate::entity_ref::{EntityRef, PARSER_VERSION};
use crate::knowledge::{Knowledge, KnowledgeEntry, Status};

pub(crate) fn knowledge_entity_id(entry_id: &str) -> String {
    EntityRef::Knowledge {
        id: entry_id.to_string(),
    }
    .to_string()
}

pub(crate) fn knowledge_chunk_hash(entry: &KnowledgeEntry) -> String {
    let mut hasher = Sha256::new();
    hasher.update(entry.content.as_bytes());
    hex::encode(hasher.finalize())
}

pub(crate) fn build_knowledge_doc(
    entry: &KnowledgeEntry,
    knowledge_path: &Path,
    f: FieldHandles,
) -> TantivyDocument {
    let mut doc = TantivyDocument::new();
    doc.add_text(f.doc_type, "knowledge");
    doc.add_text(f.parser_version, PARSER_VERSION);
    doc.add_text(f.entity_id, knowledge_entity_id(&entry.id));
    doc.add_text(f.chunk_hash, knowledge_chunk_hash(entry));
    doc.add_text(f.chunk_kind, "knowledge_entry");
    doc.add_text(f.account, "knowledge");
    doc.add_text(f.role, "knowledge");
    doc.add_text(f.file_path, knowledge_path.to_string_lossy());
    doc.add_text(f.timestamp, &entry.updated_at);
    if let Some(project) = &entry.project {
        doc.add_text(f.project, project);
    }
    doc.add_text(f.content, format!("{}\n\n{}", entry.title, entry.content));
    doc
}

pub(crate) fn indexable_knowledge_entry(entry: &KnowledgeEntry) -> bool {
    entry.status == Status::Active
}

pub(crate) fn reindex_knowledge_store_standalone(
    knowledge_path: &Path,
    fields: FieldHandles,
    writer: &mut IndexWriter,
    meta: &mut HashMap<String, FileMeta>,
) -> Result<u64> {
    writer.delete_term(Term::from_field_text(fields.doc_type, "knowledge"));
    if !knowledge_path.exists() {
        meta.remove(knowledge_path.to_string_lossy().as_ref());
        return Ok(0);
    }
    let knowledge = Knowledge::open(knowledge_path)?;
    let mut docs = 0;
    for entry in knowledge
        .all_entries()
        .iter()
        .filter(|entry| indexable_knowledge_entry(entry))
    {
        writer.add_document(build_knowledge_doc(entry, knowledge_path, fields))?;
        docs += 1;
    }
    if let Some(file_meta) = file_meta(knowledge_path) {
        meta.insert(knowledge_path.to_string_lossy().to_string(), file_meta);
    }
    Ok(docs)
}

pub(crate) fn upsert_knowledge_entry(
    index: &Index,
    fields: FieldHandles,
    knowledge_path: &Path,
    entry: &KnowledgeEntry,
) -> Result<()> {
    let mut writer: IndexWriter = index.writer(50_000_000)?;
    let entity_id = knowledge_entity_id(&entry.id);
    writer.delete_term(Term::from_field_text(fields.entity_id, &entity_id));
    if indexable_knowledge_entry(entry) {
        writer.add_document(build_knowledge_doc(entry, knowledge_path, fields))?;
    }
    writer.commit()?;
    Ok(())
}

pub(crate) fn delete_knowledge_entry(
    index: &Index,
    fields: FieldHandles,
    entry_id: &str,
) -> Result<()> {
    let mut writer: IndexWriter = index.writer(50_000_000)?;
    writer.delete_term(Term::from_field_text(
        fields.entity_id,
        &knowledge_entity_id(entry_id),
    ));
    writer.commit()?;
    Ok(())
}

fn file_meta(path: &Path) -> Option<FileMeta> {
    let meta = fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    Some(FileMeta {
        mtime,
        size: meta.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::{Approval, Category, Priority, Scope};

    #[test]
    fn knowledge_doc_carries_entity_id_and_content() {
        let (_schema, fields) = crate::index::build_schema();
        let entry = KnowledgeEntry {
            id: "abc12345".into(),
            title: "Render lifecycle".into(),
            content: "bbox_render publishes approved knowledge".into(),
            cluster: None,
            variants: Default::default(),
            category: Category::Memory,
            scope: Scope::Global,
            project: None,
            providers: Vec::new(),
            priority: Priority::Standard,
            weight: 100,
            status: Status::Active,
            approval: Approval::UserConfirmed,
            render: true,
            decay: true,
            review_at: None,
            supersedes: None,
            rationale: None,
            expires_at: None,
            source: "test".into(),
            created_at: "2026-05-05T17:30:00Z".into(),
            updated_at: "2026-05-05T17:30:00Z".into(),
            recall_count: 0,
            last_recalled: None,
        };

        let doc = build_knowledge_doc(&entry, Path::new("/tmp/kb.json"), fields);

        assert_eq!(first_text(&doc, fields.doc_type), "knowledge");
        assert_eq!(first_text(&doc, fields.entity_id), "knowledge:abc12345");
        assert!(first_text(&doc, fields.content).contains("bbox_render"));
    }

    fn first_text(doc: &TantivyDocument, field: tantivy::schema::Field) -> String {
        doc.get_all(field)
            .next()
            .and_then(|value| match value {
                tantivy::schema::OwnedValue::Str(text) => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }
}
