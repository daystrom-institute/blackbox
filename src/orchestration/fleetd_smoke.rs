
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
                    };
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            panic!("fleetd did not bind {} within the deadline", socket.display());
        }

        fn config(&self) -> FleetdConfig {
            let mut config = FleetdConfig::in_state_dir(&self.state_dir);
            // Never let an ambient BLACKBOX_FLEETD_BIN from the developer's shell
            // point this at a different supervisor, and never let the executor
            // auto-start a second one: this test owns the fleetd it talks to.
            config.binary = Some(fleetd_binary());
            config
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

    /// `kill(pid, 0)`: the signal-free liveness probe. Exact, unlike a
    /// `pgrep -f` on the script path, which stops matching the moment the stub
    /// `exec`s and replaces its own argv.
    fn process_is_alive(pid: u32) -> bool {
        // SAFETY: signal 0 sends nothing; it only runs the permission and
        // existence checks and reports through errno.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
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
