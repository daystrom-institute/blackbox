use super::*;
use crate::knowledge::KnowledgeEntry;
use crate::server::state::catalog_fixture::{CatalogFixture, knowledge_entry};
use bbox_corpus_core::identity::PublishedScope;

const PROJECT: &str = "p_knowledge_queue";
const ENTRY: &str = "1234567890abcdef";

fn cover_catalog_projects(server: &mut BlackboxServer) {
    use bbox_indexing::knowledge_transport_cutover::{
        KnowledgeTransportCutoverMarkerV1, KnowledgeTransportCutoverRuntimeV1,
        PredictedKnowledgeTransportCutoverRowV1,
    };
    use bbox_indexing::project_catalog_inventory::Sha256ValueV1;

    let mut rows = server
        .catalog_published_targets(None)
        .unwrap()
        .into_iter()
        .map(|target| PredictedKnowledgeTransportCutoverRowV1 {
            project_id: target.project_id,
            scope: target.catalog_scope.unwrap(),
            producer_id: "producer".into(),
            grant_commitment: Sha256ValueV1::digest(b"grant"),
            accepted_generation_id: "a".repeat(64),
            accepted_generation_sha256: "b".repeat(64),
            accepted_pointer_sha256: "c".repeat(64),
            source_generation_id: format!("kps_{}", "d".repeat(64)),
            source_generation_sha256: "e".repeat(64),
            publication_parity_commitment: Sha256ValueV1::digest(b"publication"),
            parity_workspace_ids: Vec::new(),
            workspace_parity_commitment: Sha256ValueV1::digest(b"workspace"),
            shadow_observation_commitment: Sha256ValueV1::digest(b"shadow"),
            capability_baselines: Vec::new(),
            observation_window_start_sequence: 0,
            observation_window_end_sequence: 0,
        })
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| left.project_id.cmp(&right.project_id));
    let marker = KnowledgeTransportCutoverMarkerV1 {
        version: 1,
        applied_at: "unix:1".into(),
        report_artifact_hash: Sha256ValueV1::digest(b"report"),
        resolution_artifact_hash: Sha256ValueV1::digest(b"resolution"),
        predecessor_marker_checksum: None,
        predecessor_catalog_epoch: 1,
        inventory_hash: Sha256ValueV1::digest(b"inventory"),
        observation_snapshot_hash: Sha256ValueV1::digest(b"observations"),
        rows,
        checksum_sha256: Sha256ValueV1::digest(b"test fixture bypasses marker decoding"),
    };
    std::sync::Arc::get_mut(&mut server.state)
        .unwrap()
        .knowledge_transport_cutover = std::sync::Arc::new(
        KnowledgeTransportCutoverRuntimeV1::from_marker(Some(marker)),
    );
}

fn queue_server(fixture: &CatalogFixture) -> BlackboxServer {
    let mut server = fixture.server();
    cover_catalog_projects(&mut server);
    server
}

fn publish(
    fixture: &CatalogFixture,
    server: &BlackboxServer,
    scope: &PublishedScope,
    commit: &str,
    entries: &[KnowledgeEntry],
) {
    fixture.install_publication(PROJECT, scope, &commit.repeat(40), entries, &[]);
    server.invalidate_catalog_published_content(
        &bbox_corpus_core::project_catalog::ProjectId::parse(PROJECT).unwrap(),
    );
}

fn published_fixture() -> (CatalogFixture, PublishedScope) {
    let fixture = CatalogFixture::new();
    let scope = CatalogFixture::scope(".");
    fixture.add_published_project(PROJECT, &scope);
    fixture.install_publication(
        PROJECT,
        &scope,
        &"1".repeat(40),
        &[knowledge_entry(ENTRY, "published")],
        &[],
    );
    (fixture, scope)
}

fn fixture() -> (CatalogFixture, BlackboxServer, PublishedScope) {
    let (fixture, scope) = published_fixture();
    let server = queue_server(&fixture);
    (fixture, server, scope)
}

fn learn(id: Option<&str>, content: &str) -> LearnParams {
    LearnParams {
        render_placement: None,
        id: id.map(str::to_string),
        content: content.into(),
        category: "convention".into(),
        scope: Some("project".into()),
        project: Some(PROJECT.into()),
        ..Default::default()
    }
}

/// An id-addressed update through the checkout owner: the entry's owner
/// comes from the served entry, not a project selector.
fn update_by_id(
    server: &BlackboxServer,
    id: &str,
    content: &str,
) -> anyhow::Result<Option<(String, String)>> {
    let mut params = learn(Some(id), content);
    params.project = None;
    server.enqueue_learn_update_by_id_via_checkout_owner(&params, id)
}

fn latest(server: &BlackboxServer, scope: &PublishedScope, id: &str) -> KnowledgeEntry {
    let queue = server.state.checkout_mutations.read();
    let row = queue
        .outstanding_writes()
        .filter(|row| {
            &row.mutation.scope == scope
                && row.mutation.relative_path == format!(".bbox/knowledge/{id}.json")
        })
        .last()
        .unwrap();
    serde_json::from_str(row.mutation.content_json.as_deref().unwrap()).unwrap()
}

#[tokio::test]
async fn queued_knowledge_edits_compose_before_and_after_delivery_and_publication() {
    let (fixture, server, scope) = fixture();
    let result = server
        .bbox_learn(Parameters(learn(Some(ENTRY), "queued content")))
        .await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    let restarted = queue_server(&fixture);
    assert_eq!(latest(&restarted, &scope, ENTRY).content, "queued content");
    let mut params = learn(Some(ENTRY), "queued content");
    params.render = Some(false);
    let result = server.bbox_learn(Parameters(params)).await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    let queued = latest(&fixture.server(), &scope, ENTRY);
    assert_eq!(queued.content, "queued content");
    assert!(!queued.render);
    let rows = server
        .state
        .checkout_mutations
        .read()
        .poll(&BTreeSet::from([scope.clone()]), false)
        .mutations;
    for row in rows {
        server
            .state
            .checkout_mutations
            .write()
            .ack(
                &row.mutation_id,
                "applied",
                None,
                None,
                "2026-09-06T00:00:00Z",
            )
            .unwrap();
    }
    update_by_id(&server, ENTRY, "delivered content")
        .unwrap()
        .unwrap();
    let entry = latest(&server, &scope, ENTRY);
    assert_eq!(entry.content, "delivered content");
    assert!(!entry.render, "an update composes on the delivered write");
    publish(&fixture, &server, &scope, "2", &[entry.clone()]);
    server
        .session_knowledge_view(Some(PROJECT), Some("published"))
        .unwrap();
    assert_eq!(
        server
            .state
            .checkout_mutations
            .read()
            .outstanding_intents()
            .count(),
        0
    );
    let mut external = entry;
    external.content = "later publication".into();
    external.render = true;
    publish(&fixture, &server, &scope, "3", &[external]);
    update_by_id(&server, ENTRY, "after later publication")
        .unwrap()
        .unwrap();
    let updated = latest(&server, &scope, ENTRY);
    assert_eq!(updated.content, "after later publication");
    assert!(
        updated.render,
        "the update starts from the later publication"
    );
}

#[tokio::test]
async fn queued_knowledge_preserves_the_complete_canonical_entry() {
    let (fixture, server, scope) = fixture();
    let mut entry = knowledge_entry(ENTRY, "complete");
    entry.project_id = Some(PROJECT.into());
    entry.cluster = Some("cluster".into());
    entry.providers = vec!["provider".into()];
    entry.priority = crate::knowledge::Priority::Critical;
    entry.render = false;
    entry.render_placement = crate::knowledge::RenderPlacement::Satellite {
        topic: crate::knowledge::GuidanceTopic::Operations,
    };
    publish(&fixture, &server, &scope, "2", &[entry.clone()]);
    server
        .mutate_queued_knowledge(PROJECT, scope.clone(), "touch", |transaction| {
            let mut entry = transaction.entry(ENTRY)?;
            entry.updated_at = bbox_util::util::now_iso();
            transaction.stage(&entry, false)?;
            Ok("touched".into())
        })
        .unwrap();
    let updated = latest(&server, &scope, ENTRY);
    entry.updated_at = updated.updated_at.clone();
    assert_eq!(
        crate::knowledge::committed_knowledge_entry_bytes(&updated).unwrap(),
        crate::knowledge::committed_knowledge_entry_bytes(&entry).unwrap()
    );
}

/// An id-addressed update that omits `render_placement` or `render` keeps
/// the published entry's values, and one that passes them changes them.
#[tokio::test]
async fn queued_knowledge_update_by_id_preserves_omitted_render_fields() {
    use crate::knowledge::{GuidanceTopic, RenderPlacement};

    let (fixture, server, scope) = fixture();
    let mut entry = knowledge_entry(ENTRY, "satellite");
    entry.project_id = Some(PROJECT.into());
    entry.render = false;
    entry.render_placement = RenderPlacement::Satellite {
        topic: GuidanceTopic::Operations,
    };
    publish(&fixture, &server, &scope, "2", &[entry.clone()]);

    update_by_id(&server, ENTRY, "omitted fields")
        .unwrap()
        .unwrap();
    let updated = latest(&server, &scope, ENTRY);
    assert_eq!(updated.content, "omitted fields");
    assert_eq!(updated.render_placement, entry.render_placement);
    assert!(!updated.render);
    let row = server
        .state
        .checkout_mutations
        .read()
        .outstanding_writes()
        .last()
        .unwrap()
        .mutation
        .content_json
        .clone()
        .unwrap();
    assert!(
        row.contains("\"render_placement\""),
        "the delivered file keeps its placement: {row}"
    );

    let mut params = learn(Some(ENTRY), "passed fields");
    params.project = None;
    params.render = Some(true);
    params.render_placement = Some(RenderPlacement::Satellite {
        topic: GuidanceTopic::Build,
    });
    server
        .enqueue_learn_update_by_id_via_checkout_owner(&params, ENTRY)
        .unwrap()
        .unwrap();
    let changed = latest(&server, &scope, ENTRY);
    assert_eq!(
        changed.render_placement,
        RenderPlacement::Satellite {
            topic: GuidanceTopic::Build
        }
    );
    assert!(changed.render);

    params.content = "back inline".into();
    params.render = None;
    params.render_placement = Some(RenderPlacement::Inline);
    server
        .enqueue_learn_update_by_id_via_checkout_owner(&params, ENTRY)
        .unwrap()
        .unwrap();
    let inline = latest(&server, &scope, ENTRY);
    assert!(inline.render_placement.is_inline());
    assert!(inline.render, "an omitted render keeps the queued value");
}

#[tokio::test]
async fn queued_knowledge_refuses_publication_conflicts_and_retries_capture_races() {
    let (fixture, server, scope) = fixture();
    let mut captures = 0;
    server
        .mutate_queued_knowledge_with_snapshot_hook(
            PROJECT,
            scope.clone(),
            "race",
            || {
                captures += 1;
                if captures == 1 {
                    publish(
                        &fixture,
                        &server,
                        &scope,
                        "2",
                        &[knowledge_entry(ENTRY, "changed during capture")],
                    );
                }
            },
            |transaction| {
                let mut entry = transaction.entry(ENTRY)?;
                assert_eq!(entry.content, "changed during capture");
                entry.title = "queued title".into();
                transaction.stage(&entry, false)?;
                Ok("updated".into())
            },
        )
        .unwrap();
    assert_eq!(captures, 2);
    publish(
        &fixture,
        &server,
        &scope,
        "3",
        &[knowledge_entry(ENTRY, "conflicting publication")],
    );
    let count = server.state.checkout_mutations.read().pending_count();
    let error = update_by_id(&server, ENTRY, "conflicting update").unwrap_err();
    assert!(
        error.to_string().contains("checkout_mutation_conflict"),
        "{error:#}"
    );
    assert_eq!(
        server.state.checkout_mutations.read().pending_count(),
        count
    );
}

#[tokio::test]
async fn queued_knowledge_delete_is_a_tombstone_until_publication() {
    let (fixture, server, scope) = fixture();
    server
        .enqueue_learn_via_checkout_owner(
            &learn(Some(ENTRY), "queued"),
            PROJECT,
            PROJECT,
            scope.clone(),
        )
        .unwrap();
    let result = server
        .bbox_forget(Parameters(ForgetParams {
            project: None,
            id: ENTRY.into(),
        }))
        .await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    let restarted = queue_server(&fixture);
    let rows = restarted
        .state
        .checkout_mutations
        .read()
        .poll(&BTreeSet::from([scope.clone()]), false)
        .mutations;
    assert_eq!(rows.last().unwrap().mode, "delete");
    for row in rows {
        restarted
            .state
            .checkout_mutations
            .write()
            .ack(
                &row.mutation_id,
                "applied",
                None,
                None,
                "2026-09-06T00:00:00Z",
            )
            .unwrap();
    }
    assert!(update_by_id(&restarted, ENTRY, "resurrect by id").is_err());
    assert!(
        restarted
            .enqueue_learn_via_checkout_owner(
                &learn(Some(ENTRY), "resurrect"),
                PROJECT,
                PROJECT,
                scope.clone()
            )
            .is_err()
    );
    publish(&fixture, &restarted, &scope, "2", &[]);
    restarted
        .session_knowledge_view(Some(PROJECT), Some("published"))
        .unwrap();
    assert_eq!(
        restarted
            .state
            .checkout_mutations
            .read()
            .outstanding_intents()
            .count(),
        0
    );
}

#[tokio::test]
async fn queued_knowledge_scope_and_id_ambiguity_never_cross_project_boundaries() {
    let (fixture, scope) = published_fixture();
    let other_scope = CatalogFixture::scope("nested");
    fixture.add_published_project("p_other", &other_scope);
    fixture.install_publication(
        "p_other",
        &other_scope,
        &"2".repeat(40),
        &[knowledge_entry(ENTRY, "other")],
        &[],
    );
    let server = queue_server(&fixture);
    assert_eq!(
        server.covered_scope_for_project_id("p_other"),
        Some(other_scope.clone()),
    );
    server
        .enqueue_learn_via_checkout_owner(
            &learn(Some(ENTRY), "first scope"),
            PROJECT,
            PROJECT,
            scope.clone(),
        )
        .unwrap();
    server
        .enqueue_learn_via_checkout_owner(
            &learn(Some(ENTRY), "second scope"),
            "p_other",
            "p_other",
            other_scope.clone(),
        )
        .unwrap();
    assert_eq!(latest(&server, &scope, ENTRY).content, "first scope");
    assert_eq!(latest(&server, &other_scope, ENTRY).content, "second scope");
    assert!(
        update_by_id(&server, ENTRY, "ambiguous owner")
            .unwrap_err()
            .to_string()
            .contains("multiple projects")
    );
    let count = server.state.checkout_mutations.read().pending_count();
    assert!(
        server
            .mutate_queued_knowledge(PROJECT, other_scope, "wrong scope", |_| {
                panic!("a wrong scope must fail before the edit")
            })
            .is_err()
    );
    assert_eq!(
        server.state.checkout_mutations.read().pending_count(),
        count
    );
}

#[tokio::test]
async fn queued_knowledge_oversized_content_is_refused_before_admission() {
    let (_fixture, server, scope) = fixture();
    let oversized = "x".repeat(bbox_code_source::MAX_CHECKOUT_MUTATION_CONTENT_BYTES + 1);
    assert!(
        server
            .enqueue_learn_via_checkout_owner(
                &learn(None, &oversized),
                PROJECT,
                PROJECT,
                scope.clone()
            )
            .is_err()
    );
    assert!(
        server
            .enqueue_learn_via_checkout_owner(
                &learn(Some(ENTRY), &oversized),
                PROJECT,
                PROJECT,
                scope.clone()
            )
            .is_err()
    );
    assert_eq!(server.state.checkout_mutations.read().pending_count(), 0);
    server
        .enqueue_learn_via_checkout_owner(
            &learn(Some(ENTRY), "bounded"),
            PROJECT,
            PROJECT,
            scope.clone(),
        )
        .unwrap();
    assert_eq!(latest(&server, &scope, ENTRY).content, "bounded");
    assert_eq!(server.state.checkout_mutations.read().pending_count(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_knowledge_concurrent_creates_mint_distinct_records() {
    let (_fixture, server, scope) = fixture();
    let mut tasks = Vec::new();
    for index in 0..12 {
        let server = server.clone();
        let scope = scope.clone();
        tasks.push(tokio::task::spawn_blocking(move || {
            server
                .enqueue_learn_via_checkout_owner(
                    &learn(None, &format!("concurrent {index}")),
                    PROJECT,
                    PROJECT,
                    scope,
                )
                .unwrap();
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    let paths = server
        .state
        .checkout_mutations
        .read()
        .outstanding_writes()
        .filter(|row| row.mutation.scope == scope)
        .map(|row| row.mutation.relative_path.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(paths.len(), 12);
    assert_eq!(server.state.checkout_mutations.read().pending_count(), 12);
}

#[tokio::test]
async fn queued_knowledge_genesis_and_id_addressed_updates_survive_restart() {
    let fixture = CatalogFixture::new();
    let scope = CatalogFixture::scope(".");
    fixture.add_published_project(PROJECT, &scope);
    let server = queue_server(&fixture);
    let result = server.bbox_learn(Parameters(learn(None, "genesis"))).await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    let id = server
        .state
        .checkout_mutations
        .read()
        .outstanding_writes()
        .next()
        .unwrap()
        .mutation
        .relative_path
        .clone();
    let id = id
        .trim_start_matches(".bbox/knowledge/")
        .trim_end_matches(".json");
    let mut update = learn(Some(id), "id addressed");
    update.project = None;
    let result = server.bbox_learn(Parameters(update)).await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    assert_eq!(
        latest(&fixture.server(), &scope, id).content,
        "id addressed"
    );
    let result = server
        .bbox_forget(Parameters(ForgetParams {
            project: None,
            id: id.into(),
        }))
        .await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    assert!(update_by_id(&queue_server(&fixture), id, "after delete").is_err());
}

#[tokio::test]
async fn queued_knowledge_durability_failure_never_returns_success() {
    let (_fixture, mut server, _scope) = fixture();
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let state = std::sync::Arc::get_mut(&mut server.state).unwrap();
    state.checkout_mutations_persister = crate::store_persister::StorePersister::spawn(
        "knowledge-queue-failure",
        state.checkout_mutations.clone(),
        root,
    );
    let result = server
        .bbox_learn(Parameters(learn(Some(ENTRY), "accepted but not durable")))
        .await;
    assert_eq!(result.is_error, Some(true));
    assert!(format!("{result:?}").contains("durability failed"));
}

#[tokio::test]
async fn queued_knowledge_unrelated_broken_publication_preserves_known_global_authority() {
    let (fixture, scope) = published_fixture();
    let broken_scope = CatalogFixture::scope("broken");
    fixture.add_published_project("p_broken", &broken_scope);
    let publication = fixture.install_publication(
        "p_broken",
        &broken_scope,
        &"2".repeat(40),
        &[knowledge_entry("2222222222222222", "broken")],
        &[],
    );
    fixture.corrupt_generation("p_broken", &publication.generation_id);
    let server = queue_server(&fixture);
    assert_eq!(
        server.covered_scope_for_project_id("p_broken"),
        Some(broken_scope),
    );
    let global = server
        .state
        .kb
        .write()
        .learn_result_with_checkout(
            &LearnParams {
                render_placement: None,
                content: "global rule".into(),
                category: "convention".into(),
                scope: Some("global".into()),
                ..Default::default()
            },
            None,
            None,
        )
        .unwrap();
    let result = server
        .bbox_learn(Parameters(LearnParams {
            id: Some(global.id.clone()),
            content: "global rule updated".into(),
            category: "convention".into(),
            scope: Some("global".into()),
            ..Default::default()
        }))
        .await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    assert_eq!(server.state.checkout_mutations.read().pending_count(), 0);
    server
        .enqueue_learn_via_checkout_owner(
            &learn(Some(ENTRY), "explicit healthy project"),
            PROJECT,
            PROJECT,
            scope.clone(),
        )
        .unwrap();
    assert_eq!(
        latest(&server, &scope, ENTRY).content,
        "explicit healthy project"
    );
    assert!(update_by_id(&server, "unknown", "unknown owner").is_err());
}

#[tokio::test]
async fn queued_knowledge_genesis_delete_does_not_retire_on_preexisting_absence() {
    let fixture = CatalogFixture::new();
    let scope = CatalogFixture::scope(".");
    fixture.add_published_project(PROJECT, &scope);
    let server = queue_server(&fixture);
    server
        .enqueue_learn_via_checkout_owner(&learn(None, "genesis"), PROJECT, PROJECT, scope.clone())
        .unwrap();
    let created: KnowledgeEntry = {
        let queue = server.state.checkout_mutations.read();
        serde_json::from_str(
            queue
                .outstanding_writes()
                .next()
                .unwrap()
                .mutation
                .content_json
                .as_deref()
                .unwrap(),
        )
        .unwrap()
    };
    server
        .enqueue_forget_via_checkout_owner(&ForgetParams {
            project: None,
            id: created.id.clone(),
        })
        .unwrap();
    publish(&fixture, &server, &scope, "1", &[]);
    server
        .session_knowledge_view(Some(PROJECT), Some("published"))
        .unwrap();
    assert_eq!(
        server
            .state
            .checkout_mutations
            .read()
            .outstanding_intents()
            .count(),
        2
    );
    assert!(update_by_id(&server, &created.id, "after delete").is_err());
    publish(&fixture, &server, &scope, "2", &[created.clone()]);
    server
        .session_knowledge_view(Some(PROJECT), Some("published"))
        .unwrap();
    assert_eq!(
        server
            .state
            .checkout_mutations
            .read()
            .outstanding_intents()
            .count(),
        1
    );
    assert!(update_by_id(&server, &created.id, "after delete").is_err());
    let rows = server
        .state
        .checkout_mutations
        .read()
        .poll(&BTreeSet::from([scope.clone()]), false)
        .mutations;
    for row in rows {
        server
            .state
            .checkout_mutations
            .write()
            .ack(
                &row.mutation_id,
                "applied",
                None,
                None,
                "2026-09-06T00:00:00Z",
            )
            .unwrap();
    }
    publish(&fixture, &server, &scope, "3", &[]);
    server
        .session_knowledge_view(Some(PROJECT), Some("published"))
        .unwrap();
    assert_eq!(
        server
            .state
            .checkout_mutations
            .read()
            .outstanding_intents()
            .count(),
        0
    );
}

#[tokio::test]
async fn queued_knowledge_acknowledged_create_delete_survives_delayed_publication() {
    let fixture = CatalogFixture::new();
    let scope = CatalogFixture::scope(".");
    fixture.add_published_project(PROJECT, &scope);
    fixture.install_publication(PROJECT, &scope, &"1".repeat(40), &[], &[]);
    let server = queue_server(&fixture);
    let result = server
        .bbox_learn(Parameters(learn(None, "captured before deletion")))
        .await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    let create = server
        .state
        .checkout_mutations
        .read()
        .poll(&BTreeSet::from([scope.clone()]), false)
        .mutations[0]
        .clone();
    let created: KnowledgeEntry =
        serde_json::from_str(create.content_json.as_deref().unwrap()).unwrap();
    server
        .state
        .checkout_mutations
        .write()
        .ack(
            &create.mutation_id,
            "applied",
            None,
            None,
            "2026-09-06T00:00:00Z",
        )
        .unwrap();
    let result = server
        .bbox_forget(Parameters(ForgetParams {
            id: created.id.clone(),
            project: Some(PROJECT.into()),
        }))
        .await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    let delete = server
        .state
        .checkout_mutations
        .read()
        .poll(&BTreeSet::from([scope.clone()]), false)
        .mutations[0]
        .clone();
    assert_eq!(delete.mode, "delete");
    server
        .state
        .checkout_mutations
        .write()
        .ack(
            &delete.mutation_id,
            "applied",
            None,
            None,
            "2026-09-06T00:00:01Z",
        )
        .unwrap();
    server
        .state
        .persist_checkout_mutations_durable()
        .await
        .unwrap();
    drop(server);
    let server = queue_server(&fixture);
    server
        .session_knowledge_view(Some(PROJECT), Some("published"))
        .unwrap();
    assert_eq!(
        server
            .state
            .checkout_mutations
            .read()
            .outstanding_intents()
            .count(),
        2
    );
    assert!(
        server
            .enqueue_forget_via_checkout_owner(&ForgetParams {
                id: created.id.clone(),
                project: Some(PROJECT.into()),
            })
            .is_err()
    );
    publish(&fixture, &server, &scope, "2", &[created.clone()]);
    server
        .session_knowledge_view(Some(PROJECT), Some("published"))
        .unwrap();
    assert_eq!(
        server
            .state
            .checkout_mutations
            .read()
            .outstanding_intents()
            .count(),
        1
    );
    assert!(
        server
            .enqueue_forget_via_checkout_owner(&ForgetParams {
                id: created.id.clone(),
                project: Some(PROJECT.into()),
            })
            .is_err()
    );
    publish(&fixture, &server, &scope, "3", &[]);
    server
        .session_knowledge_view(Some(PROJECT), Some("published"))
        .unwrap();
    assert_eq!(
        server
            .state
            .checkout_mutations
            .read()
            .outstanding_intents()
            .count(),
        0
    );
}

#[tokio::test]
async fn queued_knowledge_explicit_owner_isolates_update_and_forget_from_broken_projects() {
    let (fixture, scope) = published_fixture();
    let broken_scope = CatalogFixture::scope("broken");
    fixture.add_published_project("p_broken", &broken_scope);
    let publication = fixture.install_publication(
        "p_broken",
        &broken_scope,
        &"2".repeat(40),
        &[knowledge_entry(ENTRY, "duplicate owner")],
        &[],
    );
    let server = queue_server(&fixture);
    assert_eq!(
        server.covered_scope_for_project_id("p_broken"),
        Some(broken_scope.clone()),
    );
    let global = server
        .state
        .kb
        .write()
        .learn_result_with_checkout(
            &LearnParams {
                render_placement: None,
                content: "global owner".into(),
                category: "convention".into(),
                scope: Some("global".into()),
                ..Default::default()
            },
            None,
            None,
        )
        .unwrap();
    assert!(
        update_by_id(&server, ENTRY, "ambiguous owner")
            .unwrap_err()
            .to_string()
            .contains("multiple projects")
    );
    fixture.corrupt_generation("p_broken", &publication.generation_id);
    server.invalidate_catalog_published_content(
        &bbox_corpus_core::project_catalog::ProjectId::parse("p_broken").unwrap(),
    );
    let mut ambiguous = learn(Some(ENTRY), "unscoped update");
    ambiguous.project = None;
    let ambiguous = server.bbox_learn(Parameters(ambiguous)).await;
    assert_eq!(ambiguous.is_error, Some(true));
    assert!(format!("{ambiguous:?}").contains("pass project"));
    assert!(
        server
            .enqueue_forget_via_checkout_owner(&ForgetParams {
                id: ENTRY.into(),
                project: None,
            })
            .is_err()
    );
    assert_eq!(server.state.checkout_mutations.read().pending_count(), 0);
    let mismatch = server
        .bbox_forget(Parameters(ForgetParams {
            id: global.id.clone(),
            project: Some(PROJECT.into()),
        }))
        .await;
    assert_eq!(mismatch.is_error, Some(true));
    let local = server
        .bbox_learn(Parameters(LearnParams {
            id: Some(global.id.clone()),
            content: "global owner updated".into(),
            category: "convention".into(),
            scope: Some("global".into()),
            ..Default::default()
        }))
        .await;
    assert_ne!(local.is_error, Some(true), "{local:?}");
    assert_eq!(server.state.checkout_mutations.read().pending_count(), 0);
    for selector in ["p_broken", "p_nonexistent"] {
        let result = server
            .bbox_forget(Parameters(ForgetParams {
                id: ENTRY.into(),
                project: Some(selector.into()),
            }))
            .await;
        assert_eq!(result.is_error, Some(true));
    }
    assert_eq!(server.state.checkout_mutations.read().pending_count(), 0);
    let result = server
        .bbox_learn(Parameters(learn(Some(ENTRY), "explicit owner update")))
        .await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    assert_eq!(
        latest(&server, &scope, ENTRY).content,
        "explicit owner update"
    );
    let result = server
        .bbox_forget(Parameters(ForgetParams {
            id: ENTRY.into(),
            project: Some(PROJECT.into()),
        }))
        .await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    assert!(
        server
            .enqueue_forget_via_checkout_owner(&ForgetParams {
                id: ENTRY.into(),
                project: Some(PROJECT.into()),
            })
            .is_err()
    );
    let restarted = queue_server(&fixture);
    let rows = restarted
        .state
        .checkout_mutations
        .read()
        .poll(&BTreeSet::from([scope.clone(), broken_scope]), false)
        .mutations;
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|row| row.scope == scope));
    assert_eq!(rows.last().unwrap().mode, "delete");
}
#[tokio::test]
async fn queued_knowledge_create_receipts_durably_admit_recall_entries_and_deletes() {
    let (fixture, server, _scope) = fixture();
    let mut recall = learn(None, "queued memory");
    recall.category = "memory".into();
    recall.render = Some(false);
    let result = server.bbox_learn(Parameters(recall)).await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    let restarted = fixture.server();
    assert_eq!(restarted.state.checkout_mutations.read().pending_count(), 1);
    let queued = restarted
        .state
        .checkout_mutations
        .read()
        .outstanding_writes()
        .map(|row| {
            serde_json::from_str::<KnowledgeEntry>(row.mutation.content_json.as_deref().unwrap())
                .unwrap()
        })
        .next()
        .unwrap();
    assert!(!queued.render);
    assert_eq!(queued.category, crate::knowledge::Category::Memory);
    let result = server
        .bbox_forget(Parameters(ForgetParams {
            id: ENTRY.into(),
            project: Some(PROJECT.into()),
        }))
        .await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    let restarted = fixture.server();
    assert_eq!(restarted.state.checkout_mutations.read().pending_count(), 2);
}
#[tokio::test]
async fn queued_knowledge_broken_queue_does_not_block_durable_global_mutations() {
    let (fixture, mut server, _scope) = fixture();
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let state = std::sync::Arc::get_mut(&mut server.state).unwrap();
    state.checkout_mutations_persister = crate::store_persister::StorePersister::spawn(
        "unrelated-broken-queue",
        state.checkout_mutations.clone(),
        root,
    );
    let result = server
        .bbox_learn(Parameters(LearnParams {
            render_placement: None,
            content: "durable global".into(),
            category: "convention".into(),
            scope: Some("global".into()),
            ..Default::default()
        }))
        .await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    let global = fixture
        .server()
        .state
        .kb
        .read()
        .all_entries()
        .iter()
        .find(|entry| entry.content == "durable global")
        .unwrap()
        .clone();
    let result = server
        .bbox_learn(Parameters(LearnParams {
            render_placement: None,
            id: Some(global.id.clone()),
            content: "updated durable global".into(),
            category: "convention".into(),
            scope: Some("global".into()),
            ..Default::default()
        }))
        .await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    assert!(
        fixture
            .server()
            .state
            .kb
            .read()
            .all_entries()
            .iter()
            .any(|entry| entry.content == "updated durable global")
    );
    let result = server
        .bbox_learn(Parameters(LearnParams {
            content: "durable global recall".into(),
            category: "memory".into(),
            scope: Some("global".into()),
            render: Some(false),
            ..Default::default()
        }))
        .await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    assert!(
        fixture
            .server()
            .state
            .kb
            .read()
            .all_entries()
            .iter()
            .any(|entry| entry.content == "durable global recall" && !entry.render)
    );
    let result = server
        .bbox_forget(Parameters(ForgetParams {
            project: None,
            id: global.id.clone(),
        }))
        .await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    assert!(
        !fixture
            .server()
            .state
            .kb
            .read()
            .all_entries()
            .iter()
            .any(|entry| entry.id == global.id)
    );
}
