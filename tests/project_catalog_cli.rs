use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use bbox_code_source_store::{
    CodeSourceStore, MigrationEffectiveSourceManifestV1, MigrationEffectiveSourceSelectionV1,
    encode_migration_effective_source_manifest_v1,
};
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

    // A producer assignment lives in the effective source manifest, which no
    // path-shaped probe reaches: it must be decoded to be counted.
    let generation = "c".repeat(64);
    let manifest = MigrationEffectiveSourceManifestV1 {
        version: 1,
        selections: vec![MigrationEffectiveSourceSelectionV1 {
            project_id: ProjectId::parse(project_id.clone()).unwrap(),
            published_scope: PublishedScope::try_new("producerfamily", ".").unwrap(),
            generation_id: generation.clone(),
            selector: format!("collected:{project_id}:{generation}:m{}", "0".repeat(16)),
        }],
    };
    write(
        &state.join("code-sources/effective-source-manifest.json"),
        &encode_migration_effective_source_manifest_v1(&manifest).unwrap(),
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
}

#[test]
fn retire_probes_generations_under_a_previously_owned_scope() {
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

    // The retained generation sits under the scope the project owned BEFORE
    // the migration. A probe that only reads the current scope hash reports
    // zero and lets --execute destroy a still-referenced project.
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
    assert_eq!(reported["result"]["blocking"]["code_source_generations"], 1);

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
}
