//! Exercise the shared production constructor with isolated durable resources.
//! No provider authentication, process environment mutation, host instructions,
//! external MCP connections, or locality synchronization are needed.

use super::*;
use async_trait::async_trait;
use clap::Parser;
use std::path::{Path, PathBuf};

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
    build_with_runtime(cli, root, compaction_models, None, Arc::new(|_| {}))
        .await
        .unwrap()
}

async fn build_with_runtime(
    cli: &Cli,
    root: &Path,
    compaction_models: Arc<StdMutex<Vec<String>>>,
    instruction_fs: Option<Arc<dyn crate::instruction_io::InstructionFs>>,
    callback: crate::emit::EventCallback,
) -> Result<Session> {
    let store = SessionStore::open_in(root, None, cli.session_id.as_deref(), cli.resume.as_deref())
        .unwrap();
    let event_log = Arc::new(EventLog::at_path(
        store.store_path().with_extension("events.jsonl"),
    ));
    Session::build_with_runtime(
        cli,
        Some(callback),
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
            instruction_fs,
        }),
    )
    .await
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

const INSTRUCTION_BUDGET: std::time::Duration = std::time::Duration::from_millis(200);
/// Scheduling tolerance on top of the instruction budget for a build whose
/// other stages are controlled.
const BUILD_TOLERANCE: std::time::Duration = std::time::Duration::from_secs(2);

/// Store under `base`; discover instructions (no `--system-prompt`) in
/// `base/project`, a git root with one document and a synthetic Codex home.
fn discovering_cli(base: &Path, resume: bool) -> Cli {
    let project = base.join("project");
    let mut args = vec![
        "bro-harness",
        "--cwd",
        project.to_str().unwrap(),
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
    Cli::try_parse_from(args).unwrap()
}

fn discovering_workspace() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    std::fs::create_dir_all(base.join("project/.git")).unwrap();
    std::fs::create_dir_all(base.join("home")).unwrap();
    std::fs::write(base.join("project/AGENTS.md"), "PROJECT RULE").unwrap();
    (dir, base)
}

async fn build_discovering(
    base: &Path,
    resume: bool,
    fs: &Arc<crate::instruction_io::testing::GatedFs>,
) -> (Result<Session>, Arc<StdMutex<Vec<Value>>>) {
    let events = Arc::new(StdMutex::new(Vec::new()));
    let sink = events.clone();
    let vars = BTreeMap::from([
        (
            "CODEX_HOME".to_owned(),
            base.join("home").display().to_string(),
        ),
        (
            crate::instruction_io::TIMEOUT_ENV.to_owned(),
            INSTRUCTION_BUDGET.as_millis().to_string(),
        ),
    ]);
    let cli = discovering_cli(base, resume);
    let session = transport::with_session_env(
        vars,
        build_with_runtime(
            &cli,
            base,
            Arc::default(),
            Some(fs.clone()),
            Arc::new(move |event| sink.lock().unwrap().push(event)),
        ),
    )
    .await;
    (session, events)
}

fn logged_events(base: &Path) -> Vec<Value> {
    std::fs::read_to_string(base.join("startup.events.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap()["event"].clone())
        .collect()
}

fn instruction_timeouts(events: &[Value]) -> Vec<Value> {
    events
        .iter()
        .filter(|event| event["subtype"] == "instruction_read_timeout")
        .cloned()
        .collect()
}

/// Released workers drop their filesystem handle when they exit.
async fn wait_instruction_workers_exit(fs: &Arc<crate::instruction_io::testing::GatedFs>) {
    fs.release();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while Arc::strong_count(fs) > 1 {
        assert!(
            std::time::Instant::now() < deadline,
            "instruction worker leaked"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn production_startup_instruction_timeout_warns_starts_and_gates_inference() {
    use crate::instruction_io::FsOp;
    let (_dir, base) = discovering_workspace();
    let document = base.join("project/AGENTS.md");
    let fs = crate::instruction_io::testing::GatedFs::new();
    fs.stall_on(FsOp::Read, document.clone());
    let started = std::time::Instant::now();
    let (session, events) = build_discovering(&base, false, &fs).await;
    let mut session = session.unwrap();
    assert!(started.elapsed() < INSTRUCTION_BUDGET + BUILD_TOLERANCE);

    let emitted = instruction_timeouts(&events.lock().unwrap());
    assert_eq!(emitted.len(), 1, "{emitted:?}");
    assert_eq!(emitted[0]["phase"], "startup");
    assert_eq!(emitted[0]["operation"], "read");
    assert_eq!(emitted[0]["path"], document.to_string_lossy().as_ref());
    assert_eq!(emitted[0]["session_id"], "startup");
    // The warning precedes the ordinary start milestone in the durable log.
    let log = session.event_log.clone();
    tokio::task::spawn_blocking(move || log.flush_blocking())
        .await
        .unwrap();
    let logged = logged_events(&base);
    let warning = logged
        .iter()
        .position(|event| event["subtype"] == "instruction_read_timeout")
        .expect("timeout event in the durable log");
    let start = logged
        .iter()
        .position(|event| event["milestone"] == "session_start")
        .expect("session_start milestone");
    assert!(warning < start, "{logged:?}");

    // No provider request while the strict boundary refresh cannot complete.
    let (_cancel, cancel_rx) = watch::channel(false);
    let error = session
        .user_turn("task", cancel_rx, Arc::default())
        .await
        .unwrap_err();
    let error = format!("{error:#}");
    assert!(error.contains("instruction refresh timed out"), "{error}");
    assert!(error.contains(&*document.to_string_lossy()), "{error}");
    assert!(!session.tx.snapshot().to_string().contains("PROJECT RULE"));

    // Once the stalled worker exits, a fresh strict refresh delivers the
    // document before the request reaches the transport.
    fs.release();
    let (_cancel, cancel_rx) = watch::channel(false);
    let error = session
        .user_turn("task", cancel_rx, Arc::default())
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("startup tests must not request model inference"),
        "{error:#}"
    );
    let delivered = format!(
        "{}{}",
        session.tx.snapshot(),
        session.instruction_system.clone().unwrap_or_default()
    );
    assert!(delivered.contains("PROJECT RULE"), "{delivered}");
    drop(session);
    wait_instruction_workers_exit(&fs).await;
}

#[tokio::test]
async fn production_resume_instruction_timeout_fails_before_session_resume() {
    use crate::instruction_io::FsOp;
    let (_dir, base) = discovering_workspace();
    let document = base.join("project/AGENTS.md");
    let fs = crate::instruction_io::testing::GatedFs::new();
    let (initial, _) = build_discovering(&base, false, &fs).await;
    let mut initial = initial.unwrap();
    assert!(!initial.scoped_project_docs.active_documents().is_empty());
    initial.persist().await.unwrap();
    drop(initial);

    let fs = crate::instruction_io::testing::GatedFs::new();
    fs.stall_on(FsOp::Read, document.clone());
    let started = std::time::Instant::now();
    let (resumed, events) = build_discovering(&base, true, &fs).await;
    let error = format!("{:#}", resumed.err().expect("resume must fail closed"));
    // Startup discovery and resume restoration each spend their own budget.
    assert!(started.elapsed() < INSTRUCTION_BUDGET * 2 + BUILD_TOLERANCE);
    assert!(error.contains("instruction resume timed out"), "{error}");
    assert!(error.contains(&*document.to_string_lossy()), "{error}");
    let emitted = instruction_timeouts(&events.lock().unwrap());
    let phases: Vec<_> = emitted.iter().map(|event| event["phase"].clone()).collect();
    assert_eq!(phases, ["startup", "resume"], "{emitted:?}");
    assert_eq!(emitted[1]["path"], document.to_string_lossy().as_ref());
    // The failure is durable even though build returned before its
    // session_resume milestone.
    let logged = logged_events(&base);
    assert_eq!(instruction_timeouts(&logged), emitted, "{logged:?}");
    assert!(
        !logged
            .iter()
            .any(|event| event["milestone"] == "session_resume"),
        "{logged:?}"
    );
    wait_instruction_workers_exit(&fs).await;
}
