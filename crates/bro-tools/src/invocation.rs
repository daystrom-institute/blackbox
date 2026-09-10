//! Completion ownership shared by flat and nested tool dispatch.
//!
//! Cancellation prevents queued work from starting. Admitted work owns its
//! admission guard until it returns an actual outcome. Cooperative tools stop
//! through ToolCx::cancellation; non-cooperative work can delay acknowledgement.

use std::sync::Arc;

use serde_json::Value;
use tokio::sync::{RwLock, watch};
use tokio_util::sync::CancellationToken;

use crate::{Tool, ToolCx, ToolResult, call_tool_with_arg_defaults};

/// A retained terminal outcome, independent of the caller's wait future.
#[derive(Clone)]
pub struct InvocationReceipt {
    result: watch::Receiver<Option<ToolResult>>,
}

impl InvocationReceipt {
    pub fn outcome(&self) -> Option<ToolResult> {
        self.result.borrow().clone()
    }

    pub async fn wait(&mut self) -> ToolResult {
        loop {
            if let Some(result) = self.outcome() {
                return result;
            }
            if self.result.changed().await.is_err() {
                return self.outcome().unwrap_or_else(|| {
                    ToolResult::Error("tool execution owner ended without an outcome".into())
                });
            }
        }
    }
}

/// Dropping the caller requests cancellation but never aborts active work.
/// Keep a receipt when the outcome must be collected after dropping this handle.
pub struct InvocationHandle {
    cancellation: CancellationToken,
    receipt: InvocationReceipt,
}

impl InvocationHandle {
    pub fn completed(result: ToolResult) -> Self {
        let (_, result) = watch::channel(Some(result));
        Self {
            cancellation: CancellationToken::new(),
            receipt: InvocationReceipt { result },
        }
    }

    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    pub fn receipt(&self) -> InvocationReceipt {
        self.receipt.clone()
    }

    pub async fn wait(&mut self) -> ToolResult {
        self.receipt.wait().await
    }
}

impl Drop for InvocationHandle {
    fn drop(&mut self) {
        if self.receipt.outcome().is_none() {
            self.cancel();
        }
    }
}

struct CompletionOwner {
    result: watch::Sender<Option<ToolResult>>,
}

impl Drop for CompletionOwner {
    fn drop(&mut self) {
        // A panicking tool must still close its receipt. Normal completion
        // already stored the actual outcome and leaves it untouched.
        if self.result.borrow().is_none() {
            self.result.send_replace(Some(ToolResult::Error(
                "tool execution task ended before returning an outcome".into(),
            )));
        }
    }
}

pub fn start_tool_invocation(
    tool: Arc<dyn Tool>,
    input: Value,
    mut cx: ToolCx,
    execution: Option<Arc<RwLock<()>>>,
) -> InvocationHandle {
    cx.cancellation = cx.cancellation.child_token();
    let cancellation = cx.cancellation.clone();
    let (result_tx, result_rx) = watch::channel(None);
    let completion = CompletionOwner { result: result_tx };
    // Host-owned process controls and operator status bypass workspace admission.
    // They own their synchronization and do not mutate workspace files.
    let execution = if matches!(
        tool.name(),
        "shell_poll" | "shell_kill" | "shell_list" | "report"
    ) {
        None
    } else {
        execution
    };
    tokio::spawn(async move {
        let result = run_owned(tool, input, cx, execution).await;
        completion.result.send_replace(Some(result));
    });
    InvocationHandle {
        cancellation,
        receipt: InvocationReceipt { result: result_rx },
    }
}

async fn run_owned(
    tool: Arc<dyn Tool>,
    input: Value,
    cx: ToolCx,
    execution: Option<Arc<RwLock<()>>>,
) -> ToolResult {
    let mut read_guard = None;
    let mut write_guard = None;
    if let Some(execution) = execution {
        if tool.annotations().read_only {
            read_guard = Some(tokio::select! {
                biased;
                _ = cx.cancellation.cancelled() => return cancelled_before_start(),
                guard = execution.read_owned() => guard,
            });
        } else {
            write_guard = Some(tokio::select! {
                biased;
                _ = cx.cancellation.cancelled() => return cancelled_before_start(),
                guard = execution.write_owned() => guard,
            });
        }
    }
    if cx.cancellation.is_cancelled() {
        return cancelled_before_start();
    }
    let result = call_tool_with_arg_defaults(tool.as_ref(), tool.name(), input, &cx).await;
    drop((read_guard, write_guard));
    result
}

fn cancelled_before_start() -> ToolResult {
    ToolResult::Error("tool invocation cancelled before execution".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::oneshot;

    fn cx(root: &std::path::Path) -> ToolCx {
        ToolCx {
            tool_observations: Default::default(),
            instruction_generation: 0,
            instruction_policy: None,
            root: root.to_owned(),
            cancellation: Default::default(),
            safety: Arc::new(crate::SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: Arc::new(Mutex::new(Default::default())),
            shell_sessions: Arc::new(Mutex::new(Default::default())),
            edits: Arc::new(Mutex::new(Default::default())),
            session_env: Arc::new(Default::default()),
            output_budget: 16 * 1024,
            child_env: Arc::new(Default::default()),
            shell_env: Arc::new(Default::default()),
            tool_arg_defaults: Arc::new(Default::default()),
        }
    }

    struct BlockingMutation {
        started: Mutex<Option<oneshot::Sender<()>>>,
        release: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    }

    #[async_trait::async_trait]
    impl Tool for BlockingMutation {
        fn name(&self) -> &str {
            "blocking_mutation"
        }
        fn description(&self) -> &str {
            "Blocking mutation fixture"
        }
        fn input_schema(&self) -> Value {
            serde_json::json!({"type":"object"})
        }
        async fn call(&self, _: Value, cx: &ToolCx) -> ToolResult {
            let started = self.started.lock().unwrap().take().unwrap();
            let release = self.release.lock().unwrap().take().unwrap();
            let path = cx.root.join("committed.txt");
            crate::tool::call_blocking(move || {
                started.send(()).unwrap();
                release.recv().unwrap();
                std::fs::write(path, "actual mutation").unwrap();
                ToolResult::Text("mutation completed".into())
            })
            .await
        }
    }

    struct Counter(Arc<AtomicUsize>);

    #[async_trait::async_trait]
    impl Tool for Counter {
        fn name(&self) -> &str {
            "counter"
        }
        fn description(&self) -> &str {
            "Admission fixture"
        }
        fn input_schema(&self) -> Value {
            serde_json::json!({"type":"object"})
        }
        async fn call(&self, _: Value, _: &ToolCx) -> ToolResult {
            self.0.fetch_add(1, Ordering::SeqCst);
            ToolResult::Text("ran".into())
        }
    }

    #[tokio::test]
    async fn dropped_waiter_retains_mutation_guard_and_actual_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let cx = cx(&root);
        let gate = Arc::new(RwLock::new(()));
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let handle = start_tool_invocation(
            Arc::new(BlockingMutation {
                started: Mutex::new(Some(started_tx)),
                release: Mutex::new(Some(release_rx)),
            }),
            serde_json::json!({}),
            cx.clone(),
            Some(gate.clone()),
        );
        let mut receipt = handle.receipt();
        started_rx.await.unwrap();
        drop(handle);
        assert!(receipt.outcome().is_none());
        assert!(
            gate.try_write().is_err(),
            "active blocking work still owns admission"
        );

        let count = Arc::new(AtomicUsize::new(0));
        let mut queued = start_tool_invocation(
            Arc::new(Counter(count.clone())),
            serde_json::json!({}),
            cx,
            Some(gate.clone()),
        );
        queued.cancel();
        let queued_result = tokio::time::timeout(std::time::Duration::from_secs(2), queued.wait())
            .await
            .unwrap();
        assert!(matches!(queued_result, ToolResult::Error(e) if e.contains("before execution")));
        assert_eq!(count.load(Ordering::SeqCst), 0);
        assert!(!root.join("committed.txt").exists());

        release_tx.send(()).unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), receipt.wait())
            .await
            .unwrap();
        assert!(matches!(result, ToolResult::Text(ref value) if value == "mutation completed"));
        assert_eq!(
            std::fs::read_to_string(root.join("committed.txt")).unwrap(),
            "actual mutation"
        );
        assert!(gate.try_write().is_ok());
        assert!(
            matches!(receipt.wait().await, ToolResult::Text(value) if value == "mutation completed")
        );
    }

    struct Cooperative {
        started: Mutex<Option<oneshot::Sender<()>>>,
    }

    #[async_trait::async_trait]
    impl Tool for Cooperative {
        fn name(&self) -> &str {
            "cooperative"
        }
        fn description(&self) -> &str {
            "Cancellation cleanup fixture"
        }
        fn input_schema(&self) -> Value {
            serde_json::json!({"type":"object"})
        }
        async fn call(&self, _: Value, cx: &ToolCx) -> ToolResult {
            self.started
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .send(())
                .unwrap();
            cx.cancellation.cancelled().await;
            tokio::task::yield_now().await;
            ToolResult::Json(serde_json::json!({"cancelled":true,"cleanup_complete":true}))
        }
    }

    #[tokio::test]
    async fn cancellation_waits_for_cooperative_cleanup_and_retains_its_result() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (started_tx, started_rx) = oneshot::channel();
        let mut handle = start_tool_invocation(
            Arc::new(Cooperative {
                started: Mutex::new(Some(started_tx)),
            }),
            serde_json::json!({}),
            cx(&root),
            Some(Arc::new(RwLock::new(()))),
        );
        started_rx.await.unwrap();
        handle.cancel();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), handle.wait())
            .await
            .unwrap();
        assert!(matches!(result, ToolResult::Json(v) if v["cleanup_complete"] == true));
    }
}
