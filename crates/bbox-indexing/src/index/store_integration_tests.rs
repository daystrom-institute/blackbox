//! Store-coupled index integration tests: exercises the engine
//! (TranscriptIndex, writer actor, build_index) against the daemon-side
//! stores (Knowledge) and the registered store hooks. Lives daemon-side
//! because `cfg(test)` and store types do not cross the engine crate
//! boundary.

use super::writer_actor::IndexWriteOp;
use super::{SearchParams, TranscriptIndex};

#[test]
fn delete_knowledge_entry_removes_tantivy_doc() {
    let dir = tempfile::tempdir().unwrap();
    let index_path = dir.path().join("index");
    let knowledge_path = dir.path().join("knowledge.json");
    let index = TranscriptIndex::open_or_create(
        &index_path,
        Vec::new(),
        None,
        dir.path().join("projects.json"),
        knowledge_path.clone(),
        dir.path().join("threads.json"),
        dir.path().join("roadmap.json"),
    )
    .unwrap();
    let entry = bbox_knowledge::knowledge::KnowledgeEntry {
        id: "abc12345".into(),
        title: "Delete fixture".into(),
        content: "tombstone searchable knowledge phrase".into(),
        cluster: None,
        variants: Default::default(),
        category: bbox_knowledge::knowledge::Category::Memory,
        scope: bbox_knowledge::knowledge::Scope::Global,
        project: None,
        providers: Vec::new(),
        priority: bbox_knowledge::knowledge::Priority::Standard,
        weight: 100,
        status: bbox_knowledge::knowledge::Status::Active,
        approval: bbox_knowledge::knowledge::Approval::UserConfirmed,
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

    let actor = super::writer_actor::IndexWriterActor::spawn_for(&index);
    actor.enqueue(IndexWriteOp::UpsertKnowledge(Box::new(entry)));
    actor.flush_blocking().unwrap();
    let hits = index
        .search(&SearchParams {
            query: "tombstone searchable".into(),
            mode: None,
            account: None,
            project: None,
            role: None,
            include_subagents: None,
            limit: Some(5),
            exclude_self: None,
        })
        .unwrap();
    assert!(hits.contains("tombstone"), "{hits}");
    assert!(hits.contains("searchable"), "{hits}");

    actor.enqueue(IndexWriteOp::DeleteKnowledge("abc12345".to_string()));
    actor.flush_blocking().unwrap();
    let hits = index
        .search(&SearchParams {
            query: "tombstone searchable".into(),
            mode: None,
            account: None,
            project: None,
            role: None,
            include_subagents: None,
            limit: Some(5),
            exclude_self: None,
        })
        .unwrap();
    assert!(
        hits == "No results found." || hits == "Index is empty. Run blackbox_reindex first.",
        "{hits}"
    );
}

#[test]
fn knowledge_entries_are_searchable_after_reindex() {
    // build_index reconciles store docs through the daemon-registered
    // pass; tests that drive it directly must wire the hooks first.
    super::writer_actor::register_index_store_hooks();
    let dir = tempfile::tempdir().unwrap();
    let knowledge_path = dir.path().join("knowledge.json");
    let mut knowledge = bbox_knowledge::knowledge::Knowledge::open(&knowledge_path).unwrap();
    knowledge
        .remember(
            &bbox_knowledge::knowledge::RememberParams {
                content: "durable zebra phrase for knowledge indexing".into(),
                category: None,
                title: Some("Knowledge indexing fixture".into()),
                scope: None,
                project: None,
                decay: None,
                review_at: None,
                expires_at: None,
            },
            false,
        )
        .unwrap();
    bbox_corpus_core::json_store::atomic_write_json_locked(
            &knowledge_path,
            &<bbox_knowledge::knowledge::Knowledge as bbox_stores::store_persister::StoreSnapshot>::snapshot(
                &knowledge,
            )
            .unwrap(),
        )
        .unwrap();

    let mut index = TranscriptIndex::open_or_create(
        &dir.path().join("index"),
        Vec::new(),
        None,
        dir.path().join("projects.json"),
        knowledge_path,
        dir.path().join("threads.json"),
        dir.path().join("roadmap.json"),
    )
    .unwrap();
    index.build_index(false).unwrap();

    let hits = index
        .search(&SearchParams {
            query: "durable zebra phrase".into(),
            mode: None,
            account: None,
            project: None,
            role: None,
            include_subagents: None,
            limit: Some(5),
            exclude_self: None,
        })
        .unwrap();
    assert!(hits.contains("durable"), "{hits}");
    assert!(hits.contains("zebra"), "{hits}");
    assert!(hits.contains("phrase"), "{hits}");
}
