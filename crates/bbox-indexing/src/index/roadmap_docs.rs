use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::UNIX_EPOCH;

use anyhow::Result;
use sha2::{Digest, Sha256};
use tantivy::schema::Term;
use tantivy::{IndexWriter, TantivyDocument};

use super::{FieldHandles, FileMeta};
use bbox_corpus_core::entity_ref::{EntityRef, PARSER_VERSION};
use bbox_stores::roadmap::{Roadmap, RoadmapItem};

pub fn roadmap_entity_id(id: &str) -> String {
    EntityRef::RoadmapItem { id: id.to_string() }.to_string()
}

pub fn roadmap_chunk_hash(item: &RoadmapItem) -> String {
    let mut hasher = Sha256::new();
    hasher.update(item.body.as_bytes());
    hex::encode(hasher.finalize())
}

pub fn build_roadmap_doc(
    item: &RoadmapItem,
    roadmap_path: &Path,
    f: FieldHandles,
) -> TantivyDocument {
    let mut doc = TantivyDocument::new();
    doc.add_text(f.doc_type, "roadmap");
    doc.add_text(f.parser_version, PARSER_VERSION);
    doc.add_text(f.entity_id, roadmap_entity_id(&item.id));
    doc.add_text(f.chunk_hash, roadmap_chunk_hash(item));
    doc.add_text(f.chunk_kind, "roadmap_item");
    doc.add_text(f.file_path, roadmap_path.to_string_lossy());
    doc.add_text(f.timestamp, &item.updated_at);
    if let Some(project) = &item.project {
        doc.add_text(f.project, project);
    }
    doc.add_text(f.content, format!("{}\n\n{}", item.title, item.body));
    doc
}

/// All non-rejected roadmap items are indexable.
pub fn indexable_roadmap_item(item: &RoadmapItem) -> bool {
    !matches!(item.status, bbox_stores::roadmap::RoadmapStatus::Rejected)
}

pub fn reindex_roadmap_store_standalone(
    roadmap_path: &Path,
    fields: FieldHandles,
    writer: &mut IndexWriter,
    meta: &mut HashMap<String, FileMeta>,
) -> Result<u64> {
    writer.delete_term(Term::from_field_text(fields.doc_type, "roadmap"));
    if !roadmap_path.exists() {
        meta.remove(roadmap_path.to_string_lossy().as_ref());
        return Ok(0);
    }
    let roadmap = Roadmap::open(roadmap_path)?;
    let mut docs = 0;
    for item in roadmap
        .all_items()
        .iter()
        .filter(|item| indexable_roadmap_item(item))
    {
        writer.add_document(build_roadmap_doc(item, roadmap_path, fields))?;
        docs += 1;
    }
    if let Some(file_meta) = file_meta(roadmap_path) {
        meta.insert(roadmap_path.to_string_lossy().to_string(), file_meta);
    }
    Ok(docs)
}

/// Apply a roadmap upsert to an already-held writer (no commit).
pub fn apply_roadmap_upsert(
    writer: &mut IndexWriter,
    fields: FieldHandles,
    roadmap_path: &Path,
    item: &RoadmapItem,
) -> Result<()> {
    let entity_id = roadmap_entity_id(&item.id);
    writer.delete_term(Term::from_field_text(fields.entity_id, &entity_id));
    if indexable_roadmap_item(item) {
        writer.add_document(build_roadmap_doc(item, roadmap_path, fields))?;
    }
    Ok(())
}

/// Apply a roadmap delete to an already-held writer (no commit).
pub fn apply_roadmap_delete(
    writer: &mut IndexWriter,
    fields: FieldHandles,
    item_id: &str,
) -> Result<()> {
    writer.delete_term(Term::from_field_text(
        fields.entity_id,
        &roadmap_entity_id(item_id),
    ));
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
        mat_version: None,
        source: Default::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roadmap_doc_carries_entity_id_and_content() {
        let (_schema, fields) = crate::index::build_schema();
        let item = RoadmapItem {
            id: "roadmap-a1b2c3d4".into(),
            title: "Add roadmap tracker".into(),
            body: "Build bbox_roadmap tool for prospective work tracking".into(),
            status: bbox_stores::roadmap::RoadmapStatus::Accepted,
            category: bbox_stores::roadmap::RoadmapCategory::Feature,
            priority: bbox_stores::roadmap::RoadmapPriority::High,
            scope: "project".into(),
            project: Some("/tmp/test-project".into()),
            project_id: None,
            created_at: "2026-05-08T12:00:00Z".into(),
            updated_at: "2026-05-08T12:00:00Z".into(),
            transitions: Vec::new(),
        };

        let doc = build_roadmap_doc(&item, Path::new("/tmp/roadmap.json"), fields);

        assert_eq!(first_text(&doc, fields.doc_type), "roadmap");
        assert_eq!(
            first_text(&doc, fields.entity_id),
            "roadmap_item:roadmap-a1b2c3d4"
        );
        assert!(first_text(&doc, fields.content).contains("bbox_roadmap"));
    }

    #[test]
    fn rejected_roadmap_items_are_not_indexable() {
        let mut item = RoadmapItem {
            id: "roadmap-x1".into(),
            title: "Bad idea".into(),
            body: "nope".into(),
            status: bbox_stores::roadmap::RoadmapStatus::Rejected,
            category: bbox_stores::roadmap::RoadmapCategory::Feature,
            priority: bbox_stores::roadmap::RoadmapPriority::Low,
            scope: "project".into(),
            project: None,
            project_id: None,
            created_at: "2026-05-08T12:00:00Z".into(),
            updated_at: "2026-05-08T12:00:00Z".into(),
            transitions: Vec::new(),
        };
        assert!(!indexable_roadmap_item(&item));
        item.status = bbox_stores::roadmap::RoadmapStatus::Accepted;
        assert!(indexable_roadmap_item(&item));
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
