//! Lane host-build sessions disable rust-analyzer's automatic flycheck.
//!
//! Three layers: the pure lane predicate and initialize options; a fake
//! server that records the spawn environment and the initialize message it
//! actually received; and a real rust-analyzer run with a recording `CARGO`
//! wrapper that proves flycheck is observable under defaults and absent in
//! lane mode while native analysis and host build data keep working. The
//! real-server tests resolve the toolchain first, then run under a temporary
//! home, XDG and Cargo state; they skip when no rust-analyzer binary resolves.

// Test-only module that writes fixture trees and reads fake-server records.
// The blocking-I/O lint guards production actor contexts (concurrency-model
// §5); it has no production surface to protect here.
#![allow(clippy::disallowed_methods)]

use std::ffi::{OsStr, OsString};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::sync::{Mutex as StdMutex, MutexGuard};

use lsp_types::notification::DidSaveTextDocument;
use lsp_types::{DiagnosticSeverity, DidSaveTextDocumentParams, Position};

use super::*;

const CHECK_ON_SAVE_OFF: &str = r#"{"checkOnSave":false}"#;

static ENV_LOCK: StdMutex<()> = StdMutex::new(());

/// Serializes process-env mutation within this crate's test binary and
/// restores every touched variable on drop.
struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
    saved: Vec<(&'static str, Option<OsString>)>,
}

impl EnvGuard {
    fn new() -> Self {
        Self {
            _lock: ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            saved: Vec::new(),
        }
    }

    fn remember(&mut self, key: &'static str) {
        if !self.saved.iter().any(|(saved, _)| *saved == key) {
            self.saved.push((key, std::env::var_os(key)));
        }
    }

    fn set(&mut self, key: &'static str, value: impl AsRef<OsStr>) {
        self.remember(key);
        // SAFETY: env mutation is serialized by ENV_LOCK and undone on drop.
        unsafe { std::env::set_var(key, value) };
    }

    fn remove(&mut self, key: &'static str) {
        self.remember(key);
        // SAFETY: env mutation is serialized by ENV_LOCK and undone on drop.
        unsafe { std::env::remove_var(key) };
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in self.saved.drain(..).rev() {
            // SAFETY: still under ENV_LOCK; restores the pre-test value.
            unsafe {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

fn canonical_tempdir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    (dir, root)
}

fn mkdir(path: &Path) -> PathBuf {
    std::fs::create_dir_all(path).unwrap();
    path.canonicalize().unwrap()
}

fn write_executable(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn check_on_save_off() -> Value {
    serde_json::from_str(CHECK_ON_SAVE_OFF).unwrap()
}

fn rust_options(root: &Path, lane_host_build: bool) -> Option<Value> {
    build_init_params(root, Language::Rust, &LspConfig::default(), lane_host_build)
        .unwrap()
        .initialization_options
}

// ── lane predicate and initialize options ──

#[test]
fn lane_flag_values_select_checkless_rust_options() {
    let (_dir, base) = canonical_tempdir();
    let home = mkdir(&base.join("home"));
    let root = mkdir(&base.join("work"));
    let cases = [
        (None, false),
        (Some(""), false),
        (Some("   "), false),
        (Some("0"), false),
        (Some(" 0 "), false),
        (Some("1"), true),
        (Some(" 1\n"), true),
        (Some("yes"), true),
        (Some("2"), true),
    ];
    for (flag, expected) in cases {
        let lane = lane_host_build_detected(&root, flag, Some(&home));
        assert_eq!(lane, expected, "flag {flag:?}");
        let expected_options = expected.then(check_on_save_off);
        assert_eq!(rust_options(&root, lane), expected_options, "flag {flag:?}");
    }
}

#[test]
fn lane_paths_select_checkless_rust_options_through_symlinks() {
    let (_dir, base) = canonical_tempdir();
    let home = mkdir(&base.join("home"));
    let lane_root = mkdir(&home.join("lanes/pool1/blackbox"));
    let home_link = base.join("home-link");
    symlink(&home, &home_link).unwrap();
    let outside_link = base.join("checkout-link");
    symlink(&lane_root, &outside_link).unwrap();

    let lane_views = [
        (lane_root.clone(), home.clone()),
        // Root reached through a symlinked home.
        (home_link.join("lanes/pool1/blackbox"), home.clone()),
        // Home itself given as a symlink.
        (lane_root.clone(), home_link.clone()),
        // A link outside home that resolves into the lanes tree.
        (outside_link.clone(), home.clone()),
    ];
    for (root, home) in &lane_views {
        // `0`, empty, and absent flags never override path detection.
        for flag in [None, Some(""), Some("0")] {
            let lane = lane_host_build_detected(root, flag, Some(home));
            assert!(
                lane,
                "root {} home {} flag {flag:?}",
                root.display(),
                home.display()
            );
            assert_eq!(rust_options(root, lane), Some(check_on_save_off()));
        }
    }

    let non_lane = [
        mkdir(&home.join("src/blackbox")),
        // Sibling whose name merely starts with "lanes".
        mkdir(&home.join("lanes-archive/blackbox")),
        mkdir(&base.join("elsewhere/lanes/blackbox")),
        // Missing roots cannot canonicalize, so they are not lane roots.
        home.join("lanes/missing"),
    ];
    for root in &non_lane {
        let lane = lane_host_build_detected(root, Some("0"), Some(&home));
        assert!(!lane, "root {}", root.display());
        assert_eq!(rust_options(root, lane), None);
    }
    assert!(!lane_host_build_detected(&lane_root, None, None));
}

#[test]
fn java_initialize_options_ignore_rust_lane_policy() {
    let (_dir, root) = canonical_tempdir();
    let unpinned = LspConfig {
        jdtls_gradle_version: None,
        ..LspConfig::default()
    };
    let pinned = LspConfig {
        jdtls_gradle_version: Some("8.14.3".into()),
        ..LspConfig::default()
    };
    let pinned_options = serde_json::json!({
        "settings": { "java": { "import": { "gradle": {
            "version": "8.14.3",
            "wrapper": { "enabled": false }
        } } } }
    });
    for lane_host_build in [false, true] {
        let java = |config: &LspConfig| {
            build_init_params(&root, Language::Java, config, lane_host_build)
                .unwrap()
                .initialization_options
        };
        assert_eq!(java(&unpinned), None);
        assert_eq!(java(&pinned), Some(pinned_options.clone()));
    }
    assert_eq!(
        build_init_params(&root, Language::Rust, &pinned, false)
            .unwrap()
            .initialization_options,
        None,
        "a Java gradle pin must not leak into Rust sessions",
    );
}

// ── spawn/protocol: one decision drives environment and initialize ──

/// A minimal language server in POSIX sh: records its environment and the
/// raw initialize request under `record`, answers it, reports `health`, then
/// idles until stdin closes.
fn fake_server(record: &Path, health: &str) -> PathBuf {
    std::fs::create_dir_all(record).unwrap();
    let script = record.join("fake-server");
    let rec = record.display();
    write_executable(
        &script,
        &format!(
            r#"#!/bin/sh
env > '{rec}/env.txt'
IFS= read -r header
IFS= read -r blank
length=$(printf '%s' "$header" | tr -cd '0-9')
head -c "$length" > '{rec}/initialize.json'
reply() {{ printf 'Content-Length: %s\r\n\r\n%s' "${{#1}}" "$1"; }}
reply '{{"jsonrpc":"2.0","id":1,"result":{{"capabilities":{{}}}}}}'
reply '{{"jsonrpc":"2.0","method":"experimental/serverStatus","params":{{"health":"{health}","quiescent":true}}}}'
exec cat > /dev/null
"#
        ),
    );
    script
}

struct Recorded {
    env: BTreeMap<String, String>,
    initialize: Value,
}

impl Recorded {
    fn read(record: &Path) -> Self {
        let env = std::fs::read_to_string(record.join("env.txt"))
            .unwrap()
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        let initialize =
            serde_json::from_slice(&std::fs::read(record.join("initialize.json")).unwrap())
                .unwrap();
        Self { env, initialize }
    }

    fn initialization_options(&self) -> Option<&Value> {
        assert_eq!(self.initialize["method"], "initialize");
        self.initialize["params"].get("initializationOptions")
    }

    fn path_has(&self, dir: &Path) -> bool {
        let dir = dir.display().to_string();
        self.env
            .get("PATH")
            .is_some_and(|path| path.split(':').any(|entry| entry == dir))
    }
}

/// Isolated lane environment: temp HOME, shim dir first on PATH, and a temp
/// host target dir. Returns (home, shim, target).
fn isolate_lane_env(env: &mut EnvGuard, base: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let home = mkdir(&base.join("home"));
    let shim = mkdir(&base.join("shims"));
    let target = mkdir(&base.join("ra-target"));
    let path = std::env::var("PATH").unwrap_or_default();
    env.set("PATH", format!("{}:{path}", shim.display()));
    env.set("HOME", &home);
    env.set("BRO_LSP_LANE_SHIM_DIR", &shim);
    env.set("BRO_LSP_RA_TARGET_DIR", &target);
    (home, shim, target)
}

fn fake_config(server: PathBuf, language: Language) -> LspConfig {
    let mut config = LspConfig {
        child_env_scrub_keys: vec!["CARGO_TARGET_DIR".into()],
        init_timeout: Duration::from_secs(20),
        ready_timeout: Duration::from_secs(20),
        request_timeout: Duration::from_secs(20),
        jdtls_ready_timeout: Duration::from_millis(300),
        jdtls_gradle_version: None,
        ..LspConfig::default()
    };
    match language {
        Language::Rust => config.rust_analyzer_bin = Some(server),
        Language::Java => config.jdtls_bin = Some(server),
    }
    config
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rust_spawn_sends_the_environment_decision_in_initialize() {
    let mut env = EnvGuard::new();
    let (_dir, base) = canonical_tempdir();
    let (home, shim, target) = isolate_lane_env(&mut env, &base);
    let cases = [
        ("flag", Some("1"), mkdir(&base.join("work/flag")), true),
        (
            "other-flag",
            Some("yes"),
            mkdir(&base.join("work/other")),
            true,
        ),
        ("path", None, mkdir(&home.join("lanes/a/blackbox")), true),
        (
            "path-flag-0",
            Some("0"),
            mkdir(&home.join("lanes/b/blackbox")),
            true,
        ),
        ("plain", None, mkdir(&base.join("work/plain")), false),
        (
            "plain-flag-0",
            Some("0"),
            mkdir(&base.join("work/zero")),
            false,
        ),
        (
            "plain-flag-empty",
            Some(" "),
            mkdir(&base.join("work/empty")),
            false,
        ),
    ];
    for (name, flag, root, lane) in cases {
        match flag {
            Some(flag) => env.set("BRO_LSP_RA_HOST_BUILD", flag),
            None => env.remove("BRO_LSP_RA_HOST_BUILD"),
        }
        let record = base.join("records").join(name);
        let config = fake_config(fake_server(&record, "ok"), Language::Rust);
        let key = SessionKey {
            root,
            language: Language::Rust,
        };
        let session = spawn_session(&key, &config).await.unwrap();
        assert_eq!(session.readiness.state, ReadinessState::Ready, "{name}");
        let recorded = Recorded::read(&record);

        let target_dir = recorded.env.get("CARGO_TARGET_DIR");
        let options = recorded.initialization_options();
        if lane {
            assert_eq!(target_dir, Some(&target.display().to_string()), "{name}");
            assert!(
                !recorded.path_has(&shim),
                "{name}: lane shim must be stripped"
            );
            assert_eq!(options, Some(&check_on_save_off()), "{name}");
        } else {
            assert_eq!(target_dir, None, "{name}");
            assert!(recorded.path_has(&shim), "{name}: PATH must be untouched");
            assert_eq!(options, None, "{name}");
        }
        // kill_on_drop ends the fake server; it never answers shutdown.
        drop(session);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn java_spawn_is_unchanged_when_the_rust_lane_flag_is_set() {
    let mut env = EnvGuard::new();
    let (_dir, base) = canonical_tempdir();
    let (home, shim, _target) = isolate_lane_env(&mut env, &base);
    env.set("BRO_LSP_RA_HOST_BUILD", "1");
    let root = mkdir(&home.join("lanes/java/app"));
    for pin in [None, Some("8.14.3")] {
        let record = base.join("records").join(pin.unwrap_or("wrapper"));
        let config = LspConfig {
            jdtls_gradle_version: pin.map(str::to_string),
            ..fake_config(fake_server(&record, "ok"), Language::Java)
        };
        let key = SessionKey {
            root: root.clone(),
            language: Language::Java,
        };
        let session = spawn_session(&key, &config).await.unwrap();
        let recorded = Recorded::read(&record);
        assert_eq!(recorded.env.get("CARGO_TARGET_DIR"), None);
        assert!(recorded.path_has(&shim), "Java keeps the inherited PATH");
        let expected = pin.map(|version| {
            serde_json::json!({
                "settings": { "java": { "import": { "gradle": {
                    "version": version,
                    "wrapper": { "enabled": false }
                } } } }
            })
        });
        assert_eq!(recorded.initialization_options(), expected.as_ref());
        drop(session);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lane_session_failures_stay_errors() {
    let mut env = EnvGuard::new();
    let (_dir, base) = canonical_tempdir();
    let (home, _shim, _target) = isolate_lane_env(&mut env, &base);
    env.remove("BRO_LSP_RA_HOST_BUILD");
    let root = mkdir(&home.join("lanes/fail/blackbox"));
    std::fs::create_dir_all(root.join("src")).unwrap();
    let source = root.join("src/lib.rs");
    std::fs::write(&source, "pub fn f() {}\n").unwrap();

    let missing = SessionPool::new(fake_config(base.join("missing-server"), Language::Rust));
    let err = missing
        .open_document(&root, Language::Rust, &source, 1, "pub fn f() {}\n".into())
        .await
        .expect_err("a missing lane server must be an error");
    assert!(err.is_lsp_unavailable(), "got {err:?}");

    let record = base.join("records/unhealthy");
    let failed = SessionPool::new(fake_config(fake_server(&record, "error"), Language::Rust));
    let doc = failed
        .open_document(&root, Language::Rust, &source, 1, "pub fn f() {}\n".into())
        .await
        .unwrap();
    assert_eq!(
        Recorded::read(&record).initialization_options(),
        Some(&check_on_save_off())
    );
    let err = failed
        .hover(&doc, Position::new(0, 7), Duration::from_secs(5))
        .await
        .expect_err("failed readiness must not answer as success");
    assert!(
        matches!(
            err,
            Error::NotReady {
                state: ReadinessState::Failed,
                ..
            }
        ),
        "got {err:?}"
    );
    drop(failed);
}

// ── real rust-analyzer: flycheck observable by default, absent in lanes ──

const APP_SOURCE: &str = "mac::make_answer!();
include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\"));

pub fn total() -> u32 {
    answer_from_macro() + answer_from_build()
}
";

/// Workspace with a proc-macro crate and a build script, so analysis build
/// data (build-script output, proc-macro dylib) is needed for full semantics.
fn write_fixture(root: &Path) -> PathBuf {
    let files = [
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"app\", \"mac\"]\nresolver = \"2\"\n",
        ),
        (
            "mac/Cargo.toml",
            "[package]\nname = \"mac\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\nproc-macro = true\n",
        ),
        (
            "mac/src/lib.rs",
            "use proc_macro::TokenStream;\n\n#[proc_macro]\npub fn make_answer(_item: TokenStream) -> TokenStream {\n    \"pub fn answer_from_macro() -> u32 { 42 }\".parse().unwrap()\n}\n",
        ),
        (
            "app/Cargo.toml",
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nmac = { path = \"../mac\" }\n",
        ),
        (
            "app/build.rs",
            "fn main() {\n    let out = std::env::var(\"OUT_DIR\").unwrap();\n    std::fs::write(\n        format!(\"{out}/generated.rs\"),\n        \"pub fn answer_from_build() -> u32 { 7 }\\n\",\n    )\n    .unwrap();\n}\n",
        ),
        ("app/src/lib.rs", APP_SOURCE),
    ];
    for (path, body) in files {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }
    root.join("app/src/lib.rs").canonicalize().unwrap()
}

/// Controlled flycheck command. rust-analyzer resolves flycheck's cargo
/// through `CARGO`; every invocation is logged, a flycheck-shaped one (a
/// `check` without the build-data run's `--quiet`) returns immediately, and
/// anything else runs the real cargo so analysis builds still work.
struct CargoRecorder {
    wrapper: PathBuf,
    log: PathBuf,
}

impl CargoRecorder {
    fn new(dir: &Path, toolchain: &Toolchain) -> Self {
        let real = toolchain.bin().join("cargo");
        let wrapper = dir.join("recording-cargo");
        let log = dir.join("cargo.log");
        write_executable(
            &wrapper,
            &format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{log}'
if [ "$1" = check ]; then
  case " $* " in
    *" --quiet "*) ;;
    *) exit 0 ;;
  esac
fi
exec '{real}' "$@"
"#,
                log = log.display(),
                real = real.display(),
            ),
        );
        Self { wrapper, log }
    }

    fn invocations(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn flychecks(&self) -> usize {
        self.invocations()
            .iter()
            .filter(|args| is_flycheck(args))
            .count()
    }

    async fn wait_for_flychecks(&self, at_least: usize, timeout: Duration) -> usize {
        let deadline = Instant::now() + timeout;
        loop {
            let count = self.flychecks();
            if count >= at_least || Instant::now() >= deadline {
                return count;
            }
            time::sleep(Duration::from_millis(200)).await;
        }
    }
}

fn is_flycheck(args: &str) -> bool {
    let mut words = args.split_whitespace();
    words.next() == Some("check") && !words.any(|word| word == "--quiet")
}

/// Toolchain executables resolved from the ambient environment before a
/// real-server test isolates it. The isolated run then reaches the compiler
/// through explicit sysroot paths and reads no user configuration.
struct Toolchain {
    rust_analyzer: PathBuf,
    sysroot: PathBuf,
}

impl Toolchain {
    fn resolve() -> Option<Self> {
        let rustc = env_path("RUSTC").unwrap_or_else(|| PathBuf::from("rustc"));
        let output = std::process::Command::new(rustc)
            .args(["--print", "sysroot"])
            .output()
            .ok()
            .filter(|output| output.status.success())?;
        let sysroot = PathBuf::from(String::from_utf8(output.stdout).ok()?.trim())
            .canonicalize()
            .ok()?;
        let bin = sysroot.join("bin");
        if !bin.join("cargo").is_file() || !bin.join("rustc").is_file() {
            return None;
        }
        let rust_analyzer = [
            env_path("BRO_LSP_RUST_ANALYZER_BIN"),
            env_path("BRO_RUST_ANALYZER_BIN"),
            env_path("BLACKBOX_RUST_ANALYZER_BIN"),
            Some(bin.join("rust-analyzer")),
            Some(PathBuf::from("rust-analyzer")),
            dirs::home_dir().map(|home| home.join(".cargo/bin/rust-analyzer")),
        ]
        .into_iter()
        .flatten()
        .filter_map(|candidate| absolute_executable(&candidate))
        .find(|path| runs_version(path))?;
        Some(Self {
            rust_analyzer,
            sysroot,
        })
    }

    fn bin(&self) -> PathBuf {
        self.sysroot.join("bin")
    }
}

/// An absolute path for `candidate`, searching PATH for a bare name.
fn absolute_executable(candidate: &Path) -> Option<PathBuf> {
    if candidate.components().count() > 1 {
        return candidate
            .is_file()
            .then(|| std::path::absolute(candidate).ok())
            .flatten();
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(candidate))
        .find(|path| path.is_absolute() && path.is_file())
}

fn runs_version(path: &Path) -> bool {
    std::process::Command::new(path)
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

/// Run a real-server test with no operator state: temporary HOME, XDG
/// config/cache/data/state and Cargo home, the resolved sysroot first on
/// PATH, and no ambient build wrappers or target dirs. A rustup proxy keeps
/// only the toolchain store and is pinned to the resolved toolchain.
fn isolate_real_server_env(env: &mut EnvGuard, base: &Path, toolchain: &Toolchain) {
    let rustup_home = env_path("RUSTUP_HOME")
        .or_else(|| dirs::home_dir().map(|home| home.join(".rustup")))
        .filter(|dir| dir.is_dir());
    let home = mkdir(&base.join("home"));
    env.set("HOME", &home);
    for (key, dir) in [
        ("XDG_CONFIG_HOME", ".config"),
        ("XDG_CACHE_HOME", ".cache"),
        ("XDG_DATA_HOME", ".local/share"),
        ("XDG_STATE_HOME", ".local/state"),
    ] {
        env.set(key, mkdir(&home.join(dir)));
    }
    env.set("CARGO_HOME", mkdir(&base.join("cargo-home")));
    env.remove("RUSTUP_HOME");
    env.remove("RUSTUP_TOOLCHAIN");
    if let Some(rustup_home) = rustup_home {
        if let (Some(toolchains), Some(name)) =
            (toolchain.sysroot.parent(), toolchain.sysroot.file_name())
            && toolchains == rustup_home.join("toolchains")
        {
            env.set("RUSTUP_TOOLCHAIN", name);
        }
        env.set("RUSTUP_HOME", rustup_home);
    }
    let bin = toolchain.bin();
    env.set("RUSTC", bin.join("rustc"));
    let path = std::env::var("PATH").unwrap_or_default();
    env.set("PATH", format!("{}:{path}", bin.display()));
    for key in [
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "CARGO_TARGET_DIR",
        "CARGO_BUILD_TARGET_DIR",
        "BRO_LSP_LANE_SHIM_DIR",
        "BRO_LSP_RA_TARGET_DIR",
        "BRO_LSP_RA_HOST_BUILD",
    ] {
        env.remove(key);
    }
    assert!(
        runs_version(&toolchain.rust_analyzer),
        "{} must run under the isolated environment",
        toolchain.rust_analyzer.display()
    );
}

fn real_config(toolchain: &Toolchain, cargo: &CargoRecorder) -> LspConfig {
    LspConfig {
        child_env: BTreeMap::from([("CARGO".into(), cargo.wrapper.display().to_string())]),
        request_timeout: Duration::from_secs(90),
        init_timeout: Duration::from_secs(90),
        ready_timeout: Duration::from_secs(60),
        rust_analyzer_bin: Some(toolchain.rust_analyzer.clone()),
        ..LspConfig::default()
    }
}

async fn send_saves(pool: &SessionPool, doc: &OpenDocument, count: usize) {
    let session = pool.session(doc.root.clone(), doc.language).await.unwrap();
    for _ in 0..count {
        session
            .lock()
            .await
            .send_notification::<DidSaveTextDocument>(&DidSaveTextDocumentParams {
                text_document: TextDocumentIdentifier {
                    uri: doc.uri.clone(),
                },
                text: None,
            })
            .await
            .unwrap();
        time::sleep(Duration::from_millis(500)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_rust_session_runs_the_controlled_flycheck() {
    let mut env = EnvGuard::new();
    let Some(toolchain) = Toolchain::resolve() else {
        eprintln!("skipping flycheck positive control: rust-analyzer not found");
        return;
    };
    let (_dir, base) = canonical_tempdir();
    isolate_real_server_env(&mut env, &base, &toolchain);
    let root = mkdir(&base.join("fixture"));
    let source = write_fixture(&root);
    assert!(
        !is_lane_host_build(&root),
        "positive control must be non-lane"
    );
    let cargo = CargoRecorder::new(&base, &toolchain);
    let mut config = real_config(&toolchain, &cargo);
    // Keep the control's analysis builds out of any shared target dir.
    config.child_env_scrub_keys.push("CARGO_TARGET_DIR".into());
    config.child_env.insert(
        "CARGO_TARGET_DIR".into(),
        base.join("target").display().to_string(),
    );
    let pool = SessionPool::new(config);

    let doc = pool
        .open_document(&root, Language::Rust, &source, 1, APP_SOURCE.into())
        .await
        .unwrap();
    let startup = cargo.wait_for_flychecks(1, Duration::from_secs(90)).await;
    assert!(
        startup >= 1,
        "default settings must run flycheck at startup; invocations: {:#?}",
        cargo.invocations()
    );
    // Let the startup check settle so a save is not coalesced into it.
    time::sleep(Duration::from_secs(2)).await;
    let before_save = cargo.flychecks();
    send_saves(&pool, &doc, 1).await;
    let after_save = cargo
        .wait_for_flychecks(before_save + 1, Duration::from_secs(30))
        .await;
    assert!(
        after_save > before_save,
        "default settings must run flycheck after didSave; invocations: {:#?}",
        cargo.invocations()
    );
    pool.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lane_rust_session_keeps_analysis_without_flycheck() {
    let mut env = EnvGuard::new();
    let Some(toolchain) = Toolchain::resolve() else {
        eprintln!("skipping lane flycheck test: rust-analyzer not found");
        return;
    };
    let (_dir, base) = canonical_tempdir();
    isolate_real_server_env(&mut env, &base, &toolchain);
    let target = mkdir(&base.join("ra-target"));
    env.set("BRO_LSP_RA_HOST_BUILD", "1");
    env.set("BRO_LSP_RA_TARGET_DIR", &target);
    let root = mkdir(&base.join("fixture"));
    let source = write_fixture(&root);
    assert!(is_lane_host_build(&root));
    let cargo = CargoRecorder::new(&base, &toolchain);
    let pool = SessionPool::new(real_config(&toolchain, &cargo));

    // The harness path: didOpen, then semantic requests once build data loads.
    let doc = pool
        .open_document(&root, Language::Rust, &source, 1, APP_SOURCE.into())
        .await
        .unwrap();
    for (position, signature) in [
        (Position::new(4, 6), "fn answer_from_macro() -> u32"),
        (Position::new(4, 28), "fn answer_from_build() -> u32"),
    ] {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let hover = pool
                .hover(&doc, position, Duration::from_secs(60))
                .await
                .unwrap();
            let text = serde_json::to_string(&hover).unwrap();
            if text.contains(signature) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "hover never resolved {signature} (proc-macro/build-script data); last: {text}"
            );
            time::sleep(Duration::from_millis(500)).await;
        }
    }
    let generated = std::fs::read_dir(target.join("debug/build"))
        .unwrap()
        .filter_map(|entry| entry.ok())
        .any(|entry| entry.path().join("out/generated.rs").is_file());
    assert!(
        generated,
        "build-script output must land in the host target dir {}",
        target.display()
    );

    // The harness path: didChange, with native diagnostics following it.
    let mut doc = doc;
    let broken = format!("{APP_SOURCE}\npub fn broken() -> u32 {{\n    \"not a number\"\n}}\n");
    pool.apply_change(&mut doc, 2, broken).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    let diagnostics = loop {
        let diagnostics = pool.diagnostics(&doc, 2).await.unwrap();
        if diagnostics
            .iter()
            .any(|diag| diag.severity == Some(DiagnosticSeverity::ERROR))
            || Instant::now() >= deadline
        {
            break diagnostics;
        }
        time::sleep(Duration::from_millis(250)).await;
    };
    assert!(
        diagnostics
            .iter()
            .any(|diag| diag.severity == Some(DiagnosticSeverity::ERROR)),
        "native type error expected without flycheck, got {diagnostics:#?}"
    );
    pool.apply_change(&mut doc, 3, APP_SOURCE.into())
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    let clean = loop {
        let diagnostics = pool.diagnostics(&doc, 3).await.unwrap();
        if diagnostics.is_empty() || Instant::now() >= deadline {
            break diagnostics;
        }
        time::sleep(Duration::from_millis(250)).await;
    };
    assert!(
        clean.is_empty(),
        "resolved macros and build output leave no diagnostics, got {clean:#?}"
    );

    // Saves are what trigger flycheck under defaults; none may run here.
    send_saves(&pool, &doc, 3).await;
    time::sleep(Duration::from_secs(5)).await;
    let invocations = cargo.invocations();
    assert_eq!(
        cargo.flychecks(),
        0,
        "lane sessions must not run automatic flycheck; invocations: {invocations:#?}"
    );
    pool.shutdown_all().await;
}
