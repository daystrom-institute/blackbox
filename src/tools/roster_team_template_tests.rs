//! Catalog-mode `bro_team` project template actions over a daemon with no
//! checkout on disk: reads come from the accepted publication, edits are
//! guarded checkout mutations, and team creation, member validation, roster
//! rows and member dispatch resolve accepted project configuration first.

use super::*;
use crate::checkout_mutations::{CheckoutMutationProgress, content_sha256};
use crate::server::state::catalog_fixture::{COMMIT_ONE, COMMIT_TWO, CatalogFixture};
use bbox_code_source::ProjectConfigTargetV1;
use bbox_corpus_core::identity::PublishedScope;
use orchestration::{brofile, team};

const PROJECT: &str = "p_team_templates";
const REVIEWER: &str = ".bro/brofiles/reviewer.json";
const SQUAD: &str = ".bro/teamplates/squad.json";

fn commit_three() -> String {
    "3".repeat(40)
}

fn brofile_json(name: &str, provider: &str, model: &str) -> String {
    json!({"name": name, "provider": provider, "model": model}).to_string()
}

fn template_json(name: &str, brofile: &str, count: u32) -> String {
    json!({"name": name, "members": [{"brofile": brofile, "count": count}]}).to_string()
}

/// The exact bytes `save_template` queues for a one-slot template.
fn saved_bytes(name: &str, brofile: &str, count: u32) -> String {
    let template: team::Teamplate =
        serde_json::from_str(&template_json(name, brofile, count)).unwrap();
    String::from_utf8(crate::json_store::to_vec_pretty_newline(&template).unwrap()).unwrap()
}

fn params(value: Value) -> TeamParams {
    serde_json::from_value(value).unwrap()
}

fn text(result: &CallToolResult) -> String {
    let wire = serde_json::to_value(result).unwrap();
    wire["content"][0]["text"].as_str().unwrap().to_string()
}

async fn call(server: &BlackboxServer, request: Value) -> Result<Value, String> {
    let result = server.bro_team(Parameters(params(request))).await;
    if result.is_error == Some(true) {
        Err(text(&result))
    } else {
        Ok(serde_json::from_str(&text(&result)).unwrap())
    }
}

fn invalidate(server: &BlackboxServer) {
    server
        .state
        .accepted_publications
        .as_ref()
        .unwrap()
        .invalidate_content(&bbox_corpus_core::project_catalog::ProjectId::parse(PROJECT).unwrap());
}

/// Global brofiles `reviewer` and `writer`, and global templates `squad`
/// (same name as the project's) and `writers`.
fn global_configuration(server: &BlackboxServer) {
    let store = &server.state.store_dir;
    for (name, model) in [("reviewer", "global-reviewer"), ("writer", "global-writer")] {
        let value: brofile::Brofile =
            serde_json::from_str(&brofile_json(name, "glm", model)).unwrap();
        brofile::save_brofile(&value, "global", store, None).unwrap();
    }
    for (name, member, count) in [("squad", "writer", 7), ("writers", "writer", 1)] {
        let value: team::Teamplate =
            serde_json::from_str(&template_json(name, member, count)).unwrap();
        team::save_teamplate(&value, "global", store, None).unwrap();
    }
}

fn publish(fixture: &CatalogFixture, scope: &PublishedScope, commit: &str, files: &[(&str, &str)]) {
    let files = files
        .iter()
        .map(|(path, bytes)| (*path, bytes.as_bytes()))
        .collect::<Vec<_>>();
    fixture.install_config_publication(PROJECT, scope, commit, Some(&files));
}

/// A published project overriding brofile `reviewer` and template `squad`
/// (two reviewers), with no checkout on the daemon host.
fn fixture() -> (CatalogFixture, PublishedScope, BlackboxServer) {
    let fixture = CatalogFixture::new();
    let scope = CatalogFixture::scope(".");
    fixture.add_published_project(PROJECT, &scope);
    publish(
        &fixture,
        &scope,
        COMMIT_ONE,
        &[
            (
                REVIEWER,
                &brofile_json("reviewer", "deepseek", "project-reviewer"),
            ),
            (SQUAD, &template_json("squad", "reviewer", 2)),
        ],
    );
    let server = fixture.server();
    global_configuration(&server);
    (fixture, scope, server)
}

fn project_request(action: &str, name: Option<&str>) -> Value {
    let mut request = json!({"action": action, "scope": "project", "project_dir": PROJECT});
    if let Some(name) = name {
        request["name"] = json!(name);
    }
    request
}

fn save_request(name: &str, brofile: &str, count: u32) -> Value {
    let mut request = project_request("save_template", Some(name));
    request["members"] = json!([{"brofile": brofile, "count": count}]);
    request
}

fn read_body(server: &BlackboxServer, mut request: Value) -> Value {
    let mut body = String::new();
    loop {
        let page = team_discovery_or_catalog(server, &request).unwrap();
        body.push_str(page["body"]["text"].as_str().unwrap());
        match page["body"]["next_cursor"].as_str() {
            Some(cursor) => request["cursor"] = json!(cursor),
            None => return serde_json::from_str(&body).unwrap(),
        }
    }
}

fn team_discovery_or_catalog(server: &BlackboxServer, request: &Value) -> anyhow::Result<Value> {
    let p = params(request.clone());
    if is_project_template_action(&p) && !server.state.project_authority.is_bridge() {
        catalog_project_template_action(server, &p)
    } else {
        team_discovery(server, &p)
    }
}

#[tokio::test]
async fn catalog_project_template_reads_are_exact_scope_bounded_and_content_bound() {
    let fixture = CatalogFixture::new();
    let scope = CatalogFixture::scope(".");
    fixture.add_published_project(PROJECT, &scope);
    let mut files = (0..25)
        .map(|n| {
            (
                format!(".bro/teamplates/tpl-{n:02}.json"),
                template_json(&format!("tpl-{n:02}"), "reviewer", 1),
            )
        })
        .collect::<Vec<_>>();
    // A large template whose exact body needs several pages.
    let large = json!({"name": "large", "members": (0..40)
        .map(|n| json!({"brofile": "reviewer", "alias": format!("lens-{n:02}-{}", "x".repeat(40)), "count": 1}))
        .collect::<Vec<_>>()})
    .to_string();
    files.push((".bro/teamplates/large.json".into(), large.clone()));
    files.push((SQUAD.into(), template_json("squad", "reviewer", 2)));
    let borrowed = files
        .iter()
        .map(|(path, bytes)| (path.as_str(), bytes.as_str()))
        .collect::<Vec<_>>();
    publish(&fixture, &scope, COMMIT_ONE, &borrowed);
    let server = fixture.server();
    global_configuration(&server);

    // Pages cover exactly the project's templates, never global ones.
    let mut names = Vec::new();
    let mut request = project_request("list_templates", None);
    request["limit"] = json!(10);
    loop {
        let page = call(&server, request.clone()).await.unwrap();
        assert_eq!(page["scope"], "project");
        assert_eq!(page["projectId"], PROJECT);
        assert_eq!(page["source"]["accepted_commit"], COMMIT_ONE);
        assert_eq!(page["total"], 27);
        assert!(page["count"].as_u64().unwrap() <= 10);
        for row in page["templates"].as_array().unwrap() {
            names.push(row["name"].as_str().unwrap().to_owned());
        }
        match page["next_offset"].as_u64() {
            Some(next) => request["offset"] = json!(next),
            None => break,
        }
    }
    assert_eq!(names.len(), 27);
    assert!(names.contains(&"squad".to_string()));
    assert!(!names.contains(&"writers".to_string()));
    let filtered = call(&server, project_request("list_templates", Some("writers")))
        .await
        .unwrap();
    assert_eq!(filtered["total"], 0, "a global template never leaks");

    // Same-named project template shadows the global one in exact reads.
    let squad = read_body(&server, project_request("get_template", Some("squad")));
    assert_eq!(squad["members"][0]["brofile"], "reviewer");
    assert_eq!(squad["members"][0]["count"], 2);
    let global = read_body(&server, json!({"action": "get_template", "name": "squad"}));
    assert_eq!(global["members"][0]["count"], 7);
    let missing = call(&server, project_request("get_template", Some("writers")))
        .await
        .unwrap_err();
    assert!(
        missing.contains("Teamplate not found in project"),
        "{missing}"
    );

    // Exact body pages are bounded and bound to scope and content.
    let mut first = project_request("get_template", Some("large"));
    first["body_limit"] = json!(512);
    let page = call(&server, first.clone()).await.unwrap();
    assert_eq!(page["source"]["project_id"], PROJECT);
    assert!(page["body"]["text"].as_str().unwrap().len() <= 512);
    assert!(serde_json::to_vec(&page["body"]).unwrap().len() <= 4096);
    let cursor = page["body"]["next_cursor"].as_str().unwrap().to_owned();
    let mut continued = first.clone();
    continued["cursor"] = json!(cursor);
    assert!(call(&server, continued.clone()).await.is_ok());
    assert_eq!(
        read_body(&server, first.clone()),
        serde_json::from_str::<Value>(&large).unwrap()
    );
    let global_large = team::Teamplate {
        name: "large".into(),
        members: vec![team::TeamplateMember {
            brofile: "writer".into(),
            alias: None,
            count: 1,
        }],
        advisor: None,
        diversity_floor: None,
    };
    team::save_teamplate(&global_large, "global", &server.state.store_dir, None).unwrap();
    let cross_scope = call(
        &server,
        json!({"action": "get_template", "name": "large", "cursor": cursor, "body_limit": 512}),
    )
    .await
    .unwrap_err();
    assert!(cross_scope.contains("cursor"), "{cross_scope}");

    // A new generation with the same template bytes keeps the cursor; a
    // generation that changes them refuses it.
    publish(&fixture, &scope, COMMIT_TWO, &borrowed);
    invalidate(&server);
    let page = call(&server, continued.clone()).await.unwrap();
    assert_eq!(page["source"]["accepted_commit"], COMMIT_TWO);
    let mut changed = borrowed.clone();
    let replacement = template_json("large", "reviewer", 1);
    changed.retain(|(path, _)| *path != ".bro/teamplates/large.json");
    changed.push((".bro/teamplates/large.json", &replacement));
    publish(&fixture, &scope, &commit_three(), &changed);
    invalidate(&server);
    let stale = call(&server, continued).await.unwrap_err();
    assert!(stale.contains("changed"), "{stale}");
}

#[tokio::test]
async fn catalog_project_template_edits_chain_and_take_effect_only_at_publication() {
    let (fixture, scope, server) = fixture();

    let created = call(&server, save_request("fresh", "reviewer", 1))
        .await
        .unwrap();
    assert_eq!(created["saved"], "fresh");
    assert_eq!(created["scope"], "project");
    assert_eq!(created["state"], "queued");
    let first = &created["mutation"];
    assert_eq!(first["mode"], "write");
    assert_eq!(first["project_id"], PROJECT);
    assert_eq!(
        first["landing"]["scope_relative_path"],
        ".bro/teamplates/fresh.json"
    );
    assert_eq!(
        first["landing"]["repository_relative_path"],
        ".bro/teamplates/fresh.json"
    );
    assert!(
        first["expected_sha256"].is_null(),
        "creation asserts absence"
    );
    assert!(first["predecessor"].is_null());
    assert!(
        first["next_step"]
            .as_str()
            .unwrap()
            .contains("Commit and publish")
    );
    let first_id = first["mutation_id"].as_str().unwrap().to_owned();
    {
        let queue = server.state.checkout_mutations.read();
        let row = queue.get(&first_id).unwrap();
        assert_eq!(row.mutation.relative_path, ".bro/teamplates/fresh.json");
        assert_eq!(row.mutation.scope, scope);
        assert_eq!(
            row.mutation.content_json.as_deref(),
            Some(saved_bytes("fresh", "reviewer", 1).as_str())
        );
    }

    // Queued is not published: reads and create stay on the accepted view.
    let listed = call(&server, project_request("list_templates", Some("fresh")))
        .await
        .unwrap();
    assert_eq!(listed["total"], 0);
    let refused = call(
        &server,
        json!({"action": "create", "template": "fresh", "name": "early", "project_dir": PROJECT}),
    )
    .await
    .unwrap_err();
    assert!(refused.contains("Teamplate not found: fresh"), "{refused}");
    assert!(team::load_team("early", &server.state.store_dir).is_none());

    // Replacement and deletion before publication chain on the queued edit.
    let replaced = call(&server, save_request("fresh", "reviewer", 3))
        .await
        .unwrap();
    let second = &replaced["mutation"];
    assert_eq!(second["predecessor"], first_id.as_str());
    assert_eq!(
        second["expected_sha256"],
        content_sha256(&saved_bytes("fresh", "reviewer", 1))
    );
    let unchanged = call(&server, save_request("fresh", "reviewer", 3))
        .await
        .unwrap();
    assert_eq!(unchanged["state"], "unchanged");
    assert!(unchanged.get("mutation").is_none());
    let deleted = call(&server, project_request("delete_template", Some("fresh")))
        .await
        .unwrap();
    assert_eq!(deleted["deleted"], "fresh");
    assert_eq!(deleted["mutation"]["mode"], "delete");
    assert_eq!(deleted["mutation"]["predecessor"], second["mutation_id"]);
    assert_eq!(
        deleted["mutation"]["expected_sha256"],
        content_sha256(&saved_bytes("fresh", "reviewer", 3))
    );
    let again = call(&server, project_request("delete_template", Some("fresh")))
        .await
        .unwrap_err();
    assert!(again.contains("Teamplate not found: fresh"), "{again}");
    let absent = call(&server, project_request("delete_template", Some("never")))
        .await
        .unwrap_err();
    assert!(absent.contains("Teamplate not found: never"), "{absent}");

    // Replacing an accepted template preconditions on its accepted bytes.
    let accepted_squad = template_json("squad", "reviewer", 2);
    let squad = call(&server, save_request("squad", "reviewer", 4))
        .await
        .unwrap();
    assert_eq!(
        squad["mutation"]["expected_sha256"],
        content_sha256(&accepted_squad)
    );
    assert_eq!(
        squad["mutation"]["accepted_generation"],
        server
            .state
            .load_accepted_project_config(PROJECT)
            .unwrap()
            .stamp
            .generation_id()
    );
    let squad_id = squad["mutation"]["mutation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let created = call(
        &server,
        json!({"action": "create", "template": "squad", "name": "before", "project_dir": PROJECT}),
    )
    .await
    .unwrap();
    assert_eq!(
        created["memberCount"], 2,
        "the accepted squad still applies"
    );

    // The receipt was persisted durably: a restart keeps the chain.
    drop(server);
    let server = fixture.server();
    let status = server
        .state
        .project_config_mutation_status(&squad_id)
        .unwrap();
    assert_eq!(status.state, CheckoutMutationProgress::Queued);

    // The owner applies, commits and publishes: reads and create switch.
    server
        .state
        .checkout_mutations
        .write()
        .ack(&squad_id, "applied", None, None, "now")
        .unwrap();
    let saved = saved_bytes("squad", "reviewer", 4);
    publish(
        &fixture,
        &scope,
        COMMIT_TWO,
        &[
            (
                REVIEWER,
                &brofile_json("reviewer", "deepseek", "project-reviewer"),
            ),
            (SQUAD, &saved),
        ],
    );
    invalidate(&server);
    assert_eq!(
        server
            .state
            .project_config_mutation_status(&squad_id)
            .unwrap()
            .state,
        CheckoutMutationProgress::Published
    );
    let squad = read_body(&server, project_request("get_template", Some("squad")));
    assert_eq!(squad["members"][0]["count"], 4);
    let created = call(
        &server,
        json!({"action": "create", "template": "squad", "name": "after", "project_dir": PROJECT}),
    )
    .await
    .unwrap();
    assert_eq!(created["memberCount"], 4);
    assert_eq!(created["templateSource"]["accepted_commit"], COMMIT_TWO);
    // The next edit starts from the newly accepted bytes.
    let next = call(&server, project_request("delete_template", Some("squad")))
        .await
        .unwrap();
    assert_eq!(next["mutation"]["expected_sha256"], content_sha256(&saved));
    assert!(next["mutation"]["predecessor"].is_null());
}

#[tokio::test]
async fn catalog_project_template_conflicts_block_successors_and_recover_from_accepted_bytes() {
    let (fixture, scope, server) = fixture();
    let write = call(&server, save_request("squad", "reviewer", 5))
        .await
        .unwrap();
    let delete = call(&server, project_request("delete_template", Some("squad")))
        .await
        .unwrap();
    let write_id = write["mutation"]["mutation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let delete_id = delete["mutation"]["mutation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(delete["mutation"]["predecessor"], write_id.as_str());

    // The owner's file changed locally: the write conflicts, the delete
    // queued behind it is blocked, and neither overwrites local bytes.
    let local = template_json("squad", "reviewer", 9);
    server
        .state
        .checkout_mutations
        .write()
        .ack_with_observation(
            &write_id,
            "conflicted",
            Some("local bytes preserved".into()),
            None,
            Some(content_sha256(&local)),
            "now",
        )
        .unwrap();
    let conflicted = server
        .state
        .project_config_mutation_status(&write_id)
        .unwrap();
    assert_eq!(conflicted.state, CheckoutMutationProgress::Conflicted);
    assert!(conflicted.next_step.contains("Reconcile"));
    let blocked = server
        .state
        .project_config_mutation_status(&delete_id)
        .unwrap();
    assert_eq!(blocked.state, CheckoutMutationProgress::Blocked);
    assert_eq!(blocked.blocked_by.as_deref(), Some(write_id.as_str()));

    // A re-issued delete is recomputed from accepted bytes with a fresh
    // precondition; it never drops the guard.
    let retry = call(&server, project_request("delete_template", Some("squad")))
        .await
        .unwrap();
    assert_eq!(
        retry["mutation"]["expected_sha256"],
        content_sha256(&template_json("squad", "reviewer", 2))
    );
    assert!(retry["mutation"]["predecessor"].is_null());
    // Its precondition does not match the owner's local bytes either.
    server
        .state
        .checkout_mutations
        .write()
        .ack_with_observation(
            retry["mutation"]["mutation_id"].as_str().unwrap(),
            "conflicted",
            Some("local bytes preserved".into()),
            None,
            Some(content_sha256(&local)),
            "now",
        )
        .unwrap();

    // The owner reconciles by publishing its local bytes; the generation
    // change retires the chain and the next edit builds on the publication.
    publish(
        &fixture,
        &scope,
        COMMIT_TWO,
        &[
            (
                REVIEWER,
                &brofile_json("reviewer", "deepseek", "project-reviewer"),
            ),
            (SQUAD, &local),
        ],
    );
    invalidate(&server);
    let squad = read_body(&server, project_request("get_template", Some("squad")));
    assert_eq!(squad["members"][0]["count"], 9);
    let next = call(&server, save_request("squad", "reviewer", 6))
        .await
        .unwrap();
    assert_eq!(next["mutation"]["expected_sha256"], content_sha256(&local));
    assert_eq!(
        next["mutation"]["accepted_generation"],
        server
            .state
            .load_accepted_project_config(PROJECT)
            .unwrap()
            .stamp
            .generation_id()
    );
}

#[tokio::test]
async fn template_member_validation_uses_accepted_project_first_resolution() {
    let (fixture, scope, server) = fixture();
    let unresolved = call(&server, save_request("panel", "newbie", 1))
        .await
        .unwrap_err();
    assert!(
        unresolved.contains("Brofile not found: newbie"),
        "{unresolved}"
    );

    // Queued brofile edits never activate before publication: a queued new
    // brofile stays unresolved, and a queued replacement or deletion of an
    // accepted one does not invalidate it.
    for (name, edit) in [
        (
            "newbie",
            crate::tools::project_config::ProjectConfigEdit::Write(brofile_json(
                "newbie", "claude", "queued",
            )),
        ),
        (
            "reviewer",
            crate::tools::project_config::ProjectConfigEdit::Delete,
        ),
        (
            "writer",
            crate::tools::project_config::ProjectConfigEdit::Write(brofile_json(
                "writer",
                "claude",
                "queued-writer",
            )),
        ),
    ] {
        server
            .state
            .prepare_project_config_mutation(
                PROJECT,
                &ProjectConfigTargetV1::Brofile(name.into()),
                "test",
                move |_| Ok(Some(edit)),
            )
            .unwrap()
            .unwrap();
    }
    let still = call(&server, save_request("panel", "newbie", 1))
        .await
        .unwrap_err();
    assert!(still.contains("Brofile not found: newbie"), "{still}");
    assert!(
        call(&server, save_request("panel", "reviewer", 1))
            .await
            .is_ok(),
        "the accepted project brofile stays usable"
    );
    assert!(
        call(&server, save_request("writers-panel", "writer", 1))
            .await
            .is_ok(),
        "the global brofile stays usable"
    );

    // Limits and name validation still apply before anything is queued.
    let too_many = call(&server, save_request("huge", "reviewer", 257))
        .await
        .unwrap_err();
    assert!(too_many.contains("exceeds maximum 256"), "{too_many}");
    let mut empty = project_request("save_template", Some("empty"));
    empty["members"] = json!([]);
    assert!(
        call(&server, empty)
            .await
            .unwrap_err()
            .contains("members is required")
    );
    let hidden = call(&server, save_request(".hidden", "reviewer", 1))
        .await
        .unwrap_err();
    assert!(hidden.contains("project template name"), "{hidden}");
    {
        let mut queue = server.state.checkout_mutations.write();
        for name in ["huge", "empty", ".hidden"] {
            let path = format!(".bro/teamplates/{name}.json");
            assert!(
                queue
                    .guarded_write_base(&scope, &path, None)
                    .unwrap()
                    .is_none(),
                "{name} queued nothing"
            );
        }
    }

    // Once the owner publishes the new brofile, the same reference resolves.
    publish(
        &fixture,
        &scope,
        COMMIT_TWO,
        &[
            (
                REVIEWER,
                &brofile_json("reviewer", "deepseek", "project-reviewer"),
            ),
            (SQUAD, &template_json("squad", "reviewer", 2)),
            (
                ".bro/brofiles/newbie.json",
                &brofile_json("newbie", "claude", "queued"),
            ),
        ],
    );
    invalidate(&server);
    let saved = call(&server, save_request("panel", "newbie", 1))
        .await
        .unwrap();
    assert_eq!(saved["state"], "queued");
}

#[tokio::test]
async fn catalog_team_create_roster_and_member_dispatch_resolve_accepted_configuration() {
    let (fixture, _scope, server) = fixture();

    // Same-name project override: the project's squad (two reviewers), not
    // the global squad (seven writers).
    let created = call(
        &server,
        json!({"action": "create", "template": "squad", "name": "panel", "project_dir": PROJECT}),
    )
    .await
    .unwrap();
    assert_eq!(created["templateScope"], "project");
    assert_eq!(created["templateSource"]["source"], "project");
    assert_eq!(created["templateSource"]["accepted_commit"], COMMIT_ONE);
    assert_eq!(created["memberCount"], 2);

    // Roster rows and member dispatch use the accepted project brofile,
    // including after a queued, unpublished replacement.
    server
        .state
        .prepare_project_config_mutation(
            PROJECT,
            &ProjectConfigTargetV1::Brofile("reviewer".into()),
            "test",
            |_| {
                Ok(Some(
                    crate::tools::project_config::ProjectConfigEdit::Write(brofile_json(
                        "reviewer",
                        "claude",
                        "queued-model",
                    )),
                ))
            },
        )
        .unwrap()
        .unwrap();
    let saved = team::load_team("panel", &server.state.store_dir).unwrap();
    let entry = crate::tools::bro_helpers::build_member_entry(
        &saved,
        &saved.members[0],
        &server.state,
        &server.state.idx.read().reindex_config(),
    );
    assert_eq!(entry.model.as_deref(), Some("project-reviewer"));
    assert_eq!(entry.provider, "deepseek");
    let selector = format!("panel::{}", saved.members[0].name);
    let member = server
        .resolve_exec_brofile_for_allocator(&selector, None)
        .unwrap()
        .unwrap();
    assert_eq!(member.model.as_deref(), Some("project-reviewer"));

    // Verified absence of a project override falls back to global, attributed.
    let fallback = call(
        &server,
        json!({"action": "create", "template": "writers", "name": "fallback", "project_dir": PROJECT}),
    )
    .await
    .unwrap();
    assert_eq!(fallback["templateScope"], "global");
    assert_eq!(fallback["templateSource"]["source"], "global_fallback");
    assert_eq!(fallback["templateSource"]["project_id"], PROJECT);

    // A queued project template never activates for create.
    call(&server, save_request("queued-only", "reviewer", 1))
        .await
        .unwrap();
    let refused = call(
        &server,
        json!({"action": "create", "template": "queued-only", "name": "nope", "project_dir": PROJECT}),
    )
    .await
    .unwrap_err();
    assert!(refused.contains("Teamplate not found"), "{refused}");

    // Unavailable, unsupported and invalid views refuse by name, for create
    // and for every project template action, without global fallback.
    let cases = [
        ("p_team_unpublished", "unpublished", None),
        ("p_team_legacy", "legacy", Some(None)),
        (
            "p_team_invalid",
            "invalid",
            Some(Some(r#"{"name":"squad","members":"not-a-list"}"#)),
        ),
    ];
    for (project, relative, config) in cases {
        let project_scope = CatalogFixture::scope(relative);
        fixture.add_published_project(project, &project_scope);
        match config {
            None => {}
            Some(None) => {
                fixture.install_config_publication(project, &project_scope, COMMIT_ONE, None);
            }
            Some(Some(bytes)) => {
                fixture.install_config_publication(
                    project,
                    &project_scope,
                    COMMIT_ONE,
                    Some(&[(SQUAD, bytes.as_bytes())]),
                );
            }
        }
    }
    let server = fixture.server();
    for (project, code) in [
        (
            "p_team_unpublished",
            "error.project_config_publication_unavailable",
        ),
        ("p_team_legacy", "error.project_config_lane_unsupported"),
        ("p_team_invalid", "error.project_config_invalid"),
    ] {
        let create = call(
            &server,
            json!({"action": "create", "template": "writers", "name": "must-not-exist", "project_dir": project}),
        )
        .await
        .unwrap_err();
        assert!(create.contains(code), "{create}");
        assert!(create.contains("No global fallback"), "{create}");
        for mut request in [
            project_request("list_templates", None),
            project_request("get_template", Some("writers")),
            save_request("writers", "writer", 1),
            project_request("delete_template", Some("writers")),
        ] {
            request["project_dir"] = json!(project);
            let error = call(&server, request.clone()).await.unwrap_err();
            assert!(error.contains(code), "{request}: {error}");
            assert!(!error.contains("not-a-list"), "{error}");
        }
    }
    assert!(team::load_team("must-not-exist", &server.state.store_dir).is_none());
}

#[tokio::test]
async fn catalog_global_templates_and_bridge_project_templates_keep_their_stores() {
    // Catalog mode: global template actions use the daemon-owned store.
    let (_fixture, scope, server) = fixture();
    call(
        &server,
        json!({"action": "save_template", "name": "globe", "members": [{"brofile": "writer"}]}),
    )
    .await
    .unwrap();
    let listed = call(
        &server,
        json!({"action": "list_templates", "name": "globe"}),
    )
    .await
    .unwrap();
    assert_eq!(listed["total"], 1);
    assert_eq!(listed["scope"], "global");
    let deleted = call(
        &server,
        json!({"action": "delete_template", "name": "globe"}),
    )
    .await
    .unwrap();
    assert_eq!(deleted["deleted"], "globe");
    assert!(
        server
            .state
            .checkout_mutations
            .read()
            .poll(&std::collections::BTreeSet::from([scope]), true)
            .mutations
            .is_empty(),
        "global template edits queue no checkout mutation"
    );

    // Bridge mode: project scope reads and writes the local checkout.
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let server = BlackboxServer::new(Arc::new(crate::server::state::SharedState::for_test(&root)));
    let reviewer: brofile::Brofile =
        serde_json::from_str(&brofile_json("reviewer", "glm", "bridge")).unwrap();
    brofile::save_brofile(&reviewer, "global", &server.state.store_dir, None).unwrap();
    let project = root.join("bridge-project");
    std::fs::create_dir_all(&project).unwrap();
    let project = project.to_str().unwrap();
    let mut save = save_request("local", "reviewer", 2);
    save["project_dir"] = json!(project);
    let saved = call(&server, save.clone()).await.unwrap();
    assert_eq!(saved["saved"], "local");
    assert!(
        Path::new(project)
            .join(".bro/teamplates/local.json")
            .exists()
    );
    let mut list = project_request("list_templates", None);
    list["project_dir"] = json!(project);
    assert_eq!(call(&server, list).await.unwrap()["total"], 1);
    let mut get = project_request("get_template", Some("local"));
    get["project_dir"] = json!(project);
    assert_eq!(read_body(&server, get)["members"][0]["count"], 2);
    let mut delete = project_request("delete_template", Some("local"));
    delete["project_dir"] = json!(project);
    assert_eq!(call(&server, delete).await.unwrap()["deleted"], "local");
    assert!(
        !Path::new(project)
            .join(".bro/teamplates/local.json")
            .exists()
    );
    save["project_dir"] = json!("relative/project");
    let relative = call(&server, save).await.unwrap_err();
    assert!(relative.contains("absolute"), "{relative}");
    assert!(!root.join("relative").exists());
}

/// Member validation binds to the accepted generation the edit is prepared
/// against: a publication that lands between the handler's first read and
/// preparation's revalidation is the one members must resolve in, and the
/// one the precondition and receipt name.
#[tokio::test]
async fn template_member_validation_binds_to_the_generation_the_edit_is_prepared_against() {
    let fixture = CatalogFixture::new();
    let scope = CatalogFixture::scope(".");
    fixture.add_published_project(PROJECT, &scope);
    let solo = brofile_json("solo", "deepseek", "project-only");
    publish(
        &fixture,
        &scope,
        COMMIT_ONE,
        &[
            (".bro/brofiles/solo.json", &solo),
            (SQUAD, &template_json("squad", "solo", 2)),
        ],
    );
    let server = fixture.server();
    global_configuration(&server);

    // Generation B removes the project-only brofile and changes the target.
    let without_solo = template_json("squad", "writer", 3);
    let mut advanced = false;
    let error = catalog_project_template_action_with_hook(
        &server,
        &params(save_request("squad", "solo", 1)),
        || {
            if !advanced {
                advanced = true;
                publish(&fixture, &scope, COMMIT_TWO, &[(SQUAD, &without_solo)]);
                invalidate(&server);
            }
        },
    )
    .unwrap_err()
    .to_string();
    assert!(advanced);
    assert!(error.contains("Brofile not found: solo"), "{error}");
    assert_eq!(
        server.state.checkout_mutations.read().pending_count(),
        0,
        "the invalid edit is not queued"
    );

    // Generation C publishes the brofile again: the edit validates against
    // C and preconditions on C's template bytes.
    let restored = template_json("squad", "solo", 4);
    let mut advanced = false;
    let receipt = catalog_project_template_action_with_hook(
        &server,
        &params(save_request("squad", "solo", 1)),
        || {
            if !advanced {
                advanced = true;
                publish(
                    &fixture,
                    &scope,
                    &commit_three(),
                    &[(".bro/brofiles/solo.json", &solo), (SQUAD, &restored)],
                );
                invalidate(&server);
            }
        },
    )
    .unwrap();
    let accepted = server.state.load_accepted_project_config(PROJECT).unwrap();
    assert_eq!(accepted.stamp.accepted_commit(), commit_three());
    assert_eq!(
        receipt["mutation"]["accepted_generation"],
        accepted.stamp.generation_id()
    );
    assert_eq!(
        receipt["mutation"]["expected_sha256"],
        content_sha256(&restored)
    );
}
