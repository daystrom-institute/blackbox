use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use bbox_code_source::source_selector;
use bbox_code_source_store::{ActivationRecord, CodeSourceStore};
use bbox_config::config::{self, LoadOptions};
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::project_catalog::ProjectId;
use bbox_corpus_index::index::TranscriptIndex;
use bbox_edge_sidecar::manifest::ManifestIndex;
use bbox_indexing::project_catalog_migration::project_catalog_migration_store_limits;
use bbox_indexing::project_catalog_migration_lock::ProjectCatalogMigrationLock;
use bbox_indexing::project_catalog_store::ProjectCatalogStore;
use bbox_vectors::VectorStore;
use serde_json::Value;
use tempfile::tempdir;

fn write(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().expect("fixture path has a parent")).unwrap();
    fs::write(path, bytes).unwrap();
}

fn write_collected_activation(state: &Path, project_id: &str, generation_id: &str) {
    let activation = ActivationRecord {
        version: 1,
        project_id: project_id.to_string(),
        generation_id: generation_id.to_string(),
        selector: format!(
            "{}:m0123456789abcdef",
            source_selector(project_id, generation_id)
        ),
        snapshot_id: format!("collected-{}", "a".repeat(32)),
        document_count: 0,
        entity_inventory_sha256: "b".repeat(64),
        current_chunk_targets: Default::default(),
        activated_unix_secs: 1,
        cutback_pending: false,
        diagnostic: None,
    };
    write(
        &state.join(format!("code-sources/activations/{project_id}.json")),
        &serde_json::to_vec(&activation).unwrap(),
    );
}

fn initialize_empty_owner_state(root: &Path, config_path: &Path) {
    let state = root.join("state");
    let index_path = state.join("index");
    let index = TranscriptIndex::open_or_create_with_records(
        &index_path,
        Vec::new(),
        None,
        state.join("projects.json"),
        state.join("blackbox-knowledge.json"),
        state.join("blackbox-threads.json"),
        state.join("blackbox-roadmap.json"),
        std::sync::Arc::new(bbox_corpus_index::index::StaticProjectRecordsProvider::empty()),
    )
    .unwrap();
    drop(index);
    write(
        &index_path.join("_meta.json"),
        br#"{"version":2,"rows":{}}"#,
    );

    VectorStore::open(state.join("vectors")).unwrap();
    ManifestIndex::new()
        .write_atomic(&state.join("edges"))
        .unwrap();
    fs::create_dir_all(state.join("git_meta")).unwrap();

    for (name, body) in [
        ("blackbox-knowledge.json", r#"{"version":1,"entries":[]}"#),
        ("blackbox-gaps.json", r#"{"version":1,"gaps":[]}"#),
        ("blackbox-threads.json", r#"{"version":1,"threads":[]}"#),
        ("blackbox-notes.json", r#"{"version":1,"notes":[]}"#),
        ("blackbox-pins.json", r#"{"version":1,"pins":[]}"#),
        (
            "blackbox-roadmap.json",
            r#"{"version":1,"items":[],"edges":[]}"#,
        ),
    ] {
        write(&state.join(name), body.as_bytes());
    }

    for directory in [
        state.join("packets"),
        state.join("artifacts"),
        state.join("bro/badgey/proposals"),
        state.join("bro/whiteboards"),
    ] {
        fs::create_dir_all(directory).unwrap();
    }
    write(&state.join("bro/tasks.json"), b"[]");
    write(
        &state.join("bro/slack-channel-bindings.json"),
        br#"{"bindings":{}}"#,
    );
    write(
        &state.join("bro/slack-proposal-links.json"),
        br#"{"order":[],"links":{},"by_proposal":{}}"#,
    );

    let config = config::load_with(LoadOptions {
        config_path: Some(config_path.to_path_buf()),
        ..Default::default()
    })
    .unwrap();
    CodeSourceStore::open(
        state.join("code-sources"),
        project_catalog_migration_store_limits(&config),
    )
    .unwrap();
}

fn run(args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_blackbox"));
    command
        .args(args)
        .env_remove("BLACKBOX_CONFIG")
        .env_remove("BLACKBOX_STATE_DIR")
        .env_remove("BLACKBOX_VECTORS_PATH")
        .env_remove("TRANSCRIPT_SEARCH_INDEX_PATH");
    command.output().unwrap()
}

/// Run one offline command with the corpus index pinned inside the test's own
/// root. The index is the single retire-probe input that does not derive from
/// `state_dir`, so leaving it unset would let the probe read the host's real
/// index instead of this test's isolated state.
fn run_with_isolated_index(args: &[&str], index_path: &Path) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_blackbox"));
    command
        .args(args)
        .env_remove("BLACKBOX_CONFIG")
        .env_remove("BLACKBOX_STATE_DIR")
        .env_remove("BLACKBOX_VECTORS_PATH")
        .env("TRANSCRIPT_SEARCH_INDEX_PATH", index_path);
    command.output().unwrap()
}

fn run_with_isolated_index_and_env(
    args: &[&str],
    index_path: &Path,
    key: &str,
    value: &str,
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_blackbox"));
    command
        .args(args)
        .env_remove("BLACKBOX_CONFIG")
        .env_remove("BLACKBOX_STATE_DIR")
        .env_remove("BLACKBOX_VECTORS_PATH")
        .env("TRANSCRIPT_SEARCH_INDEX_PATH", index_path)
        .env(key, value);
    command.output().unwrap()
}

/// An isolated state root holding an initialized empty v2 catalog store plus
/// the configuration file every offline command resolves its evidence roots
/// from. Returns the state dir, the projects path, the config path, and the
/// isolated corpus index path.
fn isolated_state_root(root: &Path) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let state = root.join("state");
    fs::create_dir_all(&state).unwrap();
    let projects_path = state.join("projects.json");
    drop(ProjectCatalogStore::initialize_empty(&projects_path).unwrap());
    let config_path = root.join("config.toml");
    // `vectors_dir` is written explicitly: the vector root resolves to the
    // PLATFORM state directory by default (R33F1), so a fixture that omitted
    // it would inventory and discharge the host's real vector store.
    let vectors = state.join("vectors");
    write(
        &config_path,
        format!("[paths]\nstate_dir = {state:?}\nvectors_dir = {vectors:?}\n").as_bytes(),
    );
    (state, projects_path, config_path, root.join("index"))
}

/// Add one published catalog project and return its minted id.
fn add_published_project(projects: &str, repo_id: &str, relpath: &str, created_at: &str) -> String {
    let added = success_json(&run(&[
        "project-catalog",
        "add",
        "--projects-path",
        projects,
        "--repo-id",
        repo_id,
        "--relpath",
        relpath,
        "--display-name",
        "offline evidence fixture",
        "--created-at",
        created_at,
    ]));
    added["result"]["project_id"].as_str().unwrap().to_string()
}

fn success_json(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    serde_json::from_slice(&output.stdout).unwrap()
}

fn retirement_plan_hash(
    projects: &str,
    project_id: &str,
    config: &str,
    bro_home: Option<&Path>,
    index_path: &Path,
) -> String {
    let mut args = vec![
        "project-catalog",
        "retirement-journal",
        "--projects-path",
        projects,
        "--project",
        project_id,
        "--config",
        config,
    ];
    let bro_home_text = bro_home.map(|path| path.to_str().unwrap());
    if let Some(path) = bro_home_text {
        args.extend(["--bro-home", path]);
    }
    success_json(&run_with_isolated_index(&args, index_path))["result"]["plan_hash"]
        .as_str()
        .unwrap()
        .to_string()
}

fn assert_redacted(value: &Value, private_root: &Path) {
    let serialized = serde_json::to_string(value).unwrap();
    assert!(
        !serialized.contains(private_root.to_string_lossy().as_ref()),
        "public CLI envelope leaked a local path: {serialized}"
    );
}

#[test]
fn release_inventory_installs_the_offline_cli_deliberately() {
    let manifest: toml::Value = toml::from_str(include_str!("../Cargo.toml")).unwrap();
    let bins = manifest["bin"].as_array().unwrap();
    assert!(bins.iter().any(|bin| {
        bin["name"].as_str() == Some("blackbox")
            && bin["path"].as_str() == Some("src/bin/blackbox.rs")
    }));

    let flake = include_str!("../flake.nix");
    assert!(flake.contains(r#"blackbox = mkProductApp "blackbox";"#));

    for install_doc in [
        include_str!("../README.md"),
        include_str!("../docs/getting-started.md"),
        include_str!("../docs/operating-blackbox.md"),
        include_str!("../docs/operations.md"),
    ] {
        assert!(install_doc.contains("target/release/blackbox"));
    }
}

#[test]
fn help_and_version_do_not_load_config_or_create_state() {
    let directory = tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let invalid_config = root.join("invalid.toml");
    let state = root.join("must-not-exist");
    write(&invalid_config, b"this is not valid TOML =");

    for args in [
        vec![
            "project-catalog",
            "migrate",
            "--help",
            "--config",
            invalid_config.to_str().unwrap(),
            "--state-dir",
            state.to_str().unwrap(),
        ],
        vec!["--version"],
    ] {
        let output = run(&args);
        assert!(output.status.success());
        assert!(!output.stdout.is_empty());
        assert!(output.stderr.is_empty());
        assert!(!state.exists());
    }
}

#[test]
fn parser_failures_use_clap_output_instead_of_json() {
    let output = run(&[
        "project-catalog",
        "migrate",
        "--preflight",
        "--report",
        "/tmp/report.json",
    ]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--resolution"));
}

#[test]
fn domain_errors_use_one_redacted_json_envelope() {
    let directory = tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let config_path = root.join("config.toml");
    write(
        &config_path,
        format!(
            "[paths]\nstate_dir = {:?}\nvectors_dir = {:?}\n",
            root.join("protected"),
            root.join("protected").join("vectors")
        )
        .as_bytes(),
    );

    let output = run(&[
        "project-catalog",
        "verify",
        "--root",
        root.join("missing-rehearsal").to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
    ]);
    assert!(!output.status.success());
    let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["version"], 1);
    assert_eq!(envelope["command"], "project_catalog_verify");
    assert!(envelope.get("result").is_none());
    let envelope = envelope.as_object().unwrap();
    assert_eq!(envelope.len(), 3);
    assert!(envelope.contains_key("version"));
    assert!(envelope.contains_key("command"));
    assert!(envelope.contains_key("error"));
    let error = envelope.get("error").unwrap().as_object().unwrap();
    assert_eq!(error.len(), 2);
    assert!(error.contains_key("code"));
    assert!(error.contains_key("message"));
    assert_redacted(&Value::Object(envelope.clone()), &root);
}

#[test]
fn cli_runs_clean_preflight_apply_and_fresh_verify() {
    let directory = tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let rehearsal_root = root.join("rehearsal");
    let protected_root = root.join("protected");
    let config_path = root.join("config.toml");
    write(
        &config_path,
        format!(
            "[paths]\nstate_dir = {:?}\nvectors_dir = {:?}\n",
            protected_root,
            protected_root.join("vectors")
        )
        .as_bytes(),
    );
    fs::create_dir_all(&protected_root).unwrap();
    initialize_empty_owner_state(&rehearsal_root, &config_path);

    let report = rehearsal_root.join("review/report.json");
    let resolution = rehearsal_root.join("review/resolution.json");
    let local_paths = rehearsal_root.join("review/local-paths.json");
    let preflight = success_json(&run(&[
        "project-catalog",
        "migrate",
        "--preflight",
        "--report",
        report.to_str().unwrap(),
        "--resolution",
        resolution.to_str().unwrap(),
        "--include-local-paths",
        local_paths.to_str().unwrap(),
        "--state-dir",
        rehearsal_root.join("state").to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
    ]));
    assert_eq!(preflight["command"], "project_catalog_migrate_preflight");
    assert_eq!(preflight["result"]["status"], "clean");
    assert!(report.is_file());
    assert!(resolution.is_file());
    assert!(local_paths.is_file());
    assert_redacted(&preflight, &root);

    let held =
        ProjectCatalogMigrationLock::acquire_shared(&rehearsal_root.join("state/projects.json"))
            .unwrap();
    let refused = run(&[
        "project-catalog",
        "migrate",
        "--apply",
        "--report",
        report.to_str().unwrap(),
        "--resolution",
        resolution.to_str().unwrap(),
        "--rehearsal-root",
        rehearsal_root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
    ]);
    assert!(!refused.status.success());
    let refused: Value = serde_json::from_slice(&refused.stdout).unwrap();
    assert_eq!(
        refused["error"]["code"],
        "error.project_catalog_lifetime_lock_busy"
    );
    assert_redacted(&refused, &root);
    drop(held);

    let source_path = rehearsal_root.join("state/blackbox-knowledge.json");
    let source_before = fs::read(&source_path).unwrap();
    let mut drifted_source = source_before.clone();
    drifted_source.push(b'\n');
    fs::write(&source_path, &drifted_source).unwrap();
    let drifted = run(&[
        "project-catalog",
        "migrate",
        "--apply",
        "--report",
        report.to_str().unwrap(),
        "--resolution",
        resolution.to_str().unwrap(),
        "--rehearsal-root",
        rehearsal_root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
    ]);
    assert!(!drifted.status.success());
    let drifted: Value = serde_json::from_slice(&drifted.stdout).unwrap();
    assert_ne!(drifted["error"]["code"], Value::Null);
    assert_redacted(&drifted, &root);
    fs::write(&source_path, source_before).unwrap();
    let apply = success_json(&run(&[
        "project-catalog",
        "migrate",
        "--apply",
        "--report",
        report.to_str().unwrap(),
        "--resolution",
        resolution.to_str().unwrap(),
        "--rehearsal-root",
        rehearsal_root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
    ]));
    assert_eq!(apply["command"], "project_catalog_migrate_apply");
    assert_eq!(apply["result"]["outcome"], "applied");
    assert_redacted(&apply, &root);

    let verify = success_json(&run(&[
        "project-catalog",
        "verify",
        "--root",
        rehearsal_root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
    ]));
    assert_eq!(verify["command"], "project_catalog_verify");
    assert_eq!(
        verify["result"]["transaction_id"],
        apply["result"]["verification"]["transaction_id"]
    );
    assert_eq!(verify["result"]["attached_project_count"], 0);
    assert_eq!(verify["result"]["omitted_catalog_count"], 0);
    assert_redacted(&verify, &root);
}

#[test]
fn admin_subcommands_round_trip_on_an_isolated_v2_store() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    // An initialized empty v2 store is the administered substrate; the
    // exclusive lifetime lock is free because no daemon shares this root.
    let (_state, projects_path, config_path, index_path) = isolated_state_root(&root);
    let projects = projects_path.to_str().unwrap();
    let config = config_path.to_str().unwrap();

    // Published add with an initial alias.
    let added = success_json(&run(&[
        "project-catalog",
        "add",
        "--projects-path",
        projects,
        "--repo-id",
        "clifamily",
        "--relpath",
        ".",
        "--display-name",
        "cli remote",
        "--alias",
        "cli-alias",
        "--created-at",
        "2026-07-24T00:00:00Z",
    ]));
    let project_id = added["result"]["project_id"].as_str().unwrap().to_string();
    assert!(project_id.starts_with("p_"));

    // Legacy-local add.
    let local = success_json(&run(&[
        "project-catalog",
        "add",
        "--projects-path",
        projects,
        "--legacy-local",
        "--display-name",
        "cli local",
        "--created-at",
        "2026-07-24T00:00:01Z",
    ]));
    let local_id = local["result"]["project_id"].as_str().unwrap().to_string();

    // List shows both, including the remote-only published project.
    let listed = success_json(&run(&[
        "project-catalog",
        "list",
        "--projects-path",
        projects,
    ]));
    let projects_json = listed["result"]["projects"].as_array().unwrap();
    assert_eq!(projects_json.len(), 2);
    assert_redacted(&listed, &root);

    // Get returns the catalog record.
    let fetched = success_json(&run(&[
        "project-catalog",
        "get",
        "--projects-path",
        projects,
        "--project",
        &project_id,
    ]));
    assert_eq!(
        fetched["result"]["project"]["scope"]["repo_id"]
            .as_str()
            .unwrap(),
        "clifamily"
    );

    // Plant a pending nomination the way attach ingests one, then prove the
    // acceptance command is epoch-checked (plan §7.6): acceptance grants
    // host-wide selector authority, so a stale read must refuse rather than
    // decide against a snapshot the operator never saw. The store handle is
    // dropped before the CLI runs; it holds a shared lifetime lock.
    let planted_epoch = {
        let store = ProjectCatalogStore::open_existing(&projects_path).unwrap();
        let current = store.snapshot().unwrap().epoch();
        let target = ProjectId::parse(local_id.clone()).unwrap();
        let commit = store
            .transact(current, move |catalog, _attachments| {
                catalog
                    .projects
                    .get_mut(&target)
                    .expect("the legacy-local project is in the catalog")
                    .nominated_aliases
                    .insert("nominated-alias".to_string());
                Ok(())
            })
            .unwrap();
        commit.epoch
    };
    let stale_epoch = (planted_epoch - 1).to_string();
    let fresh_epoch = planted_epoch.to_string();
    let stale = run(&[
        "project-catalog",
        "alias",
        "accept",
        "--projects-path",
        projects,
        "--project",
        &local_id,
        "--alias",
        "nominated-alias",
        "--expected-epoch",
        &stale_epoch,
    ]);
    assert!(!stale.status.success());
    let stale: Value = serde_json::from_slice(&stale.stdout).unwrap();
    assert_eq!(stale["error"]["code"], "error.project_catalog_stale_epoch");
    let accepted = success_json(&run(&[
        "project-catalog",
        "alias",
        "accept",
        "--projects-path",
        projects,
        "--project",
        &local_id,
        "--alias",
        "nominated-alias",
        "--expected-epoch",
        &fresh_epoch,
    ]));
    assert_eq!(accepted["result"]["accepted"], Value::Bool(true));

    // Attested relpath move on the remote-only project.
    let migrated = success_json(&run(&[
        "project-catalog",
        "scope-migrate",
        "--projects-path",
        projects,
        "--operator-attested",
        "--project",
        &project_id,
        "--expected-old-repo",
        "clifamily",
        "--expected-old-relpath",
        ".",
        "--new-repo",
        "clifamily",
        "--new-relpath",
        "svc/api",
        "--kind",
        "relpath-move",
        "--acknowledge-unattached-scope-migration",
        "--reason",
        "relocating the remote-only service root",
        "--migrated-at",
        "2026-07-24T00:00:02Z",
        "--config",
        config,
    ]));
    assert!(
        migrated["result"]["scope_migration_id"]
            .as_str()
            .unwrap()
            .starts_with("sm_")
    );

    // An explicitly named configuration file that does not exist is a typed
    // refusal, not a silent fall back to the default roots: every evidence
    // class would otherwise be probed against the wrong state.
    let missing_config = root.join("missing-config.toml");
    let refused = run_with_isolated_index(
        &[
            "project-catalog",
            "retire",
            "--projects-path",
            projects,
            "--project",
            &project_id,
            "--execute",
            "--config",
            missing_config.to_str().unwrap(),
        ],
        &index_path,
    );
    assert!(!refused.status.success());
    let refused: Value = serde_json::from_slice(&refused.stdout).unwrap();
    assert_eq!(refused["error"]["code"], "error.project_catalog_cli_config");
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("missing-config.toml")
    );

    // Retire inventories, then removes when clean; the legacy-local
    // project survives untouched.
    let retired = success_json(&run_with_isolated_index(
        &[
            "project-catalog",
            "retire",
            "--projects-path",
            projects,
            "--project",
            &project_id,
            "--execute",
            "--config",
            config,
        ],
        &index_path,
    ));
    assert_eq!(retired["result"]["removed"], serde_json::Value::Bool(true));
    assert!(
        retired["result"]["unprobeable_reference_classes"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let listed = success_json(&run(&[
        "project-catalog",
        "list",
        "--projects-path",
        projects,
    ]));
    let remaining = listed["result"]["projects"].as_array().unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(
        remaining[0]["project_id"].as_str().unwrap(),
        local_id.as_str()
    );
}

#[test]
fn attested_scope_migration_records_both_bridge_generations() {
    let directory = tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (state, projects_path, config_path, _index_path) = isolated_state_root(&root);
    let projects = projects_path.to_str().unwrap();
    let project_id = add_published_project(projects, "bridgefamily", ".", "2026-07-24T00:00:00Z");

    // An active collected generation and an accepted publication pointer.
    // The offline channel must carry both onto the migration record exactly
    // as the attachment-proved channel does.
    let code_generation = "a".repeat(64);
    let publication_generation = "b".repeat(64);
    write(
        &state.join(format!("code-sources/activations/{project_id}.json")),
        format!(r#"{{"generation_id":"{code_generation}"}}"#).as_bytes(),
    );
    write(
        &state.join(format!("accepted-publications/pointers/{project_id}.json")),
        format!(r#"{{"accepted_generation":"{publication_generation}"}}"#).as_bytes(),
    );

    success_json(&run(&[
        "project-catalog",
        "scope-migrate",
        "--projects-path",
        projects,
        "--operator-attested",
        "--project",
        &project_id,
        "--expected-old-repo",
        "bridgefamily",
        "--expected-old-relpath",
        ".",
        "--new-repo",
        "bridgefamily",
        "--new-relpath",
        "svc/api",
        "--kind",
        "relpath-move",
        "--acknowledge-unattached-scope-migration",
        "--reason",
        "relocating a project that still holds bridge generations",
        "--migrated-at",
        "2026-07-24T00:00:01Z",
        "--config",
        config_path.to_str().unwrap(),
    ]));

    let store = ProjectCatalogStore::open_existing(&projects_path).unwrap();
    let snapshot = store.snapshot().unwrap();
    let record = snapshot
        .catalog()
        .scope_migrations
        .values()
        .find(|record| record.project_id.as_str() == project_id)
        .expect("the attested migration wrote its record");
    assert_eq!(
        record.code_bridge_generation.as_deref(),
        Some(code_generation.as_str())
    );
    assert_eq!(
        record.publication_bridge_generation.as_deref(),
        Some(publication_generation.as_str())
    );
}

#[test]
fn retire_refuses_on_a_producer_assignment() {
    let directory = tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (state, projects_path, config_path, index_path) = isolated_state_root(&root);
    let projects = projects_path.to_str().unwrap();
    let config = config_path.to_str().unwrap();
    let project_id = add_published_project(projects, "producerfamily", ".", "2026-07-24T00:00:00Z");

    // R3F1: producer assignments come from the config grants, not the
    // migration-era effective-source manifest. Write a producer grant
    // for the project's scope in the config file.
    let token_file = root.join("producer.token");
    write(&token_file, b"producer-secret-token");
    // Rewriting the config drops what `isolated_state_root` wrote, so restate
    // `vectors_dir`: without it the vector root resolves to the PLATFORM state
    // directory and this test would probe the host's real vector store.
    let vectors = state.join("vectors");
    write(
        &config_path,
        format!(
            "[paths]\nstate_dir = {state:?}\nvectors_dir = {vectors:?}\n\
             [[code_collection.producers]]\n\
             producer_id = \"producerfamily-producer\"\n\
             token_file = {token_file:?}\n\
             scopes = [{{ repo_id = \"producerfamily\", bbox_root_relpath = \".\" }}]\n"
        )
        .as_bytes(),
    );

    let reported = success_json(&run_with_isolated_index(
        &[
            "project-catalog",
            "retire",
            "--projects-path",
            projects,
            "--project",
            &project_id,
            "--config",
            config,
        ],
        &index_path,
    ));
    assert_eq!(reported["result"]["blocking"]["producer_assignments"], 1);
    assert!(
        reported["result"]["unprobeable_reference_classes"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let probed = reported["result"]["probed_reference_classes"]
        .as_array()
        .unwrap();
    for class in [
        "producer_assignments",
        "artifact_rows",
        "whiteboard_rows",
        "packet_rows",
        "slack_channel_bindings",
        "slack_proposal_links",
        "edge_sidecar_rows",
        "index_entity_refs",
        "vector_entity_refs",
    ] {
        assert!(
            probed.iter().any(|value| value.as_str() == Some(class)),
            "{class}"
        );
    }

    let refused = run_with_isolated_index(
        &[
            "project-catalog",
            "retire",
            "--projects-path",
            projects,
            "--project",
            &project_id,
            "--execute",
            "--config",
            config,
        ],
        &index_path,
    );
    assert!(!refused.status.success());
    let refused: Value = serde_json::from_slice(&refused.stdout).unwrap();
    assert_eq!(
        refused["error"]["code"],
        "error.project_catalog_admin_retire_blocked"
    );

    let bro_home = root.join("bro-home");
    let plan_hash =
        retirement_plan_hash(projects, &project_id, config, Some(&bro_home), &index_path);
    let journal_refused = run_with_isolated_index(
        &[
            "project-catalog",
            "retirement-journal",
            "--projects-path",
            projects,
            "--project",
            &project_id,
            "--execute",
            "--plan-hash",
            &plan_hash,
            "--config",
            config,
            "--bro-home",
            bro_home.to_str().unwrap(),
        ],
        &index_path,
    );
    assert!(!journal_refused.status.success());
    let journal_refused: Value = serde_json::from_slice(&journal_refused.stdout).unwrap();
    assert_eq!(
        journal_refused["error"]["code"],
        "error.project_catalog_retire_producer_grant"
    );
    assert!(!bro_home.join("retirement-journals").exists());

    let mut prepared = bbox_indexing::project_catalog_admin::ProjectRetirementJournal::new(
        ProjectId::parse(project_id.clone()).unwrap(),
        ProjectCatalogStore::open_existing(&projects_path)
            .unwrap()
            .snapshot()
            .unwrap()
            .epoch(),
        "2026-07-27T00:00:00Z",
    );
    prepared.evidence.catalog_scope = Some(PublishedScope::try_new("producerfamily", ".").unwrap());
    prepared.seal_retirement_evidence();
    bbox_indexing::project_catalog_admin::save_retirement_journal(&bro_home, &prepared).unwrap();

    let resume_plan_hash =
        retirement_plan_hash(projects, &project_id, config, Some(&bro_home), &index_path);
    let resume_refused = run_with_isolated_index(
        &[
            "project-catalog",
            "retirement-journal",
            "--projects-path",
            projects,
            "--project",
            &project_id,
            "--execute",
            "--plan-hash",
            &resume_plan_hash,
            "--config",
            config,
            "--bro-home",
            bro_home.to_str().unwrap(),
        ],
        &index_path,
    );
    assert!(!resume_refused.status.success());
    let resume_refused: Value = serde_json::from_slice(&resume_refused.stdout).unwrap();
    assert_eq!(
        resume_refused["error"]["code"],
        "error.project_catalog_retire_producer_grant"
    );
    assert_eq!(
        bbox_indexing::project_catalog_admin::load_retirement_journal(
            &bro_home,
            &ProjectId::parse(project_id).unwrap(),
        )
        .unwrap()
        .unwrap()
        .current_stage,
        bbox_indexing::project_catalog_admin::RetirementJournalStage::Prepared
    );
}

#[test]
fn retire_treats_historical_scope_generations_as_provenance_only() {
    let directory = tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (state, projects_path, config_path, index_path) = isolated_state_root(&root);
    let projects = projects_path.to_str().unwrap();
    let config = config_path.to_str().unwrap();
    let project_id = add_published_project(projects, "scopefamily", ".", "2026-07-24T00:00:00Z");

    success_json(&run(&[
        "project-catalog",
        "scope-migrate",
        "--projects-path",
        projects,
        "--operator-attested",
        "--project",
        &project_id,
        "--expected-old-repo",
        "scopefamily",
        "--expected-old-relpath",
        ".",
        "--new-repo",
        "scopefamily",
        "--new-relpath",
        "svc/api",
        "--kind",
        "relpath-move",
        "--acknowledge-unattached-scope-migration",
        "--reason",
        "relocating before the retained generation is discharged",
        "--migrated-at",
        "2026-07-24T00:00:01Z",
        "--config",
        config,
    ]));
    let _project_two = add_published_project(projects, "scopefamily", ".", "2026-07-24T00:00:02Z");

    // The retained generation sits under the old scope after another project
    // has claimed it. Historical migration endpoints are provenance only.
    let old_scope = PublishedScope::try_new("scopefamily", ".").unwrap();
    fs::create_dir_all(
        state
            .join("code-sources/scopes")
            .join(bbox_code_source::scope_hash(&old_scope))
            .join("generations")
            .join("d".repeat(64)),
    )
    .unwrap();

    let reported = success_json(&run_with_isolated_index(
        &[
            "project-catalog",
            "retire",
            "--projects-path",
            projects,
            "--project",
            &project_id,
            "--config",
            config,
        ],
        &index_path,
    ));
    assert!(
        reported["result"]["blocking"]["code_source_generations"].is_null()
            || reported["result"]["blocking"]["code_source_generations"] == 0
    );

    let retired = run_with_isolated_index(
        &[
            "project-catalog",
            "retire",
            "--projects-path",
            projects,
            "--project",
            &project_id,
            "--execute",
            "--config",
            config,
        ],
        &index_path,
    );
    assert!(retired.status.success());
    assert!(
        state
            .join("code-sources/scopes")
            .join(bbox_code_source::scope_hash(&old_scope))
            .join("generations")
            .join("d".repeat(64))
            .is_dir()
    );
}

/// R33F1 regression: retirement discharges the RESOLVED vector root.
///
/// The fixture separates the two roots that used to be assumed equal: the
/// configured state directory, which the inventory and the discharge derived
/// `state_dir/vectors` from, and the vector store the runtime actually opens.
/// The owner rows live only in the runtime store. Before the fix the
/// inventory captured the state-derived directory, observed no rows, counted
/// that as zero, discharged nothing, passed its final proof, and removed the
/// project with its rows still live.
#[test]
fn retire_discharges_the_resolved_vector_root_not_a_state_derived_one() {
    let directory = tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let state = root.join("state");
    fs::create_dir_all(&state).unwrap();
    let projects_path = state.join("projects.json");
    drop(ProjectCatalogStore::initialize_empty(&projects_path).unwrap());
    let index_path = root.join("index");

    // The runtime vector store, deliberately NOT below the state directory,
    // exactly as the platform default sits outside a configured state root.
    let runtime_vectors = root.join("runtime-vectors");
    let state_derived_vectors = state.join("vectors");
    let config_path = root.join("config.toml");
    write(
        &config_path,
        format!("[paths]\nstate_dir = {state:?}\nvectors_dir = {runtime_vectors:?}\n").as_bytes(),
    );

    let projects = projects_path.to_str().unwrap();
    let config = config_path.to_str().unwrap();
    let project_id = add_published_project(projects, "vectorfamily", ".", "2026-07-24T00:00:00Z");

    // One owner row in the runtime store, plus a decoy row for the same
    // project in the state-derived directory the old derivation would have
    // discharged instead.
    let owner_ref = format!("project_file:{project_id}:src:lib.rs:v1");
    let runtime = VectorStore::open(&runtime_vectors).unwrap();
    runtime
        .upsert("route-a", &owner_ref, &"c".repeat(64), vec![0.5, 0.5])
        .unwrap();
    runtime.flush_all().unwrap();
    drop(runtime);
    let decoy = VectorStore::open(&state_derived_vectors).unwrap();
    decoy
        .upsert("route-a", &owner_ref, &"d".repeat(64), vec![0.25, 0.75])
        .unwrap();
    decoy.flush_all().unwrap();
    drop(decoy);

    let captured = bbox_vectors::migration_inventory::capture_migration_snapshot_no_create(
        &runtime_vectors,
        Default::default(),
    );
    assert_eq!(
        captured.project_scoped_refs.len(),
        1,
        "the fixture's owner row is in the runtime store"
    );

    // The journal path is what discharges owner rows, so drive that.
    let plan_hash = retirement_plan_hash(projects, &project_id, config, None, &index_path);
    let retired = success_json(&run_with_isolated_index(
        &[
            "project-catalog",
            "retirement-journal",
            "--projects-path",
            projects,
            "--project",
            &project_id,
            "--execute",
            "--plan-hash",
            &plan_hash,
            "--config",
            config,
        ],
        &index_path,
    ));
    assert!(
        retired["result"]["unprobeable_reference_classes"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let listed = success_json(&run(&[
        "project-catalog",
        "list",
        "--projects-path",
        projects,
    ]));
    assert!(listed["result"]["projects"].as_array().unwrap().is_empty());

    // The runtime store is the one that was inventoried and discharged.
    let after = bbox_vectors::migration_inventory::capture_migration_snapshot_no_create(
        &runtime_vectors,
        Default::default(),
    );
    assert!(
        after
            .project_scoped_refs
            .iter()
            .all(|row| row.project_id != project_id),
        "retirement left owner rows in the runtime vector store: {:?}",
        after.project_scoped_refs
    );

    // And the state-derived directory was never the authority: its rows are
    // untouched, which is the discharge the pre-fix code performed instead.
    let decoy_after = bbox_vectors::migration_inventory::capture_migration_snapshot_no_create(
        &state_derived_vectors,
        Default::default(),
    );
    assert_eq!(
        decoy_after.project_scoped_refs.len(),
        1,
        "the state-derived directory is not a vector authority"
    );
}

/// R3F1 lifecycle test: a normally collected project with a retained
/// activation record (but no config-level producer assignment) must
/// retire end-to-end through the journal path. The activation record
/// is collected state that gets discharged by stage
/// CollectedGenerationsDischarged, NOT authority that blocks stage
/// SourceAuthorityQuiesced. The second run is idempotent.
#[test]
fn retire_lifecycle_with_collected_activation_converges_and_is_idempotent() {
    let directory = tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (state, projects_path, config_path, index_path) = isolated_state_root(&root);
    let projects = projects_path.to_str().unwrap();
    let config = config_path.to_str().unwrap();
    let project_id = add_published_project(projects, "lifecycle", ".", "2026-07-24T00:00:00Z");

    // Retain a collected activation record (normal collected state).
    let generation = "e".repeat(64);
    write_collected_activation(&state, &project_id, &generation);

    // No producer assignment in config (config has no code_collection
    // section).

    // Execute retire end-to-end through the journal path.
    let plan_hash = retirement_plan_hash(projects, &project_id, config, None, &index_path);
    let retired = success_json(&run_with_isolated_index(
        &[
            "project-catalog",
            "retirement-journal",
            "--projects-path",
            projects,
            "--project",
            &project_id,
            "--execute",
            "--plan-hash",
            &plan_hash,
            "--config",
            config,
        ],
        &index_path,
    ));
    assert!(
        retired["result"]["unprobeable_reference_classes"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    // Verify the project is gone from the catalog.
    let listed = success_json(&run(&[
        "project-catalog",
        "list",
        "--projects-path",
        projects,
    ]));
    let remaining = listed["result"]["projects"].as_array().unwrap();
    assert!(
        remaining.is_empty(),
        "project should be retired from catalog"
    );

    // Verify the activation record was discharged.
    assert!(
        !state
            .join(format!("code-sources/activations/{project_id}.json"))
            .exists(),
        "activation record should be cleared after discharge"
    );

    // Second run is idempotent: retiring an already-retired project
    // should succeed without error (no-op).
    let second_plan_hash = retirement_plan_hash(projects, &project_id, config, None, &index_path);
    let second = success_json(&run_with_isolated_index(
        &[
            "project-catalog",
            "retirement-journal",
            "--projects-path",
            projects,
            "--project",
            &project_id,
            "--execute",
            "--plan-hash",
            &second_plan_hash,
            "--config",
            config,
        ],
        &index_path,
    ));
    // Journal should already be at Complete.
    if let Some(journal) = second["result"]["journal"].as_object() {
        if let Some(stage) = journal["current_stage"].as_str() {
            assert!(
                stage.contains("Complete"),
                "second run journal should be Complete, got: {stage}"
            );
        }
    }

    write_collected_activation(&state, &project_id, &"f".repeat(64));
    let recovery_plan_hash = retirement_plan_hash(projects, &project_id, config, None, &index_path);
    let recovery_refused = run_with_isolated_index(
        &[
            "project-catalog",
            "retirement-journal",
            "--projects-path",
            projects,
            "--project",
            &project_id,
            "--execute",
            "--plan-hash",
            &recovery_plan_hash,
            "--config",
            config,
        ],
        &index_path,
    );
    assert!(!recovery_refused.status.success());
    let recovery_refused: Value = serde_json::from_slice(&recovery_refused.stdout).unwrap();
    assert_eq!(
        recovery_refused["error"]["code"],
        "error.project_catalog_retire_recovery_not_quiescent"
    );

    fs::remove_file(state.join(format!("code-sources/activations/{project_id}.json"))).unwrap();
    let scope = PublishedScope::try_new("lifecycle", ".").unwrap();
    fs::create_dir_all(
        state
            .join("code-sources/scopes")
            .join(bbox_code_source::scope_hash(&scope))
            .join("generations")
            .join("9".repeat(64)),
    )
    .unwrap();
    let generation_plan_hash =
        retirement_plan_hash(projects, &project_id, config, None, &index_path);
    let generation_refused = run_with_isolated_index(
        &[
            "project-catalog",
            "retirement-journal",
            "--projects-path",
            projects,
            "--project",
            &project_id,
            "--execute",
            "--plan-hash",
            &generation_plan_hash,
            "--config",
            config,
        ],
        &index_path,
    );
    assert!(!generation_refused.status.success());
    let generation_refused: Value = serde_json::from_slice(&generation_refused.stdout).unwrap();
    assert_eq!(
        generation_refused["error"]["code"],
        "error.project_catalog_retire_evidence_generations"
    );
}

#[test]
fn retirement_execute_refuses_when_prepared_plan_hash_drifts() {
    let directory = tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (state, projects_path, config_path, index_path) = isolated_state_root(&root);
    let projects = projects_path.to_str().unwrap();
    let config = config_path.to_str().unwrap();
    let project_id = add_published_project(projects, "planfamily", ".", "2026-07-27T00:00:00Z");
    write(
        &state.join("bro/slack-channel-bindings.json"),
        serde_json::to_vec(&serde_json::json!({
            "bindings": {
              "T1:C1": {
                "team_id": "T1",
                "channel_id": "C1",
                "project_id": project_id,
                "project_dir": "/late/checkout",
                "registered_at": "2026-07-27T00:00:00Z"
              }
            }
        }))
        .unwrap()
        .as_slice(),
    );
    let plan_hash = retirement_plan_hash(projects, &project_id, config, None, &index_path);
    write(
        &state.join("bro/slack-channel-bindings.json"),
        serde_json::to_vec(&serde_json::json!({
            "bindings": {
              "T1:C2": {
                "team_id": "T1",
                "channel_id": "C2",
                "project_id": project_id,
                "project_dir": "/replacement/checkout",
                "registered_at": "2026-07-27T00:00:01Z"
              }
            }
        }))
        .unwrap()
        .as_slice(),
    );
    let refused = run_with_isolated_index(
        &[
            "project-catalog",
            "retirement-journal",
            "--projects-path",
            projects,
            "--project",
            &project_id,
            "--execute",
            "--plan-hash",
            &plan_hash,
            "--config",
            config,
        ],
        &index_path,
    );
    assert!(!refused.status.success());
    let refused: Value = serde_json::from_slice(&refused.stdout).unwrap();
    assert_eq!(
        refused["error"]["code"],
        "error.project_catalog_retire_plan_drift"
    );
    assert!(
        ProjectCatalogStore::open_existing(&projects_path)
            .unwrap()
            .snapshot()
            .unwrap()
            .catalog()
            .projects
            .contains_key(&ProjectId::parse(project_id).unwrap())
    );
}

#[test]
fn retirement_execute_refuses_activation_content_mutation() {
    let directory = tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (state, projects_path, config_path, index_path) = isolated_state_root(&root);
    let projects = projects_path.to_str().unwrap();
    let config = config_path.to_str().unwrap();
    let project_id = add_published_project(projects, "activationplan", ".", "2026-07-27T00:00:00Z");
    write_collected_activation(&state, &project_id, &"a".repeat(64));
    let plan_hash = retirement_plan_hash(projects, &project_id, config, None, &index_path);
    write_collected_activation(&state, &project_id, &"b".repeat(64));
    let refused = run_with_isolated_index(
        &[
            "project-catalog",
            "retirement-journal",
            "--projects-path",
            projects,
            "--project",
            &project_id,
            "--execute",
            "--plan-hash",
            &plan_hash,
            "--config",
            config,
        ],
        &index_path,
    );
    assert!(!refused.status.success());
    let refused: Value = serde_json::from_slice(&refused.stdout).unwrap();
    assert_eq!(
        refused["error"]["code"],
        "error.project_catalog_retire_plan_drift"
    );
}

#[test]
fn retirement_journal_resumes_each_artifact_tombstone_boundary() {
    for boundary in [
        "before_payload_hide",
        "payload_hidden",
        "metadata_hidden",
        "before_tombstone_validation",
        "after_tombstone_validation",
        "before_committed_tree_delete",
        "committed_file_unlinked",
        "committed_directory_removed",
        "metadata_tombstone_removed",
        "payload_tombstone_removed",
    ] {
        let directory = tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let (state, projects_path, config_path, index_path) = isolated_state_root(&root);
        let projects = projects_path.to_str().unwrap();
        let config = config_path.to_str().unwrap();
        let project_id =
            add_published_project(projects, "artifactfault", ".", "2026-07-27T00:00:00Z");
        let artifact_dir = state
            .join("artifacts/projects")
            .join(&project_id)
            .join("local/agent/retire-me");
        write(
            &artifact_dir.join("metadata.json"),
            &serde_json::to_vec(&serde_json::json!({
                "kind": "agent",
                "name": "retire-me",
                "version": "1",
                "source": "fixture",
                "installed_at": "2026-07-27T00:00:00Z",
                "content_sha256": "a".repeat(64),
                "project_id": project_id,
                "project_path": null,
                "local": true,
                "supersedes": null,
                "supersedes_chain": [],
                "superseded_by": null,
                "active": true,
                "install_warnings": []
            }))
            .unwrap(),
        );
        write(
            &artifact_dir.join(".versions/1.metadata.json"),
            &serde_json::to_vec(&serde_json::json!({
                "kind": "agent",
                "name": "retire-me",
                "version": "1",
                "source": "fixture",
                "installed_at": "2026-07-27T00:00:00Z",
                "content_sha256": "a".repeat(64),
                "project_id": project_id,
                "project_path": null,
                "local": true,
                "supersedes": null,
                "supersedes_chain": [],
                "superseded_by": null,
                "active": true,
                "install_warnings": []
            }))
            .unwrap(),
        );
        write(
            &artifact_dir.with_extension("json"),
            br#"{"name":"retire-me"}"#,
        );
        let plan_hash = retirement_plan_hash(projects, &project_id, config, None, &index_path);
        let args = [
            "project-catalog",
            "retirement-journal",
            "--projects-path",
            projects,
            "--project",
            &project_id,
            "--execute",
            "--plan-hash",
            &plan_hash,
            "--config",
            config,
        ];
        let interrupted = run_with_isolated_index_and_env(
            &args,
            &index_path,
            "BLACKBOX_TEST_ARTIFACT_RETIRE_FAULT",
            boundary,
        );
        assert!(
            !interrupted.status.success(),
            "{boundary} did not interrupt"
        );
        assert!(
            state
                .join("bro/retirement-journals")
                .join(format!("{project_id}.json"))
                .exists()
        );

        let resumed = run_with_isolated_index(&args, &index_path);
        assert!(
            resumed.status.success(),
            "{boundary} resume failed: {}",
            String::from_utf8_lossy(&resumed.stderr)
        );
        assert!(!artifact_dir.exists());
        assert!(!artifact_dir.with_extension("json").exists());
        assert!(
            !ProjectCatalogStore::open_existing(&projects_path)
                .unwrap()
                .snapshot()
                .unwrap()
                .catalog()
                .projects
                .contains_key(&ProjectId::parse(project_id).unwrap())
        );
    }
}

#[test]
fn retire_refuses_on_a_slack_channel_binding() {
    let directory = tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (state, projects_path, config_path, index_path) = isolated_state_root(&root);
    let projects = projects_path.to_str().unwrap();
    let config = config_path.to_str().unwrap();
    let project_id = add_published_project(projects, "slackfamily", ".", "2026-07-24T00:00:00Z");

    // A channel binding keys its row by project id as well as by the legacy
    // project directory. The owner capture surface exposes only the directory
    // selector, so the id-keyed row is exactly what a narrower probe misses.
    write(
        &state.join("bro/slack-channel-bindings.json"),
        format!(
            r#"{{"bindings":{{"T1:C1":{{"team_id":"T1","channel_id":"C1",
             "project_dir":"/nowhere/checkout","project_id":"{project_id}",
             "registered_at":"2026-07-24T00:00:00Z"}}}}}}"#
        )
        .as_bytes(),
    );

    let reported = success_json(&run_with_isolated_index(
        &[
            "project-catalog",
            "retire",
            "--projects-path",
            projects,
            "--project",
            &project_id,
            "--config",
            config,
        ],
        &index_path,
    ));
    assert_eq!(reported["result"]["blocking"]["slack_channel_bindings"], 1);
    assert!(
        reported["result"]["unprobeable_reference_classes"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let refused = run_with_isolated_index(
        &[
            "project-catalog",
            "retire",
            "--projects-path",
            projects,
            "--project",
            &project_id,
            "--execute",
            "--config",
            config,
        ],
        &index_path,
    );
    assert!(!refused.status.success());
    let refused: Value = serde_json::from_slice(&refused.stdout).unwrap();
    assert_eq!(
        refused["error"]["code"],
        "error.project_catalog_admin_retire_blocked"
    );

    let plan_hash = retirement_plan_hash(projects, &project_id, config, None, &index_path);
    let journaled = run_with_isolated_index(
        &[
            "project-catalog",
            "retirement-journal",
            "--projects-path",
            projects,
            "--project",
            &project_id,
            "--execute",
            "--plan-hash",
            &plan_hash,
            "--config",
            config,
        ],
        &index_path,
    );
    assert!(
        journaled.status.success(),
        "{}",
        String::from_utf8_lossy(&journaled.stderr)
    );
    let bindings: Value =
        serde_json::from_slice(&fs::read(state.join("bro/slack-channel-bindings.json")).unwrap())
            .unwrap();
    assert!(bindings["bindings"].as_object().unwrap().is_empty());
}

/// Plan sections 3.2 and 4.2, as amended during the operational-cut repair
/// arc: configured apply takes the lifetime claim as a PROBE before any
/// target read or mutation, then RELEASES it so the migration transaction
/// can perform its own exclusive acquisition (the flock self-conflict
/// class: a second same-process descriptor can never take the lock
/// exclusively while any claim is held). It cannot use `open_admin_store`,
/// whose strict open would refuse the still-version-1 configured store that
/// exists at exactly this moment.
///
/// This test pins the probe's ORDERING and its refusal against an external
/// shared holder (exactly what a live daemon looks like). The success half
/// of the contract, that the SAME artifacts reach `Applied` once no claim
/// is held anywhere, is pinned at the facade layer in
/// `configured_apply_installs_the_reviewed_post_image_on_the_configured_layout`
/// (bbox-indexing), which also pins the self-conflict refusal a HELD claim
/// produces from the transaction itself.
#[test]
fn migrate_apply_configured_takes_the_lifetime_claim_before_touching_the_target() {
    let directory = tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (_state, projects_path, config_path, _index) = isolated_state_root(&root);

    // Artifacts that do not exist: if the claim were taken AFTER the target
    // read, the refusal would name the missing artifacts instead of the lock.
    let invocation = || {
        run(&[
            "project-catalog",
            "migrate",
            "--apply",
            "--configured",
            "--config",
            config_path.to_str().unwrap(),
            "--report",
            root.join("review/report.json").to_str().unwrap(),
            "--resolution",
            root.join("review/resolution.json").to_str().unwrap(),
        ])
    };

    // Premise: with the lock FREE the claim succeeds, so whatever this
    // invocation goes on to report is not the lock refusal.
    let available: Value = serde_json::from_slice(&invocation().stdout).unwrap();
    assert_ne!(
        available["error"]["code"], "error.project_catalog_cli_lock",
        "an unheld lifetime lock must not produce the claim refusal: {available}"
    );

    // A shared holder is exactly what a live daemon looks like.
    let held = ProjectCatalogMigrationLock::acquire_shared(&projects_path).unwrap();
    let refused = invocation();
    assert!(!refused.status.success());
    let refused: Value = serde_json::from_slice(&refused.stdout).unwrap();
    assert_eq!(refused["error"]["code"], "error.project_catalog_cli_lock");
    drop(held);
}

/// A missing or mode-incompatible `migrate` target is a TYPED handler
/// refusal, produced before configuration loading (plan section 3.1, Q-A).
/// The named config path does not exist, so a refusal that reached the
/// config loader would carry `error.project_catalog_cli_config` instead.
#[test]
fn migrate_target_rules_refuse_before_configuration_is_loaded() {
    let directory = tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let absent_config = root.join("no-such-config.toml");

    let missing_target = run(&[
        "project-catalog",
        "migrate",
        "--apply",
        "--config",
        absent_config.to_str().unwrap(),
        "--report",
        root.join("review/report.json").to_str().unwrap(),
        "--resolution",
        root.join("review/resolution.json").to_str().unwrap(),
    ]);
    assert!(!missing_target.status.success());
    let missing_target: Value = serde_json::from_slice(&missing_target.stdout).unwrap();
    assert_eq!(
        missing_target["error"]["code"],
        "error.project_catalog_cli_arguments"
    );

    let incompatible_target = run(&[
        "project-catalog",
        "migrate",
        "--preflight",
        "--configured",
        "--config",
        absent_config.to_str().unwrap(),
        "--report",
        root.join("review/report.json").to_str().unwrap(),
        "--resolution",
        root.join("review/resolution.json").to_str().unwrap(),
    ]);
    assert!(!incompatible_target.status.success());
    let incompatible_target: Value = serde_json::from_slice(&incompatible_target.stdout).unwrap();
    assert_eq!(
        incompatible_target["error"]["code"],
        "error.project_catalog_cli_arguments"
    );
}

/// Plan section 3.2: `verify --require-exclusive-availability` is the
/// bridge-down proof, and it selects the CONFIGURED target. A live daemon
/// holds the configured lifetime lock SHARED, so the exclusive probe finds
/// no guard and the command refuses.
///
/// Both halves are asserted: the first pins that the refusal is caused by
/// the held lock rather than by the rest of the invocation, and the second
/// pins the refusal itself.
#[test]
fn verify_require_exclusive_availability_refuses_while_the_bridge_holds_the_lock() {
    let directory = tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (_state, projects_path, config_path, _index) = isolated_state_root(&root);

    // `--require-exclusive-availability` SELECTS the configured target (plan
    // section 3.2), so it carries no `--root`: the layout it probes and then
    // verifies is the one this `--config` resolves.
    let invocation = |config: &str| {
        run(&[
            "project-catalog",
            "verify",
            "--config",
            config,
            "--require-exclusive-availability",
        ])
    };

    // Premise: with the lock FREE the availability probe passes, so whatever
    // this invocation goes on to report is not the lock refusal.
    let available = invocation(config_path.to_str().unwrap());
    let available: Value = serde_json::from_slice(&available.stdout).unwrap();
    assert_ne!(
        available["error"]["code"], "error.project_catalog_cli_lock",
        "an unheld lifetime lock must not produce the bridge-live refusal: {available}"
    );

    // The proof: a shared holder is exactly what a live bridge looks like.
    let held = ProjectCatalogMigrationLock::acquire_shared(&projects_path).unwrap();
    let refused = invocation(config_path.to_str().unwrap());
    assert!(!refused.status.success());
    let refused: Value = serde_json::from_slice(&refused.stdout).unwrap();
    assert_eq!(refused["error"]["code"], "error.project_catalog_cli_lock");
    drop(held);
}
