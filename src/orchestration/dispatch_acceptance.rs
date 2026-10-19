
/// Acceptance tests for the dispatch plane: the tool handlers drive a real
/// `fleetd` process, which spawns a stub child standing in for a native provider CLI.
/// The stub records the argv and environment it was started with, so every
/// assertion about routing is made on what the child actually received.
#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod acceptance {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use rmcp::handler::server::wrapper::Parameters;
    use serde_json::{Value, json};

    use super::smoke::{FleetdProcess, tool_text};

    /// A stub standing in for a native provider CLI. It records what it was started
    /// with under `<root>/child-<pid>/` (argv, cwd, the provider it was told to
    /// be, the path it was run as, and environment variable names, never
    /// values), appends every stdin line it receives, and, when it was given
    /// no session, mints `stub-thread-<pid>` and reports it in an `init`
    /// event containing its session identity. It ends a turn with an
    /// error result and keeps running when a line mentions TURN_FAIL,
    /// finishes its turn when a line mentions FINISH, and records a
    /// termination signal before exiting on one.
    fn write_recording_stub(root: &Path) -> PathBuf {
        let path = root.join("recording-harness.sh");
        let script = format!(
            "#!/bin/sh\n\
             out='{root}'/child-$$\n\
             mkdir -p \"$out\"\n\
             session=\n\
             prev=\n\
             for a in \"$@\"; do\n\
             \x20 printf '%s\\n' \"$a\" >> \"$out/argv\"\n\
             \x20 case \"$prev\" in --session-id|--resume) session=$a;; esac\n\
             \x20 prev=$a\n\
             done\n\
             if [ -z \"$session\" ]; then\n\
             \x20 session=stub-thread-$$\n\
             \x20 printf '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"%s\"}}\\n' \"$session\"\n\
             fi\n\
             env | sed 's/=.*//' | sort > \"$out/env-names\"\n\
             printf '%s\\n' \"$PWD\" > \"$out/cwd\"\n\
             printf '%s\\n' \"$0\" > \"$out/bin\"\n\
             printf '%s\\n' \"$BRO_HARNESS_PROVIDER\" > \"$out/provider\"\n\
             trap 'echo term > \"$out/signal\"; exit 143' TERM\n\
             trap 'echo int > \"$out/signal\"; exit 130' INT\n\
             : > \"$out/ready\"\n\
             while IFS= read -r line; do\n\
             \x20 printf '%s\\n' \"$line\" >> \"$out/stdin\"\n\
             \x20 case \"$line\" in *REPORT_SHELLS*)\n\
             \x20   printf '{{\"type\":\"harness_shell_sessions\",\"session_id\":\"%s\",\"seq\":5,\"sessions\":[{{\"id\":\"sh-1\",\"command\":\"sleep 600\",\"elapsed_ms\":1500,\"running\":true}}]}}\\n' \"$session\"\n\
             \x20   printf '{{\"type\":\"harness_shell_sessions\",\"session_id\":\"another-session\",\"seq\":6,\"sessions\":[]}}\\n';;\n\
             \x20 esac\n\
             \x20 case \"$line\" in *TURN_FAIL*)\n\
             \x20   printf '{{\"type\":\"result\",\"is_error\":true,\"result\":\"stub turn failed\",\"session_id\":\"%s\"}}\\n' \"$session\";;\n\
             \x20 esac\n\
             \x20 case \"$line\" in *FINISH*)\n\
             \x20   printf '{{\"type\":\"result\",\"is_error\":false,\"result\":\"stub ok\",\"session_id\":\"%s\"}}\\n' \"$session\"\n\
             \x20   exit 0;;\n\
             \x20 esac\n\
             done\n\
             : > \"$out/stdin-closed\"\n\
             sleep 300 &\n\
             wait $!\n",
            root = root.display()
        );
        std::fs::write(&path, script).expect("write stub");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }

    fn write_codex_stub(root: &Path) -> PathBuf {
        let path = root.join("recording-codex.py");
        let script = r#"#!/usr/bin/env python3
import json, os, pathlib, signal, sys, time
root = pathlib.Path(__file__).parent
out = root / ("child-" + str(os.getpid()))
out.mkdir()
for name, value in {"argv":"\n".join(sys.argv[1:]), "cwd":os.getcwd(), "bin":sys.argv[0], "provider":""}.items():
    (out / name).write_text(value)
def stop(*args):
    (out / "signal").write_text("term")
    sys.exit(0)
signal.signal(signal.SIGTERM, stop)
(out / "ready").touch()
thread = "stub-thread-" + str(os.getpid())
def emit(obj):
    print(json.dumps(obj), flush=True)
for line in sys.stdin:
    with (out / "stdin").open("a") as record:
        record.write(line)
    req = json.loads(line)
    method, params = req.get("method"), req.get("params", {})
    if "id" not in req: continue
    result = {}
    if method == "config/read": result = {"config":{"mcp_servers":{}}}
    if method in ("thread/start", "thread/resume"):
        thread = params.get("threadId", thread)
        result = {"thread":{"id":thread}, "model":params.get("model"), "cwd":os.getcwd()}
    if method == "turn/start": result = {"turn":{"id":"turn-1", "status":"inProgress"}}
    if method == "turn/steer": result = {"turnId":"turn-1"}
    emit({"id":req["id"], "result":result})
    if method == "turn/steer" and "TURN_FAIL" in line:
        emit({"method":"turn/completed", "params":{"threadId":thread, "turn":{"id":"turn-1", "status":"failed", "error":{"message":"stub turn failed"}}}})
(out / "stdin-closed").touch()
while True: time.sleep(1)
"#;
        std::fs::write(&path, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn child_dirs(root: &Path) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(root)
            .expect("read root")
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("child-"))
            })
            .collect();
        dirs.sort();
        dirs
    }

    fn call<T: serde::de::DeserializeOwned>(value: Value) -> Parameters<T> {
        Parameters(serde_json::from_value(value).expect("tool params"))
    }

    async fn await_file(path: &Path, needle: &str) -> String {
        let deadline = tokio::time::Instant::now() + super::smoke::DEADLINE;
        loop {
            if let Ok(text) = std::fs::read_to_string(path)
                && text.contains(needle)
            {
                return text;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "{} never contained {needle:?}",
                path.display()
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    fn parsed(result: &rmcp::model::CallToolResult) -> Value {
        let text = tool_text(result);
        serde_json::from_str(&text).unwrap_or_else(|_| panic!("tool result is not JSON: {text}"))
    }

    fn read(child: &Path, file: &str) -> String {
        std::fs::read_to_string(child.join(file)).unwrap_or_default()
    }

    fn argv(child: &Path) -> Vec<String> {
        read(child, "argv").lines().map(str::to_string).collect()
    }

    fn flag<'a>(argv: &'a [String], name: &str) -> Option<&'a str> {
        argv.iter()
            .position(|arg| arg == name)
            .and_then(|at| argv.get(at + 1))
            .map(String::as_str)
    }

    struct WaitClient;
    impl rmcp::ClientHandler for WaitClient {}

    /// One daemon's worth of dispatch plane: a real fleetd, the fleetd
    /// executor installed, the tool handlers over fresh test state, and an
    /// in-memory MCP session for the tools whose handlers need a live peer.
    struct Plane {
        client: rmcp::service::RunningService<rmcp::RoleClient, WaitClient>,
        root: PathBuf,
        state: Arc<crate::server::state::SharedState>,
        server: crate::server::BlackboxServer,
        seen: Vec<PathBuf>,
        _fleetd: FleetdProcess,
        _env: crate::util::TestEnvGuard,
        _tmp: tempfile::TempDir,
    }

    impl Plane {
async fn start_with_executor(kind: bbox_config::config::ExecutorKind) -> Self {
            Self::start_with_config(kind, false).await
        }

        /// A plane whose daemon and fleetd run with HOME and BRO_HOME inside
        /// the fixture, so the runner's global blackbox config (its global
        /// mcp.json filters among it) cannot reach dispatch resolution.
        async fn start_isolated() -> Self {
            Self::start_with_config(bbox_config::config::ExecutorKind::Fleetd, true).await
        }

        async fn start_with_config(
            kind: bbox_config::config::ExecutorKind,
            isolate_home: bool,
        ) -> Self {
            let tmp = tempfile::tempdir().expect("tempdir");
            let root = tmp.path().canonicalize().expect("canonical tempdir");
            let mut env = crate::util::TestEnvGuard::new();
            if isolate_home {
                env.set("HOME", &root.join("home"));
                env.set("BRO_HOME", &root.join("bro-home"));
            }
            let fleetd = FleetdProcess::start(&root.join("state")).await;
            let store_dir = root.join("bro");
            std::fs::create_dir_all(&store_dir).expect("store dir");
            let stub = write_recording_stub(&root);
            env.remove("BLACKBOX_MCP_URL");
            env.set("CLAUDE_BIN", &stub);
            let state = Arc::new(crate::server::state::SharedState::for_test(&store_dir));
            assert!(crate::orchestration::install_harness_executor_with_config(
                kind,
                fleetd.config(),
                store_dir,
                state.task_store.clone(),
                state.tail_tx.clone(),
                None,
            ));
            let server = crate::server::BlackboxServer::new(state.clone());
            let (server_io, client_io) = tokio::io::duplex(64 * 1024);
            let served = crate::server::BlackboxServer::new(state.clone());
            tokio::spawn(async move {
                use rmcp::ServiceExt;
                if let Ok(running) = served.serve(server_io).await {
                    let _ = running.waiting().await;
                }
            });
            let client = {
                use rmcp::ServiceExt;
                WaitClient.serve(client_io).await.expect("in-memory MCP session")
            };
            Self {
                client,
                root,
                state,
                server,
                seen: Vec::new(),
                _fleetd: fleetd,
                _env: env,
                _tmp: tmp,
            }
        }

        async fn start() -> Self {
            Self::start_with_executor(bbox_config::config::ExecutorKind::Fleetd).await
        }



        fn cwd(&self) -> String {
            self.root.to_string_lossy().into_owned()
        }

        /// The child spawned since the last call, once it has recorded its
        /// launch facts.
        async fn next_child(&mut self) -> PathBuf {
            let deadline = tokio::time::Instant::now() + super::smoke::DEADLINE;
            loop {
                let fresh = child_dirs(&self.root)
                    .into_iter()
                    .find(|dir| !self.seen.contains(dir) && dir.join("ready").exists());
                if let Some(dir) = fresh {
                    self.seen.push(dir.clone());
                    return dir;
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "no new child appeared under {}",
                    self.root.display()
                );
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        }

        async fn exec(&mut self, params: Value) -> (String, String, PathBuf) {
            let result = self.server.bro_exec(call(params)).await;
            let exec = parsed(&result);
            assert_ne!(result.is_error, Some(true), "{exec}");
            let child = self.next_child().await;
            (
                exec["taskId"].as_str().expect("taskId").to_string(),
                exec["sessionId"].as_str().expect("sessionId").to_string(),
                child,
            )
        }

        fn status(&self, task_id: &str) -> Value {
            parsed(&self.server.bro_status(call(json!({ "task_id": task_id }))))
        }

        fn steer(&self, task_id: &str, prompt: &str) -> Value {
            parsed(
                &self
                    .server
                    .bro_steer(call(json!({ "task_id": task_id, "prompt": prompt }))),
            )
        }

        async fn finish(&self, task_id: &str) -> Value {
            self.steer(task_id, "FINISH");
            self.wait(task_id, 20.0).await
        }

        /// Call a tool through the MCP session, so the handler runs with a
        /// real request context and progress peer.
        async fn tool(&self, name: &'static str, args: Value) -> Value {
            let response = self
                .client
                .call_tool(
                    rmcp::model::CallToolRequestParams::new(name)
                        .with_arguments(args.as_object().expect("object args").clone()),
                )
                .await
                .unwrap_or_else(|error| panic!("{name} failed: {error}"));
            let text = response.content[0].as_text().expect("text content").text.clone();
            assert_ne!(response.is_error, Some(true), "{name}: {text}");
            serde_json::from_str(&text).unwrap_or_else(|_| panic!("{name} is not JSON: {text}"))
        }

        async fn wait(&self, task_id: &str, timeout_seconds: f64) -> Value {
            self.tool(
                "bro_wait",
                json!({ "task_id": task_id, "timeout_seconds": timeout_seconds }),
            )
            .await
        }

        fn task_count(&self) -> usize {
            self.state.task_store.read().all_tasks().len()
        }

        fn origin(&self, task_id: &str) -> bro_core::Origin {
            self.state
                .task_store
                .read()
                .get(task_id)
                .expect("task")
                .inner
                .lock()
                .origin
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mcp_lifecycle_runs_end_to_end_against_a_fleetd_child() {
        let mut plane = Plane::start().await;
        let cwd = plane.cwd();
        let (task, session, child) = plane
            .exec(json!({ "prompt": "first turn", "provider": "glm", "cwd": cwd }))
            .await;
        await_file(&child.join("stdin"), "first turn").await;
        assert_eq!(plane.status(&task)["status"], "running");

        assert_eq!(plane.steer(&task, "steer marker")["status"], "steered");
        await_file(&child.join("stdin"), "steer marker").await;

        let waited = plane.finish(&task).await;
        assert_eq!(waited["status"], "completed", "{waited}");
        assert!(waited.to_string().contains("stub ok"), "{waited}");

        // A finished session resumes as a new task on a new child that is told
        // which session to continue.
        let resumed = plane
            .server
            .bro_resume(call(json!({
                "prompt": "second turn",
                "session_id": session,
                "provider": "glm",
                "cwd": cwd,
            })))
            .await;
        let resumed = parsed(&resumed);
        let resumed_task = resumed["taskId"].as_str().expect("resume taskId").to_string();
        assert_ne!(resumed_task, task);
        let second = plane.next_child().await;
        await_file(&second.join("stdin"), "second turn").await;
        let second_argv = argv(&second);
        assert_eq!(flag(&second_argv, "--resume"), Some(session.as_str()), "{second_argv:?}");

        let cancelled = parsed(&plane.server.bro_cancel(call(json!({ "task_id": resumed_task }))));
        assert_eq!(cancelled["status"], "cancelled", "{cancelled}");
        await_file(&second.join("signal"), "term").await;
        assert_eq!(plane.status(&resumed_task)["status"], "cancelled");
        assert_eq!(plane.task_count(), 2);
    }

    /// A claude-lane dispatch through fleetd launches the CLI's own argv,
    /// gets its session log written by fleetd, and has its stdin closed by
    /// the daemon once the turn's result arrives.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_claude_lane_child_gets_cli_argv_a_fleetd_written_log_and_end_of_input() {
        let mut plane = Plane::start().await;
        let cwd = plane.cwd();
        let (task, session, child) = plane
            .exec(json!({ "prompt": "first turn", "provider": "glm", "cwd": cwd }))
            .await;
        await_file(&child.join("stdin"), "first turn").await;

        let launched = argv(&child);
        assert_eq!(flag(&launched, "--input-format"), Some("stream-json"), "{launched:?}");
        assert!(launched.iter().any(|a| a == "--replay-user-messages"));
        for harness_only in ["--exit-when-idle", "--daemon-worker", "--cwd", "--shell-env"] {
            assert!(!launched.iter().any(|a| a == harness_only), "{launched:?}");
        }
        assert_eq!(read(&child, "cwd").trim(), cwd);
        assert_eq!(read(&child, "provider").trim(), "");
        assert!(
            read(&child, "env-names")
                .lines()
                .any(|name| name == "CLAUDE_CONFIG_DIR")
        );

        // The error result ends the turn: the daemon closes stdin, and fleetd
        // has written the result into the session log at the pinned path.
        assert_eq!(plane.steer(&task, "TURN_FAIL")["status"], "steered");
        await_file(&child.join("stdin-closed"), "").await;
        let log = plane
            .root
            .join("bro")
            .join("harness-sessions")
            .join(format!("{session}.events.jsonl"));
        await_file(&log, "stub turn failed").await;
        let record: Value = serde_json::from_str(
            std::fs::read_to_string(&log)
                .unwrap()
                .lines()
                .last()
                .expect("one record"),
        )
        .unwrap();
        assert_eq!(record["event"]["type"], "result");
        assert!(record["ts"].as_str().unwrap().ends_with('Z'));

        // The lane is gone, so a late steer is refused rather than queued.
        let late = plane
            .server
            .bro_steer(call(json!({ "task_id": task, "prompt": "too late" })));
        assert_eq!(late.is_error, Some(true), "{}", tool_text(&late));
        assert!(
            tool_text(&late).contains("no live harness control channel"),
            "{}",
            tool_text(&late)
        );

        // The stub stays alive after end of input; cancel finishes it.
        let cancelled = parsed(&plane.server.bro_cancel(call(json!({ "task_id": task }))));
        assert_eq!(cancelled["status"], "cancelled", "{cancelled}");
        await_file(&child.join("signal"), "term").await;
    }

    /// Native Codex RPC dispatch through a real fleetd, including identity,
    /// durable replay, EOF delivery, and resume by the provider thread id.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_codex_lane_child_adopts_its_thread_id_and_resumes_by_it() {
        codex_roundtrip(bbox_config::config::ExecutorKind::Fleetd).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn local_codex_adapter_adopts_its_thread_id_and_resumes_by_it() {
        codex_roundtrip(bbox_config::config::ExecutorKind::Local).await;
    }

    async fn codex_roundtrip(kind: bbox_config::config::ExecutorKind) {
        let mut plane = Plane::start_with_executor(kind).await;
        let codex_stub = write_codex_stub(&plane.root);
        plane._env.set("CODEX_BIN", &codex_stub);
        let cwd = plane.cwd();
        let (task, _, child) = plane
            .exec(json!({ "prompt": "first turn", "provider": "codex", "cwd": cwd }))
            .await;
        await_file(&child.join("stdin"), "first turn").await;

        let launched = argv(&child);
        assert_eq!(launched, ["app-server"]);
        let requests: Vec<Value> = read(&child, "stdin").lines().map(|s| serde_json::from_str(s).unwrap()).collect();
        let start = requests.iter().find(|v| v["method"] == "thread/start").unwrap();
        assert_eq!(start["params"]["approvalPolicy"], "never");
        assert!(start["params"]["model"].is_string());
        assert_eq!(read(&child, "bin").trim(), codex_stub.to_string_lossy());
        assert_eq!(read(&child, "cwd").trim(), cwd);
        assert_eq!(read(&child, "provider").trim(), "");

        // The task adopts the thread the worker reported.
        let pid = child
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix("child-"))
            .expect("child pid")
            .to_string();
        let thread = format!("stub-thread-{pid}");
        let deadline = tokio::time::Instant::now() + super::smoke::DEADLINE;
        loop {
            let status = plane.status(&task);
            if status["sessionId"] == thread.as_str() {
                assert_eq!(status["status"], "running", "{status}");
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the task never adopted {thread}: {status}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }

        // The result ends the turn: stdin closes, and fleetd has logged the
        // whole stream at the path pinned at spawn, named by the task.
        assert_eq!(plane.steer(&task, "TURN_FAIL")["status"], "steered");
        await_file(&child.join("stdin-closed"), "").await;
        let log = plane
            .root
            .join("bro")
            .join("harness-sessions")
            .join(format!("{task}.events.jsonl"));
        let logged = await_file(&log, "stub turn failed").await;
        assert!(logged.contains(&format!("\"session_id\":\"{thread}\"")), "{logged}");
        assert!(logged.contains("\"subtype\":\"init\""), "{logged}");

        let cancelled = parsed(&plane.server.bro_cancel(call(json!({ "task_id": task }))));
        assert_eq!(cancelled["status"], "cancelled", "{cancelled}");
        await_file(&child.join("signal"), "term").await;

        // Resume names the thread; the resumed task records it from the start.
        let resumed = parsed(
            &plane
                .server
                .bro_resume(call(json!({
                    "prompt": "second turn",
                    "session_id": thread,
                    "provider": "codex",
                    "cwd": cwd,
                })))
                .await,
        );
        let resumed_task = resumed["taskId"].as_str().expect("resume taskId").to_string();
        let second = plane.next_child().await;
        await_file(&second.join("stdin"), "second turn").await;
        let second_argv = argv(&second);
        assert_eq!(second_argv, ["app-server"]);
        let requests: Vec<Value> = read(&second, "stdin").lines().map(|s| serde_json::from_str(s).unwrap()).collect();
        let resume = requests.iter().find(|v| v["method"] == "thread/resume").unwrap();
        assert_eq!(resume["params"]["threadId"], thread);
        let records: Vec<Value> = logged.lines().map(|s| serde_json::from_str(s).unwrap()).collect();
        assert!(records.windows(2).all(|w| w[0]["event"]["seq"].as_u64().unwrap() < w[1]["event"]["seq"].as_u64().unwrap()));
        let canonical = log.with_file_name(format!("{thread}.events.jsonl"));
        assert!(canonical.exists());
        assert_eq!(read(&second, "bin").trim(), codex_stub.to_string_lossy());
        assert_eq!(plane.status(&resumed_task)["sessionId"], thread.as_str());
        let cancelled = parsed(&plane.server.bro_cancel(call(json!({ "task_id": resumed_task }))));
        assert_eq!(cancelled["status"], "cancelled", "{cancelled}");
        await_file(&second.join("signal"), "term").await;
    }



    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn control_routes_run_the_lifecycle_as_control_tasks() {
        use axum::extract::State as AxumState;
        let mut plane = Plane::start().await;
        let cwd = plane.cwd();
        let exec = crate::server::control::control_exec_handler(
            AxumState(plane.state.clone()),
            axum::Json(
                serde_json::from_value(json!({
                    "prompt": "control turn",
                    "provider": "glm",
                    "cwd": cwd,

                }))
                .expect("exec body"),
            ),
        )
        .await;
        let exec = parsed(&exec.0);
        let task = exec["taskId"].as_str().expect("taskId").to_string();
        let session = exec["sessionId"].as_str().expect("sessionId").to_string();
        let child = plane.next_child().await;
        await_file(&child.join("stdin"), "control turn").await;
        assert_eq!(plane.origin(&task), bro_core::Origin::Control);



        let waited = plane.finish(&task).await;
        assert_eq!(waited["status"], "completed", "{waited}");

        let resumed = crate::server::control::control_resume_handler(
            AxumState(plane.state.clone()),
            axum::Json(
                serde_json::from_value(json!({
                    "prompt": "control follow-up",
                    "session_id": session,
                    "provider": "glm",
                    "cwd": cwd,
                }))
                .expect("resume body"),
            ),
        )
        .await;
        let resumed = parsed(&resumed.0);
        let resumed_task = resumed["taskId"].as_str().expect("resume taskId").to_string();
        let second = plane.next_child().await;
        await_file(&second.join("stdin"), "control follow-up").await;
        assert_eq!(plane.origin(&resumed_task), bro_core::Origin::Control);
        let waited = plane.finish(&resumed_task).await;
        assert_eq!(waited["status"], "completed", "{waited}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn waiting_on_tasks_launches_nothing() {
        let mut plane = Plane::start().await;
        let cwd = plane.cwd();
        let (done, _, _) = plane
            .exec(json!({ "prompt": "finishes", "provider": "glm", "cwd": cwd }))
            .await;
        assert_eq!(plane.finish(&done).await["status"], "completed");
        let (running, _, child) = plane
            .exec(json!({ "prompt": "keeps running", "provider": "glm", "cwd": cwd }))
            .await;
        await_file(&child.join("stdin"), "keeps running").await;

        let finished = plane.wait(&done, 5.0).await;
        assert_eq!(finished["status"], "completed", "{finished}");
        let all = plane
            .tool("bro_when_all", json!({ "task_ids": [done], "timeout_seconds": 5.0 }))
            .await;
        assert_eq!(all["all_completed"], true, "{all}");
        let any = plane
            .tool(
                "bro_when_any",
                json!({ "task_ids": [done, running], "timeout_seconds": 5.0 }),
            )
            .await;
        assert!(any.to_string().contains(done.as_str()), "{any}");
        let still = plane.wait(&running, 0.2).await;
        assert_eq!(still["status"], "running", "{still}");
        assert!(still.get("timed_out").is_some(), "{still}");
        let pending = plane
            .tool("bro_when_all", json!({ "task_ids": [done, running], "timeout_seconds": 0.2 }))
            .await;
        assert_eq!(pending["all_completed"], false, "{pending}");

        assert_eq!(plane.task_count(), 2, "waiting created a task");
        assert_eq!(child_dirs(&plane.root).len(), 2, "waiting spawned a child");
        assert_eq!(plane.status(&running)["status"], "running");
        plane.server.bro_cancel(call(json!({ "task_id": running })));
        await_file(&child.join("signal"), "term").await;
    }

    async fn create_brofile(plane: &Plane, body: Value) {
        let created = plane.server.bro_brofile(call(body)).await;
        assert_ne!(created.is_error, Some(true), "{}", tool_text(&created));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resume_of_a_named_session_restores_the_brofile_lanes_and_relabels() {
        let mut plane = Plane::start_isolated().await;
        let cwd = plane.cwd();
        create_brofile(
            &plane,
            json!({
                "action": "set_account",
                "name": "acceptance-account",
                "env": { "ACCEPTANCE_ACCOUNT_MARKER": "synthetic" },
            }),
        )
        .await;
        create_brofile(
            &plane,
            json!({
                "action": "create",
                "name": "acceptance-bro",
                "provider": "glm",
                "account": "acceptance-account",
                "model": "glm-5.3-flash",
                "effort": "high",
                "lens": "acceptance persona",
                "disallow_tools": ["glob"],
            }),
        )
        .await;
        let (task, session, child) = plane
            .exec(json!({ "prompt": "named first turn", "bro": "acceptance-bro", "cwd": cwd }))
            .await;
        await_file(&child.join("stdin"), "named first turn").await;
        assert_eq!(plane.finish(&task).await["status"], "completed");

        // A resume of the named session restores the brofile in full and
        // says so.
        let resumed = plane
            .server
            .bro_resume(call(json!({
                "prompt": "named second turn",
                "session_id": session,
                "provider": "glm",
                "cwd": cwd,
            })))
            .await;
        let resumed = parsed(&resumed);
        assert_eq!(
            resumed["brofile"],
            json!({ "name": "acceptance-bro", "restored": true }),
            "{resumed}"
        );
        let resumed_task = resumed["taskId"].as_str().expect("resume taskId").to_string();
        let second = plane.next_child().await;
        await_file(&second.join("stdin"), "named second turn").await;
        assert_brofile_lanes(&second, &session);
        let waited = plane.finish(&resumed_task).await;
        assert_eq!(waited["status"], "completed", "{waited}");
        // The resumed task still names the brofile, so waits and roster rows
        // report it.
        assert_eq!(waited["bro"], "acceptance-bro", "{waited}");
        let roster = {
            let store = plane.state.task_store.read();
            let task = store.get(&resumed_task).expect("resumed task");
            crate::orchestration::roster_summary_from_task(&task)
        };
        assert_eq!(roster.label.as_deref(), Some("acceptance-bro"));
        assert_eq!(roster.name.as_deref(), Some("acceptance-bro"));

        // The first resume restamped the label, so a second resume of the
        // same session restores the brofile again.
        let again = plane
            .server
            .bro_resume(call(json!({
                "prompt": "named third turn",
                "session_id": session,
                "provider": "glm",
                "cwd": cwd,
            })))
            .await;
        let again = parsed(&again);
        assert_eq!(
            again["brofile"],
            json!({ "name": "acceptance-bro", "restored": true }),
            "{again}"
        );
        let again_task = again["taskId"].as_str().expect("resume taskId").to_string();
        let third = plane.next_child().await;
        await_file(&third.join("stdin"), "named third turn").await;
        assert_brofile_lanes(&third, &session);
        assert_eq!(plane.finish(&again_task).await["bro"], "acceptance-bro");
    }

    /// A resumed child of a named session carries everything the brofile
    /// owns: the account environment, model, effort, persona with the scope
    /// block, and the deny filter.
    fn assert_brofile_lanes(child: &Path, session: &str) {
        let argv = argv(child);
        assert_eq!(flag(&argv, "--resume"), Some(session), "{argv:?}");
        assert_eq!(flag(&argv, "--model"), Some("glm-5.3-flash"), "{argv:?}");
        assert_eq!(flag(&argv, "--effort"), Some("high"), "{argv:?}");
        // Persona and scope share --append-system-prompt: the persona leads,
        // and the scope block below it names the brofile.
        assert_eq!(
            flag(&argv, "--append-system-prompt"),
            Some("acceptance persona"),
            "{argv:?}"
        );
        let raw = read(child, "argv");
        assert!(raw.contains("bro: acceptance-bro"), "{raw}");
        let denied: Vec<&str> = flag(&argv, "--disallowedTools")
            .expect("deny")
            .split(',')
            .collect();
        assert!(denied.contains(&"glob"), "{denied:?}");
        assert!(
            read(child, "env-names")
                .lines()
                .any(|name| name == "ACCEPTANCE_ACCOUNT_MARKER")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resume_of_a_session_whose_brofile_is_gone_runs_bare_and_says_so() {
        let mut plane = Plane::start().await;
        let cwd = plane.cwd();
        create_brofile(
            &plane,
            json!({
                "action": "create",
                "name": "short-lived-bro",
                "provider": "glm",
                "model": "glm-5.3-flash",
                "disallow_tools": ["glob"],
            }),
        )
        .await;
        let (task, session, child) = plane
            .exec(json!({ "prompt": "named turn", "bro": "short-lived-bro", "cwd": cwd }))
            .await;
        await_file(&child.join("stdin"), "named turn").await;
        assert_eq!(plane.finish(&task).await["status"], "completed");
        create_brofile(&plane, json!({ "action": "delete", "name": "short-lived-bro" })).await;

        let resumed = plane
            .server
            .bro_resume(call(json!({
                "prompt": "after the brofile was deleted",
                "session_id": session,
                "provider": "glm",
                "cwd": cwd,
            })))
            .await;
        let resumed = parsed(&resumed);
        assert_eq!(resumed["brofile"]["name"], "short-lived-bro", "{resumed}");
        assert_eq!(resumed["brofile"]["restored"], false, "{resumed}");
        let notice = resumed["brofile"]["notice"].as_str().expect("notice");
        assert!(notice.contains("Unknown brofile: short-lived-bro"), "{notice}");
        assert!(notice.contains("runs without that brofile's"), "{notice}");

        let resumed_task = resumed["taskId"].as_str().expect("resume taskId").to_string();
        let second = plane.next_child().await;
        await_file(&second.join("stdin"), "after the brofile was deleted").await;
        let argv = argv(&second);
        let denied: Vec<&str> = flag(&argv, "--disallowedTools").expect("deny").split(',').collect();
        assert!(!denied.contains(&"glob"), "{denied:?}");
        assert_eq!(plane.finish(&resumed_task).await["status"], "completed");

        // A session that was never dispatched by name reports no brofile.
        let (plain, plain_session, plain_child) = plane
            .exec(json!({ "prompt": "plain turn", "provider": "glm", "cwd": cwd }))
            .await;
        await_file(&plain_child.join("stdin"), "plain turn").await;
        assert_eq!(plane.finish(&plain).await["status"], "completed");
        let resumed = plane
            .server
            .bro_resume(call(json!({
                "prompt": "plain follow-up",
                "session_id": plain_session,
                "provider": "glm",
                "cwd": cwd,
            })))
            .await;
        let resumed = parsed(&resumed);
        assert!(resumed.get("brofile").is_none(), "{resumed}");
        let resumed_task = resumed["taskId"].as_str().expect("resume taskId").to_string();
        let child = plane.next_child().await;
        await_file(&child.join("stdin"), "plain follow-up").await;
        assert_eq!(plane.finish(&resumed_task).await["status"], "completed");
    }








}
