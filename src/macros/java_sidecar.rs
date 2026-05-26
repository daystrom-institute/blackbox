//! Rust client for the Java macro sidecar (`blackbox-java-worker`).
//!
//! Spawn model: lazy per-project — first request for a given `project_root`
//! spawns a `JavaWorker`; subsequent requests reuse it. The worker stays alive
//! for the daemon's lifetime.
//!
//! Concurrency: each `JavaWorker` is single-flight — calls are serialized
//! through a `Mutex` so JSON-RPC ids stay coherent.
//!
//! Resolution: `JavaWorkerPool::worker_for` returns an `Arc<Mutex<JavaWorker>>`
//! when the JAR path is configured and the worker spawns successfully.
//!
//! Fail-closed invariant (mirrors RX-V3): when the worker is unavailable
//! (JAR absent, JVM missing, spawn fails, capabilities mismatch), every method
//! returns an `error.backend_unavailable` error. The caller never silently
//! downgrades to unverified template output.
//!
//! # Environment knobs
//!
//! - `BLACKBOX_JAVA_WORKER_JAR` — absolute path to the sidecar JAR. When
//!   unset or empty and no built-in default is present, the pool is unavailable.
//! - `BLACKBOX_JAVA_BIN` — path to the `java` executable (default `"java"`).

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::java_sidecar_protocol::{
    self as protocol, GetCapabilitiesParams, GetCapabilitiesResult, RpcRequest, RpcResponse,
    ShutdownParams,
};

/// Required `protocol_version` value in the `getCapabilities` response.
/// Any other value fails closed (RX-V3 mirror).
const REQUIRED_PROTOCOL_VERSION: u32 = 1;

/// Required Java major version for the sidecar JVM.
///
/// The worker JAR bundles `rewrite-java-21`, which uses JDK-internal
/// `com.sun.tools.javac` APIs from JDK 21. Running the process on any
/// other JDK major version causes universal `NoClassDefFoundError` from
/// every parse call. When OpenRewrite ships a `rewrite-java-NN` module
/// and the pom.xml dependency is updated, this constant must be updated
/// to match so the fail-closed check stays accurate.
const REQUIRED_JAVA_MAJOR: u32 = 21;

// ── Process-wide worker pool ──────────────────────────────────────────────────

/// Process-wide worker pool keyed by canonical project root. Lives in a
/// `OnceLock` so the sidecar backend can dip into it without threading a
/// handle through every call site.
static POOL: OnceLock<JavaWorkerPool> = OnceLock::new();

pub(crate) fn pool() -> &'static JavaWorkerPool {
    POOL.get_or_init(JavaWorkerPool::default)
}

// ── JavaWorkerPool ────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct JavaWorkerPool {
    workers: Mutex<HashMap<PathBuf, Arc<Mutex<JavaWorker>>>>,
}

impl JavaWorkerPool {
    /// Get or spawn the worker for `project_root`.
    ///
    /// Returns `Err` when:
    /// - `BLACKBOX_JAVA_WORKER_JAR` is unset/empty (fail closed — no JAR, no worker).
    /// - The JAR path does not exist on disk.
    /// - The `java` binary is missing or spawn fails.
    /// - `getCapabilities` fails or returns `protocol_version != 1`.
    /// - `getCapabilities` reports a Java major version other than `REQUIRED_JAVA_MAJOR`
    ///   (the bundled `rewrite-java-21` parser requires JDK 21).
    pub fn worker_for(&self, project_root: &Path) -> Result<Arc<Mutex<JavaWorker>>> {
        let canonical = project_root
            .canonicalize()
            .unwrap_or_else(|_| project_root.to_path_buf());
        let mut workers = self.workers.lock().unwrap();
        if let Some(w) = workers.get(&canonical) {
            return Ok(Arc::clone(w));
        }
        let worker = JavaWorker::spawn(&canonical).with_context(|| {
            format!(
                "spawning blackbox-java-worker for {}",
                canonical.display()
            )
        })?;
        let arc = Arc::new(Mutex::new(worker));
        workers.insert(canonical, Arc::clone(&arc));
        Ok(arc)
    }
}

// ── JavaWorker ────────────────────────────────────────────────────────────────

pub struct JavaWorker {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    next_id: u64,
    project_root: PathBuf,
}

impl JavaWorker {
    /// Spawn a Java worker for `project_root`.
    ///
    /// Reads `BLACKBOX_JAVA_WORKER_JAR` for the JAR path and
    /// `BLACKBOX_JAVA_BIN` for the java executable (default `"java"`).
    ///
    /// Immediately calls `getCapabilities` after spawn; returns `Err` when:
    /// - JAR env var is unset or empty.
    /// - JAR file does not exist.
    /// - `java` binary is missing or spawn fails.
    /// - `getCapabilities` call fails.
    /// - `protocol_version` != 1.
    pub fn spawn(project_root: &Path) -> Result<Self> {
        // Resolve JAR path from env. Fail closed when unset or empty.
        let jar_path = std::env::var("BLACKBOX_JAVA_WORKER_JAR")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                anyhow!(
                    "error.backend_unavailable: BLACKBOX_JAVA_WORKER_JAR is not set; \
                     the Java macro sidecar JAR path must be configured"
                )
            })?;

        // Verify the JAR exists before attempting spawn.
        let jar = PathBuf::from(&jar_path);
        if !jar.exists() {
            bail!(
                "error.backend_unavailable: Java worker JAR not found at '{}' \
                 (BLACKBOX_JAVA_WORKER_JAR); check the path",
                jar_path
            );
        }

        let java_bin = std::env::var("BLACKBOX_JAVA_BIN")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "java".to_string());

        // Build the command. NEVER use shell; args are typed constants only.
        let mut cmd = Command::new(&java_bin);
        cmd.arg("-jar")
            .arg(&jar)
            .current_dir(project_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        let mut child = cmd.spawn().with_context(|| {
            format!(
                "launching `{java_bin} -jar {jar_path}`; \
                 ensure `{java_bin}` is on PATH and the JAR is readable"
            )
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("java worker stdin not piped"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("java worker stdout not piped"))?;

        let mut worker = JavaWorker {
            child,
            stdin,
            reader: BufReader::new(stdout),
            next_id: 1,
            project_root: project_root.to_path_buf(),
        };

        // Immediately validate capabilities — fail closed on any issue.
        let caps: GetCapabilitiesResult = worker
            .call(protocol::METHOD_GET_CAPABILITIES, GetCapabilitiesParams {})
            .with_context(|| {
                "getCapabilities call to Java worker failed immediately after spawn; \
                 the JAR may not speak the expected JSON-RPC protocol"
            })?;

        if caps.protocol_version != REQUIRED_PROTOCOL_VERSION {
            bail!(
                "error.backend_unavailable: Java worker returned protocol_version={} but \
                 this client requires protocol_version={}; upgrade the worker JAR",
                caps.protocol_version,
                REQUIRED_PROTOCOL_VERSION
            );
        }

        // Validate JVM major version. The bundled rewrite-java-21 parser uses
        // JDK-internal javac APIs and must run on exactly JDK 21. Any other
        // major version will produce a universal NoClassDefFoundError on every
        // parse() call with no indication of the root cause at the call site.
        let reported_major = java_major_version(&caps.java_version).ok_or_else(|| {
            anyhow!(
                "error.backend_unavailable: Java worker reported unrecognisable \
                 java_version '{}'; cannot verify JVM compatibility",
                caps.java_version
            )
        })?;
        if reported_major != REQUIRED_JAVA_MAJOR {
            bail!(
                "error.backend_unavailable: Java macro sidecar is running on Java {} but \
                 OpenRewrite's bundled rewrite-java-21 parser requires Java {}; \
                 set BLACKBOX_JAVA_BIN to a JDK {} 'java' binary \
                 (e.g. /usr/lib/jvm/java-21-openjdk/bin/java)",
                reported_major,
                REQUIRED_JAVA_MAJOR,
                REQUIRED_JAVA_MAJOR,
            );
        }

        Ok(worker)
    }

    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    /// Send a typed JSON-RPC request and return the typed result.
    ///
    /// Single-flight: serialized through the enclosing `Mutex`. Callers
    /// must hold the lock for the duration of the call.
    pub fn call<P, R>(&mut self, method: &str, params: P) -> Result<R>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        let id = self.next_id;
        self.next_id += 1;
        let request = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id,
            method: method.to_string(),
            params: Some(params),
        };
        let payload = serde_json::to_string(&request)?;
        writeln!(self.stdin, "{}", payload)
            .context("writing JSON-RPC request to java worker stdin")?;
        self.stdin
            .flush()
            .context("flushing JSON-RPC request to java worker stdin")?;

        let mut line = String::new();
        self.reader
            .read_line(&mut line)
            .context("reading JSON-RPC response from java worker stdout")?;
        if line.trim().is_empty() {
            bail!("java worker returned empty response for method `{method}`");
        }
        let response: RpcResponse<R> = serde_json::from_str(&line)
            .with_context(|| format!("parsing JSON-RPC response: {line:?}"))?;
        if response.id != id {
            bail!(
                "java worker returned mismatched id: sent {id}, got {} \
                 (single-flight protocol; investigate sidecar)",
                response.id
            );
        }
        if let Some(err) = response.error {
            bail!(
                "java worker JSON-RPC error code={} message={}",
                err.code,
                err.message
            );
        }
        response
            .result
            .ok_or_else(|| anyhow!("java worker JSON-RPC response had neither result nor error"))
    }
}

impl Drop for JavaWorker {
    fn drop(&mut self) {
        // Best-effort graceful shutdown so the worker can release resources.
        let request = RpcRequest::<ShutdownParams> {
            jsonrpc: "2.0".to_string(),
            id: 0,
            method: protocol::METHOD_SHUTDOWN.to_string(),
            params: Some(ShutdownParams {}),
        };
        if let Ok(payload) = serde_json::to_string(&request) {
            let _ = writeln!(self.stdin, "{}", payload);
            let _ = self.stdin.flush();
        }
        // Read and discard any final response, then wait for exit.
        let mut sink = String::new();
        let _ = self.reader.read_line(&mut sink);
        let _ = self.child.wait();
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Parse the major Java version from a version string.
///
/// Handles the standard `"MAJOR.minor.patch"` form (e.g. `"21.0.11"` → `21`)
/// and the bare-major form (e.g. `"21"` → `21`). Returns `None` when the
/// string is empty or the first segment is not a valid `u32`.
fn java_major_version(version: &str) -> Option<u32> {
    version.split('.').next()?.parse().ok()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn java_major_version_parses_correctly() {
        assert_eq!(java_major_version("21.0.11"), Some(21));
        assert_eq!(java_major_version("21"), Some(21));
        assert_eq!(java_major_version("26"), Some(26));
        assert_eq!(java_major_version("17.0.2"), Some(17));
        assert_eq!(java_major_version("11.0.0"), Some(11));
        assert_eq!(java_major_version(""), None);
        assert_eq!(java_major_version("not-a-version"), None);
        // Edge: trailing dot is still parseable — first segment "21" is valid
        assert_eq!(java_major_version("21."), Some(21));
    }

    #[test]
    fn pool_worker_for_fails_when_jar_unset() {
        // Ensure BLACKBOX_JAVA_WORKER_JAR is not set for this test.
        // We use a temp env override to avoid disturbing the process env
        // in a racy way — but since we can't set per-thread env easily in
        // std, we rely on the CI/test environment not having this var set.
        // The test documents the fail-closed behaviour when JAR is absent.
        //
        // If BLACKBOX_JAVA_WORKER_JAR IS set in the environment, this test
        // will only exercise the jar-exists check; if the jar also exists the
        // test is skipped to avoid spawning a real worker.
        if let Ok(jar) = std::env::var("BLACKBOX_JAVA_WORKER_JAR") {
            if !jar.trim().is_empty() && PathBuf::from(&jar).exists() {
                eprintln!(
                    "[java_sidecar] BLACKBOX_JAVA_WORKER_JAR is set and exists — \
                     skipping unavailability test (live worker would succeed)"
                );
                return;
            }
        }
        let pool = JavaWorkerPool::default();
        let result = pool.worker_for(Path::new("/tmp/nonexistent-project"));
        let Err(err) = result else {
            panic!("expected error when JAR is unavailable, got Ok");
        };
        // Use the alternate formatter to surface the full cause chain — the outermost
        // context ("spawning blackbox-java-worker for …") masks the inner markers when
        // using plain to_string(). The production SidecarBackend adds its own
        // error.backend_unavailable prefix, so this is a test-only concern.
        let msg = format!("{err:#}");
        // The error chain should contain a backend-unavailable marker.
        assert!(
            msg.contains("backend_unavailable") || msg.contains("BLACKBOX_JAVA_WORKER_JAR"),
            "expected backend_unavailable in error; got: {msg}"
        );
    }

    #[test]
    fn live_worker_test_skips_when_jar_absent() {
        // Mirror of csharp_sidecar's resolver_returns_some_when_sidecar_available.
        // This test is a no-op unless BLACKBOX_JAVA_WORKER_JAR points to a real jar.
        let Some(jar) = std::env::var_os("BLACKBOX_JAVA_WORKER_JAR") else {
            eprintln!(
                "[java_sidecar] BLACKBOX_JAVA_WORKER_JAR unset — skipping live worker test"
            );
            return;
        };
        if !PathBuf::from(&jar).exists() {
            eprintln!("[java_sidecar] worker JAR missing — skipping live test");
            return;
        }
        let pool = JavaWorkerPool::default();
        let dir = tempfile::tempdir().unwrap();
        let result = pool.worker_for(dir.path());
        assert!(
            result.is_ok(),
            "expected worker when JAR is available; got: {:?}",
            result.err()
        );
    }
}
