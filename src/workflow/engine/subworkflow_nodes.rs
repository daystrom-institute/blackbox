use std::collections::HashSet;

use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value, json};

use super::{MAX_COMPOSITION_DEPTH, WorkflowRunner, run_workflow_at_depth_with_cancel};
use crate::server::workflow_capabilities::validate_workflow_capabilities;

impl<'a> WorkflowRunner<'a> {
    /// Recursively run a sub-workflow embedded in a node. The parent
    /// node's output becomes the concatenated labeled outputs of every
    /// activity node in the sub-workflow (stable order). The sub-arc
    /// runs in its own compiled context but under the same server and
    /// project_dir — so it opens its own arc thread, applies its own
    /// gates, etc. Depth-limited to prevent runaway recursion.
    pub(super) async fn run_subworkflow_node(&mut self, node_id: &str) -> Result<()> {
        self.ensure_not_cancelled(node_id, "subworkflow_entry")?;
        let spec = self
            .compiled
            .spec
            .nodes
            .get(node_id)
            .ok_or_else(|| anyhow!("no metadata for subworkflow node '{node_id}'"))?
            .clone();
        let sub_spec = if let Some(inline) = &spec.subworkflow {
            (**inline).clone()
        } else if let Some(id) = &spec.subworkflow_ref {
            self.server
                .resolve_workflow_by_id(id)
                .ok_or_else(|| anyhow!("subworkflow_ref '{id}' on node '{node_id}' not in registry — install via bro_workflow_install"))?
        } else {
            bail!("subworkflow missing from node '{node_id}' (neither inline nor ref set)");
        };

        // Depth is threaded through `run_workflow_at_depth`. Check at
        // dispatch time so the error surfaces here (with node context)
        // rather than inside the child runner's error return.
        let child_depth = self.composition_depth + 1;
        if child_depth > MAX_COMPOSITION_DEPTH {
            bail!(
                "subworkflow recursion would exceed ceiling {MAX_COMPOSITION_DEPTH} at node '{node_id}' (current depth {}, child would be {child_depth})",
                self.composition_depth
            );
        }

        self.log_event(
            "subworkflow_begin",
            json!({
                "node": node_id,
                "sub_name": sub_spec.name.clone(),
                "sub_version": sub_spec.version,
                "parent_depth": self.composition_depth,
                "child_depth": child_depth,
            }),
        );
        self.arc_note(
            "learned",
            &format!(
                "node '{node_id}' entering sub-workflow '{}' (depth {child_depth}/{MAX_COMPOSITION_DEPTH})",
                sub_spec.name
            ),
        );

        let compiled = super::super::compile(sub_spec)
            .map_err(|e| anyhow!("subworkflow on node '{node_id}' failed to compile: {e}"))?;
        // Capability validation on the resolved sub-spec. Without this
        // a brofile/team can change after install and a referenced
        // subworkflow silently dispatches against a now-incapable
        // provider. Ref-resolution is dispatch-time, so this check
        // must happen here, not at parent compile.
        validate_workflow_capabilities(&compiled, &self.server.state)
            .map_err(|e| anyhow!("subworkflow on node '{node_id}' capability validation: {e}"))?;
        let project_dir = self.project_dir.clone();
        // Seed the sub-runner with the parent's node_outputs so sub
        // prompts can reference `${ParentNode.output}` the same way
        // siblings do. Caveat: sub-node names that collide with parent
        // names overwrite parent entries in the sub's local copy.
        let seed_outputs = self.node_outputs.clone();

        // Compute initial_vars for the sub: explicit imports list +
        // import_renames (extractor expressions on parent context).
        // import_renames takes precedence; missing import keys are a
        // soft warning (sub may be tolerant).
        let mut initial_vars: Map<String, Value> = Map::new();
        for k in &spec.imports {
            if let Some(v) = self.ctx.vars.get(k) {
                initial_vars.insert(k.clone(), v.clone());
            } else {
                self.log_event(
                    "subworkflow_import_missing",
                    json!({"node": node_id, "import": k}),
                );
            }
        }
        for (local_name, parent_path) in &spec.import_renames {
            match self.ctx.resolve(parent_path) {
                Some(v) => {
                    initial_vars.insert(local_name.clone(), v);
                }
                None => {
                    self.log_event(
                        "subworkflow_rename_unresolved",
                        json!({
                            "node": node_id,
                            "local": local_name,
                            "parent_path": parent_path,
                        }),
                    );
                }
            }
        }

        let parent_arc_id = self.ctx.meta.arc_id.clone();
        // Box the recursive future to avoid infinitely-sized types.
        // Depth is threaded so the child runner knows where it is in
        // the composition tree.
        let sub_result = Box::pin(run_workflow_at_depth_with_cancel(
            self.server,
            &compiled,
            project_dir,
            Some(25),
            child_depth,
            seed_outputs,
            initial_vars,
            Some(parent_arc_id),
            Some(self.cancel_token.clone()),
            None,
        ))
        .await;

        if !sub_result.status.starts_with("completed") {
            bail!(
                "subworkflow '{}' did not complete cleanly: {}",
                compiled.spec.name,
                sub_result.status
            );
        }

        // Merge sub-node outputs into a single labeled string — same
        // shape as ensemble output so downstream templates can consume
        // it consistently. Filter OUT seeded parent outputs (keys not
        // in the sub's own node set) so the merge reflects only what
        // the sub produced, not what it received as input context.
        let sub_node_names: HashSet<String> = compiled.spec.nodes.keys().cloned().collect();
        let mut sub_outputs: Vec<(String, String)> = sub_result
            .node_outputs
            .into_iter()
            .filter(|(k, _)| sub_node_names.contains(k))
            .collect();
        sub_outputs.sort_by(|a, b| a.0.cmp(&b.0));
        let merged = sub_outputs
            .iter()
            .map(|(n, o)| format!("── sub:{n} ──\n{o}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        self.record_output(node_id, merged.clone());

        // Promote declared exports from sub vars back into parent.
        // Missing exports are a runtime error — declared contract.
        for k in &spec.exports {
            match sub_result.vars.get(k) {
                Some(v) => {
                    if let Err(e) =
                        self.ctx
                            .set_var(k, v.clone(), self.compiled.spec.vars_schema.as_ref())
                    {
                        bail!(
                            "subworkflow '{}' export '{k}' parent-write rejected: {e}",
                            compiled.spec.name
                        );
                    }
                    self.log_event(
                        "subworkflow_export",
                        json!({"node": node_id, "key": k, "value": v.clone()}),
                    );
                }
                None => {
                    bail!(
                        "subworkflow '{}' did not export declared key '{k}' (have: {:?})",
                        compiled.spec.name,
                        sub_result.vars.keys().collect::<Vec<_>>()
                    );
                }
            }
        }

        self.log_event(
            "subworkflow_complete",
            json!({
                "node": node_id,
                "sub_arc_thread_id": sub_result.arc_thread_id,
                "sub_events": sub_result.events.len(),
                "sub_node_count": sub_outputs.len(),
                "exports_promoted": spec.exports.len(),
                "merged_bytes": merged.len(),
            }),
        );
        self.arc_note(
            "done",
            &format!(
                "sub-workflow '{}' completed ({} sub-nodes) — arc={}",
                compiled.spec.name,
                sub_outputs.len(),
                sub_result.arc_thread_id.as_deref().unwrap_or("?")
            ),
        );
        Ok(())
    }
}
