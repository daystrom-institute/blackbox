//! Catalog-mode project configuration over a daemon with no checkout on disk:
//! every read comes from the accepted publication, every write is a guarded
//! checkout mutation, and every unavailable state is a named refusal.

use super::*;
use crate::server::BlackboxServer;
use crate::server::state::catalog_fixture::{COMMIT_ONE, COMMIT_TWO, CatalogFixture};

const PROJECT: &str = "p_config_accepted";
const BROFILE: &str = ".bro/brofiles/reviewer.json";

fn brofile(name: &str, provider: &str, model: &str) -> String {
    serde_json::json!({"name": name, "provider": provider, "model": model}).to_string()
}

fn project_mcp() -> String {
    serde_json::json!({
        "version": 1,
        "servers": {},
        "filters": {"allow": [], "disallow": ["mcp__project__forbidden"]}
    })
    .to_string()
}

fn global_brofiles(server: &BlackboxServer) {
    for (name, model) in [("reviewer", "global-reviewer"), ("writer", "global-writer")] {
        let brofile: orchestration::brofile::Brofile =
            serde_json::from_str(&brofile(name, "glm", model)).unwrap();
        orchestration::brofile::save_brofile(&brofile, "global", &server.state.store_dir, None)
            .unwrap();
    }
    let template = orchestration::team::Teamplate {
        name: "global-panel".into(),
        members: vec![orchestration::team::TeamplateMember {
            brofile: "writer".into(),
            alias: None,
            count: 1,
        }],
        advisor: None,
        diversity_floor: None,
    };
    orchestration::team::save_teamplate(&template, "global", &server.state.store_dir, None)
        .unwrap();
}

/// A published project whose accepted configuration overrides `reviewer`,
/// publishes one template and a project MCP filter, and disables project MCP.
fn accepted_fixture(scope: &PublishedScope) -> (CatalogFixture, BlackboxServer) {
    let fixture = CatalogFixture::new();
    fixture.add_published_project(PROJECT, scope);
    let reviewer = brofile("reviewer", "deepseek", "project-reviewer");
    let squad = r#"{"name":"squad","members":[{"brofile":"reviewer","count":1},{"brofile":"writer","count":1}]}"#;
    let mcp = project_mcp();
    fixture.install_config_publication(
        PROJECT,
        scope,
        COMMIT_ONE,
        Some(&[
            (".bbox/config.toml", b"[mcp]\nenabled = false\n"),
            (".bbox/mcp.json", mcp.as_bytes()),
            (BROFILE, reviewer.as_bytes()),
            (".bro/teamplates/squad.json", squad.as_bytes()),
        ]),
    );
    let server = fixture.server();
    global_brofiles(&server);
    (fixture, server)
}

/// A test install writes the accepted store directly; a real advance also
/// invalidates the runtime's cached view, so do the same here.
fn invalidate(server: &BlackboxServer, project_id: &str) {
    server
        .state
        .accepted_publications
        .as_ref()
        .unwrap()
        .invalidate_content(
            &bbox_corpus_core::project_catalog::ProjectId::parse(project_id).unwrap(),
        );
}

fn model(resolved: &Resolved<orchestration::brofile::Brofile>) -> &str {
    resolved.value.model.as_deref().unwrap()
}

#[test]
fn accepted_view_serves_exact_scope_reads_and_project_first_resolution() {
    let scope = CatalogFixture::scope(".");
    let (_fixture, server) = accepted_fixture(&scope);
    let state = &server.state;

    let accepted = state.load_accepted_project_config(PROJECT).unwrap();
    // Exact-scope discovery lists only the project's own configuration.
    assert_eq!(
        accepted
            .snapshot
            .brofiles()
            .map(|(name, _)| name)
            .collect::<Vec<_>>(),
        vec!["reviewer"]
    );
    assert_eq!(
        accepted
            .snapshot
            .teamplates()
            .map(|(name, _)| name)
            .collect::<Vec<_>>(),
        vec!["squad"]
    );
    assert_eq!(accepted.snapshot.mcp_enabled(), Some(false));
    assert_eq!(accepted.snapshot.provenance().accepted_commit, COMMIT_ONE);

    let project = state
        .resolve_config_brofile("reviewer", Some(PROJECT))
        .unwrap()
        .unwrap();
    assert_eq!(model(&project), "project-reviewer");
    assert!(matches!(project.source, ProjectConfigSource::Project(_)));
    let fallback = state
        .resolve_config_brofile("writer", Some(PROJECT))
        .unwrap()
        .unwrap();
    assert_eq!(model(&fallback), "global-writer");
    match &fallback.source {
        ProjectConfigSource::GlobalFallback(provenance) => {
            assert_eq!(provenance.project_id, PROJECT);
            assert_eq!(provenance.accepted_commit, COMMIT_ONE);
        }
        other => panic!("expected an attributed global fallback, got {other:?}"),
    }
    assert!(
        state
            .resolve_config_brofile("absent", Some(PROJECT))
            .unwrap()
            .is_none()
    );
    let global = state
        .resolve_config_brofile("reviewer", None)
        .unwrap()
        .unwrap();
    assert_eq!(model(&global), "global-reviewer");
    assert_eq!(global.source, ProjectConfigSource::Global);

    let template = state
        .resolve_config_teamplate("squad", Some(PROJECT))
        .unwrap()
        .unwrap();
    assert!(matches!(template.source, ProjectConfigSource::Project(_)));
    let global_template = state
        .resolve_config_teamplate("global-panel", Some(PROJECT))
        .unwrap()
        .unwrap();
    assert!(matches!(
        global_template.source,
        ProjectConfigSource::GlobalFallback(_)
    ));

    // Dispatch filters merge the accepted project store exactly as the
    // daemon-local store merged: global, then project, then the guard.
    let project_store = state.dispatch_project_mcp_store(Some(PROJECT)).unwrap();
    let filters = crate::server::progress::resolve_dispatch_filters(
        crate::orchestration::providers::Provider::Glm,
        project_store.as_ref(),
        false,
        "task",
        None,
    )
    .unwrap();
    assert!(
        filters
            .filters
            .disallow
            .iter()
            .any(|pattern| pattern.contains("mcp__project__forbidden"))
    );
    assert!(
        filters
            .filters
            .disallow
            .iter()
            .any(|pattern| pattern.contains("bro_exec")),
        "the recursion guard still applies"
    );
    assert!(
        state
            .dispatch_project_mcp_store(Some("/not/a/catalog/project"))
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_selected_path_never_reads_the_daemon_local_checkout() {
    let scope = CatalogFixture::scope(".");
    let (fixture, server) = accepted_fixture(&scope);
    // A directory on the daemon host that looks like the checkout and holds
    // different configuration. Attaching it makes the path a selector for
    // the project; its files must still never be read.
    let local = fixture.root().join("look-alike-checkout");
    std::fs::create_dir_all(local.join(".bro/brofiles")).unwrap();
    std::fs::create_dir_all(local.join(".bbox")).unwrap();
    std::fs::write(
        local.join(BROFILE),
        brofile("reviewer", "glm", "daemon-local-reviewer"),
    )
    .unwrap();
    std::fs::write(
        local.join(".bbox/mcp.json"),
        r#"{"version":1,"servers":{},"filters":{"allow":[],"disallow":["local-only"]}}"#,
    )
    .unwrap();
    fixture.attach_checkout(
        PROJECT,
        &scope,
        &local,
        "att_22222222222222222222222222222222",
    );
    let server = {
        drop(server);
        let server = fixture.server();
        global_brofiles(&server);
        server
    };
    let selector = local.to_str().unwrap();
    let resolved = server
        .state
        .resolve_config_brofile("reviewer", Some(selector))
        .unwrap()
        .unwrap();
    assert_eq!(model(&resolved), "project-reviewer");
    let store = server
        .state
        .dispatch_project_mcp_store(Some(selector))
        .unwrap()
        .unwrap();
    assert_eq!(store.filters.disallow, vec!["mcp__project__forbidden"]);
    // A path that selects no project is worker context: global only.
    let unrelated = fixture.root().join("unrelated");
    std::fs::create_dir_all(unrelated.join(".bro/brofiles")).unwrap();
    std::fs::write(
        unrelated.join(BROFILE),
        brofile("reviewer", "glm", "unrelated-local"),
    )
    .unwrap();
    let global = server
        .state
        .resolve_config_brofile("reviewer", unrelated.to_str())
        .unwrap()
        .unwrap();
    assert_eq!(model(&global), "global-reviewer");
    assert_eq!(global.source, ProjectConfigSource::Global);
}

#[test]
fn unavailable_unsupported_and_invalid_views_refuse_instead_of_falling_back() {
    let fixture = CatalogFixture::new();
    let scope = CatalogFixture::scope(".");
    let unpublished = "p_config_unpublished";
    fixture.add_published_project(unpublished, &scope);
    let legacy_scope = CatalogFixture::scope("legacy");
    let legacy = "p_config_legacy";
    fixture.add_published_project(legacy, &legacy_scope);
    fixture.install_config_publication(legacy, &legacy_scope, COMMIT_ONE, None);
    let invalid_scope = CatalogFixture::scope("invalid");
    let invalid = "p_config_invalid";
    fixture.add_published_project(invalid, &invalid_scope);
    // Installed below the daemon's acceptance parser, the way damaged or
    // foreign state would appear: the read path must still refuse it.
    fixture.install_config_publication(
        invalid,
        &invalid_scope,
        COMMIT_ONE,
        Some(&[(
            BROFILE,
            br#"{"name":"reviewer","provider":"sk-not-a-provider"}"#,
        )]),
    );
    let empty_scope = CatalogFixture::scope("empty");
    let empty = "p_config_empty";
    fixture.add_published_project(empty, &empty_scope);
    fixture.install_config_publication(empty, &empty_scope, COMMIT_ONE, Some(&[]));
    let server = fixture.server();
    global_brofiles(&server);

    for (project, code) in [
        (unpublished, "error.project_config_publication_unavailable"),
        (legacy, "error.project_config_lane_unsupported"),
        (invalid, "error.project_config_invalid"),
    ] {
        let error = server
            .state
            .resolve_config_brofile("reviewer", Some(project))
            .unwrap_err();
        assert_eq!(error.code(), code, "{project}");
        let text = server
            .state
            .dispatch_brofile("reviewer", Some(project))
            .unwrap_err();
        assert!(text.starts_with(code), "{text}");
        assert!(text.contains("No global fallback"), "{text}");
        assert!(!text.contains("sk-not-a-provider"), "{text}");
        assert!(
            server
                .state
                .dispatch_project_mcp_store(Some(project))
                .is_err()
        );
        assert!(
            server
                .resolve_exec_target(Some("reviewer"), None, Some(project))
                .is_err()
        );
    }
    // A verified empty configuration is not an error: it proves absence.
    let fallback = server
        .state
        .resolve_config_brofile("reviewer", Some(empty))
        .unwrap()
        .unwrap();
    assert_eq!(model(&fallback), "global-reviewer");
    assert!(matches!(
        fallback.source,
        ProjectConfigSource::GlobalFallback(_)
    ));
    assert!(
        server
            .state
            .dispatch_project_mcp_store(Some(empty))
            .unwrap()
            .is_none()
    );
}

#[test]
fn nested_published_scopes_map_landing_paths_consistently() {
    let scope = CatalogFixture::scope("services/api");
    let (_fixture, server) = accepted_fixture(&scope);
    let resolved = server
        .state
        .resolve_config_brofile("reviewer", Some(PROJECT))
        .unwrap()
        .unwrap();
    assert_eq!(model(&resolved), "project-reviewer");
    let receipt = server
        .state
        .prepare_project_config_mutation(
            PROJECT,
            &ProjectConfigTargetV1::Brofile("reviewer".into()),
            "test",
            |_| {
                Ok(Some(ProjectConfigEdit::Write(brofile(
                    "reviewer", "claude", "next",
                ))))
            },
        )
        .unwrap()
        .unwrap();
    assert_eq!(receipt.landing.scope_relative_path, BROFILE);
    assert_eq!(
        receipt.landing.repository_relative_path,
        format!("services/api/{BROFILE}")
    );
    assert_eq!(receipt.landing.bbox_root_relpath, "services/api");
    let queued = server.state.checkout_mutations.read();
    let row = queued.get(&receipt.mutation_id).unwrap();
    assert_eq!(
        row.mutation.relative_path, BROFILE,
        "delivery stays scope-relative"
    );
    assert_eq!(row.mutation.scope, scope);
}

#[tokio::test]
async fn guarded_edits_chain_privately_and_reads_switch_only_at_publication() {
    let scope = CatalogFixture::scope(".");
    let (fixture, server) = accepted_fixture(&scope);
    let target = ProjectConfigTargetV1::Brofile("reviewer".into());
    let accepted = brofile("reviewer", "deepseek", "project-reviewer");
    let first = brofile("reviewer", "claude", "first-edit");
    let second = brofile("reviewer", "claude", "second-edit");

    let mut bases = Vec::new();
    let mut edit = |next: Option<String>| {
        server
            .state
            .prepare_project_config_mutation(
                PROJECT,
                &target,
                "bro_brofile(scope=project)",
                |base| {
                    bases.push(base.map(str::to_owned));
                    Ok(Some(match next {
                        Some(content) => ProjectConfigEdit::Write(content),
                        None => ProjectConfigEdit::Delete,
                    }))
                },
            )
            .unwrap()
            .unwrap()
    };
    let create = edit(Some(first.clone()));
    let replace = edit(Some(second.clone()));
    let delete = edit(None);
    let recreate = edit(Some(first.clone()));
    assert_eq!(
        bases,
        vec![
            Some(accepted.clone()),
            Some(first.clone()),
            Some(second.clone()),
            None
        ],
        "each edit builds on its predecessor, not on the accepted bytes"
    );
    let sha = crate::checkout_mutations::content_sha256;
    assert_eq!(create.expected_sha256, Some(sha(&accepted)));
    assert_eq!(create.predecessor, None);
    assert_eq!(replace.expected_sha256, Some(sha(&first)));
    assert_eq!(
        replace.predecessor.as_deref(),
        Some(create.mutation_id.as_str())
    );
    assert_eq!(delete.mode, "delete");
    assert_eq!(delete.expected_sha256, Some(sha(&second)));
    assert_eq!(recreate.expected_sha256, None);
    assert_eq!(
        recreate.predecessor.as_deref(),
        Some(delete.mutation_id.as_str())
    );
    assert_eq!(create.state, CheckoutMutationProgress::Queued);
    assert!(create.next_step.contains("Commit and publish"));
    assert_eq!(create.landing.repository_relative_path, BROFILE);

    // Deleting an absent name and rewriting identical bytes change nothing.
    assert!(
        server
            .state
            .prepare_project_config_mutation(
                PROJECT,
                &ProjectConfigTargetV1::Brofile("ghost".into()),
                "test",
                |_| Ok(Some(ProjectConfigEdit::Delete)),
            )
            .unwrap()
            .is_none()
    );
    assert!(
        server
            .state
            .prepare_project_config_mutation(PROJECT, &target, "test", |base| Ok(Some(
                ProjectConfigEdit::Write(base.unwrap().to_string())
            )))
            .unwrap()
            .is_none()
    );

    // Reads and dispatch stay on the accepted generation throughout.
    let current = server
        .state
        .resolve_config_brofile("reviewer", Some(PROJECT))
        .unwrap()
        .unwrap();
    assert_eq!(model(&current), "project-reviewer");

    // Restart keeps the chain.
    server
        .state
        .persist_checkout_mutations_durable()
        .await
        .unwrap();
    let server = fixture.server();
    global_brofiles(&server);
    let status = server
        .state
        .project_config_mutation_status(&create.mutation_id)
        .unwrap();
    assert_eq!(status.state, CheckoutMutationProgress::Queued);
    for mutation in [&create, &replace, &delete, &recreate] {
        server
            .state
            .checkout_mutations
            .write()
            .ack(&mutation.mutation_id, "applied", None, None, "now")
            .unwrap();
    }
    let status = server
        .state
        .project_config_mutation_status(&recreate.mutation_id)
        .unwrap();
    assert_eq!(status.state, CheckoutMutationProgress::Delivered);
    assert!(status.next_step.contains("not yet published"));
    assert_eq!(
        model(
            &server
                .state
                .resolve_config_brofile("reviewer", Some(PROJECT))
                .unwrap()
                .unwrap()
        ),
        "project-reviewer",
        "delivery is not publication"
    );

    // The owner commits and publishes the chain's final bytes.
    let mcp = project_mcp();
    fixture.install_config_publication(
        PROJECT,
        &scope,
        COMMIT_TWO,
        Some(&[
            (".bbox/mcp.json", mcp.as_bytes()),
            (BROFILE, first.as_bytes()),
        ]),
    );
    invalidate(&server, PROJECT);
    let status = server
        .state
        .project_config_mutation_status(&recreate.mutation_id)
        .unwrap();
    assert_eq!(status.state, CheckoutMutationProgress::Published);
    assert_eq!(
        model(
            &server
                .state
                .resolve_config_brofile("reviewer", Some(PROJECT))
                .unwrap()
                .unwrap()
        ),
        "first-edit"
    );
    // The next edit starts from the newly accepted bytes.
    let next = server
        .state
        .prepare_project_config_mutation(PROJECT, &target, "test", |base| {
            assert_eq!(base, Some(first.as_str()));
            Ok(Some(ProjectConfigEdit::Delete))
        })
        .unwrap()
        .unwrap();
    assert_eq!(next.expected_sha256, Some(sha(&first)));
    assert_eq!(next.predecessor, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simultaneous_read_modify_write_edits_serialize_without_losing_either() {
    let scope = CatalogFixture::scope(".");
    let (_fixture, server) = accepted_fixture(&scope);
    let target = ProjectConfigTargetV1::McpStore;
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let handles = ["alpha", "beta"]
        .into_iter()
        .map(|name| {
            let server = server.clone();
            let barrier = barrier.clone();
            let target = target.clone();
            tokio::task::spawn_blocking(move || {
                barrier.wait();
                server
                    .state
                    .prepare_project_config_mutation(PROJECT, &target, "bro_mcp", |base| {
                        let mut store: serde_json::Value =
                            serde_json::from_str(base.unwrap()).unwrap();
                        store["filters"]["allow"]
                            .as_array_mut()
                            .unwrap()
                            .push(serde_json::json!(name));
                        Ok(Some(ProjectConfigEdit::Write(store.to_string())))
                    })
                    .unwrap()
                    .unwrap()
            })
        })
        .collect::<Vec<_>>();
    let mut receipts = Vec::new();
    for handle in handles {
        receipts.push(handle.await.unwrap());
    }
    receipts.sort_by_key(|receipt| receipt.predecessor.is_some());
    let (first, second) = (&receipts[0], &receipts[1]);
    assert_eq!(
        second.predecessor.as_deref(),
        Some(first.mutation_id.as_str())
    );
    let queue = server.state.checkout_mutations.read();
    let last = queue.get(&second.mutation_id).unwrap();
    let merged: serde_json::Value =
        serde_json::from_str(last.mutation.content_json.as_deref().unwrap()).unwrap();
    let allow = merged["filters"]["allow"].as_array().unwrap();
    assert_eq!(allow.len(), 2, "{merged}");
    let first_content = queue
        .get(&first.mutation_id)
        .unwrap()
        .mutation
        .content_json
        .clone()
        .unwrap();
    assert_eq!(
        second.expected_sha256,
        Some(crate::checkout_mutations::content_sha256(&first_content))
    );
}

#[test]
fn a_publication_that_moves_during_preparation_is_re_read_before_enqueue() {
    let scope = CatalogFixture::scope(".");
    let (fixture, server) = accepted_fixture(&scope);
    let moved = brofile("reviewer", "claude", "moved-under-us");
    let mut moved_once = false;
    let receipt = server
        .state
        .prepare_project_config_mutation_with_snapshot_hook(
            PROJECT,
            &ProjectConfigTargetV1::Brofile("reviewer".into()),
            "test",
            || {
                if !moved_once {
                    moved_once = true;
                    fixture.install_config_publication(
                        PROJECT,
                        &scope,
                        COMMIT_TWO,
                        Some(&[(BROFILE, moved.as_bytes())]),
                    );
                    invalidate(&server, PROJECT);
                }
            },
            |base| {
                assert_eq!(base, Some(moved.as_str()));
                Ok(Some(ProjectConfigEdit::Write(brofile(
                    "reviewer", "claude", "after",
                ))))
            },
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        receipt.expected_sha256,
        Some(crate::checkout_mutations::content_sha256(&moved))
    );
    let accepted = server.state.load_accepted_project_config(PROJECT).unwrap();
    assert_eq!(receipt.accepted_generation, accepted.stamp.generation_id());
    assert_eq!(accepted.stamp.accepted_commit(), COMMIT_TWO);
}

#[test]
fn conflicts_blocked_successors_and_unsupported_owners_are_named_states() {
    let scope = CatalogFixture::scope(".");
    let (_fixture, server) = accepted_fixture(&scope);
    let target = ProjectConfigTargetV1::McpStore;
    let write = |value: &str| {
        let value = value.to_string();
        server
            .state
            .prepare_project_config_mutation(PROJECT, &target, "bro_mcp", move |_| {
                Ok(Some(ProjectConfigEdit::Write(
                    serde_json::json!({"version": 1, "servers": {}, "filters": {"allow": [value], "disallow": []}}).to_string(),
                )))
            })
            .unwrap()
            .unwrap()
    };
    let first = write("one");
    let second = write("two");
    // An owner without guarded support sees neither; both are stamped.
    let poll = server
        .state
        .checkout_mutations
        .read()
        .poll(&std::collections::BTreeSet::from([scope.clone()]), false);
    assert!(poll.mutations.is_empty());
    server
        .state
        .checkout_mutations
        .write()
        .note_owner_unsupported(&poll.withheld_unsupported, "now");
    let status = server
        .state
        .project_config_mutation_status(&first.mutation_id)
        .unwrap();
    assert_eq!(status.state, CheckoutMutationProgress::Queued);
    assert!(status.owner_collector_unsupported);
    assert!(status.next_step.contains("Upgrade the collector"));

    let observed = "d".repeat(64);
    server
        .state
        .checkout_mutations
        .write()
        .ack_with_observation(
            &first.mutation_id,
            "conflicted",
            Some("local bytes preserved".into()),
            None,
            Some(observed.clone()),
            "now",
        )
        .unwrap();
    let conflicted = server
        .state
        .project_config_mutation_status(&first.mutation_id)
        .unwrap();
    assert_eq!(conflicted.state, CheckoutMutationProgress::Conflicted);
    assert_eq!(conflicted.observed_sha256, Some(Some(observed)));
    assert!(conflicted.next_step.contains("Reconcile"));
    assert_eq!(conflicted.landing.scope_relative_path, ".bbox/mcp.json");
    let blocked = server
        .state
        .project_config_mutation_status(&second.mutation_id)
        .unwrap();
    assert_eq!(blocked.state, CheckoutMutationProgress::Blocked);
    assert_eq!(
        blocked.blocked_by.as_deref(),
        Some(first.mutation_id.as_str())
    );
    let rendered = serde_json::to_string(&conflicted).unwrap();
    assert!(
        !rendered.contains("\"one\""),
        "no configuration bytes in status: {rendered}"
    );

    // Recovery recomputes from the accepted bytes with a precondition.
    let retry = write("three");
    assert_eq!(
        retry.expected_sha256,
        Some(crate::checkout_mutations::content_sha256(&project_mcp()))
    );
    assert_eq!(retry.predecessor, None);
    // A legacy knowledge mutation id is not a configuration mutation.
    assert!(
        server
            .state
            .project_config_mutation_status("cm-0000000000000000")
            .is_err()
    );
}

#[test]
fn bridge_mode_keeps_its_local_behavior_and_refuses_the_lane() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let server = BlackboxServer::new(std::sync::Arc::new(
        crate::server::state::SharedState::for_test(&root),
    ));
    let project = root.join("bridge-project");
    std::fs::create_dir_all(project.join(".bro/brofiles")).unwrap();
    std::fs::write(
        project.join(BROFILE),
        brofile("reviewer", "glm", "bridge-local"),
    )
    .unwrap();
    let selector = project.to_str().unwrap();
    assert!(matches!(
        server.state.project_config_context(Some(selector)),
        Ok(ProjectConfigContext::Local(_))
    ));
    let resolved = server
        .state
        .resolve_config_brofile("reviewer", Some(selector))
        .unwrap()
        .unwrap();
    assert_eq!(model(&resolved), "bridge-local");
    assert_eq!(resolved.source, ProjectConfigSource::Local);
    let error = server
        .state
        .prepare_project_config_mutation(PROJECT, &ProjectConfigTargetV1::McpStore, "test", |_| {
            Ok(Some(ProjectConfigEdit::Delete))
        })
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("error.project_config_lane_catalog_only")
    );
}
