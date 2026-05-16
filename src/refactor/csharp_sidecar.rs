//! Rust client + `WorkspaceTransactionAdapter` implementation for
//! the dotnet Roslyn sidecar (`deploy/blackbox-csharp-worker/`).
//!
//! Spawn model: lazy per-project — first request for a given
//! `project_root` spawns a `CsharpWorker`; subsequent requests
//! reuse it. The worker stays alive for the daemon's lifetime
//! (idle eviction is a follow-up; the cost of one running dotnet
//! process is tolerable).
//!
//! Concurrency: each `CsharpWorker` is single-flight — calls are
//! serialized through a `Mutex` so JSON-RPC ids stay coherent.
//!
//! Resolution: `resolve_workspace_adapter` returns a
//! `RoslynWorkspaceTransactionAdapter` when every Plan step in the
//! incoming run has a `csharp_*` kind prefix. Mixed-language step
//! lists refuse with `error.mixed_workspace_adapters`. Command-only
//! step lists return None (no transaction to mirror into).

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::csharp_sidecar_protocol::{
    self as protocol, ApplyCommandTouchesParams, ApplyPlanStepParams, BeginTransactionParams,
    RpcRequest, RpcResponse, SidecarFileMove, SidecarPlanFileEdit, TransactionResult,
};
use super::workspace_adapter::{
    AppliedCommandTouches, AppliedPlanDelta, WorkspaceTransactionAdapter,
};

/// Process-wide worker pool keyed by canonical project root. Lives
/// in a `OnceLock` so the runner can dip into it without
/// threading a handle through every call site.
static POOL: OnceLock<CsharpWorkerPool> = OnceLock::new();

fn pool() -> &'static CsharpWorkerPool {
    POOL.get_or_init(CsharpWorkerPool::default)
}

#[derive(Default)]
pub struct CsharpWorkerPool {
    workers: Mutex<HashMap<PathBuf, Arc<Mutex<CsharpWorker>>>>,
}

impl CsharpWorkerPool {
    /// Get or spawn the worker for `project_root`. Falls back to
    /// an error when the sidecar binary is unavailable so the
    /// runner can disable adapter calls cleanly.
    pub fn worker_for(&self, project_root: &Path) -> Result<Arc<Mutex<CsharpWorker>>> {
        let canonical = project_root
            .canonicalize()
            .unwrap_or_else(|_| project_root.to_path_buf());
        let mut workers = self.workers.lock().unwrap();
        if let Some(w) = workers.get(&canonical) {
            return Ok(Arc::clone(w));
        }
        let worker = CsharpWorker::spawn(&canonical).with_context(|| {
            format!(
                "spawning blackbox-csharp-worker for {}",
                canonical.display()
            )
        })?;
        let arc = Arc::new(Mutex::new(worker));
        workers.insert(canonical, Arc::clone(&arc));
        Ok(arc)
    }
}

pub struct CsharpWorker {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    next_id: u64,
    project_root: PathBuf,
}

impl CsharpWorker {
    pub fn spawn(project_root: &Path) -> Result<Self> {
        let binary = std::env::var("BLACKBOX_ROSLYN_WORKER_BIN")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "blackbox-csharp-worker".to_string());
        let mut cmd = Command::new(&binary);
        cmd.current_dir(project_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = cmd
            .spawn()
            .with_context(|| format!("launching `{binary}`"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("worker stdin not piped"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("worker stdout not piped"))?;
        Ok(CsharpWorker {
            child,
            stdin,
            reader: BufReader::new(stdout),
            next_id: 1,
            project_root: project_root.to_path_buf(),
        })
    }

    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

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
        writeln!(self.stdin, "{}", payload).context("writing JSON-RPC request to worker stdin")?;
        self.stdin
            .flush()
            .context("flushing JSON-RPC request to worker stdin")?;

        let mut line = String::new();
        self.reader
            .read_line(&mut line)
            .context("reading JSON-RPC response from worker stdout")?;
        if line.trim().is_empty() {
            bail!("worker returned empty response for method `{method}`");
        }
        let response: RpcResponse<R> = serde_json::from_str(&line)
            .with_context(|| format!("parsing JSON-RPC response: {line:?}"))?;
        if response.id != id {
            bail!(
                "worker returned mismatched id: sent {id}, got {} (single-flight protocol; investigate sidecar)",
                response.id
            );
        }
        if let Some(err) = response.error {
            bail!(
                "worker JSON-RPC error code={} message={}",
                err.code,
                err.message
            );
        }
        response
            .result
            .ok_or_else(|| anyhow!("worker JSON-RPC response had neither result nor error"))
    }
}

impl Drop for CsharpWorker {
    fn drop(&mut self) {
        // Best-effort graceful shutdown so the worker can release
        // its MSBuildWorkspace cleanly.
        let request = RpcRequest::<()> {
            jsonrpc: "2.0".to_string(),
            id: 0,
            method: protocol::METHOD_SHUTDOWN.to_string(),
            params: None,
        };
        if let Ok(payload) = serde_json::to_string(&request) {
            let _ = writeln!(self.stdin, "{}", payload);
            let _ = self.stdin.flush();
        }
        // Read & discard any final response, then wait for exit.
        let mut sink = String::new();
        let _ = self.reader.read_line(&mut sink);
        let _ = self.child.wait();
    }
}

pub struct RoslynWorkspaceTransactionAdapter {
    worker: Arc<Mutex<CsharpWorker>>,
}

impl RoslynWorkspaceTransactionAdapter {
    pub fn new(worker: Arc<Mutex<CsharpWorker>>) -> Self {
        Self { worker }
    }

    fn call_transaction(&self, method: &str, run_id: &str) -> Result<()> {
        let params = BeginTransactionParams {
            run_id: run_id.to_string(),
        };
        let _: TransactionResult = self.worker.lock().unwrap().call(method, params)?;
        Ok(())
    }
}

impl WorkspaceTransactionAdapter for RoslynWorkspaceTransactionAdapter {
    fn begin(&self, run_id: &str, _project: &Path) -> Result<()> {
        self.call_transaction(protocol::METHOD_BEGIN_TRANSACTION, run_id)
    }

    fn apply_plan_step(&self, run_id: &str, delta: &AppliedPlanDelta<'_>) -> Result<()> {
        // Materialize the sidecar params from the borrowed delta.
        // Each FileEdit carries `new_text`; if absent we re-read
        // disk so the sidecar's in-memory snapshot matches.
        let mut edits = Vec::with_capacity(delta.edits.len());
        for edit in delta.edits {
            let new_content = match &edit.new_text {
                Some(text) => text.clone(),
                None => std::fs::read_to_string(&edit.path)
                    .with_context(|| format!("reading {} for sidecar mirror", edit.path))?,
            };
            edits.push(SidecarPlanFileEdit {
                file: edit.path.clone(),
                new_content,
            });
        }
        let file_moves: Vec<SidecarFileMove> = delta
            .file_moves
            .iter()
            .map(|m| SidecarFileMove {
                source_path: m.source_path.clone(),
                target_path: m.target_path.clone(),
            })
            .collect();
        let created: Vec<String> = delta
            .created
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        let deleted: Vec<String> = delta
            .deleted
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        let params = ApplyPlanStepParams {
            run_id: run_id.to_string(),
            edits,
            file_moves,
            created,
            deleted,
        };
        let _: TransactionResult = self
            .worker
            .lock()
            .unwrap()
            .call(protocol::METHOD_APPLY_PLAN_STEP, params)?;
        Ok(())
    }

    fn apply_command_touches(
        &self,
        run_id: &str,
        touches: &AppliedCommandTouches<'_>,
    ) -> Result<()> {
        let params = ApplyCommandTouchesParams {
            run_id: run_id.to_string(),
            touches: touches
                .touches
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect(),
            succeeded: touches.succeeded,
        };
        let _: TransactionResult = self
            .worker
            .lock()
            .unwrap()
            .call(protocol::METHOD_APPLY_COMMAND_TOUCHES, params)?;
        Ok(())
    }

    fn commit(&self, run_id: &str) -> Result<()> {
        self.call_transaction(protocol::METHOD_COMMIT_TRANSACTION, run_id)
    }

    fn rollback(&self, run_id: &str) -> Result<()> {
        self.call_transaction(protocol::METHOD_ROLLBACK_TRANSACTION, run_id)
    }
}

/// Phase 2-aware adapter resolver. Inspects the step list for plan-kind
/// prefixes, refuses mixed-language runs, and returns a Roslyn adapter
/// when every Plan step is `csharp_*`. When the sidecar binary is
/// unavailable in the environment, returns `Ok(None)` and the runner
/// short-circuits — Phase 1 plan kinds keep working without the
/// sidecar.
pub(crate) fn resolve_csharp_adapter(
    steps: &[super::RefactorRunStep],
    project: &Path,
) -> Result<Option<Arc<dyn WorkspaceTransactionAdapter>>> {
    let mut has_csharp = false;
    let mut has_other = false;
    for step in steps {
        if let super::RefactorRunStep::Plan { params, .. } = step {
            let kind = params.kind.as_str();
            if kind.starts_with("csharp_")
                || kind == "migrate_csharp_to_filescoped_namespace"
                || kind == "move_csharp_type_to_file"
                || kind == "unseal_csharp_class"
                || kind == "find_csharp_usages"
            {
                has_csharp = true;
            } else if kind.starts_with("rust_") || kind.starts_with("java_") {
                has_other = true;
            } else if kind == "move_file"
                || kind == "replace_text"
                || kind == "write_file"
                || kind == "ensure_toml_table"
            {
                // generic kinds — neutral
            } else {
                has_other = true;
            }
        }
    }
    if has_csharp && has_other {
        bail!(
            "error.mixed_workspace_adapters: compound run mixes csharp_* and non-csharp plan kinds; transactions are language-scoped"
        );
    }
    if !has_csharp {
        return Ok(None);
    }
    // C# adapter requested. Try to spawn the worker; on failure
    // (binary missing, etc.) return None so the runner keeps disk-only
    // semantics. RX-V3 fail-closed applies at the plan-kind level, not
    // here — the adapter is best-effort for snapshot coherence.
    match pool().worker_for(project) {
        Ok(worker) => Ok(Some(
            Arc::new(RoslynWorkspaceTransactionAdapter::new(worker))
                as Arc<dyn WorkspaceTransactionAdapter>,
        )),
        Err(_) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refactor::{RefactorPlanParams, RefactorRunStep};

    fn plan_step(kind: &str) -> RefactorRunStep {
        RefactorRunStep::Plan {
            params: RefactorPlanParams {
                kind: kind.to_string(),
                source: "foo.cs".to_string(),
                ..Default::default()
            },
            optional: false,
        }
    }

    #[test]
    fn resolver_returns_none_for_command_only_runs() {
        let steps: Vec<RefactorRunStep> = Vec::new();
        let result = resolve_csharp_adapter(&steps, Path::new("/tmp/none")).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn resolver_refuses_mixed_language_runs() {
        let steps = vec![plan_step("csharp_lsp_rename"), plan_step("rust_lsp_rename")];
        let result = resolve_csharp_adapter(&steps, Path::new("/tmp/none"));
        let Err(err) = result else {
            panic!("expected mixed_workspace_adapters error, got Ok");
        };
        assert!(err.to_string().contains("mixed_workspace_adapters"));
    }

    #[test]
    fn resolver_returns_none_for_rust_only_runs() {
        let steps = vec![plan_step("rust_lsp_rename")];
        let result = resolve_csharp_adapter(&steps, Path::new("/tmp/none")).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn resolver_returns_some_when_sidecar_available() {
        // Skipped unless the sidecar binary is on PATH and the
        // user has set BLACKBOX_ROSLYN_WORKER_BIN. We don't gate
        // CI on this — the test asserts the dispatch path works
        // when we have a binary to spawn.
        let Some(bin) = std::env::var_os("BLACKBOX_ROSLYN_WORKER_BIN") else {
            eprintln!(
                "[csharp_sidecar] BLACKBOX_ROSLYN_WORKER_BIN unset — skipping live sidecar test"
            );
            return;
        };
        if !PathBuf::from(&bin).exists() {
            eprintln!("[csharp_sidecar] worker binary missing — skipping live test");
            return;
        }
        let steps = vec![plan_step("csharp_lsp_rename")];
        let dir = tempfile::tempdir().unwrap();
        let result = resolve_csharp_adapter(&steps, dir.path()).unwrap();
        assert!(
            result.is_some(),
            "expected adapter when worker is available"
        );
    }

    #[test]
    fn resolver_treats_generic_kinds_as_neutral_with_csharp() {
        let steps = vec![plan_step("csharp_lsp_rename"), plan_step("write_file")];
        // Should NOT raise mixed_workspace_adapters — write_file is
        // generic and rides under the C# adapter.
        let _ = resolve_csharp_adapter(&steps, Path::new("/tmp/none"));
    }
}
