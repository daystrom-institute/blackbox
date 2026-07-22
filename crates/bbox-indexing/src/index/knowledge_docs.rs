use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::Path;
use std::time::UNIX_EPOCH;

use anyhow::Result;
use sha2::{Digest, Sha256};
use tantivy::schema::Term;
use tantivy::{IndexWriter, TantivyDocument};

use super::{FieldHandles, FileMeta};
use bbox_corpus_core::entity_ref::{EntityRef, PARSER_VERSION};
use bbox_corpus_core::identity::PublishedScope;
use bbox_knowledge::knowledge::{Knowledge, KnowledgeEntry, Status};
use bbox_knowledge::overlay::{load_published_snapshot, published_scope_hash};

#[derive(Debug, Clone)]
pub struct KnowledgeIndexDocument {
    pub entry: KnowledgeEntry,
    pub entity_id: String,
    pub logical_ref: String,
    pub visibility: String,
    pub scope_hash: Option<String>,
    pub checkout_id: Option<String>,
    pub snapshot_id: Option<String>,
}

impl KnowledgeIndexDocument {
    pub fn published(entry: KnowledgeEntry) -> Self {
        let entity_id = knowledge_entity_id(&entry.id);
        Self {
            logical_ref: entity_id.clone(),
            entity_id,
            entry,
            visibility: "published".into(),
            scope_hash: None,
            checkout_id: None,
            snapshot_id: None,
        }
    }
}

pub fn knowledge_entity_id(entry_id: &str) -> String {
    EntityRef::Knowledge {
        id: entry_id.to_string(),
    }
    .to_string()
}

pub fn knowledge_chunk_hash(entry: &KnowledgeEntry) -> String {
    let mut hasher = Sha256::new();
    hasher.update(entry.content.as_bytes());
    hex::encode(hasher.finalize())
}

/// Build a tantivy document for a knowledge entry. `account` and `role` fields
/// are transcript-only; queries scoped by account/role return only transcript
/// hits, not knowledge entries.
pub fn build_knowledge_doc(
    entry: &KnowledgeEntry,
    knowledge_path: &Path,
    f: FieldHandles,
) -> TantivyDocument {
    build_knowledge_index_doc(
        &KnowledgeIndexDocument::published(entry.clone()),
        knowledge_path,
        f,
    )
}

pub fn build_knowledge_index_doc(
    source: &KnowledgeIndexDocument,
    knowledge_path: &Path,
    f: FieldHandles,
) -> TantivyDocument {
    let entry = &source.entry;
    let mut doc = TantivyDocument::new();
    doc.add_text(f.doc_type, "knowledge");
    doc.add_text(f.parser_version, PARSER_VERSION);
    doc.add_text(f.entity_id, &source.entity_id);
    doc.add_text(f.logical_ref, &source.logical_ref);
    doc.add_text(f.knowledge_visibility, &source.visibility);
    if let Some(scope_hash) = &source.scope_hash {
        doc.add_text(f.knowledge_scope_hash, scope_hash);
    }
    if let Some(checkout_id) = &source.checkout_id {
        doc.add_text(f.knowledge_checkout_id, checkout_id);
    }
    if let Some(snapshot_id) = &source.snapshot_id {
        doc.add_text(f.knowledge_snapshot_id, snapshot_id);
    }
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

pub fn apply_knowledge_replace(
    writer: &mut IndexWriter,
    fields: FieldHandles,
    knowledge_path: &Path,
    documents: &[KnowledgeIndexDocument],
) -> Result<()> {
    writer.delete_term(Term::from_field_text(fields.doc_type, "knowledge"));
    for document in documents
        .iter()
        .filter(|document| indexable_knowledge_entry(&document.entry))
    {
        writer.add_document(build_knowledge_index_doc(document, knowledge_path, fields))?;
    }
    Ok(())
}

/// Replace every published or provisional document for one logical knowledge
/// ref while leaving unrelated knowledge documents untouched.
pub fn apply_knowledge_logical_replace(
    writer: &mut IndexWriter,
    fields: FieldHandles,
    knowledge_path: &Path,
    logical_ref: &str,
    documents: &[KnowledgeIndexDocument],
) -> Result<()> {
    writer.delete_term(Term::from_field_text(fields.logical_ref, logical_ref));
    for document in documents
        .iter()
        .filter(|document| document.logical_ref == logical_ref)
        .filter(|document| indexable_knowledge_entry(&document.entry))
    {
        writer.add_document(build_knowledge_index_doc(document, knowledge_path, fields))?;
    }
    Ok(())
}

/// Replace every published or provisional document for one managed project
/// scope while leaving global knowledge and other repositories untouched.
/// This is the convergence operation used when a pinned publisher ref moves.
pub fn apply_knowledge_scope_replace(
    writer: &mut IndexWriter,
    fields: FieldHandles,
    knowledge_path: &Path,
    scope_hash: &str,
    documents: &[KnowledgeIndexDocument],
) -> Result<()> {
    writer.delete_term(Term::from_field_text(
        fields.knowledge_scope_hash,
        scope_hash,
    ));
    for document in documents
        .iter()
        .filter(|document| document.scope_hash.as_deref() == Some(scope_hash))
        .filter(|document| indexable_knowledge_entry(&document.entry))
    {
        writer.add_document(build_knowledge_index_doc(document, knowledge_path, fields))?;
    }
    Ok(())
}

pub fn indexable_knowledge_entry(entry: &KnowledgeEntry) -> bool {
    // Superseded entries remain searchable so history queries can find the
    // original decision; H1 rerank should downweight them by status.
    matches!(entry.status, Status::Active | Status::Superseded)
}

// executes inside the IndexWriterActor pass (sanctioned single-writer).
#[allow(clippy::disallowed_methods)]
pub fn reindex_knowledge_store_standalone(
    knowledge_path: &Path,
    projects_path: &Path,
    fields: FieldHandles,
    writer: &mut IndexWriter,
    meta: &mut HashMap<String, FileMeta>,
) -> Result<u64> {
    // Reindex owns the published generation only. Provisional documents are
    // reconstructed from live checkout overlays and must survive an unrelated
    // committed-store pass.
    writer.delete_term(Term::from_field_text(
        fields.knowledge_visibility,
        "published",
    ));
    // Central knowledge contributes globals and legacy, non-repo-owned
    // projects. Registered project scopes are replaced below from their
    // committed pinned publisher tree, never from working-tree bytes or a
    // legacy central project copy.
    let knowledge = Knowledge::open(knowledge_path)?;
    let projects =
        crate::projects::ProjectRegistry::load_records(projects_path).unwrap_or_default();
    // Before the migration cut, a registered project without recorded durable
    // identity still publishes through its legacy central entry lane. Exclude
    // only scopes whose committed publisher lane can replace those bytes. The
    // cut gate separately guarantees that no legacy central entries remain, so
    // this compatibility rule becomes inert after migration.
    let managed_paths = projects
        .iter()
        .filter(|project| {
            crate::publisher::project_published_scope(
                project,
                bbox_config::config::read_repo_id_inputs,
            )
            .is_some()
        })
        .map(|project| project.canonical_path.as_str())
        .collect::<BTreeSet<_>>();
    let mut documents = knowledge
        .all_entries()
        .iter()
        .filter(|entry| {
            !entry
                .project
                .as_deref()
                .is_some_and(|project| managed_paths.contains(project))
        })
        .cloned()
        .map(KnowledgeIndexDocument::published)
        .collect::<Vec<_>>();

    let mut publishers = BTreeMap::<PublishedScope, Vec<_>>::new();
    for project in &projects {
        let Some(scope) = crate::publisher::project_published_scope(
            project,
            bbox_config::config::read_repo_id_inputs,
        ) else {
            continue;
        };
        publishers.entry(scope).or_default().push(project);
    }
    let refs_path = projects_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("publisher-refs.json");
    let mut refs = crate::publisher::PublisherRefStore::open(refs_path)?;
    for (scope, claiming) in publishers {
        if claiming.len() != 1 {
            tracing::warn!(
                scope = ?scope,
                publishers = claiming.len(),
                "knowledge reindex omitted duplicate publisher scope"
            );
            continue;
        }
        let project = claiming[0];
        let root = Path::new(&project.canonical_path);
        let pin = match refs.ensure_pinned(&scope, root) {
            Ok(pin) => pin,
            Err(err) => {
                tracing::warn!(scope = ?scope, error = %err, "knowledge reindex omitted unpinned scope");
                continue;
            }
        };
        let Some(commit) = bbox_corpus_core::git::resolve_commit(root, &pin.branch_ref) else {
            tracing::warn!(
                scope = ?scope,
                published_ref = %pin.branch_ref,
                "knowledge reindex omitted unresolved publisher ref"
            );
            continue;
        };
        let pinned_inputs = match bbox_config::config::read_repo_id_inputs_at_ref(root, &commit) {
            Ok(inputs) => inputs,
            Err(err) => {
                tracing::warn!(
                    scope = ?scope,
                    published_ref = %pin.branch_ref,
                    error = %err,
                    "knowledge reindex omitted publisher with unreadable committed authority"
                );
                continue;
            }
        };
        if crate::publisher::project_published_scope(project, |_| pinned_inputs.clone()).as_ref()
            != Some(&scope)
        {
            tracing::warn!(
                scope = ?scope,
                published_ref = %pin.branch_ref,
                "knowledge reindex omitted publisher whose pinned config changed scope"
            );
            continue;
        }
        let published = match load_published_snapshot(
            root,
            &commit,
            &scope,
            &project.canonical_path,
        ) {
            Ok(published) => published,
            Err(err) => {
                tracing::warn!(scope = ?scope, error = %err, "knowledge reindex omitted unreadable published scope");
                continue;
            }
        };
        let scope_hash = published_scope_hash(&scope);
        documents.extend(published.entries.into_values().map(|published_entry| {
            let logical_ref = knowledge_entity_id(&published_entry.entry.id);
            KnowledgeIndexDocument {
                entity_id: logical_ref.clone(),
                logical_ref,
                entry: published_entry.entry,
                visibility: "published".into(),
                scope_hash: Some(scope_hash.clone()),
                checkout_id: None,
                snapshot_id: Some(published.publisher_commit.clone()),
            }
        }));
    }
    let mut docs = 0;
    for document in documents
        .iter()
        .filter(|document| indexable_knowledge_entry(&document.entry))
    {
        writer.add_document(build_knowledge_index_doc(document, knowledge_path, fields))?;
        docs += 1;
    }
    match file_meta(knowledge_path) {
        Some(file_meta) => {
            meta.insert(knowledge_path.to_string_lossy().to_string(), file_meta);
        }
        // Central absent: clear any stale meta entry so its disappearance does
        // not wrongly suppress future reindex passes.
        None => {
            meta.remove(knowledge_path.to_string_lossy().as_ref());
        }
    }
    Ok(docs)
}

/// Apply a knowledge upsert to an already-held writer (no commit). The
/// IndexWriterActor is the production caller; it batches ops and commits once.
pub fn apply_knowledge_upsert(
    writer: &mut IndexWriter,
    fields: FieldHandles,
    knowledge_path: &Path,
    entry: &KnowledgeEntry,
) -> Result<()> {
    let entity_id = knowledge_entity_id(&entry.id);
    writer.delete_term(Term::from_field_text(fields.entity_id, &entity_id));
    if indexable_knowledge_entry(entry) {
        writer.add_document(build_knowledge_doc(entry, knowledge_path, fields))?;
    }
    Ok(())
}

/// Apply a knowledge delete to an already-held writer (no commit).
pub fn apply_knowledge_delete(
    writer: &mut IndexWriter,
    fields: FieldHandles,
    entry_id: &str,
) -> Result<()> {
    writer.delete_term(Term::from_field_text(
        fields.entity_id,
        &knowledge_entity_id(entry_id),
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
    use bbox_knowledge::knowledge::{Approval, Category, Priority, Scope};

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
        use bbox_knowledge::knowledge::KnowledgeStore;
        use tantivy::Index;

        let central = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let repo_root = repo.path().canonicalize().unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo_root)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        std::fs::write(repo_root.join("README.md"), "seed\n").unwrap();
        git(&["add", "README.md"]);
        git(&["commit", "-q", "-m", "seed"]);
        bbox_config::config::ensure_recorded_repo_id(&repo_root).unwrap();

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
        git(&["add", ".bbox"]);
        git(&["commit", "-q", "-m", "knowledge"]);

        // Empty central store + a sibling projects.json registering the repo.
        let kb_path = central.path().join("kb.json");
        std::fs::write(
            &kb_path,
            serde_json::to_string(&KnowledgeStore {
                version: 1,
                built_from: Default::default(),
                provenance: Default::default(),
                entries: vec![],
            })
            .unwrap(),
        )
        .unwrap();
        let projects_path = central.path().join("projects.json");
        {
            let mut reg = crate::projects::ProjectRegistry::open(&projects_path).unwrap();
            reg.register_path(&repo_root).unwrap();
            bbox_corpus_core::json_store::atomic_write_json_locked(
                &projects_path,
                &<crate::projects::ProjectRegistry as bbox_stores::store_persister::StoreSnapshot>::snapshot(&reg)
                    .unwrap(),
            )
            .unwrap();
        }

        let (schema, fields) = crate::index::build_schema();
        let index = Index::create_in_ram(schema);
        let mut writer: IndexWriter = index.writer(15_000_000).unwrap();
        let mut meta = HashMap::new();
        let docs = reindex_knowledge_store_standalone(
            &kb_path,
            &projects_path,
            fields,
            &mut writer,
            &mut meta,
        )
        .unwrap();

        assert_eq!(
            docs, 1,
            "the committed project .bbox/knowledge entry must be indexed even though central is empty"
        );
    }

    #[test]
    fn reindex_keeps_legacy_registered_scope_before_identity_migration() {
        use bbox_knowledge::knowledge::KnowledgeStore;
        use tantivy::Index;

        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("legacy-project");
        std::fs::create_dir_all(&project).unwrap();
        let project = project.canonicalize().unwrap();
        let entry = KnowledgeEntry {
            id: "legacy01".into(),
            title: "legacy project knowledge".into(),
            content: "LEGACY_SCOPE_STAYS_SEARCHABLE".into(),
            cluster: None,
            variants: Default::default(),
            category: Category::Convention,
            scope: Scope::Project,
            project: Some(project.to_string_lossy().into_owned()),
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
        let knowledge_path = temp.path().join("knowledge.json");
        std::fs::write(
            &knowledge_path,
            serde_json::to_vec_pretty(&KnowledgeStore {
                version: 1,
                built_from: Default::default(),
                provenance: Default::default(),
                entries: vec![entry],
            })
            .unwrap(),
        )
        .unwrap();
        let projects_path = temp.path().join("projects.json");
        let mut registry = crate::projects::ProjectRegistry::open(&projects_path).unwrap();
        registry.register_path(&project).unwrap();
        bbox_corpus_core::json_store::atomic_write_json_locked(
            &projects_path,
            &<crate::projects::ProjectRegistry as bbox_stores::store_persister::StoreSnapshot>::snapshot(
                &registry,
            )
            .unwrap(),
        )
        .unwrap();

        let (schema, fields) = crate::index::build_schema();
        let index = Index::create_in_ram(schema);
        let mut writer: IndexWriter = index.writer(15_000_000).unwrap();
        let docs = reindex_knowledge_store_standalone(
            &knowledge_path,
            &projects_path,
            fields,
            &mut writer,
            &mut HashMap::new(),
        )
        .unwrap();

        assert_eq!(docs, 1);
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
