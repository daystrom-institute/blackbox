use super::*;
use crate::workflow::{compile, schema};

impl<'a> WorkflowRunner<'a> {
    pub(super) async fn run_dynamic_fanout_node(
        &mut self,
        node_id: &str,
        spec: &schema::NodeSpec,
    ) -> Result<()> {
        let runtime = self.materialize_fanout_runtime(node_id, spec)?;
        let total = runtime.items.len();
        if total > MAX_FOREACH_ITEMS {
            bail!(
                "node '{node_id}' {} materialized {total} items, above ceiling {MAX_FOREACH_ITEMS}",
                runtime.kind
            );
        }
        let child_depth = self.composition_depth + 1;
        if child_depth > MAX_COMPOSITION_DEPTH {
            bail!(
                "{} recursion would exceed ceiling {MAX_COMPOSITION_DEPTH} at node '{node_id}' (current depth {}, child would be {child_depth})",
                runtime.kind,
                self.composition_depth
            );
        }

        let compiled = compile(runtime.child_workflow.clone()).map_err(|e| {
            anyhow!(
                "{} child on node '{node_id}' failed to compile: {e}",
                runtime.kind
            )
        })?;
        crate::validate_workflow_capabilities(&compiled, &self.server.state).map_err(|e| {
            anyhow!(
                "{} child on node '{node_id}' capability validation: {e}",
                runtime.kind
            )
        })?;

        let plans = self.build_fanout_child_plans(node_id, &runtime)?;
        let mut results: Vec<Option<Value>> = vec![None; total];
        let mut first_failure: Option<String> = None;
        let seed_outputs = self.node_outputs.clone();
        let project_dir = self.project_dir.clone();
        let parent_arc_id = self.ctx.meta.arc_id.clone();
        let group_cancel = self.cancel_token.child_token();
        let effective_parallelism = runtime.parallelism.clamp(1, MAX_FOREACH_PARALLELISM);
        let mut joinset = tokio::task::JoinSet::new();
        let mut next_to_start = 0usize;
        let mut stop_new_dispatch = false;

        self.log_event(
            "fanout_begin",
            json!({
                "node": node_id,
                "kind": runtime.kind,
                "items": total,
                "parallelism": runtime.parallelism,
                "effective_parallelism": effective_parallelism,
                "collect_into": runtime.collect_into,
                "on_item_failure": runtime.failure_policy,
            }),
        );

        while next_to_start < total || !joinset.is_empty() {
            if self.cancel_token.is_cancelled() {
                group_cancel.cancel();
                bail!("arc cancelled");
            }

            while !stop_new_dispatch
                && next_to_start < total
                && joinset.len() < effective_parallelism
            {
                let plan = plans[next_to_start].clone();
                self.log_event(
                    "fanout_item_start",
                    json!({
                        "node": node_id,
                        "index": plan.index,
                        "key": plan.key,
                    }),
                );
                let state = self.server.state.clone();
                let compiled_for_child = compiled.clone();
                let project_dir_for_child = project_dir.clone();
                let seed_outputs_for_child = seed_outputs.clone();
                let parent_arc_id_for_child = parent_arc_id.clone();
                let cancel_for_child = group_cancel.clone();
                let exports_for_child = runtime.exports.clone();
                joinset.spawn_blocking(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build();
                    let Ok(runtime) = runtime else {
                        return FanoutChildOutcome {
                            index: plan.index,
                            key: plan.key,
                            item: plan.item,
                            status: "error".into(),
                            exports: Map::new(),
                            outputs: Map::new(),
                            arc_id: String::new(),
                            arc_thread_id: None,
                            error: Some("failed to build fanout child runtime".into()),
                        };
                    };
                    runtime.block_on(run_fanout_child_owned(
                        state,
                        compiled_for_child,
                        project_dir_for_child,
                        child_depth,
                        seed_outputs_for_child,
                        parent_arc_id_for_child,
                        cancel_for_child,
                        plan,
                        exports_for_child,
                    ))
                });
                next_to_start += 1;
            }

            if joinset.is_empty() {
                break;
            }
            let outcome = match joinset.join_next().await {
                Some(Ok(outcome)) => outcome,
                Some(Err(err)) => {
                    first_failure.get_or_insert_with(|| format!("fanout child task join: {err}"));
                    stop_new_dispatch = true;
                    group_cancel.cancel();
                    continue;
                }
                None => break,
            };
            let failed = outcome.is_failure();
            let failure_msg = outcome
                .error
                .clone()
                .unwrap_or_else(|| format!("item {} status {}", outcome.index, outcome.status));
            self.log_event(
                if failed {
                    "fanout_item_failed"
                } else {
                    "fanout_item_complete"
                },
                json!({
                    "node": node_id,
                    "index": outcome.index,
                    "key": outcome.key,
                    "status": outcome.status,
                    "error": outcome.error,
                }),
            );
            let idx = outcome.index;
            results[idx] = Some(outcome.into_value());
            if failed
                && first_failure.is_none()
                && !matches!(runtime.failure_policy, ItemFailurePolicy::Continue)
            {
                first_failure = Some(failure_msg);
                stop_new_dispatch = true;
                if matches!(runtime.failure_policy, ItemFailurePolicy::Halt) {
                    group_cancel.cancel();
                }
            }
        }

        if stop_new_dispatch {
            for idx in next_to_start..total {
                if results[idx].is_none() {
                    results[idx] = Some(fanout_skipped_value(idx, &plans[idx]));
                }
            }
        }

        let collected = Value::Array(results.into_iter().flatten().collect());
        self.ctx.set_var(
            &runtime.collect_into,
            collected.clone(),
            self.compiled.spec.vars_schema.as_ref(),
        )?;
        self.record_output(node_id, collected.to_string());
        self.log_event(
            "fanout_complete",
            json!({
                "node": node_id,
                "kind": runtime.kind,
                "items": total,
                "collected": collected.as_array().map(Vec::len).unwrap_or_default(),
                "collect_into": runtime.collect_into,
                "failed": first_failure.is_some(),
            }),
        );
        self.arc_note(
            if first_failure.is_some() {
                "blocked"
            } else {
                "done"
            },
            &format!(
                "{} node '{node_id}' collected {} item result(s) into vars.{}",
                runtime.kind,
                collected.as_array().map(Vec::len).unwrap_or_default(),
                runtime.collect_into
            ),
        );

        if let Some(error) = first_failure {
            bail!(
                "{} node '{node_id}' failed under {:?}: {error}",
                runtime.kind,
                runtime.failure_policy
            );
        }
        Ok(())
    }

    fn materialize_fanout_runtime(
        &self,
        node_id: &str,
        spec: &schema::NodeSpec,
    ) -> Result<FanoutRuntime> {
        match (&spec.foreach, &spec.matrix) {
            (Some(foreach), None) => self.materialize_foreach_runtime(node_id, foreach),
            (None, Some(matrix)) => self.materialize_matrix_runtime(node_id, matrix),
            _ => bail!("node '{node_id}' expected exactly one of foreach or matrix"),
        }
    }

    fn materialize_foreach_runtime(
        &self,
        node_id: &str,
        spec: &ForeachSpec,
    ) -> Result<FanoutRuntime> {
        let items_value = resolve_arg_value(&self.ctx, &spec.items)
            .map_err(|e| anyhow!("foreach node '{node_id}' items: {e}"))?;
        let items = items_value
            .as_array()
            .cloned()
            .ok_or_else(|| anyhow!("foreach node '{node_id}' items resolved to non-array"))?;
        Ok(FanoutRuntime {
            kind: "foreach",
            items,
            as_var: spec.as_var.clone(),
            index_as: spec.index_as.clone(),
            key_template: spec.key.clone(),
            child_workflow: self.resolve_fanout_child_workflow(
                node_id,
                spec.subworkflow.as_deref(),
                spec.subworkflow_ref.as_deref(),
            )?,
            imports: spec.imports.clone(),
            import_renames: spec.import_renames.clone(),
            exports: spec.exports.clone(),
            parallelism: spec.parallelism.unwrap_or(1),
            collect_into: spec.collect.into_var.clone(),
            failure_policy: spec.on_item_failure.clone(),
        })
    }

    fn materialize_matrix_runtime(
        &self,
        node_id: &str,
        spec: &MatrixSpec,
    ) -> Result<FanoutRuntime> {
        let mut axes: Vec<(String, Vec<Value>)> = Vec::new();
        for axis in &spec.axes {
            let values = resolve_arg_value(&self.ctx, &axis.values)
                .map_err(|e| anyhow!("matrix node '{node_id}' axis '{}': {e}", axis.name))?;
            let values = values.as_array().cloned().ok_or_else(|| {
                anyhow!(
                    "matrix node '{node_id}' axis '{}' resolved to non-array",
                    axis.name
                )
            })?;
            axes.push((axis.name.clone(), values));
        }
        let items = expand_matrix_items(node_id, &axes)?;
        Ok(FanoutRuntime {
            kind: "matrix",
            items,
            as_var: spec.as_var.clone(),
            index_as: spec.index_as.clone(),
            key_template: spec.key.clone(),
            child_workflow: self.resolve_fanout_child_workflow(
                node_id,
                spec.subworkflow.as_deref(),
                spec.subworkflow_ref.as_deref(),
            )?,
            imports: spec.imports.clone(),
            import_renames: spec.import_renames.clone(),
            exports: spec.exports.clone(),
            parallelism: spec.parallelism.unwrap_or(1),
            collect_into: spec.collect.into_var.clone(),
            failure_policy: spec.on_item_failure.clone(),
        })
    }

    fn resolve_fanout_child_workflow(
        &self,
        node_id: &str,
        inline: Option<&Workflow>,
        reference: Option<&str>,
    ) -> Result<Workflow> {
        if let Some(inline) = inline {
            return Ok(inline.clone());
        }
        let id =
            reference.ok_or_else(|| anyhow!("fanout node '{node_id}' missing child workflow"))?;
        self.server
            .resolve_workflow_by_id(id)
            .ok_or_else(|| anyhow!("subworkflow_ref '{id}' on fanout node '{node_id}' not in registry — install via bro_workflow_install"))
    }

    fn build_fanout_child_plans(
        &mut self,
        node_id: &str,
        runtime: &FanoutRuntime,
    ) -> Result<Vec<FanoutChildPlan>> {
        let mut plans = Vec::with_capacity(runtime.items.len());
        let mut keys = HashSet::new();
        for (idx, item) in runtime.items.iter().cloned().enumerate() {
            let mut initial_vars: Map<String, Value> = Map::new();
            for k in &runtime.imports {
                if let Some(v) = self.ctx.vars.get(k) {
                    initial_vars.insert(k.clone(), v.clone());
                } else {
                    self.log_event(
                        "fanout_import_missing",
                        json!({"node": node_id, "index": idx, "import": k}),
                    );
                }
            }
            for (local_name, parent_path) in &runtime.import_renames {
                match self.ctx.resolve(parent_path) {
                    Some(v) => {
                        initial_vars.insert(local_name.clone(), v);
                    }
                    None => {
                        self.log_event(
                            "fanout_rename_unresolved",
                            json!({
                                "node": node_id,
                                "index": idx,
                                "local": local_name,
                                "parent_path": parent_path,
                            }),
                        );
                    }
                }
            }
            initial_vars.insert(runtime.as_var.clone(), item.clone());
            if let Some(index_as) = &runtime.index_as {
                initial_vars.insert(index_as.clone(), json!(idx));
            }

            let key = if let Some(template) = &runtime.key_template {
                let mut item_ctx = self.ctx.clone();
                item_ctx.vars.insert(runtime.as_var.clone(), item.clone());
                if let Some(index_as) = &runtime.index_as {
                    item_ctx.vars.insert(index_as.clone(), json!(idx));
                }
                item_ctx.render_template(template)
            } else {
                idx.to_string()
            };
            if key.trim().is_empty() {
                bail!(
                    "{} node '{node_id}' item {idx} rendered an empty key",
                    runtime.kind
                );
            }
            if !keys.insert(key.clone()) {
                bail!(
                    "{} node '{node_id}' rendered duplicate item key '{key}'",
                    runtime.kind
                );
            }
            plans.push(FanoutChildPlan {
                index: idx,
                key,
                item,
                initial_vars,
            });
        }
        Ok(plans)
    }
}

#[derive(Clone)]
pub(super) struct FanoutRuntime {
    pub(super) kind: &'static str,
    pub(super) items: Vec<Value>,
    pub(super) as_var: String,
    pub(super) index_as: Option<String>,
    pub(super) key_template: Option<String>,
    pub(super) child_workflow: Workflow,
    pub(super) imports: Vec<String>,
    pub(super) import_renames: HashMap<String, String>,
    pub(super) exports: Vec<String>,
    pub(super) parallelism: usize,
    pub(super) collect_into: String,
    pub(super) failure_policy: ItemFailurePolicy,
}

#[derive(Clone)]
pub(super) struct FanoutChildPlan {
    pub(super) index: usize,
    pub(super) key: String,
    pub(super) item: Value,
    pub(super) initial_vars: Map<String, Value>,
}

pub(super) struct FanoutChildOutcome {
    pub(super) index: usize,
    pub(super) key: String,
    pub(super) item: Value,
    pub(super) status: String,
    pub(super) exports: Map<String, Value>,
    pub(super) outputs: Map<String, Value>,
    pub(super) arc_id: String,
    pub(super) arc_thread_id: Option<String>,
    pub(super) error: Option<String>,
}

pub(super) async fn run_fanout_child_owned(
    state: Arc<crate::SharedState>,
    compiled: CompiledWorkflow,
    project_dir: Option<String>,
    child_depth: u32,
    seed_outputs: HashMap<String, String>,
    parent_arc_id: String,
    parent_cancel_token: CancellationToken,
    plan: FanoutChildPlan,
    exports: Vec<String>,
) -> FanoutChildOutcome {
    let server = BlackboxServer::new(state);
    run_fanout_child(
        &server,
        compiled,
        project_dir,
        child_depth,
        seed_outputs,
        parent_arc_id,
        parent_cancel_token,
        plan,
        exports,
    )
    .await
}

pub(super) async fn run_fanout_child(
    server: &BlackboxServer,
    compiled: CompiledWorkflow,
    project_dir: Option<String>,
    child_depth: u32,
    seed_outputs: HashMap<String, String>,
    parent_arc_id: String,
    parent_cancel_token: CancellationToken,
    plan: FanoutChildPlan,
    exports: Vec<String>,
) -> FanoutChildOutcome {
    let result = Box::pin(run_workflow_at_depth_with_cancel(
        server,
        &compiled,
        project_dir,
        Some(25),
        child_depth,
        seed_outputs,
        plan.initial_vars,
        Some(parent_arc_id),
        Some(parent_cancel_token),
        None,
    ))
    .await;

    let mut export_values = Map::new();
    let mut error = None;
    if result.status.starts_with("completed") {
        for key in &exports {
            match result.vars.get(key) {
                Some(value) => {
                    export_values.insert(key.clone(), value.clone());
                }
                None => {
                    error = Some(format!(
                        "child workflow '{}' did not export declared key '{key}'",
                        compiled.spec.name
                    ));
                    break;
                }
            }
        }
    } else {
        error = Some(result.status.clone());
    }

    let outputs = result
        .node_outputs
        .into_iter()
        .map(|(key, value)| (key, Value::String(value)))
        .collect();
    let status = if error.is_some() {
        if result.status == "cancelled" {
            "cancelled".to_string()
        } else {
            "error".to_string()
        }
    } else {
        "completed".to_string()
    };
    FanoutChildOutcome {
        index: plan.index,
        key: plan.key,
        item: plan.item,
        status,
        exports: export_values,
        outputs,
        arc_id: result.arc_id,
        arc_thread_id: result.arc_thread_id,
        error,
    }
}

pub(super) fn fanout_skipped_value(index: usize, plan: &FanoutChildPlan) -> Value {
    json!({
        "index": index,
        "key": plan.key,
        "item": plan.item,
        "status": "skipped",
        "exports": {},
        "outputs": {},
        "error": "not dispatched after earlier item failure"
    })
}

pub(super) fn expand_matrix_items(
    node_id: &str,
    axes: &[(String, Vec<Value>)],
) -> Result<Vec<Value>> {
    let mut items: Vec<Map<String, Value>> = vec![Map::new()];
    for (axis_name, values) in axes {
        let projected = items
            .len()
            .checked_mul(values.len())
            .ok_or_else(|| anyhow!("matrix node '{node_id}' item count overflow"))?;
        if projected > MAX_FOREACH_ITEMS {
            bail!(
                "matrix node '{node_id}' would materialize {projected} items, above ceiling {MAX_FOREACH_ITEMS}"
            );
        }
        let mut next = Vec::with_capacity(projected);
        for base in &items {
            for value in values {
                let mut item = base.clone();
                item.insert(axis_name.clone(), value.clone());
                next.push(item);
            }
        }
        items = next;
    }
    Ok(items.into_iter().map(Value::Object).collect())
}

impl FanoutChildOutcome {
    fn is_failure(&self) -> bool {
        self.status != "completed"
    }

    fn into_value(self) -> Value {
        let mut out = Map::new();
        out.insert("index".into(), json!(self.index));
        out.insert("key".into(), Value::String(self.key));
        out.insert("item".into(), self.item);
        out.insert("status".into(), Value::String(self.status));
        out.insert("exports".into(), Value::Object(self.exports));
        out.insert("outputs".into(), Value::Object(self.outputs));
        if !self.arc_id.is_empty() {
            out.insert("arc_id".into(), Value::String(self.arc_id));
        }
        if let Some(thread_id) = self.arc_thread_id {
            out.insert("arc_thread_id".into(), Value::String(thread_id));
        }
        if let Some(error) = self.error {
            out.insert("error".into(), Value::String(error));
        }
        Value::Object(out)
    }
}
