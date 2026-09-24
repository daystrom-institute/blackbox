//! Deadline, worker-bound, and commit-race tests for instruction I/O. A gated
//! filesystem stalls individual probes; every test releases its gate and
//! proves the worker exited, so no forever-blocked thread leaks.

use super::*;
use crate::emit::Emitter;
use crate::instruction_io::testing::{GatedFs, wait_workers_exit};
use crate::instruction_io::{FsOp, TIMEOUT_ENV};
use bro_tools::InstructionAccess::{Mutate, Read};
use bro_tools::InstructionPolicy;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant as StdInstant;

const BUDGET: Duration = Duration::from_millis(200);
/// Scheduling tolerance for deadline assertions: an operation must return
/// no later than its budget plus this.
const TOLERANCE: Duration = Duration::from_millis(750);

type Events = Arc<StdMutex<Vec<Value>>>;

fn capture() -> (Emitter, Events) {
    let events = Events::default();
    let sink = events.clone();
    let emitter = Emitter::with_callback(
        "instruction-test".into(),
        Arc::new(move |event| sink.lock().unwrap().push(event)),
    );
    (emitter, events)
}

fn timeouts(events: &Events) -> Vec<Value> {
    events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| event["subtype"] == "instruction_read_timeout")
        .cloned()
        .collect()
}

fn write(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

fn gated(
    root: &Path,
    fs: &Arc<GatedFs>,
    budget: Duration,
    emitter: Option<Emitter>,
) -> ScopedProjectDocs {
    ScopedProjectDocs::with_io(
        root.to_path_buf(),
        None,
        default_project_doc_files(),
        budget,
        fs.clone(),
        emitter,
    )
}

async fn startup(
    root: &Path,
    vars: BTreeMap<String, String>,
    system_prompt: Option<&str>,
    fs: &Arc<GatedFs>,
    emitter: Option<Emitter>,
) -> ScopedProjectDocs {
    crate::transport::with_session_env(
        vars,
        ScopedProjectDocs::for_session(root.to_path_buf(), system_prompt, fs.clone(), emitter),
    )
    .await
}

fn startup_vars(home: &Path, budget: Duration) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("CODEX_HOME".into(), home.display().to_string()),
        (TIMEOUT_ENV.into(), budget.as_millis().to_string()),
        (
            "BRO_HARNESS_PROJECT_DOC_FILES".into(),
            "AGENTS.override.md,AGENTS.md".into(),
        ),
    ])
}

fn request(path: PathBuf, access: bro_tools::InstructionAccess) -> bro_tools::InstructionPaths {
    bro_tools::InstructionPaths {
        paths: vec![path],
        access,
    }
}

fn assert_within_budget(started: StdInstant, budget: Duration) {
    let elapsed = started.elapsed();
    assert!(
        elapsed + Duration::from_millis(5) >= budget && elapsed < budget + TOLERANCE,
        "returned after {elapsed:?} with a {budget:?} budget"
    );
}

fn tool_error(result: Result<(), bro_tools::ToolResult>) -> Value {
    match result {
        Err(bro_tools::ToolResult::Error(error)) => serde_json::from_str(&error).unwrap(),
        other => panic!("expected a structured tool error, got {other:?}"),
    }
}

async fn wait_stalled(fs: &Arc<GatedFs>) {
    let fs = fs.clone();
    tokio::task::spawn_blocking(move || fs.wait_stalled())
        .await
        .unwrap();
}

async fn cleanup(fs: &Arc<GatedFs>, docs: &ScopedProjectDocs) {
    fs.release();
    let slot = docs.slot.clone();
    tokio::task::spawn_blocking(move || wait_workers_exit(&slot))
        .await
        .unwrap();
}

/// Workspace with a git root, a root document, and a nested include.
fn workspace() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let base = directory.path().canonicalize().unwrap();
    let root = base.join("project");
    let home = base.join("home");
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::create_dir_all(&home).unwrap();
    write(&root.join("AGENTS.md"), "ROOT RULES\nRead @rules.md.");
    write(&root.join("rules.md"), "INCLUDED RULES");
    (directory, root, home)
}

#[tokio::test]
async fn every_instruction_phase_times_out_on_each_stalled_probe_and_names_the_path() {
    let (_directory, root, home) = workspace();
    let seed = {
        let ledger = ScopedProjectDocs::with_names(root.clone(), None, default_project_doc_files());
        ledger.refresh().await.unwrap();
        ledger.active_documents()
    };
    assert_eq!(seed.len(), 2);
    let stalls = [
        (FsOp::Metadata, root.join(".git")),
        (FsOp::Metadata, root.join("AGENTS.md")),
        (FsOp::Canonicalize, root.join("AGENTS.md")),
        (FsOp::Read, root.join("AGENTS.md")),
        (FsOp::Read, root.join("rules.md")),
    ];
    for (operation, path) in stalls {
        for phase in [Phase::Startup, Phase::Refresh, Phase::Check, Phase::Resume] {
            let fs = GatedFs::new();
            fs.stall_on(operation, path.clone());
            let (emitter, events) = capture();
            let started = StdInstant::now();
            let (docs, message) = match phase {
                Phase::Startup => {
                    let docs =
                        startup(&root, startup_vars(&home, BUDGET), None, &fs, Some(emitter)).await;
                    (docs, None)
                }
                Phase::Refresh => {
                    let docs = gated(&root, &fs, BUDGET, Some(emitter));
                    let error = docs.refresh().await.unwrap_err();
                    (docs, Some(error))
                }
                Phase::Check => {
                    let docs = gated(&root, &fs, BUDGET, Some(emitter));
                    let error =
                        tool_error(docs.check(request(root.join("new.txt"), Mutate), 1).await);
                    assert_eq!(error["error"], "instruction_read_error");
                    assert_eq!(error["filesystem_effects"], false);
                    (docs, Some(error["message"].as_str().unwrap().to_owned()))
                }
                Phase::Resume => {
                    let docs = gated(&root, &fs, BUDGET, Some(emitter));
                    let error = docs.restore_documents(seed.clone()).await.unwrap_err();
                    (docs, Some(error))
                }
            };
            assert_within_budget(started, BUDGET);
            let case = format!(
                "{} {} during {}",
                operation.as_str(),
                path.display(),
                phase.as_str()
            );
            if let Some(message) = message {
                assert!(
                    message.contains(operation.as_str())
                        && message.contains(&*path.to_string_lossy()),
                    "{case}: {message}"
                );
            }
            let events = timeouts(&events);
            assert_eq!(events.len(), 1, "{case}: {events:?}");
            assert_eq!(events[0]["phase"], phase.as_str(), "{case}");
            assert_eq!(events[0]["operation"], operation.as_str(), "{case}");
            assert_eq!(events[0]["path"], path.to_string_lossy().as_ref(), "{case}");
            assert_eq!(events[0]["waiting_for_admission"], false, "{case}");
            if phase != Phase::Resume {
                assert!(docs.pending_batch(1).is_none(), "{case}");
            }
            cleanup(&fs, &docs).await;
        }
    }
}

#[tokio::test]
async fn startup_timeout_keeps_candidates_enrolled_and_boundary_refresh_stays_strict() {
    let (_directory, root, home) = workspace();
    write(&home.join("AGENTS.md"), "GLOBAL RULE");
    let fs = GatedFs::new();
    fs.stall_on(FsOp::Read, root.join("AGENTS.md"));
    let (emitter, events) = capture();
    let started = StdInstant::now();
    let docs = startup(&root, startup_vars(&home, BUDGET), None, &fs, Some(emitter)).await;
    assert_within_budget(started, BUDGET);
    assert_eq!(timeouts(&events).len(), 1);
    // The incomplete startup result is discarded, not partially applied.
    assert!(docs.pending_batch(1).is_none());
    assert_eq!(docs.observed_paths(), vec![root.clone()]);
    {
        let ledger = docs.lock();
        for name in [AGENTS_FILE, AGENTS_OVERRIDE_FILE] {
            assert!(
                ledger
                    .explicit_origins
                    .contains(&(home.join(name), root.clone()))
            );
        }
    }

    // The first boundary refresh is strict: it queues behind the stalled
    // startup worker and fails closed within its own budget.
    let started = StdInstant::now();
    let error = docs.refresh().await.unwrap_err();
    assert_within_budget(started, BUDGET);
    assert!(error.contains("waited for admission"), "{error}");
    assert!(
        error.contains(&*root.join("AGENTS.md").to_string_lossy()),
        "{error}"
    );
    assert!(timeouts(&events)[1]["waiting_for_admission"] == true);
    assert!(docs.pending_batch(1).is_none());

    cleanup(&fs, &docs).await;
    docs.refresh().await.unwrap();
    let batch = docs.pending_batch(1).unwrap();
    for body in ["GLOBAL RULE", "ROOT RULES", "INCLUDED RULES"] {
        assert!(batch.text.contains(body), "{body}");
    }
    assert_eq!(docs.slot.spawned_workers(), 2);
}

#[tokio::test]
async fn explicit_system_prompts_suppress_startup_discovery_without_reads() {
    for prompt in ["", "Explicit operator prompt."] {
        let (_directory, root, home) = workspace();
        write(&root.join("child/AGENTS.md"), "CHILD RULES");
        let fs = GatedFs::new();
        let docs = startup(&root, startup_vars(&home, BUDGET), Some(prompt), &fs, None).await;
        assert!(fs.calls().is_empty(), "{prompt:?}: {:?}", fs.calls());
        assert!(docs.observed_paths().is_empty());
        docs.refresh().await.unwrap();
        assert!(docs.pending_batch(1).is_none(), "{prompt:?}");
        // Later structured paths still opt their scopes into discovery.
        assert!(
            docs.check(request(root.join("child/file.txt"), Mutate), 1)
                .await
                .is_err()
        );
        assert!(docs.pending_batch(1).unwrap().text.contains("CHILD RULES"));
        cleanup(&fs, &docs).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_worker_holds_no_ledger_lock_and_concurrent_checks_keep_their_own_budget() {
    let (_directory, root, _home) = workspace();
    let fs = GatedFs::new();
    let docs = gated(&root, &fs, BUDGET, None);
    docs.refresh().await.unwrap();
    let delivered = docs.pending_batch(1).unwrap();
    docs.acknowledge(&delivered);

    fs.stall_on(FsOp::Read, root.join("AGENTS.md"));
    let first = tokio::spawn({
        let docs = docs.clone();
        let root = root.clone();
        async move {
            let started = StdInstant::now();
            let result = docs.check(request(root.join("a.txt"), Mutate), 1).await;
            (started.elapsed(), result)
        }
    });
    wait_stalled(&fs).await;

    // Every ledger operation stays responsive while the worker is blocked.
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn({
        let docs = docs.clone();
        let root = root.clone();
        move || {
            let snapshot = docs.snapshot_batch(2);
            docs.acknowledge(&snapshot);
            let _ = docs.pending_batch(2);
            docs.invalidate_delivery();
            let _ = docs.active_documents();
            docs.restore_observed_paths(vec![root.join("elsewhere")]);
            let _ = docs.observed_paths();
            sender.send(()).unwrap();
        }
    });
    receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("ledger operations blocked behind a stalled instruction read");

    // A check that starts later times out on its own deadline, reporting the
    // active worker's path without claiming a read of its own.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let started = StdInstant::now();
    let second = tool_error(docs.check(request(root.join("b.txt"), Mutate), 1).await);
    assert_within_budget(started, BUDGET);
    let message = second["message"].as_str().unwrap();
    assert!(message.contains("waited for admission"), "{message}");
    assert!(
        message.contains(&*root.join("AGENTS.md").to_string_lossy()),
        "{message}"
    );

    let (elapsed, first) = first.await.unwrap();
    assert!(elapsed < BUDGET + TOLERANCE, "{elapsed:?}");
    assert_eq!(tool_error(first)["filesystem_effects"], false);
    cleanup(&fs, &docs).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retries_against_a_blocked_worker_keep_one_worker_and_recover_after_release() {
    let (_directory, root, _home) = workspace();
    let document = root.join("AGENTS.md");
    let fs = GatedFs::new();
    fs.stall_on(FsOp::Read, document.clone());
    let docs = gated(&root, &fs, BUDGET, None);
    let first = tokio::spawn({
        let docs = docs.clone();
        async move { docs.refresh().await }
    });
    wait_stalled(&fs).await;

    let started = StdInstant::now();
    let retries = (0..40).map(|index| {
        let docs = docs.clone();
        let root = root.clone();
        async move {
            if index % 2 == 0 {
                docs.refresh().await.unwrap_err()
            } else {
                tool_error(
                    docs.check(request(root.join(format!("{index}.txt")), Read), 1)
                        .await,
                )["message"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            }
        }
    });
    for message in futures_util::future::join_all(retries).await {
        assert!(message.contains("waited for admission"), "{message}");
    }
    assert_within_budget(started, BUDGET);
    assert!(first.await.unwrap().is_err());
    assert_eq!(docs.slot.spawned_workers(), 1);
    assert_eq!(docs.slot.live_workers(), 1);

    cleanup(&fs, &docs).await;
    // No deferred scans ran after the stalled worker exited.
    assert_eq!(docs.slot.spawned_workers(), 1);
    assert_eq!(fs.reads_of(&document), 1);
    docs.refresh().await.unwrap();
    assert_eq!(docs.slot.spawned_workers(), 2);
    assert_eq!(fs.reads_of(&document), 2);
    assert!(docs.pending_batch(1).unwrap().text.contains("ROOT RULES"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admission_wait_spends_the_same_deadline_as_the_read() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    write(&root.join("AGENTS.md"), "ONLY RULE");
    let budget = Duration::from_millis(400);
    let fs = GatedFs::new();
    fs.delay_reads(Duration::from_millis(300));
    let docs = gated(&root, &fs, budget, None);
    let first = tokio::spawn({
        let docs = docs.clone();
        async move { docs.refresh().await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    // Admission takes most of this budget, so its own 300 ms read cannot
    // finish: a deadline restarted at admission would let it succeed.
    let started = StdInstant::now();
    let error = tool_error(docs.check(request(root.join("new.txt"), Read), 1).await);
    assert_within_budget(started, budget);
    let message = error["message"].as_str().unwrap();
    assert!(!message.contains("waited for admission"), "{message}");
    assert!(message.contains("read"), "{message}");
    first.await.unwrap().unwrap();
    cleanup(&fs, &docs).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_results_after_timeout_or_cancellation_never_commit() {
    let (_directory, root, _home) = workspace();
    write(&root.join("child/AGENTS.md"), "CHILD RULES");
    let fs = GatedFs::new();
    let docs = gated(&root, &fs, BUDGET, None);
    docs.refresh().await.unwrap();
    docs.acknowledge(&docs.pending_batch(1).unwrap());
    let before = docs.active_documents();

    // Once released, the stalled check's scan would enroll its path and
    // observe both the changed bytes and the removed include.
    write(&root.join("AGENTS.md"), "ROOT RULES v2 without the include");
    fs.stall_on(FsOp::Read, root.join("AGENTS.md"));
    assert!(
        docs.check(request(root.join("child/new.txt"), Read), 1)
            .await
            .is_err()
    );
    assert!(docs.refresh().await.is_err());
    cleanup(&fs, &docs).await;
    assert!(docs.pending_batch(2).is_none());
    assert_eq!(docs.active_documents(), before);
    assert!(!docs.observed_paths().contains(&root.join("child/new.txt")));

    // A cancelled caller discards its worker's result the same way.
    let fs = GatedFs::new();
    let docs = gated(&root, &fs, BUDGET, None);
    fs.stall_on(FsOp::Read, root.join("AGENTS.md"));
    assert!(
        tokio::time::timeout(Duration::from_millis(50), docs.refresh())
            .await
            .is_err()
    );
    cleanup(&fs, &docs).await;
    assert!(docs.pending_batch(1).is_none());
    assert!(docs.active_documents().is_empty());
    docs.refresh().await.unwrap();
    assert!(
        docs.pending_batch(1)
            .unwrap()
            .text
            .contains("ROOT RULES v2")
    );
}

/// Fire `change` once, from inside the worker's first read of `path`.
fn interfere_once(fs: &Arc<GatedFs>, path: PathBuf, change: impl Fn() + Send + Sync + 'static) {
    let fired = AtomicBool::new(false);
    fs.on_call(move |operation, candidate| {
        if operation == FsOp::Read && candidate == path && !fired.swap(true, Ordering::SeqCst) {
            change();
        }
    });
}

#[tokio::test]
async fn conflicting_ledger_changes_retry_within_the_budget_and_preserve_current_state() {
    let (_directory, root, _home) = workspace();
    let document = root.join("AGENTS.md");

    // Compaction invalidation during a scan survives the retried commit.
    let fs = GatedFs::new();
    let docs = gated(&root, &fs, BUDGET, None);
    docs.refresh().await.unwrap();
    docs.acknowledge(&docs.pending_batch(1).unwrap());
    let other = docs.clone();
    interfere_once(&fs, document.clone(), move || other.invalidate_delivery());
    docs.refresh().await.unwrap();
    assert_eq!(docs.slot.spawned_workers(), 3);
    assert_eq!(docs.pending_batch(2).unwrap().documents.len(), 2);
    fs.clear_hook();

    // A stale acknowledgment landing mid-scan cannot deliver newer bytes.
    let fs = GatedFs::new();
    let docs = gated(&root, &fs, BUDGET, None);
    docs.refresh().await.unwrap();
    let stale = docs.pending_batch(1).unwrap();
    write(&document, "ROOT RULES v2\nRead @rules.md.");
    let other = docs.clone();
    interfere_once(&fs, document.clone(), move || other.acknowledge(&stale));
    docs.refresh().await.unwrap();
    let current = docs.pending_batch(2).unwrap();
    assert_eq!(current.documents.len(), 1);
    assert!(current.text.contains("ROOT RULES v2"));
    assert!(
        docs.check(request(root.join("file.txt"), Mutate), 1)
            .await
            .is_err()
    );
    fs.clear_hook();

    // A path observed mid-scan is included by the retry.
    write(&root.join("child/AGENTS.md"), "CHILD RULES");
    let fs = GatedFs::new();
    let docs = gated(&root, &fs, BUDGET, None);
    let other = docs.clone();
    let child = root.join("child/file.txt");
    interfere_once(&fs, document.clone(), move || {
        other.restore_observed_paths(vec![child.clone()])
    });
    docs.refresh().await.unwrap();
    assert!(docs.pending_batch(1).unwrap().text.contains("CHILD RULES"));
    fs.clear_hook();

    // Sustained interference expires on the original deadline and applies
    // nothing from the discarded reads.
    let fs = GatedFs::new();
    let docs = gated(&root, &fs, BUDGET, None);
    docs.refresh().await.unwrap();
    let delivered = docs.pending_batch(1).unwrap();
    docs.acknowledge(&delivered);
    write(&document, "ROOT RULES v3\nRead @rules.md.");
    let other = docs.clone();
    fs.on_call(move |operation, _| {
        if operation == FsOp::Read {
            other.invalidate_delivery();
        }
    });
    let started = StdInstant::now();
    let error = docs.refresh().await.unwrap_err();
    assert_within_budget(started, BUDGET);
    assert!(
        error.contains("discarded because instruction state changed"),
        "{error}"
    );
    let pending = docs.pending_batch(2).unwrap();
    assert!(!pending.text.contains("ROOT RULES v3"));
    fs.clear_hook();
    docs.refresh().await.unwrap();
    assert!(
        docs.pending_batch(3)
            .unwrap()
            .text
            .contains("ROOT RULES v3")
    );
    cleanup(&fs, &docs).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simultaneous_mutations_serialize_and_each_queue_their_own_scope() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    write(&root.join("left/AGENTS.md"), "LEFT RULES");
    write(&root.join("right/AGENTS.md"), "RIGHT RULES");
    let fs = GatedFs::new();
    fs.delay_reads(Duration::from_millis(20));
    let docs = gated(&root, &fs, Duration::from_secs(5), None);
    let (left, right) = tokio::join!(
        docs.check(request(root.join("left/a.txt"), Mutate), 1),
        docs.check(request(root.join("right/b.txt"), Mutate), 1),
    );
    assert_eq!(tool_error(left)["error"], "instructions_required");
    assert_eq!(tool_error(right)["error"], "instructions_required");
    let observed = docs.observed_paths();
    assert!(observed.contains(&root.join("left/a.txt")));
    assert!(observed.contains(&root.join("right/b.txt")));
    let batch = docs.pending_batch(2).unwrap();
    assert!(batch.text.contains("LEFT RULES") && batch.text.contains("RIGHT RULES"));
    assert!(!root.join("left/a.txt").exists() && !root.join("right/b.txt").exists());
    cleanup(&fs, &docs).await;
}

#[tokio::test]
async fn session_settings_are_snapshotted_and_isolated_across_library_sessions() {
    let directory = tempfile::tempdir().unwrap();
    let base = directory.path().canonicalize().unwrap();
    let root = base.join("project");
    write(&root.join("RULES_A.md"), "PROJECT A");
    write(&root.join("RULES_B.md"), "PROJECT B");
    write(&base.join("home-a/AGENTS.md"), "GLOBAL A");
    write(&base.join("home-b/AGENTS.md"), "GLOBAL B");
    let vars = |label: &str, millis: &str| {
        BTreeMap::from([
            (
                "CODEX_HOME".to_owned(),
                base.join(format!("home-{label}")).display().to_string(),
            ),
            (TIMEOUT_ENV.to_owned(), millis.to_owned()),
            (
                "BRO_HARNESS_PROJECT_DOC_FILES".to_owned(),
                format!("RULES_{}.md", label.to_uppercase()),
            ),
        ])
    };
    let (fs_a, fs_b) = (GatedFs::new(), GatedFs::new());
    let (a, b) = tokio::join!(
        startup(&root, vars("a", "150"), None, &fs_a, None),
        startup(&root, vars("b", "0"), None, &fs_b, None),
    );
    assert_eq!(a.timeout, Duration::from_millis(150));
    assert_eq!(b.timeout, instruction_io::DEFAULT_TIMEOUT);
    // Refresh outside either session scope still uses each captured config.
    for (docs, own, foreign) in [(&a, "A", "B"), (&b, "B", "A")] {
        docs.refresh().await.unwrap();
        let text = docs.pending_batch(1).unwrap().text;
        assert!(text.contains(&format!("PROJECT {own}")), "{text}");
        assert!(text.contains(&format!("GLOBAL {own}")), "{text}");
        assert!(!text.contains(&format!("PROJECT {foreign}")), "{text}");
        assert!(!text.contains(&format!("GLOBAL {foreign}")), "{text}");
    }
    cleanup(&fs_a, &a).await;
    cleanup(&fs_b, &b).await;
}

#[test]
fn embedded_runtime_shuts_down_without_joining_a_stalled_worker() {
    let (_directory, root, _home) = workspace();
    let fs = GatedFs::new();
    fs.stall_on(FsOp::Read, root.join("AGENTS.md"));
    let docs = gated(&root, &fs, BUDGET, None);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    assert!(runtime.block_on(docs.refresh()).is_err());
    assert_eq!(docs.slot.live_workers(), 1);
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        drop(runtime);
        sender.send(()).unwrap();
    });
    receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("runtime shutdown joined the stalled instruction worker");
    fs.release();
    wait_workers_exit(&docs.slot);
}

#[tokio::test]
async fn timed_out_guarded_mutation_performs_no_effect() {
    use bro_tools::Tool;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    write(&root.join("child/AGENTS.md"), "CHILD RULES");
    let fs = GatedFs::new();
    fs.stall_on(FsOp::Read, root.join("child/AGENTS.md"));
    let docs = Arc::new(gated(&root, &fs, BUDGET, None));
    let cx = bro_tools::ToolCx {
        tool_observations: Default::default(),
        instruction_generation: 1,
        instruction_policy: Some(docs.clone()),
        root: root.clone(),
        cancellation: Default::default(),
        output_budget: 16 * 1024,
        safety: Arc::new(bro_tools::SafetyPolicy::new()),
        http: reqwest::Client::new(),
        todos: Default::default(),
        shell_sessions: Default::default(),
        edits: Default::default(),
        session_env: Default::default(),
        child_env: Default::default(),
        shell_env: Default::default(),
        tool_arg_defaults: Default::default(),
    };
    let tool = bro_tools::workspace::FileWrite;
    let result = bro_tools::call_tool_with_arg_defaults(
        &tool,
        tool.name(),
        serde_json::json!({"file_path":"child/new.txt", "content":"new"}),
        &cx,
    )
    .await;
    let bro_tools::ToolResult::Error(error) = result else {
        panic!("guarded write must fail closed: {result:?}");
    };
    assert!(error.contains("instruction_read_error"), "{error}");
    assert!(error.contains("\"filesystem_effects\":false"), "{error}");
    assert!(!root.join("child/new.txt").exists());
    cleanup(&fs, &docs).await;
    assert!(!root.join("child/new.txt").exists());
}

#[tokio::test]
async fn timeout_diagnostics_omit_instruction_text_and_environment_values() {
    let (_directory, root, home) = workspace();
    write(&root.join("AGENTS.md"), "CONFIDENTIAL INSTRUCTION BODY");
    let fs = GatedFs::new();
    fs.stall_on(FsOp::Read, root.join("AGENTS.md"));
    let mut vars = startup_vars(&home, BUDGET);
    vars.insert("BRO_HARNESS_API_KEY".into(), "sk-secret-value".into());
    let (emitter, events) = capture();
    let docs = startup(&root, vars, None, &fs, Some(emitter)).await;
    let error = docs.refresh().await.unwrap_err();
    let events = timeouts(&events);
    assert_eq!(events.len(), 2);
    for text in events
        .iter()
        .map(Value::to_string)
        .chain(std::iter::once(error))
    {
        assert!(!text.contains("CONFIDENTIAL"), "{text}");
        assert!(!text.contains("sk-secret-value"), "{text}");
    }
    for event in &events {
        for field in [
            "phase",
            "operation",
            "path",
            "budget_ms",
            "reason",
            "session_id",
        ] {
            assert!(!event[field].is_null(), "{field}: {event}");
        }
    }
    assert_eq!(events[0]["budget_ms"], 200);
    cleanup(&fs, &docs).await;
}
