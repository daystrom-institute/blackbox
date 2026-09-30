//! `bro_mcp` project scope in catalog mode, over a daemon with no checkout on
//! disk: reads come from the accepted `.bbox/mcp.json`, every mutation is a
//! guarded edit for the checkout owner, and unavailable states refuse by
//! name. Global scope, bridge mode and the retired sync keep their behavior.

use super::*;
use crate::checkout_mutations::{CheckoutMutationProgress, content_sha256};
use crate::server::state::catalog_fixture::{COMMIT_ONE, COMMIT_TWO, CatalogFixture};
use bbox_corpus_core::identity::PublishedScope;
use serde_json::{Value, json};

const PROJECT: &str = "p_mcp_accepted";
const SECRETS: &[&str] = &[
    "sample-user",
    "sample-password",
    "private-token",
    "query-secret",
    "inline-credential",
    "opaque-header-value",
];

/// A project store holding one credential-bearing server, one secret
/// reference and one filter per list, as committed bytes.
fn accepted_store() -> String {
    String::from_utf8(
        crate::json_store::to_vec_pretty_newline(&json!({
            "version": 1,
            "servers": {
                "remote": {
                    "type": "http",
                    "url": "https://sample-user:sample-password@remote.example/private-token?q=query-secret",
                    "headers": {
                        "Authorization": "Bearer inline-credential",
                        "X-Custom": "opaque-header-value",
                        "X-Reference": {"$secret": "SYNTHETIC_REFERENCE_NAME"}
                    }
                }
            },
            "filters": {"disallow": ["mcp__remote__drop"], "allow": ["mcp__remote__keep"]}
        }))
        .unwrap(),
    )
    .unwrap()
}

fn install(
    fixture: &CatalogFixture,
    scope: &PublishedScope,
    commit: &str,
    files: Option<&[(&str, &[u8])]>,
) {
    fixture.install_config_publication(PROJECT, scope, commit, files);
}

/// A per-test global MCP store. `BRO_HOME` wins over every other location
/// `global_store_path` consults, so no test reads or writes the operator's
/// store or inherits its filters.
struct IsolatedBroHome {
    _directory: tempfile::TempDir,
    _env: crate::util::TestEnvGuard,
    root: std::path::PathBuf,
}

fn isolated_bro_home() -> IsolatedBroHome {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let mut env = crate::util::TestEnvGuard::new();
    env.set("BRO_HOME", root.join("bro"));
    let store = orchestration::mcp::global_store_path().unwrap();
    assert!(store.starts_with(&root), "{}", store.display());
    IsolatedBroHome {
        _directory: directory,
        _env: env,
        root,
    }
}

fn fixture_with(config_toml: Option<&str>) -> (CatalogFixture, BlackboxServer) {
    let fixture = CatalogFixture::new();
    let scope = CatalogFixture::scope(".");
    fixture.add_published_project(PROJECT, &scope);
    let store = accepted_store();
    let mut files: Vec<(&str, &[u8])> = vec![(".bbox/mcp.json", store.as_bytes())];
    if let Some(toml) = config_toml {
        files.push((".bbox/config.toml", toml.as_bytes()));
    }
    install(&fixture, &scope, COMMIT_ONE, Some(&files));
    let server = fixture.server();
    (fixture, server)
}

fn invalidate(server: &BlackboxServer) {
    server
        .state
        .accepted_publications
        .as_ref()
        .unwrap()
        .invalidate_content(&bbox_corpus_core::project_catalog::ProjectId::parse(PROJECT).unwrap());
}

async fn call(server: &BlackboxServer, value: Value) -> (bool, String) {
    let response = server
        .bro_mcp(Parameters(serde_json::from_value(value).unwrap()))
        .await;
    let wire = serde_json::to_value(&response).unwrap();
    let text = wire["content"][0]["text"].as_str().unwrap().to_string();
    (response.is_error == Some(true), text)
}

async fn ok(server: &BlackboxServer, value: Value) -> String {
    let (error, text) = call(server, value).await;
    assert!(!error, "{text}");
    text
}

async fn ok_json(server: &BlackboxServer, value: Value) -> Value {
    serde_json::from_str(&ok(server, value).await).unwrap()
}

async fn refused(server: &BlackboxServer, value: Value) -> String {
    let (error, text) = call(server, value).await;
    assert!(error, "{text}");
    text
}

fn project(mut value: Value) -> Value {
    value["scope"] = json!("project");
    value["project"] = json!(PROJECT);
    value
}

/// Concatenate an exact body's pages, checking the envelope on each.
async fn exact(server: &BlackboxServer, request: Value) -> (Value, Value) {
    let mut text = String::new();
    let mut cursor: Option<String> = None;
    let mut first = None;
    loop {
        let mut page = request.clone();
        page["body_limit"] = json!(512);
        if let Some(cursor) = &cursor {
            page["cursor"] = json!(cursor);
        }
        let envelope = ok_json(server, page).await;
        text.push_str(envelope["body"]["text"].as_str().unwrap());
        cursor = envelope["body"]["next_cursor"].as_str().map(str::to_string);
        first.get_or_insert(envelope);
        if cursor.is_none() {
            break;
        }
    }
    (first.unwrap(), serde_json::from_str(&text).unwrap())
}

fn assert_redacted(text: &str) {
    for secret in SECRETS {
        assert!(
            !text.contains(secret),
            "response disclosed {secret}: {text}"
        );
    }
}

fn queued_content(server: &BlackboxServer, mutation_id: &str) -> Option<String> {
    server
        .state
        .checkout_mutations
        .read()
        .get(mutation_id)
        .unwrap()
        .mutation
        .content_json
        .clone()
}

fn queued_store(server: &BlackboxServer, mutation_id: &str) -> orchestration::mcp::McpStore {
    serde_json::from_str(&queued_content(server, mutation_id).unwrap()).unwrap()
}

fn dispatch_disallow(server: &BlackboxServer) -> Vec<String> {
    let store = server
        .state
        .dispatch_project_mcp_store(Some(PROJECT))
        .unwrap();
    crate::server::progress::resolve_dispatch_filters(
        crate::orchestration::providers::Provider::Glm,
        store.as_ref(),
        false,
        "task",
        None,
    )
    .unwrap()
    .filters
    .disallow
}

#[tokio::test]
async fn catalog_project_reads_answer_from_the_accepted_store_redacted_and_bounded() {
    let _home = isolated_bro_home();
    let (fixture, server) = fixture_with(None);

    let listing = ok(&server, project(json!({"action": "list"}))).await;
    assert!(
        listing.contains("project scope: rows 1-1 of 1"),
        "{listing}"
    );
    assert!(
        listing.contains("remote: http https://remote.example"),
        "{listing}"
    );
    assert!(
        listing.contains(&format!("Source: project {PROJECT}")),
        "{listing}"
    );
    assert!(listing.contains(COMMIT_ONE), "{listing}");
    assert!(listing.contains("mcp__remote__drop"), "{listing}");
    assert!(!listing.contains("Owner-lane edits"), "{listing}");
    assert_redacted(&listing);

    let (envelope, inventory) = exact(&server, project(json!({"action": "list"}))).await;
    assert_eq!(envelope["scope"], "project");
    assert_eq!(envelope["projectId"], PROJECT);
    assert_eq!(envelope["source"]["accepted_commit"], COMMIT_ONE);
    assert_eq!(envelope["ownerEdits"]["total"], 0);
    assert_eq!(inventory["contributes_to_dispatch"], true);
    assert_eq!(
        inventory["servers"]["remote"]["headers"]["X-Reference"]["$secret"],
        "SYNTHETIC_REFERENCE_NAME"
    );
    assert_eq!(
        inventory["servers"]["remote"]["headers"]["Authorization"]["redacted"],
        true
    );
    assert_eq!(inventory["filters"]["allow"], json!(["mcp__remote__keep"]));
    assert_redacted(&inventory.to_string());

    let (_, detail) = exact(&server, project(json!({"action": "get", "name": "remote"}))).await;
    assert_eq!(detail["name"], "remote");
    assert_eq!(
        detail["config"]["endpoint_origin"],
        "https://remote.example"
    );
    assert_redacted(&detail.to_string());
    let (_, filters) = exact(&server, project(json!({"action": "get_filters"}))).await;
    assert_eq!(
        filters,
        json!({"disallow": ["mcp__remote__drop"], "allow": ["mcp__remote__keep"]})
    );
    let missing = ok(&server, project(json!({"action": "get", "name": "absent"}))).await;
    assert!(
        missing.contains("absent: not registered in the project MCP store"),
        "{missing}"
    );

    // Cursors bind scope and content: the same bytes at a new generation
    // continue, changed bytes refuse.
    let first = ok_json(
        &server,
        project(json!({"action": "get_filters", "body_limit": 16})),
    )
    .await;
    let cursor = first["body"]["next_cursor"].as_str().unwrap().to_string();
    let store = accepted_store();
    install(
        &fixture,
        &CatalogFixture::scope("."),
        COMMIT_TWO,
        Some(&[(".bbox/mcp.json", store.as_bytes())]),
    );
    invalidate(&server);
    let continued = ok_json(
        &server,
        project(json!({"action": "get_filters", "body_limit": 16, "cursor": cursor})),
    )
    .await;
    assert_eq!(continued["source"]["accepted_commit"], COMMIT_TWO);
    let changed =
        json!({"version": 1, "servers": {}, "filters": {"disallow": ["other"]}}).to_string();
    install(
        &fixture,
        &CatalogFixture::scope("."),
        COMMIT_ONE,
        Some(&[(".bbox/mcp.json", changed.as_bytes())]),
    );
    invalidate(&server);
    refused(
        &server,
        project(json!({"action": "get_filters", "body_limit": 16, "cursor": cursor})),
    )
    .await;
}

#[tokio::test]
async fn enablement_comes_from_committed_config_without_changing_dispatch_composition() {
    let _home = isolated_bro_home();
    let mut global = orchestration::mcp::McpStore::new();
    global.filters.disallow = vec!["mcp__global__seeded".into()];
    global
        .save(&orchestration::mcp::global_store_path().unwrap())
        .unwrap();
    for (toml, disabled) in [
        (Some("[mcp]\nenabled = false\n"), true),
        (Some("[mcp]\nenabled = true\n"), false),
        (Some("[project]\naliases = []\n"), false),
        (None, false),
    ] {
        let (_fixture, server) = fixture_with(toml);
        let listing = ok(&server, project(json!({"action": "list"}))).await;
        assert_eq!(
            listing.contains("Project MCP is disabled"),
            disabled,
            "{toml:?}: {listing}"
        );
        assert_eq!(
            listing.contains("remote:"),
            !disabled,
            "{toml:?}: {listing}"
        );
        let (_, inventory) = exact(&server, project(json!({"action": "list"}))).await;
        assert_eq!(inventory["contributes_to_dispatch"], !disabled, "{toml:?}");
        assert!(inventory["servers"]["remote"].is_object(), "{toml:?}");
        // Dispatch composition is unchanged by the flag: global, then the
        // accepted project filters, then the recursion guard.
        let disallow = dispatch_disallow(&server);
        let position = |wanted: &dyn Fn(&str) -> bool| {
            disallow
                .iter()
                .position(|pattern| wanted(pattern))
                .unwrap_or_else(|| panic!("{toml:?}: {disallow:?}"))
        };
        let global = position(&|p| p == "mcp__global__seeded");
        let project = position(&|p| p == "mcp__remote__drop");
        let guard = position(&|p| p.contains("bro_exec"));
        assert!(
            global < project && project < guard,
            "{toml:?}: {disallow:?}"
        );
    }

    // Malformed enablement input refuses every project action by name.
    let (_fixture, server) = fixture_with(Some("[mcp]\nenabled = \"sk-not-a-bool\"\n"));
    for request in [
        json!({"action": "list"}),
        json!({"action": "get_filters"}),
        json!({"action": "allow", "pattern": "mcp__x__y"}),
    ] {
        let text = refused(&server, project(request)).await;
        assert!(text.contains("error.project_config_invalid"), "{text}");
        assert!(!text.contains("sk-not-a-bool"), "{text}");
    }
    assert_eq!(server.state.checkout_mutations.read().pending_count(), 0);
}

#[tokio::test]
async fn every_mutation_action_queues_a_guarded_owner_edit_that_chains_before_publication() {
    let _home = isolated_bro_home();
    let (fixture, server) = fixture_with(None);
    let accepted = accepted_store();

    let first = ok_json(
        &server,
        project(
            json!({"action": "add", "name": "alpha", "url": "https://alpha.example/mcp",
            "headers": {"Authorization": "Bearer inline-credential"}}),
        ),
    )
    .await;
    assert_eq!(first["state"], "queued");
    assert_eq!(first["projectId"], PROJECT);
    let receipt = &first["mutation"];
    assert_eq!(
        receipt["landing"]["repository_relative_path"],
        ".bbox/mcp.json"
    );
    assert_eq!(receipt["expected_sha256"], json!(content_sha256(&accepted)));
    assert_eq!(receipt["predecessor"], Value::Null);
    assert!(
        receipt["next_step"]
            .as_str()
            .unwrap()
            .contains("Commit and publish")
    );
    assert_eq!(first["ownerEdits"]["total"], 1);
    assert_redacted(&first.to_string());
    let first_id = receipt["mutation_id"].as_str().unwrap().to_string();

    let mut previous = first_id.clone();
    let mut ids = vec![first_id.clone()];
    for request in [
        json!({"action": "add", "name": "beta", "url": "https://beta.example/mcp", "transport": "sse"}),
        json!({"action": "remove", "name": "remote"}),
        json!({"action": "allow", "pattern": "alpha(read_file)"}),
        json!({"action": "disallow", "pattern": "mcp__beta__.write"}),
    ] {
        let reply = ok_json(&server, project(request.clone())).await;
        assert_eq!(reply["state"], "queued", "{request}");
        let mutation = &reply["mutation"];
        assert_eq!(mutation["predecessor"], json!(previous), "{request}");
        assert_eq!(
            mutation["expected_sha256"],
            json!(content_sha256(&queued_content(&server, &previous).unwrap())),
            "{request}: the precondition is the predecessor's exact bytes"
        );
        previous = mutation["mutation_id"].as_str().unwrap().to_string();
        ids.push(previous.clone());
    }
    let chained = queued_store(&server, &previous);
    assert_eq!(
        chained.servers.keys().collect::<Vec<_>>(),
        vec!["alpha", "beta"],
        "both adds and the remove are preserved"
    );
    // Filter normalization and append order match the local store.
    assert_eq!(
        chained.filters.allow,
        vec!["mcp__remote__keep", "mcp__alpha__read_file"]
    );
    assert_eq!(
        chained.filters.disallow,
        vec!["mcp__remote__drop", "mcp__beta__write"]
    );
    // Secret representation matches a local save: header values are stored
    // as given, and never echoed.
    match &chained.servers["alpha"] {
        orchestration::mcp::McpServerConfig::Http { headers, .. } => assert_eq!(
            headers["Authorization"],
            orchestration::mcp::SecretString::Plain("Bearer inline-credential".into())
        ),
        other => panic!("unexpected {other:?}"),
    }
    // Queued bytes are exactly what a local save writes.
    assert_eq!(
        queued_content(&server, &previous).unwrap().as_bytes(),
        crate::json_store::to_vec_pretty_newline(&chained)
            .unwrap()
            .as_slice()
    );

    // No-op edits over the edit base queue nothing.
    for (request, detail) in [
        (
            json!({"action": "remove", "name": "remote"}),
            "not registered",
        ),
        (
            json!({"action": "allow", "pattern": "mcp__alpha__read_file"}),
            "already present",
        ),
        (
            json!({"action": "add", "name": "beta", "url": "https://beta.example/mcp", "transport": "sse"}),
            "already has this exact server configuration",
        ),
    ] {
        let reply = ok_json(&server, project(request.clone())).await;
        assert_eq!(reply["state"], "unchanged", "{request}");
        assert!(
            reply["detail"].as_str().unwrap().contains(detail),
            "{reply}"
        );
        assert!(reply.get("mutation").is_none());
    }

    let cleared = ok_json(&server, project(json!({"action": "clear_filters"}))).await;
    assert_eq!(cleared["mutation"]["predecessor"], json!(previous));
    let cleared_id = cleared["mutation"]["mutation_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(queued_store(&server, &cleared_id).filters.is_empty());
    let again = ok_json(&server, project(json!({"action": "clear_filters"}))).await;
    assert_eq!(again["state"], "unchanged");
    ids.push(cleared_id.clone());

    // Reads and dispatch stay on the accepted generation, and the reads name
    // the edits that are not reflected yet.
    let listing = ok(&server, project(json!({"action": "list"}))).await;
    assert!(listing.contains("remote:"), "{listing}");
    assert!(!listing.contains("alpha:"), "{listing}");
    assert!(
        listing.contains(&format!(
            "Owner-lane edits not yet reflected here ({}",
            ids.len()
        )),
        "{listing}"
    );
    assert!(
        listing.contains(&format!("{cleared_id}: queued")),
        "{listing}"
    );
    let (envelope, _) = exact(&server, project(json!({"action": "list"}))).await;
    assert_eq!(envelope["ownerEdits"]["total"], ids.len());
    assert_eq!(
        envelope["ownerEdits"]["shown"].as_array().unwrap().len(),
        ids.len().min(8)
    );
    assert!(dispatch_disallow(&server).contains(&"mcp__remote__drop".to_string()));

    // Restart keeps the chain, and the next edit builds on it.
    server
        .state
        .persist_checkout_mutations_durable()
        .await
        .unwrap();
    let server = fixture.server();
    let next = ok_json(
        &server,
        project(json!({"action": "disallow", "pattern": "mcp__alpha__admin"})),
    )
    .await;
    assert_eq!(next["mutation"]["predecessor"], json!(cleared_id));
    let final_id = next["mutation"]["mutation_id"]
        .as_str()
        .unwrap()
        .to_string();

    // The owner applies and publishes the chain's final bytes: reads and
    // dispatch switch together and the lane settles.
    for id in ids.iter().chain([&final_id]) {
        server
            .state
            .checkout_mutations
            .write()
            .ack(id, "applied", None, None, "now")
            .unwrap();
    }
    let published = queued_content(&server, &final_id).unwrap();
    install(
        &fixture,
        &CatalogFixture::scope("."),
        COMMIT_TWO,
        Some(&[(".bbox/mcp.json", published.as_bytes())]),
    );
    invalidate(&server);
    let listing = ok(&server, project(json!({"action": "list"}))).await;
    assert!(
        listing.contains("alpha:") && listing.contains("beta:"),
        "{listing}"
    );
    assert!(!listing.contains("remote:"), "{listing}");
    assert!(!listing.contains("Owner-lane edits"), "{listing}");
    assert!(listing.contains(COMMIT_TWO), "{listing}");
    let disallow = dispatch_disallow(&server);
    assert!(disallow.contains(&"mcp__alpha__admin".to_string()));
    assert!(!disallow.contains(&"mcp__remote__drop".to_string()));
    assert_eq!(
        server
            .state
            .project_config_mutation_status(&final_id)
            .unwrap()
            .state,
        CheckoutMutationProgress::Published
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_requests_compose_and_share_failures_along_the_chain() {
    let _home = isolated_bro_home();
    let (_fixture, server) = fixture_with(None);
    let requests = [
        json!({"action": "add", "name": "one", "url": "https://one.example"}),
        json!({"action": "add", "name": "two", "url": "https://two.example"}),
        json!({"action": "allow", "pattern": "mcp__one__x"}),
        json!({"action": "disallow", "pattern": "mcp__two__y"}),
    ];
    let handles = requests
        .iter()
        .map(|request| {
            let server = server.clone();
            let request = project(request.clone());
            tokio::spawn(async move { ok_json(&server, request).await })
        })
        .collect::<Vec<_>>();
    let mut receipts = Vec::new();
    for handle in handles {
        receipts.push(handle.await.unwrap()["mutation"].clone());
    }
    // Exactly one chain: one head on the accepted bytes, each other edit on
    // one distinct predecessor's exact bytes.
    let heads = receipts
        .iter()
        .filter(|receipt| receipt["predecessor"].is_null())
        .count();
    assert_eq!(heads, 1);
    let mut predecessors = receipts
        .iter()
        .filter_map(|receipt| receipt["predecessor"].as_str())
        .collect::<Vec<_>>();
    predecessors.sort();
    predecessors.dedup();
    assert_eq!(predecessors.len(), 3);
    for receipt in &receipts {
        if let Some(predecessor) = receipt["predecessor"].as_str() {
            assert_eq!(
                receipt["expected_sha256"],
                json!(content_sha256(
                    &queued_content(&server, predecessor).unwrap()
                ))
            );
        }
    }
    let tail = receipts
        .iter()
        .map(|receipt| receipt["mutation_id"].as_str().unwrap())
        .find(|id| !predecessors.contains(id))
        .unwrap();
    let merged = queued_store(&server, tail);
    assert!(merged.servers.contains_key("one") && merged.servers.contains_key("two"));
    assert!(merged.servers.contains_key("remote"));
    assert!(merged.filters.allow.contains(&"mcp__one__x".to_string()));
    assert!(merged.filters.disallow.contains(&"mcp__two__y".to_string()));

    // The owner's file did not match the head's precondition: the head
    // conflicts and every successor settles blocked behind it.
    let head = receipts
        .iter()
        .find(|receipt| receipt["predecessor"].is_null())
        .unwrap()["mutation_id"]
        .as_str()
        .unwrap()
        .to_string();
    server
        .state
        .checkout_mutations
        .write()
        .ack_with_observation(
            &head,
            "conflicted",
            Some("local bytes preserved".into()),
            None,
            Some("e".repeat(64)),
            "now",
        )
        .unwrap();
    let (envelope, _) = exact(&server, project(json!({"action": "list"}))).await;
    let shown = envelope["ownerEdits"]["shown"].as_array().unwrap();
    assert_eq!(shown.len(), 4);
    assert_eq!(shown[0]["mutation_id"], json!(head));
    assert_eq!(shown[0]["state"], "conflicted");
    assert_eq!(shown[0]["observed_sha256"], json!("e".repeat(64)));
    assert!(
        shown[0]["next_step"]
            .as_str()
            .unwrap()
            .contains("Reconcile")
    );
    for blocked in &shown[1..] {
        assert_eq!(blocked["state"], "blocked");
        assert_eq!(blocked["blocked_by"], json!(head));
    }
    assert_redacted(&envelope.to_string());

    // Recovery recomputes from the accepted bytes with a fresh
    // precondition; nothing drops the guard or forces the owner's file.
    let retry = ok_json(
        &server,
        project(json!({"action": "add", "name": "one", "url": "https://one.example"})),
    )
    .await;
    assert_eq!(retry["mutation"]["predecessor"], Value::Null);
    assert_eq!(
        retry["mutation"]["expected_sha256"],
        json!(content_sha256(&accepted_store()))
    );
    assert_eq!(retry["ownerEdits"]["total"], 1, "{retry}");
}

#[tokio::test]
async fn divergent_publication_under_a_pending_chain_refuses_without_queuing() {
    let _home = isolated_bro_home();
    let (fixture, server) = fixture_with(None);
    let queued = ok_json(
        &server,
        project(json!({"action": "allow", "pattern": "mcp__first__edit"})),
    )
    .await;
    let queued_id = queued["mutation"]["mutation_id"].as_str().unwrap();
    let divergent =
        json!({"version": 1, "servers": {}, "filters": {"allow": ["owner-local"]}}).to_string();
    install(
        &fixture,
        &CatalogFixture::scope("."),
        COMMIT_TWO,
        Some(&[(".bbox/mcp.json", divergent.as_bytes())]),
    );
    invalidate(&server);
    let pending_before = server.state.checkout_mutations.read().pending_count();
    let text = refused(
        &server,
        project(json!({"action": "allow", "pattern": "mcp__second__edit"})),
    )
    .await;
    assert!(text.contains("error.checkout_mutation_conflict"), "{text}");
    assert!(text.contains(queued_id), "{text}");
    assert!(!text.contains("owner-local"), "{text}");
    assert_eq!(
        server.state.checkout_mutations.read().pending_count(),
        pending_before
    );
    // Reads and dispatch follow the owner's publication.
    let (_, filters) = exact(&server, project(json!({"action": "get_filters"}))).await;
    assert_eq!(filters["allow"], json!(["owner-local"]));
}

#[tokio::test]
async fn empty_unavailable_unsupported_unknown_and_oversized_states_are_named() {
    let _home = isolated_bro_home();
    let fixture = CatalogFixture::new();
    let scope = CatalogFixture::scope(".");
    fixture.add_published_project(PROJECT, &scope);
    let unpublished = "p_mcp_unpublished";
    fixture.add_published_project(unpublished, &CatalogFixture::scope("unpublished"));
    let legacy = "p_mcp_legacy";
    let legacy_scope = CatalogFixture::scope("legacy");
    fixture.add_published_project(legacy, &legacy_scope);
    fixture.install_config_publication(legacy, &legacy_scope, COMMIT_ONE, None);
    // A verified empty configuration lane: no .bbox/mcp.json at all.
    install(&fixture, &scope, COMMIT_ONE, Some(&[]));
    // An accepted store too large for the mutation lane.
    let mut large = orchestration::mcp::McpStore::new();
    for n in 0..1200 {
        large.servers.insert(
            format!("server-{n:04}-{}", "x".repeat(160)),
            orchestration::mcp::McpServerConfig::Http {
                url: "https://large.example/mcp".into(),
                headers: Default::default(),
                exclude_tools: Vec::new(),
            },
        );
    }
    let large = crate::json_store::to_vec_pretty_newline(&large).unwrap();
    assert!(large.len() > bbox_code_source::MAX_CHECKOUT_MUTATION_CONTENT_BYTES);
    let big = "p_mcp_large";
    let big_scope = CatalogFixture::scope("large");
    fixture.add_published_project(big, &big_scope);
    fixture.install_config_publication(
        big,
        &big_scope,
        COMMIT_ONE,
        Some(&[(".bbox/mcp.json", large.as_slice())]),
    );
    let server = fixture.server();

    let listing = ok(&server, project(json!({"action": "list"}))).await;
    assert!(
        listing.contains("No MCP servers registered in project scope"),
        "{listing}"
    );
    let (_, inventory) = exact(&server, project(json!({"action": "list"}))).await;
    assert_eq!(inventory["servers"], json!({}));
    let created = ok_json(
        &server,
        project(json!({"action": "add", "name": "first", "url": "https://first.example"})),
    )
    .await;
    assert_eq!(
        created["mutation"]["expected_sha256"],
        Value::Null,
        "creating the file asserts its absence"
    );
    let removed_absent = ok_json(
        &server,
        project(json!({"action": "remove", "name": "never"})),
    )
    .await;
    assert_eq!(removed_absent["state"], "unchanged");
    let pending = server.state.checkout_mutations.read().pending_count();

    for (selector, code) in [
        (unpublished, "error.project_config_publication_unavailable"),
        (legacy, "error.project_config_lane_unsupported"),
        (
            "/not/a/catalog/project",
            "error.project_config_project_unknown",
        ),
    ] {
        for request in [
            json!({"action": "list"}),
            json!({"action": "list", "body_limit": 4096}),
            json!({"action": "get", "name": "first"}),
            json!({"action": "get_filters"}),
            json!({"action": "add", "name": "x", "url": "https://x.example"}),
            json!({"action": "remove", "name": "x"}),
            json!({"action": "allow", "pattern": "p"}),
            json!({"action": "disallow", "pattern": "p"}),
            json!({"action": "clear_filters"}),
        ] {
            let mut request = request;
            request["scope"] = json!("project");
            request["project"] = json!(selector);
            let text = refused(&server, request.clone()).await;
            assert!(text.contains(code), "{request}: {text}");
            assert!(!text.contains("mcp_config_locality_required"), "{text}");
        }
    }
    assert_eq!(
        server.state.checkout_mutations.read().pending_count(),
        pending
    );

    // Invalid mutation parameters refuse before any project access.
    let stdio = refused(
        &server,
        json!({"action": "add", "name": "x", "url": "https://x.example", "transport": "stdio",
            "scope": "project", "project": "/not/a/catalog/project"}),
    )
    .await;
    assert!(stdio.contains("http and sse"), "{stdio}");

    // An accepted store too large for the mutation lane stays readable in
    // bounded pages, and an edit to it refuses without queuing.
    let listing = ok(
        &server,
        json!({"action": "list", "scope": "project", "project": big, "limit": 100}),
    )
    .await;
    assert!(serde_json::to_vec(&listing).unwrap().len() <= 32 * 1024);
    let page = ok_json(
        &server,
        json!({"action": "list", "scope": "project", "project": big, "body_limit": 4096}),
    )
    .await;
    assert!(page["body"]["next_cursor"].is_string());
    let text = refused(
        &server,
        json!({"action": "allow", "pattern": "p", "scope": "project", "project": big}),
    )
    .await;
    assert!(text.contains("error.mcp_project_store_too_large"), "{text}");
    assert_eq!(
        server.state.checkout_mutations.read().pending_count(),
        pending
    );
}

#[tokio::test]
async fn sync_global_and_bridge_keep_their_lanes() {
    let (_fixture, server) = fixture_with(None);
    // Retired sync refuses before project selection or store access: an
    // unknown selector is not even resolved.
    for selector in [PROJECT, "/not/a/catalog/project"] {
        let text = refused(
            &server,
            json!({"action": "sync", "scope": "project", "project": selector}),
        )
        .await;
        assert!(text.contains("error.mcp_sync_retired"), "{text}");
        assert!(!text.contains("project_config"), "{text}");
    }

    // Global scope in catalog mode keeps the daemon-owned store.
    let home = isolated_bro_home();
    let saved = ok(
        &server,
        json!({"action": "add", "name": "global-server", "url": "https://global.example"}),
    )
    .await;
    assert!(
        saved.contains("Saved global-server to the global MCP store"),
        "{saved}"
    );
    let global_path = orchestration::mcp::global_store_path().unwrap();
    assert!(global_path.starts_with(&home.root));
    let global = orchestration::mcp::McpStore::load(&global_path).unwrap();
    assert!(global.servers.contains_key("global-server"));
    let project_listing = ok(&server, project(json!({"action": "list"}))).await;
    assert!(
        !project_listing.contains("global-server"),
        "{project_listing}"
    );
    assert_eq!(server.state.checkout_mutations.read().pending_count(), 0);

    // Bridge mode writes the local project store directly.
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let bridge = BlackboxServer::new(std::sync::Arc::new(
        crate::server::state::SharedState::for_test(&root),
    ));
    let checkout = root.join("bridge-project");
    std::fs::create_dir_all(&checkout).unwrap();
    let selector = checkout.to_str().unwrap();
    let saved = ok(
        &bridge,
        json!({"action": "add", "name": "local", "url": "https://local.example",
            "scope": "project", "project": selector}),
    )
    .await;
    assert!(
        saved.contains("Saved local to the project MCP store"),
        "{saved}"
    );
    let local =
        orchestration::mcp::McpStore::load(&orchestration::mcp::project_store_path(&checkout))
            .unwrap();
    assert!(local.servers.contains_key("local"));
    assert_eq!(bridge.state.checkout_mutations.read().pending_count(), 0);
    drop(home);
}

#[tokio::test]
async fn a_read_holding_an_older_snapshot_never_retires_a_newer_pending_edit() {
    let _home = isolated_bro_home();
    let (fixture, server) = fixture_with(None);
    let a = accepted_store();
    // A read captures accepted configuration A and pauses.
    let held = server.state.load_accepted_project_config(PROJECT).unwrap();
    // Publication advances to B, and an edit changes B back to A's bytes.
    let b = json!({"version": 1, "servers": {}, "filters": {"allow": ["published-b"]}}).to_string();
    install(
        &fixture,
        &CatalogFixture::scope("."),
        COMMIT_TWO,
        Some(&[(".bbox/mcp.json", b.as_bytes())]),
    );
    invalidate(&server);
    let back = server
        .state
        .prepare_project_config_mutation(
            PROJECT,
            &ProjectConfigTargetV1::McpStore,
            "test",
            |base| {
                assert_eq!(base, Some(b.as_str()));
                Ok(Some(ProjectConfigEdit::Write(a.clone())))
            },
        )
        .unwrap()
        .unwrap();
    assert_eq!(back.expected_sha256, Some(content_sha256(&b)));
    // The paused read resumes with its A snapshot. It must not treat the
    // pending B -> A edit as published.
    let open = server
        .state
        .project_config_open_edits(&held, &ProjectConfigTargetV1::McpStore);
    assert_eq!(
        open.iter()
            .map(|status| status.mutation_id.as_str())
            .collect::<Vec<_>>(),
        vec![back.mutation_id.as_str()]
    );
    assert_eq!(open[0].state, CheckoutMutationProgress::Queued);
    assert_eq!(
        server
            .state
            .project_config_mutation_status(&back.mutation_id)
            .unwrap()
            .state,
        CheckoutMutationProgress::Queued
    );
    // The next edit still composes on the pending edit.
    let next = ok_json(
        &server,
        project(json!({"action": "allow", "pattern": "mcp__after__edit"})),
    )
    .await;
    assert_eq!(next["mutation"]["predecessor"], json!(back.mutation_id));
    assert_eq!(
        next["mutation"]["expected_sha256"],
        json!(content_sha256(&a))
    );
}

#[tokio::test]
async fn owner_edit_envelopes_stay_bounded_for_long_escaped_scopes() {
    let _home = isolated_bro_home();
    let fixture = CatalogFixture::new();
    // The longest-escaping valid scope shape: 255-quote components.
    let relative = vec!["\"".repeat(255); 8].join("/");
    let scope = CatalogFixture::scope(&relative);
    fixture.add_published_project(PROJECT, &scope);
    let store = accepted_store();
    install(
        &fixture,
        &scope,
        COMMIT_ONE,
        Some(&[(".bbox/mcp.json", store.as_bytes())]),
    );
    let server = fixture.server();
    let complete = |response: &CallToolResult| {
        let bytes = serde_json::to_vec(response).unwrap().len();
        assert_ne!(response.is_error, Some(true), "{response:?}");
        assert!(
            bytes <= BlackboxServer::MCP_RESPONSE_CAP_BYTES,
            "{bytes} bytes"
        );
    };
    for n in 0..10 {
        let request = project(json!({"action": "allow", "pattern": format!("mcp__edit__{n}")}));
        complete(
            &server
                .bro_mcp(Parameters(serde_json::from_value(request).unwrap()))
                .await,
        );
    }
    for request in [
        json!({"action": "list"}),
        json!({"action": "list", "body_limit": 4096}),
        json!({"action": "get_filters", "body_limit": 4096}),
        json!({"action": "get", "name": "remote", "body_limit": 4096}),
        json!({"action": "disallow", "pattern": "mcp__edit__last"}),
    ] {
        complete(
            &server
                .bro_mcp(Parameters(
                    serde_json::from_value(project(request)).unwrap(),
                ))
                .await,
        );
    }
    let summary = ok_json(&server, project(json!({"action": "get_filters"}))).await;
    assert_eq!(summary["ownerEdits"]["total"], 11);
    assert_eq!(summary["ownerEdits"]["shown"].as_array().unwrap().len(), 8);
    assert_eq!(summary["ownerEdits"]["omitted"], 3);
    assert!(summary["ownerEdits"]["shown"][0].get("landing").is_none());
    // Every open edit, with its exact landing path, pages through the
    // exact inventory.
    let (_, inventory) = exact(&server, project(json!({"action": "list"}))).await;
    let edits = inventory["owner_edits"].as_array().unwrap();
    assert_eq!(edits.len(), 11);
    assert_eq!(
        edits[0]["landing"]["repository_relative_path"],
        json!(format!("{relative}/.bbox/mcp.json"))
    );
}
