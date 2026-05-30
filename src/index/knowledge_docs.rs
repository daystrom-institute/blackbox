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

/// Build a tantivy document for a knowledge entry. `account` and `role` fields
/// are transcript-only; queries scoped by account/role return only transcript
/// hits, not knowledge entries.
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
    doc.add_text(f.file_path, knowledge_path.to_string_lossy());
    doc.add_text(f.timestamp, &entry.updated_at);
    if let Some(project) = &entry.project {
        doc.add_text(f.project, project);
    }
    doc.add_text(f.content, format!("{}\n\n{}", entry.title, entry.content));
    doc
}

pub(crate) fn indexable_knowledge_entry(entry: &KnowledgeEntry) -> bool {
    // Superseded entries remain searchable so history queries can find the
    // original decision; H1 rerank should downweight them by status.
    matches!(entry.status, Status::Active | Status::Superseded)
}

pub(crate) fn reindex_knowledge_store_standalone(
    knowledge_path: &Path,
    projects_path: &Path,
    fields: FieldHandles,
    writer: &mut IndexWriter,
    meta: &mut HashMap<String, FileMeta>,
) -> Result<u64> {
    writer.delete_term(Term::from_field_text(fields.doc_type, "knowledge"));
    if !knowledge_path.exists() {
        meta.remove(knowledge_path.to_string_lossy().as_ref());
        return Ok(0);
    }
    let mut knowledge = Knowledge::open(knowledge_path)?;
    // Project-scoped entries live in each repo's committed .bbox/knowledge/, not
    // the central store, so a reindex that only read kb.json would silently drop
    // them from search. Load the registered repos' roots so committed project
    // knowledge is indexed too.
    let roots: Vec<std::path::PathBuf> = crate::projects::ProjectRegistry::load_records(projects_path)
        .unwrap_or_default()
        .into_iter()
        .map(|r| std::path::PathBuf::from(r.canonical_path))
        .collect();
    if let Err(e) = knowledge.set_project_roots(roots) {
        tracing::warn!("kb reindex project-root load: {e:#}");
    }
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
    // NOTE: opens a fresh writer per call. Acceptable at low knowledge-write volume
    // (typical: <10 writes/min from operator + agent activity). If burst writes start
    // appearing in logs, share a long-lived writer in SharedState with debounced
    // commits, or hand off to the existing reindex thread via a channel.
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
    // NOTE: opens a fresh writer per call. Acceptable at low knowledge-write volume
    // (typical: <10 writes/min from operator + agent activity). If burst writes start
    // appearing in logs, share a long-lived writer in SharedState with debounced
    // commits, or hand off to the existing reindex thread via a channel.
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
        mat_version: None,
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
            links: Vec::new(),
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
        assert_eq!(first_text(&doc, fields.account), "");
        assert_eq!(first_text(&doc, fields.role), "");
        assert!(first_text(&doc, fields.content).contains("bbox_render"));
    }

    #[test]
    fn superseded_knowledge_entries_remain_indexable() {
        let mut entry = KnowledgeEntry {
            id: "abc12345".into(),
            title: "Original decision".into(),
            content: "first decision about postgres consolidation".into(),
            cluster: None,
            variants: Default::default(),
            category: Category::Decision,
            scope: Scope::Global,
            project: None,
            providers: Vec::new(),
            priority: Priority::Standard,
            weight: 100,
            status: Status::Superseded,
            approval: Approval::UserConfirmed,
            render: true,
            decay: true,
            review_at: None,
            supersedes: None,
            links: Vec::new(),
            rationale: Some("fixture".into()),
            expires_at: None,
            source: "test".into(),
            created_at: "2026-05-05T17:30:00Z".into(),
            updated_at: "2026-05-05T17:30:00Z".into(),
            recall_count: 0,
            last_recalled: None,
        };
        assert!(indexable_knowledge_entry(&entry));
        entry.status = Status::Deleted;
        assert!(!indexable_knowledge_entry(&entry));
    }

    #[test]
    fn reindex_includes_committed_project_bbox_entries() {
        use crate::knowledge::KnowledgeStore;
        use tantivy::Index;

        let central = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let repo_root = repo.path().canonicalize().unwrap();

        // Committed project entry in the repo's .bbox/knowledge/ (project omitted).
        let kb_dir = repo_root.join(".bbox").join("knowledge");
        std::fs::create_dir_all(&kb_dir).unwrap();
        let entry = KnowledgeEntry {
            id: "proj0001".into(),
            title: "repo convention".into(),
            content: "REPO_OWNED_SEARCHABLE".into(),
            cluster: None,
            variants: Default::default(),
            category: Category::Convention,
            scope: Scope::Project,
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
            links: Vec::new(),
            rationale: None,
            expires_at: None,
            source: "test".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            recall_count: 0,
            last_recalled: None,
        };
        std::fs::write(
            kb_dir.join("proj0001.json"),
            serde_json::to_string(&entry).unwrap(),
        )
        .unwrap();

        // Empty central store + a sibling projects.json registering the repo.
        let kb_path = central.path().join("kb.json");
        std::fs::write(
            &kb_path,
            serde_json::to_string(&KnowledgeStore {
                version: 1,
                entries: vec![],
            })
            .unwrap(),
        )
        .unwrap();
        let projects_path = central.path().join("projects.json");
        {
            let mut reg = crate::projects::ProjectRegistry::open(&projects_path).unwrap();
            reg.register_path(&repo_root).unwrap();
        }

        let (schema, fields) = crate::index::build_schema();
        let index = Index::create_in_ram(schema);
        let mut writer: IndexWriter = index.writer(15_000_000).unwrap();
        let mut meta = HashMap::new();
        let docs =
            reindex_knowledge_store_standalone(&kb_path, &projects_path, fields, &mut writer, &mut meta)
                .unwrap();

        assert_eq!(
            docs, 1,
            "the committed project .bbox/knowledge entry must be indexed even though central is empty"
        );
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
