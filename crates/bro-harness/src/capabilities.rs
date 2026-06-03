//! In-process capability bindings (harness-daemon-boundary.md §2/§6).
//!
//! When the daemon runs the harness in-process it *installs* concrete
//! [`bro_capabilities`] implementations backed by its in-memory stores. The
//! harness then exposes them as ordinary [`Tool`]s whose `call` is a direct
//! trait dispatch — no MCP round-trip, no wire serialization for blackbox's own
//! surfaces.
//!
//! The standalone `bro-harness` binary never installs anything, so the slot
//! stays empty and capability-backed tools are simply not registered: the
//! fail-closed behaviour the boundary doc requires (§2 — "the standalone binary
//! injects absent impls → corpus capabilities fail closed").

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use bro_capabilities::{CorpusCapability, CorpusLookup};
use bro_tools::{Tool, ToolCx, ToolResult};
use serde_json::{Value, json};

/// Process-global corpus capability slot. The daemon is a singleton, so a
/// single installed implementation is the whole story; standalone leaves it
/// `None`. Pushed by the daemon (`blackbox` → `bro-harness`), never pulled —
/// the harness keeps no dependency on the daemon.
static CORPUS: RwLock<Option<Arc<dyn CorpusCapability>>> = RwLock::new(None);

/// Install the daemon's in-memory corpus implementation. Called once, at daemon
/// startup, from the `blackbox` crate. Last writer wins.
pub fn install_corpus(capability: Arc<dyn CorpusCapability>) {
    *CORPUS.write().expect("corpus capability slot poisoned") = Some(capability);
}

fn corpus() -> Option<Arc<dyn CorpusCapability>> {
    CORPUS
        .read()
        .expect("corpus capability slot poisoned")
        .clone()
}

/// Capability-backed tools to merge into the registry. Empty when nothing was
/// installed (standalone harness) → corpus tools fail closed by absence.
pub fn capability_tools() -> Vec<Arc<dyn Tool>> {
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    if let Some(c) = corpus() {
        tools.push(Arc::new(CorpusSearchTool(c)));
    }
    tools
}

/// `corpus_search`: ranked transcript/corpus lookup via a direct in-memory
/// trait call. This is the §6 "skip the wire" path — the model still speaks
/// JSON, but the result never crosses an MCP transport.
struct CorpusSearchTool(Arc<dyn CorpusCapability>);

#[async_trait]
impl Tool for CorpusSearchTool {
    fn name(&self) -> &str {
        "corpus_search"
    }

    fn description(&self) -> &str {
        "Search the indexed transcript/corpus in-process (no MCP round-trip). \
         Returns ranked hits, each with an id and an excerpt."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query." },
                "limit": {
                    "type": "integer",
                    "description": "Maximum hits to return (default 10, max 100).",
                    "minimum": 1
                }
            },
            "required": ["query"]
        })
    }

    async fn call(&self, input: Value, _cx: &ToolCx) -> ToolResult {
        let query = match input.get("query").and_then(Value::as_str) {
            Some(q) if !q.trim().is_empty() => q.to_string(),
            _ => return ToolResult::Error("corpus_search: `query` is required".into()),
        };
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(10)
            .clamp(1, 100) as usize;
        match self.0.search_corpus(CorpusLookup { query, limit }).await {
            Ok(hits) => ToolResult::Json(json!({
                "hits": hits
                    .iter()
                    .map(|h| json!({ "id": h.id, "text": h.text }))
                    .collect::<Vec<_>>(),
            })),
            Err(e) => {
                ToolResult::Error(format!("corpus_search failed: {}: {}", e.code, e.message))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bro_capabilities::{CapabilityResult, CorpusHit};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Stub capability that records invocations — proves the tool dispatches to
    /// the injected trait object, not over any wire.
    struct StubCorpus {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl CorpusCapability for StubCorpus {
        async fn search_corpus(&self, lookup: CorpusLookup) -> CapabilityResult<Vec<CorpusHit>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![CorpusHit {
                id: format!("hit-for-{}", lookup.query),
                text: format!("excerpt (limit {})", lookup.limit),
            }])
        }
    }

    fn test_cx() -> ToolCx {
        use std::sync::Mutex;
        // corpus_search ignores cx, so a minimal context is sufficient.
        ToolCx {
            root: std::env::temp_dir(),
            safety: Arc::new(bro_tools::SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: Arc::new(Mutex::new(bro_tools::TodoList::default())),
            shell_sessions: Arc::new(Mutex::new(bro_tools::ShellSessions::default())),
            promises: Arc::new(Mutex::new(bro_tools::PromiseStore::default())),
            clipboard: Arc::new(Mutex::new(bro_tools::Registers::default())),
            edits: Arc::new(Mutex::new(bro_tools::EditSink::default())),
        }
    }

    #[tokio::test]
    async fn corpus_search_tool_dispatches_to_injected_capability() {
        let stub = Arc::new(StubCorpus {
            calls: AtomicUsize::new(0),
        });
        let tool = CorpusSearchTool(stub.clone());
        let result = tool
            .call(json!({ "query": "boundary", "limit": 3 }), &test_cx())
            .await;
        // The trait was actually invoked in-process — no MCP round-trip.
        assert_eq!(stub.calls.load(Ordering::SeqCst), 1);
        match result {
            ToolResult::Json(v) => {
                let hits = v.get("hits").and_then(Value::as_array).expect("hits array");
                assert_eq!(hits.len(), 1);
                assert_eq!(hits[0]["id"], "hit-for-boundary");
                assert_eq!(hits[0]["text"], "excerpt (limit 3)");
            }
            other => panic!("expected Json result, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn corpus_search_requires_query() {
        let tool = CorpusSearchTool(Arc::new(StubCorpus {
            calls: AtomicUsize::new(0),
        }));
        let result = tool.call(json!({ "limit": 3 }), &test_cx()).await;
        assert!(matches!(result, ToolResult::Error(_)));
    }
}
