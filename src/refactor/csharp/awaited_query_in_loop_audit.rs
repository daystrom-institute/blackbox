//! `csharp_awaited_query_in_loop_audit` — analysis-only IOperation walk.
//!
//! Walks the IOperation tree exposed by the sidecar's `getOperations`
//! and reports every `IAwaitOperation` that sits *inside* the body
//! of an `IForEachLoopOperation` or `IForLoopOperation`. Classifies
//! each finding as either:
//!   - `per_iteration_await` — the await is inside the loop body
//!     (the actual N+1 risk).
//!   - `loop_collection_await` — the await is in the loop's
//!     collection expression (single call returning a collection;
//!     not an N+1).
//!
//! v1 input shape: pass `source` (relative .cs path) and
//! `item_names[0]` (method simple name). Sidecar walks the method
//! body and returns the operation tree; we classify the result.
//!
//! RX-V3 fail-closed on missing sidecar.

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::refactor::RefactorPlanParams;
use crate::refactor::csharp_sidecar::CsharpWorkerPool;
use crate::refactor::csharp_sidecar_protocol::{
    GetOperationsParams, GetOperationsResult, METHOD_GET_OPERATIONS, OperationNode,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AwaitedQuerySite {
    pub line: u32,
    pub character: u32,
    pub classification: String, // "per_iteration_await" | "loop_collection_await"
    pub target_full_name: Option<String>,
    pub loop_kind: String, // "ForEachLoop" | "ForLoop"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AwaitedQueryReport {
    pub kind: String,
    pub project_dir: String,
    pub source: String,
    pub method: String,
    pub findings: Vec<AwaitedQuerySite>,
    pub per_iteration_count: usize,
    pub collection_await_count: usize,
}

pub fn plan_awaited_query_audit(p: &RefactorPlanParams) -> Result<String> {
    let project_dir = p
        .project_dir
        .as_deref()
        .ok_or_else(|| anyhow!("project_dir is required for csharp_awaited_query_in_loop_audit"))?;
    let project_root = PathBuf::from(project_dir);
    let source_rel = p.source.clone();
    let source_path = if PathBuf::from(&source_rel).is_absolute() {
        PathBuf::from(&source_rel)
    } else {
        project_root.join(&source_rel)
    };
    let method = p
        .item_names
        .as_deref()
        .and_then(|n| n.first())
        .map(String::as_str)
        .ok_or_else(|| anyhow!("item_names[0] (method name) is required"))?;

    let pool = CsharpWorkerPool::default();
    let worker = pool.worker_for(&project_root).map_err(|e| {
        anyhow!(
            "error.lsp_unavailable: csharp_awaited_query_in_loop_audit requires the Roslyn sidecar (RX-V3); {e}"
        )
    })?;
    let result: GetOperationsResult = worker
        .lock()
        .unwrap()
        .call(
            METHOD_GET_OPERATIONS,
            GetOperationsParams {
                file: source_path
                    .to_str()
                    .ok_or_else(|| anyhow!("source path not valid UTF-8"))?
                    .to_string(),
                method_name: method.to_string(),
            },
        )
        .map_err(|e| anyhow!("error.lsp_unavailable: getOperations failed: {e}"))?;

    let mut findings = Vec::new();
    for op in &result.operations {
        walk_for_loops(op, &mut findings);
    }

    let per_iteration = findings
        .iter()
        .filter(|f| f.classification == "per_iteration_await")
        .count();
    let collection_await = findings
        .iter()
        .filter(|f| f.classification == "loop_collection_await")
        .count();
    let report = AwaitedQueryReport {
        kind: "csharp_awaited_query_in_loop_audit".to_string(),
        project_dir: project_dir.to_string(),
        source: source_path.to_string_lossy().to_string(),
        method: method.to_string(),
        findings,
        per_iteration_count: per_iteration,
        collection_await_count: collection_await,
    };
    Ok(serde_json::to_string_pretty(&report)?)
}

fn walk_for_loops(node: &OperationNode, out: &mut Vec<AwaitedQuerySite>) {
    let is_loop = matches!(node.kind.as_str(), "ForEachLoop" | "ForLoop");
    if is_loop {
        // The first child is the collection expression; the rest are
        // body statements. The sidecar emits children in source order,
        // so we treat children[0] as the collection.
        let (collection, body) = if !node.children.is_empty() {
            (Some(&node.children[0]), &node.children[1..])
        } else {
            (None, &node.children[..])
        };
        if let Some(c) = collection {
            collect_awaits(c, &node.kind, "loop_collection_await", out);
        }
        for stmt in body {
            collect_awaits(stmt, &node.kind, "per_iteration_await", out);
        }
    }
    for child in &node.children {
        walk_for_loops(child, out);
    }
    let _ = collect_awaits;
}

fn collect_awaits(
    node: &OperationNode,
    loop_kind: &str,
    classification: &'static str,
    out: &mut Vec<AwaitedQuerySite>,
) {
    if node.kind == "Await" {
        // The await wraps an Invocation. Walk one level to find it.
        let target = node
            .children
            .iter()
            .find(|c| c.kind == "Invocation")
            .and_then(|c| c.target_full_name.clone());
        out.push(AwaitedQuerySite {
            line: node.line,
            character: node.character,
            classification: classification.to_string(),
            target_full_name: target,
            loop_kind: loop_kind.to_string(),
        });
    }
    for child in &node.children {
        collect_awaits(child, loop_kind, classification, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn await_node(line: u32, target: Option<&str>) -> OperationNode {
        OperationNode {
            kind: "Await".to_string(),
            line,
            character: 0,
            end_line: line,
            end_character: 0,
            target_full_name: None,
            children: vec![OperationNode {
                kind: "Invocation".to_string(),
                line,
                character: 0,
                end_line: line,
                end_character: 0,
                target_full_name: target.map(String::from),
                children: vec![],
            }],
        }
    }

    fn for_each(collection: OperationNode, body: Vec<OperationNode>) -> OperationNode {
        let mut children = vec![collection];
        children.extend(body);
        OperationNode {
            kind: "ForEachLoop".to_string(),
            line: 1,
            character: 0,
            end_line: 10,
            end_character: 0,
            target_full_name: None,
            children,
        }
    }

    #[test]
    fn flags_per_iteration_await() {
        let collection = OperationNode {
            kind: "LocalReference".to_string(),
            line: 1,
            character: 0,
            end_line: 1,
            end_character: 0,
            target_full_name: None,
            children: vec![],
        };
        let body_stmt = await_node(2, Some("FooStore.AnyAsync"));
        let loop_node = for_each(collection, vec![body_stmt]);
        let mut findings = Vec::new();
        walk_for_loops(&loop_node, &mut findings);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].classification, "per_iteration_await");
        assert_eq!(
            findings[0].target_full_name.as_deref(),
            Some("FooStore.AnyAsync")
        );
    }

    #[test]
    fn distinguishes_collection_await_from_body_await() {
        let collection = await_node(1, Some("Store.GetAllAsync"));
        let body_stmt = OperationNode {
            kind: "VariableDeclaration".to_string(),
            line: 2,
            character: 0,
            end_line: 2,
            end_character: 0,
            target_full_name: None,
            children: vec![],
        };
        let loop_node = for_each(collection, vec![body_stmt]);
        let mut findings = Vec::new();
        walk_for_loops(&loop_node, &mut findings);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].classification, "loop_collection_await");
    }
}
