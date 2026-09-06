use super::*;
use crate::knowledge::{Approval, KnowledgeEntry, ReviewParams, Status};
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
        id: id.map(str::to_string),
        content: content.into(),
        category: "convention".into(),
        scope: Some("project".into()),
        project: Some(PROJECT.into()),
        ..Default::default()
    }
}

fn link(target: &str) -> KnowledgeLinkParams {
    serde_json::from_value(json!({
        "source": format!("knowledge:{ENTRY}"),
        "target": target,
        "kind": "RelatesTo"
    }))
    .unwrap()
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

fn serialized_text(result: &CallToolResult) -> (usize, String) {
    let wire = serde_json::to_vec(result).unwrap();
    let envelope: serde_json::Value = serde_json::from_slice(&wire).unwrap();
    let text = envelope["content"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|content| content["text"].as_str())
        .unwrap()
        .to_string();
    (wire.len(), text)
}

#[tokio::test]
async fn review_list_serialized_envelope_bounds_worst_case_escaping() {
    let (fixture, scope) = published_fixture();
    let entries: Vec<_> = (0..100)
        .map(|index| {
            let id = format!("{index:016x}");
            let mut entry = knowledge_entry(&id, &"\"escaped\"\t".repeat(256));
            entry.title = "\"title\"\n".repeat(128);
            entry.approval = Approval::AgentInferred;
            entry
        })
        .collect();
    fixture.install_publication(PROJECT, &scope, &"2".repeat(40), &entries, &[]);
    let server = queue_server(&fixture);
    let result = server
        .bbox_review(Parameters(ReviewParams {
            action: Some("list".into()),
            limit: Some(100),
            ..Default::default()
        }))
        .await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    let (wire_bytes, text) = serialized_text(&result);
    assert!(
        wire_bytes <= BlackboxServer::MCP_RESPONSE_CAP_BYTES,
        "serialized review list was {wire_bytes} bytes"
    );
    let reply: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(reply["rows"].as_array().unwrap().len(), 100);
    for row in reply["rows"].as_array().unwrap() {
        assert!(row["title_preview"].as_str().unwrap().len() < 192);
        assert!(row["content_preview"].as_str().unwrap().len() < 192);
    }
}

#[tokio::test]
async fn review_exact_serialized_envelope_pages_one_huge_record() {
    let (fixture, scope) = published_fixture();
    let mut entry = knowledge_entry(ENTRY, &"\"escaped content\"\t".repeat(4_000));
    entry.title = "\"escaped title\"\n".repeat(2_000);
    entry.approval = Approval::AgentInferred;
    fixture.install_publication(PROJECT, &scope, &"2".repeat(40), &[entry.clone()], &[]);
    let server = queue_server(&fixture);
    let mut params = ReviewParams {
        action: Some("get".into()),
        id: Some(ENTRY.into()),
        limit: Some(257),
        ..Default::default()
    };
    let first = server.bbox_review(Parameters(params.clone())).await;
    assert_ne!(first.is_error, Some(true), "{first:?}");
    let (wire_bytes, text) = serialized_text(&first);
    assert!(
        wire_bytes <= BlackboxServer::MCP_RESPONSE_CAP_BYTES,
        "serialized exact review page was {wire_bytes} bytes"
    );
    let first: serde_json::Value = serde_json::from_str(&text).unwrap();
    let mut reconstructed = first["body"]["text"].as_str().unwrap().to_string();
    let mut cursor = first["next_cursor"].as_str().map(str::to_string);
    while let Some(active_cursor) = cursor {
        params.cursor = Some(active_cursor);
        let page = server.bbox_review(Parameters(params.clone())).await;
        assert_ne!(page.is_error, Some(true), "{page:?}");
        let (wire_bytes, text) = serialized_text(&page);
        assert!(wire_bytes <= BlackboxServer::MCP_RESPONSE_CAP_BYTES);
        let page: serde_json::Value = serde_json::from_str(&text).unwrap();
        reconstructed.push_str(page["body"]["text"].as_str().unwrap());
        cursor = page["next_cursor"].as_str().map(str::to_string);
    }
    let recovered: KnowledgeEntry = serde_json::from_str(&reconstructed).unwrap();
    assert_eq!(recovered.title, entry.title);
    assert_eq!(recovered.content, entry.content);
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
    let result = server
        .bbox_knowledge_link(Parameters(link("knowledge:1111111111111111")))
        .await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    assert_eq!(latest(&fixture.server(), &scope, ENTRY).links.len(), 1);
    let rows = server
        .state
        .checkout_mutations
        .read()
        .poll(&BTreeSet::from([scope.clone()]))
        .0;
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
    server
        .enqueue_link_via_checkout_owner(&link("knowledge:2222222222222222"))
        .unwrap();
    let entry = latest(&server, &scope, ENTRY);
    assert_eq!(entry.content, "queued content");
    assert_eq!(entry.links.len(), 2);
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
    publish(&fixture, &server, &scope, "3", &[external]);
    server
        .enqueue_link_via_checkout_owner(&link("knowledge:3333333333333333"))
        .unwrap();
    assert_eq!(latest(&server, &scope, ENTRY).content, "later publication");
}

#[tokio::test]
async fn queued_knowledge_preserves_the_complete_canonical_entry() {
    let (fixture, server, scope) = fixture();
    let mut entry = knowledge_entry(ENTRY, "complete");
    entry.project_id = Some(PROJECT.into());
    entry.cluster = Some("cluster".into());
    entry.variants.insert("provider".into(), "variant".into());
    entry.providers = vec!["provider".into()];
    entry.review_at = Some("2027-01-01".into());
    entry.expires_at = Some("2028-01-01".into());
    entry.rationale = Some("rationale".into());
    entry.supersedes = Some("1111111111111111".into());
    entry.weight = 83;
    entry.approval = Approval::AgentInferred;
    publish(&fixture, &server, &scope, "2", &[entry.clone()]);
    server
        .enqueue_review_via_checkout_owner("approve", ENTRY, None)
        .unwrap();
    let updated = latest(&server, &scope, ENTRY);
    entry.approval = Approval::UserConfirmed;
    entry.updated_at = updated.updated_at.clone();
    assert_eq!(
        crate::knowledge::committed_knowledge_entry_bytes(&updated).unwrap(),
        crate::knowledge::committed_knowledge_entry_bytes(&entry).unwrap()
    );
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
    let error = server
        .enqueue_link_via_checkout_owner(&link("knowledge:1111111111111111"))
        .unwrap_err();
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
            superseded_by: None,
        }))
        .await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    let restarted = queue_server(&fixture);
    let rows = restarted
        .state
        .checkout_mutations
        .read()
        .poll(&BTreeSet::from([scope.clone()]))
        .0;
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
    assert!(
        restarted
            .enqueue_link_via_checkout_owner(&link("knowledge:1111111111111111"))
            .is_err()
    );
    assert!(
        restarted
            .enqueue_review_via_checkout_owner("approve", ENTRY, None)
            .is_err()
    );
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
        server
            .enqueue_link_via_checkout_owner(&link("knowledge:1111111111111111"))
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
async fn queued_knowledge_supersession_validates_both_records_before_admission() {
    let (fixture, scope) = published_fixture();
    let other_scope = CatalogFixture::scope("other");
    fixture.add_published_project("p_other", &other_scope);
    fixture.install_publication(
        "p_other",
        &other_scope,
        &"2".repeat(40),
        &[knowledge_entry("2222222222222222", "foreign")],
        &[],
    );
    let server = queue_server(&fixture);
    assert_eq!(
        server.covered_scope_for_project_id("p_other"),
        Some(other_scope),
    );
    let mut params = DecideParams {
        content: "replacement".into(),
        rationale: "justification".into(),
        supersedes: Some("2222222222222222".into()),
        ..Default::default()
    };
    assert!(
        server
            .enqueue_decide_via_checkout_owner(&params, PROJECT, PROJECT, scope.clone())
            .is_err()
    );
    assert_eq!(server.state.checkout_mutations.read().pending_count(), 0);
    params.supersedes = Some(ENTRY.into());
    params.content = "x".repeat(bbox_code_source::MAX_CHECKOUT_MUTATION_CONTENT_BYTES + 1);
    assert!(
        server
            .enqueue_decide_via_checkout_owner(&params, PROJECT, PROJECT, scope.clone())
            .is_err()
    );
    assert_eq!(server.state.checkout_mutations.read().pending_count(), 0);
    params.content = "replacement".into();
    server
        .enqueue_decide_via_checkout_owner(&params, PROJECT, PROJECT, scope.clone())
        .unwrap();
    let old = latest(&server, &scope, ENTRY);
    assert_eq!(old.status, Status::Superseded);
    let replacement = latest(&server, &scope, old.supersedes.as_deref().unwrap());
    assert_eq!(replacement.content, "replacement");
    assert_eq!(server.state.checkout_mutations.read().pending_count(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_knowledge_concurrent_links_compose_and_duplicate_links_do_not_enqueue() {
    let (_fixture, server, scope) = fixture();
    let mut tasks = Vec::new();
    for index in 0..12 {
        let server = server.clone();
        tasks.push(tokio::task::spawn_blocking(move || {
            server
                .enqueue_link_via_checkout_owner(&link(&format!("knowledge:{index:016x}")))
                .unwrap();
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    assert_eq!(latest(&server, &scope, ENTRY).links.len(), 12);
    let count = server.state.checkout_mutations.read().pending_count();
    server
        .enqueue_link_via_checkout_owner(&link("knowledge:0000000000000000"))
        .unwrap();
    assert_eq!(
        server.state.checkout_mutations.read().pending_count(),
        count
    );
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
        .bbox_review(Parameters(ReviewParams {
            project: None,
            action: Some("reject".into()),
            id: Some(id.into()),
            ..Default::default()
        }))
        .await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    assert!(
        queue_server(&fixture)
            .enqueue_review_via_checkout_owner("approve", id, None)
            .is_err()
    );
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
                content: "global rule".into(),
                category: "convention".into(),
                scope: Some("global".into()),
                ..Default::default()
            },
            false,
            None,
            None,
        )
        .unwrap();
    let mut params = link("knowledge:1111111111111111");
    params.source = format!("knowledge:{}", global.id);
    let result = server.bbox_knowledge_link(Parameters(params)).await;
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
    assert!(
        server
            .enqueue_review_via_checkout_owner("approve", "unknown", None)
            .is_err()
    );
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
            superseded_by: None,
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
    assert!(
        server
            .enqueue_review_via_checkout_owner("approve", &created.id, None)
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
            .enqueue_review_via_checkout_owner("approve", &created.id, None)
            .is_err()
    );
    let rows = server
        .state
        .checkout_mutations
        .read()
        .poll(&BTreeSet::from([scope.clone()]))
        .0;
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
        .poll(&BTreeSet::from([scope.clone()]))
        .0[0]
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
            superseded_by: None,
        }))
        .await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    let delete = server
        .state
        .checkout_mutations
        .read()
        .poll(&BTreeSet::from([scope.clone()]))
        .0[0]
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
            .enqueue_review_via_checkout_owner("approve", &created.id, Some(PROJECT))
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
            .enqueue_review_via_checkout_owner("approve", &created.id, Some(PROJECT))
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
async fn queued_knowledge_explicit_owner_isolates_review_link_and_forget_from_broken_projects() {
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
                content: "global owner".into(),
                category: "convention".into(),
                scope: Some("global".into()),
                ..Default::default()
            },
            false,
            None,
            None,
        )
        .unwrap();
    assert!(
        server
            .enqueue_link_via_checkout_owner(&link("knowledge:1111111111111111"))
            .unwrap_err()
            .to_string()
            .contains("multiple projects")
    );
    fixture.corrupt_generation("p_broken", &publication.generation_id);
    server.invalidate_catalog_published_content(
        &bbox_corpus_core::project_catalog::ProjectId::parse("p_broken").unwrap(),
    );
    let ambiguous = server
        .bbox_review(Parameters(ReviewParams {
            id: Some(ENTRY.into()),
            action: Some("approve".into()),
            project: None,
            ..Default::default()
        }))
        .await;
    assert_eq!(ambiguous.is_error, Some(true));
    assert!(format!("{ambiguous:?}").contains("pass project"));
    assert!(
        server
            .enqueue_link_via_checkout_owner(&link("knowledge:1111111111111111"))
            .is_err()
    );
    assert!(
        server
            .enqueue_forget_via_checkout_owner(&ForgetParams {
                id: ENTRY.into(),
                superseded_by: None,
                project: None,
            })
            .is_err()
    );
    assert_eq!(server.state.checkout_mutations.read().pending_count(), 0);
    let mismatch = server
        .bbox_review(Parameters(ReviewParams {
            id: Some(global.id.clone()),
            action: Some("approve".into()),
            project: Some(PROJECT.into()),
            ..Default::default()
        }))
        .await;
    assert_eq!(mismatch.is_error, Some(true));
    let local = server
        .bbox_review(Parameters(ReviewParams {
            id: Some(global.id),
            action: Some("approve".into()),
            project: None,
            ..Default::default()
        }))
        .await;
    assert_ne!(local.is_error, Some(true), "{local:?}");
    assert_eq!(server.state.checkout_mutations.read().pending_count(), 0);
    for selector in ["p_broken", "p_nonexistent"] {
        let result = server
            .bbox_review(Parameters(ReviewParams {
                id: Some(ENTRY.into()),
                action: Some("approve".into()),
                project: Some(selector.into()),
                ..Default::default()
            }))
            .await;
        assert_eq!(result.is_error, Some(true));
    }
    assert_eq!(server.state.checkout_mutations.read().pending_count(), 0);
    let result = server
        .bbox_review(Parameters(ReviewParams {
            id: Some(ENTRY.into()),
            action: Some("approve".into()),
            project: Some(PROJECT.into()),
            ..Default::default()
        }))
        .await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    let mut params = link("knowledge:1111111111111111");
    params.project = Some(PROJECT.into());
    let result = server.bbox_knowledge_link(Parameters(params)).await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    assert_eq!(latest(&server, &scope, ENTRY).links.len(), 1);
    let result = server
        .bbox_forget(Parameters(ForgetParams {
            id: ENTRY.into(),
            superseded_by: None,
            project: Some(PROJECT.into()),
        }))
        .await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    assert!(
        server
            .enqueue_review_via_checkout_owner("approve", ENTRY, Some(PROJECT))
            .is_err()
    );
    let restarted = queue_server(&fixture);
    let rows = restarted
        .state
        .checkout_mutations
        .read()
        .poll(&BTreeSet::from([scope.clone(), broken_scope]))
        .0;
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|row| row.scope == scope));
    assert_eq!(rows.last().unwrap().mode, "delete");
}

#[tokio::test]
async fn queued_knowledge_create_receipts_durably_admit_remember_and_decide() {
    let (fixture, server, scope) = fixture();
    let result = server
        .bbox_remember(Parameters(RememberParams {
            content: "queued memory".into(),
            scope: Some("project".into()),
            project: Some(PROJECT.into()),
            ..Default::default()
        }))
        .await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    let restarted = fixture.server();
    assert_eq!(restarted.state.checkout_mutations.read().pending_count(), 1);
    let result = server
        .bbox_decide(Parameters(DecideParams {
            content: "queued decision".into(),
            rationale: "paired durable admission".into(),
            supersedes: Some(ENTRY.into()),
            scope: Some("project".into()),
            project: Some(PROJECT.into()),
            ..Default::default()
        }))
        .await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    let restarted = fixture.server();
    assert_eq!(restarted.state.checkout_mutations.read().pending_count(), 3);
    assert_eq!(latest(&restarted, &scope, ENTRY).status, Status::Superseded);
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
        .bbox_decide(Parameters(DecideParams {
            content: "durable global decision".into(),
            rationale: "keep local authority independent".into(),
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
            .any(|entry| entry.content == "durable global decision")
    );
    let mut params = link("knowledge:1111111111111111");
    params.source = global.id.clone();
    let result = server.bbox_knowledge_link(Parameters(params)).await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    assert_eq!(
        fixture
            .server()
            .state
            .kb
            .read()
            .all_entries()
            .iter()
            .find(|entry| entry.id == global.id)
            .unwrap()
            .links
            .len(),
        1
    );
    let result = server
        .bbox_review(Parameters(ReviewParams {
            project: None,
            action: Some("approve".into()),
            id: Some(global.id.clone()),
            ..Default::default()
        }))
        .await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    let result = server
        .bbox_forget(Parameters(ForgetParams {
            project: None,
            id: global.id.clone(),
            superseded_by: None,
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
            .any(|entry| entry.id == global.id && entry.status == Status::Active)
    );
}
