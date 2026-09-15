//! Exercise the shared production constructor with isolated durable resources.
//! No provider authentication, process environment mutation, host instructions,
//! external MCP connections, or locality synchronization are needed.

use super::*;
use async_trait::async_trait;
use clap::Parser;
use std::path::Path;

struct StartupTransport {
    snapshot: Value,
    compaction_models: Arc<StdMutex<Vec<String>>>,
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

    async fn compact(
        &mut self,
        _params: transport::CompactionParams,
        _instruction: &str,
        _tools: &[transport::ToolSpec],
        opts: &TurnOpts,
    ) -> Result<Option<String>> {
        self.compaction_models
            .lock()
            .unwrap()
            .push(opts.model.clone());
        self.snapshot = json!([
            {"role":"user","content":[{"type":"text","text":"retained compacted task"}]},
        ]);
        Ok(Some("retained compacted task".into()))
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
    build_with_compaction_log(cli, root, Arc::default()).await
}

async fn build_with_compaction_log(
    cli: &Cli,
    root: &Path,
    compaction_models: Arc<StdMutex<Vec<String>>>,
) -> Session {
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
                compaction_models,
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
        {"role":"user","content":[{"type":"text","text":"legacy task ".repeat(10_000)}]},
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
    assert_eq!(resumed.last_prompt_tokens, 0);
    assert_eq!(resumed.pending_input_estimate, 0);
    assert_eq!(resumed.last_request_overhead_tokens, 0);
    let mut opts = resumed.base_opts.clone();
    opts.base_instructions = None;
    opts.system = SystemPrompt::default();
    let projected = resumed.projected_request_tokens(&[], &opts);
    assert!(
        projected > 30_000,
        "legacy history must contribute to occupancy"
    );
    resumed.persist().await.unwrap();
    drop(resumed);

    // Saving upgrades the header without inventing an effort selection.
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(saved["version"], crate::session::SNAPSHOT_VERSION);
    assert!(saved["effort"].is_null());
    let reopened = build(&cli(&root, None, true), &root).await;
    assert_eq!(reopened.base_opts.effort, None);
    assert_eq!(reopened.tx.snapshot(), expected_history);
    assert_eq!(reopened.projected_request_tokens(&[], &opts), projected);
}

#[tokio::test]
async fn production_startup_restores_atomic_context_budget_with_native_history() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let path = root.join("startup.json");
    let mut initial = build(&cli(&root, None, false), &root).await;
    initial
        .tx
        .push_user_text("measured history and appended output");
    let expected_history = initial.tx.snapshot();
    initial.last_prompt_tokens = 120_000;
    initial.pending_input_estimate = 9_000;
    initial.last_request_overhead_tokens = 700;
    let expected_checkpoint = serde_json::to_value(initial.budget_checkpoint()).unwrap();
    let mut opts = initial.base_opts.clone();
    opts.base_instructions = None;
    opts.system = SystemPrompt::default();
    assert_eq!(initial.projected_request_tokens(&[], &opts), 129_000);
    initial.persist().await.unwrap();
    drop(initial);

    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(saved["side"]["context_budget"], expected_checkpoint);
    assert_eq!(saved["snapshot"], expected_history);

    let mut resumed = build(&cli(&root, None, true), &root).await;
    assert_eq!(resumed.tx.snapshot(), expected_history);
    assert_eq!(resumed.last_prompt_tokens, 120_000);
    assert_eq!(resumed.pending_input_estimate, 9_000);
    assert_eq!(resumed.last_request_overhead_tokens, 700);
    assert_eq!(
        serde_json::to_value(resumed.budget_checkpoint()).unwrap(),
        expected_checkpoint
    );
    assert_eq!(resumed.projected_request_tokens(&[], &opts), 129_000);
    resumed.persist().await.unwrap();
    drop(resumed);

    let reopened = build(&cli(&root, None, true), &root).await;
    assert_eq!(
        serde_json::to_value(reopened.budget_checkpoint()).unwrap(),
        expected_checkpoint
    );
    assert_eq!(reopened.projected_request_tokens(&[], &opts), 129_000);
}

#[tokio::test]
async fn production_startup_compacts_with_previous_model_before_persisting_downshift() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let path = root.join("startup.json");
    let mut initial_cli = cli(&root, Some("high"), false);
    initial_cli.model = Some("MiniMax-M3".into());
    let mut initial = build(&initial_cli, &root).await;
    assert_eq!(initial.context_window, Some(1_000_000));
    initial
        .tx
        .push_user_text("task requiring a smaller replacement history");
    initial.last_prompt_tokens = 400_000;
    initial.pending_input_estimate = 9_000;
    initial.persist().await.unwrap();
    drop(initial);

    let compaction_models = Arc::new(StdMutex::new(Vec::new()));
    let mut resumed_cli = cli(&root, None, true);
    resumed_cli.model = Some("gpt-5.5".into());
    let resumed = build_with_compaction_log(&resumed_cli, &root, compaction_models.clone()).await;
    assert_eq!(*compaction_models.lock().unwrap(), vec!["MiniMax-M3"]);
    assert_eq!(resumed.base_opts.model, "gpt-5.5");
    assert_eq!(resumed.base_opts.effort.as_deref(), Some("high"));
    assert_eq!(resumed.context_window, Some(272_000));
    assert_eq!(resumed.last_prompt_tokens, 0);
    assert_eq!(resumed.pending_input_estimate, 0);
    assert_eq!(resumed.last_request_overhead_tokens, 0);
    let compacted_history = resumed.tx.snapshot();
    assert!(
        compacted_history
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["content"][0]["text"] == "retained compacted task")
    );
    let budget = serde_json::to_value(resumed.budget_checkpoint()).unwrap();
    // The constructor itself checkpoints the successful transition before any
    // user turn or inference. Reopening must therefore see the replacement.
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(saved["model"], "gpt-5.5");
    assert_eq!(saved["snapshot"], compacted_history);
    assert_eq!(saved["side"]["context_budget"], budget);
    drop(resumed);

    let reopened =
        build_with_compaction_log(&cli(&root, None, true), &root, compaction_models.clone()).await;
    assert_eq!(reopened.base_opts.model, "gpt-5.5");
    assert_eq!(reopened.tx.snapshot(), compacted_history);
    assert_eq!(*compaction_models.lock().unwrap(), vec!["MiniMax-M3"]);
}

#[tokio::test]
async fn production_resume_recovers_an_uncheckpointed_tail_and_briefs_the_model() {
    use std::io::Write as _;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut initial = build(&cli(&root, None, false), &root).await;
    initial.tx.push_user_text("checkpointed task");
    let checkpointed = initial.tx.snapshot();
    initial.persist().await.unwrap();
    drop(initial);

    // The previous process kept working after its checkpoint and was killed
    // mid-write: a tool_search activation of a tool this catalog does not
    // carry, a file write, and a torn final record. The activation receipt
    // lies beyond the checkpoint, so it must not become a resume requirement.
    let log = root.join("startup.events.jsonl");
    let mut file = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
    let receipt = json!({"loaded":["mcp__fixture__gone"]}).to_string();
    for event in [
        json!({"type":"assistant","seq":48,"message":{"content":[
            {"type":"tool_use","id":"s1","name":"tool_search","input":{}}
        ]}}),
        json!({"type":"user","seq":49,"message":{"content":[
            {"type":"tool_result","tool_use_id":"s1","content":receipt}
        ]}}),
        json!({"type":"assistant","seq":50,"message":{"content":[
            {"type":"tool_use","id":"c1","name":"file_write","input":{"file_path":"notes.md"}}
        ]}}),
        json!({"type":"user","seq":51,"message":{"content":[
            {"type":"tool_result","tool_use_id":"c1","content":"ok"}
        ]}}),
    ] {
        writeln!(
            file,
            "{}",
            json!({"ts":"2026-01-01T00:00:00Z","event":event})
        )
        .unwrap();
    }
    write!(
        file,
        "{{\"ts\":\"2026-01-01T00:00:01Z\",\"event\":{{\"type\":\"assis"
    )
    .unwrap();
    drop(file);

    let mut resumed = build(&cli(&root, None, true), &root).await;
    assert_eq!(
        resumed.tx.snapshot(),
        checkpointed,
        "the snapshot stays authoritative for model history"
    );
    assert!(
        resumed.seq_counter.load(Ordering::SeqCst) >= 51,
        "seqs recorded after the checkpoint must not be reused"
    );
    resumed.deliver_instruction_context().await.unwrap();
    let history = resumed.tx.snapshot().to_string();
    for expected in [
        "[Checkpoint gap recovered]",
        "tool_search",
        "file_write",
        "notes.md",
        "torn final log record",
    ] {
        assert!(history.contains(expected), "{expected}: {history}");
    }
    // Recovery is on the durable log, the torn bytes are gone, and the next
    // checkpoint covers everything so a further resume is clean. The persist
    // drains the log writer before the file is read.
    resumed.persist().await.unwrap();
    drop(resumed);
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(
        text.contains("\"subtype\":\"checkpoint_gap_recovered\""),
        "{text}"
    );
    assert!(text.ends_with('\n'));
    assert!(!text.contains("assis\n"));
    let reopened = build(&cli(&root, None, true), &root).await;
    assert!(reopened.resume_gap_notice.is_none());
    assert_eq!(
        reopened
            .tx
            .snapshot()
            .to_string()
            .matches("[Checkpoint gap recovered]")
            .count(),
        1
    );
}
