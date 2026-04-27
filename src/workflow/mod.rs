//! Workflow DSL — protocol-level descriptions of actor interaction over time.
//!
//! A workflow file declares actors, nodes, and per-node control-flow
//! transitions. Each node carries a `next` clause whose tagged variant
//! (`goto` / `branch` / `fork` / `terminal`) names how control advances
//! after the node returns. There is no separate graph string; the
//! topology is fully expressed by the per-node transitions.
//!
//! The execution loop (`bro orchestrate run`) is separate; this module is
//! parsing + validation only.

pub mod context;
pub mod engine;
pub mod extractor;
pub mod ops;
pub mod schema;
pub mod wait;

pub use engine::{
    run_workflow, run_workflow_streaming, run_workflow_streaming_with_vars,
    run_workflow_with_initial_vars, WorkflowRunResult,
};
pub use schema::{
    load_workflow, ActorKind, ActorSpec, BranchSelector, GateMode, NodeMode, NodeTransition,
    Workflow,
};
#[cfg(test)]
pub use schema::{InjectPolicy, LateInject, NodeSpec, RetryPolicy};

use anyhow::{anyhow, bail, Result};
use std::collections::HashSet;

/// A workflow that has been loaded AND cross-validated. Produced by
/// [`compile`].
#[derive(Debug)]
pub struct CompiledWorkflow {
    pub spec: Workflow,
}

/// Cross-validate transitions, actor references, late_inject sources,
/// fork branches, and reachability. Errors name the specific mismatch.
pub fn compile(spec: Workflow) -> Result<CompiledWorkflow> {
    cross_validate(&spec)?;
    Ok(CompiledWorkflow { spec })
}

fn cross_validate(spec: &Workflow) -> Result<()> {
    // start must reference a real node.
    if !spec.nodes.contains_key(&spec.start) {
        bail!(
            "workflow start='{}' does not reference any declared node",
            spec.start
        );
    }

    // Every node's `next` targets must reference declared nodes; gate
    // packet, actor, late_inject, subworkflow, wait_for cross-checks.
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
        }

        // Actor reference must resolve when present. Empty `actor` is
        // legal — that's a hook-only or pure-routing node (Setup/Done
        // patterns: hooks fire, prompt is captured as output, walk
        // continues per `next`).
        if !node.actor.is_empty() && !spec.actors.contains_key(&node.actor) {
            bail!(
                "node '{node_id}' references undeclared actor '{}'",
                node.actor
            );
        }

        // late_inject.from must reference a declared node.
        if let Some(li) = &node.late_inject {
            if !spec.nodes.contains_key(&li.from) {
                bail!(
                    "node '{node_id}' late_inject.from='{}' is not a declared node",
                    li.from
                );
            }
        }

        // wait_for entries must reference declared nodes.
        for src in &node.wait_for {
            if !spec.nodes.contains_key(src) {
                bail!(
                    "node '{node_id}' wait_for entry '{src}' is not a declared node"
                );
            }
        }

        validate_transition(node_id, &node.next, spec)?;
    }

    // Reachability: every declared node must be reachable from `start`
    // via transitions. Unreachable nodes are spec bugs.
    let reachable = reachable_nodes(spec);
    let declared: HashSet<&str> = spec.nodes.keys().map(String::as_str).collect();
    let unreachable: Vec<&&str> = declared.difference(&reachable).collect();
    if !unreachable.is_empty() {
        let mut names: Vec<String> = unreachable.iter().map(|s| (***s).to_string()).collect();
        names.sort();
        bail!(
            "workflow declares nodes that are unreachable from start='{}': {:?}",
            spec.start,
            names
        );
    }

    // At least one terminal must exist; otherwise the arc can't end
    // (other than via policy_packet halt or hook failure, both of
    // which are abnormal exits).
    let has_terminal = spec
        .nodes
        .values()
        .any(|n| matches!(n.next, NodeTransition::Terminal));
    if !has_terminal {
        bail!("workflow has no Terminal transition — arc can never complete normally");
    }

    Ok(())
}

fn validate_transition(
    node_id: &str,
    transition: &NodeTransition,
    spec: &Workflow,
) -> Result<()> {
    match transition {
        NodeTransition::Goto { to } => {
            if !spec.nodes.contains_key(to) {
                bail!("node '{node_id}' goto target '{to}' is not a declared node");
            }
        }
        NodeTransition::Branch { cases, default, .. } => {
            if cases.is_empty() && default.is_none() {
                bail!(
                    "node '{node_id}' branch transition has no cases and no default"
                );
            }
            for (verdict, target) in cases {
                if !spec.nodes.contains_key(target) {
                    bail!(
                        "node '{node_id}' branch case '{verdict}' → '{target}' is not a declared node"
                    );
                }
            }
            if let Some(d) = default {
                if !spec.nodes.contains_key(d) {
                    bail!(
                        "node '{node_id}' branch default → '{d}' is not a declared node"
                    );
                }
            }
        }
        NodeTransition::Fork {
            branches,
            continue_to,
        } => {
            if branches.is_empty() {
                bail!("node '{node_id}' fork has no branches");
            }
            for b in branches {
                if !spec.nodes.contains_key(b) {
                    bail!("node '{node_id}' fork branch '{b}' is not a declared node");
                }
            }
            if !spec.nodes.contains_key(continue_to) {
                bail!(
                    "node '{node_id}' fork continue_to='{continue_to}' is not a declared node"
                );
            }
        }
        NodeTransition::Terminal => {}
    }
    Ok(())
}

fn reachable_nodes(spec: &Workflow) -> HashSet<&str> {
    let mut reachable: HashSet<&str> = HashSet::new();
    let mut stack: Vec<&str> = vec![spec.start.as_str()];
    while let Some(cur) = stack.pop() {
        if !reachable.insert(cur) {
            continue;
        }
        let Some(node) = spec.nodes.get(cur) else {
            continue;
        };
        for src in &node.wait_for {
            stack.push(src.as_str());
        }
        if let Some(li) = &node.late_inject {
            stack.push(li.from.as_str());
        }
        match &node.next {
            NodeTransition::Goto { to } => stack.push(to.as_str()),
            NodeTransition::Branch { cases, default, .. } => {
                for v in cases.values() {
                    stack.push(v.as_str());
                }
                if let Some(d) = default {
                    stack.push(d.as_str());
                }
            }
            NodeTransition::Fork {
                branches,
                continue_to,
            } => {
                for b in branches {
                    stack.push(b.as_str());
                }
                stack.push(continue_to.as_str());
            }
            NodeTransition::Terminal => {}
        }
    }
    reachable
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

        out.push_str("\nnodes (walk order from start):\n");
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
                if matches!(spec.mode, schema::NodeMode::FireAndForget) {
                    out.push_str(" [fire-and-forget]");
                }
                if !spec.wait_for.is_empty() {
                    out.push_str(&format!(" wait_for={:?}", spec.wait_for));
                }
                out.push('\n');
                out.push_str(&format!("    next: {}\n", format_transition(&spec.next)));
            }
        }
        out
    }

    /// Best-effort traversal order from `start`. Back-edges (loops) are
    /// broken by visited-set. Branch targets are visited in deterministic
    /// (sorted) order.
    fn topological_order(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut visited: HashSet<String> = HashSet::new();
        let mut stack: Vec<String> = vec![self.spec.start.clone()];
        while let Some(cur) = stack.pop() {
            if !visited.insert(cur.clone()) {
                continue;
            }
            out.push(cur.clone());
            let Some(node) = self.spec.nodes.get(&cur) else {
                continue;
            };
            let mut succs: Vec<String> = Vec::new();
            match &node.next {
                NodeTransition::Goto { to } => succs.push(to.clone()),
                NodeTransition::Branch { cases, default, .. } => {
                    let mut keys: Vec<&String> = cases.keys().collect();
                    keys.sort();
                    for k in keys {
                        succs.push(cases[k].clone());
                    }
                    if let Some(d) = default {
                        succs.push(d.clone());
                    }
                }
                NodeTransition::Fork {
                    branches,
                    continue_to,
                } => {
                    succs.extend(branches.iter().cloned());
                    succs.push(continue_to.clone());
                }
                NodeTransition::Terminal => {}
            }
            // Push children in reverse so original order pops first
            succs.reverse();
            for c in succs {
                if !visited.contains(&c) {
                    stack.push(c);
                }
            }
        }
        out
    }
}

fn format_transition(t: &NodeTransition) -> String {
    match t {
        NodeTransition::Goto { to } => format!("goto {to}"),
        NodeTransition::Branch {
            cases, default, ..
        } => {
            let mut keys: Vec<&String> = cases.keys().collect();
            keys.sort();
            let arms = keys
                .iter()
                .map(|k| format!("{k}→{}", cases[*k]))
                .collect::<Vec<_>>()
                .join(", ");
            match default {
                Some(d) => format!("branch [{arms}] default→{d}"),
                None => format!("branch [{arms}]"),
            }
        }
        NodeTransition::Fork {
            branches,
            continue_to,
        } => format!("fork {branches:?} continue_to={continue_to}"),
        NodeTransition::Terminal => "terminal".into(),
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

        // Convergence is now expressed via a Branch transition on
        // Ensemble_Blind_Iter.
        let iter = compiled
            .spec
            .nodes
            .get("Ensemble_Blind_Iter")
            .expect("convergence node present");
        let NodeTransition::Branch { cases, .. } = &iter.next else {
            panic!("Ensemble_Blind_Iter expected a branch transition, got {:?}", iter.next);
        };
        assert_eq!(cases.get("revise").map(String::as_str), Some("Exec_Propose"));
        assert_eq!(cases.get("converged").map(String::as_str), Some("Exec_Work"));
        eprintln!("\n=== BLIND WORKFLOW DRY-RUN ===\n{}", compiled.summarize());
    }

    #[test]
    fn missing_start_node_fails_validation() {
        let spec_json = r#"{
            "name": "broken",
            "version": 1,
            "actors": {"e": {"kind": "executor", "brofile": "x"}},
            "nodes": {"P1": {"actor": "e", "next": {"type": "terminal"}}},
            "start": "Ghost"
        }"#;
        let spec = load_workflow(spec_json).unwrap();
        let err = compile(spec).unwrap_err().to_string();
        assert!(err.contains("start='Ghost'"), "err: {err}");
    }

    #[test]
    fn unreachable_node_fails_validation() {
        let spec_json = r#"{
            "name": "broken",
            "version": 1,
            "actors": {"e": {"kind": "executor", "brofile": "x"}},
            "nodes": {
                "Real": {"actor": "e", "next": {"type": "terminal"}},
                "Ghost": {"actor": "e", "next": {"type": "terminal"}}
            },
            "start": "Real"
        }"#;
        let spec = load_workflow(spec_json).unwrap();
        let err = compile(spec).unwrap_err().to_string();
        assert!(err.contains("unreachable") && err.contains("Ghost"), "err: {err}");
    }

    #[test]
    fn goto_to_undeclared_node_fails_validation() {
        let spec_json = r#"{
            "name": "broken",
            "version": 1,
            "actors": {"e": {"kind": "executor", "brofile": "x"}},
            "nodes": {
                "P1": {"actor": "e", "next": {"type": "goto", "to": "Ghost"}}
            },
            "start": "P1"
        }"#;
        let spec = load_workflow(spec_json).unwrap();
        let err = compile(spec).unwrap_err().to_string();
        assert!(err.contains("goto target 'Ghost'"), "err: {err}");
    }

    #[test]
    fn node_referencing_undeclared_actor_fails_validation() {
        let spec_json = r#"{
            "name": "broken",
            "version": 1,
            "actors": {"e": {"kind": "executor", "brofile": "x"}},
            "nodes": {
                "P1": {"actor": "nonexistent", "next": {"type": "terminal"}}
            },
            "start": "P1"
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
                "P1": {"actor": "e", "late_inject": {"from": "Ghost", "policy": "resume_on_return"}, "next": {"type": "terminal"}}
            },
            "start": "P1"
        }"#;
        let spec = load_workflow(spec_json).unwrap();
        let err = compile(spec).unwrap_err().to_string();
        assert!(err.contains("late_inject") && err.contains("Ghost"), "err: {err}");
    }

    #[test]
    fn subworkflow_recursively_compiles_at_parent_compile_time() {
        let spec_json = r#"{
            "name": "parent",
            "version": 1,
            "actors": {"a": {"kind": "executor", "brofile": "b"}},
            "nodes": {
                "Lead": {"actor": "a", "prompt": "lead", "next": {"type": "goto", "to": "Compound"}},
                "Compound": {
                    "actor": "a",
                    "next": {"type": "terminal"},
                    "subworkflow": {
                        "name": "sub",
                        "version": 1,
                        "actors": {"sa": {"kind": "executor", "brofile": "sb"}},
                        "nodes": {"S1": {"actor": "sa", "prompt": "sub first", "next": {"type": "terminal"}}},
                        "start": "S1"
                    }
                }
            },
            "start": "Lead"
        }"#;
        let spec = load_workflow(spec_json).unwrap();
        let compiled = compile(spec).expect("valid parent+sub compiles");
        assert!(compiled.spec.nodes["Compound"].subworkflow.is_some());
    }

    #[test]
    fn subworkflow_with_broken_internals_surfaces_error_at_parent_compile() {
        // Sub references a missing goto target.
        let spec_json = r#"{
            "name": "parent",
            "version": 1,
            "actors": {"a": {"kind": "executor", "brofile": "b"}},
            "nodes": {
                "Compound": {
                    "actor": "a",
                    "next": {"type": "terminal"},
                    "subworkflow": {
                        "name": "bad-sub",
                        "version": 1,
                        "actors": {"sa": {"kind": "executor", "brofile": "sb"}},
                        "nodes": {
                            "S1": {"actor": "sa", "next": {"type": "goto", "to": "Orphan"}}
                        },
                        "start": "S1"
                    }
                }
            },
            "start": "Compound"
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
        let spec_json = r#"{
            "name": "parent",
            "version": 1,
            "actors": {},
            "nodes": {
                "Compound": {
                    "actor": "",
                    "next": {"type": "terminal"},
                    "subworkflow": {
                        "name": "sub",
                        "version": 1,
                        "actors": {"sa": {"kind": "executor", "brofile": "sb"}},
                        "nodes": {"S1": {"actor": "sa", "next": {"type": "terminal"}}},
                        "start": "S1"
                    }
                }
            },
            "start": "Compound"
        }"#;
        let spec = load_workflow(spec_json).unwrap();
        compile(spec).expect("subworkflow nodes don't require actor declaration");
    }

    #[test]
    fn hook_only_node_without_actor_accepted() {
        // Hook-only nodes (Setup / Done patterns) have empty actor and
        // run only on_enter / on_exit. Validator must accept this.
        let spec_json = r#"{
            "name": "hook-only",
            "version": 1,
            "actors": {},
            "nodes": {
                "Setup": {
                    "actor": "",
                    "prompt": "setup phase complete",
                    "next": {"type": "terminal"}
                }
            },
            "start": "Setup"
        }"#;
        let spec = load_workflow(spec_json).unwrap();
        compile(spec).expect("hook-only nodes without actor are valid");
    }

    #[test]
    fn fork_with_no_branches_fails_validation() {
        let spec_json = r#"{
            "name": "broken",
            "version": 1,
            "actors": {"e": {"kind": "executor", "brofile": "x"}},
            "nodes": {
                "P1": {"actor": "e", "next": {"type": "fork", "branches": [], "continue_to": "P2"}},
                "P2": {"actor": "e", "next": {"type": "terminal"}}
            },
            "start": "P1"
        }"#;
        let spec = load_workflow(spec_json).unwrap();
        let err = compile(spec).unwrap_err().to_string();
        assert!(err.contains("fork has no branches"), "err: {err}");
    }

    #[test]
    fn workflow_without_terminal_fails_validation() {
        let spec_json = r#"{
            "name": "broken",
            "version": 1,
            "actors": {"e": {"kind": "executor", "brofile": "x"}},
            "nodes": {
                "P1": {"actor": "e", "next": {"type": "goto", "to": "P1"}}
            },
            "start": "P1"
        }"#;
        let spec = load_workflow(spec_json).unwrap();
        let err = compile(spec).unwrap_err().to_string();
        assert!(err.contains("no Terminal transition"), "err: {err}");
    }

    #[test]
    fn workflow_json_schema_is_valid() {
        // The schema/workflow.schema.json artifact is the public surface
        // for editor tooling and downstream validation. Make sure it
        // parses and that its draft-07 metaschema accepts it (catches
        // hand-edit typos in keywords like `oneOf` vs `oneof`).
        let raw = include_str!("../../schema/workflow.schema.json");
        let parsed: serde_json::Value =
            serde_json::from_str(raw).expect("workflow.schema.json must be valid JSON");

        // Sanity: top-level shape we depend on.
        assert_eq!(parsed["title"], "Blackbox Workflow Spec");
        assert!(parsed["definitions"]["NodeTransition"].is_object());
        assert!(parsed["definitions"]["NodeSpec"].is_object());

        // Compile against the draft-07 metaschema. If this fails, the
        // schema itself is malformed at the schema level.
        let compiled = jsonschema::JSONSchema::options()
            .with_draft(jsonschema::Draft::Draft7)
            .compile(&parsed)
            .expect("workflow.schema.json must compile as a draft-07 schema");
        // Trivial smoke: validate a minimal valid workflow against the schema.
        let minimal: serde_json::Value = serde_json::json!({
            "name": "minimal",
            "version": 1,
            "actors": {},
            "nodes": {
                "Only": { "actor": "", "next": { "type": "terminal" } }
            },
            "start": "Only"
        });
        assert!(
            compiled.is_valid(&minimal),
            "minimal workflow rejected by schema; errors: {:?}",
            compiled
                .validate(&minimal)
                .err()
                .map(|errs| errs.map(|e| e.to_string()).collect::<Vec<_>>())
        );
    }

    #[test]
    fn every_example_validates_against_json_schema() {
        let raw_schema = include_str!("../../schema/workflow.schema.json");
        let schema_json: serde_json::Value = serde_json::from_str(raw_schema).unwrap();
        let compiled = jsonschema::JSONSchema::options()
            .with_draft(jsonschema::Draft::Draft7)
            .compile(&schema_json)
            .expect("schema compiles");

        let cases: &[(&str, &str)] = &[
            ("e2e-smoke",         include_str!("../../examples/workflows/e2e-smoke.json")),
            ("e2e-gated",         include_str!("../../examples/workflows/e2e-gated.json")),
            ("e2e-async-review",  include_str!("../../examples/workflows/e2e-async-review.json")),
            ("e2e-fork-join",     include_str!("../../examples/workflows/e2e-fork-join.json")),
            ("e2e-composition",   include_str!("../../examples/workflows/e2e-composition.json")),
            ("e2e-policy",        include_str!("../../examples/workflows/e2e-policy.json")),
            ("e2e-review-mode",   include_str!("../../examples/workflows/e2e-review-mode.json")),
            ("e2e-self-audit",    include_str!("../../examples/workflows/e2e-self-audit.json")),
            ("e2e-ensemble-vote", include_str!("../../examples/workflows/e2e-ensemble-vote.json")),
            ("e2e-combo",         include_str!("../../examples/workflows/e2e-combo.json")),
            ("optimistic",        include_str!("../../examples/workflows/optimistic.json")),
            ("blind",             include_str!("../../examples/workflows/blind.json")),
            ("keystone-issue-to-merged-pr",       include_str!("../../examples/keystone/workflows/issue-to-merged-pr.json")),
            ("keystone-implementer-arc",          include_str!("../../examples/keystone/workflows/implementer-arc.json")),
            ("keystone-reviewer-arc",             include_str!("../../examples/keystone/workflows/reviewer-arc.json")),
            ("keystone-implementer-feedback-arc", include_str!("../../examples/keystone/workflows/implementer-feedback-arc.json")),
        ];

        let mut failures: Vec<String> = Vec::new();
        for (name, src) in cases {
            let v: serde_json::Value = serde_json::from_str(src)
                .unwrap_or_else(|e| panic!("{name}: parse failed: {e}"));
            let errors_opt: Option<Vec<String>> = compiled
                .validate(&v)
                .err()
                .map(|errs| errs.map(|e| format!("  {}: {}", e.instance_path, e)).collect());
            if let Some(msgs) = errors_opt {
                failures.push(format!("{name}:\n{}", msgs.join("\n")));
            }
        }
        assert!(failures.is_empty(), "schema validation failures:\n{}", failures.join("\n\n"));
    }

    #[test]
    fn every_example_workflow_compiles() {
        let cases: &[(&str, &str)] = &[
            ("e2e-smoke",         include_str!("../../examples/workflows/e2e-smoke.json")),
            ("e2e-gated",         include_str!("../../examples/workflows/e2e-gated.json")),
            ("e2e-async-review",  include_str!("../../examples/workflows/e2e-async-review.json")),
            ("e2e-fork-join",     include_str!("../../examples/workflows/e2e-fork-join.json")),
            ("e2e-composition",   include_str!("../../examples/workflows/e2e-composition.json")),
            ("e2e-policy",        include_str!("../../examples/workflows/e2e-policy.json")),
            ("e2e-review-mode",   include_str!("../../examples/workflows/e2e-review-mode.json")),
            ("e2e-self-audit",    include_str!("../../examples/workflows/e2e-self-audit.json")),
            ("e2e-ensemble-vote", include_str!("../../examples/workflows/e2e-ensemble-vote.json")),
            ("e2e-combo",         include_str!("../../examples/workflows/e2e-combo.json")),
            ("optimistic",        include_str!("../../examples/workflows/optimistic.json")),
            ("blind",             include_str!("../../examples/workflows/blind.json")),
            ("keystone-issue-to-merged-pr",       include_str!("../../examples/keystone/workflows/issue-to-merged-pr.json")),
            ("keystone-implementer-arc",          include_str!("../../examples/keystone/workflows/implementer-arc.json")),
            ("keystone-reviewer-arc",             include_str!("../../examples/keystone/workflows/reviewer-arc.json")),
            ("keystone-implementer-feedback-arc", include_str!("../../examples/keystone/workflows/implementer-feedback-arc.json")),
        ];
        for (name, src) in cases {
            let spec = load_workflow(src)
                .unwrap_or_else(|e| panic!("{name}: parse failed: {e:?}"));
            compile(spec).unwrap_or_else(|e| panic!("{name}: compile failed: {e:?}"));
        }
    }

    #[test]
    fn branch_to_undeclared_node_fails_validation() {
        let spec_json = r#"{
            "name": "broken",
            "version": 1,
            "actors": {"e": {"kind": "executor", "brofile": "x"}},
            "nodes": {
                "P1": {"actor": "e", "next": {"type": "branch", "cases": {"yes": "Ghost"}}}
            },
            "start": "P1"
        }"#;
        let spec = load_workflow(spec_json).unwrap();
        let err = compile(spec).unwrap_err().to_string();
        assert!(err.contains("branch case 'yes'") && err.contains("Ghost"), "err: {err}");
    }
}
