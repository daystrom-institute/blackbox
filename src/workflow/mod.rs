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
    run_workflow, run_workflow_streaming, run_workflow_streaming_with_vars_and_arc_id,
    run_workflow_with_initial_vars,
};
#[cfg(test)]
pub use schema::load_workflow;
pub use schema::{
    ActorFailureMode, ActorKind, ActorSpec, AtomBinding, AtomBindingLimits, ForeachSpec, GateMode,
    ItemFailurePolicy, MatrixSpec, NodeMode, NodeSpec, NodeTransition, Workflow,
};

use anyhow::{Result, anyhow, bail};
use std::collections::{HashMap, HashSet};

use self::context::VarKind;
use self::engine::{MAX_FOREACH_ITEMS, MAX_FOREACH_PARALLELISM};
use serde_json::Value;

/// A workflow that has been loaded AND cross-validated. Produced by
/// [`compile`].
#[derive(Debug, Clone)]
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

    for (binding_id, binding) in &spec.atom_bindings {
        if let Some(declared_id) = &binding.id
            && declared_id != binding_id
        {
            bail!(
                "atom binding '{binding_id}' declares id='{declared_id}' — binding id must match the map key"
            );
        }
        if crate::orchestration::atoms::types::AtomRef::parse(&binding.atom_ref).is_none() {
            bail!(
                "atom binding '{binding_id}' has invalid atom_ref '{}' (expected atom:name@vN or atom:name@latest)",
                binding.atom_ref
            );
        }
        if let Some(supervision_override) = &binding.supervision_override {
            validate_supervision_override(binding_id, supervision_override)?;
        }
    }

    // Every node's `next` targets must reference declared nodes; gate
    // packet, actor, late_inject, subworkflow, wait_for cross-checks.
    for (node_id, node) in &spec.nodes {
        if node.subworkflow.is_some() && node.subworkflow_ref.is_some() {
            bail!("node '{node_id}' has BOTH subworkflow (inline) and subworkflow_ref — pick one");
        }
        validate_dynamic_fanout(node_id, node, spec)?;
        if node.subworkflow.is_some() {
            // Recursively compile the sub-workflow so errors surface
            // at parent-compile time, not at dispatch time.
            if let Some(sub) = &node.subworkflow {
                compile((**sub).clone()).map_err(|e| {
                    anyhow!("subworkflow on node '{node_id}' failed to compile: {e}")
                })?;
            }
        }

        if !node.actor.is_empty() && !node.atom.is_empty() {
            bail!(
                "node '{node_id}' declares both actor '{}' and atom binding '{}' — pick one",
                node.actor,
                node.atom
            );
        }

        if !node.atom.is_empty() {
            if !spec.atom_bindings.contains_key(&node.atom) {
                bail!(
                    "node '{node_id}' references undeclared atom binding '{}'",
                    node.atom
                );
            }
            if node.subworkflow.is_some() || node.subworkflow_ref.is_some() {
                bail!("node '{node_id}' cannot combine atom binding with subworkflow");
            }
            if node.wait.is_some() {
                bail!("node '{node_id}' cannot combine atom binding with wait");
            }
            if node.sleep.is_some() {
                bail!("node '{node_id}' cannot combine atom binding with sleep");
            }
            if node.foreach.is_some() || node.matrix.is_some() {
                bail!("node '{node_id}' cannot combine atom binding with foreach/matrix");
            }
        }

        if let Some(sleep) = &node.sleep {
            if sleep.duration_ms == 0 {
                bail!("node '{node_id}' sleep.duration_ms must be greater than zero");
            }
            if !node.atom.is_empty() {
                bail!("node '{node_id}' cannot combine atom binding with sleep");
            }
            if !node.actor.is_empty() {
                bail!("node '{node_id}' cannot combine actor with sleep");
            }
            if node.wait.is_some() {
                bail!("node '{node_id}' cannot combine wait with sleep");
            }
            if node.subworkflow.is_some() || node.subworkflow_ref.is_some() {
                bail!("node '{node_id}' cannot combine subworkflow with sleep");
            }
            if node.foreach.is_some() || node.matrix.is_some() {
                bail!("node '{node_id}' cannot combine sleep with foreach/matrix");
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
                bail!("node '{node_id}' wait_for entry '{src}' is not a declared node");
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

fn validate_supervision_override(
    binding_id: &str,
    override_policy: &crate::orchestration::atoms::types::SupervisionPlanOverride,
) -> Result<()> {
    override_policy.validate_shape().map_err(|e| {
        anyhow!("atom binding '{binding_id}' has invalid supervision_override: {e}")
    })?;
    Ok(())
}

fn validate_dynamic_fanout(
    node_id: &str,
    node: &schema::NodeSpec,
    parent: &Workflow,
) -> Result<()> {
    let fanout_count = node.foreach.is_some() as u8 + node.matrix.is_some() as u8;
    if fanout_count == 0 {
        return Ok(());
    }
    if fanout_count > 1 {
        bail!("node '{node_id}' has BOTH foreach and matrix — pick one");
    }
    if !node.actor.is_empty() {
        bail!(
            "node '{node_id}' foreach/matrix cannot also declare actor '{}'",
            node.actor
        );
    }
    if !node.atom.is_empty() {
        bail!(
            "node '{node_id}' foreach/matrix cannot also declare atom binding '{}'",
            node.atom
        );
    }
    if node.subworkflow.is_some() || node.subworkflow_ref.is_some() {
        bail!("node '{node_id}' foreach/matrix cannot also declare node-level subworkflow");
    }
    if node.wait.is_some() {
        bail!("node '{node_id}' foreach/matrix cannot also declare wait");
    }
    if matches!(node.mode, NodeMode::FireAndForget) {
        bail!("node '{node_id}' foreach/matrix cannot use mode=fire_and_forget");
    }

    match (&node.foreach, &node.matrix) {
        (Some(spec), None) => {
            validate_literal_array_len(node_id, "foreach items", &spec.items)?;
            validate_foreach_like(node_id, spec, &parent.vars_schema)
        }
        (None, Some(matrix)) => {
            if matrix.axes.is_empty() {
                bail!("node '{node_id}' matrix has no axes");
            }
            let mut axis_names = HashSet::new();
            for axis in &matrix.axes {
                validate_var_name(node_id, "matrix axis", &axis.name)?;
                validate_literal_array_len(
                    node_id,
                    &format!("matrix axis '{}'", axis.name),
                    &axis.values,
                )?;
                if !axis_names.insert(axis.name.as_str()) {
                    bail!("node '{node_id}' matrix has duplicate axis '{}'", axis.name);
                }
            }
            validate_foreach_like(node_id, matrix, &parent.vars_schema)
        }
        _ => Ok(()),
    }
}

trait FanoutSpec {
    fn as_var(&self) -> &str;
    fn index_as(&self) -> Option<&str>;
    fn subworkflow(&self) -> Option<&Workflow>;
    fn subworkflow_ref(&self) -> Option<&str>;
    fn imports(&self) -> &[String];
    fn import_renames(&self) -> &HashMap<String, String>;
    fn exports(&self) -> &[String];
    fn parallelism(&self) -> Option<usize>;
    fn collect_into_var(&self) -> &str;
}

impl FanoutSpec for ForeachSpec {
    fn as_var(&self) -> &str {
        &self.as_var
    }
    fn index_as(&self) -> Option<&str> {
        self.index_as.as_deref()
    }
    fn subworkflow(&self) -> Option<&Workflow> {
        self.subworkflow.as_deref()
    }
    fn subworkflow_ref(&self) -> Option<&str> {
        self.subworkflow_ref.as_deref()
    }
    fn imports(&self) -> &[String] {
        &self.imports
    }
    fn import_renames(&self) -> &HashMap<String, String> {
        &self.import_renames
    }
    fn exports(&self) -> &[String] {
        &self.exports
    }
    fn parallelism(&self) -> Option<usize> {
        self.parallelism
    }
    fn collect_into_var(&self) -> &str {
        &self.collect.into_var
    }
}

impl FanoutSpec for MatrixSpec {
    fn as_var(&self) -> &str {
        &self.as_var
    }
    fn index_as(&self) -> Option<&str> {
        self.index_as.as_deref()
    }
    fn subworkflow(&self) -> Option<&Workflow> {
        self.subworkflow.as_deref()
    }
    fn subworkflow_ref(&self) -> Option<&str> {
        self.subworkflow_ref.as_deref()
    }
    fn imports(&self) -> &[String] {
        &self.imports
    }
    fn import_renames(&self) -> &HashMap<String, String> {
        &self.import_renames
    }
    fn exports(&self) -> &[String] {
        &self.exports
    }
    fn parallelism(&self) -> Option<usize> {
        self.parallelism
    }
    fn collect_into_var(&self) -> &str {
        &self.collect.into_var
    }
}

fn validate_foreach_like<S: FanoutSpec>(
    node_id: &str,
    spec: &S,
    vars_schema: &Option<context::VarsSchema>,
) -> Result<()> {
    validate_var_name(node_id, "as_var", spec.as_var())?;
    if let Some(index_as) = spec.index_as() {
        validate_var_name(node_id, "index_as", index_as)?;
    }
    validate_var_name(node_id, "collect.into_var", spec.collect_into_var())?;
    validate_binding_collisions(node_id, spec)?;
    // TODO(PR2): key template dry-run validation belongs with runtime
    // item expansion, where the per-item value shape is known. A
    // schema-only placeholder render would falsely reject valid keys
    // like `${vars.item.id}` when `item` is a runtime object.

    match (spec.subworkflow(), spec.subworkflow_ref()) {
        (Some(_), Some(_)) => {
            bail!(
                "node '{node_id}' foreach/matrix has BOTH subworkflow and subworkflow_ref — pick one"
            )
        }
        (None, None) => {
            bail!("node '{node_id}' foreach/matrix requires subworkflow or subworkflow_ref")
        }
        _ => {}
    }

    if let Some(parallelism) = spec.parallelism() {
        if parallelism == 0 || parallelism > MAX_FOREACH_PARALLELISM {
            bail!(
                "node '{node_id}' foreach/matrix parallelism must be 1..={MAX_FOREACH_PARALLELISM}"
            );
        }
    }

    if let Some(schema) = vars_schema {
        if let Some(entry) = schema.get(spec.collect_into_var()) {
            if entry.kind != VarKind::Array {
                bail!(
                    "node '{node_id}' foreach/matrix collect.into_var '{}' must be kind=array in vars_schema",
                    spec.collect_into_var()
                );
            }
        }
    }

    if let Some(sub) = spec.subworkflow() {
        compile(sub.clone()).map_err(|e| {
            anyhow!("foreach/matrix subworkflow on node '{node_id}' failed to compile: {e}")
        })?;
        if let Some(schema) = sub.vars_schema.as_ref() {
            for key in spec.imports() {
                if !schema.contains_key(key) {
                    bail!(
                        "node '{node_id}' foreach/matrix import '{key}' is not declared in inline subworkflow vars_schema"
                    );
                }
            }
            for key in spec.import_renames().keys() {
                if !schema.contains_key(key) {
                    bail!(
                        "node '{node_id}' foreach/matrix import_renames target '{key}' is not declared in inline subworkflow vars_schema"
                    );
                }
            }
            for key in spec.exports() {
                if !schema.contains_key(key) {
                    bail!(
                        "node '{node_id}' foreach/matrix export '{key}' is not declared in inline subworkflow vars_schema"
                    );
                }
            }
        }
    }

    Ok(())
}

fn validate_binding_collisions<S: FanoutSpec>(node_id: &str, spec: &S) -> Result<()> {
    let mut collisions = Vec::new();
    for key in spec.imports().iter().map(String::as_str) {
        if key == spec.as_var() || spec.index_as().is_some_and(|index_as| key == index_as) {
            collisions.push(key.to_string());
        }
    }
    for key in spec.import_renames().keys().map(String::as_str) {
        if key == spec.as_var() || spec.index_as().is_some_and(|index_as| key == index_as) {
            collisions.push(key.to_string());
        }
    }
    collisions.sort();
    collisions.dedup();
    if !collisions.is_empty() {
        bail!(
            "node '{node_id}' foreach/matrix imports collide with per-item bindings: {:?}",
            collisions
        );
    }
    Ok(())
}

fn validate_literal_array_len(node_id: &str, label: &str, value: &Value) -> Result<()> {
    if let Value::Array(items) = value {
        if items.len() > MAX_FOREACH_ITEMS {
            bail!(
                "node '{node_id}' {label} literal has {} items, exceeding MAX_FOREACH_ITEMS={MAX_FOREACH_ITEMS}",
                items.len()
            );
        }
    }
    Ok(())
}

fn validate_var_name(node_id: &str, label: &str, name: &str) -> Result<()> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        bail!("node '{node_id}' {label} must not be empty");
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        bail!("node '{node_id}' {label} '{name}' is not a valid var identifier");
    }
    if chars.any(|c| !(c == '_' || c.is_ascii_alphanumeric())) {
        bail!("node '{node_id}' {label} '{name}' is not a valid var identifier");
    }
    Ok(())
}

fn validate_transition(node_id: &str, transition: &NodeTransition, spec: &Workflow) -> Result<()> {
    match transition {
        NodeTransition::Goto { to } => {
            if !spec.nodes.contains_key(to) {
                bail!("node '{node_id}' goto target '{to}' is not a declared node");
            }
        }
        NodeTransition::Branch { cases, default, .. } => {
            if cases.is_empty() && default.is_none() {
                bail!("node '{node_id}' branch transition has no cases and no default");
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
                    bail!("node '{node_id}' branch default → '{d}' is not a declared node");
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
                bail!("node '{node_id}' fork continue_to='{continue_to}' is not a declared node");
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
        NodeTransition::Branch { cases, default, .. } => {
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

    fn compile_err(spec_json: &str) -> String {
        let spec = load_workflow(spec_json).unwrap();
        compile(spec).unwrap_err().to_string()
    }

    fn phase_decompose_workflow_cases() -> &'static [(&'static str, &'static str)] {
        &[
            (
                "phase-decompose-discovery",
                include_str!("../../system-defaults/workflows/phase-decompose/discovery.json"),
            ),
            (
                "phase-decompose-ensemble-decompose",
                include_str!(
                    "../../system-defaults/workflows/phase-decompose/ensemble-decompose.json"
                ),
            ),
            (
                "phase-decompose-supervised-impl",
                include_str!(
                    "../../system-defaults/workflows/phase-decompose/supervised-impl.json"
                ),
            ),
            (
                "phase-decompose-supervised-impl-edit",
                include_str!(
                    "../../system-defaults/workflows/phase-decompose/supervised-impl-edit.json"
                ),
            ),
            (
                "phase-decompose-recompose",
                include_str!("../../system-defaults/workflows/phase-decompose/recompose.json"),
            ),
            (
                "phase-decompose-main",
                include_str!("../../system-defaults/workflows/phase-decompose/main.json"),
            ),
            (
                "phase-decompose-main-edit",
                include_str!("../../system-defaults/workflows/phase-decompose/main-edit.json"),
            ),
        ]
    }

    #[test]
    fn atom_binding_workflow_loads_and_validates() {
        let json = r#"{
            "name": "atom-binding-smoke",
            "version": 1,
            "actors": {},
            "atom_bindings": {
                "echoer": {
                    "atom_ref": "atom:echo@v1",
                    "durable": true,
                    "limits": {"dispatches_runs": 0}
                }
            },
            "nodes": {
                "Echo": {
                    "atom": "echoer",
                    "atom_args": {"message": "${vars.message}"},
                    "next": {"type": "terminal"}
                }
            },
            "start": "Echo"
        }"#;
        let spec = load_workflow(json).expect("parse atom-binding workflow");
        let compiled = compile(spec).expect("compile atom-binding workflow");
        assert_eq!(
            compiled.spec.atom_bindings["echoer"].atom_ref,
            "atom:echo@v1"
        );
        assert_eq!(compiled.spec.nodes["Echo"].atom, "echoer");
    }

    #[test]
    fn atom_binding_rejects_bare_atom_ref() {
        let err = compile_err(
            r#"{
                "name": "bad-binding",
                "version": 1,
                "actors": {},
                "atom_bindings": {
                    "echoer": {"atom_ref": "echo"}
                },
                "nodes": {
                    "Echo": {"atom": "echoer", "next": {"type": "terminal"}}
                },
                "start": "Echo"
            }"#,
        );
        assert!(err.contains("invalid atom_ref"), "{err}");
    }

    #[test]
    fn atom_binding_accepts_typed_supervision_override() {
        let json = r#"{
            "name": "atom-binding-supervision",
            "version": 1,
            "actors": {},
            "atom_bindings": {
                "echoer": {
                    "atom_ref": "atom:echo@v1",
                    "supervision_override": {
                        "classifier": {
                            "mode": "none"
                        },
                        "advisor": {
                            "mode": "on_alert",
                            "atom_ref": "atom:turn-end-advisor@v1"
                        },
                        "tail_policy": {
                            "events": 5,
                            "notes": 3,
                            "assistant_bytes": 2048
                        }
                    }
                }
            },
            "nodes": {
                "Echo": {"atom": "echoer", "next": {"type": "terminal"}}
            },
            "start": "Echo"
        }"#;
        let spec = load_workflow(json).expect("parse atom-binding workflow");
        compile(spec).expect("typed supervision_override should compile");
    }

    #[test]
    fn atom_binding_rejects_malformed_supervision_override() {
        let err = compile_err(
            r#"{
                "name": "bad-binding-supervision",
                "version": 1,
                "actors": {},
                "atom_bindings": {
                    "echoer": {
                        "atom_ref": "atom:echo@v1",
                        "supervision_override": {
                            "classifier": {
                                "atom_ref": "behavior-classifier"
                            }
                        }
                    }
                },
                "nodes": {
                    "Echo": {"atom": "echoer", "next": {"type": "terminal"}}
                },
                "start": "Echo"
            }"#,
        );
        assert!(err.contains("supervision_override"), "{err}");
        assert!(err.contains("typed atom ref"), "{err}");
    }

    #[test]
    fn node_cannot_reference_actor_and_atom_binding() {
        let err = compile_err(
            r#"{
                "name": "bad-node",
                "version": 1,
                "actors": {"worker": {"kind": "executor", "brofile": "worker"}},
                "atom_bindings": {
                    "echoer": {"atom_ref": "atom:echo@v1"}
                },
                "nodes": {
                    "Echo": {
                        "actor": "worker",
                        "atom": "echoer",
                        "next": {"type": "terminal"}
                    }
                },
                "start": "Echo"
            }"#,
        );
        assert!(err.contains("declares both actor"), "{err}");
    }

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
    fn sastquatch_workflows_compile() {
        // The five JSON files under examples/sastquatch/workflows/
        // are the production artifacts the install.sh script registers
        // with the daemon. Compile-validate them in the test suite so
        // schema drift surfaces immediately.
        for path in &[
            "../../examples/sastquatch/workflows/sastquatch-arc.json",
            "../../examples/sastquatch/workflows/sastquatch-analyzer-arc.json",
            "../../examples/sastquatch/workflows/sastquatch-fixer-arc.json",
            "../../examples/sastquatch/workflows/sastquatch-feedback-arc.json",
            "../../examples/sastquatch/workflows/sastquatch-reviewer-arc.json",
        ] {
            let full = format!(
                "{}/{}",
                env!("CARGO_MANIFEST_DIR"),
                path.trim_start_matches("../../")
            );
            let json =
                std::fs::read_to_string(&full).unwrap_or_else(|e| panic!("read {path}: {e}"));
            let spec = load_workflow(&json).unwrap_or_else(|e| panic!("parse {path}: {e:?}"));
            compile(spec).unwrap_or_else(|e| panic!("compile {path}: {e:?}"));
        }
    }

    #[test]
    fn whiteboard_workflows_compile() {
        {
            let path = &"../../examples/whiteboard/workflows/whiteboard-arc.json";
            let full = format!(
                "{}/{}",
                env!("CARGO_MANIFEST_DIR"),
                path.trim_start_matches("../../")
            );
            let json =
                std::fs::read_to_string(&full).unwrap_or_else(|e| panic!("read {path}: {e}"));
            let spec = load_workflow(&json).unwrap_or_else(|e| panic!("parse {path}: {e:?}"));
            compile(spec).unwrap_or_else(|e| panic!("compile {path}: {e:?}"));
        }
    }

    #[test]
    fn phase_decompose_workflows_compile() {
        for (name, src) in phase_decompose_workflow_cases() {
            let spec = load_workflow(src).unwrap_or_else(|e| panic!("{name}: parse failed: {e:?}"));
            compile(spec).unwrap_or_else(|e| panic!("{name}: compile failed: {e:?}"));
        }
    }

    #[test]
    fn pathology_ensemble_workflows_compile() {
        let cases: &[(&str, &str)] = &[
            (
                "arch-pathology-rust",
                include_str!("../../system-defaults/workflows/refactor/arch-pathology-rust.json"),
            ),
            (
                "arch-pathology-java",
                include_str!("../../system-defaults/workflows/refactor/arch-pathology-java.json"),
            ),
            (
                "perf-pathology-rust",
                include_str!("../../system-defaults/workflows/refactor/perf-pathology-rust.json"),
            ),
            (
                "perf-pathology-java",
                include_str!("../../system-defaults/workflows/refactor/perf-pathology-java.json"),
            ),
        ];
        for (name, src) in cases {
            let spec = load_workflow(src).unwrap_or_else(|e| panic!("{name}: parse failed: {e:?}"));
            compile(spec).unwrap_or_else(|e| panic!("{name}: compile failed: {e:?}"));
        }
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
            panic!(
                "Ensemble_Blind_Iter expected a branch transition, got {:?}",
                iter.next
            );
        };
        assert_eq!(
            cases.get("revise").map(String::as_str),
            Some("Exec_Propose")
        );
        assert_eq!(
            cases.get("converged").map(String::as_str),
            Some("Exec_Work")
        );
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
        assert!(
            err.contains("unreachable") && err.contains("Ghost"),
            "err: {err}"
        );
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
    fn foreach_node_with_inline_subworkflow_compiles() {
        let spec_json = r#"{
            "name": "foreach-parent",
            "version": 1,
            "actors": {},
            "vars_schema": {
                "results": {"kind": "array"}
            },
            "nodes": {
                "Each": {
                    "actor": "",
                    "foreach": {
                        "items": ["a", "b"],
                        "as_var": "item",
                        "index_as": "item_index",
                        "on_item_failure": "collect_then_halt",
                        "collect": {"into_var": "results"},
                        "subworkflow": {
                            "name": "foreach-child",
                            "version": 1,
                            "actors": {},
                            "nodes": {
                                "Only": {"actor": "", "next": {"type": "terminal"}}
                            },
                            "start": "Only"
                        }
                    },
                    "next": {"type": "terminal"}
                }
            },
            "start": "Each"
        }"#;
        let spec = load_workflow(spec_json).unwrap();
        compile(spec).expect("foreach schema-only node compiles");
    }

    #[test]
    fn matrix_node_with_runtime_axis_values_compiles() {
        let spec_json = r#"{
            "name": "matrix-parent",
            "version": 1,
            "actors": {},
            "vars_schema": {
                "results": {"kind": "array"}
            },
            "nodes": {
                "Grid": {
                    "actor": "",
                    "matrix": {
                        "axes": [
                            {"name": "query", "values": "${vars.queries}"},
                            {"name": "strategy", "values": ["search-only", "agentic"]}
                        ],
                        "as_var": "case",
                        "collect": {"into_var": "results"},
                        "subworkflow_ref": "installed-child-workflow"
                    },
                    "next": {"type": "terminal"}
                }
            },
            "start": "Grid"
        }"#;
        let spec = load_workflow(spec_json).unwrap();
        compile(spec).expect("matrix with runtime axis values compiles");
    }

    #[test]
    fn foreach_with_actor_fails_validation() {
        let spec_json = r#"{
            "name": "bad-foreach",
            "version": 1,
            "actors": {"e": {"kind": "executor", "brofile": "x"}},
            "nodes": {
                "Each": {
                    "actor": "e",
                    "foreach": {
                        "items": [],
                        "as_var": "item",
                        "collect": {"into_var": "results"},
                        "subworkflow_ref": "child"
                    },
                    "next": {"type": "terminal"}
                }
            },
            "start": "Each"
        }"#;
        let spec = load_workflow(spec_json).unwrap();
        let err = compile(spec).unwrap_err().to_string();
        assert!(
            err.contains("foreach/matrix") && err.contains("actor"),
            "err: {err}"
        );
    }

    #[test]
    fn foreach_collect_var_must_be_array_when_declared() {
        let spec_json = r#"{
            "name": "bad-foreach",
            "version": 1,
            "actors": {},
            "vars_schema": {
                "results": {"kind": "string"}
            },
            "nodes": {
                "Each": {
                    "actor": "",
                    "foreach": {
                        "items": [],
                        "as_var": "item",
                        "collect": {"into_var": "results"},
                        "subworkflow_ref": "child"
                    },
                    "next": {"type": "terminal"}
                }
            },
            "start": "Each"
        }"#;
        let spec = load_workflow(spec_json).unwrap();
        let err = compile(spec).unwrap_err().to_string();
        assert!(err.contains("kind=array"), "err: {err}");
    }

    #[test]
    fn matrix_duplicate_axis_fails_validation() {
        let spec_json = r#"{
            "name": "bad-matrix",
            "version": 1,
            "actors": {},
            "nodes": {
                "Grid": {
                    "actor": "",
                    "matrix": {
                        "axes": [
                            {"name": "query", "values": []},
                            {"name": "query", "values": []}
                        ],
                        "as_var": "case",
                        "collect": {"into_var": "results"},
                        "subworkflow_ref": "child"
                    },
                    "next": {"type": "terminal"}
                }
            },
            "start": "Grid"
        }"#;
        let spec = load_workflow(spec_json).unwrap();
        let err = compile(spec).unwrap_err().to_string();
        assert!(err.contains("duplicate axis"), "err: {err}");
    }

    #[test]
    fn foreach_and_matrix_on_same_node_fails_validation() {
        let err = compile_err(
            r#"{
                "name": "bad-fanout",
                "version": 1,
                "actors": {},
                "nodes": {
                    "Node": {
                        "actor": "",
                        "foreach": {
                            "items": [],
                            "as_var": "item",
                            "collect": {"into_var": "results"},
                            "subworkflow_ref": "child"
                        },
                        "matrix": {
                            "axes": [{"name": "axis", "values": []}],
                            "as_var": "case",
                            "collect": {"into_var": "results"},
                            "subworkflow_ref": "child"
                        },
                        "next": {"type": "terminal"}
                    }
                },
                "start": "Node"
            }"#,
        );
        assert!(err.contains("BOTH foreach and matrix"), "err: {err}");
    }

    #[test]
    fn foreach_with_wait_or_node_subworkflow_fails_validation() {
        let wait_err = compile_err(
            r#"{
                "name": "bad-foreach",
                "version": 1,
                "actors": {},
                "nodes": {
                    "Each": {
                        "actor": "",
                        "wait": {"any_of": [{"signal": "go"}]},
                        "foreach": {
                            "items": [],
                            "as_var": "item",
                            "collect": {"into_var": "results"},
                            "subworkflow_ref": "child"
                        },
                        "next": {"type": "terminal"}
                    }
                },
                "start": "Each"
            }"#,
        );
        assert!(wait_err.contains("declare wait"), "err: {wait_err}");

        let sub_err = compile_err(
            r#"{
                "name": "bad-foreach",
                "version": 1,
                "actors": {},
                "nodes": {
                    "Each": {
                        "actor": "",
                        "subworkflow_ref": "outer-child",
                        "foreach": {
                            "items": [],
                            "as_var": "item",
                            "collect": {"into_var": "results"},
                            "subworkflow_ref": "inner-child"
                        },
                        "next": {"type": "terminal"}
                    }
                },
                "start": "Each"
            }"#,
        );
        assert!(sub_err.contains("node-level subworkflow"), "err: {sub_err}");
    }

    #[test]
    fn foreach_fire_and_forget_or_missing_child_fails_validation() {
        let mode_err = compile_err(
            r#"{
                "name": "bad-foreach",
                "version": 1,
                "actors": {},
                "nodes": {
                    "Each": {
                        "actor": "",
                        "mode": "fire_and_forget",
                        "foreach": {
                            "items": [],
                            "as_var": "item",
                            "collect": {"into_var": "results"},
                            "subworkflow_ref": "child"
                        },
                        "next": {"type": "terminal"}
                    }
                },
                "start": "Each"
            }"#,
        );
        assert!(mode_err.contains("fire_and_forget"), "err: {mode_err}");

        let child_err = compile_err(
            r#"{
                "name": "bad-foreach",
                "version": 1,
                "actors": {},
                "nodes": {
                    "Each": {
                        "actor": "",
                        "foreach": {
                            "items": [],
                            "as_var": "item",
                            "collect": {"into_var": "results"}
                        },
                        "next": {"type": "terminal"}
                    }
                },
                "start": "Each"
            }"#,
        );
        assert!(
            child_err.contains("requires subworkflow"),
            "err: {child_err}"
        );
    }

    #[test]
    fn foreach_both_child_workflow_forms_fails_validation() {
        let err = compile_err(
            r#"{
                "name": "bad-foreach",
                "version": 1,
                "actors": {},
                "nodes": {
                    "Each": {
                        "actor": "",
                        "foreach": {
                            "items": [],
                            "as_var": "item",
                            "collect": {"into_var": "results"},
                            "subworkflow_ref": "child",
                            "subworkflow": {
                                "name": "child",
                                "version": 1,
                                "actors": {},
                                "nodes": {"Only": {"actor": "", "next": {"type": "terminal"}}},
                                "start": "Only"
                            }
                        },
                        "next": {"type": "terminal"}
                    }
                },
                "start": "Each"
            }"#,
        );
        assert!(
            err.contains("BOTH subworkflow and subworkflow_ref"),
            "err: {err}"
        );
    }

    #[test]
    fn foreach_parallelism_bounds_fail_validation() {
        for (parallelism, expected) in [(0, "1..=16"), (17, "1..=16")] {
            let err = compile_err(&format!(
                r#"{{
                    "name": "bad-foreach",
                    "version": 1,
                    "actors": {{}},
                    "nodes": {{
                        "Each": {{
                            "actor": "",
                            "foreach": {{
                                "items": [],
                                "as_var": "item",
                                "parallelism": {parallelism},
                                "collect": {{"into_var": "results"}},
                                "subworkflow_ref": "child"
                            }},
                            "next": {{"type": "terminal"}}
                        }}
                    }},
                    "start": "Each"
                }}"#
            ));
            assert!(
                err.contains(expected),
                "parallelism={parallelism}, err: {err}"
            );
        }
    }

    #[test]
    fn foreach_rejects_bad_binding_identifiers() {
        for (field, snippet) in [
            ("as_var", r#""as_var": "1bad""#),
            ("index_as", r#""as_var": "item", "index_as": "has-dash""#),
        ] {
            let err = compile_err(&format!(
                r#"{{
                    "name": "bad-foreach",
                    "version": 1,
                    "actors": {{}},
                    "nodes": {{
                        "Each": {{
                            "actor": "",
                            "foreach": {{
                                "items": [],
                                {snippet},
                                "collect": {{"into_var": "results"}},
                                "subworkflow_ref": "child"
                            }},
                            "next": {{"type": "terminal"}}
                        }}
                    }},
                    "start": "Each"
                }}"#
            ));
            assert!(
                err.contains(field) && err.contains("valid var identifier"),
                "err: {err}"
            );
        }
    }

    #[test]
    fn foreach_inline_child_schema_validates_imports_exports_and_collisions() {
        let missing_import = compile_err(
            r#"{
                "name": "bad-foreach",
                "version": 1,
                "actors": {},
                "nodes": {
                    "Each": {
                        "actor": "",
                        "foreach": {
                            "items": [],
                            "as_var": "item",
                            "imports": ["missing"],
                            "collect": {"into_var": "results"},
                            "subworkflow": {
                                "name": "child",
                                "version": 1,
                                "actors": {},
                                "vars_schema": {"item": {"kind": "string"}, "summary": {"kind": "string"}},
                                "nodes": {"Only": {"actor": "", "next": {"type": "terminal"}}},
                                "start": "Only"
                            }
                        },
                        "next": {"type": "terminal"}
                    }
                },
                "start": "Each"
            }"#,
        );
        assert!(
            missing_import.contains("import 'missing'"),
            "err: {missing_import}"
        );

        let missing_export = compile_err(
            r#"{
                "name": "bad-foreach",
                "version": 1,
                "actors": {},
                "nodes": {
                    "Each": {
                        "actor": "",
                        "foreach": {
                            "items": [],
                            "as_var": "item",
                            "exports": ["sumary"],
                            "collect": {"into_var": "results"},
                            "subworkflow": {
                                "name": "child",
                                "version": 1,
                                "actors": {},
                                "vars_schema": {"item": {"kind": "string"}, "summary": {"kind": "string"}},
                                "nodes": {"Only": {"actor": "", "next": {"type": "terminal"}}},
                                "start": "Only"
                            }
                        },
                        "next": {"type": "terminal"}
                    }
                },
                "start": "Each"
            }"#,
        );
        assert!(
            missing_export.contains("export 'sumary'"),
            "err: {missing_export}"
        );

        let collision = compile_err(
            r#"{
                "name": "bad-foreach",
                "version": 1,
                "actors": {},
                "nodes": {
                    "Each": {
                        "actor": "",
                        "foreach": {
                            "items": [],
                            "as_var": "item",
                            "imports": ["item"],
                            "collect": {"into_var": "results"},
                            "subworkflow_ref": "child"
                        },
                        "next": {"type": "terminal"}
                    }
                },
                "start": "Each"
            }"#,
        );
        assert!(collision.contains("collide"), "err: {collision}");
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
    fn sleep_node_without_actor_accepted() {
        let spec_json = r#"{
            "name": "sleep-loop",
            "version": 1,
            "actors": {},
            "nodes": {
                "Sleep": {
                    "sleep": {"duration_ms": 1},
                    "next": {"type": "terminal"}
                }
            },
            "start": "Sleep"
        }"#;
        let spec = load_workflow(spec_json).unwrap();
        compile(spec).expect("sleep node without actor should compile");
    }

    #[test]
    fn sleep_node_rejects_zero_duration() {
        let err = compile_err(
            r#"{
                "name": "bad-sleep",
                "version": 1,
                "actors": {},
                "nodes": {
                    "Sleep": {
                        "sleep": {"duration_ms": 0},
                        "next": {"type": "terminal"}
                    }
                },
                "start": "Sleep"
            }"#,
        );
        assert!(err.contains("sleep.duration_ms"), "err: {err}");
    }

    #[test]
    fn sleep_node_rejects_actor_combo() {
        let err = compile_err(
            r#"{
                "name": "bad-sleep",
                "version": 1,
                "actors": {"e": {"kind": "executor", "brofile": "x"}},
                "nodes": {
                    "Sleep": {
                        "actor": "e",
                        "sleep": {"duration_ms": 1},
                        "next": {"type": "terminal"}
                    }
                },
                "start": "Sleep"
            }"#,
        );
        assert!(
            err.contains("cannot combine actor with sleep"),
            "err: {err}"
        );
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

        let invalid_parallelism: serde_json::Value = serde_json::json!({
            "name": "bad-foreach",
            "version": 1,
            "actors": {},
            "nodes": {
                "Each": {
                    "actor": "",
                    "foreach": {
                        "items": [],
                        "as_var": "item",
                        "parallelism": 17,
                        "collect": {"into_var": "results"},
                        "subworkflow_ref": "child"
                    },
                    "next": {"type": "terminal"}
                }
            },
            "start": "Each"
        });
        assert!(
            !compiled.is_valid(&invalid_parallelism),
            "workflow.schema.json should reject foreach parallelism above the engine cap"
        );

        let invalid_child_xor: serde_json::Value = serde_json::json!({
            "name": "bad-foreach",
            "version": 1,
            "actors": {},
            "nodes": {
                "Each": {
                    "actor": "",
                    "foreach": {
                        "items": [],
                        "as_var": "item",
                        "collect": {"into_var": "results"},
                        "subworkflow_ref": "child",
                        "subworkflow": {
                            "name": "child",
                            "version": 1,
                            "actors": {},
                            "nodes": {"Only": {"actor": "", "next": {"type": "terminal"}}},
                            "start": "Only"
                        }
                    },
                    "next": {"type": "terminal"}
                }
            },
            "start": "Each"
        });
        assert!(
            !compiled.is_valid(&invalid_child_xor),
            "workflow.schema.json should reject foreach child workflow XOR violations"
        );

        let valid_sleep: serde_json::Value = serde_json::json!({
            "name": "sleep-workflow",
            "version": 1,
            "actors": {},
            "nodes": {
                "Sleep": { "sleep": { "duration_ms": 1 }, "next": { "type": "terminal" } }
            },
            "start": "Sleep"
        });
        assert!(
            compiled.is_valid(&valid_sleep),
            "workflow.schema.json should accept sleep nodes"
        );

        let invalid_sleep: serde_json::Value = serde_json::json!({
            "name": "bad-sleep-workflow",
            "version": 1,
            "actors": {},
            "nodes": {
                "Sleep": { "sleep": { "duration_ms": 0 }, "next": { "type": "terminal" } }
            },
            "start": "Sleep"
        });
        assert!(
            !compiled.is_valid(&invalid_sleep),
            "workflow.schema.json should reject zero-duration sleep nodes"
        );

        let valid_supervision_override: serde_json::Value = serde_json::json!({
            "name": "supervised-binding",
            "version": 1,
            "actors": {},
            "atom_bindings": {
                "worker": {
                    "atom_ref": "atom:echo@v1",
                    "supervision_override": {
                        "classifier": {
                            "mode": "cadence",
                            "atom_ref": "atom:behavior-classifier@v1",
                            "cadence_ms": 1000
                        },
                        "advisor": {
                            "mode": "on_alert",
                            "atom_ref": "atom:turn-end-advisor@v1"
                        }
                    }
                }
            },
            "nodes": {
                "Only": { "atom": "worker", "next": { "type": "terminal" } }
            },
            "start": "Only"
        });
        assert!(
            compiled.is_valid(&valid_supervision_override),
            "workflow.schema.json should accept typed supervision_override"
        );

        let invalid_supervision_override: serde_json::Value = serde_json::json!({
            "name": "bad-supervised-binding",
            "version": 1,
            "actors": {},
            "atom_bindings": {
                "worker": {
                    "atom_ref": "atom:echo@v1",
                    "supervision_override": {
                        "classifier": {
                            "mode": "cadence",
                            "atom_ref": "behavior-classifier"
                        }
                    }
                }
            },
            "nodes": {
                "Only": { "atom": "worker", "next": { "type": "terminal" } }
            },
            "start": "Only"
        });
        assert!(
            !compiled.is_valid(&invalid_supervision_override),
            "workflow.schema.json should reject malformed supervision_override"
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
            (
                "e2e-smoke",
                include_str!("../../examples/workflows/e2e-smoke.json"),
            ),
            (
                "e2e-gated",
                include_str!("../../examples/workflows/e2e-gated.json"),
            ),
            (
                "e2e-async-review",
                include_str!("../../examples/workflows/e2e-async-review.json"),
            ),
            (
                "e2e-fork-join",
                include_str!("../../examples/workflows/e2e-fork-join.json"),
            ),
            (
                "e2e-composition",
                include_str!("../../examples/workflows/e2e-composition.json"),
            ),
            (
                "e2e-policy",
                include_str!("../../examples/workflows/e2e-policy.json"),
            ),
            (
                "e2e-review-mode",
                include_str!("../../examples/workflows/e2e-review-mode.json"),
            ),
            (
                "e2e-self-audit",
                include_str!("../../examples/workflows/e2e-self-audit.json"),
            ),
            (
                "e2e-ensemble-vote",
                include_str!("../../examples/workflows/e2e-ensemble-vote.json"),
            ),
            (
                "e2e-combo",
                include_str!("../../examples/workflows/e2e-combo.json"),
            ),
            (
                "e2e-atom-binding",
                include_str!("../../examples/workflows/e2e-atom-binding.json"),
            ),
            (
                "optimistic",
                include_str!("../../examples/workflows/optimistic.json"),
            ),
            ("blind", include_str!("../../examples/workflows/blind.json")),
            (
                "keystone-issue-to-merged-pr",
                include_str!("../../examples/keystone/workflows/issue-to-merged-pr.json"),
            ),
            (
                "keystone-implementer-arc",
                include_str!("../../examples/keystone/workflows/implementer-arc.json"),
            ),
            (
                "keystone-reviewer-arc",
                include_str!("../../examples/keystone/workflows/reviewer-arc.json"),
            ),
            (
                "keystone-implementer-feedback-arc",
                include_str!("../../examples/keystone/workflows/implementer-feedback-arc.json"),
            ),
            (
                "keystone-reviewer-arc-with-identity",
                include_str!("../../examples/keystone/workflows/reviewer-arc-with-identity.json"),
            ),
            (
                "agent-chain",
                include_str!("../../system-defaults/agents/workflows/chain.json"),
            ),
            (
                "agent-fan-out",
                include_str!("../../system-defaults/agents/workflows/fan-out.json"),
            ),
            (
                "agent-escalation",
                include_str!("../../system-defaults/agents/workflows/escalation.json"),
            ),
            (
                "agent-eval-arc",
                include_str!("../../system-defaults/agents/workflows/agent-eval-arc.json"),
            ),
            (
                "agent-cuing-eval-arc",
                include_str!("../../system-defaults/agents/workflows/agent-cuing-eval-arc.json"),
            ),
            (
                "atom-echo-review",
                include_str!("../../system-defaults/workflows/atoms/echo-review.json"),
            ),
        ];

        let mut failures: Vec<String> = Vec::new();
        for (name, src) in cases {
            let v: serde_json::Value =
                serde_json::from_str(src).unwrap_or_else(|e| panic!("{name}: parse failed: {e}"));
            let errors_opt: Option<Vec<String>> = compiled.validate(&v).err().map(|errs| {
                errs.map(|e| format!("  {}: {}", e.instance_path, e))
                    .collect()
            });
            if let Some(msgs) = errors_opt {
                failures.push(format!("{name}:\n{}", msgs.join("\n")));
            }
        }
        assert!(
            failures.is_empty(),
            "schema validation failures:\n{}",
            failures.join("\n\n")
        );
    }

    #[test]
    fn every_example_workflow_compiles() {
        let cases: &[(&str, &str)] = &[
            (
                "e2e-smoke",
                include_str!("../../examples/workflows/e2e-smoke.json"),
            ),
            (
                "e2e-gated",
                include_str!("../../examples/workflows/e2e-gated.json"),
            ),
            (
                "e2e-async-review",
                include_str!("../../examples/workflows/e2e-async-review.json"),
            ),
            (
                "e2e-fork-join",
                include_str!("../../examples/workflows/e2e-fork-join.json"),
            ),
            (
                "e2e-composition",
                include_str!("../../examples/workflows/e2e-composition.json"),
            ),
            (
                "e2e-policy",
                include_str!("../../examples/workflows/e2e-policy.json"),
            ),
            (
                "e2e-review-mode",
                include_str!("../../examples/workflows/e2e-review-mode.json"),
            ),
            (
                "e2e-self-audit",
                include_str!("../../examples/workflows/e2e-self-audit.json"),
            ),
            (
                "e2e-ensemble-vote",
                include_str!("../../examples/workflows/e2e-ensemble-vote.json"),
            ),
            (
                "e2e-combo",
                include_str!("../../examples/workflows/e2e-combo.json"),
            ),
            (
                "e2e-atom-binding",
                include_str!("../../examples/workflows/e2e-atom-binding.json"),
            ),
            (
                "optimistic",
                include_str!("../../examples/workflows/optimistic.json"),
            ),
            ("blind", include_str!("../../examples/workflows/blind.json")),
            (
                "keystone-issue-to-merged-pr",
                include_str!("../../examples/keystone/workflows/issue-to-merged-pr.json"),
            ),
            (
                "keystone-implementer-arc",
                include_str!("../../examples/keystone/workflows/implementer-arc.json"),
            ),
            (
                "keystone-reviewer-arc",
                include_str!("../../examples/keystone/workflows/reviewer-arc.json"),
            ),
            (
                "keystone-implementer-feedback-arc",
                include_str!("../../examples/keystone/workflows/implementer-feedback-arc.json"),
            ),
            (
                "keystone-reviewer-arc-with-identity",
                include_str!("../../examples/keystone/workflows/reviewer-arc-with-identity.json"),
            ),
            (
                "agent-chain",
                include_str!("../../system-defaults/agents/workflows/chain.json"),
            ),
            (
                "agent-fan-out",
                include_str!("../../system-defaults/agents/workflows/fan-out.json"),
            ),
            (
                "agent-escalation",
                include_str!("../../system-defaults/agents/workflows/escalation.json"),
            ),
            (
                "agent-eval-arc",
                include_str!("../../system-defaults/agents/workflows/agent-eval-arc.json"),
            ),
            (
                "agent-cuing-eval-arc",
                include_str!("../../system-defaults/agents/workflows/agent-cuing-eval-arc.json"),
            ),
            (
                "atom-echo-review",
                include_str!("../../system-defaults/workflows/atoms/echo-review.json"),
            ),
        ];
        for (name, src) in cases {
            let spec = load_workflow(src).unwrap_or_else(|e| panic!("{name}: parse failed: {e:?}"));
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
        assert!(
            err.contains("branch case 'yes'") && err.contains("Ghost"),
            "err: {err}"
        );
    }

    #[test]
    fn system_default_workflows_install_via_artifact_catalog() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = crate::artifacts::ArtifactCatalog::open(tmp.path().to_path_buf()).unwrap();
        let cases: &[(&str, &str)] = &[
            (
                "agent-chain",
                include_str!("../../system-defaults/agents/workflows/chain.json"),
            ),
            (
                "agent-fan-out",
                include_str!("../../system-defaults/agents/workflows/fan-out.json"),
            ),
            (
                "agent-escalation",
                include_str!("../../system-defaults/agents/workflows/escalation.json"),
            ),
            (
                "agent-eval-arc",
                include_str!("../../system-defaults/agents/workflows/agent-eval-arc.json"),
            ),
            (
                "agent-cuing-eval-arc",
                include_str!("../../system-defaults/agents/workflows/agent-cuing-eval-arc.json"),
            ),
            (
                "atom-echo-review",
                include_str!("../../system-defaults/workflows/atoms/echo-review.json"),
            ),
        ];
        for (name, src) in cases.iter().chain(phase_decompose_workflow_cases()) {
            let v: serde_json::Value = serde_json::from_str(src)
                .unwrap_or_else(|e| panic!("{name}: JSON parse failed: {e}"));
            catalog
                .install_value(
                    crate::artifacts::ArtifactKind::Workflow,
                    format!("{name}.json"),
                    &v,
                    None,
                    None,
                    None,
                )
                .unwrap_or_else(|e| panic!("{name}: artifact install failed: {e}"));
        }
        let rows = catalog
            .list(&crate::artifacts::ArtifactListParams {
                kind: Some(crate::artifacts::ArtifactKind::Workflow),
                name: None,
                include_superseded: false,
            })
            .unwrap();
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"agent-chain"), "agent-chain not in catalog");
        assert!(
            names.contains(&"agent-fan-out"),
            "agent-fan-out not in catalog"
        );
        assert!(
            names.contains(&"agent-escalation"),
            "agent-escalation not in catalog"
        );
        assert!(
            names.contains(&"agent-eval-arc"),
            "agent-eval-arc not in catalog"
        );
        assert!(
            names.contains(&"agent-cuing-eval-arc"),
            "agent-cuing-eval-arc not in catalog"
        );
    }

    #[test]
    fn agent_workflows_dry_run_produces_plan() {
        let cases: &[(&str, &str)] = &[
            (
                "agent-chain",
                include_str!("../../system-defaults/agents/workflows/chain.json"),
            ),
            (
                "agent-fan-out",
                include_str!("../../system-defaults/agents/workflows/fan-out.json"),
            ),
            (
                "agent-escalation",
                include_str!("../../system-defaults/agents/workflows/escalation.json"),
            ),
            (
                "agent-eval-arc",
                include_str!("../../system-defaults/agents/workflows/agent-eval-arc.json"),
            ),
            (
                "agent-cuing-eval-arc",
                include_str!("../../system-defaults/agents/workflows/agent-cuing-eval-arc.json"),
            ),
            (
                "atom-echo-review",
                include_str!("../../system-defaults/workflows/atoms/echo-review.json"),
            ),
        ];
        for (name, src) in cases {
            let spec = load_workflow(src).unwrap_or_else(|e| panic!("{name}: parse failed: {e:?}"));
            let compiled =
                compile(spec).unwrap_or_else(|e| panic!("{name}: compile failed: {e:?}"));
            let result = engine::dry_run(&compiled);
            assert_eq!(result.status, "dry_run", "{name}: dry_run status");
            let plan = result.plan.unwrap_or_else(|| panic!("{name}: no plan"));
            assert!(plan.contains(name), "{name}: plan missing workflow name");
        }
    }

    #[test]
    fn agent_chain_uses_dispatch_wait_pattern() {
        let src = include_str!("../../system-defaults/agents/workflows/chain.json");
        let spec = load_workflow(src).unwrap();
        let node = spec.nodes.get("RunFirst").expect("RunFirst node");
        let ops = &node.on_enter;
        assert_eq!(
            ops.len(),
            2,
            "RunFirst should have 2 hooks: dispatch + wait"
        );
        assert!(matches!(ops[0].op, super::ops::OpKind::McpCall));
        assert!(matches!(ops[1].op, super::ops::OpKind::McpCall));
        let dispatch_args = ops[0].args.as_object().expect("dispatch args object");
        assert_eq!(
            dispatch_args.get("tool").and_then(|v| v.as_str()),
            Some("bro_agent_dispatch")
        );
        let dispatch_arguments = dispatch_args
            .get("arguments")
            .and_then(|v| v.as_object())
            .expect("bro_agent_dispatch arguments");
        assert_eq!(
            dispatch_arguments.get("agent").and_then(|v| v.as_str()),
            Some("diff-narrator"),
            "chain first agent should be diff-narrator"
        );
        let wait_args = ops[1].args.as_object().expect("wait args object");
        assert_eq!(
            wait_args.get("tool").and_then(|v| v.as_str()),
            Some("bro_wait")
        );
        let wait_arguments = wait_args
            .get("arguments")
            .and_then(|v| v.as_object())
            .expect("bro_wait arguments");
        assert!(
            wait_arguments.get("task_id").is_some(),
            "bro_wait should use task_id (snake_case), not taskId"
        );
        let run_second = spec.nodes.get("RunSecond").expect("RunSecond node");
        assert_eq!(
            run_second.on_enter.len(),
            2,
            "RunSecond should dispatch code-reviewer and wait"
        );
        let r2_args = run_second.on_enter[0]
            .args
            .as_object()
            .expect("RunSecond dispatch args");
        let r2_arguments = r2_args
            .get("arguments")
            .and_then(|v| v.as_object())
            .expect("RunSecond dispatch arguments");
        assert_eq!(
            r2_arguments.get("agent").and_then(|v| v.as_str()),
            Some("code-reviewer"),
            "chain second agent should be code-reviewer"
        );
        let r2_agent_args = r2_arguments
            .get("args")
            .and_then(|v| v.as_object())
            .expect("RunSecond agent args");
        assert!(
            r2_agent_args
                .get("diff")
                .and_then(|v| v.as_str())
                .is_some_and(|diff| diff.contains("${vars.first_output.result}")),
            "RunSecond should feed diff-narrator output into code-reviewer prompt input"
        );
        assert!(
            !r2_agent_args.contains_key("context_refs"),
            "RunSecond should not pass context_refs that code-reviewer does not render"
        );
        let r2_wait_args = run_second.on_enter[1]
            .args
            .as_object()
            .expect("RunSecond wait args");
        let r2_wait_arguments = r2_wait_args
            .get("arguments")
            .and_then(|v| v.as_object())
            .expect("RunSecond wait arguments");
        assert!(
            r2_wait_arguments.get("task_id").is_some(),
            "RunSecond bro_wait should use task_id"
        );
        // All on_failure policies should be halt (not warn)
        for (i, hook) in run_second.on_enter.iter().enumerate() {
            assert_eq!(
                hook.on_failure,
                super::ops::OnFailure::Halt,
                "RunSecond hook[{i}] should halt on failure"
            );
        }
    }

    #[test]
    fn agent_fan_out_uses_sequential_dispatch_wait_aggregate() {
        let src = include_str!("../../system-defaults/agents/workflows/fan-out.json");
        let spec = load_workflow(src).unwrap();
        let dispatch_both = spec.nodes.get("DispatchBoth").expect("DispatchBoth node");
        assert_eq!(
            dispatch_both.on_enter.len(),
            2,
            "DispatchBoth should dispatch two agents"
        );
        let left_args = dispatch_both.on_enter[0]
            .args
            .as_object()
            .expect("left dispatch args");
        let left_arguments = left_args
            .get("arguments")
            .and_then(|v| v.as_object())
            .expect("left dispatch arguments");
        assert_eq!(
            left_arguments.get("agent").and_then(|v| v.as_str()),
            Some("diff-narrator"),
            "fan-out left should be diff-narrator"
        );
        let right_args = dispatch_both.on_enter[1]
            .args
            .as_object()
            .expect("right dispatch args");
        let right_arguments = right_args
            .get("arguments")
            .and_then(|v| v.as_object())
            .expect("right dispatch arguments");
        assert_eq!(
            right_arguments.get("agent").and_then(|v| v.as_str()),
            Some("code-reviewer"),
            "fan-out right should be code-reviewer"
        );
        let wait_both = spec.nodes.get("WaitBoth").expect("WaitBoth node");
        assert_eq!(
            wait_both.on_enter.len(),
            2,
            "WaitBoth should wait for two tasks"
        );
        let w1_args = wait_both.on_enter[0].args.as_object().expect("wait args");
        let w1_arguments = w1_args
            .get("arguments")
            .and_then(|v| v.as_object())
            .expect("wait arguments");
        assert!(
            w1_arguments.get("task_id").is_some(),
            "bro_wait should use task_id (snake_case), not taskId"
        );
        let agg = spec.nodes.get("Aggregate").expect("Aggregate node");
        assert!(
            agg.prompt.as_deref().unwrap_or("").contains("left_output")
                && agg.prompt.as_deref().unwrap_or("").contains("right_output"),
            "Aggregate should reference both left_output and right_output"
        );
    }

    #[test]
    fn agent_escalation_uses_gate_branch() {
        let src = include_str!("../../system-defaults/agents/workflows/escalation.json");
        let spec = load_workflow(src).unwrap();
        let cheap = spec.nodes.get("CheapAttempt").expect("CheapAttempt node");
        assert!(
            cheap.gate.is_some(),
            "CheapAttempt should have a gate packet"
        );
        assert_eq!(
            cheap.gate.as_deref(),
            Some("domain:agents/escalation-judge")
        );
        assert_eq!(cheap.on_enter.len(), 2, "should have dispatch + wait hooks");
        let dispatch_args = cheap.on_enter[0].args.as_object().expect("dispatch args");
        let dispatch_arguments = dispatch_args
            .get("arguments")
            .and_then(|v| v.as_object())
            .expect("dispatch arguments");
        assert_eq!(
            dispatch_arguments.get("agent").and_then(|v| v.as_str()),
            Some("diff-narrator"),
            "escalation cheap agent should be diff-narrator"
        );
        let wait_args = cheap.on_enter[1].args.as_object().expect("wait args");
        let wait_arguments = wait_args
            .get("arguments")
            .and_then(|v| v.as_object())
            .expect("wait arguments");
        assert!(
            wait_arguments.get("task_id").is_some(),
            "bro_wait should use task_id (snake_case), not taskId"
        );
        match &cheap.next {
            super::NodeTransition::Branch { cases, default, .. } => {
                assert!(cases.contains_key("escalate"));
                assert_eq!(cases.get("escalate").map(String::as_str), Some("Escalate"));
                assert_eq!(default.as_deref(), Some("Done"));
            }
            other => panic!("CheapAttempt next should be Branch, got: {other:?}"),
        }
        let escalate = spec.nodes.get("Escalate").expect("Escalate node");
        assert_eq!(
            escalate.on_enter.len(),
            2,
            "Escalate should dispatch + wait"
        );
        let esc_args = escalate.on_enter[0]
            .args
            .as_object()
            .expect("escalate dispatch args");
        let esc_arguments = esc_args
            .get("arguments")
            .and_then(|v| v.as_object())
            .expect("escalate dispatch arguments");
        assert_eq!(
            esc_arguments.get("agent").and_then(|v| v.as_str()),
            Some("code-reviewer"),
            "escalation expensive agent should be code-reviewer"
        );
        let esc_wait_args = escalate.on_enter[1]
            .args
            .as_object()
            .expect("escalate wait args");
        let esc_wait_arguments = esc_wait_args
            .get("arguments")
            .and_then(|v| v.as_object())
            .expect("escalate wait arguments");
        assert!(
            esc_wait_arguments.get("task_id").is_some(),
            "Escalate bro_wait should use task_id"
        );
        assert!(
            spec.vars_schema
                .as_ref()
                .is_some_and(|vs| vs.contains_key("expensive_output")),
            "vars_schema should include expensive_output for completed escalation wait"
        );
    }

    #[test]
    fn agent_escalation_gate_packet_matches_workflow_domain() {
        let workflow_src = include_str!("../../system-defaults/agents/workflows/escalation.json");
        let packet_src = include_str!("../../system-defaults/agents/packets/escalation-judge.json");
        let spec = load_workflow(workflow_src).unwrap();
        let cheap = spec.nodes.get("CheapAttempt").expect("CheapAttempt node");
        assert_eq!(
            cheap.gate.as_deref(),
            Some("domain:agents/escalation-judge")
        );

        let params: crate::packets::CompileParams = serde_json::from_str(packet_src).unwrap();
        assert_eq!(params.domain, "agents/escalation-judge");
        let tmp = tempfile::tempdir().unwrap();
        let packets = crate::packets::Packets::open(tmp.path()).unwrap();
        let compile_message = packets.compile(&params).unwrap();
        let loaded = packets.load("domain:agents/escalation-judge").unwrap();
        assert!(
            compile_message.contains(&loaded.id),
            "compile message should name loaded packet id: {compile_message}"
        );

        let escalate = crate::packets::apply(
            &loaded,
            &serde_json::json!({
                "vars": {
                    "cheap_output": {
                        "result": "confidence-low: missing coverage"
                    }
                }
            }),
        )
        .expect("confidence-low should escalate");
        assert_eq!(escalate.classification, "escalate");
        assert_eq!(escalate.consequent.to_json(), serde_json::json!("Escalate"));

        let pass = crate::packets::apply(
            &loaded,
            &serde_json::json!({
                "vars": {
                    "cheap_output": {
                        "result": "coverage is sufficient"
                    }
                }
            }),
        )
        .expect("fallback should pass");
        assert_eq!(pass.classification, "pass");
        assert_eq!(pass.consequent.to_json(), serde_json::json!("Done"));
    }

    // ── reviewer-arc-with-identity structural tests ───────────────────────────

    #[test]
    fn reviewer_arc_with_identity_compiles() {
        let src = include_str!("../../examples/keystone/workflows/reviewer-arc-with-identity.json");
        let spec = load_workflow(src).unwrap();
        compile(spec).expect("reviewer-arc-with-identity should compile");
    }

    #[test]
    fn reviewer_arc_with_identity_contains_require_identity_op() {
        let src = include_str!("../../examples/keystone/workflows/reviewer-arc-with-identity.json");
        let spec = load_workflow(src).unwrap();
        let request_node = spec
            .nodes
            .get("RequestIdentity")
            .expect("RequestIdentity node must exist");
        let has_require_identity = request_node
            .on_enter
            .iter()
            .any(|h| matches!(h.op, super::ops::OpKind::RequireIdentity));
        assert!(
            has_require_identity,
            "RequestIdentity.on_enter must include a require_identity op"
        );
    }

    #[test]
    fn reviewer_arc_with_identity_no_env_forgejo_token() {
        let src = include_str!("../../examples/keystone/workflows/reviewer-arc-with-identity.json");
        assert!(
            !src.contains("env.FORGEJO_TOKEN"),
            "reviewer-arc-with-identity must not use ${{env.FORGEJO_TOKEN}} — \
             use the mapped identity token_ref via secret_headers instead"
        );
    }

    #[test]
    fn reviewer_arc_with_identity_await_identity_wait_node() {
        let src = include_str!("../../examples/keystone/workflows/reviewer-arc-with-identity.json");
        let spec = load_workflow(src).unwrap();
        let await_node = spec
            .nodes
            .get("AwaitIdentity")
            .expect("AwaitIdentity node must exist");
        let wait = await_node
            .wait
            .as_ref()
            .expect("AwaitIdentity must have a wait spec");
        let has_provisioned_signal = wait
            .any_of
            .iter()
            .any(|w| w.signal == "bro.identity.provisioned");
        assert!(
            has_provisioned_signal,
            "AwaitIdentity.wait must listen for bro.identity.provisioned"
        );
    }

    #[test]
    fn reviewer_arc_with_identity_preserves_base_node_topology() {
        // Identity extension must NOT collapse the reviewer arc into a
        // Review→PostFeedback shortcut. The control flow after identity
        // readiness mirrors the base reviewer-arc: FetchDiff → Review →
        // Aggregate → PostReview.
        let src = include_str!("../../examples/keystone/workflows/reviewer-arc-with-identity.json");
        let spec = load_workflow(src).unwrap();
        for required in ["FetchDiff", "Review", "Aggregate", "PostReview"] {
            assert!(
                spec.nodes.contains_key(required),
                "reviewer-arc-with-identity must contain node '{required}'"
            );
        }
        // No PostFeedback shortcut.
        assert!(
            !spec.nodes.contains_key("PostFeedback"),
            "reviewer-arc-with-identity must not contain a PostFeedback shortcut node"
        );
    }

    #[test]
    fn reviewer_arc_with_identity_fetch_diff_captures_pr_diff_via_secret_headers() {
        let src = include_str!("../../examples/keystone/workflows/reviewer-arc-with-identity.json");
        let v: serde_json::Value = serde_json::from_str(src).unwrap();
        let fetch = v
            .pointer("/nodes/FetchDiff")
            .expect("FetchDiff node must exist");
        let hooks = fetch
            .get("on_enter")
            .and_then(|v| v.as_array())
            .expect("FetchDiff.on_enter");
        let http_op = hooks
            .iter()
            .find(|h| h.get("op").and_then(|v| v.as_str()) == Some("http_json"))
            .expect("FetchDiff must have http_json op");
        assert_eq!(
            http_op.get("into_var").and_then(|v| v.as_str()),
            Some("pr_diff"),
            "FetchDiff must capture into 'pr_diff'"
        );
        let args = http_op.get("args").unwrap();
        assert_eq!(
            args.get("response_kind").and_then(|v| v.as_str()),
            Some("text"),
            "FetchDiff must request text response for the .diff URL"
        );
        assert_eq!(
            args.get("allow_empty_body").and_then(|v| v.as_bool()),
            Some(true)
        );
        let secret_hdrs = args
            .get("secret_headers")
            .expect("FetchDiff must use secret_headers for auth");
        let auth = secret_hdrs
            .get("Authorization")
            .and_then(|v| v.as_str())
            .expect("Authorization in secret_headers");
        assert!(
            auth.contains("identity_result"),
            "FetchDiff Authorization must reference vars.identity_result.identity.token_ref"
        );
        // Review prompt must consume the captured diff.
        let prompt = v
            .pointer("/nodes/Review/prompt")
            .and_then(|p| p.as_str())
            .expect("Review.prompt");
        assert!(
            prompt.contains("${vars.pr_diff}"),
            "Review.prompt must reference ${{vars.pr_diff}} from FetchDiff capture"
        );
    }

    #[test]
    fn reviewer_arc_with_identity_post_review_parses_aggregator_output() {
        let src = include_str!("../../examples/keystone/workflows/reviewer-arc-with-identity.json");
        let v: serde_json::Value = serde_json::from_str(src).unwrap();
        let post_review = v
            .pointer("/nodes/PostReview")
            .expect("PostReview node must exist");
        let hooks = post_review
            .get("on_enter")
            .and_then(|v| v.as_array())
            .expect("PostReview.on_enter");
        let parse = hooks
            .iter()
            .find(|h| h.get("op").and_then(|v| v.as_str()) == Some("parse_json"))
            .expect("PostReview must include parse_json before HTTP calls");
        assert_eq!(
            parse
                .get("args")
                .and_then(|a| a.get("from"))
                .and_then(|v| v.as_str()),
            Some("${Aggregate.output}"),
            "parse_json.from must be ${{Aggregate.output}}"
        );
        assert_eq!(
            parse.get("into_var").and_then(|v| v.as_str()),
            Some("review_payload"),
            "parse_json must capture into review_payload"
        );
        // feedback_posted must be set at the tail.
        let sets_flag = hooks.iter().any(|h| {
            h.get("op").and_then(|v| v.as_str()) == Some("set_var")
                && h.pointer("/args/key").and_then(|v| v.as_str()) == Some("feedback_posted")
        });
        assert!(sets_flag, "PostReview must set feedback_posted=true");
    }

    #[test]
    fn reviewer_arc_with_identity_all_http_json_uses_secret_headers() {
        let src = include_str!("../../examples/keystone/workflows/reviewer-arc-with-identity.json");
        let v: serde_json::Value = serde_json::from_str(src).unwrap();
        let nodes = v.get("nodes").and_then(|n| n.as_object()).unwrap();
        let mut http_op_count = 0usize;
        for (node_name, node) in nodes {
            let hooks = match node.get("on_enter").and_then(|v| v.as_array()) {
                Some(h) => h,
                None => continue,
            };
            for hook in hooks {
                if hook.get("op").and_then(|v| v.as_str()) != Some("http_json") {
                    continue;
                }
                http_op_count += 1;
                let args = hook.get("args").unwrap();
                assert!(
                    args.get("headers")
                        .and_then(|h| h.get("Authorization"))
                        .is_none(),
                    "node {node_name}: http_json must not put Authorization in plain headers"
                );
                let secret_hdrs = args.get("secret_headers").unwrap_or_else(|| {
                    panic!("node {node_name}: http_json must use secret_headers")
                });
                let auth = secret_hdrs
                    .get("Authorization")
                    .and_then(|v| v.as_str())
                    .unwrap_or_else(|| panic!("node {node_name}: Authorization in secret_headers"));
                assert!(
                    auth.contains("identity_result"),
                    "node {node_name}: Authorization must reference vars.identity_result.identity.token_ref, got '{auth}'"
                );
            }
        }
        // FetchDiff, PostReview review POST, PostReview merge POST = 3 http_json ops.
        assert!(
            http_op_count >= 3,
            "reviewer-arc-with-identity must have at least 3 http_json ops (FetchDiff, review POST, merge POST), found {http_op_count}"
        );
    }

    // ── Phase 7 acceptance: implementer + top-level identity workflows ─────

    #[test]
    fn implementer_arc_with_identity_compiles_and_includes_require_identity() {
        let src =
            include_str!("../../examples/keystone/workflows/implementer-arc-with-identity.json");
        let spec = load_workflow(src).expect("implementer-arc-with-identity must parse");
        assert_eq!(spec.name, "implementer-arc-with-identity");
        let request = spec
            .nodes
            .get("RequestIdentity")
            .expect("RequestIdentity node must exist");
        let has_require_identity = request
            .on_enter
            .iter()
            .any(|h| matches!(h.op, super::ops::OpKind::RequireIdentity));
        assert!(
            has_require_identity,
            "RequestIdentity.on_enter must include a require_identity op"
        );
    }

    #[test]
    fn implementer_arc_with_identity_no_env_forgejo_token() {
        let src =
            include_str!("../../examples/keystone/workflows/implementer-arc-with-identity.json");
        assert!(
            !src.contains("env.FORGEJO_TOKEN"),
            "implementer-arc-with-identity must not use ${{env.FORGEJO_TOKEN}} — \
             use the mapped identity token_ref via secret_headers instead"
        );
    }

    #[test]
    fn implementer_arc_with_identity_await_node_listens_for_provisioned() {
        let src =
            include_str!("../../examples/keystone/workflows/implementer-arc-with-identity.json");
        let spec = load_workflow(src).unwrap();
        let await_node = spec
            .nodes
            .get("AwaitIdentity")
            .expect("AwaitIdentity node must exist");
        let wait = await_node
            .wait
            .as_ref()
            .expect("AwaitIdentity must have a wait spec");
        assert!(
            wait.any_of
                .iter()
                .any(|w| w.signal == "bro.identity.provisioned"),
            "AwaitIdentity.wait must listen for bro.identity.provisioned"
        );
    }

    #[test]
    fn implementer_arc_with_identity_http_calls_use_secret_headers() {
        let src =
            include_str!("../../examples/keystone/workflows/implementer-arc-with-identity.json");
        let v: serde_json::Value = serde_json::from_str(src).unwrap();
        let nodes = v.get("nodes").and_then(|n| n.as_object()).unwrap();
        for (node_name, node) in nodes {
            let on_enter = match node.get("on_enter").and_then(|v| v.as_array()) {
                Some(arr) => arr,
                None => continue,
            };
            for hook in on_enter {
                if hook.get("op").and_then(|v| v.as_str()) != Some("http_json") {
                    continue;
                }
                let args = hook.get("args").expect("http_json args");
                // No raw Authorization in plain headers.
                let plain_auth = args.get("headers").and_then(|h| h.get("Authorization"));
                assert!(
                    plain_auth.is_none(),
                    "node {node_name}: http_json must not put Authorization in plain headers"
                );
                let secret_hdrs = args
                    .get("secret_headers")
                    .expect("node http_json must use secret_headers for auth");
                let auth = secret_hdrs
                    .get("Authorization")
                    .and_then(|v| v.as_str())
                    .expect("Authorization in secret_headers");
                assert!(
                    auth.contains("identity_result"),
                    "node {node_name}: Authorization must reference vars.identity_result.identity.token_ref, got '{auth}'"
                );
            }
        }
    }

    #[test]
    fn issue_to_merged_pr_with_identity_compiles_and_wires_identity_subworkflows() {
        let src =
            include_str!("../../examples/keystone/workflows/issue-to-merged-pr-with-identity.json");
        let spec = load_workflow(src).expect("identity top-level workflow must parse");
        assert_eq!(spec.name, "issue-to-merged-pr-with-identity");

        let v: serde_json::Value = serde_json::from_str(src).unwrap();
        let nodes = v.get("nodes").and_then(|n| n.as_object()).unwrap();

        let implement_ref = nodes
            .get("Implement")
            .and_then(|n| n.get("subworkflow_ref"))
            .and_then(|v| v.as_str())
            .expect("Implement.subworkflow_ref");
        assert_eq!(implement_ref, "implementer-arc-with-identity");

        let review_ref = nodes
            .get("Review")
            .and_then(|n| n.get("subworkflow_ref"))
            .and_then(|v| v.as_str())
            .expect("Review.subworkflow_ref");
        assert_eq!(review_ref, "reviewer-arc-with-identity");

        // Both subworkflows must receive forgejo_instance via imports.
        for node_name in ["Implement", "Review"] {
            let imports = nodes
                .get(node_name)
                .and_then(|n| n.get("imports"))
                .and_then(|v| v.as_array())
                .unwrap_or_else(|| panic!("{node_name}.imports"));
            let strs: Vec<&str> = imports.iter().filter_map(|v| v.as_str()).collect();
            assert!(
                strs.contains(&"forgejo_instance"),
                "{node_name}.imports must include forgejo_instance, got {strs:?}"
            );
        }
    }

    #[test]
    fn issue_to_merged_pr_with_identity_declares_forgejo_instance_var() {
        let src =
            include_str!("../../examples/keystone/workflows/issue-to-merged-pr-with-identity.json");
        let spec = load_workflow(src).unwrap();
        let schema = spec
            .vars_schema
            .as_ref()
            .expect("vars_schema must be declared");
        assert!(
            schema.contains_key("forgejo_instance"),
            "top-level identity workflow must declare forgejo_instance in vars_schema"
        );
    }

    #[test]
    fn identity_smoke_script_exists_and_asserts_distinct_principals() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/examples/keystone/scripts/identity-smoke.sh"
        );
        let content = std::fs::read_to_string(path)
            .expect("identity-smoke.sh must exist alongside install.sh");
        assert!(content.starts_with("#!"), "script must have a shebang");
        // The acceptance contract: the script must explicitly fail when the
        // implementer and reviewer Forgejo principals are the same user.
        assert!(
            content.contains("PR_AUTHOR") && content.contains("REVIEW_AUTHOR"),
            "script must compare PR_AUTHOR and REVIEW_AUTHOR"
        );
        assert!(
            content.contains("same Forgejo user") || content.contains("self-approval"),
            "script must mention self-approval / same-user failure mode"
        );
        // Must verify the typed identity mappings exist for both bros.
        for bro in ["keystone-impl", "keystone-review"] {
            assert!(
                content.contains(bro),
                "smoke script must check identity mapping for {bro}"
            );
        }
    }

    #[test]
    fn identity_smoke_script_uses_only_real_daemon_surfaces() {
        // Negative coverage: the script must NOT reference any of the
        // non-existent admin endpoints that were in the first draft. The
        // daemon does not expose these — see src/main.rs admin routes.
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/examples/keystone/scripts/identity-smoke.sh"
        );
        let content = std::fs::read_to_string(path).unwrap();
        for nonexistent in [
            "/admin/workflow/list",
            "/admin/reaction/list",
            "/admin/workflow/start",
            "/admin/identity/list",
        ] {
            assert!(
                !content.contains(nonexistent),
                "smoke script must not reference nonexistent endpoint {nonexistent}"
            );
        }
        // Positive coverage: the script must use the real dispatch endpoint
        // with the real request shape documented in src/server/routes.rs.
        for required in [
            "/orchestrate/by-id",
            "workflow_id",
            "project_dir",
            "initial_vars",
            "PROJECT_DIR",
        ] {
            assert!(
                content.contains(required),
                "smoke script must reference real dispatch surface fragment {required}"
            );
        }
    }

    #[test]
    fn install_script_installs_identity_workflows() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/examples/keystone/scripts/install.sh"
        );
        let content = std::fs::read_to_string(path).expect("install.sh must exist");
        for wf in [
            "implementer-arc-with-identity",
            "reviewer-arc-with-identity",
            "issue-to-merged-pr-with-identity",
        ] {
            assert!(
                content.contains(wf),
                "install.sh must install identity workflow {wf}"
            );
        }
        // Base workflows must remain because implementer-feedback-arc is
        // identity-agnostic and is still referenced by the identity wrapper.
        for wf in [
            "implementer-arc",
            "reviewer-arc",
            "issue-to-merged-pr",
            "implementer-feedback-arc",
        ] {
            assert!(
                content.contains(wf),
                "install.sh must continue to install base workflow {wf}"
            );
        }
    }

    #[test]
    fn identity_workflow_model_ids_match_installer_catalog() {
        // The Keystone install.sh creates the brofiles with the catalog
        // model IDs `claude-sonnet-4-6` and `claude-haiku-4-5-20251001`.
        // Identity keys are `(scope, instance, subject, provider, model)`,
        // so the `require_identity.model` field MUST match the brofile's
        // catalog id — otherwise the workflow checks a different identity
        // from the one the dispatch will actually run with.
        let impl_src =
            include_str!("../../examples/keystone/workflows/implementer-arc-with-identity.json");
        let v: serde_json::Value = serde_json::from_str(impl_src).unwrap();
        let req = v
            .pointer("/nodes/RequestIdentity/on_enter/0/args/model")
            .and_then(|m| m.as_str())
            .expect("implementer require_identity model");
        assert_eq!(
            req, "claude-sonnet-4-6",
            "implementer-arc-with-identity must request the claude-sonnet-4-6 brofile identity"
        );

        let rev_src =
            include_str!("../../examples/keystone/workflows/reviewer-arc-with-identity.json");
        let v: serde_json::Value = serde_json::from_str(rev_src).unwrap();
        let req = v
            .pointer("/nodes/RequestIdentity/on_enter/0/args/model")
            .and_then(|m| m.as_str())
            .expect("reviewer require_identity model");
        assert_eq!(
            req, "claude-haiku-4-5-20251001",
            "reviewer-arc-with-identity must request the claude-haiku-4-5-20251001 brofile identity"
        );

        // And those catalog IDs must actually be wired in install.sh —
        // a one-shot belt-and-suspenders check that future installer edits
        // do not silently desync.
        let install = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/examples/keystone/scripts/install.sh"
        ))
        .unwrap();
        assert!(
            install.contains("claude-sonnet-4-6"),
            "install.sh must upsert a brofile with model claude-sonnet-4-6"
        );
        assert!(
            install.contains("claude-haiku-4-5-20251001"),
            "install.sh must upsert a brofile with model claude-haiku-4-5-20251001"
        );
    }

    #[test]
    fn existing_non_identity_keystone_workflows_still_compile() {
        for src in [
            include_str!("../../examples/keystone/workflows/implementer-arc.json"),
            include_str!("../../examples/keystone/workflows/reviewer-arc.json"),
            include_str!("../../examples/keystone/workflows/issue-to-merged-pr.json"),
            include_str!("../../examples/keystone/workflows/implementer-feedback-arc.json"),
        ] {
            load_workflow(src).expect("base keystone workflow must still parse");
        }
    }

    #[test]
    fn gate_identity_status_packet_compiles() {
        let src = include_str!("../../examples/keystone/packets/gate-identity-status.json");
        let params: crate::packets::CompileParams = serde_json::from_str(src)
            .expect("gate-identity-status.json should parse as CompileParams");
        let tmp = tempfile::tempdir().unwrap();
        let packets = crate::packets::Packets::open(tmp.path()).unwrap();
        packets
            .compile(&params)
            .expect("gate-identity-status packet should compile");
        let loaded = packets
            .load("domain:workflow-gate/identity-status")
            .expect("should load compiled gate");

        // ready verdict when status == "ready"
        let ready = crate::packets::apply(
            &loaded,
            &serde_json::json!({"vars": {"identity_result": {"status": "ready"}}}),
        )
        .expect("ready entity should match");
        assert_eq!(ready.classification, "ready");

        // pending verdict on fallback
        let pending = crate::packets::apply(
            &loaded,
            &serde_json::json!({"vars": {"identity_result": {"status": "pending"}}}),
        )
        .expect("pending entity should match");
        assert_eq!(pending.classification, "pending");
    }
}
