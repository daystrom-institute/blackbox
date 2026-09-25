
/// End-to-end smoke against a REAL `fleetd` process over a real Unix socket.
///
/// This is the test the slice exists for: a worker spawned through fleetd must
/// survive the daemon that started it. The shape is deliberately not a mock.
/// It starts the actual `fleetd` binary, drives one dispatch end to end
/// (spawn, ingest, terminal), then throws the executor away and builds a new
/// one, which is what a `blackboxd` rebuild and `launchctl kickstart` looks
/// like from fleetd's side. The second executor has to find the still-running
/// child by itself.
///
/// The child is a stub shell script, not `bro-harness`. What is under test is
/// the socket contract, the supervision lifecycle, and the cursor; a stub
/// emitting known envelope lines with known `seq` values makes every assertion
/// exact instead of dependent on a provider round trip. It appends those same
/// lines to the spec's `event_log_path`, because that durable log (not an
/// in-memory buffer) is what fleetd replays from.
///
/// Fixture setup is blocking `std::fs` before the system under test runs. The
/// `disallowed_methods` deny targets blocking I/O on tokio worker threads in
/// serving paths; test fixtures are not that. Same carve-out
/// `crates/fleetd/tests/connection.rs` takes.
#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod smoke {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;

    use bro_protocol::{SecretEnv, WorkerSpawnSpec};

    use super::{FleetdConfig, FleetdExecutor};
    use crate::orchestration::executor::HarnessExecutor;

    /// Every wait is bounded so a broken assumption fails the test instead of
    /// hanging the suite.
    const DEADLINE: Duration = Duration::from_secs(20);

    /// Locate the `fleetd` binary cargo built alongside this test binary.
    ///
    /// `env!("CARGO_BIN_EXE_...")` only covers the current package's own bins, and
    /// fleetd is a separate workspace crate, so this walks out of `deps/`. A
    /// workspace test run builds every workspace binary, so a miss means something
    /// is genuinely wrong rather than "not built yet"; failing loudly beats
    /// silently skipping the one test that proves the slice works.
    fn fleetd_binary() -> PathBuf {
        let test_binary = std::env::current_exe().expect("current exe");
        let profile_dir = test_binary
            .parent()
            .and_then(|deps| deps.parent())
            .expect("target/<profile> above deps/");
        let candidate = profile_dir.join("fleetd");
        assert!(
            candidate.is_file(),
            "fleetd binary not found at {}. Run this through `cargo nextest run --workspace`, \
             which builds every workspace binary.",
            candidate.display()
        );
        candidate
    }

    /// A running `fleetd` child plus the paths it derived from its state dir.
    struct FleetdProcess {
        child: std::process::Child,
        state_dir: PathBuf,
        tcp_address: Option<std::net::SocketAddr>,
    }

    impl FleetdProcess {
        async fn start(state_dir: &Path) -> Self {
            std::fs::create_dir_all(state_dir).expect("state dir");
            let socket = state_dir.join("fleetd.sock");
            let child = std::process::Command::new(fleetd_binary())
                .arg("--state-dir")
                .arg(state_dir)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("start fleetd");

            let deadline = tokio::time::Instant::now() + DEADLINE;
            while tokio::time::Instant::now() < deadline {
                if tokio::net::UnixStream::connect(&socket).await.is_ok() {
                    return Self {
                        child,
                        state_dir: state_dir.to_path_buf(),
                        tcp_address: None,
                    };
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            panic!("fleetd did not bind {} within the deadline", socket.display());
        }

        async fn start_tcp(state_dir: &Path) -> Self {
            std::fs::create_dir_all(state_dir).expect("state dir");
            let reservation = std::net::TcpListener::bind("127.0.0.1:0")
                .expect("reserve loopback port");
            let tcp_address = reservation.local_addr().expect("reserved address");
            drop(reservation);
            let child = std::process::Command::new(fleetd_binary())
                .arg("--state-dir")
                .arg(state_dir)
                .arg("--listen-tcp")
                .arg(tcp_address.to_string())
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("start TCP fleetd");

            let deadline = tokio::time::Instant::now() + DEADLINE;
            while tokio::time::Instant::now() < deadline {
                if tokio::net::TcpStream::connect(tcp_address).await.is_ok() {
                    return Self {
                        child,
                        state_dir: state_dir.to_path_buf(),
                        tcp_address: Some(tcp_address),
                    };
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            panic!("fleetd did not bind tcp://{tcp_address} within the deadline");
        }

        fn config(&self) -> FleetdConfig {
            let mut config = FleetdConfig::in_state_dir(&self.state_dir);
            // Never let an ambient BLACKBOX_FLEETD_BIN from the developer's shell
            // point this at a different supervisor, and never let the executor
            // auto-start a second one: this test owns the fleetd it talks to.
            config.binary = Some(fleetd_binary());
            config
        }

        fn remote_config(&self) -> FleetdConfig {
            let address = self.tcp_address.expect("TCP fleetd process");
            FleetdConfig::resolve(
                &self.state_dir,
                Some(&format!("tcp://{address}")),
                Some(&self.state_dir.join("fleetd.token")),
                Some(&self.state_dir),
                Some(&self.state_dir),
            )
            .expect("valid remote config")
        }
    }

    impl Drop for FleetdProcess {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// A stub standing in for `bro-harness`: emit `lines` on stdout AND append them
    /// to the durable event log (which is what the real harness does and what
    /// fleetd replays from), then either exit or idle.
    fn write_stub(root: &Path, name: &str, lines: &[&str], log: &Path, then: &str) -> String {
        let path = root.join(name);
        let mut script = String::from("#!/bin/sh\n");
        for line in lines {
            script.push_str(&format!(
                "printf '%s\\n' '{line}'\nprintf '%s\\n' '{line}' >> '{}'\n",
                log.display()
            ));
        }
        script.push_str(then);
        std::fs::write(&path, script).expect("write stub");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        path.to_string_lossy().into_owned()
    }

    fn spec_for(bin: &str, session_id: &str, task_id: &str, log: &Path, home: &Path) -> WorkerSpawnSpec {
        WorkerSpawnSpec {
            task_id: task_id.to_string(),
            session_id: session_id.to_string(),
            workspace_id: None,
            workspace_scope: None,
            provider: bro_core::Provider::Glm,
            bin_override: Some(bin.to_string()),
            argv: Vec::new(),
            cwd: None,
            env: SecretEnv::new(Default::default()),
            env_unset: Vec::new(),
            initial_messages: Vec::new(),
            bro_home: home.to_path_buf(),
            event_log_path: log.to_path_buf(),
        }
    }

    async fn recv_line(events: &mut tokio::sync::mpsc::UnboundedReceiver<String>) -> String {
        tokio::time::timeout(DEADLINE, events.recv())
            .await
            .expect("an event arrived within the deadline")
            .expect("the event lane is open")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fleetd_executor_drives_a_dispatch_and_readopts_across_a_restart() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().canonicalize().expect("canonical tempdir");
        let fleetd = FleetdProcess::start(&root.join("state")).await;

        // ---------------------------------------------------------------------
        // Phase 1: one dispatch end to end. Spawn through fleetd, ingest the
        // relayed events, observe the terminal outcome.
        // ---------------------------------------------------------------------
        let short_log = root.join("short.events.jsonl");
        let short_stub = write_stub(
            &root,
            "short.sh",
            &[
                r#"{"type":"system","seq":1}"#,
                r#"{"type":"assistant","seq":2}"#,
            ],
            &short_log,
            "exit 0\n",
        );

        let executor = FleetdExecutor::new(fleetd.config());
        let mut handle = executor
            .spawn(spec_for(
                &short_stub,
                "sess-short",
                "task-short",
                &short_log,
                &root,
            ))
            .await
            .expect("fleetd accepted the spawn");

        assert!(handle.pid.is_some(), "fleetd reports the child pid");
        assert!(recv_line(&mut handle.events).await.contains(r#""seq":1"#));
        assert!(recv_line(&mut handle.events).await.contains(r#""seq":2"#));

        let outcome = tokio::time::timeout(DEADLINE, handle.outcome)
            .await
            .expect("terminal outcome within the deadline")
            .expect("outcome published");
        assert_eq!(outcome.exit_code, Some(0), "clean exit is reported as such");

        // ---------------------------------------------------------------------
        // Phase 2: re-adoption. A long-lived session, then the executor is thrown
        // away (the daemon restarting) and a fresh one has to find the child and
        // replay from the durable cursor.
        // ---------------------------------------------------------------------
        let long_log = root.join("long.events.jsonl");
        let long_stub = write_stub(
            &root,
            "long.sh",
            &[
                r#"{"type":"system","seq":1}"#,
                r#"{"type":"assistant","seq":2}"#,
                r#"{"type":"assistant","seq":3}"#,
            ],
            &long_log,
            // Idle long enough that the session is unambiguously still running
            // when the second executor enumerates it.
            "exec sleep 300\n",
        );

        let first = FleetdExecutor::new(fleetd.config());
        let mut live = first
            .spawn(spec_for(
                &long_stub,
                "sess-long",
                "task-long",
                &long_log,
                &root,
            ))
            .await
            .expect("fleetd accepted the long spawn");
        for expected in 1..=3u64 {
            let line = recv_line(&mut live.events).await;
            assert!(
                line.contains(&format!(r#""seq":{expected}"#)),
                "expected seq {expected}, got {line}"
            );
        }

        // The daemon goes away mid-session. The child must not. Capture the
        // pid first: it is the only unambiguous way to ask "is that exact
        // child still alive" after the handle is gone.
        let survivor_pid = live.pid.expect("fleetd reported the long child's pid");
        drop(live);
        drop(first);
        tokio::time::sleep(Duration::from_millis(200)).await;

        // A brand-new executor. `ListSessions` on connect is what finds the
        // orphan; `readopt_harness_session` is what would reattach it to a live
        // task. There is no task store installed in this test binary, so
        // re-adoption declines the session and leaves it ALONE, which is itself
        // the contract for a session the daemon does not recognize: never killed.
        let second = FleetdExecutor::new(fleetd.config());
        let replacement_log = root.join("replacement.events.jsonl");
        let replacement_stub = write_stub(
            &root,
            "replacement.sh",
            &[r#"{"type":"system","seq":1}"#],
            &replacement_log,
            "exit 0\n",
        );
        // Spawning through the new executor forces the connect (and therefore the
        // re-adoption sweep) to complete.
        let mut replacement = second
            .spawn(spec_for(
                &replacement_stub,
                "sess-replacement",
                "task-replacement",
                &replacement_log,
                &root,
            ))
            .await
            .expect("the reconnecting executor can still dispatch");
        assert!(
            recv_line(&mut replacement.events)
                .await
                .contains(r#""seq":1"#)
        );

        // The unrecognized survivor is still running: re-adoption logged it and
        // left it be rather than reaping it.
        assert!(
            process_is_alive(survivor_pid),
            "a session the daemon does not recognize must be left running, not \
             killed (pid {survivor_pid})"
        );

        drop(replacement);
        drop(second);
        // Dropping the fleetd process signals its children on the way out.
        drop(fleetd);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remote_fleetd_executor_authenticates_and_drives_a_real_dispatch() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().canonicalize().expect("canonical tempdir");
        let fleetd = FleetdProcess::start_tcp(&root.join("state")).await;
        let log = root.join("remote.events.jsonl");
        let observed_bro_home = root.join("observed-bro-home.txt");
        let stub = write_stub(
            &root,
            "remote.sh",
            &[r#"{"type":"assistant","seq":1}"#],
            &log,
            &format!(
                "printf '%s' \"$BRO_HOME\" > '{}'\nexit 0\n",
                observed_bro_home.display()
            ),
        );

        let executor = FleetdExecutor::new(fleetd.remote_config());
        let mut handle = executor
            .spawn(spec_for(
                &stub,
                "remote-session",
                "remote-task",
                &log,
                &root,
            ))
            .await
            .expect("remote fleetd accepted the spawn");
        assert!(recv_line(&mut handle.events).await.contains(r#""seq":1"#));
        let outcome = tokio::time::timeout(DEADLINE, handle.outcome)
            .await
            .expect("remote outcome within deadline")
            .expect("remote outcome published");
        assert_eq!(outcome.exit_code, Some(0));
        assert_eq!(
            std::fs::read_to_string(observed_bro_home).unwrap(),
            fleetd.state_dir.to_string_lossy(),
            "the remote executor must overwrite daemon-local BRO_HOME with the worker root"
        );
    }

    #[tokio::test]
    async fn unreachable_remote_fleetd_is_never_autostarted_and_creates_no_token() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().canonicalize().expect("canonical tempdir");
        let reservation = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("reserve unreachable port");
        let address = reservation.local_addr().expect("reserved address");
        drop(reservation);
        let token = root.join("missing-fleetd.token");
        let config = FleetdConfig::resolve(
            &root,
            Some(&format!("tcp://{address}")),
            Some(&token),
            Some(&root),
            Some(&root),
        )
        .expect("valid remote config");
        let executor = FleetdExecutor::new(config);
        let error = executor
            .spawn(spec_for(
                "/bin/true",
                "unreachable-remote",
                "unreachable-task",
                &root.join("unreachable.events.jsonl"),
                &root,
            ))
            .await
            .err()
            .expect("unreachable remote fleetd must fail");
        assert!(error.to_string().contains("Remote fleetd is never auto-started"));
        assert!(!token.exists(), "remote token must never be synthesized");
    }

    /// `kill(pid, 0)`: the signal-free liveness probe. Exact, unlike a
    /// `pgrep -f` on the script path, which stops matching the moment the stub
    /// `exec`s and replaces its own argv.
    fn process_is_alive(pid: u32) -> bool {
        // SAFETY: signal 0 sends nothing; it only runs the permission and
        // existence checks and reports through errno.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    /// The full re-adoption drill, against a real fleetd and a real task
    /// store: the daemon ingests part of a session, dies, and the replacement
    /// daemon has to find the still-running child at startup, with no
    /// dispatch to force the connect, reattach it to its task, and replay
    /// exactly the events it missed while it was gone.
    ///
    /// The replacement loads the persisted store the way daemon startup does,
    /// so the task starts out as the restart left it (failed, advising
    /// `bro_resume`). Observation goes through the tool surface: `bro_status`
    /// must read it running, and `bro_resume` on its session must hand back
    /// the live task instead of spawning onto the live supervision key, with
    /// the in-memory resume lease free as it is after every restart.
    ///
    /// The stub gates its second batch on a file the test creates only AFTER
    /// the disconnect, so "events produced while no daemon was attached" is
    /// deterministic rather than a sleep race. Those events reach fleetd's
    /// stdout relay with no owner attached and are dropped on purpose (that is
    /// the documented invariant); the durable event log is the backlog, and
    /// replay is what closes the gap.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_replacement_daemon_reattaches_the_task_and_replays_what_it_missed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().canonicalize().expect("canonical tempdir");
        let fleetd = FleetdProcess::start(&root.join("state")).await;
        let store_dir = root.join("bro");
        std::fs::create_dir_all(&store_dir).expect("store dir");

        let log = root.join("drill.events.jsonl");
        let gate = root.join("resume.gate");
        let stub_path = root.join("drill.sh");
        // The two sinks carry DIFFERENT shapes, and getting this wrong is the
        // whole reason this drill is worth having. stdout is the raw
        // stream-json envelope. The durable log is the harness `EventLog`
        // wrapper, `{"ts":...,"event":{...}}`, which is what fleetd's replay
        // reads `event.seq` out of. A stub that wrote raw envelopes to the log
        // would make replay silently find no seq-carrying lines and send
        // nothing.
        std::fs::write(
            &stub_path,
            format!(
                "#!/bin/sh\n\
                 emit() {{\n\
                 \x20 printf '{{\"type\":\"assistant\",\"seq\":%s}}\\n' \"$1\"\n\
                 \x20 printf '{{\"ts\":\"2026-07-19T00:00:00.000Z\",\"event\":\
{{\"type\":\"assistant\",\"seq\":%s}}}}\\n' \"$1\" >> '{log}'\n\
                 }}\n\
                 for n in 1 2 3; do emit \"$n\"; done\n\
                 while [ ! -f '{gate}' ]; do sleep 0.05; done\n\
                 for n in 4 5; do emit \"$n\"; done\n\
                 exec sleep 300\n",
                log = log.display(),
                gate = gate.display(),
            ),
        )
        .expect("write drill stub");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub_path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        let stub = stub_path.to_string_lossy().into_owned();

        // A real task store with a real task, as the first daemon holds it.
        let store = std::sync::Arc::new(parking_lot::RwLock::new(
            crate::orchestration::TaskStore::new(),
        ));
        let (tail_tx, _tail_rx) = tokio::sync::broadcast::channel(256);
        let task = crate::orchestration::spawn_in_process_task(
            "drill-task".to_string(),
            bro_core::Provider::Glm,
            "drill-session".to_string(),
            None,
            store_dir.clone(),
            store.clone(),
            tail_tx.clone(),
            None,
            None,
            None,
            None,
            bro_core::Origin::AgentDispatch,
        );
        store
            .write()
            .insert_reserved("drill-task".to_string(), task.clone())
            .ok();

        // ---- the first daemon: spawn, wire ingest, get the cursor to 3 ----
        let first = FleetdExecutor::new(fleetd.config());
        let live = first
            .spawn(spec_for(
                &stub,
                "drill-session",
                "drill-task",
                &log,
                &root,
            ))
            .await
            .expect("fleetd accepted the drill spawn");
        let survivor_pid = live.pid.expect("fleetd reported the pid");
        let ingest = crate::orchestration::spawn_harness_ingest_loop(
            task.clone(),
            bro_core::Provider::Glm,
            "drill-task".to_string(),
            store_dir.clone(),
            None,
            tail_tx.clone(),
            None,
            live.events,
        );
        await_cursor(&task, 3).await;

        // ---- the daemon dies mid-session, its store on disk ----
        ingest.abort();
        drop(live.control);
        drop(first);
        store.read().persist(&store_dir);
        drop(store);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            process_is_alive(survivor_pid),
            "the child must outlive the daemon: that is the entire point"
        );

        // Only now does the child emit 4 and 5. No daemon is attached, so
        // fleetd drops the relay and only the durable log keeps them.
        std::fs::write(&gate, b"go").expect("open the gate");

        // Wait until both have actually landed in the durable log BEFORE the
        // replacement daemon connects. Without this barrier the test is a race:
        // if the child emitted them a moment later, they would arrive over the
        // live relay instead and the test would pass whether or not replay
        // works at all.
        await_log_contains(&log, r#""seq":5"#).await;

        // ---- the replacement daemon starts ----
        let state = std::sync::Arc::new(crate::server::state::SharedState::for_test(&store_dir));
        *state.task_store.write() =
            crate::orchestration::TaskStore::load(&store_dir, 24 * 60 * 60 * 1000);
        let task = state
            .task_store
            .read()
            .get("drill-task")
            .expect("the persisted task survives the restart");
        {
            // Pin the restart's verdict and the cursor before re-adoption.
            // Asserting EXACTLY 3 is what makes the final cursor a genuine
            // proof that replay delivered 4 and 5.
            let inner = task.inner.lock();
            assert_eq!(inner.status, crate::orchestration::TaskStatus::Failed);
            assert!(inner.stderr.contains("server restarted"), "{}", inner.stderr);
            assert_eq!(
                inner.harness_ingest_seq, 3,
                "the dead daemon ingested through seq 3 and no further"
            );
        }
        assert!(
            crate::orchestration::install_harness_executor_with_config(
                bbox_config::config::ExecutorKind::Fleetd,
                fleetd.config(),
                store_dir.clone(),
                state.task_store.clone(),
                state.tail_tx.clone(),
                None,
                None,
            ),
            "the replacement daemon installs the fleetd executor"
        );
        // Exactly what daemon startup runs. Nothing dispatches after this.
        crate::orchestration::start_harness_readoption();

        // The replay closed the gap: 4 and 5 were produced while nothing was
        // listening, and the cursor moved only because they were replayed off
        // the durable log from the task's own cursor and ingested.
        await_cursor(&task, 5).await;
        assert_eq!(task.inner.lock().harness_ingest_seq, 5);

        let server = crate::server::BlackboxServer::new(state.clone());
        let status = server.bro_status(rmcp::handler::server::wrapper::Parameters(
            serde_json::from_value(serde_json::json!({ "task_id": "drill-task" }))
                .expect("status params"),
        ));
        assert_ne!(status.is_error, Some(true), "{status:?}");
        let status: serde_json::Value =
            serde_json::from_str(&tool_text(&status)).expect("bro_status JSON");
        assert_eq!(
            status["status"], "running",
            "a reattached session is live, not failed: {status}"
        );
        assert!(
            !status.to_string().contains("server restarted"),
            "the restart notice is gone once the session is back: {status}"
        );
        assert_eq!(
            *task.child_id.lock(),
            Some(survivor_pid),
            "the reattached task points at the surviving child"
        );

        // bro_resume on the live session returns the live task, not a spawn.
        let resumed = server
            .bro_resume(rmcp::handler::server::wrapper::Parameters(
                serde_json::from_value(serde_json::json!({
                    "prompt": "continue",
                    "session_id": "drill-session",
                    "provider": "glm",
                }))
                .expect("resume params"),
            ))
            .await;
        let text = tool_text(&resumed);
        assert_eq!(resumed.is_error, Some(true), "{text}");
        assert!(
            text.contains(r#"bro_wait(task_id="drill-task""#)
                && text.contains(r#"bro_cancel(task_id="drill-task")"#),
            "the caller is pointed at the live task: {text}"
        );
        assert_eq!(
            state.task_store.read().all_tasks().len(),
            1,
            "no resume task was created"
        );
        assert_eq!(
            task.inner.lock().status,
            crate::orchestration::TaskStatus::Running
        );
        assert!(process_is_alive(survivor_pid));

        drop(server);
        drop(fleetd);
    }

    fn tool_text(result: &rmcp::model::CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|content| content.as_text().map(|text| text.text.clone()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Poll the durable event log until it contains `needle`. This is the
    /// barrier that makes the replay assertion honest: it proves the events
    /// existed on disk, with no daemon attached, before the replacement daemon
    /// ever dialed.
    async fn await_log_contains(log: &Path, needle: &str) {
        let deadline = tokio::time::Instant::now() + DEADLINE;
        while tokio::time::Instant::now() < deadline {
            if std::fs::read_to_string(log)
                .map(|text| text.contains(needle))
                .unwrap_or(false)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("{needle} never reached the durable log at {}", log.display());
    }

    /// Poll the task's durable ingest cursor up to the deadline. Polling beats
    /// a fixed sleep: replay is asynchronous, and a too-short sleep would make
    /// this flaky while a too-long one would slow every run.
    async fn await_cursor(task: &std::sync::Arc<crate::orchestration::Task>, want: u64) {
        let deadline = tokio::time::Instant::now() + DEADLINE;
        while tokio::time::Instant::now() < deadline {
            let seen = task.inner.lock().harness_ingest_seq;
            if seen >= want {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!(
            "cursor never reached {want} (stuck at {})",
            task.inner.lock().harness_ingest_seq
        );
    }

    /// A fleetd started with no `--state-dir`, under the daemon's environment,
    /// binds the socket and creates the token the daemon's executor dials:
    /// `FleetdConfig::in_state_dir(cfg.paths.bro_home)`. The environment uses a
    /// `~`-prefixed `BLACKBOX_STATE_DIR`, so both the `bro` suffix and tilde
    /// expansion have to agree.
    #[tokio::test]
    async fn fleetd_default_state_dir_is_the_socket_the_daemon_dials() {
        use super::FleetdEndpoint;

        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().canonicalize().expect("canonical tempdir");
        let missing_config = root.join("absent-config.toml");
        let environment = [
            ("HOME", Some(root.to_string_lossy().into_owned())),
            ("BLACKBOX_STATE_DIR", Some("~/s".to_string())),
            ("BRO_HOME", None),
            ("XDG_STATE_HOME", None),
        ];

        let bro_home = {
            let _guard = crate::util::test_env_lock();
            let saved: Vec<_> = environment
                .iter()
                .map(|(key, _)| (*key, std::env::var_os(key)))
                .collect();
            for (key, value) in &environment {
                match value {
                    Some(value) => unsafe { std::env::set_var(key, value) },
                    None => unsafe { std::env::remove_var(key) },
                }
            }
            let loaded = bbox_config::config::load_with(bbox_config::config::LoadOptions {
                config_path: Some(missing_config),
                ..Default::default()
            });
            for (key, value) in saved {
                match value {
                    Some(value) => unsafe { std::env::set_var(key, value) },
                    None => unsafe { std::env::remove_var(key) },
                }
            }
            loaded.expect("env-only daemon config").paths.bro_home
        };
        assert_eq!(bro_home, root.join("s").join("bro"));

        let config = FleetdConfig::in_state_dir(&bro_home);
        let FleetdEndpoint::Unix(socket) = &config.endpoint else {
            panic!("the state-local default is a Unix endpoint");
        };

        let mut command = std::process::Command::new(fleetd_binary());
        for (key, value) in &environment {
            match value {
                Some(value) => command.env(key, value),
                None => command.env_remove(key),
            };
        }
        let child = command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("start fleetd with no --state-dir");
        let _fleetd = FleetdProcess {
            child,
            state_dir: bro_home.clone(),
            tcp_address: None,
        };

        let deadline = tokio::time::Instant::now() + DEADLINE;
        while tokio::net::UnixStream::connect(socket).await.is_err() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "fleetd did not bind {} within the deadline",
                socket.display()
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            config.token.is_file(),
            "fleetd created the token the daemon reads at {}",
            config.token.display()
        );
    }

    /// The executor must fail LOUDLY when fleetd is unreachable, never fall back to
    /// spawning the worker as a daemon child. A silent downgrade would reintroduce
    /// exactly the restart-drops-sessions problem this slice removes, invisibly.
    #[tokio::test]
    async fn a_missing_fleetd_fails_the_dispatch_instead_of_downgrading() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().canonicalize().expect("canonical tempdir");

        let mut config = FleetdConfig::in_state_dir(root.join("state"));
        // Point auto-start at something that cannot possibly come up, so the
        // bounded wait expires and the error surfaces.
        config.binary = Some(root.join("definitely-not-a-supervisor"));

        let executor = FleetdExecutor::new(config);
        let error = executor
            .spawn(spec_for(
                "/bin/true",
                "sess-none",
                "task-none",
                &root.join("none.events.jsonl"),
                &root,
            ))
            .await
            .err()
            .expect("an unreachable fleetd must fail the dispatch");
        let message = format!("{error:#}");
        assert!(
            message.contains("fleetd"),
            "the error must name fleetd so the operator knows what is missing: {message}"
        );
    }

    /// Keeps `Arc` in use for the `HarnessExecutor` object-safety assumption the
    /// daemon relies on: the installed executor is stored as `Arc<dyn ...>`.
    #[test]
    fn the_executor_is_object_safe() {
        fn assert_object_safe(_: Arc<dyn HarnessExecutor>) {}
        let tmp = tempfile::tempdir().expect("tempdir");
        assert_object_safe(Arc::new(FleetdExecutor::new(FleetdConfig::in_state_dir(
            tmp.path(),
        ))));
    }

}
