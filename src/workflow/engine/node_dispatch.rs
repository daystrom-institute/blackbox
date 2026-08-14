use anyhow::{Result, anyhow, bail};
use serde_json::json;

use crate::workflow::{ActorKind, NodeMode, NodeTransition};

use super::WorkflowRunner;

impl WorkflowRunner<'_> {
    pub(super) async fn run_node(&mut self, node_id: &str) -> Result<()> {
        self.ensure_not_cancelled(node_id, "before_node")?;
        // wait_for: explicit fan-in. Join any listed in-flight sources
        // before running the node body so their outputs are available
        // for prompt rendering / gate evaluation.
        let wait_for: Vec<String> = self
            .compiled
            .spec
            .nodes
            .get(node_id)
            .map(|n| n.wait_for.clone())
            .unwrap_or_default();
        if !wait_for.is_empty() {
            let mut joined = 0usize;
            let mut already = 0usize;
            for src in &wait_for {
                if self.join_in_flight_source(src).await? {
                    joined += 1;
                } else {
                    already += 1;
                }
            }
            self.log_event(
                "fan_in",
                json!({
                    "node": node_id,
                    "wait_for": wait_for,
                    "joined": joined,
                    "already_completed": already,
                }),
            );
        }
        self.ensure_not_cancelled(node_id, "after_wait_for")?;
        self.run_activity_node(node_id).await
    }

    async fn run_fork_dispatch(&mut self, node_id: &str) -> Result<()> {
        let branches: Vec<String> = self
            .compiled
            .spec
            .nodes
            .get(node_id)
            .and_then(|n| match &n.next {
                NodeTransition::Fork { branches, .. } => Some(branches.clone()),
                _ => None,
            })
            .unwrap_or_default();
        if branches.is_empty() {
            return Ok(());
        }
        self.log_event(
            "fork",
            json!({
                "node": node_id,
                "branches": branches.clone(),
            }),
        );
        for target in branches {
            self.dispatch_fire_and_forget(&target).await?;
        }
        Ok(())
    }

    async fn run_activity_node(&mut self, node_id: &str) -> Result<()> {
        self.ensure_not_cancelled(node_id, "before_activity")?;
        let spec = self
            .compiled
            .spec
            .nodes
            .get(node_id)
            .ok_or_else(|| anyhow!("no metadata for activity node '{node_id}'"))?
            .clone();

        // Checkpoint resume re-enters a Wait node whose on_enter hooks
        // already ran before the restart; re-running them would repeat
        // non-idempotent setup (worktree creation, var seeding). The
        // marker is one-shot and node-scoped.
        let skip_on_enter = match self.resume_skip_on_enter.take() {
            Some(marked) if marked == node_id => true,
            Some(marked) => {
                self.resume_skip_on_enter = Some(marked);
                false
            }
            None => false,
        };
        // on_enter hooks fire BEFORE every node body — including
        // subworkflow descents, Wait registrations, and actor
        // dispatches. They set up state (worktree, vars, branch
        // names) and are the right place to run setup ops.
        if !spec.on_enter.is_empty() && !skip_on_enter {
            self.run_hooks(&spec.on_enter, &format!("{node_id}/on_enter"))
                .await?;
        }
        self.ensure_not_cancelled(node_id, "after_on_enter")?;

        // Dynamic fanout: foreach/matrix nodes own child
        // sub-workflow dispatch and collection; they otherwise pass
        // through the ordinary on_exit/gate/next boundary.
        if spec.foreach.is_some() || spec.matrix.is_some() {
            self.run_dynamic_fanout_node(node_id, &spec).await?;
            self.run_node_exit_hooks(&spec.on_exit, node_id).await?;
            self.apply_node_gate(node_id, &spec).await?;
            return Ok(());
        }

        // Wait node — suspend the arc on a signal. Mutually exclusive
        // with subworkflow + actor.
        if let Some(wait_spec) = spec.wait.clone() {
            self.run_wait_node(node_id, &wait_spec).await?;
            self.run_node_exit_hooks(&spec.on_exit, node_id).await?;
            self.apply_node_gate(node_id, &spec).await?;
            return Ok(());
        }

        if let Some(sleep_spec) = spec.sleep.clone() {
            self.run_sleep_node(node_id, sleep_spec.duration_ms).await?;
            self.run_node_exit_hooks(&spec.on_exit, node_id).await?;
            self.apply_node_gate(node_id, &spec).await?;
            return Ok(());
        }

        // Sub-workflow composition: if the node embeds a workflow OR
        // references one by id, run it recursively instead of
        // dispatching an actor. The parent node's output becomes the
        // concatenated sub-node outputs.
        if spec.subworkflow.is_some() || spec.subworkflow_ref.is_some() {
            self.ensure_not_cancelled(node_id, "before_subworkflow")?;
            self.run_subworkflow_node(node_id).await?;
            self.run_node_exit_hooks(&spec.on_exit, node_id).await?;
            self.apply_node_gate(node_id, &spec).await?;
            return Ok(());
        }

        if !spec.atom.is_empty() {
            if matches!(spec.mode, NodeMode::FireAndForget) {
                bail!(
                    "node '{node_id}' uses atom binding '{}' with mode=fire_and_forget; atom workflow bindings require synchronous execution in v1",
                    spec.atom
                );
            }
            self.join_late_inject(node_id).await?;

            let visit_count = {
                let c = self.visit_counts.entry(node_id.to_string()).or_insert(0);
                *c += 1;
                *c
            };
            if let Some(retry) = &spec.retry {
                if visit_count > retry.max_generations {
                    bail!(
                        "node '{node_id}' exceeded retry ceiling ({} generations; visited {visit_count} times)",
                        retry.max_generations
                    );
                }
            }

            let binding = self
                .compiled
                .spec
                .atom_bindings
                .get(&spec.atom)
                .ok_or_else(|| {
                    anyhow!(
                        "node '{node_id}' references undeclared atom binding '{}'",
                        spec.atom
                    )
                })?
                .clone();
            self.run_atom_node(node_id, &binding, &spec, visit_count)
                .await?;
            if matches!(spec.next, NodeTransition::Fork { .. }) {
                self.run_fork_dispatch(node_id).await?;
            }
            self.run_node_exit_hooks(&spec.on_exit, node_id).await?;
            self.apply_node_gate(node_id, &spec).await?;
            return Ok(());
        }

        let actor_name = spec.actor.clone();

        // Hook-only / pure-routing node: no actor declared. The
        // rendered prompt becomes the node's captured output (so
        // downstream `${NodeName.output}` references stay legal),
        // hooks have already fired around this point, gate runs as
        // usual. Fork side-dispatch fires here too.
        if actor_name.is_empty() {
            let raw_template = spec.prompt.as_deref().unwrap_or("");
            let prompt = self.render_prompt(raw_template);
            self.record_output(node_id, prompt.clone());
            self.log_event(
                "node_complete_hookless",
                json!({"node": node_id, "output_bytes": prompt.len()}),
            );
            if matches!(spec.next, NodeTransition::Fork { .. }) {
                self.run_fork_dispatch(node_id).await?;
            }
            self.run_node_exit_hooks(&spec.on_exit, node_id).await?;
            self.apply_node_gate(node_id, &spec).await?;
            return Ok(());
        }

        let actor = self.compiled.spec.actors.get(&actor_name).ok_or_else(|| {
            anyhow!("node '{node_id}' references undeclared actor '{actor_name}'")
        })?;
        // Fire-and-forget on the main walk: dispatch and store the
        // handle, then advance without waiting. Downstream late_inject
        // consumers will join.
        if matches!(spec.mode, NodeMode::FireAndForget) {
            self.dispatch_fire_and_forget(node_id).await?;
            return Ok(());
        }

        // Late-inject join — if this node's spec references an
        // in-flight source, wait for it and fold its output into
        // node_outputs before rendering this node's prompt template.
        self.join_late_inject(node_id).await?;

        // Retry ceiling — every visit bumps the count; if we exceed the
        // node's `retry.max_generations`, halt the arc. This is the
        // circuit-breaker from daystrom's generation-tracking pattern.
        let visit_count = {
            let c = self.visit_counts.entry(node_id.to_string()).or_insert(0);
            *c += 1;
            *c
        };
        if let Some(retry) = &spec.retry {
            if visit_count > retry.max_generations {
                bail!(
                    "node '{node_id}' exceeded retry ceiling ({} generations; visited {visit_count} times)",
                    retry.max_generations
                );
            }
        }

        let raw_template = spec.prompt.as_deref().unwrap_or("");
        let mut prompt = self.render_prompt(raw_template);
        if visit_count > 1 {
            // Prepend retry context so the retried bro sees the prior
            // gate verdict. Durable actors also see their own prior
            // turn via session continuity; non-durable actors get the
            // verdict string as the only signal.
            let verdict = self.last_verdict.as_deref().unwrap_or("(no verdict)");
            prompt = format!(
                "[retry — attempt {visit_count}, prior gate verdict: {verdict}]\n\n{prompt}"
            );
        }

        let actor_failure = spec.actor_failure.unwrap_or_default();
        match &actor.kind {
            ActorKind::Executor => {
                self.run_executor_node(node_id, actor, &actor_name, &prompt, actor_failure)
                    .await?;
            }
            ActorKind::Ensemble => {
                self.run_ensemble_node(node_id, actor, &actor_name, &prompt)
                    .await?;
            }
        }

        // Fork dispatch: if this activity node's `next` is a Fork,
        // spawn the side-branches fire-and-forget AFTER the main
        // body has captured its output.
        if matches!(spec.next, NodeTransition::Fork { .. }) {
            self.run_fork_dispatch(node_id).await?;
        }

        // on_exit hooks — fire AFTER actor return but BEFORE gate so
        // the gate sees normalized output (e.g. after a ParseJson
        // hook stuffs structured data into vars).
        self.run_node_exit_hooks(&spec.on_exit, node_id).await?;

        self.apply_node_gate(node_id, &spec).await?;
        Ok(())
    }
}
