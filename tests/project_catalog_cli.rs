use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use bbox_code_source_store::CodeSourceStore;
use bbox_config::config::{self, LoadOptions};
use bbox_corpus_index::index::TranscriptIndex;
use bbox_edge_sidecar::manifest::ManifestIndex;
use bbox_indexing::project_catalog_migration::project_catalog_migration_store_limits;
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
    let preflight = success_json(&run(&[
        "project-catalog",
        "migrate",
        "--preflight",
        "--report",
        report.to_str().unwrap(),
        "--resolution",
        resolution.to_str().unwrap(),
        "--state-dir",
        rehearsal_root.join("state").to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
    ]));
    assert_eq!(preflight["command"], "project_catalog_migrate_preflight");
    assert_eq!(preflight["result"]["status"], "clean");
    assert!(report.is_file());
    assert!(resolution.is_file());
    assert_redacted(&preflight, &root);

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
