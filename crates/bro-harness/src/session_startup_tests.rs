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
    build_with_kind(
        cli,
        root,
        compaction_models,
        instruction_fs,
        callback,
        TransportKind::Anthropic,
    )
    .await
}

async fn build_with_kind(
    cli: &Cli,
    root: &Path,
    compaction_models: Arc<StdMutex<Vec<String>>>,
    instruction_fs: Option<Arc<dyn crate::instruction_io::InstructionFs>>,
    callback: crate::emit::EventCallback,
    kind: TransportKind,
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
            kind,
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
    build_discovering_within(base, resume, fs, INSTRUCTION_BUDGET).await
}

/// `budget` bounds each instruction phase. A build that stalls a read on
/// purpose uses the short test budget so its timeout arrives quickly. A build
/// that must find its documents uses the production default: its discovery
/// does real filesystem work, and a short budget there would time how fast
/// the machine is, since a startup timeout is not an error and simply leaves
/// the session without documents.
async fn build_discovering_within(
    base: &Path,
    resume: bool,
    fs: &Arc<crate::instruction_io::testing::GatedFs>,
    budget: std::time::Duration,
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
            budget.as_millis().to_string(),
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
        .user_turn("task", cancel_rx, MidTurnInputs::new())
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
        .user_turn("task", cancel_rx, MidTurnInputs::new())
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
    let (initial, _) =
        build_discovering_within(&base, false, &fs, crate::instruction_io::DEFAULT_TIMEOUT).await;
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

fn discipline_cli(root: &Path, extra: &[&str], resume: bool) -> Cli {
    let mut args = vec![
        "bro-harness",
        "--cwd",
        root.to_str().unwrap(),
        "--system-prompt",
        "",
    ];
    if resume {
        args.extend(["--resume", "discipline"]);
    } else {
        args.extend(["--session-id", "discipline", "--model", "gpt-5.5"]);
    }
    args.extend(extra);
    Cli::try_parse_from(args).unwrap()
}

async fn try_build(cli: &Cli, root: &Path) -> Result<Session> {
    build_with_runtime(cli, root, Arc::default(), None, Arc::new(|_| {})).await
}

fn wire_names(session: &Session) -> Vec<String> {
    session
        .reg
        .wire_specs()
        .into_iter()
        .map(|spec| spec.name)
        .collect()
}

fn refusal_of(tool: &str) -> String {
    crate::edit_discipline::EditDiscipline::Structured
        .refusal(tool)
        .unwrap()
}

#[tokio::test]
async fn structured_edit_discipline_refuses_raw_edit_tools_flat_deferred_and_in_cells() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let fixture = root.join("fixture.txt");
    std::fs::write(&fixture, "original bytes\n").unwrap();

    // An allow list naming the raw tools cannot bring them back.
    let mut session = try_build(
        &discipline_cli(
            &root,
            &[
                "--edit-discipline",
                "structured",
                "--allow-tools",
                "file_edit,file_write,apply_patch,file_read,exec,wait",
            ],
            false,
        ),
        &root,
    )
    .await
    .unwrap();
    session.cx.root = root.clone();
    let wire = wire_names(&session);
    assert!(wire.contains(&"file_read".to_string()), "{wire:?}");
    assert!(wire.contains(&"exec".to_string()), "{wire:?}");
    let manifest: Vec<String> = session.reg.manifest().into_iter().map(|(n, _)| n).collect();
    for tool in crate::edit_discipline::RAW_EDIT_TOOLS {
        assert!(!wire.contains(&tool.to_string()), "{wire:?}");
        assert!(!manifest.contains(&tool.to_string()), "{manifest:?}");
        // A flat call explains the refusal instead of reporting an unknown tool.
        let flat = session
            .reg
            .dispatch(
                tool,
                json!({"file_path":"fixture.txt","content":"changed","old_string":"original","new_string":"changed"}),
                &session.cx,
            )
            .await;
        assert!(flat.is_error());
        assert_eq!(flat.into_content().0, refusal_of(tool));
    }
    let unknown = session
        .reg
        .dispatch("never_registered", json!({}), &session.cx)
        .await;
    assert_eq!(unknown.into_content().0, "unknown tool: never_registered");

    // Inside a cell, dot and bracket access both reach the same refusal, and
    // the names stay out of the cell catalog.
    let cell = session
        .reg
        .dispatch(
            "exec",
            json!({"source": r#"
const names = ["file_edit", "file_write", "apply_patch"];
const seen = [];
for (const name of names) {
  for (const call of [() => tools[name]({file_path: "fixture.txt", content: "changed"}),
                      () => (name === "file_edit" ? tools.file_edit : name === "file_write" ? tools.file_write : tools.apply_patch)({file_path: "fixture.txt", content: "changed"})]) {
    try { await call(); seen.push("no error"); } catch (e) { seen.push(e.message); }
  }
}
text(JSON.stringify({
  seen,
  listed: ALL_TOOLS.filter((tool) => names.includes(tool.name)).length,
  enumerable: Object.keys(tools).filter((name) => names.includes(name)).length,
}));
"#}),
            &session.cx,
        )
        .await;
    let output = cell.into_content().0;
    for tool in crate::edit_discipline::RAW_EDIT_TOOLS {
        assert_eq!(output.matches(&refusal_of(tool)).count(), 2, "{output}");
    }
    assert!(!output.contains("no error"), "{output}");
    assert!(
        output.contains(r#"\"listed\":0"#) || output.contains(r#""listed":0"#),
        "{output}"
    );
    assert!(
        output.contains(r#"\"enumerable\":0"#) || output.contains(r#""enumerable":0"#),
        "{output}"
    );
    assert_eq!(
        std::fs::read_to_string(&fixture).unwrap(),
        "original bytes\n"
    );
}

#[tokio::test]
async fn edit_discipline_is_saved_with_the_session_and_restored_without_the_flag() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut initial = try_build(
        &discipline_cli(&root, &["--edit-discipline", "structured"], false),
        &root,
    )
    .await
    .unwrap();
    initial.persist().await.unwrap();
    drop(initial);

    // A routine resume passes neither the flag nor a deny list.
    let mut resumed = try_build(&discipline_cli(&root, &[], true), &root)
        .await
        .unwrap();
    assert_eq!(
        resumed.edit_discipline,
        crate::edit_discipline::EditDiscipline::Structured
    );
    assert!(!wire_names(&resumed).contains(&"file_edit".to_string()));
    resumed.persist().await.unwrap();
    drop(resumed);

    // The saved session cannot be resumed without a code surface.
    let refused = try_build(&discipline_cli(&root, &["--code-mode", "off"], true), &root)
        .await
        .err()
        .expect("structured without a code surface must not start")
        .to_string();
    assert!(
        refused.contains("edit_discipline 'structured'"),
        "{refused}"
    );
    assert!(refused.contains("code_mode 'off'"), "{refused}");

    // An explicit value replaces the saved one and is saved in turn.
    let mut freed = try_build(
        &discipline_cli(&root, &["--edit-discipline", "free"], true),
        &root,
    )
    .await
    .unwrap();
    assert!(wire_names(&freed).contains(&"file_edit".to_string()));
    freed.persist().await.unwrap();
    drop(freed);
    let reopened = try_build(&discipline_cli(&root, &[], true), &root)
        .await
        .unwrap();
    assert_eq!(
        reopened.edit_discipline,
        crate::edit_discipline::EditDiscipline::Free
    );
}

#[tokio::test]
async fn edit_discipline_defaults_to_free_and_never_guesses_an_unknown_value() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();

    // Structured needs a code surface on a fresh session too.
    let refused = try_build(
        &discipline_cli(
            &root,
            &["--edit-discipline", "structured", "--code-mode", "off"],
            false,
        ),
        &root,
    )
    .await
    .err()
    .expect("structured with code mode off must not start")
    .to_string();
    assert!(refused.contains("code_mode 'off'"), "{refused}");
    let unknown = try_build(
        &discipline_cli(&root, &["--edit-discipline", "strict"], false),
        &root,
    )
    .await
    .err()
    .expect("an unknown discipline must not start")
    .to_string();
    assert!(
        unknown.contains("unknown edit discipline 'strict'"),
        "{unknown}"
    );

    // A snapshot written before the field existed resumes as free, with
    // today's surface.
    crate::session::write_atomic(
        &root.join("discipline.json"),
        &json!({"transport":"anthropic", "model":"gpt-5.5", "snapshot":[]}).to_string(),
    )
    .unwrap();
    let legacy = try_build(&discipline_cli(&root, &[], true), &root)
        .await
        .unwrap();
    assert_eq!(
        legacy.edit_discipline,
        crate::edit_discipline::EditDiscipline::Free
    );
    let wire = wire_names(&legacy);
    assert!(wire.contains(&"file_edit".to_string()), "{wire:?}");
    assert!(wire.contains(&"file_write".to_string()), "{wire:?}");
}

#[tokio::test]
async fn structured_edit_discipline_refuses_apply_patch_where_the_transport_offers_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let build = |extra: &'static [&'static str], id: &'static str| {
        let root = root.clone();
        async move {
            let mut args = vec![
                "bro-harness",
                "--cwd",
                root.to_str().unwrap(),
                "--system-prompt",
                "",
                "--session-id",
                id,
                "--model",
                "gpt-5.5",
            ];
            args.extend(extra);
            build_with_kind(
                &Cli::try_parse_from(args).unwrap(),
                &root,
                Arc::default(),
                None,
                Arc::new(|_| {}),
                TransportKind::OpenAiResponses,
            )
            .await
            .unwrap()
        }
    };

    // A grammar transport offers apply_patch to a free session.
    let free = build(&[], "grammar-free").await;
    assert!(
        free.reg
            .manifest()
            .iter()
            .any(|(name, _)| name == "apply_patch")
            || wire_names(&free).contains(&"apply_patch".to_string()),
        "the grammar transport must offer apply_patch for this test to mean anything"
    );

    let structured = build(&["--edit-discipline", "structured"], "grammar-structured").await;
    let wire = wire_names(&structured);
    let manifest: Vec<String> = structured
        .reg
        .manifest()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    for tool in crate::edit_discipline::RAW_EDIT_TOOLS {
        assert!(!wire.contains(&tool.to_string()), "{wire:?}");
        assert!(!manifest.contains(&tool.to_string()), "{manifest:?}");
    }
    let refused = structured
        .reg
        .dispatch(
            "apply_patch",
            json!("*** Begin Patch\n*** End Patch"),
            &structured.cx,
        )
        .await;
    assert_eq!(refused.into_content().0, refusal_of("apply_patch"));
}

#[tokio::test]
async fn structured_edit_discipline_still_applies_edit_sets_and_holds_across_cells() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let fixture = root.join("notes.txt");
    std::fs::write(&fixture, "alpha\nbeta\n").unwrap();
    let mut session = try_build(
        &discipline_cli(&root, &["--edit-discipline", "structured"], false),
        &root,
    )
    .await
    .unwrap();
    session.cx.root = root.clone();

    // The supported path works, on a non-source text file.
    let applied = session
        .reg
        .dispatch(
            "exec",
            json!({"source": r#"
const es = await edits.begin();
await edits.replaceText({ es, file: "notes.txt", find: "beta", replace: "gamma" });
const result = await edits.apply({ es });
text(JSON.stringify(result));
"#}),
            &session.cx,
        )
        .await;
    let output = applied.into_content().0;
    assert_eq!(
        std::fs::read_to_string(&fixture).unwrap(),
        "alpha\ngamma\n",
        "{output}"
    );

    // Two fresh cells running at once and a cell that yielded and was waited
    // on all meet the same refusal.
    let probe = r#"
try { await tools.file_write({ file_path: "notes.txt", content: "raw" }); text("no error"); }
catch (e) { text(e.message); }
"#;
    let (first, second) = tokio::join!(
        session
            .reg
            .dispatch("exec", json!({ "source": probe }), &session.cx),
        session
            .reg
            .dispatch("exec", json!({ "source": probe }), &session.cx),
    );
    for output in [first.into_content().0, second.into_content().0] {
        assert!(output.contains(&refusal_of("file_write")), "{output}");
    }
    let yielded = session
        .reg
        .dispatch(
            "exec",
            json!({"source": format!(
                "// @exec: {{\"yield_time_ms\": 1}}\nawait new Promise((resolve) => setTimeout(resolve, 300));{probe}"
            )}),
            &session.cx,
        )
        .await
        .into_content()
        .0;
    let cell_id = yielded
        .split("cell ID ")
        .nth(1)
        .and_then(|rest| rest.split('.').next())
        .unwrap_or_else(|| panic!("the cell did not yield: {yielded}"))
        .to_string();
    let waited = session
        .reg
        .dispatch(
            "wait",
            json!({ "cell_id": cell_id, "yield_time_ms": 5000 }),
            &session.cx,
        )
        .await
        .into_content()
        .0;
    assert!(waited.contains(&refusal_of("file_write")), "{waited}");
    assert_eq!(std::fs::read_to_string(&fixture).unwrap(), "alpha\ngamma\n");
}

async fn reporting_session(root: &Path) -> (Session, Arc<StdMutex<Vec<Value>>>) {
    let events = Arc::new(StdMutex::new(Vec::new()));
    let sink = events.clone();
    let mut session = build_with_runtime(
        &discipline_cli(root, &[], false),
        root,
        Arc::default(),
        None,
        Arc::new(move |event| sink.lock().unwrap().push(event)),
    )
    .await
    .unwrap();
    session.cx.root = root.to_path_buf();
    (session, events)
}

fn shell_reports(events: &StdMutex<Vec<Value>>) -> Vec<Value> {
    events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| event["type"] == "harness_shell_sessions")
        .cloned()
        .collect()
}

/// The newest report, once one satisfies `wanted`. Reports are change-driven,
/// so this waits for the publisher instead of polling the registry.
async fn await_shell_report(
    events: &StdMutex<Vec<Value>>,
    what: &str,
    wanted: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if let Some(report) = shell_reports(events).last().filter(|report| wanted(report)) {
            return report.clone();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no shell report where {what}: {:?}",
            shell_reports(events)
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

fn running_ids(report: &Value) -> Vec<String> {
    report["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|session| session["running"] == true)
        .map(|session| session["id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn shell_session_reports_follow_start_exit_kill_and_removal_without_polling() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let (session, events) = reporting_session(&root).await;

    // One report at process start: this process has no shell sessions.
    let initial = shell_reports(&events);
    assert_eq!(initial.len(), 1, "{initial:?}");
    assert_eq!(initial[0]["sessions"], json!([]));
    assert_eq!(initial[0]["session_id"], "discipline");
    assert!(initial[0]["seq"].as_u64().is_some());

    // A command that outlives its yield is reported running, with a bounded
    // command head and nothing of its output.
    // The command prints OUTPUT-MARKER without containing that text itself.
    let long_command = format!(
        "printf 'OUT%s' PUT-MARKER; sleep 30 # {}",
        "\u{00e9}".repeat(300)
    );
    let started = session
        .reg
        .dispatch(
            "shell_run",
            json!({ "command": long_command, "yield_time_ms": 50 }),
            &session.cx,
        )
        .await
        .into_content()
        .0;
    let started: Value = serde_json::from_str(&started).unwrap();
    let long_id = started["session_id"].as_str().unwrap().to_string();
    let report = await_shell_report(&events, "the long command is running", |report| {
        running_ids(report) == vec![long_id.clone()]
    })
    .await;
    let row = &report["sessions"][0];
    assert_eq!(row["command"].as_str().unwrap().chars().count(), 120);
    assert!(row["elapsed_ms"].as_u64().is_some());
    assert!(!report.to_string().contains("OUTPUT-MARKER"), "{report}");

    // A second command exits by itself. Nobody polls it; the report still
    // says it stopped running, and it stays listed while its output is unread.
    let quick = session
        .reg
        .dispatch(
            "shell_run",
            json!({ "command": "sleep 0.4; echo done", "yield_time_ms": 1 }),
            &session.cx,
        )
        .await
        .into_content()
        .0;
    let quick: Value = serde_json::from_str(&quick).unwrap();
    let quick_id = quick["session_id"].as_str().unwrap().to_string();
    let report = await_shell_report(&events, "the quick command exited unpolled", |report| {
        report["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|session| session["id"] == quick_id.as_str() && session["running"] == false)
    })
    .await;
    assert_eq!(running_ids(&report), vec![long_id.clone()]);

    // Reading its output removes it, which is reported too.
    session
        .reg
        .dispatch(
            "shell_poll",
            json!({ "session_id": quick_id, "yield_time_ms": 0 }),
            &session.cx,
        )
        .await;
    await_shell_report(&events, "the quick command is gone", |report| {
        report["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|session| session["id"] != quick_id.as_str())
    })
    .await;

    // Killing the long command ends with an empty report.
    session
        .reg
        .dispatch("shell_kill", json!({ "session_id": long_id }), &session.cx)
        .await;
    let last = await_shell_report(&events, "no session is running", |report| {
        running_ids(report).is_empty()
    })
    .await;

    // Reports carry strictly increasing sequence numbers, and the event log
    // holds the same reports under the same numbers.
    let reports = shell_reports(&events);
    let seqs: Vec<u64> = reports
        .iter()
        .map(|report| report["seq"].as_u64().unwrap())
        .collect();
    assert!(seqs.windows(2).all(|pair| pair[0] < pair[1]), "{seqs:?}");
    session.event_log.flush_blocking_checked().unwrap();
    let logged = std::fs::read_to_string(session.event_log.path()).unwrap();
    let logged: Vec<u64> = logged
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .map(|line| line["event"].clone())
        .filter(|event| event["type"] == "harness_shell_sessions")
        .map(|event| event["seq"].as_u64().unwrap())
        .collect();
    assert_eq!(logged, seqs);
    assert_eq!(*seqs.last().unwrap(), last["seq"].as_u64().unwrap());
}

#[tokio::test]
async fn shell_session_reports_never_read_empty_while_shutdown_is_still_reaping() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let (session, events) = reporting_session(&root).await;
    for _ in 0..2 {
        session
            .reg
            .dispatch(
                "shell_run",
                json!({ "command": "sleep 30", "yield_time_ms": 20 }),
                &session.cx,
            )
            .await;
    }
    await_shell_report(&events, "two commands are running", |report| {
        running_ids(report).len() == 2
    })
    .await;
    let before = shell_reports(&events).len();

    bro_tools::shell::shutdown_shell_sessions(&session.cx).await;
    await_shell_report(&events, "shutdown finished", |report| {
        report["sessions"] == json!([])
    })
    .await;

    // Every report between the shutdown and the final empty one still showed
    // the sessions being stopped as running: none of them claimed an empty
    // set while a process was pending.
    let reports = shell_reports(&events);
    let (last, during) = reports[before..].split_last().unwrap();
    assert_eq!(last["sessions"], json!([]));
    for report in during {
        assert!(!running_ids(report).is_empty(), "{report}");
    }
    assert!(session.cx.shell_sessions.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_command_consumed_before_the_publisher_wakes_adds_no_report() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let (session, events) = reporting_session(&root).await;
    // Runs to completion inside its own call: started, exited and removed
    // before the invocation returns.
    for _ in 0..5 {
        let result = session
            .reg
            .dispatch(
                "shell_run",
                json!({ "command": "true", "yield_time_ms": 0 }),
                &session.cx,
            )
            .await
            .into_content()
            .0;
        assert!(result.contains("\"running\":false"), "{result}");
    }
    // Give the publisher every chance to speak, then require the picture to
    // have ended where it began: any reports in between came in pairs.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let reports = shell_reports(&events);
    assert_eq!(reports.last().unwrap()["sessions"], json!([]));
    assert!(
        reports.len() <= 1 + 2 * 5,
        "more reports than transitions: {reports:?}"
    );
    for pair in reports.windows(2) {
        assert_ne!(
            pair[0]["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|session| (session["id"].clone(), session["running"].clone()))
                .collect::<Vec<_>>(),
            pair[1]["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|session| (session["id"].clone(), session["running"].clone()))
                .collect::<Vec<_>>(),
            "two consecutive reports show the same picture"
        );
    }
}

#[tokio::test]
async fn a_shell_command_started_inside_a_cell_is_reported_like_a_flat_one() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let (session, events) = reporting_session(&root).await;
    let cell = session
        .reg
        .dispatch(
            "exec",
            json!({"source": r#"
const started = await tools.shell_run({ command: "sleep 30", yield_time_ms: 20 });
text(started.session_id);
"#}),
            &session.cx,
        )
        .await
        .into_content()
        .0;
    let report = await_shell_report(&events, "the nested command is running", |report| {
        running_ids(report).len() == 1
    })
    .await;
    let id = running_ids(&report).remove(0);
    assert!(cell.contains(&id), "{cell}");
    session
        .reg
        .dispatch("shell_kill", json!({ "session_id": id }), &session.cx)
        .await;
    await_shell_report(&events, "the nested command stopped", |report| {
        running_ids(report).is_empty()
    })
    .await;
}
