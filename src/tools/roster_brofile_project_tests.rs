//! Catalog-mode `bro_brofile` project scope over a daemon with no checkout on
//! disk: `project_dir` is an absolute owner-host path that only selects a
//! catalog project, reads come from its accepted publication, and create and
//! delete queue guarded mutations for its checkout owner.

use super::*;
use crate::checkout_mutations::{CheckoutMutationProgress, content_sha256};
use crate::orchestration::project_config::ProjectConfigSource;
use crate::server::state::catalog_fixture::{COMMIT_ONE, COMMIT_TWO, CatalogFixture};
use bbox_corpus_core::identity::PublishedScope;
use orchestration::brofile;

const PROJECT: &str = "p_brofile_project";
const OTHER: &str = "p_brofile_other";
/// Owner-host checkout paths. Nothing exists at them on the daemon host.
const OWNER: &str = "/owner-host/checkouts/app";
const REVIEWER: &str = ".bro/brofiles/reviewer.json";

fn commit_three() -> String {
    "3".repeat(40)
}

fn brofile_json(name: &str, provider: &str, model: &str) -> String {
    json!({"name": name, "provider": provider, "model": model}).to_string()
}

/// The exact bytes `create` queues: what a bridge-mode create writes.
fn created_bytes(name: &str, provider: &str, model: &str) -> String {
    let value: brofile::Brofile =
        serde_json::from_str(&brofile_json(name, provider, model)).unwrap();
    serde_json::to_string_pretty(&value).unwrap()
}

fn text(result: &CallToolResult) -> String {
    let wire = serde_json::to_value(result).unwrap();
    wire["content"][0]["text"].as_str().unwrap().to_string()
}

async fn call(server: &BlackboxServer, request: Value) -> Result<Value, String> {
    let result = server
        .bro_brofile(Parameters(serde_json::from_value(request).unwrap()))
        .await;
    if result.is_error == Some(true) {
        Err(text(&result))
    } else {
        Ok(serde_json::from_str(&text(&result)).unwrap())
    }
}

fn request(action: &str, name: Option<&str>) -> Value {
    request_at(OWNER, action, name)
}

fn request_at(owner: &str, action: &str, name: Option<&str>) -> Value {
    let mut request = json!({"action": action, "scope": "project", "project_dir": owner});
    if let Some(name) = name {
        request["name"] = json!(name);
    }
    request
}

fn create(name: &str, provider: &str, model: &str) -> Value {
    let mut request = request("create", Some(name));
    request["provider"] = json!(provider);
    request["model"] = json!(model);
    request
}

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

/// Global brofiles `reviewer` and `writer`.
fn global_brofiles(server: &BlackboxServer) {
    for (name, model) in [("reviewer", "global-reviewer"), ("writer", "global-writer")] {
        let value: brofile::Brofile =
            serde_json::from_str(&brofile_json(name, "glm", model)).unwrap();
        brofile::save_brofile(&value, "global", &server.state.store_dir, None).unwrap();
    }
}

fn publish(
    fixture: &CatalogFixture,
    project_id: &str,
    scope: &PublishedScope,
    commit: &str,
    files: &[(&str, &str)],
) {
    let files = files
        .iter()
        .map(|(path, bytes)| (*path, bytes.as_bytes()))
        .collect::<Vec<_>>();
    fixture.install_config_publication(project_id, scope, commit, Some(&files));
}

/// A published project overriding `reviewer`, selected by its owner-host
/// checkout path.
fn fixture() -> (CatalogFixture, PublishedScope, BlackboxServer) {
    let fixture = CatalogFixture::new();
    let scope = CatalogFixture::scope(".");
    fixture.add_published_project(PROJECT, &scope);
    fixture.attach_checkout(
        PROJECT,
        &scope,
        Path::new(OWNER),
        "att_22222222222222222222222222222222",
    );
    publish(
        &fixture,
        PROJECT,
        &scope,
        COMMIT_ONE,
        &[(
            REVIEWER,
            &brofile_json("reviewer", "deepseek", "project-reviewer"),
        )],
    );
    let server = fixture.server();
    global_brofiles(&server);
    (fixture, scope, server)
}

fn dispatch_model(server: &BlackboxServer, name: &str) -> Option<(String, ProjectConfigSource)> {
    server
        .state
        .resolve_config_brofile(name, Some(OWNER))
        .unwrap()
        .map(|resolved| (resolved.value.model.clone().unwrap(), resolved.source))
}

async fn read_body(server: &BlackboxServer, mut request: Value) -> Value {
    let mut body = String::new();
    loop {
        let page = call(server, request.clone()).await.unwrap();
        body.push_str(page["body"]["text"].as_str().unwrap());
        match page["body"]["next_cursor"].as_str() {
            Some(cursor) => request["cursor"] = json!(cursor),
            None => return serde_json::from_str(&body).unwrap(),
        }
    }
}

#[tokio::test]
async fn catalog_project_brofile_reads_are_exact_scope_bounded_and_content_bound() {
    let fixture = CatalogFixture::new();
    let scope = CatalogFixture::scope(".");
    fixture.add_published_project(PROJECT, &scope);
    fixture.attach_checkout(
        PROJECT,
        &scope,
        Path::new(OWNER),
        "att_22222222222222222222222222222222",
    );
    let mut files = (0..25)
        .map(|n| {
            let name = format!("bf-{n:02}");
            let provider = if n % 2 == 0 { "brodex" } else { "deepseek" };
            (
                format!(".bro/brofiles/{name}.json"),
                brofile_json(&name, provider, "project-model"),
            )
        })
        .collect::<Vec<_>>();
    // A large brofile whose exact body needs several pages.
    let large =
        json!({"name": "large", "provider": "brodex", "lens": "l".repeat(9000)}).to_string();
    files.push((".bro/brofiles/large.json".into(), large.clone()));
    files.push((
        REVIEWER.into(),
        brofile_json("reviewer", "deepseek", "project-reviewer"),
    ));
    let borrowed = files
        .iter()
        .map(|(path, bytes)| (path.as_str(), bytes.as_str()))
        .collect::<Vec<_>>();
    publish(&fixture, PROJECT, &scope, COMMIT_ONE, &borrowed);
    let server = fixture.server();
    global_brofiles(&server);

    // Pages cover exactly the project's brofiles, never global ones.
    let mut names = Vec::new();
    let mut page_request = request("list", None);
    page_request["limit"] = json!(10);
    loop {
        let page = call(&server, page_request.clone()).await.unwrap();
        assert_eq!(page["scope"], "project");
        assert_eq!(page["projectId"], PROJECT);
        assert_eq!(page["source"]["accepted_commit"], COMMIT_ONE);
        assert_eq!(page["total"], 27);
        assert!(page["count"].as_u64().unwrap() <= 10);
        for row in page["brofiles"].as_array().unwrap() {
            names.push(row["name"].as_str().unwrap().to_owned());
        }
        match page["next_offset"].as_u64() {
            Some(next) => page_request["offset"] = json!(next),
            None => break,
        }
    }
    assert_eq!(names.len(), 27);
    assert!(names.contains(&"reviewer".to_string()));
    assert!(!names.contains(&"writer".to_string()));

    // Filters and pagination limits are the global lane's.
    let writer = call(&server, request("list", Some("writer")))
        .await
        .unwrap();
    assert_eq!(writer["total"], 0, "a global brofile never leaks");
    let mut by_provider = request("list", None);
    by_provider["provider"] = json!("deepseek");
    by_provider["limit"] = json!(1000);
    let by_provider = call(&server, by_provider).await.unwrap();
    assert_eq!(by_provider["limit"], 100);
    assert_eq!(by_provider["total"], 13, "12 odd bf-NN plus reviewer");
    let mut unknown_provider = request("list", None);
    unknown_provider["provider"] = json!("not-a-provider");
    let refused = call(&server, unknown_provider).await.unwrap_err();
    assert!(refused.contains("Unknown provider"), "{refused}");

    // The same-named project brofile shadows the global one in exact reads.
    let reviewer = read_body(&server, request("get", Some("reviewer"))).await;
    assert_eq!(reviewer["model"], "project-reviewer");
    let global = read_body(&server, json!({"action": "get", "name": "reviewer"})).await;
    assert_eq!(global["model"], "global-reviewer");
    let missing = call(&server, request("get", Some("writer")))
        .await
        .unwrap_err();
    assert!(
        missing.contains("Brofile not found in project scope: writer"),
        "{missing}"
    );

    // Exact body pages are bounded and bound to scope and content.
    let mut first = request("get", Some("large"));
    first["body_limit"] = json!(512);
    let page = call(&server, first.clone()).await.unwrap();
    assert_eq!(page["source"]["project_id"], PROJECT);
    assert_eq!(page["summary"]["name"], "large");
    assert!(page["body"]["text"].as_str().unwrap().len() <= 512);
    assert!(serde_json::to_vec(&page["body"]).unwrap().len() <= 4096);
    let cursor = page["body"]["next_cursor"].as_str().unwrap().to_owned();
    let mut continued = first.clone();
    continued["cursor"] = json!(cursor);
    assert!(call(&server, continued.clone()).await.is_ok());
    assert_eq!(
        read_body(&server, first.clone()).await,
        serde_json::from_str::<Value>(&large).unwrap()
    );
    let global_large: brofile::Brofile = serde_json::from_str(&large).unwrap();
    brofile::save_brofile(&global_large, "global", &server.state.store_dir, None).unwrap();
    let cross_scope = call(
        &server,
        json!({"action": "get", "name": "large", "cursor": cursor, "body_limit": 512}),
    )
    .await
    .unwrap_err();
    assert!(cross_scope.contains("cursor"), "{cross_scope}");

    // A new generation with the same brofile bytes keeps the cursor; a
    // generation that changes them refuses it.
    publish(&fixture, PROJECT, &scope, COMMIT_TWO, &borrowed);
    invalidate(&server, PROJECT);
    let page = call(&server, continued.clone()).await.unwrap();
    assert_eq!(page["source"]["accepted_commit"], COMMIT_TWO);
    let mut changed = borrowed.clone();
    let replacement = json!({"name": "large", "provider": "brodex", "lens": "short"}).to_string();
    changed.retain(|(path, _)| *path != ".bro/brofiles/large.json");
    changed.push((".bro/brofiles/large.json", &replacement));
    publish(&fixture, PROJECT, &scope, &commit_three(), &changed);
    invalidate(&server, PROJECT);
    let stale = call(&server, continued).await.unwrap_err();
    assert!(stale.contains("changed"), "{stale}");
}

#[tokio::test]
async fn catalog_project_brofile_edits_chain_and_take_effect_only_at_publication() {
    let (fixture, scope, server) = fixture();

    // Create a new name: absence precondition, no predecessor.
    let created = call(&server, create("fresh", "brodex", "fresh-one"))
        .await
        .unwrap();
    assert_eq!(created["created"], "fresh");
    assert_eq!(created["replaces"], false);
    assert_eq!(created["scope"], "project");
    assert_eq!(created["projectId"], PROJECT);
    assert_eq!(created["state"], "queued");
    assert_eq!(created["summary"]["model"], "fresh-one");
    let first = &created["mutation"];
    assert_eq!(first["mode"], "write");
    assert_eq!(first["project_id"], PROJECT);
    assert_eq!(
        first["landing"]["repository_relative_path"],
        ".bro/brofiles/fresh.json"
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
        assert_eq!(queue.pending_count(), 1, "only the selected path is queued");
        let row = queue.get(&first_id).unwrap();
        assert_eq!(row.mutation.relative_path, ".bro/brofiles/fresh.json");
        assert_eq!(row.mutation.scope, scope);
        assert_eq!(
            row.mutation.content_json.as_deref(),
            Some(created_bytes("fresh", "brodex", "fresh-one").as_str())
        );
    }

    // Queued is not published: reads and dispatch stay on the accepted view.
    let listed = call(&server, request("list", Some("fresh"))).await.unwrap();
    assert_eq!(listed["total"], 0);
    assert!(
        call(&server, request("get", Some("fresh")))
            .await
            .unwrap_err()
            .contains("Brofile not found in project scope")
    );
    assert!(dispatch_model(&server, "fresh").is_none());

    // Replacement, an unchanged rewrite, deletion and recreation before
    // publication chain on the queued edits.
    let replaced = call(&server, create("fresh", "brodex", "fresh-two"))
        .await
        .unwrap();
    assert_eq!(replaced["replaces"], true);
    let second = &replaced["mutation"];
    assert_eq!(second["predecessor"], first_id.as_str());
    assert_eq!(
        second["expected_sha256"],
        content_sha256(&created_bytes("fresh", "brodex", "fresh-one"))
    );
    let unchanged = call(&server, create("fresh", "brodex", "fresh-two"))
        .await
        .unwrap();
    assert_eq!(unchanged["state"], "unchanged");
    assert!(unchanged.get("mutation").is_none());
    let deleted = call(&server, request("delete", Some("fresh")))
        .await
        .unwrap();
    assert_eq!(deleted["deleted"], "fresh");
    assert_eq!(deleted["state"], "queued");
    assert_eq!(deleted["mutation"]["mode"], "delete");
    assert_eq!(deleted["mutation"]["predecessor"], second["mutation_id"]);
    assert_eq!(
        deleted["mutation"]["expected_sha256"],
        content_sha256(&created_bytes("fresh", "brodex", "fresh-two"))
    );
    let again = call(&server, request("delete", Some("fresh")))
        .await
        .unwrap_err();
    assert!(again.contains("Brofile not found: fresh"), "{again}");
    let absent = call(&server, request("delete", Some("never")))
        .await
        .unwrap_err();
    assert!(absent.contains("Brofile not found: never"), "{absent}");
    assert!(absent.contains("nothing was queued"), "{absent}");
    let recreated = call(&server, create("fresh", "brodex", "fresh-three"))
        .await
        .unwrap();
    assert_eq!(recreated["replaces"], false);
    assert!(recreated["mutation"]["expected_sha256"].is_null());
    assert_eq!(
        recreated["mutation"]["predecessor"],
        deleted["mutation"]["mutation_id"]
    );
    assert_eq!(server.state.checkout_mutations.read().pending_count(), 4);

    // Replacing an accepted brofile preconditions on its accepted bytes.
    let accepted_reviewer = brofile_json("reviewer", "deepseek", "project-reviewer");
    let reviewer = call(&server, create("reviewer", "brodex", "reviewer-edit"))
        .await
        .unwrap();
    assert_eq!(reviewer["replaces"], true);
    assert_eq!(
        reviewer["mutation"]["expected_sha256"],
        content_sha256(&accepted_reviewer)
    );
    assert!(reviewer["mutation"]["predecessor"].is_null());
    assert_eq!(
        reviewer["mutation"]["accepted_generation"],
        server
            .state
            .load_accepted_project_config(PROJECT)
            .unwrap()
            .stamp
            .generation_id()
    );
    let reviewer_id = reviewer["mutation"]["mutation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        dispatch_model(&server, "reviewer").unwrap().0,
        "project-reviewer"
    );

    // Another project's edits never chain on this one's.
    let nested = PublishedScope::try_new("repo_other", ".").unwrap();
    fixture.add_published_project(OTHER, &nested);
    // A second attachment needs its own checkout identity, which this
    // fixture records as a marker file; nothing else exists there.
    let other_owner = fixture.root().join("other-owner");
    fixture.attach_overlay_checkout(
        OTHER,
        &nested,
        &other_owner,
        "att_33333333333333333333333333333333",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb01",
        true,
    );
    let other_owner = other_owner.to_str().unwrap();
    publish(&fixture, OTHER, &nested, COMMIT_ONE, &[]);
    drop(server);
    let server = fixture.server();
    let mut other = request_at(other_owner, "create", Some("fresh"));
    other["provider"] = json!("brodex");
    let other = call(&server, other).await.unwrap();
    assert_eq!(other["projectId"], OTHER);
    assert!(other["mutation"]["expected_sha256"].is_null());
    assert!(other["mutation"]["predecessor"].is_null());
    assert_eq!(other["mutation"]["landing"]["repo_id"], "repo_other");
    assert_eq!(
        other["mutation"]["landing"]["repository_relative_path"],
        ".bro/brofiles/fresh.json"
    );
    let other_list = call(&server, request_at(other_owner, "list", None))
        .await
        .unwrap();
    assert_eq!(other_list["total"], 0);

    // The receipts were persisted durably: a restart keeps the chain.
    drop(server);
    let server = fixture.server();
    global_brofiles(&server);
    let status = server
        .state
        .project_config_mutation_status(&reviewer_id)
        .unwrap();
    assert_eq!(status.state, CheckoutMutationProgress::Queued);

    // The owner applies: delivered is still not published.
    for id in [&first_id, &reviewer_id] {
        server
            .state
            .checkout_mutations
            .write()
            .ack(id, "applied", None, None, "now")
            .unwrap();
    }
    let status = server
        .state
        .project_config_mutation_status(&reviewer_id)
        .unwrap();
    assert_eq!(status.state, CheckoutMutationProgress::Delivered);
    assert_eq!(
        dispatch_model(&server, "reviewer").unwrap().0,
        "project-reviewer",
        "delivery is not publication"
    );

    // The owner commits and publishes: reads and dispatch switch together.
    let edited = created_bytes("reviewer", "brodex", "reviewer-edit");
    let fresh = created_bytes("fresh", "brodex", "fresh-three");
    publish(
        &fixture,
        PROJECT,
        &scope,
        COMMIT_TWO,
        &[(REVIEWER, &edited), (".bro/brofiles/fresh.json", &fresh)],
    );
    invalidate(&server, PROJECT);
    let status = server
        .state
        .project_config_mutation_status(&reviewer_id)
        .unwrap();
    assert_eq!(status.state, CheckoutMutationProgress::Published);
    let listed = call(&server, request("list", None)).await.unwrap();
    assert_eq!(listed["total"], 2);
    assert_eq!(listed["source"]["accepted_commit"], COMMIT_TWO);
    let got = read_body(&server, request("get", Some("fresh"))).await;
    assert_eq!(got["model"], "fresh-three");
    let (model, source) = dispatch_model(&server, "reviewer").unwrap();
    assert_eq!(model, "reviewer-edit");
    assert!(matches!(source, ProjectConfigSource::Project(_)));

    // A publication without the override: dispatch falls back to global with
    // attribution, while exact project reads stay project-only.
    publish(
        &fixture,
        PROJECT,
        &scope,
        &commit_three(),
        &[(".bro/brofiles/fresh.json", &fresh)],
    );
    invalidate(&server, PROJECT);
    let (model, source) = dispatch_model(&server, "reviewer").unwrap();
    assert_eq!(model, "global-reviewer");
    match source {
        ProjectConfigSource::GlobalFallback(provenance) => {
            assert_eq!(provenance.project_id, PROJECT);
            assert_eq!(provenance.accepted_commit, commit_three());
        }
        other => panic!("expected attributed global fallback, got {other:?}"),
    }
    assert!(
        call(&server, request("get", Some("reviewer")))
            .await
            .unwrap_err()
            .contains("Brofile not found in project scope")
    );
    // The next edit starts from the newly accepted absence.
    let next = call(&server, create("reviewer", "brodex", "back"))
        .await
        .unwrap();
    assert_eq!(next["replaces"], false);
    assert!(next["mutation"]["expected_sha256"].is_null());
    assert!(next["mutation"]["predecessor"].is_null());
}

#[tokio::test]
async fn catalog_project_brofile_conflicts_keep_owner_bytes_and_block_successors() {
    let (_fixture, _scope, server) = fixture();
    let first = call(&server, create("reviewer", "brodex", "one"))
        .await
        .unwrap();
    let second = call(&server, create("reviewer", "brodex", "two"))
        .await
        .unwrap();
    let first_id = first["mutation"]["mutation_id"].as_str().unwrap();
    let second_id = second["mutation"]["mutation_id"].as_str().unwrap();

    // The owner's file held other bytes, so it kept them and reported what
    // it observed.
    let observed = "d".repeat(64);
    server
        .state
        .checkout_mutations
        .write()
        .ack_with_observation(
            first_id,
            "conflicted",
            Some("local bytes preserved".into()),
            None,
            Some(observed.clone()),
            "now",
        )
        .unwrap();
    let conflicted = server
        .state
        .project_config_mutation_status(first_id)
        .unwrap();
    assert_eq!(conflicted.state, CheckoutMutationProgress::Conflicted);
    assert_eq!(conflicted.observed_sha256, Some(Some(observed)));
    assert!(conflicted.next_step.contains("Reconcile"));
    let blocked = server
        .state
        .project_config_mutation_status(second_id)
        .unwrap();
    assert_eq!(blocked.state, CheckoutMutationProgress::Blocked);
    assert_eq!(blocked.blocked_by.as_deref(), Some(first_id));
    assert_eq!(
        dispatch_model(&server, "reviewer").unwrap().0,
        "project-reviewer"
    );

    // A retry recomputes from the accepted bytes and keeps a precondition;
    // it never degrades to a force write.
    let retry = call(&server, create("reviewer", "brodex", "three"))
        .await
        .unwrap();
    assert_eq!(
        retry["mutation"]["expected_sha256"],
        content_sha256(&brofile_json("reviewer", "deepseek", "project-reviewer"))
    );
    assert!(retry["mutation"]["predecessor"].is_null());
}

#[tokio::test]
async fn catalog_project_brofile_refusals_name_their_lane_and_global_stays_available() {
    let (fixture, _scope, server) = fixture();
    let create_at = |owner: &str| {
        let mut request = request_at(owner, "create", Some("reviewer"));
        request["provider"] = json!("brodex");
        request
    };
    let every_action = |owner: &str| {
        vec![
            request_at(owner, "list", None),
            request_at(owner, "get", Some("reviewer")),
            create_at(owner),
            request_at(owner, "delete", Some("reviewer")),
        ]
    };

    // The selector stays an absolute owner-host path.
    for relative in every_action("owner/checkout") {
        let refused = call(&server, relative).await.unwrap_err();
        assert!(refused.contains("absolute"), "{refused}");
    }

    // Unavailable, pre-lane and invalid publications refuse by name. Each
    // attachment path on the daemon host holds a valid look-alike brofile
    // that must never stand in for the accepted view.
    let states = [
        (
            "p_brofile_unpublished",
            None,
            "error.project_config_publication_unavailable",
        ),
        (
            "p_brofile_prelane",
            Some(None),
            "error.project_config_lane_unsupported",
        ),
        (
            "p_brofile_invalid",
            Some(Some("{not json")),
            "error.project_config_invalid",
        ),
    ];
    let mut owners = Vec::new();
    for (index, (project, publication, _)) in states.iter().enumerate() {
        let scope = PublishedScope::try_new(format!("repo_state{index}"), ".").unwrap();
        let owner = fixture.root().join(format!("state-owner-{index}"));
        fixture.add_published_project(project, &scope);
        fixture.attach_overlay_checkout(
            project,
            &scope,
            &owner,
            &format!("att_{}{index}", "4".repeat(31)),
            &format!("{}{index}", "c".repeat(31)),
            true,
        );
        std::fs::create_dir_all(owner.join(".bro/brofiles")).unwrap();
        std::fs::write(
            owner.join(REVIEWER),
            brofile_json("reviewer", "brodex", "daemon-local"),
        )
        .unwrap();
        owners.push(owner.to_str().unwrap().to_owned());
        match publication {
            None => {}
            Some(None) => {
                fixture.install_config_publication(project, &scope, COMMIT_ONE, None);
            }
            Some(Some(bytes)) => {
                publish(&fixture, project, &scope, COMMIT_ONE, &[(REVIEWER, *bytes)])
            }
        }
    }
    let server = {
        drop(server);
        let server = fixture.server();
        global_brofiles(&server);
        server
    };
    for ((_, _, code), owner) in states.into_iter().zip(&owners) {
        let owner = owner.as_str();
        for request in every_action(owner) {
            let refused = call(&server, request).await.unwrap_err();
            assert!(refused.contains(code), "{refused}");
            assert!(!refused.contains("{not json"), "{refused}");
            assert!(!refused.contains("daemon-local"), "{refused}");
        }
        // Dispatch refuses the same way instead of falling back to global.
        let error = server
            .state
            .resolve_config_brofile("reviewer", Some(owner))
            .unwrap_err();
        assert_eq!(error.code(), code);
    }
    assert_eq!(server.state.checkout_mutations.read().pending_count(), 0);

    // Name, provider and scope validation run before any selection.
    for (request, expected) in [
        (request("get", Some("../escape")), "not a path"),
        (
            {
                let mut request = request("create", Some("reviewer"));
                request["provider"] = json!("not-a-provider");
                request
            },
            "valid provider is required",
        ),
        (
            request("create", Some("reviewer")),
            "valid provider is required",
        ),
        (
            json!({"action": "list", "scope": "workspace", "project_dir": OWNER}),
            "scope",
        ),
        (
            json!({"action": "list", "scope": "global", "project_dir": OWNER}),
            "project_dir applies only to scope=project",
        ),
        (
            json!({"action": "list", "scope": "project"}),
            "project_dir is required for scope=project",
        ),
        (
            json!({"action": "list_accounts", "scope": "project", "project_dir": OWNER}),
            "account configuration lives in the daemon-owned store",
        ),
        (
            json!({"action": "set_provider_default", "scope": "project", "project_dir": OWNER, "provider": "brodex", "account": "a"}),
            "account configuration lives in the daemon-owned store",
        ),
    ] {
        let refused = call(&server, request).await.unwrap_err();
        assert!(refused.contains(expected), "{expected}: {refused}");
    }
    assert_eq!(server.state.checkout_mutations.read().pending_count(), 0);

    // Global scope keeps its daemon-owned store in catalog mode.
    let created = call(
        &server,
        json!({"action": "create", "name": "global-new", "provider": "brodex"}),
    )
    .await
    .unwrap();
    assert_eq!(created["scope"], "global");
    assert!(
        brofile::load_brofile("global-new", &server.state.store_dir.join("brofiles")).is_some()
    );
    let listed = call(&server, json!({"action": "list"})).await.unwrap();
    assert_eq!(listed["total"], 3);
    let deleted = call(&server, json!({"action": "delete", "name": "global-new"}))
        .await
        .unwrap();
    assert_eq!(deleted["deleted"], "global-new");
    assert_eq!(server.state.checkout_mutations.read().pending_count(), 0);
}
