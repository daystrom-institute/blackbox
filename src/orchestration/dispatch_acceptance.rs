
/// Acceptance tests for the dispatch plane: the tool handlers drive a real
/// `fleetd` process, which spawns a stub child standing in for `bro-harness`.
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

    /// A stub standing in for `bro-harness`. It records what it was started
    /// with under `<root>/child-<pid>/` (argv, cwd, the provider it was told to
    /// be, and environment variable names, never values), appends every stdin line it receives,
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
             env | sed 's/=.*//' | sort > \"$out/env-names\"\n\
             printf '%s\\n' \"$PWD\" > \"$out/cwd\"\n\
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
        async fn start() -> Self {
            let tmp = tempfile::tempdir().expect("tempdir");
            let root = tmp.path().canonicalize().expect("canonical tempdir");
            let fleetd = FleetdProcess::start(&root.join("state")).await;
            let store_dir = root.join("bro");
            std::fs::create_dir_all(&store_dir).expect("store dir");
            let stub = write_recording_stub(&root);
            let mut env = crate::util::TestEnvGuard::new();
            env.remove("BLACKBOX_MCP_URL");
            env.set("BRO_HARNESS_BIN", &stub);
            let state = Arc::new(crate::server::state::SharedState::for_test(&store_dir));
            assert!(crate::orchestration::install_harness_executor_with_config(
                bbox_config::config::ExecutorKind::Fleetd,
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn control_routes_run_the_lifecycle_as_cockpit_tasks() {
        use axum::extract::State as AxumState;
        let mut plane = Plane::start().await;
        let cwd = plane.cwd();
        let exec = crate::server::control::control_exec_handler(
            AxumState(plane.state.clone()),
            axum::Json(
                serde_json::from_value(json!({
                    "prompt": "cockpit turn",
                    "provider": "glm",
                    "cwd": cwd,
                    "tool_defaults": { "default:shell_run.timeout_ms": 30000 },
                }))
                .expect("exec body"),
            ),
        )
        .await;
        let exec = parsed(&exec.0);
        let task = exec["taskId"].as_str().expect("taskId").to_string();
        let session = exec["sessionId"].as_str().expect("sessionId").to_string();
        let child = plane.next_child().await;
        await_file(&child.join("stdin"), "cockpit turn").await;
        assert_eq!(plane.origin(&task), bro_core::Origin::Cockpit);
        let context: Value = serde_json::from_str(
            flag(&argv(&child), "--additional-context").expect("additional context"),
        )
        .expect("context json");
        assert_eq!(context["default:shell_run.timeout_ms"], 30000);

        let waited = plane.finish(&task).await;
        assert_eq!(waited["status"], "completed", "{waited}");

        let resumed = crate::server::control::control_resume_handler(
            AxumState(plane.state.clone()),
            axum::Json(
                serde_json::from_value(json!({
                    "prompt": "cockpit follow-up",
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
        await_file(&second.join("stdin"), "cockpit follow-up").await;
        assert_eq!(plane.origin(&resumed_task), bro_core::Origin::Cockpit);
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
    async fn the_child_receives_its_routing_and_resume_keeps_the_brofile_lanes() {
        let mut plane = Plane::start().await;
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
                "code_mode": "only",
                "tool_defaults": {
                    "default:file_read.max_lines": 111,
                    "default:shell_run.timeout_ms": 1000,
                },
                "disallow_tools": ["glob"],
            }),
        )
        .await;

        let (task, session, child) = plane
            .exec(json!({
                "prompt": "routed turn",
                "bro": "acceptance-bro",
                "cwd": cwd,
                "tool_defaults": {
                    "default:shell_run.timeout_ms": 30000,
                    "default:content_search.max_results": 7,
                },
                "disallow_tools": ["web_fetch"],
            }))
            .await;
        await_file(&child.join("stdin"), "routed turn").await;
        let first = argv(&child);
        assert_eq!(flag(&first, "--cwd"), Some(cwd.as_str()), "{first:?}");
        assert_eq!(read(&child, "cwd").trim(), cwd);
        assert_eq!(flag(&first, "--model"), Some("glm-5.3-flash"), "{first:?}");
        assert_eq!(flag(&first, "--effort"), Some("high"), "{first:?}");
        assert_eq!(flag(&first, "--code-mode"), Some("only"), "{first:?}");
        let context: Value =
            serde_json::from_str(flag(&first, "--additional-context").expect("context"))
                .expect("context json");
        // ambient < brofile < per-dispatch, most specific wins on conflict.
        assert_eq!(context["default:mcp.bbox_gap.project"], cwd, "{context}");
        assert_eq!(context["default:file_read.max_lines"], 111, "{context}");
        assert_eq!(context["default:shell_run.timeout_ms"], 30000, "{context}");
        assert_eq!(context["default:content_search.max_results"], 7, "{context}");
        let denied: Vec<&str> = flag(&first, "--deny-tools").expect("deny").split(',').collect();
        assert!(denied.contains(&"glob"), "{denied:?}");
        assert!(denied.contains(&"web_fetch"), "{denied:?}");
        // The brofile's provider and account reach the child. Only the name
        // of the account's variable is recorded, never its value.
        assert_eq!(read(&child, "provider").trim(), "glm");
        assert!(
            read(&child, "env-names")
                .lines()
                .any(|name| name == "ACCEPTANCE_ACCOUNT_MARKER")
        );
        assert_eq!(plane.finish(&task).await["status"], "completed");

        // A raw provider dispatch names a different provider and carries no
        // account environment.
        let (plain, _, plain_child) = plane
            .exec(json!({ "prompt": "plain turn", "provider": "deepseek", "cwd": cwd }))
            .await;
        await_file(&plain_child.join("stdin"), "plain turn").await;
        assert_eq!(read(&plain_child, "provider").trim(), "deepseek");
        assert!(
            !read(&plain_child, "env-names")
                .lines()
                .any(|name| name == "ACCEPTANCE_ACCOUNT_MARKER")
        );
        assert_eq!(plane.finish(&plain).await["status"], "completed");

        // Resume restores what the brofile owns. Per-dispatch defaults and
        // filters applied to the first invocation only.
        let resumed = plane
            .server
            .bro_resume(call(json!({
                "prompt": "routed follow-up",
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
        let second_child = plane.next_child().await;
        await_file(&second_child.join("stdin"), "routed follow-up").await;
        assert_brofile_lanes(&second_child, &session, &cwd);
        let second = argv(&second_child);
        let context: Value =
            serde_json::from_str(flag(&second, "--additional-context").expect("context"))
                .expect("context json");
        assert_eq!(context["default:shell_run.timeout_ms"], 1000, "{context}");
        assert!(context.get("default:content_search.max_results").is_none(), "{context}");
        let denied: Vec<&str> = flag(&second, "--deny-tools").expect("deny").split(',').collect();
        assert!(!denied.contains(&"web_fetch"), "{denied:?}");
        assert_eq!(plane.finish(&resumed_task).await["status"], "completed");

        // The resumed task names the brofile too, so a later resume of the
        // same session restores it again.
        let again = plane
            .server
            .bro_resume(call(json!({
                "prompt": "third turn",
                "session_id": session,
                "provider": "glm",
                "cwd": cwd,
            })))
            .await;
        let again = parsed(&again);
        assert_eq!(again["brofile"]["restored"], true, "{again}");
        let resumed_task = again["taskId"].as_str().expect("resume taskId").to_string();
        let third_child = plane.next_child().await;
        await_file(&third_child.join("stdin"), "third turn").await;
        assert_brofile_lanes(&third_child, &session, &cwd);
        assert_eq!(plane.finish(&resumed_task).await["status"], "completed");
    }

    /// A resumed child of the routed session carries everything the brofile
    /// owns: model, effort, persona, deny filter, tool defaults and the
    /// account environment.
    fn assert_brofile_lanes(child: &Path, session: &str, cwd: &str) {
        let argv = argv(child);
        assert_eq!(flag(&argv, "--resume"), Some(session), "{argv:?}");
        assert_eq!(flag(&argv, "--cwd"), Some(cwd), "{argv:?}");
        assert_eq!(flag(&argv, "--model"), Some("glm-5.3-flash"), "{argv:?}");
        assert_eq!(flag(&argv, "--effort"), Some("high"), "{argv:?}");
        // Code mode and service tier stay with the session: the harness saved
        // them, and passing either here would override that.
        assert_eq!(flag(&argv, "--code-mode"), None, "{argv:?}");
        assert_eq!(flag(&argv, "--service-tier"), None, "{argv:?}");
        let dispatch: Value =
            serde_json::from_str(flag(&argv, "--dispatch-context").expect("dispatch context"))
                .expect("dispatch context json");
        assert_eq!(dispatch["scope"]["bro"], "acceptance-bro", "{dispatch}");
        let context: Value =
            serde_json::from_str(flag(&argv, "--additional-context").expect("context"))
                .expect("context json");
        assert_eq!(context["default:mcp.bbox_gap.project"], cwd, "{context}");
        assert_eq!(context["default:file_read.max_lines"], 111, "{context}");
        let denied: Vec<&str> = flag(&argv, "--deny-tools").expect("deny").split(',').collect();
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
        let denied: Vec<&str> = flag(&argv, "--deny-tools").expect("deny").split(',').collect();
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_absolute_harness_binary_that_is_missing_fails_the_dispatch_by_name() {
        let plane = Plane::start().await;
        let cwd = plane.cwd();
        let missing = plane.root.join("no-such-harness");
        // SAFETY: the plane holds the process-wide test environment guard for
        // its whole lifetime, so no other test reads this variable meanwhile.
        unsafe { std::env::set_var("BRO_HARNESS_BIN", &missing) };

        let result = plane
            .server
            .bro_exec(call(json!({ "prompt": "never runs", "provider": "glm", "cwd": cwd })))
            .await;
        let exec = parsed(&result);
        let task = exec["taskId"].as_str().expect("taskId").to_string();
        let waited = plane.wait(&task, 20.0).await;
        assert_eq!(waited["status"], "failed", "{waited}");
        let text = waited.to_string();
        assert!(text.contains("harness_bin_unavailable"), "{text}");
        assert!(text.contains("no-such-harness"), "{text}");
        // Nothing ran in its place.
        assert!(child_dirs(&plane.root).is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_shell_session_report_reaches_supervision_through_fleetd() {
        let mut plane = Plane::start().await;
        let cwd = plane.cwd();
        let (task, _, child) = plane
            .exec(json!({ "prompt": "first turn", "provider": "glm", "cwd": cwd }))
            .await;
        await_file(&child.join("stdin"), "first turn").await;
        // The stub answers with one report for its own session and one that
        // names a different session.
        plane.steer(&task, "REPORT_SHELLS");
        let held = {
            let deadline = tokio::time::Instant::now() + super::smoke::DEADLINE;
            loop {
                let observed = plane
                    .state
                    .task_store
                    .read()
                    .get(&task)
                    .expect("task")
                    .inner
                    .lock()
                    .supervision
                    .shell_sessions
                    .clone();
                if let Some(observed) = observed {
                    break observed;
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the report never reached supervision"
                );
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        };
        assert_eq!(held.seq, 5);
        assert!(held.observed_this_run);
        assert_eq!(held.sessions.len(), 1);
        assert_eq!(held.sessions[0].command, "sleep 600");
        assert!(held.sessions[0].running);

        // The report naming another session is never observed: it carried a
        // higher sequence and an empty set and would have replaced the real
        // one. An envelope from a different session also fails the task, as
        // any such envelope does.
        let failed = plane.wait(&task, 20.0).await;
        assert_eq!(failed["status"], "failed", "{failed}");
        assert!(failed.to_string().contains("session fork detected"), "{failed}");
        let after = plane
            .state
            .task_store
            .read()
            .get(&task)
            .expect("task")
            .inner
            .lock()
            .supervision
            .shell_sessions
            .clone()
            .expect("observation kept");
        assert_eq!((after.seq, after.sessions.len()), (5, 1));
    }
}
