//! Exercise the shared production constructor with isolated durable resources.
//! No provider authentication, process environment mutation, host instructions,
//! external MCP connections, or locality synchronization are needed.

use super::*;
use async_trait::async_trait;
use clap::Parser;
use std::path::Path;

struct StartupTransport {
    snapshot: Value,
}

#[async_trait]
impl Transport for StartupTransport {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    fn push_user_text(&mut self, text: &str) {
        self.snapshot.as_array_mut().unwrap().push(json!({
            "role": "user", "content": [{"type": "text", "text": text}],
        }));
    }

    fn push_tool_results(&mut self, _results: Vec<transport::ToolResult>) {
        panic!("startup tests do not execute tools");
    }

    async fn run_turn(
        &mut self,
        _tools: &[transport::ToolSpec],
        _opts: &TurnOpts,
        _sink: &dyn transport::TurnSink,
    ) -> Result<transport::TurnOutput> {
        anyhow::bail!("startup tests must not request model inference")
    }

    fn snapshot(&self) -> Value {
        self.snapshot.clone()
    }

    fn restore(&mut self, snapshot: Value) {
        self.snapshot = snapshot;
    }
}

fn cli(root: &Path, effort: Option<&str>, resume: bool) -> Cli {
    let mut args = vec![
        "bro-harness",
        "--cwd",
        root.to_str().unwrap(),
        "--system-prompt",
        "",
        "--code-mode",
        "off",
        "--output-schema",
        "{}",
        "--service-tier",
        "default",
    ];
    if resume {
        args.extend(["--resume", "startup"]);
    } else {
        args.extend(["--session-id", "startup", "--model", "gpt-5.5"]);
    }
    if let Some(effort) = effort {
        args.extend(["--effort", effort]);
    }
    Cli::try_parse_from(args).unwrap()
}

async fn build(cli: &Cli, root: &Path) -> Session {
    let store = SessionStore::open_in(root, None, cli.session_id.as_deref(), cli.resume.as_deref())
        .unwrap();
    let event_log = Arc::new(EventLog::at_path(
        store.store_path().with_extension("events.jsonl"),
    ));
    Session::build_with_runtime(
        cli,
        Some(Arc::new(|_| {})),
        Some(mcp::McpConfig {
            servers: Vec::new(),
            tool_placement: Default::default(),
            server_policies: BTreeMap::new(),
        }),
        Some(BTreeMap::new()),
        Some(BTreeMap::new()),
        Some(SessionBuildRuntime {
            kind: TransportKind::Anthropic,
            tx: Box::new(StartupTransport {
                snapshot: json!([]),
            }),
            store,
            event_log,
            project_mutation_routes: Some(Vec::new()),
        }),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn production_startup_restores_saved_effort_and_cli_override_survives_another_resume() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut initial = build(&cli(&root, Some("high"), false), &root).await;
    assert_eq!(initial.base_opts.effort.as_deref(), Some("high"));
    initial.tx.push_user_text("retained task");
    let expected_history = initial.tx.snapshot();
    initial.persist().await.unwrap();
    drop(initial);

    let mut resumed = build(&cli(&root, None, true), &root).await;
    assert_eq!(resumed.base_opts.effort.as_deref(), Some("high"));
    assert_eq!(resumed.base_opts.model, "gpt-5.5");
    assert_eq!(resumed.tx.snapshot(), expected_history);
    resumed.persist().await.unwrap();
    drop(resumed);

    let mut overridden = build(&cli(&root, Some("low"), true), &root).await;
    assert_eq!(overridden.base_opts.effort.as_deref(), Some("low"));
    assert_eq!(overridden.tx.snapshot(), expected_history);
    overridden.persist().await.unwrap();
    drop(overridden);

    let restored_override = build(&cli(&root, None, true), &root).await;
    assert_eq!(restored_override.base_opts.effort.as_deref(), Some("low"));
    assert_eq!(restored_override.tx.snapshot(), expected_history);
}

#[tokio::test]
async fn production_startup_keeps_legacy_effort_unspecified() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let path = root.join("startup.json");
    let expected_history = json!([
        {"role":"user","content":[{"type":"text","text":"legacy task"}]},
    ]);
    crate::session::write_atomic(
        &path,
        &json!({
            "transport":"anthropic", "model":"gpt-5.5", "snapshot":expected_history,
        })
        .to_string(),
    )
    .unwrap();

    let mut resumed = build(&cli(&root, None, true), &root).await;
    assert_eq!(resumed.base_opts.effort, None);
    assert_eq!(resumed.tx.snapshot(), expected_history);
    resumed.persist().await.unwrap();
    drop(resumed);

    // Saving upgrades the header without inventing an effort selection.
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(saved["version"], crate::session::SNAPSHOT_VERSION);
    assert!(saved["effort"].is_null());
    let reopened = build(&cli(&root, None, true), &root).await;
    assert_eq!(reopened.base_opts.effort, None);
    assert_eq!(reopened.tx.snapshot(), expected_history);
}
