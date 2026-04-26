//! Workflow DSL — protocol-level descriptions of actor interaction over time.
//!
//! A workflow file carries two things: structured metadata (actors, nodes,
//! gates, retry policies) and an embedded mermaid state-diagram that
//! describes control flow. The mermaid is the shadow of the metadata —
//! renders anywhere, human-inspectable — and the loader cross-validates
//! that the two agree.
//!
//! The execution loop (`bro orchestrate run`) is separate; this module is
//! parsing + validation only.

pub mod context;
pub mod engine;
pub mod extractor;
pub mod mermaid;
pub mod ops;
pub mod schema;
pub mod wait;

pub use engine::{
    run_workflow, run_workflow_streaming, run_workflow_streaming_with_vars,
    run_workflow_with_initial_vars, WorkflowRunResult,
};
pub use mermaid::{parse_mermaid, MermaidGraph, MermaidNodeKind};
#[cfg(test)]
pub use mermaid::{MermaidEdge, MermaidNode};
pub use schema::{load_workflow, ActorKind, ActorSpec, GateMode, NodeMode, Workflow};
#[cfg(test)]
pub use schema::{InjectPolicy, LateInject, NodeSpec, RetryPolicy};

use anyhow::{anyhow, bail, Result};
use std::collections::HashSet;

/// A workflow that has been loaded AND cross-validated against its embedded
/// mermaid graph. Produced by [`compile`].
#[derive(Debug)]
pub struct CompiledWorkflow {
    pub spec: Workflow,
    pub graph: MermaidGraph,
}

/// Parse the mermaid graph out of `spec.graph` and cross-validate that the
/// topology agrees with the metadata. Errors name the specific mismatch.
pub fn compile(spec: Workflow) -> Result<CompiledWorkflow> {
    let graph = parse_mermaid(&spec.graph)?;
    cross_validate(&spec, &graph)?;
    Ok(CompiledWorkflow { spec, graph })
}

fn cross_validate(spec: &Workflow, graph: &MermaidGraph) -> Result<()> {
    // Every activity node in the graph must have a NodeSpec.
    // Every NodeSpec must appear in the graph.
    let graph_activities: HashSet<&str> = graph
        .nodes
        .iter()
        .filter(|n| matches!(n.kind, MermaidNodeKind::Activity))
        .map(|n| n.id.as_str())
        .collect();
    let spec_nodes: HashSet<&str> = spec.nodes.keys().map(String::as_str).collect();

    let graph_only: Vec<&&str> = graph_activities.difference(&spec_nodes).collect();
    if !graph_only.is_empty() {
        bail!("graph references activity nodes with no metadata: {graph_only:?}");
    }
    let spec_only: Vec<&&str> = spec_nodes.difference(&graph_activities).collect();
    if !spec_only.is_empty() {
        bail!("metadata declares nodes not reachable in graph: {spec_only:?}");
    }

    // Every NodeSpec.actor must be a declared actor — UNLESS the node
    // has a subworkflow, in which case the sub-spec carries its own
    // actors and the parent-level actor field is just a breadcrumb.
    for (node_id, node) in &spec.nodes {
        if node.subworkflow.is_some() && node.subworkflow_ref.is_some() {
            bail!(
                "node '{node_id}' has BOTH subworkflow (inline) and subworkflow_ref — pick one"
            );
        }
        if node.subworkflow.is_some() {
            // Recursively compile the sub-workflow so errors surface
            // at parent-compile time, not at dispatch time.
            if let Some(sub) = &node.subworkflow {
                compile((**sub).clone()).map_err(|e| {
                    anyhow!("subworkflow on node '{node_id}' failed to compile: {e}")
                })?;
            }
            continue;
        }
        if node.subworkflow_ref.is_some() {
            // Validation deferred to dispatch time — registry might
            // not contain the referenced workflow yet at parent
            // install time, and we don't want install ordering to be
            // load-bearing.
            continue;
        }
        if node.wait.is_some() {
            // Wait nodes don't need actors. Engine handles them.
            continue;
        }
        if node.actor.is_empty() {
            bail!("node '{node_id}' has no actor, no wait, and no subworkflow — at least one is required");
        }
        if !spec.actors.contains_key(&node.actor) {
            bail!(
                "node '{node_id}' references undeclared actor '{}'",
                node.actor
            );
        }
    }

    // Every late_inject.from must reference a real graph node.
    for (node_id, node) in &spec.nodes {
        if let Some(li) = &node.late_inject {
            let exists = graph.nodes.iter().any(|n| n.id == li.from);
            if !exists {
                bail!(
                    "node '{node_id}' late_inject.from='{}' does not exist in graph",
                    li.from
                );
            }
            if !spec.nodes.contains_key(&li.from) {
                bail!(
                    "node '{node_id}' late_inject.from='{}' has no metadata",
                    li.from
                );
            }
        }
    }

    // Graph must have a start edge (from [*]) and at least one terminal edge
    // (to [*]). Without these you can't determine entry/exit points.
    let has_start = graph.edges.iter().any(|e| e.from == "[*]");
    let has_end = graph.edges.iter().any(|e| e.to == "[*]");
    if !has_start {
        bail!("graph has no start edge — expected `[*] --> <node>`");
    }
    if !has_end {
        bail!("graph has no end edge — expected `<node> --> [*]`");
    }

    // Fork node semantics: a fork has one incoming and >= 2 outgoing.
    // The first outgoing listed is the sync-continuation; the rest are
    // fire-and-forget branches. We don't enforce the ordering here but
    // we do sanity-check that fork nodes have ≥ 2 outgoing edges.
    for node in &graph.nodes {
        if matches!(node.kind, MermaidNodeKind::Fork) {
            let out_count = graph.edges.iter().filter(|e| e.from == node.id).count();
            if out_count < 2 {
                bail!(
                    "fork node '{}' has {} outgoing edges; need >= 2",
                    node.id,
                    out_count
                );
            }
        }
    }

    Ok(())
}

impl CompiledWorkflow {
    /// Human-readable dry-run summary of what the workflow will do on a
    /// fresh run. Useful for eyeballing a workflow before dispatching it.
    pub fn summarize(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "workflow: {} (v{})\n",
            self.spec.name, self.spec.version
        ));
        out.push_str("\nactors:\n");
        let mut actor_keys: Vec<&String> = self.spec.actors.keys().collect();
        actor_keys.sort();
        for actor_name in actor_keys {
            let actor = &self.spec.actors[actor_name];
            let backing = match (&actor.brofile, &actor.team) {
                (Some(b), _) => format!("brofile={b}"),
                (_, Some(t)) => format!("team={t}"),
                _ => "user".into(),
            };
            out.push_str(&format!(
                "  {actor_name}: kind={:?} {backing} durable={} anchor={}\n",
                actor.kind, actor.durable, actor.compaction_anchor
            ));
        }

        out.push_str("\nnodes (graph order):\n");
        let order = self.topological_order();
        for node_id in &order {
            if let Some(spec) = self.spec.nodes.get(node_id) {
                out.push_str(&format!("  {node_id} → actor={}", spec.actor));
                if let Some(g) = &spec.gate {
                    out.push_str(&format!(" gate={g}"));
                }
                if let Some(r) = &spec.retry {
                    out.push_str(&format!(" retry_max_gen={}", r.max_generations));
                }
                if let Some(li) = &spec.late_inject {
                    out.push_str(&format!(" late_inject_from={}({:?})", li.from, li.policy));
                }
                if matches!(spec.mode, NodeMode::FireAndForget) {
                    out.push_str(" [fire-and-forget]");
                }
                out.push('\n');
            } else if node_id == "[*]" {
                continue;
            } else {
                out.push_str(&format!("  {node_id} (control node)\n"));
            }
        }

        out.push_str("\nedges:\n");
        for e in &self.graph.edges {
            let label = e
                .label
                .as_deref()
                .map(|l| format!(" :{l}"))
                .unwrap_or_default();
            out.push_str(&format!("  {} → {}{label}\n", e.from, e.to));
        }
        out
    }

    /// Best-effort topological order starting from [*]. Back-edges (loops)
    /// are broken by visited-set. Control nodes (fork/choice) appear
    /// inline where their predecessor sits.
    fn topological_order(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut visited: HashSet<String> = HashSet::new();
        let mut stack: Vec<String> = Vec::new();
        stack.push("[*]".into());
        while let Some(cur) = stack.pop() {
            if !visited.insert(cur.clone()) {
                continue;
            }
            out.push(cur.clone());
            // Push children in reverse so original order pops first
            let mut children: Vec<String> = self
                .graph
                .edges
                .iter()
                .filter(|e| e.from == cur)
                .map(|e| e.to.clone())
                .collect();
            children.reverse();
            for c in children {
                if !visited.contains(&c) {
                    stack.push(c);
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optimistic_workflow_loads_and_validates() {
        let json = include_str!("../../examples/workflows/optimistic.json");
        let spec = load_workflow(json).expect("parse optimistic spec");
        let compiled = compile(spec).expect("compile optimistic workflow");

        assert_eq!(compiled.spec.name, "optimistic-review");
        assert!(compiled.spec.actors.contains_key("executor"));
        assert!(compiled.spec.actors.contains_key("ensemble_durable"));
        assert!(compiled.spec.nodes.contains_key("P2_Executor"));

        let summary = compiled.summarize();
        assert!(summary.contains("optimistic-review"));
        assert!(summary.contains("late_inject_from=Ensemble_Durable_Review_1"));
        assert!(summary.contains("[fire-and-forget]"));
        // Emit the summary when tests run with --nocapture so humans can
        // eyeball the dry-run shape.
        eprintln!("\n=== OPTIMISTIC WORKFLOW DRY-RUN ===\n{summary}");
    }

    #[test]
    fn blind_workflow_loads_and_validates() {
        let json = include_str!("../../examples/workflows/blind.json");
        let spec = load_workflow(json).expect("parse blind spec");
        let compiled = compile(spec).expect("compile blind workflow");

        assert_eq!(compiled.spec.name, "blind-converge");
        assert!(compiled.spec.nodes.contains_key("Exec_Propose"));
        assert!(compiled.spec.nodes.contains_key("Exec_Work"));

        // Blind workflow uses a choice node for convergence
        let has_choice = compiled
            .graph
            .nodes
            .iter()
            .any(|n| matches!(n.kind, MermaidNodeKind::Choice));
        assert!(has_choice, "blind workflow expected a <<choice>> node");

        // Back-edge from choice to Exec_Propose (convergence retry loop)
        let has_back_edge = compiled
            .graph
            .edges
            .iter()
            .any(|e| e.to == "Exec_Propose" && e.from != "[*]");
        assert!(has_back_edge, "blind workflow expected a revise back-edge");
        eprintln!("\n=== BLIND WORKFLOW DRY-RUN ===\n{}", compiled.summarize());
    }

    #[test]
    fn graph_missing_node_metadata_fails_validation() {
        let spec_json = r#"{
            "name": "broken",
            "version": 1,
            "actors": {"e": {"kind": "executor", "brofile": "x"}},
            "nodes": {},
            "graph": "stateDiagram-v2\n    [*] --> P1\n    P1 --> [*]"
        }"#;
        let spec = load_workflow(spec_json).unwrap();
        let err = compile(spec).unwrap_err().to_string();
        assert!(
            err.contains("no metadata") && err.contains("P1"),
            "err: {err}"
        );
    }

    #[test]
    fn metadata_referencing_missing_graph_node_fails_validation() {
        // Real is in both graph + metadata (fine). Ghost is metadata-only —
        // that's the mismatch we want to surface.
        let spec_json = r#"{
            "name": "broken",
            "version": 1,
            "actors": {"e": {"kind": "executor", "brofile": "x"}},
            "nodes": {
                "Real": {"actor": "e"},
                "Ghost": {"actor": "e"}
            },
            "graph": "stateDiagram-v2\n    [*] --> Real\n    Real --> [*]"
        }"#;
        let spec = load_workflow(spec_json).unwrap();
        let err = compile(spec).unwrap_err().to_string();
        assert!(
            err.contains("not reachable") && err.contains("Ghost"),
            "err: {err}"
        );
    }

    #[test]
    fn node_referencing_undeclared_actor_fails_validation() {
        let spec_json = r#"{
            "name": "broken",
            "version": 1,
            "actors": {"e": {"kind": "executor", "brofile": "x"}},
            "nodes": {
                "P1": {"actor": "nonexistent"}
            },
            "graph": "stateDiagram-v2\n    [*] --> P1\n    P1 --> [*]"
        }"#;
        let spec = load_workflow(spec_json).unwrap();
        let err = compile(spec).unwrap_err().to_string();
        assert!(
            err.contains("undeclared actor") && err.contains("nonexistent"),
            "err: {err}"
        );
    }

    #[test]
    fn late_inject_referencing_missing_source_fails_validation() {
        let spec_json = r#"{
            "name": "broken",
            "version": 1,
            "actors": {"e": {"kind": "executor", "brofile": "x"}},
            "nodes": {
                "P1": {"actor": "e", "late_inject": {"from": "Ghost", "policy": "resume_on_return"}}
            },
            "graph": "stateDiagram-v2\n    [*] --> P1\n    P1 --> [*]"
        }"#;
        let spec = load_workflow(spec_json).unwrap();
        let err = compile(spec).unwrap_err().to_string();
        assert!(
            err.contains("late_inject") && err.contains("Ghost"),
            "err: {err}"
        );
    }

    #[test]
    fn subworkflow_recursively_compiles_at_parent_compile_time() {
        let spec_json = r#"{
            "name": "parent",
            "version": 1,
            "actors": {"a": {"kind": "executor", "brofile": "b"}},
            "nodes": {
                "Lead": {"actor": "a", "prompt": "lead"},
                "Compound": {
                    "actor": "a",
                    "subworkflow": {
                        "name": "sub",
                        "version": 1,
                        "actors": {"sa": {"kind": "executor", "brofile": "sb"}},
                        "nodes": {"S1": {"actor": "sa", "prompt": "sub first"}},
                        "graph": "stateDiagram-v2\n    [*] --> S1\n    S1 --> [*]"
                    }
                }
            },
            "graph": "stateDiagram-v2\n    [*] --> Lead\n    Lead --> Compound\n    Compound --> [*]"
        }"#;
        let spec = load_workflow(spec_json).unwrap();
        let compiled = compile(spec).expect("valid parent+sub compiles");
        assert!(compiled.spec.nodes["Compound"].subworkflow.is_some());
    }

    #[test]
    fn subworkflow_with_broken_graph_surfaces_error_at_parent_compile() {
        // Sub spec declares an activity node in graph but no metadata for it
        let spec_json = r#"{
            "name": "parent",
            "version": 1,
            "actors": {"a": {"kind": "executor", "brofile": "b"}},
            "nodes": {
                "Compound": {
                    "actor": "a",
                    "subworkflow": {
                        "name": "bad-sub",
                        "version": 1,
                        "actors": {"sa": {"kind": "executor", "brofile": "sb"}},
                        "nodes": {},
                        "graph": "stateDiagram-v2\n    [*] --> Orphan\n    Orphan --> [*]"
                    }
                }
            },
            "graph": "stateDiagram-v2\n    [*] --> Compound\n    Compound --> [*]"
        }"#;
        let spec = load_workflow(spec_json).unwrap();
        let err = compile(spec).unwrap_err().to_string();
        assert!(
            err.contains("subworkflow") && err.contains("Compound"),
            "err: {err}"
        );
    }

    #[test]
    fn subworkflow_node_without_actor_accepted() {
        // When a node has a subworkflow, the `actor` field is optional
        let spec_json = r#"{
            "name": "parent",
            "version": 1,
            "actors": {},
            "nodes": {
                "Compound": {
                    "actor": "",
                    "subworkflow": {
                        "name": "sub",
                        "version": 1,
                        "actors": {"sa": {"kind": "executor", "brofile": "sb"}},
                        "nodes": {"S1": {"actor": "sa"}},
                        "graph": "stateDiagram-v2\n    [*] --> S1\n    S1 --> [*]"
                    }
                }
            },
            "graph": "stateDiagram-v2\n    [*] --> Compound\n    Compound --> [*]"
        }"#;
        let spec = load_workflow(spec_json).unwrap();
        compile(spec).expect("subworkflow nodes don't require actor declaration");
    }

    #[test]
    fn fork_with_only_one_outgoing_fails_validation() {
        let spec_json = r#"{
            "name": "broken",
            "version": 1,
            "actors": {"e": {"kind": "executor", "brofile": "x"}},
            "nodes": {"P1": {"actor": "e"}},
            "graph": "stateDiagram-v2\n    [*] --> P1\n    state f1 <<fork>>\n    P1 --> f1\n    f1 --> [*]"
        }"#;
        let spec = load_workflow(spec_json).unwrap();
        let err = compile(spec).unwrap_err().to_string();
        assert!(err.contains("fork") && err.contains("f1"), "err: {err}");
    }

    #[test]
    fn graph_without_start_edge_fails_validation() {
        let spec_json = r#"{
            "name": "broken",
            "version": 1,
            "actors": {"e": {"kind": "executor", "brofile": "x"}},
            "nodes": {"P1": {"actor": "e"}},
            "graph": "stateDiagram-v2\n    P1 --> [*]"
        }"#;
        let spec = load_workflow(spec_json).unwrap();
        let err = compile(spec).unwrap_err().to_string();
        assert!(err.contains("no start edge"), "err: {err}");
    }
}
