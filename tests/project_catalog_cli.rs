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
    let index = TranscriptIndex::open_or_create(
        &index_path,
        Vec::new(),
        None,
        state.join("projects.json"),
        state.join("blackbox-knowledge.json"),
        state.join("blackbox-threads.json"),
        state.join("blackbox-roadmap.json"),
    )
    .unwrap();
    drop(index);
    write(&index_path.join("_meta.json"), b"{}");

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
        .env("TRANSCRIPT_SEARCH_INDEX_PATH", index_path);
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
    write(
        &config_path,
        format!("[paths]\nstate_dir = {state:?}\n").as_bytes(),
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
        format!("[paths]\nstate_dir = {:?}\n", root.join("protected")).as_bytes(),
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
        format!("[paths]\nstate_dir = {:?}\n", protected_root).as_bytes(),
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
    write(
        &config_path,
        format!(
            "[paths]\nstate_dir = {state:?}\n\
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
    let journal_refused = run_with_isolated_index(
        &[
            "project-catalog",
            "retirement-journal",
            "--projects-path",
            projects,
            "--project",
            &project_id,
            "--execute",
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

    let resume_refused = run_with_isolated_index(
        &[
            "project-catalog",
            "retirement-journal",
            "--projects-path",
            projects,
            "--project",
            &project_id,
            "--execute",
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
    let retired = success_json(&run_with_isolated_index(
        &[
            "project-catalog",
            "retirement-journal",
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
    let second = success_json(&run_with_isolated_index(
        &[
            "project-catalog",
            "retirement-journal",
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
    let recovery_refused = run_with_isolated_index(
        &[
            "project-catalog",
            "retirement-journal",
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
    assert!(!recovery_refused.status.success());
    let recovery_refused: Value = serde_json::from_slice(&recovery_refused.stdout).unwrap();
    assert_eq!(
        recovery_refused["error"]["code"],
        "error.project_catalog_retire_recovery_activation"
    );
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

    let journaled = run_with_isolated_index(
        &[
            "project-catalog",
            "retirement-journal",
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
    assert!(journaled.status.success());
    let bindings: Value =
        serde_json::from_slice(&fs::read(state.join("bro/slack-channel-bindings.json")).unwrap())
            .unwrap();
    assert!(bindings["bindings"].as_object().unwrap().is_empty());
}
