//! Isolated acceptance probe for the differentiated daemon binaries.
//!
//! The probe first asks Cargo to link the three named production binary targets
//! under a bounded timeout. Exercise it directly with:
//!
//! ```text
//! cargo nextest run --workspace --profile full \
//!   -E 'test(=isolated_daemon_binaries_restart_and_reconcile)'
//! ```

#![cfg(unix)]
// This process-level acceptance harness deliberately performs bounded,
// synchronous fixture and log I/O outside the daemon runtimes it exercises.
#![allow(clippy::disallowed_methods)]

use std::ffi::OsString;
use std::fs::{self, File};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde_json::{Value, json};
use tokio::process::{Child, Command};
use tokio::time::{Instant, sleep, timeout};

const START_TIMEOUT: Duration = Duration::from_secs(15);
const RECONCILE_TIMEOUT: Duration = Duration::from_secs(15);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const STOP_TIMEOUT: Duration = Duration::from_secs(8);
const BINARY_BUILD_TIMEOUT: Duration = Duration::from_secs(180);
const SERVICE_TOKEN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const WRONG_SERVICE_TOKEN: &str =
    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

#[derive(Clone)]
struct DaemonSpec {
    name: &'static str,
    binary: PathBuf,
    args: Vec<OsString>,
    home: PathBuf,
    log_root: PathBuf,
}

struct RunningDaemon {
    name: &'static str,
    child: Child,
    log_path: PathBuf,
}

impl RunningDaemon {
    async fn start(spec: &DaemonSpec, generation: usize) -> Self {
        fs::create_dir_all(&spec.log_root).unwrap();
        let log_path = spec
            .log_root
            .join(format!("{}-{generation}.log", spec.name));
        let stdout = File::create(&log_path).unwrap();
        let stderr = stdout.try_clone().unwrap();
        let tmp = spec.home.join("tmp");
        fs::create_dir_all(&tmp).unwrap();

        let child = Command::new(&spec.binary)
            .args(&spec.args)
            .current_dir(&spec.home)
            .env_clear()
            .env("HOME", &spec.home)
            .env("TMPDIR", tmp)
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("RUST_BACKTRACE", "1")
            .env("RUST_LOG", "info")
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .kill_on_drop(true)
            .spawn()
            .unwrap_or_else(|error| {
                panic!(
                    "failed to launch {} at {}: {error}",
                    spec.name,
                    spec.binary.display()
                )
            });

        Self {
            name: spec.name,
            child,
            log_path,
        }
    }

    fn assert_running(&mut self) {
        if let Some(status) = self.child.try_wait().unwrap() {
            panic!(
                "{} exited unexpectedly with {status}\n{}",
                self.name,
                self.log()
            );
        }
    }

    async fn stop(&mut self) {
        let Some(id) = self.child.id() else {
            return;
        };
        // SAFETY: `id` belongs to the child retained by this object. SIGTERM is
        // the daemons' supported graceful-shutdown contract.
        let result = unsafe { libc::kill(id as i32, libc::SIGTERM) };
        if result != 0 && self.child.try_wait().unwrap().is_none() {
            panic!("failed to signal {}\n{}", self.name, self.log());
        }
        match timeout(STOP_TIMEOUT, self.child.wait()).await {
            Ok(Ok(status)) if status.success() => {}
            Ok(Ok(status)) => {
                panic!(
                    "{} did not shut down cleanly ({status})\n{}",
                    self.name,
                    self.log()
                );
            }
            Ok(Err(error)) => {
                panic!(
                    "waiting for {} shutdown failed: {error}\n{}",
                    self.name,
                    self.log()
                );
            }
            Err(_) => {
                self.child.kill().await.unwrap();
                panic!(
                    "{} did not stop within {STOP_TIMEOUT:?}\n{}",
                    self.name,
                    self.log()
                );
            }
        }
    }

    fn log(&self) -> String {
        fs::read_to_string(&self.log_path)
            .unwrap_or_else(|error| format!("could not read {}: {error}", self.log_path.display()))
    }
}

impl Drop for RunningDaemon {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.start_kill();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn isolated_daemon_binaries_restart_and_reconcile() {
    build_daemon_binaries().await;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let home = root.join("home");
    let state = root.join("state");
    let logs = root.join("logs");
    for path in [&home, &state, &logs] {
        create_private_dir(path);
    }

    let token_path = root.join("auth/service.token");
    write_private_file(&token_path, format!("{SERVICE_TOKEN}\n").as_bytes());

    let harness_log = root.join("fixture-output/launches.log");
    create_private_dir(harness_log.parent().unwrap());
    let harness = root.join("fixtures/bro-harness-fixture");
    write_executable(
        &harness,
        b"#!/bin/sh\nset -eu\nprintf 'launch\\n' >> \"$BLACKBOX_ACCEPTANCE_LAUNCH_LOG\"\n",
    );
    let provider_config = root.join("config/providers.json");
    write_private_file(
        &provider_config,
        serde_json::to_vec(&json!({
            "accounts": {
                "acceptance": {
                    "provider": "glm",
                    "env": {
                        "ANTHROPIC_AUTH_TOKEN": "acceptance-fixture-only",
                        "BLACKBOX_ACCEPTANCE_LAUNCH_LOG": harness_log,
                    },
                    "max_concurrent": 1
                }
            },
            "provider_defaults": {"glm": {"account": "acceptance"}}
        }))
        .unwrap()
        .as_slice(),
    );

    let [corpus_port, fleet_port, blackops_port] = reserved_loopback_ports();

    let corpus_base = format!("http://127.0.0.1:{corpus_port}");
    let fleet_base = format!("http://127.0.0.1:{fleet_port}");
    let blackops_base = format!("http://127.0.0.1:{blackops_port}");
    let binary_dir = binary_directory();
    let corpus_binary = required_binary(&binary_dir, "blackbox-corpusd");
    let fleet_binary = required_binary(&binary_dir, "fleetd");
    let blackops_binary = required_binary(&binary_dir, "blackopsd");

    let corpus_spec = DaemonSpec {
        name: "blackbox-corpusd",
        binary: corpus_binary,
        args: args([
            "--bind".into(),
            "127.0.0.1".into(),
            "--port".into(),
            corpus_port.to_string().into(),
            "--index-path".into(),
            state.join("corpus/index").into_os_string(),
            "--record-root".into(),
            state.join("corpus/records").into_os_string(),
            "--fleet-transcript-root".into(),
            state.join("fleetd/workers").into_os_string(),
            "--service-token-file".into(),
            token_path.clone().into_os_string(),
        ]),
        home: home.clone(),
        log_root: logs.clone(),
    };

    let (fleet_mode, external_launcher) = acceptance_fleet_mode();
    let mut fleet_args = vec![
        "--mode".into(),
        fleet_mode.into(),
        "--bind".into(),
        format!("127.0.0.1:{fleet_port}").into(),
        "--state-dir".into(),
        state.join("fleetd").into_os_string(),
        "--service-token-file".into(),
        token_path.clone().into_os_string(),
        "--worker-socket".into(),
        state.join("fleetd/run/worker.sock").into_os_string(),
        "--bro-harness-bin".into(),
        harness.clone().into_os_string(),
        "--provider-config".into(),
        provider_config.clone().into_os_string(),
        "--managed-worktree-root".into(),
        state.join("fleetd/worktrees").into_os_string(),
        "--blackopsd-url".into(),
        blackops_base.clone().into(),
        "--blackboxd-url".into(),
        corpus_base.clone().into(),
        "--lease-duration-ms".into(),
        "60000".into(),
        "--heartbeat-interval-ms".into(),
        "10000".into(),
        "--reattach-grace-ms".into(),
        "60000".into(),
        "--record-pump-interval-ms".into(),
        "50".into(),
        "--capability-timeout-ms".into(),
        "250".into(),
    ];
    if let Some(launcher) = external_launcher {
        fleet_args.push("--worker-sandbox-launcher".into());
        fleet_args.push(launcher.into_os_string());
    }
    let fleet_spec = DaemonSpec {
        name: "fleetd",
        binary: fleet_binary,
        args: fleet_args,
        home: home.clone(),
        log_root: logs.clone(),
    };

    let blackops_spec = DaemonSpec {
        name: "blackopsd",
        binary: blackops_binary,
        args: vec![
            "--bind".into(),
            format!("127.0.0.1:{blackops_port}").into(),
            "--state-dir".into(),
            state.join("blackopsd").into_os_string(),
            "--catalog-dir".into(),
            state.join("catalog").into_os_string(),
            "--service-token-file".into(),
            token_path.clone().into_os_string(),
            "--fleetd-url".into(),
            fleet_base.clone().into(),
            "--blackboxd-url".into(),
            corpus_base.clone().into(),
            "--default-provider".into(),
            "glm".into(),
            "--default-model".into(),
            "acceptance-fixture".into(),
            "--reconcile-interval-ms".into(),
            "50".into(),
            "--upstream-timeout-ms".into(),
            "5000".into(),
        ],
        home,
        log_root: logs,
    };

    let anonymous = http_client(None);
    let authorized = http_client(Some(SERVICE_TOKEN));
    let wrong_token = http_client(Some(WRONG_SERVICE_TOKEN));

    let mut corpus = RunningDaemon::start(&corpus_spec, 1).await;
    wait_ready(
        &mut corpus,
        &anonymous,
        &corpus_base,
        "blackbox-corpus-service",
    )
    .await;
    let mut fleet = RunningDaemon::start(&fleet_spec, 1).await;
    let fleet_ready = wait_ready(&mut fleet, &anonymous, &fleet_base, "fleetd").await;
    assert_eq!(fleet_ready["mode"], fleet_mode);
    let mut blackops = RunningDaemon::start(&blackops_spec, 1).await;
    wait_ready(&mut blackops, &anonymous, &blackops_base, "blackopsd").await;

    assert_auth_boundaries(
        &anonymous,
        &wrong_token,
        &authorized,
        &corpus_base,
        &fleet_base,
        &blackops_base,
    )
    .await;

    wait_blackops_status(&authorized, &blackops_base, &blackops, |status| {
        status["pending_records"].as_u64() == Some(0)
    })
    .await;

    let execution_request = execution_request();
    let initial_execution = if fleet_mode == "authority" {
        let accepted = post_json(
            &authorized,
            &format!("{fleet_base}/v1/executions"),
            &execution_request,
        )
        .await;
        assert_eq!(accepted["deduplicated"], false);
        wait_for_launch_count(&harness_log, 1).await;
        let duplicate = post_json(
            &authorized,
            &format!("{fleet_base}/v1/executions"),
            &execution_request,
        )
        .await;
        assert_same_execution(&accepted, &duplicate);
        assert_eq!(duplicate["deduplicated"], true);
        sleep(Duration::from_millis(100)).await;
        assert_eq!(
            launch_count(&harness_log),
            1,
            "duplicate dispatch launched a worker"
        );
        Some(accepted)
    } else {
        None
    };

    install_definition(&authorized, &blackops_base, "acceptance-baseline").await;
    wait_blackops_status(&authorized, &blackops_base, &blackops, |status| {
        status["pending_records"].as_u64() == Some(0)
    })
    .await;
    wait_for_record(
        &authorized,
        &corpus_base,
        "acceptance-baseline",
        "blackops:definition:atom:acceptance-baseline:1.0.0",
    )
    .await;

    corpus.stop().await;
    wait_unavailable(&anonymous, &format!("{corpus_base}/healthz")).await;
    public_health(&anonymous, &fleet_base, "fleetd").await;
    public_health(&anonymous, &blackops_base, "blackopsd").await;

    install_definition(&authorized, &blackops_base, "acceptance-catchup").await;
    wait_blackops_status(&authorized, &blackops_base, &blackops, |status| {
        status["pending_records"]
            .as_u64()
            .is_some_and(|count| count > 0)
    })
    .await;

    corpus = RunningDaemon::start(&corpus_spec, 2).await;
    wait_ready(
        &mut corpus,
        &anonymous,
        &corpus_base,
        "blackbox-corpus-service",
    )
    .await;
    wait_blackops_status(&authorized, &blackops_base, &blackops, |status| {
        status["pending_records"].as_u64() == Some(0)
    })
    .await;
    wait_for_record(
        &authorized,
        &corpus_base,
        "acceptance-baseline",
        "blackops:definition:atom:acceptance-baseline:1.0.0",
    )
    .await;
    wait_for_record(
        &authorized,
        &corpus_base,
        "acceptance-catchup",
        "blackops:definition:atom:acceptance-catchup:1.0.0",
    )
    .await;

    let before_blackops_restart =
        get_json(&authorized, &format!("{blackops_base}/v1/status")).await;
    blackops.stop().await;
    wait_unavailable(&anonymous, &format!("{blackops_base}/healthz")).await;
    public_health(&anonymous, &corpus_base, "blackbox-corpus-service").await;
    public_health(&anonymous, &fleet_base, "fleetd").await;

    blackops = RunningDaemon::start(&blackops_spec, 2).await;
    wait_ready(&mut blackops, &anonymous, &blackops_base, "blackopsd").await;
    let after_blackops_restart =
        wait_blackops_status(&authorized, &blackops_base, &blackops, |status| {
            status["pending_records"].as_u64() == Some(0)
        })
        .await;
    assert_eq!(
        after_blackops_restart["definitions"], before_blackops_restart["definitions"],
        "blackops definition authority changed across restart"
    );
    assert!(
        after_blackops_restart["generation"].as_u64()
            >= before_blackops_restart["generation"].as_u64(),
        "blackops generation moved backwards across restart"
    );
    let definitions = get_json(&authorized, &format!("{blackops_base}/v1/definitions")).await;
    for name in ["acceptance-baseline", "acceptance-catchup"] {
        assert!(
            definitions.as_array().unwrap().iter().any(|definition| {
                definition["key"]["name"].as_str() == Some(name)
                    && definition["key"]["version"].as_str() == Some("1.0.0")
            }),
            "definition {name} did not survive blackopsd restart"
        );
    }

    fleet.stop().await;
    wait_unavailable(&anonymous, &format!("{fleet_base}/healthz")).await;
    public_health(&anonymous, &corpus_base, "blackbox-corpus-service").await;
    public_health(&anonymous, &blackops_base, "blackopsd").await;

    fleet = RunningDaemon::start(&fleet_spec, 2).await;
    wait_ready(&mut fleet, &anonymous, &fleet_base, "fleetd").await;
    if let Some(initial_execution) = initial_execution {
        let replay = post_json(
            &authorized,
            &format!("{fleet_base}/v1/executions"),
            &execution_request,
        )
        .await;
        assert_same_execution(&initial_execution, &replay);
        assert_eq!(replay["deduplicated"], true);
        sleep(Duration::from_millis(100)).await;
        assert_eq!(
            launch_count(&harness_log),
            1,
            "fleetd replay blindly redispatched the accepted operation"
        );
        let roster = get_json(&authorized, &format!("{fleet_base}/control/roster")).await;
        assert_eq!(roster["tasks"].as_array().map(Vec::len), Some(1));
    }

    blackops.stop().await;
    fleet.stop().await;
    corpus.stop().await;
}

fn args<const N: usize>(values: [OsString; N]) -> Vec<OsString> {
    values.into_iter().collect()
}

fn binary_directory() -> PathBuf {
    if let Some(path) = std::env::var_os("BLACKBOX_ACCEPTANCE_BIN_DIR") {
        return PathBuf::from(path).canonicalize().unwrap();
    }
    let executable = std::env::current_exe().unwrap().canonicalize().unwrap();
    let test_directory = executable.parent().unwrap();
    if test_directory
        .file_name()
        .is_some_and(|name| name == "deps")
    {
        test_directory.parent().unwrap().to_path_buf()
    } else {
        test_directory.to_path_buf()
    }
}

fn required_binary(directory: &Path, name: &str) -> PathBuf {
    let path = directory.join(name);
    let metadata = fs::metadata(&path).unwrap_or_else(|error| {
        panic!(
            "required binary {} is unavailable ({error}); run `cargo build --workspace --bins` first or set BLACKBOX_ACCEPTANCE_BIN_DIR",
            path.display()
        )
    });
    assert!(
        metadata.is_file(),
        "{} is not a regular file",
        path.display()
    );
    assert_ne!(
        metadata.permissions().mode() & 0o111,
        0,
        "{} is not executable",
        path.display()
    );
    path.canonicalize().unwrap()
}

fn acceptance_fleet_mode() -> (&'static str, Option<PathBuf>) {
    #[cfg(target_os = "macos")]
    {
        ("authority", None)
    }
    #[cfg(target_os = "linux")]
    {
        match std::env::var_os("BLACKBOX_ACCEPTANCE_WORKER_SANDBOX_LAUNCHER") {
            Some(path) => (
                "authority",
                Some(PathBuf::from(path).canonicalize().unwrap()),
            ),
            None => ("shadow", None),
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        ("shadow", None)
    }
}

async fn build_daemon_binaries() {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let child = Command::new(cargo)
        .args([
            "build",
            "-p",
            "blackbox-corpus-service",
            "--bin",
            "blackbox-corpusd",
            "-p",
            "fleetd",
            "--bin",
            "fleetd",
            "-p",
            "blackopsd",
            "--bin",
            "blackopsd",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let output = timeout(BINARY_BUILD_TIMEOUT, child.wait_with_output())
        .await
        .unwrap_or_else(|_| {
            panic!("daemon binary build exceeded the {BINARY_BUILD_TIMEOUT:?} acceptance bound")
        })
        .unwrap();
    assert!(
        output.status.success(),
        "building daemon binaries failed ({})\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn reserved_loopback_ports() -> [u16; 3] {
    let listeners = [
        TcpListener::bind(("127.0.0.1", 0)).unwrap(),
        TcpListener::bind(("127.0.0.1", 0)).unwrap(),
        TcpListener::bind(("127.0.0.1", 0)).unwrap(),
    ];
    let ports = listeners.map(|listener| listener.local_addr().unwrap().port());
    assert_ne!(ports[0], ports[1]);
    assert_ne!(ports[0], ports[2]);
    assert_ne!(ports[1], ports[2]);
    ports
}

fn create_private_dir(path: &Path) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn write_private_file(path: &Path, body: &[u8]) {
    create_private_dir(path.parent().unwrap());
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn write_executable(path: &Path, body: &[u8]) {
    write_private_file(path, body);
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn http_client(token: Option<&str>) -> reqwest::Client {
    let mut headers = HeaderMap::new();
    if let Some(token) = token {
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
    }
    reqwest::Client::builder()
        .default_headers(headers)
        .connect_timeout(Duration::from_millis(250))
        .timeout(REQUEST_TIMEOUT)
        .no_proxy()
        .build()
        .unwrap()
}

async fn wait_ready(
    daemon: &mut RunningDaemon,
    client: &reqwest::Client,
    base: &str,
    service: &str,
) -> Value {
    let deadline = Instant::now() + START_TIMEOUT;
    let url = format!("{base}/readyz");
    loop {
        daemon.assert_running();
        if let Ok(response) = client.get(&url).send().await
            && response.status().is_success()
        {
            let value: Value = response.json().await.unwrap();
            if value["ready"].as_bool().unwrap_or(true)
                && (value["service"].as_str().is_none()
                    || value["service"].as_str() == Some(service))
            {
                assert!(
                    value["build_id"].as_str().is_some_and(|id| !id.is_empty()),
                    "{service} did not report build identity: {value}"
                );
                return value;
            }
        }
        assert!(
            Instant::now() < deadline,
            "{service} did not become ready within {START_TIMEOUT:?}\n{}",
            daemon.log()
        );
        sleep(Duration::from_millis(25)).await;
    }
}

async fn public_health(client: &reqwest::Client, base: &str, service: &str) -> Value {
    let value = get_json(client, &format!("{base}/healthz")).await;
    assert_eq!(value["service"], service);
    assert!(value["build_id"].as_str().is_some_and(|id| !id.is_empty()));
    value
}

async fn assert_auth_boundaries(
    anonymous: &reqwest::Client,
    wrong: &reqwest::Client,
    authorized: &reqwest::Client,
    corpus: &str,
    fleet: &str,
    blackops: &str,
) {
    public_health(anonymous, corpus, "blackbox-corpus-service").await;
    public_health(anonymous, fleet, "fleetd").await;
    public_health(anonymous, blackops, "blackopsd").await;

    for client in [anonymous, wrong] {
        let corpus_response = client
            .post(format!("{corpus}/internal/records"))
            .json(&json!({"records": []}))
            .send()
            .await
            .unwrap();
        assert_eq!(corpus_response.status(), reqwest::StatusCode::UNAUTHORIZED);
        let fleet_response = client
            .get(format!("{fleet}/control/roster"))
            .send()
            .await
            .unwrap();
        assert_eq!(fleet_response.status(), reqwest::StatusCode::UNAUTHORIZED);
        let blackops_response = client
            .get(format!("{blackops}/v1/status"))
            .send()
            .await
            .unwrap();
        assert_eq!(
            blackops_response.status(),
            reqwest::StatusCode::UNAUTHORIZED
        );
    }

    let receipt = post_json(
        authorized,
        &format!("{corpus}/internal/records"),
        &json!({"records": []}),
    )
    .await;
    assert_eq!(receipt["accepted"], 0);
    get_json(authorized, &format!("{fleet}/control/roster")).await;
    let status = get_json(authorized, &format!("{blackops}/v1/status")).await;
    assert_eq!(status["service"], "blackopsd");
}

fn execution_request() -> Value {
    json!({
        "operation_id": "acceptance-operation",
        "idempotency_key": "acceptance-operation-v1",
        "kind": {"type": "fresh", "prompt": "fixture process only"},
        "provider": "glm",
        "model": "acceptance-fixture",
        "effort": null,
        "service_tier": "default",
        "code_mode": null,
        "dispatch_context": null,
        "working_set": {"type": "scratch"},
        "shell_env": {},
        "tool_policy": {},
        "system_prompt": null,
        "output_schema": null,
        "labels": {"acceptance": "isolated-daemon-binaries"}
    })
}

fn assert_same_execution(expected: &Value, actual: &Value) {
    for field in ["operation_id", "attempt_id", "task_id", "session_id"] {
        assert_eq!(actual[field], expected[field], "execution {field} drifted");
    }
}

async fn install_definition(client: &reqwest::Client, base: &str, name: &str) {
    let installed = post_json(
        client,
        &format!("{base}/v1/definitions"),
        &json!({
            "kind": "atom",
            "name": name,
            "version": "1.0.0",
            "input_contract": {},
            "body": {"acceptance_marker": name},
            "activate": true,
            "created_at_unix_ms": 1
        }),
    )
    .await;
    assert_eq!(installed["key"]["name"], name);
}

async fn wait_blackops_status(
    client: &reqwest::Client,
    base: &str,
    blackops: &RunningDaemon,
    predicate: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = Instant::now() + RECONCILE_TIMEOUT;
    loop {
        let last_observation = match client.get(format!("{base}/v1/status")).send().await {
            Ok(response) if response.status().is_success() => {
                let status: Value = response.json().await.unwrap();
                if predicate(&status) {
                    return status;
                }
                status.to_string()
            }
            Ok(response) => format!("HTTP {}", response.status()),
            Err(error) => error.to_string(),
        };
        assert!(
            Instant::now() < deadline,
            "blackops status did not reconcile within {RECONCILE_TIMEOUT:?}: {last_observation}\n{}",
            blackops.log()
        );
        sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_for_record(client: &reqwest::Client, base: &str, query: &str, record_id: &str) {
    let deadline = Instant::now() + RECONCILE_TIMEOUT;
    loop {
        let response = post_json(
            client,
            &format!("{base}/internal/capability"),
            &json!({
                "worker_id": "acceptance-worker",
                "session_id": "acceptance-session",
                "authorization": {
                    "worker_id": "acceptance-worker",
                    "session_id": "acceptance-session",
                    "task_id": "acceptance-task",
                    "attempt_id": "acceptance-attempt",
                    "session_attempt_generation": 1,
                    "policy": {
                        "version": 1,
                        "digest": "sha256:acceptance-policy"
                    },
                    "capability_policy": {
                        "allowed_operations": {
                            "corpus": ["search_corpus"]
                        },
                        "allowed_atom_refs": []
                    }
                },
                "request": {
                    "call_id": format!("search-{query}"),
                    "capability": "corpus",
                    "operation": "search_corpus",
                    "bounded_payload": {"query": query, "limit": 20}
                }
            }),
        )
        .await;
        assert_eq!(
            response["is_error"], false,
            "corpus search failed: {response}"
        );
        let matching = response["result_or_error"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|hit| hit["id"].as_str() == Some(record_id))
            .count();
        if matching == 1 {
            return;
        }
        assert_eq!(
            matching, 0,
            "record {record_id} was projected more than once"
        );
        assert!(
            Instant::now() < deadline,
            "record {record_id} did not reconcile within {RECONCILE_TIMEOUT:?}: {response}"
        );
        sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_for_launch_count(path: &Path, expected: usize) {
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if launch_count(path) == expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "harness fixture did not record {expected} launches"
        );
        sleep(Duration::from_millis(25)).await;
    }
}

fn launch_count(path: &Path) -> usize {
    fs::read_to_string(path)
        .map(|body| body.lines().count())
        .unwrap_or(0)
}

async fn wait_unavailable(client: &reqwest::Client, url: &str) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if client.get(url).send().await.is_err() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "service remained available after its process stopped: {url}"
        );
        sleep(Duration::from_millis(25)).await;
    }
}

async fn get_json(client: &reqwest::Client, url: &str) -> Value {
    response_json(client.get(url).send().await.unwrap(), url).await
}

async fn post_json(client: &reqwest::Client, url: &str, body: &Value) -> Value {
    response_json(client.post(url).json(body).send().await.unwrap(), url).await
}

async fn response_json(response: reqwest::Response, context: &str) -> Value {
    let status = response.status();
    let body = response.text().await.unwrap();
    assert!(status.is_success(), "{context} returned {status}: {body}");
    serde_json::from_str(&body)
        .unwrap_or_else(|error| panic!("{context} returned invalid JSON ({error}): {body}"))
}
