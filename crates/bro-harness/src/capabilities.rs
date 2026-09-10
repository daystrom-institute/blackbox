//! Harness-local capability adapters.
//!
//! The daemon runs the harness as an independent process, so daemon-owned
//! corpus and atom capabilities arrive through its MCP catalog. This module
//! retains only the generic [`HostTools`] seam used to project the already
//! filtered session tool set into code-mode cells.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bro_capabilities::{ToolCallOutput, ToolCapability, ToolInvocation};
use bro_core::BroError;
use bro_tools::{Tool, ToolCx, ToolResult};

/// The generic host built-in tool seam: a code-mode cell's `tools.*` call
/// dispatches here, and this runs the matching bro-tools built-in by name
/// against the per-session [`ToolCx`] — the same `Tool::call` path the flat
/// model-facing surface uses.
///
/// Deny-filter invariant: the callable set is gated by the **same** `ToolFilter`
/// as the flat surface (an unfiltered in-box surface would be a deny-bypass). The
/// caller constructs `HostTools` from the already-filtered built-in set, so a
/// denied capability is absent here and fails closed.
pub struct HostTools {
    tools: HashMap<String, Arc<dyn Tool>>,
    cx: ToolCx,
    execution: Arc<tokio::sync::RwLock<()>>,
}

impl HostTools {
    /// Build the host-tool seam from a pre-filtered built-in set + the session
    /// context. `filtered_builtins` MUST already have had the session's
    /// `ToolFilter` applied by the caller; capability/control tools
    /// (`exec`, `wait`, `report`, …) are intentionally NOT
    /// included — they are model-facing controls, not nested cell tools.
    pub fn new(filtered_builtins: Vec<Arc<dyn Tool>>, cx: ToolCx) -> Self {
        Self::with_dispatch_gate(
            filtered_builtins,
            cx,
            Arc::new(tokio::sync::RwLock::new(())),
        )
    }

    /// Share admission with flat tools so work from a yielded cell cannot race
    /// a subsequent direct invocation.
    pub fn with_dispatch_gate(
        filtered_builtins: Vec<Arc<dyn Tool>>,
        cx: ToolCx,
        execution: Arc<tokio::sync::RwLock<()>>,
    ) -> Self {
        let tools = filtered_builtins
            .into_iter()
            .map(|t| (t.name().to_string(), t))
            .collect();
        Self {
            tools,
            cx,
            execution,
        }
    }
}

#[async_trait]
impl ToolCapability for HostTools {
    async fn call_tool(&self, invocation: ToolInvocation) -> Result<ToolCallOutput, BroError> {
        let tool = self.tools.get(&invocation.name).ok_or_else(|| {
            // Unknown OR filtered-out → fail closed (no in-box route around the
            // ToolFilter, §4.5).
            BroError::new(
                "tool_unavailable",
                format!(
                    "host tool '{}' is not available in-box (unknown or denied)",
                    invocation.name
                ),
            )
        })?;
        // Cells may launch calls with Promise.all. Treat mutations as exclusive
        // barriers and allow declared reads to overlap, just like flat dispatch.
        // Acquire before applying defaults so the whole invocation is protected.
        let (_read_guard, _write_guard) = if tool.annotations().read_only {
            (Some(self.execution.read().await), None)
        } else {
            (None, Some(self.execution.write().await))
        };
        let (content, is_error, content_type) = match crate::registry::call_tool_with_arg_defaults(
            tool.as_ref(),
            &invocation.name,
            invocation.input_json,
            &self.cx,
        )
        .await
        {
            ToolResult::Text(t) => (t, false, "text/plain"),
            ToolResult::Json(v) => (
                serde_json::to_string(&v).unwrap_or_else(|_| v.to_string()),
                false,
                "application/json",
            ),
            ToolResult::Error(e) => (e, true, "text/plain"),
        };
        Ok(ToolCallOutput {
            content,
            is_error,
            content_type: content_type.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_cx() -> ToolCx {
        use std::sync::Mutex;
        // A minimal context is sufficient for host-tool projection tests.
        ToolCx {
            root: std::env::temp_dir(),
            safety: Arc::new(bro_tools::SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: Arc::new(Mutex::new(bro_tools::TodoList::default())),
            shell_sessions: Arc::new(Mutex::new(bro_tools::ShellSessions::default())),
            edits: Arc::new(Mutex::new(bro_tools::EditSink::default())),
            session_env: Arc::new(std::collections::BTreeMap::new()),
            tool_arg_defaults: Arc::new(bro_tools::ToolArgDefaults::default()),
            shell_env: Arc::new(Default::default()),
        }
    }

    #[tokio::test]
    async fn host_tools_filtered_set_fails_closed_on_denied() {
        // HostTools built from a filtered built-in set: file_read survives, but a
        // tool excluded by the filter (e.g. shell_run denied) is absent → calling
        // it in-box fails closed (no deny-bypass, §4.5).
        let filter = crate::mcp::ToolFilter::from_csv(Some("shell_run"), None);
        let allowed: Vec<Arc<dyn Tool>> = bro_tools::builtin_tools()
            .into_iter()
            .filter(|t| filter.permits(t.name()))
            .collect();
        let host = HostTools::new(allowed, test_cx());

        // file_read is permitted (no real file needed — it returns a tool error
        // for a missing path, which is is_error=true, NOT tool_unavailable).
        let read = host
            .call_tool(ToolInvocation {
                name: "file_read".to_string(),
                input_json: json!({ "file_path": "definitely-missing.xyz" }),
            })
            .await
            .expect("file_read is in the filtered set");
        assert!(read.is_error, "missing file → tool-level error");

        // shell_run was denied → absent from the in-box set → fail closed.
        let denied = host
            .call_tool(ToolInvocation {
                name: "shell_run".to_string(),
                input_json: json!({ "command": "echo nope" }),
            })
            .await;
        let err = denied.expect_err("denied tool must fail closed");
        assert_eq!(err.code, "tool_unavailable");
    }

    struct ConcurrencyProbe {
        read_only: bool,
        active: Arc<std::sync::atomic::AtomicUsize>,
        peak: Arc<std::sync::atomic::AtomicUsize>,
        rendezvous: Option<Arc<tokio::sync::Barrier>>,
    }

    #[async_trait]
    impl Tool for ConcurrencyProbe {
        fn name(&self) -> &str {
            "probe"
        }
        fn description(&self) -> &str {
            "Test nested dispatch concurrency"
        }
        fn input_schema(&self) -> serde_json::Value {
            json!({"type": "object"})
        }
        fn annotations(&self) -> bro_tools::ToolAnnotations {
            bro_tools::ToolAnnotations {
                read_only: self.read_only,
                destructive: false,
            }
        }
        async fn call(&self, _: serde_json::Value, _: &ToolCx) -> ToolResult {
            use std::sync::atomic::Ordering;
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(active, Ordering::SeqCst);
            if let Some(rendezvous) = &self.rendezvous {
                rendezvous.wait().await;
            } else {
                // Without a host barrier the sibling enters during this yield.
                tokio::task::yield_now().await;
            }
            self.active.fetch_sub(1, Ordering::SeqCst);
            ToolResult::Text("finished".into())
        }
    }

    async fn nested_concurrency_peak(read_only: bool) -> usize {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let mut cx = test_cx();
        cx.root = dir.path().canonicalize().unwrap();
        let peak = Arc::new(AtomicUsize::new(0));
        let host = HostTools::new(
            vec![Arc::new(ConcurrencyProbe {
                read_only,
                active: Arc::new(AtomicUsize::new(0)),
                peak: peak.clone(),
                rendezvous: read_only.then(|| Arc::new(tokio::sync::Barrier::new(2))),
            })],
            cx,
        );
        let invoke = || {
            host.call_tool(ToolInvocation {
                name: "probe".into(),
                input_json: json!({}),
            })
        };
        let (first, second) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::join!(invoke(), invoke())
        })
        .await
        .expect("nested dispatch should finish without deadlock");
        assert_eq!(first.unwrap().content, "finished");
        assert_eq!(second.unwrap().content, "finished");
        peak.load(Ordering::SeqCst)
    }

    #[tokio::test]
    async fn nested_mutations_are_exclusive() {
        assert_eq!(nested_concurrency_peak(false).await, 1);
    }

    #[tokio::test]
    async fn nested_reads_can_overlap() {
        assert_eq!(nested_concurrency_peak(true).await, 2);
    }

    struct GateProbe {
        name: &'static str,
        started: Arc<std::sync::atomic::AtomicUsize>,
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl Tool for GateProbe {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "Test shared dispatch admission"
        }
        fn input_schema(&self) -> serde_json::Value {
            json!({"type": "object"})
        }
        async fn call(&self, input: serde_json::Value, _: &ToolCx) -> ToolResult {
            self.started
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if input["block"] == true {
                self.release.notified().await;
            }
            ToolResult::Text(self.name.into())
        }
    }

    #[tokio::test]
    async fn live_nested_mutation_blocks_flat_mutation_but_not_cell_controls() {
        use crate::registry::{PinPolicy, Registry};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let mut cx = test_cx();
        cx.root = dir.path().canonicalize().unwrap();
        let gate = Arc::new(tokio::sync::RwLock::new(()));
        let started = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(tokio::sync::Notify::new());
        let mutation: Arc<dyn Tool> = Arc::new(GateProbe {
            name: "mutation",
            started: started.clone(),
            release: release.clone(),
        });
        let host = HostTools::with_dispatch_gate(vec![mutation.clone()], cx.clone(), gate.clone());
        let mut flat_tools = vec![mutation];
        for name in [
            bro_code_mode::PUBLIC_TOOL_NAME,
            bro_code_mode::WAIT_TOOL_NAME,
        ] {
            flat_tools.push(Arc::new(GateProbe {
                name,
                started: Arc::new(AtomicUsize::new(0)),
                release: release.clone(),
            }));
        }
        let mut registry = Registry::new(
            flat_tools,
            vec![],
            &PinPolicy::from_env(),
            &crate::mcp::ToolFilter::default(),
        );
        registry.set_dispatch_gate(gate);
        let mut nested = Box::pin(host.call_tool(ToolInvocation {
            name: "mutation".into(),
            input_json: json!({"block": true}),
        }));
        assert!(futures_util::poll!(&mut nested).is_pending());
        assert_eq!(started.load(Ordering::SeqCst), 1);
        let mut flat = Box::pin(registry.dispatch("mutation", json!({}), &cx));
        assert!(futures_util::poll!(&mut flat).is_pending());
        assert_eq!(
            started.load(Ordering::SeqCst),
            1,
            "flat mutation must await the nested mutation"
        );
        for name in [
            bro_code_mode::PUBLIC_TOOL_NAME,
            bro_code_mode::WAIT_TOOL_NAME,
        ] {
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                registry.dispatch(name, json!({}), &cx),
            )
            .await
            .expect("cell controls must bypass the shared gate");
            assert_eq!(result.into_content(), (name.into(), false));
        }
        release.notify_one();
        assert!(!nested.await.unwrap().is_error);
        assert_eq!(flat.await.into_content(), ("mutation".into(), false));
        assert_eq!(started.load(Ordering::SeqCst), 2);
    }
}
