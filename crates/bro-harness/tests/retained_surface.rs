#![allow(clippy::disallowed_methods)]

//! The standalone harness can use native tools and an admitted corpus catalog
//! without any atom or workflow capabilities being present on that server.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bro_capabilities::{ToolCapability, ToolInvocation};
use bro_harness::capabilities::HostTools;
use bro_harness::mcp::{
    McpConfig, McpServerConfig, McpSurface, McpToolSpec, ToolFilter, ToolPlacementMap,
    load_mcp_tools_from_config_with_capability_aliases, split_mcp_tools_by_placement,
};
use bro_harness::registry::{PinPolicy, Registry};
use bro_tools::{ToolCx, ToolResult};
use serde_json::{Value, json};

const NATIVE_BODY: &str = "Retained native file evidence: café 語.\n";

#[derive(Default)]
struct CorpusSurface {
    calls: Mutex<Vec<(String, Value)>>,
    unavailable: bool,
}

#[async_trait]
impl McpSurface for CorpusSurface {
    async fn list_tools(&self) -> anyhow::Result<Vec<McpToolSpec>> {
        if self.unavailable {
            anyhow::bail!("fixture capability server unavailable");
        }
        Ok(["bbox_corpus_search", "bbox_reindex"]
            .into_iter()
            .map(|name| McpToolSpec {
                name: name.into(),
                description: name.into(),
                input_schema: json!({"type": "object"}),
            })
            .collect())
    }

    async fn call_tool(&self, tool: &str, input: Value) -> anyhow::Result<ToolResult> {
        self.calls.lock().unwrap().push((tool.into(), input));
        if tool != "bbox_corpus_search" {
            anyhow::bail!("non-corpus tool must not be dispatched in this fixture");
        }
        Ok(ToolResult::Json(corpus_evidence()))
    }
}

fn corpus_evidence() -> Value {
    json!({"hits": [{"id": "knowledge:retained-example", "text": "Exact corpus evidence 語"}]})
}

fn fixture_context(root: &Path) -> ToolCx {
    ToolCx {
        root: root.into(),
        safety: Arc::new(bro_tools::SafetyPolicy::new()),
        http: reqwest::Client::new(),
        todos: Arc::new(Mutex::new(bro_tools::TodoList::default())),
        shell_sessions: Arc::new(Mutex::new(bro_tools::ShellSessions::default())),
        edits: Arc::new(Mutex::new(bro_tools::EditSink::default())),
        session_env: Arc::new(BTreeMap::new()),
        tool_arg_defaults: Arc::new(bro_tools::ToolArgDefaults::default()),
        shell_env: Arc::new(Default::default()),
    }
}

async fn load_surfaces(
    server: Option<Arc<CorpusSurface>>,
    filter: &ToolFilter,
    cx: &ToolCx,
) -> (Registry, HostTools) {
    let config = McpConfig {
        servers: server
            .into_iter()
            .map(|server| McpServerConfig::InProcess {
                name: "blackbox".into(),
                server,
            })
            .collect(),
        tool_placement: ToolPlacementMap::new(),
    };
    let mcp =
        load_mcp_tools_from_config_with_capability_aliases(&config, filter, Some("blackbox")).await;
    let (in_box, out_box) = split_mcp_tools_by_placement(&mcp, &config.tool_placement);
    let builtins = bro_tools::builtin_tools();
    let mut callable = builtins
        .iter()
        .filter(|tool| filter.permits(tool.name()))
        .cloned()
        .collect::<Vec<_>>();
    callable.extend(in_box);
    callable.extend(out_box.iter().cloned());
    (
        Registry::new(builtins, out_box, &PinPolicy::from_env(), filter),
        HostTools::new(callable, cx.clone()),
    )
}

async fn assert_native_read(registry: &Registry, host: &HostTools, cx: &ToolCx) {
    let input = json!({"file_path": "evidence.txt"});
    match registry.dispatch("file_read", input.clone(), cx).await {
        ToolResult::Text(text) => assert!(text.contains(NATIVE_BODY.trim_end())),
        other => panic!("native flat read failed: {other:?}"),
    }
    let nested = host
        .call_tool(ToolInvocation {
            name: "file_read".into(),
            input_json: input,
        })
        .await
        .unwrap();
    assert!(!nested.is_error);
    assert!(nested.content.contains(NATIVE_BODY.trim_end()));
}

#[tokio::test]
async fn corpus_only_catalog_dispatches_without_atom_or_workflow_capabilities() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(root.join("evidence.txt"), NATIVE_BODY).unwrap();
    let cx = fixture_context(&root);
    let server = Arc::new(CorpusSurface::default());
    let filter = ToolFilter::from_csv(Some("mcp__blackbox__bbox_reindex"), None);
    let (registry, host) = load_surfaces(Some(server.clone()), &filter, &cx).await;

    assert!(!registry.contains("atom_invoke"));
    assert!(!registry.contains("mcp__blackbox__atom_invoke"));
    assert!(!registry.contains("mcp__blackbox__bro_orchestrate_run"));
    assert!(!registry.contains("mcp__blackbox__bbox_reindex"));
    assert_native_read(&registry, &host, &cx).await;
    let input = json!({"query": "retained corpus 語", "limit": 1});
    for name in ["corpus_search", "mcp__blackbox__bbox_corpus_search"] {
        match registry.dispatch(name, input.clone(), &cx).await {
            ToolResult::Json(value) => assert_eq!(value, corpus_evidence()),
            other => panic!("corpus flat dispatch failed: {other:?}"),
        }
        let nested = host
            .call_tool(ToolInvocation {
                name: name.into(),
                input_json: input.clone(),
            })
            .await
            .unwrap();
        assert!(!nested.is_error);
        assert_eq!(nested.content_type, "application/json");
        assert_eq!(
            serde_json::from_str::<Value>(&nested.content).unwrap(),
            corpus_evidence()
        );
    }
    assert_eq!(
        *server.calls.lock().unwrap(),
        vec![("bbox_corpus_search".into(), input); 4],
        "both projected names and call surfaces must preserve server method and arguments"
    );
}

#[tokio::test]
async fn denied_or_unavailable_corpus_never_disables_native_tools_or_fabricates_aliases() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(root.join("evidence.txt"), NATIVE_BODY).unwrap();
    let cx = fixture_context(&root);
    let denied = Arc::new(CorpusSurface::default());
    let unavailable = Arc::new(CorpusSurface {
        unavailable: true,
        ..Default::default()
    });
    for (server, filter) in [
        (None, ToolFilter::default()),
        (Some(unavailable.clone()), ToolFilter::default()),
        (
            Some(denied.clone()),
            ToolFilter::from_csv(Some("mcp__blackbox__bbox_*"), None),
        ),
    ] {
        let (registry, host) = load_surfaces(server, &filter, &cx).await;
        assert_native_read(&registry, &host, &cx).await;
        for name in [
            "corpus_search",
            "mcp__blackbox__bbox_corpus_search",
            "atom_invoke",
        ] {
            assert!(!registry.contains(name));
            assert!(matches!(
                registry.dispatch(name, json!({}), &cx).await,
                ToolResult::Error(_)
            ));
            let error = host
                .call_tool(ToolInvocation {
                    name: name.into(),
                    input_json: json!({}),
                })
                .await
                .expect_err("absent or denied source must not have a callable cell alias");
            assert_eq!(error.code, "tool_unavailable");
        }
    }
    assert!(denied.calls.lock().unwrap().is_empty());
    assert!(unavailable.calls.lock().unwrap().is_empty());
}
