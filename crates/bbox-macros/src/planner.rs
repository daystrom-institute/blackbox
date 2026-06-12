//! Macro planner (M3).
//!
//! Converts a [`MacroInvocation`] + registry-resolved [`MacroDefinition`] into
//! a [`MacroPlan`] review artifact, and lowers [`MacroPlan`] to a
//! [`RefactorPlan`] for [`bbox_refactor::apply`].
//!
//! # Pipeline (8 hard constraints from design)
//!
//! 1. **Version**: `invocation.version` must exactly equal `def.version` when set.
//! 2. **Inputs**: validated against `def.inputs_schema` (required-field presence +
//!    JSON type check; full jsonschema deferred to a later phase).
//! 3. **Context**: [`expr::Context`] built from inputs; populated incrementally as
//!    probes execute (Phase 4 / P4b).
//! 4. **Probes** (P4b): `def.probes` executed in declared order via
//!    `ctx.probe_runner`. Each result is inserted into `Context.probes[name]`
//!    immediately so later probes and refusal predicates can reference it. A probe
//!    error propagates as a planning error (fail closed). Unknown-root validation
//!    is **phase-aware**: refusal predicates may only reference `"inputs"` or
//!    top-level `def.probes` names (inline-operation probe names are excluded —
//!    they don't exist yet when refusals evaluate). Validation guards and Record
//!    op bodies may additionally reference inline probe names. Any root outside
//!    the applicable set is a hard `error.unknown_context_root`.
//! 5. **Refusals**: each `def.refusals[].when` predicate is evaluated; any match
//!    short-circuits planning and returns a plan containing only the refusals (empty
//!    `EditSet`, no apply).
//! 6. **Operations** processed in definition order:
//!    - `Emit`/`Rewrite` → call `ctx.backend.emit/rewrite`; propagate
//!      `error.backend_unavailable` as a planning error (fail closed; never silently
//!      downgrades to template_only).
//!    - `DelegateRefactor` → call `refactor::plan_with_ctx` with `output_path=None`
//!      forced; deserialize result as `RefactorPlan` (analysis-only / custom-output
//!      kinds are rejected); reject `Blocked`/`Errored` plan statuses; merge edits
//!      into the aggregate `EditSet`; detect duplicate touched paths.
//!    - `Validate` → `"parse"` lowers to `ValidationStep::TreeSitterNoErrors` entries;
//!      compile/LSP/test checks stay as `MacroPlanCheck` metadata only.
//!    - `Record` → appended to `MacroPlan.questions`.
//! 7. **RX-V1 authority boundary**: `acknowledge_*` keys are stripped from def
//!    delegate params; ONLY `invocation.operator_opt_outs` injects authority into
//!    delegates (as `toml_entries[flag] = true`). `MacroPlan.operator_opt_outs_used`
//!    = UNION of flags ACTUALLY CONSUMED by delegated plans — not the raw invocation
//!    list.
//! 8. **Semantic status**: per-op statuses are compared; `Mixed` when any two ops
//!    differ. Ordering (worst → best): `template_only < syntax_only < indexed_hints
//!    < lsp_verified_partial < lsp_verified`.
//!
//! # Lowering
//!
//! [`MacroPlanner::lower`] maps `MacroPlan → RefactorPlan`:
//! - `EditSet.{file_edits → edits, file_creates, file_moves}`
//! - `MacroPlan.checks` (kind=`"parse"`) → `RefactorPlan.validations`
//! - `operator_opt_outs_used` carried verbatim
//! - `semantic_status` = worst concrete tier across **mutating ops only**
//!   (kinds `"probe"` and `"record"` are excluded — probe status must not
//!   leak into the lowered `RefactorPlan` tier).
//!
//! Lowering is **refused** when any mutating operation has `template_only`
//! semantic status. The caller must connect the Java backend (Phase 3) and
//! re-plan before applying.

use std::collections::{HashMap, HashSet};

use anyhow::{Context as _, Result, anyhow, bail};
use serde_json::Value;

use crate::backend::{JavaEmitOp, JavaRewriteOp};
use crate::expr;
use crate::model::{
    EditSet, MacroDefinition, MacroInvocation, MacroOperation, MacroPlan, MacroPlanCheck,
    MacroPlanOperation, MacroRefusalHit, MacroSemanticStatus,
};
use crate::planner_ctx::MacroPlannerContext;
use crate::probe::ProbeSpec;
use bbox_refactor::{
    self, PlanContext, RefactorApplyParams, RefactorPlan, RefactorPlanParams, SemanticStatus,
    ValidationStep,
};

// ── Authority constants ──────────────────────────────────────────────────────

/// Prefix for operator authority opt-out keys.
///
/// Keys with this prefix MUST NOT originate from macro definitions or delegate
/// params — they can only be injected by the planner from
/// `invocation.operator_opt_outs` (RX-V1).
const AUTHORITY_PREFIX: &str = "acknowledge_";

// ─────────────────────────────────────────────────────────────────────────────
// Public API
// ─────────────────────────────────────────────────────────────────────────────

/// Stateless macro planner.
///
/// [`MacroPlanner::plan`] executes probes via the injectable [`ProbeRunner`] (read-only,
/// no file mutations) and is therefore **not pure** — it performs I/O through the
/// runner. [`MacroPlanner::lower`] remains pure (no I/O, no side effects on `self`).
pub struct MacroPlanner;

impl MacroPlanner {
    /// Plan a macro invocation.
    ///
    /// Returns a [`MacroPlan`] review artifact describing what the macro will do.
    /// Never writes files. Call [`MacroPlanner::lower`] on the result, then
    /// [`bbox_refactor::apply`] to execute.
    ///
    /// # Errors
    ///
    /// - Version mismatch (`invocation.version` vs `def.version`)
    /// - Missing required inputs or type mismatch
    /// - Probe execution failure (fail closed; any probe error is a planning error)
    /// - Backend unavailable for `Emit`/`Rewrite` ops
    /// - `DelegateRefactor` plan failure, blocked/errored plan, or dup paths
    pub fn plan(
        invocation: &MacroInvocation,
        def: &MacroDefinition,
        ctx: &MacroPlannerContext,
    ) -> Result<MacroPlan> {
        // ── Constraint 1: version check ──────────────────────────────────────
        if let Some(ref req_version) = invocation.version {
            if req_version != &def.version {
                bail!(
                    "error.version_mismatch: invocation requested version '{}' but macro '{}' \
                     resolved to version '{}' in the registry. Pin the exact registered version \
                     or omit the version field to use whatever is registered.",
                    req_version,
                    def.id,
                    def.version
                );
            }
        }

        // ── Constraint 2: validate inputs ────────────────────────────────────
        validate_inputs(&invocation.inputs, &def.inputs_schema)?;

        // ── Constraint 3: build expr::Context (populated incrementally by probes) ─
        // Apply inputs_schema `default` values for any input the operator omitted,
        // so `${inputs.*}` interpolation and predicates see declared defaults.
        let mut ctx_inputs = invocation.inputs.clone();
        apply_input_schema_defaults(&mut ctx_inputs, &def.inputs_schema);
        let mut expr_ctx = expr::Context {
            inputs: ctx_inputs,
            probes: HashMap::new(),
            locals: HashMap::new(),
        };

        // ── Constraint 4: execute pre-refusal probes in declared order ────────
        //
        // Each probe result is inserted into `expr_ctx.probes[name]` immediately
        // so the next probe (and later refusal predicates) can reference it.
        // A probe error propagates as a planning error (fail closed).
        let mut edit_set = EditSet::default();
        let mut plan_ops: Vec<MacroPlanOperation> = vec![];
        let mut plan_checks: Vec<MacroPlanCheck> = vec![];
        let mut plan_questions: Vec<String> = vec![];
        let mut op_statuses: Vec<MacroSemanticStatus> = vec![];
        let mut backends_used: HashSet<String> = HashSet::new();
        // Constraint 7: collect ONLY consumed flags from delegates
        let mut opt_outs_used: HashSet<String> = HashSet::new();
        // Dup-path guard across all operations
        let mut touched_paths: HashSet<String> = HashSet::new();
        // Pending Rewrite content: path → (new_content, original_sha256_on_disk, original_byte_len).
        //
        // `original_sha256_on_disk` and `original_byte_len` are established by the
        // FIRST rewrite for each file (disk-read path) and remain frozen for all
        // subsequent chained rewrites.  Subsequent ops receive the latest composed
        // content as their sidecar source_text while the FileEdit is always built
        // with `byte_end = original_byte_len` and `original_sha256 = S0` so that
        // refactor::apply's range check and SHA verify both target the on-disk
        // original (not the intermediate C1 content).
        let mut pending_rewrite_content: HashMap<String, (String, String, usize)> = HashMap::new();
        // Probe provenance summaries for MacroPlan.provenance
        let mut probe_summaries: Vec<Value> = vec![];

        // ── Authority gate enforcement ────────────────────────────────────────
        //
        // Runs BEFORE probes so a missing-authority invocation never triggers
        // any read-only I/O (LSP queries, file reads). Gate checks depend only
        // on def.authority_gates and invocation.operator_opt_outs — no probe
        // results are needed.
        //
        // RX-V1 invariant: agents must NOT default or infer gate presence ("delta
        // looks small" is not a reason). Only explicit operator_opt_outs supply
        // authority.
        let mut gate_refusals: Vec<MacroRefusalHit> = vec![];
        for gate in &def.authority_gates {
            if invocation.operator_opt_outs.iter().any(|o| o == gate) {
                // Gate supplied — record in the audit set.
                opt_outs_used.insert(gate.clone());
            } else {
                // Gate missing — collect a typed refusal hit.
                gate_refusals.push(MacroRefusalHit {
                    code: "error.authority_required".to_string(),
                    message: format!(
                        "Authority gate '{}' is required to invoke macro '{}' but was not \
                         present in operator_opt_outs. Add '{}' to the invocation's \
                         operator_opt_outs to acknowledge this effect.",
                        gate, def.id, gate
                    ),
                });
            }
        }
        if !gate_refusals.is_empty() {
            // Carry opt_outs_used (supplied gates) so the audit field is accurate even
            // on refusal — the operator can see which gates they got right.
            let mut opt_outs_vec: Vec<String> = opt_outs_used.iter().cloned().collect();
            opt_outs_vec.sort();
            return Ok(MacroPlan {
                macro_id: def.id.clone(),
                summary: format!(
                    "Macro '{}' refused: {} missing authority gate(s)",
                    def.id,
                    gate_refusals.len()
                ),
                semantic_status: MacroSemanticStatus::TemplateOnly,
                operations: plan_ops,
                edits: EditSet::default(),
                checks: vec![],
                questions: vec![],
                refusals: gate_refusals,
                backends_used: vec![],
                operator_opt_outs_used: opt_outs_vec,
                provenance: build_provenance(def, &probe_summaries),
            });
        }

        for probe in &def.probes {
            // Interpolate probe spec string leaves using the current context
            // (inputs are available; probe results from earlier probes are also
            // available since context is populated incrementally).
            // Only `inputs.*` references are meaningful here — probe specs must
            // NOT reference other probe names (they run before most probes), but
            // the interpolate call is safe: unresolvable paths surface as errors.
            let interpolated_spec =
                interpolate_value_strings(&probe.spec, &expr_ctx).map_err(|e| {
                    anyhow!(
                        "error.probe_spec_interpolation: macro '{}' probe '{}' spec \
                         interpolation failed: {}",
                        def.id,
                        probe.name,
                        e
                    )
                })?;
            let spec: ProbeSpec =
                serde_json::from_value(interpolated_spec.clone()).with_context(|| {
                    format!(
                        "error.probe_spec_invalid: macro '{}' probe '{}' has an invalid or \
                         unknown spec kind after interpolation. Spec: {}",
                        def.id, probe.name, interpolated_spec
                    )
                })?;

            // Preserve the underlying error code (error.probe_backend_unavailable /
            // error.lsp_unavailable) in the Display string — a generic context wrapper
            // would mask it, and callers/operators rely on the specific fail-closed code.
            let output = ctx
                .probe_runner
                .run_probe(&probe.name, &spec, &expr_ctx, invocation)
                .map_err(|e| {
                    anyhow!(
                        "error.probe_failed: macro '{}' probe '{}': {e:#}",
                        def.id,
                        probe.name
                    )
                })?;

            // Build provenance summary (name, kind, exists, count, truncated) —
            // do NOT dump full result arrays.
            let probe_kind = probe_spec_kind_str(&spec);
            let probe_exists = output.value.get("exists").cloned();
            let probe_count = output.value.get("count").cloned();
            probe_summaries.push(serde_json::json!({
                "name": probe.name,
                "kind": probe_kind,
                "semantic_status": output.semantic_status,
                "truncated": output.truncated,
                "exists": probe_exists,
                "count": probe_count,
            }));

            // Insert result into context BEFORE processing next probe/refusal.
            expr_ctx
                .probes
                .insert(probe.name.clone(), output.value.clone());

            // Record as a plan operation (kind="probe") — contributes to
            // MacroPlan.semantic_status aggregation but is excluded from
            // lower()'s mutating-tier computation.
            let probe_op_summary = format!(
                "Probe '{}' ({}): exists={}, count={}, truncated={}",
                probe.name,
                probe_kind,
                probe_exists
                    .as_ref()
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "?".into()),
                probe_count
                    .as_ref()
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "?".into()),
                output.truncated,
            );
            op_statuses.push(output.semantic_status.clone());
            plan_ops.push(MacroPlanOperation {
                kind: "probe".to_string(),
                name: Some(probe.name.clone()),
                semantic_status: output.semantic_status,
                summary: probe_op_summary,
            });
        }

        // ── Unknown-root validation (phase-aware) ────────────────────────────
        //
        // REFUSAL phase: predicates and messages evaluate BEFORE inline-operation
        // probes run, so only top-level def.probes names are in scope. Including
        // inline-operation probe names here would silently allow references to
        // context roots that don't exist yet, causing `eval` to return false
        // instead of surfacing the typo as an error.
        //
        // OPERATION/interpolation phase: Record op bodies and validation guards
        // execute after inline probes in operation order, so all declared probe
        // names (top-level + inline) are valid roots there.
        let refusal_allowed_roots: HashSet<String> = {
            let mut s: HashSet<String> = HashSet::new();
            s.insert("inputs".to_string());
            for p in &def.probes {
                s.insert(p.name.clone());
            }
            // Inline-operation probe names are deliberately excluded from the
            // refusal-phase set: they don't exist when refusals are evaluated.
            s
        };
        let op_allowed_roots: HashSet<String> = {
            let mut s = refusal_allowed_roots.clone();
            for op in &def.operations {
                if let MacroOperation::Probe { name, .. } = op {
                    s.insert(name.clone());
                }
            }
            s
        };
        validate_context_roots(def, &refusal_allowed_roots, &op_allowed_roots)?;

        // ── Constraint 5: refusal evaluation (short-circuits all further work) ──
        let refusal_hits: Vec<MacroRefusalHit> = def
            .refusals
            .iter()
            .filter(|r| expr::eval(&r.when, &expr_ctx))
            .map(|r| {
                let message =
                    expr::interpolate(&r.message, &expr_ctx).unwrap_or_else(|_| r.message.clone());
                MacroRefusalHit {
                    code: r.code.clone(),
                    message,
                }
            })
            .collect();

        if !refusal_hits.is_empty() {
            let codes = refusal_hits
                .iter()
                .map(|h| h.code.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Ok(MacroPlan {
                macro_id: def.id.clone(),
                summary: format!("Macro '{}' refused to plan: {}", def.id, codes),
                // Vacuous status for a refused plan — has no mutating output.
                // Include probe ops so the review artifact shows what ran.
                semantic_status: MacroSemanticStatus::TemplateOnly,
                operations: plan_ops,
                edits: EditSet::default(),
                checks: vec![],
                questions: vec![],
                refusals: refusal_hits,
                backends_used: vec![],
                operator_opt_outs_used: vec![],
                provenance: build_provenance(def, &probe_summaries),
            });
        }

        // ── Constraint 6: process operations ─────────────────────────────────

        for op in &def.operations {
            match op {
                MacroOperation::Probe {
                    name,
                    spec: spec_val,
                } => {
                    // Inline probe operation: same execution semantics as
                    // def.probes, but runs in operation order so later ops
                    // can reference its result via expr_ctx.
                    // Interpolate ${inputs.*} (and prior probe results already in
                    // expr_ctx) before decoding to a typed ProbeSpec.
                    let interpolated_spec = interpolate_value_strings(spec_val, &expr_ctx)
                        .map_err(|e| {
                            anyhow!(
                                "error.probe_spec_interpolation: macro '{}' inline Probe '{}' \
                                 spec interpolation failed: {}",
                                def.id,
                                name,
                                e
                            )
                        })?;
                    let spec: ProbeSpec = serde_json::from_value(interpolated_spec.clone())
                        .with_context(|| {
                            format!(
                                "error.probe_spec_invalid: macro '{}' inline Probe operation \
                                 '{}' has an invalid spec after interpolation: {}",
                                def.id, name, interpolated_spec
                            )
                        })?;
                    let output = ctx
                        .probe_runner
                        .run_probe(name, &spec, &expr_ctx, invocation)
                        .map_err(|e| {
                            anyhow!(
                                "error.probe_failed: macro '{}' inline Probe '{}': {e:#}",
                                def.id,
                                name
                            )
                        })?;
                    let probe_kind = probe_spec_kind_str(&spec);
                    let probe_exists = output.value.get("exists").cloned();
                    let probe_count = output.value.get("count").cloned();
                    probe_summaries.push(serde_json::json!({
                        "name": name,
                        "kind": probe_kind,
                        "semantic_status": output.semantic_status,
                        "truncated": output.truncated,
                        "exists": probe_exists,
                        "count": probe_count,
                    }));
                    expr_ctx.probes.insert(name.clone(), output.value.clone());
                    let probe_op_summary = format!(
                        "Probe '{}' ({}): exists={}, count={}, truncated={}",
                        name,
                        probe_kind,
                        probe_exists
                            .as_ref()
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "?".into()),
                        probe_count
                            .as_ref()
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "?".into()),
                        output.truncated,
                    );
                    op_statuses.push(output.semantic_status.clone());
                    plan_ops.push(MacroPlanOperation {
                        kind: "probe".to_string(),
                        name: Some(name.clone()),
                        semantic_status: output.semantic_status,
                        summary: probe_op_summary,
                    });
                }

                MacroOperation::Emit {
                    name,
                    backend_op,
                    when,
                } => {
                    let mut sink = OpSink {
                        edit_set: &mut edit_set,
                        plan_ops: &mut plan_ops,
                        op_statuses: &mut op_statuses,
                        backends_used: &mut backends_used,
                        touched_paths: &mut touched_paths,
                        pending_rewrite_content: &mut pending_rewrite_content,
                    };
                    process_emit(def, ctx, &expr_ctx, name, backend_op, when, &mut sink)?;
                }

                MacroOperation::Rewrite {
                    targets,
                    backend_op,
                    when,
                } => {
                    let mut sink = OpSink {
                        edit_set: &mut edit_set,
                        plan_ops: &mut plan_ops,
                        op_statuses: &mut op_statuses,
                        backends_used: &mut backends_used,
                        touched_paths: &mut touched_paths,
                        pending_rewrite_content: &mut pending_rewrite_content,
                    };
                    process_rewrite(def, ctx, &expr_ctx, targets, backend_op, when, &mut sink)?;
                }

                MacroOperation::ForEach { over, bind, body } => {
                    // Resolve the collection; fail closed if the path is missing
                    // or does not point at an array.
                    let collection = expr_ctx.resolve(over).cloned().ok_or_else(|| {
                        anyhow!(
                            "error.macro_invalid: ForEach 'over' path '{}' did not resolve \
                             in macro '{}'",
                            over,
                            def.id
                        )
                    })?;
                    let elements = match collection {
                        Value::Array(v) => v,
                        _ => bail!(
                            "error.macro_invalid: ForEach 'over' path '{}' in macro '{}' \
                             must resolve to an array",
                            over,
                            def.id
                        ),
                    };

                    let mut expanded = 0usize;
                    for elem in &elements {
                        // Per-item context: clone the base and bind the element
                        // under `bind` so `${bind.*}` / predicate paths `bind.*`
                        // resolve into it. Locals shadow probes within scope.
                        let mut item_ctx = expr_ctx.clone();
                        item_ctx.locals.insert(bind.clone(), elem.clone());

                        let mut sink = OpSink {
                            edit_set: &mut edit_set,
                            plan_ops: &mut plan_ops,
                            op_statuses: &mut op_statuses,
                            backends_used: &mut backends_used,
                            touched_paths: &mut touched_paths,
                            pending_rewrite_content: &mut pending_rewrite_content,
                        };
                        match body.as_ref() {
                            MacroOperation::Emit {
                                name,
                                backend_op,
                                when,
                            } => process_emit(
                                def, ctx, &item_ctx, name, backend_op, when, &mut sink,
                            )?,
                            MacroOperation::Rewrite {
                                targets,
                                backend_op,
                                when,
                            } => process_rewrite(
                                def, ctx, &item_ctx, targets, backend_op, when, &mut sink,
                            )?,
                            // Defense-in-depth: registry validation already rejects
                            // a non-emit/rewrite ForEach body, but fail closed here too.
                            _ => bail!(
                                "error.macro_invalid: ForEach body in macro '{}' must be an \
                                 emit or rewrite operation",
                                def.id
                            ),
                        }
                        expanded += 1;
                    }
                    op_statuses.push(MacroSemanticStatus::SyntaxOnly);
                    plan_ops.push(MacroPlanOperation {
                        kind: "for_each".to_string(),
                        name: None,
                        semantic_status: MacroSemanticStatus::SyntaxOnly,
                        summary: format!("ForEach over '{}': expanded {} item(s)", over, expanded),
                    });
                }

                MacroOperation::DelegateRefactor {
                    refactor_kind,
                    params,
                } => {
                    // Interpolate `${inputs.*}` / probe references in the delegate
                    // params before planning, so a macro can forward its own
                    // inputs to the delegated refactor kind (e.g. builtin.java.guice
                    // forwarding `source`/`target`/`module_name` to
                    // extract_java_class). Mirrors the Emit/Rewrite interpolation.
                    let params = interpolate_value_strings(params, &expr_ctx).map_err(|e| {
                        anyhow!(
                            "error.macro_invalid: DelegateRefactor params in macro '{}' \
                             interpolation failed: {}",
                            def.id,
                            e
                        )
                    })?;
                    // Drop top-level params that resolved to JSON null, so an
                    // omitted optional macro input (declared with a null default,
                    // forwarded as a whole-placeholder `${inputs.x}`) falls back
                    // to the delegated kind's own default rather than forcing an
                    // explicit null. Authority stripping still runs in
                    // plan_delegate after this.
                    let params = match params {
                        Value::Object(mut map) => {
                            map.retain(|_, v| !v.is_null());
                            Value::Object(map)
                        }
                        other => other,
                    };
                    // plan_delegate enforces RX-V1 authority boundary (see fn doc).
                    let (rp, consumed_flags) = plan_delegate(
                        refactor_kind,
                        &params,
                        &invocation.operator_opt_outs,
                        &invocation.project_dir,
                        ctx,
                    )?;

                    // Residue surfacing: carry non-empty advisory fields forward as
                    // review questions so they are NOT silently lost when lower()
                    // constructs the lowered RefactorPlan (which resets them to empty).
                    surface_delegate_residue(refactor_kind, &rp, &mut plan_questions);

                    // Dup-path guard across all delegates
                    for fe in &rp.edits {
                        register_path(&mut touched_paths, &fe.path)?;
                    }
                    for fc in &rp.file_creates {
                        register_path(&mut touched_paths, &fc.path)?;
                    }
                    for fm in &rp.file_moves {
                        register_path(&mut touched_paths, &fm.source_path)?;
                        register_path(&mut touched_paths, &fm.target_path)?;
                    }

                    // Parse-check validations from the delegate plan become
                    // MacroPlanCheck entries (reconstructable at lower time).
                    for v in &rp.validations {
                        match v {
                            ValidationStep::TreeSitterNoErrors { path, .. } => {
                                plan_checks.push(MacroPlanCheck {
                                    check: "parse".to_string(),
                                    targets: vec![path.clone()],
                                });
                            }
                        }
                    }

                    // Merge edit payload
                    edit_set.file_edits.extend(rp.edits.clone());
                    edit_set.file_creates.extend(rp.file_creates.clone());
                    edit_set.file_moves.extend(rp.file_moves.clone());

                    // Constraint 7: union only CONSUMED authority flags
                    opt_outs_used.extend(consumed_flags);

                    let op_status = map_refactor_status(&rp.semantic_status);
                    op_statuses.push(op_status.clone());
                    plan_ops.push(MacroPlanOperation {
                        kind: "delegate_refactor".to_string(),
                        name: Some(refactor_kind.clone()),
                        semantic_status: op_status,
                        summary: format!(
                            "Delegate to refactor kind '{}': {}",
                            refactor_kind, rp.title
                        ),
                    });
                }

                MacroOperation::Validate { check, targets } => {
                    match check.as_str() {
                        // "parse" → TreeSitterNoErrors (lowerable to RefactorPlan.validations)
                        "parse" => {
                            plan_checks.push(MacroPlanCheck {
                                check: "parse".to_string(),
                                targets: targets.clone(),
                            });
                            plan_ops.push(MacroPlanOperation {
                                kind: "validate".to_string(),
                                name: None,
                                semantic_status: MacroSemanticStatus::SyntaxOnly,
                                summary: format!("Parse-validate {} target(s)", targets.len()),
                            });
                        }
                        // compile / test / lsp_no_diagnostics → MacroPlanCheck metadata only
                        other => {
                            plan_checks.push(MacroPlanCheck {
                                check: other.to_string(),
                                targets: targets.clone(),
                            });
                            plan_ops.push(MacroPlanOperation {
                                kind: "validate".to_string(),
                                name: None,
                                semantic_status: MacroSemanticStatus::SyntaxOnly,
                                summary: format!(
                                    "Validate ({}): {} target(s)",
                                    other,
                                    targets.len()
                                ),
                            });
                        }
                    }
                }

                MacroOperation::Record { label, body } => {
                    let text = expr::interpolate(body, &expr_ctx)
                        .unwrap_or_else(|e| format!("{} [interpolation error: {}]", body, e));
                    plan_questions.push(format!("[{}] {}", label, text));
                    plan_ops.push(MacroPlanOperation {
                        kind: "record".to_string(),
                        name: Some(label.clone()),
                        // Record is non-mutating; TemplateOnly status is intentional and
                        // does NOT trigger the lowering refusal (record ops are excluded).
                        semantic_status: MacroSemanticStatus::TemplateOnly,
                        summary: format!("Record note '{}'", label),
                    });
                }
            }
        }

        // ── Post-op: add def-level validations to plan_checks ────────────────
        for v in &def.validations {
            if let Some(guard) = &v.when {
                if !expr::eval(guard, &expr_ctx) {
                    continue;
                }
            }
            plan_checks.push(MacroPlanCheck {
                check: v.check.clone(),
                targets: v.targets.clone(),
            });
        }

        // ── Finalize edit_set.backends_used ──────────────────────────────────
        let mut backends_vec: Vec<String> = backends_used.into_iter().collect();
        backends_vec.sort();
        edit_set.backends_used = backends_vec.clone();

        // ── Constraint 8: aggregate semantic status ──────────────────────────
        let agg_status = aggregate_status(&op_statuses);

        // ── Constraint 7: operator_opt_outs_used = UNION of consumed flags ───
        let mut opt_outs_vec: Vec<String> = opt_outs_used.into_iter().collect();
        opt_outs_vec.sort();

        Ok(MacroPlan {
            macro_id: def.id.clone(),
            summary: format!(
                "Macro '{}' v{}: {} operation(s) planned",
                def.id,
                def.version,
                plan_ops.len()
            ),
            semantic_status: agg_status,
            operations: plan_ops,
            edits: edit_set,
            checks: plan_checks,
            questions: plan_questions,
            refusals: vec![],
            backends_used: backends_vec,
            operator_opt_outs_used: opt_outs_vec,
            provenance: build_provenance(def, &probe_summaries),
        })
    }

    /// Lower a [`MacroPlan`] to a [`RefactorPlan`] for [`bbox_refactor::apply`].
    ///
    /// # Lowering map
    ///
    /// | MacroPlan field               | RefactorPlan field          |
    /// |-------------------------------|------------------------------|
    /// | `edits.file_edits`            | `edits`                     |
    /// | `edits.file_creates`          | `file_creates`              |
    /// | `edits.file_moves`            | `file_moves`                |
    /// | `checks[kind="parse"]`        | `validations`               |
    /// | `operator_opt_outs_used`      | `operator_opt_outs_used`    |
    /// | worst concrete tier across ops| `semantic_status`           |
    ///
    /// # Fail-closed invariant
    ///
    /// Refuses lowering when ANY mutating operation (kind ≠ "record") has
    /// `template_only` semantic status. The plan must be re-generated after
    /// connecting the Java backend (Phase 3).
    pub fn lower(plan: &MacroPlan) -> Result<RefactorPlan> {
        // Refuse template_only on any mutating op (probes and records are not mutating)
        for op in &plan.operations {
            if op.kind != "record"
                && op.kind != "probe"
                && op.semantic_status == MacroSemanticStatus::TemplateOnly
            {
                bail!(
                    "error.template_only_lowering_refused: macro '{}' operation '{}' \
                     (kind='{}') has template_only semantic status. Backend verification \
                     is required before lowering to a RefactorPlan. Connect the Java \
                     backend (Phase 3) and re-plan.",
                    plan.macro_id,
                    op.name.as_deref().unwrap_or("?"),
                    op.kind
                );
            }
        }

        // Pick worst concrete tier for the lowered RefactorPlan
        let semantic_status = worst_refactor_tier(&plan.operations).with_context(|| {
            format!(
                "lowering macro '{}': could not determine semantic tier for RefactorPlan",
                plan.macro_id
            )
        })?;

        // Reconstruct ValidationSteps from parse checks
        let validations: Vec<ValidationStep> = plan
            .checks
            .iter()
            .filter(|c| c.check == "parse")
            .flat_map(|c| {
                c.targets
                    .iter()
                    .map(|t| ValidationStep::TreeSitterNoErrors {
                        path: t.clone(),
                        byte_range: None,
                    })
            })
            .collect();

        Ok(RefactorPlan {
            title: format!("macro:{} — {}", plan.macro_id, plan.summary),
            kind: format!("macro:{}", plan.macro_id),
            semantic_status,
            dry_run: true,
            file_moves: plan.edits.file_moves.clone(),
            file_creates: plan.edits.file_creates.clone(),
            edits: plan.edits.file_edits.clone(),
            validations,
            items: vec![],
            leftovers: vec![],
            captured_variables: vec![],
            remaining_source_accessors: vec![],
            remaining_source_constant_refs: vec![],
            external_calls: vec![],
            inherited_dependencies: vec![],
            deep_analysis: None,
            plan_status: bbox_refactor::PlanStatus::Planned,
            fixme_count: None,
            operator_opt_outs_used: plan.operator_opt_outs_used.clone(),
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Private helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Returns `true` if any string field within a JSON spec value contains `${`.
///
/// Used to detect whether a probe spec contains interpolation placeholders that
/// need to be expanded before deserialization. No longer used as a rejection guard
/// in the planner (probe specs now support `${inputs.*}` interpolation); kept as
/// a utility for diagnostics and the registry's structural validator.
fn spec_contains_interpolation(spec: &Value) -> bool {
    match spec {
        Value::String(s) => s.contains("${"),
        Value::Array(arr) => arr.iter().any(spec_contains_interpolation),
        Value::Object(map) => map.values().any(spec_contains_interpolation),
        _ => false,
    }
}

/// Mutable plan accumulators shared by the per-operation processors.
///
/// Bundling them lets both the top-level operation loop and `ForEach` fan-out
/// write into the same plan state through one `&mut` handle, so a fanned-out
/// `Rewrite` participates in the same `touched_paths` dup-guard and same-file
/// chaining (`pending_rewrite_content`) as a top-level one.
struct OpSink<'a> {
    edit_set: &'a mut EditSet,
    plan_ops: &'a mut Vec<MacroPlanOperation>,
    op_statuses: &'a mut Vec<MacroSemanticStatus>,
    backends_used: &'a mut HashSet<String>,
    touched_paths: &'a mut HashSet<String>,
    pending_rewrite_content: &'a mut HashMap<String, (String, String, usize)>,
}

/// Process one `Emit` operation against `expr_ctx`, writing into `sink`.
///
/// Factored out of the operation loop so `ForEach` can invoke it per element
/// with a per-item context. Behavior is identical to the inline arm: guard
/// skip, interpolate, decode to `JavaEmitOp`, call the backend, register paths.
fn process_emit(
    def: &MacroDefinition,
    ctx: &MacroPlannerContext,
    expr_ctx: &expr::Context,
    name: &str,
    backend_op: &Value,
    when: &Option<expr::Predicate>,
    sink: &mut OpSink<'_>,
) -> Result<()> {
    if let Some(guard) = when {
        if !expr::eval(guard, expr_ctx) {
            sink.op_statuses.push(MacroSemanticStatus::SyntaxOnly);
            sink.plan_ops.push(MacroPlanOperation {
                kind: "emit".to_string(),
                name: Some(name.to_string()),
                semantic_status: MacroSemanticStatus::SyntaxOnly,
                summary: format!("Emit artifact '{}': skipped (guard false)", name),
            });
            return Ok(());
        }
    }
    let interpolated_op = interpolate_value_strings(backend_op, expr_ctx).map_err(|e| {
        anyhow!(
            "error.macro_invalid: Emit operation '{}' in macro '{}' \
             backend_op interpolation failed: {}",
            name,
            def.id,
            e
        )
    })?;
    let typed_emit: JavaEmitOp = serde_json::from_value(interpolated_op).map_err(|e| {
        anyhow!(
            "error.macro_invalid: emit backend_op did not match a typed \
             JavaEmitOp variant: {e}"
        )
    })?;
    let bes = ctx.backend.emit(&typed_emit).with_context(|| {
        format!(
            "error.backend_unavailable: Emit operation '{}' in macro '{}' \
             requires the Java macro backend (Phase 3); the backend is not connected",
            name, def.id
        )
    })?;
    sink.backends_used.insert("java_poet".to_string());
    for fc in &bes.file_creates {
        register_path(sink.touched_paths, &fc.path)?;
    }
    for fe in &bes.file_edits {
        register_path(sink.touched_paths, &fe.path)?;
    }
    sink.edit_set.file_edits.extend(bes.file_edits);
    sink.edit_set.file_creates.extend(bes.file_creates);
    sink.op_statuses.push(MacroSemanticStatus::SyntaxOnly);
    sink.plan_ops.push(MacroPlanOperation {
        kind: "emit".to_string(),
        name: Some(name.to_string()),
        semantic_status: MacroSemanticStatus::SyntaxOnly,
        summary: format!("Emit artifact '{}'", name),
    });
    Ok(())
}

/// Process one `Rewrite` operation against `expr_ctx`, writing into `sink`.
///
/// Factored out of the operation loop so `ForEach` can invoke it per element.
/// Preserves same-file chaining: a prior rewrite's output is fed forward via
/// `pending_rewrite_content`, and a later edit on the same path supersedes the
/// intermediate one so only the final composed edit reaches apply.
fn process_rewrite(
    def: &MacroDefinition,
    ctx: &MacroPlannerContext,
    expr_ctx: &expr::Context,
    targets: &[String],
    backend_op: &Value,
    when: &Option<expr::Predicate>,
    sink: &mut OpSink<'_>,
) -> Result<()> {
    if let Some(guard) = when {
        if !expr::eval(guard, expr_ctx) {
            sink.op_statuses.push(MacroSemanticStatus::SyntaxOnly);
            sink.plan_ops.push(MacroPlanOperation {
                kind: "rewrite".to_string(),
                name: None,
                semantic_status: MacroSemanticStatus::SyntaxOnly,
                summary: format!("Rewrite {} target(s): skipped (guard false)", targets.len()),
            });
            return Ok(());
        }
    }
    let interpolated_op = interpolate_value_strings(backend_op, expr_ctx).map_err(|e| {
        anyhow!(
            "error.macro_invalid: Rewrite operation in macro '{}' \
             backend_op interpolation failed: {}",
            def.id,
            e
        )
    })?;
    let typed_rewrite: JavaRewriteOp = serde_json::from_value(interpolated_op).map_err(|e| {
        anyhow!(
            "error.macro_invalid: rewrite backend_op did not match a typed \
             JavaRewriteOp variant: {e}"
        )
    })?;

    let target_file = rewrite_target_file(&typed_rewrite);
    let source_override = sink
        .pending_rewrite_content
        .get(target_file)
        .map(|(content, sha, orig_len)| (content.as_str(), sha.as_str(), *orig_len));

    let bes = ctx
        .backend
        .rewrite_with_source_override(&typed_rewrite, source_override)
        .with_context(|| {
            format!(
                "error.backend_unavailable: Rewrite operation in macro '{}' \
                 requires the Java macro backend (Phase 3); the backend is not connected",
                def.id
            )
        })?;

    sink.backends_used.insert("open_rewrite".to_string());

    for fe in &bes.file_edits {
        if sink.pending_rewrite_content.contains_key(&fe.path) {
            // Same-file chain: supersede the intermediate edit for this path.
            sink.edit_set.file_edits.retain(|e| e.path != fe.path);
            sink.touched_paths.insert(fe.path.clone());
        } else {
            register_path(sink.touched_paths, &fe.path)?;
        }
        if let Some(new_text) = &fe.new_text {
            let orig_len = fe
                .edits
                .first()
                .map(|e| e.byte_end)
                .unwrap_or(new_text.len());
            sink.pending_rewrite_content.insert(
                fe.path.clone(),
                (new_text.clone(), fe.original_sha256.clone(), orig_len),
            );
        }
    }
    for fc in &bes.file_creates {
        register_path(sink.touched_paths, &fc.path)?;
    }
    sink.edit_set.file_edits.extend(bes.file_edits);
    sink.edit_set.file_creates.extend(bes.file_creates);
    sink.op_statuses.push(MacroSemanticStatus::SyntaxOnly);
    sink.plan_ops.push(MacroPlanOperation {
        kind: "rewrite".to_string(),
        name: None,
        semantic_status: MacroSemanticStatus::SyntaxOnly,
        summary: format!("Rewrite {} target(s) via backend", targets.len()),
    });
    Ok(())
}

/// Extract the target file path from a typed [`JavaRewriteOp`].
///
/// Used by the same-file chaining logic in the `Rewrite` operation handler to
/// look up any pending content from a prior Rewrite on the same file.
fn rewrite_target_file(op: &JavaRewriteOp) -> &str {
    match op {
        JavaRewriteOp::InsertMember { target_file, .. } => target_file.as_str(),
        JavaRewriteOp::ReplaceMethodBody { target_file, .. } => target_file.as_str(),
        JavaRewriteOp::InsertStatementInMethod { target_file, .. } => target_file.as_str(),
        JavaRewriteOp::InsertClassAnnotation { target_file, .. } => target_file.as_str(),
        JavaRewriteOp::DeleteMember { target_file, .. } => target_file.as_str(),
        JavaRewriteOp::InsertFieldAnnotation { target_file, .. } => target_file.as_str(),
        JavaRewriteOp::PruneUnusedImport { target_file, .. } => target_file.as_str(),
    }
}

/// Register a path in `touched`; bail with a clear error on duplicate.
fn register_path(touched: &mut HashSet<String>, path: &str) -> Result<()> {
    if !touched.insert(path.to_string()) {
        bail!(
            "error.duplicate_touched_path: path '{}' appears in multiple macro operations. \
             Each path may only be touched once across all delegate and emit/rewrite operations.",
            path
        );
    }
    Ok(())
}

/// Build a provenance `Value` for a `MacroPlan`.
///
/// `probe_summaries` carries `{name, kind, semantic_status, truncated, exists, count}`
/// for each executed probe — full result arrays are NOT included (audit-only).
fn build_provenance(def: &MacroDefinition, probe_summaries: &[Value]) -> Value {
    serde_json::json!({
        "macro_id": def.id,
        "version": def.version,
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "probes": probe_summaries,
    })
}

/// Validate `inputs` against a JSON Schema object (constraint 2).
///
/// Checks: required-field presence (from `schema.required`) and JSON type
/// matching (from `schema.properties.<key>.type`) for each present key.
/// Full JSON Schema evaluation is deferred to a later phase.
///
/// Fill `inputs` with `default` values declared in the inputs schema for any
/// property the caller omitted. Only inserts absent keys; never overrides a
/// supplied value. No-op when the schema is not an object or has no properties.
fn apply_input_schema_defaults(inputs: &mut serde_json::Map<String, Value>, schema: &Value) {
    let Some(props) = schema
        .as_object()
        .and_then(|o| o.get("properties"))
        .and_then(|p| p.as_object())
    else {
        return;
    };
    for (key, prop) in props {
        if inputs.contains_key(key) {
            continue;
        }
        if let Some(default) = prop.as_object().and_then(|o| o.get("default")) {
            inputs.insert(key.clone(), default.clone());
        }
    }
}

/// Fail-closed: a null/absent schema means no declared inputs (no constraints).
/// A present-but-non-object schema is malformed and is rejected with
/// `error.malformed_inputs_schema` rather than silently disabling validation.
fn validate_inputs(inputs: &serde_json::Map<String, Value>, schema: &Value) -> Result<()> {
    // Null/absent schema = legitimately no declared inputs — no constraints to enforce.
    if schema.is_null() {
        return Ok(());
    }
    let schema_obj = match schema.as_object() {
        Some(o) => o,
        None => bail!(
            "error.malformed_inputs_schema: inputs_schema is present but is not a JSON object \
             (got {}). A non-null inputs_schema must be a JSON object; null or absent schema \
             means no declared inputs.",
            json_type_name(schema)
        ),
    };

    // Check required keys
    if let Some(required) = schema_obj.get("required").and_then(|v| v.as_array()) {
        for req in required {
            if let Some(key) = req.as_str() {
                if !inputs.contains_key(key) {
                    bail!(
                        "error.missing_required_input: required input '{}' is not present \
                         in the invocation inputs",
                        key
                    );
                }
            }
        }
    }

    // Check type constraints for present keys
    if let Some(props) = schema_obj.get("properties").and_then(|v| v.as_object()) {
        for (key, val) in inputs {
            if let Some(prop_schema) = props.get(key) {
                if let Some(expected_type) = prop_schema.get("type").and_then(|v| v.as_str()) {
                    if !json_type_matches(val, expected_type) {
                        bail!(
                            "error.input_type_mismatch: input '{}' expected type '{}' \
                             but got {}",
                            key,
                            expected_type,
                            json_type_name(val)
                        );
                    }
                }
            }
        }
    }

    Ok(())
}

/// Recursively walk a `serde_json::Value` and call [`expr::interpolate`] on
/// every string leaf.
///
/// Used by the Emit/Rewrite planner arms to expand `${path}` placeholders in
/// `backend_op` values before decoding them to typed op structs. Non-string
/// leaves (numbers, booleans, null, arrays, objects) pass through unchanged.
///
/// Returns `Err` with the [`expr::InterpolateError`] message on the first
/// leaf that fails to interpolate.
fn interpolate_value_strings(v: &Value, ctx: &expr::Context) -> Result<Value> {
    match v {
        Value::String(s) => {
            // Whole-placeholder typed substitution: when the entire string is a
            // sole `${path}` that resolves in the context, return the resolved
            // JSON value VERBATIM, preserving its type (bool/number/array/object/
            // null) rather than stringifying it. This lets a macro forward a
            // typed input (e.g. a bool `deep_analysis` or an array `item_names`)
            // through a DelegateRefactor param. Non-whole or unresolved
            // placeholders fall through to scalar string interpolation (which
            // still errors on a genuinely missing required path).
            if let Some(resolved) = resolve_whole_placeholder_any(s, ctx) {
                return Ok(resolved);
            }
            let expanded = expr::interpolate(s, ctx)
                .map_err(|e| anyhow!("interpolation error in backend_op string leaf: {e}"))?;
            Ok(Value::String(expanded))
        }
        Value::Array(arr) => {
            // Splice-interpolation: if an array element is a whole-placeholder
            // string like "${inputs.my_array}" that resolves to a JSON array in
            // the context, flatten its elements into the parent array.  This
            // allows macro backend_op to express `"parameter_types":
            // ["${inputs.caller_method_parameter_types}"]` and have the runtime
            // splice in e.g. `["Order", "int"]`.  Any other string element
            // undergoes normal scalar interpolation; non-string elements pass
            // through recursively.
            let mut out = Vec::new();
            for item in arr {
                match item {
                    Value::String(s) => {
                        if let Some(spliced) = splice_interpolate(s, ctx) {
                            match spliced {
                                Value::Array(inner) => out.extend(inner),
                                other => out.push(other),
                            }
                        } else {
                            let expanded = expr::interpolate(s, ctx).map_err(|e| {
                                anyhow!("interpolation error in backend_op array element: {e}")
                            })?;
                            out.push(Value::String(expanded));
                        }
                    }
                    _ => out.push(interpolate_value_strings(item, ctx)?),
                }
            }
            Ok(Value::Array(out))
        }
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                out.insert(k.clone(), interpolate_value_strings(v, ctx)?);
            }
            Ok(Value::Object(out))
        }
        // Numbers, booleans, and null pass through unchanged.
        other => Ok(other.clone()),
    }
}

/// Attempt splice-interpolation for a string element that is a *pure*
/// `${path}` placeholder (no surrounding text).
///
/// If `s` is exactly `"${path}"` and `path` resolves to a non-scalar JSON
/// value (array or object) in `ctx`, returns `Some(resolved_value)`.
/// Returns `None` for partial-template strings or when the resolved value is
/// a scalar (scalars go through normal [`expr::interpolate`] path).
/// If `s` is a sole `${path}` placeholder (ignoring surrounding whitespace; no
/// other literal text, no nesting) that resolves in `ctx`, return the resolved
/// JSON value VERBATIM, preserving
/// its type. Returns `None` when `s` is not a whole placeholder or the path
/// does not resolve, so the caller falls back to scalar string interpolation
/// (which preserves the genuine-missing-path error for required inputs).
fn resolve_whole_placeholder_any(s: &str, ctx: &expr::Context) -> Option<Value> {
    let t = s.trim();
    if t.starts_with("${") && t.ends_with('}') && t.len() > 3 {
        let path = &t[2..t.len() - 1];
        if !path.contains("${") && !path.is_empty() {
            return ctx.resolve(path).cloned();
        }
    }
    None
}

fn splice_interpolate(s: &str, ctx: &expr::Context) -> Option<Value> {
    let s = s.trim();
    if s.starts_with("${") && s.ends_with('}') && s.len() > 3 {
        let path = &s[2..s.len() - 1];
        // Only act on pure paths (no nested `${` inside the placeholder).
        if !path.contains("${") && !path.is_empty() {
            if let Some(v) = ctx.resolve(path) {
                if v.is_array() || v.is_object() {
                    return Some(v.clone());
                }
            }
        }
    }
    None
}

fn json_type_matches(val: &Value, expected: &str) -> bool {
    match expected {
        "string" => val.is_string(),
        "number" => val.is_number(),
        // JSON Schema distinguishes "integer" from "number": reject fractional values.
        "integer" => val.is_i64() || val.is_u64(),
        "boolean" => val.is_boolean(),
        "null" => val.is_null(),
        "array" => val.is_array(),
        "object" => val.is_object(),
        _ => true, // unknown type specifier → pass-through
    }
}

fn json_type_name(val: &Value) -> &'static str {
    match val {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Plan a `DelegateRefactor` operation, enforcing the RX-V1 authority boundary.
///
/// # RX-V1 enforcement
///
/// 1. Strips any `acknowledge_*` keys from the def's delegate `params` and from
///    any nested `toml_entries` inside it (def-origin authority is not permitted).
/// 2. Injects each flag from `operator_opt_outs` as `toml_entries[flag] = true`
///    so the delegate plan kind can find it via its standard lookup pattern.
/// 3. Forces `output_path = None` on the params.
/// 4. Returns the set of flags ACTUALLY CONSUMED by the delegated plan (from
///    `RefactorPlan.operator_opt_outs_used`) for the caller to union into the
///    macro-level `operator_opt_outs_used`.
///
/// # Returns `(RefactorPlan, consumed_flags)`
fn plan_delegate(
    refactor_kind: &str,
    raw_params: &Value,
    operator_opt_outs: &[String],
    project_dir: &str,
    ctx: &MacroPlannerContext,
) -> Result<(RefactorPlan, HashSet<String>)> {
    // ── RX-V1: parse params object + strip illicit authority keys ────────────
    let mut params_obj = match raw_params.as_object() {
        Some(o) => o.clone(),
        None => {
            bail!(
                "error.bad_input: DelegateRefactor params for kind '{}' must be a JSON object, \
                 got: {}",
                refactor_kind,
                json_type_name(raw_params)
            );
        }
    };

    // Strip any acknowledge_* keys at the top level of the params object
    let illicit_top: Vec<String> = params_obj
        .keys()
        .filter(|k| k.starts_with(AUTHORITY_PREFIX))
        .cloned()
        .collect();
    if !illicit_top.is_empty() {
        for k in &illicit_top {
            params_obj.remove(k);
        }
        tracing::warn!(
            kind = refactor_kind,
            ?illicit_top,
            "RX-V1: stripped illicit authority key(s) from DelegateRefactor top-level params"
        );
    }

    // Strip any acknowledge_* keys from nested toml_entries
    if let Some(Value::Object(entries)) = params_obj.get_mut("toml_entries") {
        let illicit_entries: Vec<String> = entries
            .keys()
            .filter(|k| k.starts_with(AUTHORITY_PREFIX))
            .cloned()
            .collect();
        if !illicit_entries.is_empty() {
            for k in &illicit_entries {
                entries.remove(k);
            }
            tracing::warn!(
                kind = refactor_kind,
                ?illicit_entries,
                "RX-V1: stripped illicit authority key(s) from DelegateRefactor toml_entries"
            );
        }
    }

    // ── RX-V1: inject ONLY from invocation.operator_opt_outs ─────────────────
    // Standard injection point: toml_entries["acknowledge_<flag>"] = true
    // (mirrors how rust_move_fields.rs consumes acknowledge_repr).
    let has_opt_outs = operator_opt_outs
        .iter()
        .any(|f| f.starts_with(AUTHORITY_PREFIX));
    if has_opt_outs {
        let entries = params_obj
            .entry("toml_entries")
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        if let Some(map) = entries.as_object_mut() {
            for flag in operator_opt_outs {
                if flag.starts_with(AUTHORITY_PREFIX) {
                    map.insert(flag.clone(), Value::Bool(true));
                }
            }
        }
    }

    // ── Force output_path = None (constraint 6a) ─────────────────────────────
    params_obj.insert("output_path".to_string(), Value::Null);

    // ── Propagate project_dir if not already set ──────────────────────────────
    if !params_obj.contains_key("project_dir")
        || params_obj
            .get("project_dir")
            .map(|v| v.is_null())
            .unwrap_or(true)
    {
        params_obj.insert(
            "project_dir".to_string(),
            Value::String(project_dir.to_string()),
        );
    }

    // ── Inject the plan kind from the operation (the DelegateRefactor params
    // object carries the kind separately in `refactor_kind`, not inline) ──────
    // RefactorPlanParams.kind is a required field, so this must happen BEFORE
    // deserialization, not after.
    params_obj.insert("kind".to_string(), Value::String(refactor_kind.to_string()));

    // ── Parse into RefactorPlanParams ─────────────────────────────────────────
    let mut rpp: RefactorPlanParams = serde_json::from_value(Value::Object(params_obj)).context(
        "error.bad_input: failed to parse DelegateRefactor params as RefactorPlanParams",
    )?;

    // Set the kind from the operation (belt-and-suspenders; also ensures deserialized default is overridden)
    rpp.kind = refactor_kind.to_string();
    // Belt-and-suspenders: ensure output_path is None even if deserialization set something
    rpp.output_path = None;

    // ── Build PlanContext from MacroPlannerContext ────────────────────────────
    let plan_ctx = PlanContext {
        lsp: ctx.lsp.clone(),
    };

    // ── Call plan_with_ctx ────────────────────────────────────────────────────
    let plan_json_str =
        bbox_refactor::plan_with_ctx(&rpp, &plan_ctx).context("delegate_refactor plan failed")?;

    // ── Deserialize as RefactorPlan (constraint 6b: reject analysis-only kinds) ──
    let rp: RefactorPlan = serde_json::from_str(&plan_json_str).map_err(|e| {
        anyhow!(
            "error.analysis_only_kind: DelegateRefactor kind '{}' did not return a \
             RefactorPlan JSON object (analysis-only or summary-returning kinds cannot be \
             delegated to macros). Deserialize error: {}",
            refactor_kind,
            e
        )
    })?;

    // ── Reject Blocked / Errored plans (constraint 6c) ───────────────────────
    match rp.plan_status {
        bbox_refactor::PlanStatus::Blocked => {
            bail!(
                "error.delegate_plan_blocked: DelegateRefactor kind '{}' returned a Blocked \
                 plan (deep-analysis findings must be resolved before the macro can proceed). \
                 Title: {}",
                refactor_kind,
                rp.title
            );
        }
        bbox_refactor::PlanStatus::Errored => {
            bail!(
                "error.delegate_plan_errored: DelegateRefactor kind '{}' returned an \
                 Errored plan. Title: {}",
                refactor_kind,
                rp.title
            );
        }
        _ => {}
    }

    // ── Constraint 7: collect ONLY consumed authority flags ───────────────────
    let consumed: HashSet<String> = rp.operator_opt_outs_used.iter().cloned().collect();

    Ok((rp, consumed))
}

/// Surface non-empty residue from a delegated [`RefactorPlan`] as review questions.
///
/// [`MacroPlanner::lower`] resets all advisory residue fields to empty when
/// constructing the lowered `RefactorPlan` (they are not merged into the macro's
/// `EditSet`). This function ensures that residue surviving the delegate plan is
/// NOT silently dropped — it is carried forward in `MacroPlan.questions` for
/// operator review before the plan is applied.
///
/// Called from the [`MacroOperation::DelegateRefactor`] arm of [`MacroPlanner::plan`].
fn surface_delegate_residue(kind: &str, rp: &RefactorPlan, questions: &mut Vec<String>) {
    // leftovers: Vec<String> — each entry is a distinct advisory message.
    for leftover in &rp.leftovers {
        questions.push(format!(
            "[delegate {} residue: leftover] {}",
            kind, leftover
        ));
    }
    // Count-based residue fields: emit one question per non-empty field.
    if !rp.items.is_empty() {
        questions.push(format!(
            "[delegate {} residue: {} item(s) not merged into macro edit set]",
            kind,
            rp.items.len()
        ));
    }
    if !rp.external_calls.is_empty() {
        questions.push(format!(
            "[delegate {} residue: {} external_call(s) require operator review]",
            kind,
            rp.external_calls.len()
        ));
    }
    if !rp.inherited_dependencies.is_empty() {
        questions.push(format!(
            "[delegate {} residue: {} inherited_dependenc(ies) require operator review]",
            kind,
            rp.inherited_dependencies.len()
        ));
    }
    if !rp.remaining_source_accessors.is_empty() {
        questions.push(format!(
            "[delegate {} residue: {} remaining_source_accessor(s) require operator review]",
            kind,
            rp.remaining_source_accessors.len()
        ));
    }
    if !rp.remaining_source_constant_refs.is_empty() {
        questions.push(format!(
            "[delegate {} residue: {} remaining_source_constant_ref(s) require operator review]",
            kind,
            rp.remaining_source_constant_refs.len()
        ));
    }
    if !rp.captured_variables.is_empty() {
        questions.push(format!(
            "[delegate {} residue: {} captured_variable(s) require operator review]",
            kind,
            rp.captured_variables.len()
        ));
    }
}

/// Map a [`SemanticStatus`] to the equivalent [`MacroSemanticStatus`].
fn map_refactor_status(s: &SemanticStatus) -> MacroSemanticStatus {
    match s {
        SemanticStatus::SyntaxOnly => MacroSemanticStatus::SyntaxOnly,
        SemanticStatus::IndexedHints => MacroSemanticStatus::IndexedHints,
        SemanticStatus::LspVerified => MacroSemanticStatus::LspVerified,
        SemanticStatus::LspVerifiedPartial => MacroSemanticStatus::LspVerifiedPartial,
    }
}

/// Aggregate per-operation statuses into a single [`MacroSemanticStatus`].
///
/// Ordering (worst → best): `template_only < syntax_only < indexed_hints <
/// lsp_verified_partial < lsp_verified`. If all ops share the same status →
/// that status. If any two ops differ → `Mixed`.
fn aggregate_status(statuses: &[MacroSemanticStatus]) -> MacroSemanticStatus {
    if statuses.is_empty() {
        // Vacuous (no ops): TemplateOnly signals "nothing verified"
        return MacroSemanticStatus::TemplateOnly;
    }
    let first = &statuses[0];
    if statuses.iter().all(|s| s == first) {
        first.clone()
    } else {
        MacroSemanticStatus::Mixed
    }
}

/// Pick the worst concrete refactor-tier status for lowering.
///
/// Record ops (non-mutating, TemplateOnly) are excluded. `Mixed` and `TemplateOnly`
/// on mutating ops cause lowering to be refused (called before this point).
fn worst_refactor_tier(ops: &[MacroPlanOperation]) -> Result<SemanticStatus> {
    // Exclude non-mutating kinds: "record" (metadata) and "probe" (read-only).
    // Probe semantic status must NOT leak into the lowered RefactorPlan tier.
    let mutating: Vec<&MacroPlanOperation> = ops
        .iter()
        .filter(|op| op.kind != "record" && op.kind != "probe")
        .collect();

    if mutating.is_empty() {
        // Only record ops or empty plan; use SyntaxOnly as a conservative default.
        return Ok(SemanticStatus::SyntaxOnly);
    }

    // Start at the best possible tier and walk down to the worst observed
    let mut worst_rank: u8 = status_rank(&MacroSemanticStatus::LspVerified);
    let mut worst_status = MacroSemanticStatus::LspVerified;

    for op in &mutating {
        let r = status_rank(&op.semantic_status);
        if r < worst_rank {
            worst_rank = r;
            worst_status = op.semantic_status.clone();
        }
    }

    match worst_status {
        MacroSemanticStatus::SyntaxOnly => Ok(SemanticStatus::SyntaxOnly),
        MacroSemanticStatus::IndexedHints => Ok(SemanticStatus::IndexedHints),
        MacroSemanticStatus::LspVerified => Ok(SemanticStatus::LspVerified),
        MacroSemanticStatus::LspVerifiedPartial => Ok(SemanticStatus::LspVerifiedPartial),
        MacroSemanticStatus::TemplateOnly => {
            bail!(
                "error.template_only_lowering_refused: a mutating operation has \
                 template_only status; cannot lower to RefactorPlan"
            );
        }
        MacroSemanticStatus::Mixed => {
            bail!(
                "error.mixed_status_lowering: aggregate semantic status is Mixed; \
                 lowering requires a single concrete tier. Resolve per-op statuses."
            );
        }
    }
}

/// Numeric rank for [`MacroSemanticStatus`] (higher = better / more verified).
fn status_rank(s: &MacroSemanticStatus) -> u8 {
    match s {
        MacroSemanticStatus::TemplateOnly => 0,
        MacroSemanticStatus::Mixed => 0, // treat as worst for ordering
        MacroSemanticStatus::SyntaxOnly => 1,
        MacroSemanticStatus::IndexedHints => 2,
        MacroSemanticStatus::LspVerifiedPartial => 3,
        MacroSemanticStatus::LspVerified => 4,
    }
}

/// Return the `"kind"` discriminant string for a [`ProbeSpec`].
fn probe_spec_kind_str(spec: &ProbeSpec) -> &'static str {
    match spec {
        ProbeSpec::CodeQuery { .. } => "code_query",
        ProbeSpec::CodeSymbols { .. } => "code_symbols",
        ProbeSpec::ProjectText { .. } => "project_text",
        ProbeSpec::WorkspaceSymbol { .. } => "workspace_symbol",
        ProbeSpec::JavaClassAnalysis { .. } => "java_class_analysis",
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Unknown-root validation
// ─────────────────────────────────────────────────────────────────────────────

/// Extract the root (first dotted segment) of a context path.
fn path_root(path: &str) -> &str {
    path.split('.').next().unwrap_or(path)
}

/// Recursively collect the root segments referenced by a [`Predicate`].
fn collect_predicate_roots(pred: &crate::expr::Predicate, out: &mut Vec<String>) {
    use crate::expr::Predicate;
    match pred {
        Predicate::Exists { path } | Predicate::Eq { path, .. } | Predicate::In { path, .. } => {
            out.push(path_root(path).to_string());
        }
        Predicate::All { predicates } | Predicate::Any { predicates } => {
            for p in predicates {
                collect_predicate_roots(p, out);
            }
        }
        Predicate::Not { predicate } => collect_predicate_roots(predicate, out),
    }
}

/// Scan a template string for `${path}` placeholders and collect root segments.
fn collect_interpolation_roots(template: &str, out: &mut Vec<String>) {
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' && chars.peek() == Some(&'{') {
            chars.next(); // consume '{'
            let mut path = String::new();
            for ch in chars.by_ref() {
                if ch == '}' {
                    break;
                }
                path.push(ch);
            }
            if !path.is_empty() {
                out.push(path_root(&path).to_string());
            }
        }
    }
}

/// Validate that all context-path roots in refusals, validation guards, and
/// Record operation bodies reference known probe names.
///
/// # Phase-aware validation
///
/// `refusal_allowed_roots` = `{"inputs"}` ∪ top-level `def.probes` names.
/// These are the only roots that exist when refusal predicates are evaluated
/// (before any inline-operation probe has run).
///
/// `op_allowed_roots` = `refusal_allowed_roots` ∪ inline `MacroOperation::Probe`
/// names. Validation guards and Record op bodies may reference inline probe
/// names because they execute (or are checked) after inline probes run.
///
/// Any root outside the applicable set is a hard planning error
/// (`error.unknown_context_root`). Missing *nested* fields under a valid root
/// remain soft-false (existing [`expr::eval`] behaviour).
fn validate_context_roots(
    def: &MacroDefinition,
    refusal_allowed_roots: &HashSet<String>,
    op_allowed_roots: &HashSet<String>,
) -> Result<()> {
    let mut bad: Vec<String> = vec![];

    // Refusal predicates and messages: refusal-phase roots only.
    // Inline-operation probe names are excluded — they don't exist yet when
    // refusals evaluate, so a reference to one is always a typo or logic error.
    for refusal in &def.refusals {
        let mut roots = vec![];
        collect_predicate_roots(&refusal.when, &mut roots);
        for r in roots {
            if !refusal_allowed_roots.contains(&r) {
                bad.push(format!(
                    "refusal '{}' predicate references unknown root '{}' \
                     (not 'inputs' or a top-level probe name; \
                     inline-operation probe names are not in scope at refusal time)",
                    refusal.code, r
                ));
            }
        }
        let mut msg_roots = vec![];
        collect_interpolation_roots(&refusal.message, &mut msg_roots);
        for r in msg_roots {
            if !refusal_allowed_roots.contains(&r) {
                bad.push(format!(
                    "refusal '{}' message interpolation references unknown root '{}' \
                     (not 'inputs' or a top-level probe name; \
                     inline-operation probe names are not in scope at refusal time)",
                    refusal.code, r
                ));
            }
        }
    }

    // Validation guards: operation-phase roots (includes inline probe names).
    for val in &def.validations {
        if let Some(guard) = &val.when {
            let mut roots = vec![];
            collect_predicate_roots(guard, &mut roots);
            for r in roots {
                if !op_allowed_roots.contains(&r) {
                    bad.push(format!(
                        "validation guard predicate references unknown root '{}' \
                         (not 'inputs' or a declared probe name)",
                        r
                    ));
                }
            }
        }
    }

    // Emit/Rewrite guard predicates: operation-phase roots.
    for (i, op) in def.operations.iter().enumerate() {
        let guard = match op {
            MacroOperation::Emit { when, .. } => when.as_ref(),
            MacroOperation::Rewrite { when, .. } => when.as_ref(),
            _ => None,
        };
        if let Some(guard_pred) = guard {
            let mut roots = vec![];
            collect_predicate_roots(guard_pred, &mut roots);
            for r in roots {
                if !op_allowed_roots.contains(&r) {
                    bad.push(format!(
                        "operations[{i}] 'when' guard predicate references unknown root '{}' \
                         (not 'inputs' or a declared probe name)",
                        r
                    ));
                }
            }
        }
    }

    // Record operation bodies: operation-phase roots (includes inline probe names).
    for op in &def.operations {
        if let MacroOperation::Record { label, body } = op {
            let mut roots = vec![];
            collect_interpolation_roots(body, &mut roots);
            for r in roots {
                if !op_allowed_roots.contains(&r) {
                    bad.push(format!(
                        "Record operation '{}' body interpolation references unknown root '{}' \
                         (not 'inputs' or a declared probe name)",
                        label, r
                    ));
                }
            }
        }
    }

    // ForEach: structural validation. The body must be an edit-producing op
    // (emit | rewrite) in v1; a probe/delegate_refactor/validate/record/nested
    // for_each body is rejected. The `over` path must be non-empty.
    for (i, op) in def.operations.iter().enumerate() {
        if let MacroOperation::ForEach { over, body, .. } = op {
            match body.as_ref() {
                MacroOperation::Emit { .. } | MacroOperation::Rewrite { .. } => {}
                _ => bad.push(format!(
                    "operations[{i}] ForEach body must be an emit or rewrite operation \
                     (probe, delegate_refactor, validate, record, and nested for_each \
                     bodies are not supported in v1)"
                )),
            }
            if over.trim().is_empty() {
                bad.push(format!(
                    "operations[{i}] ForEach 'over' path must be a non-empty context path"
                ));
            }
        }
    }

    if bad.is_empty() {
        Ok(())
    } else {
        bail!(
            "error.unknown_context_root: {} unknown root segment(s) in macro '{}' context paths:\n{}",
            bad.len(),
            def.id,
            bad.join("\n")
        )
    }
}

// ── Helpers exposed for tool-layer tests ────────────────────────────────────

/// Build a `RefactorApplyParams` for `macro_apply`.
///
/// Critical: `allow_dirty_worktree`, `allow_unregistered_paths`, and `force_path`
/// are all `None` (= false in `refactor::apply`). `macro_apply` is an envelope
/// over `refactor::apply` — it MUST NOT set bypass flags by default.
pub fn build_macro_apply_params(
    plan_value: Value,
    confirm: Option<bool>,
    cwd: Option<String>,
) -> RefactorApplyParams {
    RefactorApplyParams {
        plan: plan_value,
        plan_path: None,
        confirm,
        // NO bypass flags — all must remain None (= false in refactor::apply).
        // The caller (operator) can re-run via bbox_refactor_apply if they need
        // bypass flags; macro_apply does not expose them.
        allow_dirty_worktree: None,
        allow_unregistered_paths: None,
        cwd,
        force_path: None,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::model::{MacroProbe, MacroRefusal, MacroScope};
    use crate::planner_ctx::MacroPlannerContext;
    use bbox_refactor;

    // ── Test fixture helpers ─────────────────────────────────────────────────

    fn minimal_def() -> MacroDefinition {
        MacroDefinition {
            id: "test.macro".into(),
            version: "1.0.0".into(),
            language: "any".into(),
            scope: MacroScope::Builtin,
            title: "Test Macro".into(),
            inputs_schema: json!({"type": "object"}),
            effects: vec![],
            authority_gates: vec![],
            probes: vec![],
            operations: vec![],
            validations: vec![],
            refusals: vec![],
        }
    }

    fn minimal_invocation(def: &MacroDefinition, project_dir: &str) -> MacroInvocation {
        MacroInvocation {
            macro_id: def.id.clone(),
            version: None,
            project_dir: project_dir.to_string(),
            inputs: serde_json::Map::new(),
            anchors: None,
            operator_opt_outs: vec![],
        }
    }

    // ── Constraint 1: version mismatch ───────────────────────────────────────

    #[test]
    fn version_mismatch_is_rejected() {
        let def = minimal_def();
        let mut inv = minimal_invocation(&def, "/tmp");
        inv.version = Some("99.0.0".to_string()); // wrong version
        let ctx = MacroPlannerContext::default();
        let err = MacroPlanner::plan(&inv, &def, &ctx).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("error.version_mismatch"),
            "expected version_mismatch error, got: {msg}"
        );
        assert!(
            msg.contains("99.0.0"),
            "error should mention requested version: {msg}"
        );
        assert!(
            msg.contains("1.0.0"),
            "error should mention resolved version: {msg}"
        );
    }

    #[test]
    fn version_match_proceeds() {
        let def = minimal_def();
        let mut inv = minimal_invocation(&def, "/tmp");
        inv.version = Some("1.0.0".to_string()); // correct version
        let ctx = MacroPlannerContext::default();
        // A macro with no ops and no probes should plan successfully (empty edit set)
        let result = MacroPlanner::plan(&inv, &def, &ctx);
        assert!(
            result.is_ok(),
            "exact version match should proceed: {:?}",
            result.err()
        );
    }

    #[test]
    fn version_none_proceeds_with_any_registered_version() {
        let def = minimal_def();
        let inv = minimal_invocation(&def, "/tmp");
        let ctx = MacroPlannerContext::default();
        // No version pin → always proceeds
        let result = MacroPlanner::plan(&inv, &def, &ctx);
        assert!(
            result.is_ok(),
            "no version pin should always proceed: {:?}",
            result.err()
        );
    }

    // ── Constraint 4: probe fail-closed via UnavailableProbeRunner ───────────
    //
    // With the P4b implementation, "fail closed" means the runner returns an
    // error, not a static guard. A valid ProbeSpec + UnavailableProbeRunner
    // must still propagate error.probe_backend_unavailable.

    #[test]
    fn probe_backend_unavailable_when_probes_declared() {
        let mut def = minimal_def();
        // Use a valid ProbeSpec kind so deserialization succeeds and the runner is reached.
        def.probes = vec![MacroProbe {
            name: "caller_type".into(),
            description: "Finds the caller type".into(),
            spec: json!({"kind": "code_symbols"}), // valid ProbeSpec::CodeSymbols
        }];
        let inv = minimal_invocation(&def, "/tmp");
        // MacroPlannerContext::default() uses UnavailableProbeRunner.
        let ctx = MacroPlannerContext::default();
        let err = MacroPlanner::plan(&inv, &def, &ctx).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("error.probe_backend_unavailable"),
            "expected probe_backend_unavailable from UnavailableProbeRunner, got: {msg}"
        );
    }

    // ── Constraint 5: refusal short-circuits ─────────────────────────────────

    #[test]
    fn refusal_fires_and_short_circuits() {
        let mut def = minimal_def();
        def.inputs_schema = json!({
            "type": "object",
            "properties": {
                "service_name": {"type": "string"}
            }
        });
        def.refusals = vec![MacroRefusal {
            when: crate::expr::Predicate::Exists {
                path: "inputs.service_name".into(),
            },
            code: "error.type_already_exists".into(),
            message: "Type ${inputs.service_name} already exists".into(),
        }];
        let mut inv = minimal_invocation(&def, "/tmp");
        inv.inputs
            .insert("service_name".into(), json!("PaymentService"));
        let ctx = MacroPlannerContext::default();

        let plan = MacroPlanner::plan(&inv, &def, &ctx)
            .expect("refusal should return Ok(MacroPlan), not Err");

        assert!(!plan.refusals.is_empty(), "refusal should be in the plan");
        assert_eq!(plan.refusals[0].code, "error.type_already_exists");
        assert!(
            plan.refusals[0].message.contains("PaymentService"),
            "message interpolation should work: {}",
            plan.refusals[0].message
        );
        assert!(
            plan.edits.file_edits.is_empty() && plan.edits.file_creates.is_empty(),
            "refusal plan must have empty EditSet"
        );
        // No ops executed
        assert!(
            plan.operations.is_empty(),
            "refusal plan must have no operations"
        );
    }

    #[test]
    fn refusal_not_fires_when_predicate_is_false() {
        let mut def = minimal_def();
        def.refusals = vec![MacroRefusal {
            when: crate::expr::Predicate::Exists {
                path: "inputs.nonexistent_key".into(),
            },
            code: "error.should_not_fire".into(),
            message: "This should not appear".into(),
        }];
        let inv = minimal_invocation(&def, "/tmp");
        let ctx = MacroPlannerContext::default();
        let plan = MacroPlanner::plan(&inv, &def, &ctx).unwrap();
        assert!(plan.refusals.is_empty(), "refusal should not fire");
    }

    // ── Constraint 6: Emit/Rewrite → backend_unavailable ────────────────────

    #[test]
    fn emit_op_returns_backend_unavailable_error() {
        let mut def = minimal_def();
        def.operations = vec![MacroOperation::Emit {
            name: "interface_file".into(),
            // Valid typed op so decode succeeds and the call reaches UnavailableBackend.
            backend_op: json!({
                "op": "emit_type",
                "source_root": "/tmp/src",
                "package": "com.example",
                "name": "PaymentService",
                "kind": "interface",
                "source_text": "package com.example;\npublic interface PaymentService {}"
            }),
            when: None,
        }];
        let inv = minimal_invocation(&def, "/tmp");
        let ctx = MacroPlannerContext::default(); // UnavailableBackend

        let err = MacroPlanner::plan(&inv, &def, &ctx).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("error.backend_unavailable"),
            "emit should propagate backend_unavailable, got: {msg}"
        );
    }

    #[test]
    fn rewrite_op_returns_backend_unavailable_error() {
        let mut def = minimal_def();
        def.operations = vec![MacroOperation::Rewrite {
            targets: vec!["src/Foo.java".into()],
            // Valid typed op so decode succeeds and the call reaches UnavailableBackend.
            backend_op: json!({
                "op": "insert_member",
                "target_file": "src/Foo.java",
                "target_type": "Foo",
                "member_text": "void x() {}",
                "imports": []
            }),
            when: None,
        }];
        let inv = minimal_invocation(&def, "/tmp");
        let ctx = MacroPlannerContext::default(); // UnavailableBackend

        let err = MacroPlanner::plan(&inv, &def, &ctx).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("error.backend_unavailable"),
            "rewrite should propagate backend_unavailable, got: {msg}"
        );
    }

    // ── Constraint 6: DelegateRefactor e2e (create_file on tempdir) ──────────

    #[test]
    fn delegate_refactor_create_file_e2e() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let project_dir = tmp.path().to_string_lossy().to_string();
        let target_path = tmp.path().join("Generated.java");
        let target_path_str = target_path.to_string_lossy().to_string();

        let mut def = minimal_def();
        def.operations = vec![MacroOperation::DelegateRefactor {
            refactor_kind: "create_file".into(),
            params: json!({
                "source": target_path_str,
                "new_text": "public class Generated {}"
            }),
        }];

        let inv = minimal_invocation(&def, &project_dir);
        let ctx = MacroPlannerContext::default();

        // Plan
        let plan = MacroPlanner::plan(&inv, &def, &ctx)
            .expect("delegate create_file should plan successfully");

        assert!(plan.refusals.is_empty(), "no refusals expected");
        assert_eq!(plan.operations.len(), 1);
        assert_eq!(plan.operations[0].kind, "delegate_refactor");
        assert_eq!(plan.edits.file_creates.len(), 1);
        assert_eq!(plan.edits.file_creates[0].path, target_path_str);
        assert_eq!(
            plan.edits.file_creates[0].content,
            "public class Generated {}"
        );

        // Lower
        let refactor_plan =
            MacroPlanner::lower(&plan).expect("lowering should succeed for create_file");

        assert_eq!(refactor_plan.file_creates.len(), 1);
        assert!(refactor_plan.edits.is_empty(), "no file edits expected");
        assert!(
            refactor_plan.file_moves.is_empty(),
            "no file moves expected"
        );

        // Apply via refactor::apply (use allow_unregistered_paths for the test fixture)
        let apply_params = RefactorApplyParams {
            plan: serde_json::to_value(&refactor_plan).expect("serialize plan"),
            plan_path: None,
            confirm: Some(true),
            allow_dirty_worktree: None,
            allow_unregistered_paths: Some(true), // test fixture not registered
            cwd: None,
            force_path: Some(true), // bypass worktree check for tempdir
        };
        let projects = vec![];
        let apply_result =
            bbox_refactor::apply(&apply_params, &projects).expect("apply should succeed");
        assert!(
            apply_result.contains("Generated.java") || apply_result.contains("files_written"),
            "apply result should mention the file: {apply_result}"
        );
        assert!(
            target_path.exists(),
            "created file should exist on disk after apply"
        );
    }

    // ── Constraint 6: DelegateRefactor duplicate path detection ──────────────

    #[test]
    fn duplicate_paths_across_delegates_rejected() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let target_path = tmp.path().join("Dup.java").to_string_lossy().to_string();

        let mut def = minimal_def();
        def.operations = vec![
            MacroOperation::DelegateRefactor {
                refactor_kind: "create_file".into(),
                params: json!({
                    "source": target_path,
                    "new_text": "class A {}"
                }),
            },
            MacroOperation::DelegateRefactor {
                refactor_kind: "create_file".into(),
                params: json!({
                    "source": target_path,
                    "new_text": "class B {}"
                }),
            },
        ];

        let project_dir = tmp.path().to_string_lossy().to_string();
        let inv = minimal_invocation(&def, &project_dir);
        let ctx = MacroPlannerContext::default();

        let err = MacroPlanner::plan(&inv, &def, &ctx).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("error.duplicate_touched_path") || msg.contains("duplicate"),
            "expected dup path error, got: {msg}"
        );
    }

    /// A `DelegateRefactor` op that creates a file must conflict with a later
    /// `Rewrite` op on the same path — the Rewrite must NOT silently drop the
    /// DelegateRefactor's edit.
    ///
    /// This is the BLOCKER-2 guard: `pending_rewrite_content` does not contain
    /// the path (only Rewrite-on-Rewrite chains register there), so the Rewrite
    /// arm falls through to `register_path`, which detects the duplicate.
    #[test]
    fn delegate_refactor_then_rewrite_same_path_is_duplicate() {
        use crate::backend::{BackendEditSet, JavaEmitOp, JavaMacroBackend, JavaRewriteOp};
        use bbox_refactor::{FileEdit, TextEdit};

        // A test-only backend that returns a canned FileEdit for any rewrite.
        struct CannedRewriteBackend {
            rewrite_path: String,
        }
        impl JavaMacroBackend for CannedRewriteBackend {
            fn emit(&self, _: &JavaEmitOp) -> anyhow::Result<BackendEditSet> {
                unimplemented!("emit not used in this test")
            }
            fn rewrite(&self, _: &JavaRewriteOp) -> anyhow::Result<BackendEditSet> {
                Ok(BackendEditSet {
                    file_edits: vec![FileEdit {
                        path: self.rewrite_path.clone(),
                        original_sha256: "deadbeef".into(),
                        edits: vec![TextEdit {
                            byte_start: 0,
                            byte_end: 12,
                            replacement: "class New {}".into(),
                        }],
                        new_text: Some("class New {}".into()),
                    }],
                    file_creates: vec![],
                })
            }
        }

        let tmp = tempfile::tempdir().expect("create tempdir");
        let target_path = tmp.path().join("Shared.java").to_string_lossy().to_string();
        let project_dir = tmp.path().to_string_lossy().to_string();

        let mut def = minimal_def();
        def.operations = vec![
            // Op 1: DelegateRefactor creates the file → registers it in touched_paths.
            MacroOperation::DelegateRefactor {
                refactor_kind: "create_file".into(),
                params: json!({
                    "source": target_path,
                    "new_text": "class Shared {}"
                }),
            },
            // Op 2: Rewrite touches the same path.
            // pending_rewrite_content does NOT contain the path (only Rewrite-on-Rewrite
            // chains go there), so the Rewrite arm calls register_path → dup error.
            MacroOperation::Rewrite {
                targets: vec![target_path.clone()],
                backend_op: json!({
                    "op": "insert_member",
                    "target_file": target_path,
                    "target_type": "Shared",
                    "member_text": "private int x;",
                    "imports": []
                }),
                when: None,
            },
        ];

        let inv = minimal_invocation(&def, &project_dir);
        let ctx = MacroPlannerContext::new(
            Box::new(CannedRewriteBackend {
                rewrite_path: target_path,
            }),
            None,
            Box::new(crate::probe::UnavailableProbeRunner),
        );

        let err = MacroPlanner::plan(&inv, &def, &ctx).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("error.duplicate_touched_path"),
            "DelegateRefactor → Rewrite same path must produce error.duplicate_touched_path; got: {msg}"
        );
    }

    // ── inputs_schema defaults ───────────────────────────────────────────────────

    #[test]
    fn apply_input_schema_defaults_fills_absent_only() {
        let schema = json!({
            "type": "object",
            "properties": {
                "strategy": {"type": "string", "default": "skip"},
                "name": {"type": "string"},
                "count": {"type": "integer", "default": 3}
            }
        });
        let mut inputs = serde_json::Map::new();
        inputs.insert("strategy".into(), json!("bridge")); // supplied — must win
        apply_input_schema_defaults(&mut inputs, &schema);
        assert_eq!(
            inputs.get("strategy"),
            Some(&json!("bridge")),
            "supplied value must not be overridden"
        );
        assert_eq!(
            inputs.get("count"),
            Some(&json!(3)),
            "absent default must be filled"
        );
        assert_eq!(
            inputs.get("name"),
            None,
            "property without a default stays absent"
        );
    }

    #[test]
    fn apply_input_schema_defaults_noop_without_properties() {
        let mut inputs = serde_json::Map::new();
        inputs.insert("x".into(), json!(1));
        apply_input_schema_defaults(&mut inputs, &json!({"type": "object"}));
        apply_input_schema_defaults(&mut inputs, &serde_json::Value::Null);
        assert_eq!(inputs.len(), 1, "no properties → no change");
    }

    // ── ForEach fan-out ────────────────────────────────────────────────────────

    /// ForEach expands one rewrite per probe-discovered item, interpolating
    /// `${item.*}` per element and honoring the body's per-item `when` guard.
    #[test]
    fn for_each_fans_out_rewrite_per_probe_item_with_per_item_guard() {
        use crate::backend::{BackendEditSet, JavaEmitOp, JavaMacroBackend, JavaRewriteOp};
        use crate::probe::{ProbeOutput, ProbeRunner, ProbeSpec};
        use bbox_refactor::{FileEdit, TextEdit};
        use std::sync::Mutex;

        // Records the target_type of every rewrite op it receives, echoing the
        // op's target_file into the resulting FileEdit path.
        struct RecordingBackend {
            seen: Mutex<Vec<String>>,
        }
        impl JavaMacroBackend for RecordingBackend {
            fn emit(&self, _: &JavaEmitOp) -> anyhow::Result<BackendEditSet> {
                unimplemented!("emit not used in this test")
            }
            fn rewrite(&self, op: &JavaRewriteOp) -> anyhow::Result<BackendEditSet> {
                let (path, ttype) = match op {
                    JavaRewriteOp::InsertMember {
                        target_file,
                        target_type,
                        ..
                    } => (target_file.clone(), target_type.clone()),
                    _ => unimplemented!("only insert_member used in this test"),
                };
                self.seen.lock().unwrap().push(ttype);
                Ok(BackendEditSet {
                    file_edits: vec![FileEdit {
                        path,
                        original_sha256: "deadbeef".into(),
                        edits: vec![TextEdit {
                            byte_start: 0,
                            byte_end: 1,
                            replacement: "x".into(),
                        }],
                        new_text: Some("x".into()),
                    }],
                    file_creates: vec![],
                })
            }
        }

        // Canned probe runner: returns a 3-item list, two of which are kept.
        struct CannedItemsRunner;
        impl ProbeRunner for CannedItemsRunner {
            fn run_probe(
                &self,
                _name: &str,
                _spec: &ProbeSpec,
                _ctx: &crate::expr::Context,
                _inv: &crate::model::MacroInvocation,
            ) -> anyhow::Result<ProbeOutput> {
                Ok(ProbeOutput {
                    value: json!({
                        "exists": true,
                        "count": 3,
                        "items": [
                            {"name": "Alpha", "keep": true},
                            {"name": "Beta",  "keep": false},
                            {"name": "Gamma", "keep": true}
                        ]
                    }),
                    semantic_status: MacroSemanticStatus::SyntaxOnly,
                    truncated: false,
                    diagnostics: vec![],
                })
            }
        }

        let mut def = minimal_def();
        def.language = "java".into();
        def.operations = vec![
            // Discover the collection.
            MacroOperation::Probe {
                name: "members".into(),
                spec: json!({
                    "kind": "code_symbols",
                    "query": "x",
                    "languages": ["java"],
                    "item_kinds": ["method_declaration"]
                }),
            },
            // Fan out a rewrite per item, skipping items whose `keep` is false.
            MacroOperation::ForEach {
                over: "members.items".into(),
                bind: "item".into(),
                body: Box::new(MacroOperation::Rewrite {
                    targets: vec!["${item.name}.java".into()],
                    backend_op: json!({
                        "op": "insert_member",
                        "target_file": "${item.name}.java",
                        "target_type": "${item.name}",
                        "member_text": "// generated",
                        "imports": []
                    }),
                    when: Some(crate::expr::Predicate::Eq {
                        path: "item.keep".into(),
                        value: json!(true),
                    }),
                }),
            },
        ];

        let backend = Box::new(RecordingBackend {
            seen: Mutex::new(vec![]),
        });
        // Keep a raw pointer-free handle: re-create assertion via plan output instead.
        let inv = minimal_invocation(&def, "/tmp");
        let ctx = MacroPlannerContext::new(backend, None, Box::new(CannedItemsRunner));

        let plan = MacroPlanner::plan(&inv, &def, &ctx).expect("plan should succeed");
        assert!(
            plan.refusals.is_empty(),
            "no refusals expected: {:?}",
            plan.refusals
        );

        // Two of three items kept → two distinct file edits (Beta skipped by guard).
        let mut paths: Vec<String> = plan
            .edits
            .file_edits
            .iter()
            .map(|e| e.path.clone())
            .collect();
        paths.sort();
        assert_eq!(
            paths,
            vec!["Alpha.java".to_string(), "Gamma.java".to_string()],
            "ForEach must fan out one edit per kept item with ${{item.*}} interpolated; \
             Beta must be skipped by its per-item guard"
        );

        // The for_each op summary records the expansion count (all 3 visited).
        let foreach_summary = plan
            .operations
            .iter()
            .find(|o| o.kind == "for_each")
            .expect("plan must contain a for_each operation summary");
        assert!(
            foreach_summary.summary.contains("expanded 3 item(s)"),
            "for_each summary should report 3 visited items; got: {}",
            foreach_summary.summary
        );
    }

    /// A ForEach whose `over` path resolves to a non-array value fails closed.
    #[test]
    fn for_each_over_non_array_fails_closed() {
        use crate::probe::{ProbeOutput, ProbeRunner, ProbeSpec};
        struct ScalarRunner;
        impl ProbeRunner for ScalarRunner {
            fn run_probe(
                &self,
                _name: &str,
                _spec: &ProbeSpec,
                _ctx: &crate::expr::Context,
                _inv: &crate::model::MacroInvocation,
            ) -> anyhow::Result<ProbeOutput> {
                Ok(ProbeOutput {
                    value: json!({"exists": true, "count": 1, "items": "not-an-array"}),
                    semantic_status: MacroSemanticStatus::SyntaxOnly,
                    truncated: false,
                    diagnostics: vec![],
                })
            }
        }

        let mut def = minimal_def();
        def.language = "java".into();
        def.operations = vec![
            MacroOperation::Probe {
                name: "members".into(),
                spec: json!({
                    "kind": "code_symbols",
                    "query": "x",
                    "languages": ["java"],
                    "item_kinds": ["method_declaration"]
                }),
            },
            MacroOperation::ForEach {
                over: "members.items".into(),
                bind: "item".into(),
                body: Box::new(MacroOperation::Rewrite {
                    targets: vec!["x".into()],
                    backend_op: json!({
                        "op": "insert_member",
                        "target_file": "x.java",
                        "target_type": "X",
                        "member_text": "// x",
                        "imports": []
                    }),
                    when: None,
                }),
            },
        ];

        let inv = minimal_invocation(&def, "/tmp");
        let ctx = MacroPlannerContext::new(
            Box::new(crate::backend::UnavailableBackend),
            None,
            Box::new(ScalarRunner),
        );
        let err = MacroPlanner::plan(&inv, &def, &ctx).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("must resolve to an array"),
            "ForEach over a non-array must fail closed; got: {msg}"
        );
    }

    /// Registry-style validation rejects a ForEach whose body is not emit/rewrite.
    #[test]
    fn for_each_non_edit_body_is_rejected() {
        let mut def = minimal_def();
        def.operations = vec![MacroOperation::ForEach {
            over: "members.items".into(),
            bind: "item".into(),
            body: Box::new(MacroOperation::Record {
                label: "x".into(),
                body: "y".into(),
            }),
        }];
        let inv = minimal_invocation(&def, "/tmp");
        let ctx = MacroPlannerContext::default();
        let err = MacroPlanner::plan(&inv, &def, &ctx).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("ForEach body must be an emit or rewrite"),
            "non-edit ForEach body must be rejected; got: {msg}"
        );
    }

    // ── Constraint 7: RX-V1 authority laundering ──────────────────────────────

    #[test]
    fn acknowledge_in_def_delegate_params_is_stripped() {
        // A def that tries to inject acknowledge_repr via its delegate params
        // must have it silently stripped (not passed to the plan kind).
        let tmp = tempfile::tempdir().expect("create tempdir");
        let target_path = tmp.path().join("Strip.java").to_string_lossy().to_string();
        let project_dir = tmp.path().to_string_lossy().to_string();

        let mut def = minimal_def();
        def.operations = vec![MacroOperation::DelegateRefactor {
            refactor_kind: "create_file".into(),
            // The def tries to inject authority — must be stripped
            params: json!({
                "source": target_path,
                "new_text": "class Strip {}",
                "acknowledge_repr": true,  // illicit: def must not carry authority
                "toml_entries": {
                    "acknowledge_public_api_change": true  // also illicit
                }
            }),
        }];

        let inv = minimal_invocation(&def, &project_dir);
        // Invocation does NOT supply operator_opt_outs
        let ctx = MacroPlannerContext::default();

        // Planning should succeed (create_file doesn't consume any authority flags),
        // and the stripped flags must NOT appear in operator_opt_outs_used.
        let plan = MacroPlanner::plan(&inv, &def, &ctx)
            .expect("plan should succeed after stripping illicit keys");

        assert!(
            plan.operator_opt_outs_used.is_empty(),
            "def-injected authority flags must not appear in operator_opt_outs_used: {:?}",
            plan.operator_opt_outs_used
        );
    }

    #[test]
    fn only_invocation_operator_opt_outs_supply_authority() {
        // Verify: flags from invocation.operator_opt_outs that are consumed
        // appear in operator_opt_outs_used; def-injected ones do not.
        // We use create_file which doesn't consume any authority, so the used
        // set is always empty — the key invariant is that def-laundering fails.
        let tmp = tempfile::tempdir().expect("create tempdir");
        let target_path = tmp.path().join("Auth.java").to_string_lossy().to_string();
        let project_dir = tmp.path().to_string_lossy().to_string();

        let mut def = minimal_def();
        def.operations = vec![MacroOperation::DelegateRefactor {
            refactor_kind: "create_file".into(),
            params: json!({
                "source": target_path,
                "new_text": "class Auth {}",
                "acknowledge_repr": true  // def-injected: must be stripped
            }),
        }];

        let mut inv = minimal_invocation(&def, &project_dir);
        // Operator supplies the flag legitimately
        inv.operator_opt_outs = vec!["acknowledge_repr".to_string()];

        let ctx = MacroPlannerContext::default();
        let plan = MacroPlanner::plan(&inv, &def, &ctx).expect("plan should succeed");

        // create_file doesn't consume acknowledge_repr, so used set is empty.
        // The key assertion: no flags from the def's launder attempt appear.
        assert!(
            plan.operator_opt_outs_used.is_empty(),
            "create_file doesn't consume authority flags; used set must be empty: {:?}",
            plan.operator_opt_outs_used
        );
    }

    // ── Authority gate enforcement ─────────────────────────────────────────────

    #[test]
    fn authority_gate_missing_from_opt_outs_produces_refusal() {
        let mut def = minimal_def();
        def.authority_gates = vec!["acknowledge_public_api_change".to_string()];
        // Invocation does NOT supply the gate.
        let inv = minimal_invocation(&def, "/tmp");
        let ctx = MacroPlannerContext::default();

        let plan = MacroPlanner::plan(&inv, &def, &ctx)
            .expect("authority gate refusal should return Ok(MacroPlan)");

        assert!(
            !plan.refusals.is_empty(),
            "missing authority gate should produce a refusal"
        );
        assert_eq!(
            plan.refusals[0].code, "error.authority_required",
            "refusal code must be error.authority_required"
        );
        assert!(
            plan.refusals[0]
                .message
                .contains("acknowledge_public_api_change"),
            "refusal message must name the missing gate: {}",
            plan.refusals[0].message
        );
        assert!(
            plan.edits.file_edits.is_empty() && plan.edits.file_creates.is_empty(),
            "authority-gate refusal must have empty EditSet"
        );
    }

    #[test]
    fn authority_gate_present_in_opt_outs_proceeds_and_appears_in_used() {
        let mut def = minimal_def();
        def.authority_gates = vec!["acknowledge_public_api_change".to_string()];
        // Operations list is empty → plan succeeds without touching the backend.
        let mut inv = minimal_invocation(&def, "/tmp");
        inv.operator_opt_outs = vec!["acknowledge_public_api_change".to_string()];
        let ctx = MacroPlannerContext::default();

        let plan = MacroPlanner::plan(&inv, &def, &ctx)
            .expect("supplied authority gate should allow planning to proceed");

        assert!(
            plan.refusals.is_empty(),
            "no refusals expected when gate is supplied"
        );
        assert!(
            plan.operator_opt_outs_used
                .contains(&"acknowledge_public_api_change".to_string()),
            "supplied gate must appear in operator_opt_outs_used: {:?}",
            plan.operator_opt_outs_used
        );
    }

    #[test]
    fn authority_gate_supplied_appears_in_opt_outs_used_even_on_other_gate_refusal() {
        // Two gates: gate A is supplied, gate B is missing.
        // Gate A must appear in opt_outs_used even though the plan is refused.
        let mut def = minimal_def();
        def.authority_gates = vec![
            "acknowledge_public_api_change".to_string(),
            "acknowledge_repr".to_string(),
        ];
        let mut inv = minimal_invocation(&def, "/tmp");
        // Supply only one of the two gates.
        inv.operator_opt_outs = vec!["acknowledge_public_api_change".to_string()];
        let ctx = MacroPlannerContext::default();

        let plan = MacroPlanner::plan(&inv, &def, &ctx)
            .expect("partial gate supply should return Ok(MacroPlan) with refusal");

        // One refusal for the missing gate.
        assert_eq!(plan.refusals.len(), 1, "exactly one gate is missing");
        assert_eq!(plan.refusals[0].code, "error.authority_required");
        assert!(
            plan.refusals[0].message.contains("acknowledge_repr"),
            "refusal must name the missing gate: {}",
            plan.refusals[0].message
        );

        // The supplied gate must appear in operator_opt_outs_used.
        assert!(
            plan.operator_opt_outs_used
                .contains(&"acknowledge_public_api_change".to_string()),
            "supplied gate must appear in operator_opt_outs_used even on refusal: {:?}",
            plan.operator_opt_outs_used
        );
    }

    #[test]
    fn no_authority_gates_declared_does_not_refuse() {
        // A macro with no authority_gates always proceeds regardless of operator_opt_outs.
        let def = minimal_def(); // authority_gates = vec![] by default
        let inv = minimal_invocation(&def, "/tmp");
        let ctx = MacroPlannerContext::default();

        let plan = MacroPlanner::plan(&inv, &def, &ctx)
            .expect("macro with no authority gates should plan without refusal");
        assert!(plan.refusals.is_empty(), "no refusals expected");
    }

    #[test]
    fn authority_gate_refusal_fires_before_regular_refusals() {
        // A macro with both an authority gate and a regular refusal.
        // When the gate is missing, we get a gate refusal (not the regular one).
        let mut def = minimal_def();
        def.authority_gates = vec!["acknowledge_repr".to_string()];
        def.refusals = vec![MacroRefusal {
            when: crate::expr::Predicate::Exists {
                path: "inputs.foo".into(),
            },
            code: "error.regular_refusal".into(),
            message: "this should not appear".into(),
        }];
        // Do NOT supply the gate.
        let inv = minimal_invocation(&def, "/tmp");
        let ctx = MacroPlannerContext::default();

        let plan =
            MacroPlanner::plan(&inv, &def, &ctx).expect("gate refusal returns Ok(MacroPlan)");

        assert_eq!(plan.refusals.len(), 1, "only the gate refusal should fire");
        assert_eq!(
            plan.refusals[0].code, "error.authority_required",
            "gate refusal must take precedence over regular refusals"
        );
    }

    // ── Lowering: template_only refuses lowering ──────────────────────────────

    #[test]
    fn template_only_status_refuses_lowering() {
        // Manually construct a MacroPlan with a non-record op that has template_only status
        use crate::model::MacroPlan;
        let plan = MacroPlan {
            macro_id: "test.macro".into(),
            summary: "test".into(),
            semantic_status: MacroSemanticStatus::TemplateOnly,
            operations: vec![MacroPlanOperation {
                kind: "emit".into(),
                name: Some("foo".into()),
                semantic_status: MacroSemanticStatus::TemplateOnly,
                summary: "Emit Foo".into(),
            }],
            edits: EditSet {
                file_creates: vec![bbox_refactor::FileCreate {
                    path: "/tmp/Foo.java".into(),
                    content: "class Foo {}".into(),
                }],
                file_edits: vec![],
                file_moves: vec![],
                backends_used: vec!["java_poet".into()],
            },
            checks: vec![],
            questions: vec![],
            refusals: vec![],
            backends_used: vec!["java_poet".into()],
            operator_opt_outs_used: vec![],
            provenance: json!({}),
        };

        let err = MacroPlanner::lower(&plan).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("error.template_only_lowering_refused"),
            "lowering a template_only plan must fail: {msg}"
        );
    }

    #[test]
    fn record_only_plan_lowers_to_syntax_only() {
        // A plan with only Record ops (template_only each) should lower successfully
        // because Record is non-mutating (excluded from the tier computation).
        use crate::model::MacroPlan;
        let plan = MacroPlan {
            macro_id: "test.macro".into(),
            summary: "only notes".into(),
            semantic_status: MacroSemanticStatus::TemplateOnly,
            operations: vec![MacroPlanOperation {
                kind: "record".into(), // non-mutating → excluded from lowering refusal
                name: Some("note".into()),
                semantic_status: MacroSemanticStatus::TemplateOnly,
                summary: "Record a note".into(),
            }],
            // Need at least one file create/edit for validate_plan_shape (called by apply)
            // but lower() itself doesn't call validate_plan_shape.
            edits: EditSet::default(),
            checks: vec![],
            questions: vec!["[note] do something".into()],
            refusals: vec![],
            backends_used: vec![],
            operator_opt_outs_used: vec![],
            provenance: json!({}),
        };

        // lower() itself should succeed (validate_plan_shape is called by apply, not lower)
        let rp = MacroPlanner::lower(&plan).expect("record-only plan should lower");
        assert_eq!(rp.semantic_status, SemanticStatus::SyntaxOnly);
    }

    // ── macro_apply bypass flags assertion ────────────────────────────────────

    #[test]
    fn macro_apply_builds_params_with_no_bypass_flags() {
        // Verify that the helper that builds RefactorApplyParams for macro_apply
        // sets allow_dirty_worktree, allow_unregistered_paths, and force_path to None.
        // We test this by calling the helper directly.
        let plan_value = json!({
            "title": "macro:test",
            "kind": "macro:test.macro",
            "semantic_status": "syntax_only",
            "dry_run": true,
            "file_creates": [{
                "path": "/tmp/Foo.java",
                "content": "class Foo {}"
            }],
            "edits": [],
            "validations": [],
            "items": [],
            "leftovers": [],
            "plan_status": "planned"
        });

        let params = build_macro_apply_params(
            plan_value,
            Some(true), // confirm
            None,       // cwd
        );

        assert_eq!(
            params.confirm,
            Some(true),
            "confirm must be threaded through"
        );
        assert!(
            params.allow_dirty_worktree.is_none(),
            "allow_dirty_worktree must NOT be set (default=None → false in apply)"
        );
        assert!(
            params.allow_unregistered_paths.is_none(),
            "allow_unregistered_paths must NOT be set (default=None → false in apply)"
        );
        assert!(
            params.force_path.is_none(),
            "force_path must NOT be set (default=None → false in apply)"
        );
        assert!(params.plan_path.is_none(), "plan_path must be None");
    }

    // ── Input validation ──────────────────────────────────────────────────────

    #[test]
    fn missing_required_input_is_rejected() {
        let mut def = minimal_def();
        def.inputs_schema = json!({
            "type": "object",
            "properties": {
                "service_name": {"type": "string"}
            },
            "required": ["service_name"]
        });
        let inv = minimal_invocation(&def, "/tmp");
        let ctx = MacroPlannerContext::default();
        let err = MacroPlanner::plan(&inv, &def, &ctx).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("error.missing_required_input"),
            "missing required input should be rejected: {msg}"
        );
        assert!(
            msg.contains("service_name"),
            "error should name the field: {msg}"
        );
    }

    #[test]
    fn wrong_type_input_is_rejected() {
        let mut def = minimal_def();
        def.inputs_schema = json!({
            "type": "object",
            "properties": {
                "count": {"type": "integer"}
            }
        });
        let mut inv = minimal_invocation(&def, "/tmp");
        inv.inputs.insert("count".into(), json!("not_a_number"));
        let ctx = MacroPlannerContext::default();
        let err = MacroPlanner::plan(&inv, &def, &ctx).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("error.input_type_mismatch"),
            "type mismatch should be rejected: {msg}"
        );
    }

    // ── Semantic status aggregation ───────────────────────────────────────────

    #[test]
    fn all_same_status_aggregates_to_that_status() {
        let statuses = vec![
            MacroSemanticStatus::SyntaxOnly,
            MacroSemanticStatus::SyntaxOnly,
        ];
        assert_eq!(aggregate_status(&statuses), MacroSemanticStatus::SyntaxOnly);
    }

    #[test]
    fn different_statuses_aggregate_to_mixed() {
        let statuses = vec![
            MacroSemanticStatus::SyntaxOnly,
            MacroSemanticStatus::LspVerified,
        ];
        assert_eq!(aggregate_status(&statuses), MacroSemanticStatus::Mixed);
    }

    #[test]
    fn empty_statuses_aggregate_to_template_only() {
        assert_eq!(aggregate_status(&[]), MacroSemanticStatus::TemplateOnly);
    }

    // ── Record op: notes appear in plan_questions ─────────────────────────────

    #[test]
    fn record_op_appends_to_questions() {
        let mut def = minimal_def();
        def.inputs_schema = json!({
            "type": "object",
            "properties": {
                "service_name": {"type": "string"}
            }
        });
        def.operations = vec![MacroOperation::Record {
            label: "manual_wiring".into(),
            body: "Wire Guice binding for ${inputs.service_name}".into(),
        }];
        let mut inv = minimal_invocation(&def, "/tmp");
        inv.inputs
            .insert("service_name".into(), json!("OrderService"));
        let ctx = MacroPlannerContext::default();

        let plan = MacroPlanner::plan(&inv, &def, &ctx).expect("plan should succeed");
        assert_eq!(plan.questions.len(), 1);
        assert!(
            plan.questions[0].contains("OrderService"),
            "question should have interpolated value: {}",
            plan.questions[0]
        );
        assert!(
            plan.questions[0].contains("manual_wiring"),
            "question should include label: {}",
            plan.questions[0]
        );
    }

    // ── Delegate residue surfaces in MacroPlan.questions ─────────────────────

    #[test]
    fn delegate_residue_surfaces_in_questions() {
        use bbox_refactor::{ExternalCall, ExtractedCallSite, PlanStatus};

        // Construct a RefactorPlan with leftovers + external_calls residue.
        // The dup-check and edit-merge paths in plan() are not exercised here —
        // we test surface_delegate_residue directly (same file, accessible via
        // `use super::*`).
        let rp = RefactorPlan {
            title: "test delegate plan".into(),
            kind: "extract_java_methods".into(),
            semantic_status: bbox_refactor::SemanticStatus::SyntaxOnly,
            dry_run: true,
            file_moves: vec![],
            file_creates: vec![],
            edits: vec![],
            validations: vec![],
            items: vec![],
            leftovers: vec![
                "MyHelper.doWork() is not in the extraction set".into(),
                "AnotherHelper.process() is external".into(),
            ],
            captured_variables: vec![],
            remaining_source_accessors: vec![],
            remaining_source_constant_refs: vec![],
            external_calls: vec![ExternalCall {
                method: "compute".into(),
                signature: "void compute()".into(),
                signature_partial: false,
                source_visibility: None,
                source_is_static: false,
                recommended_resolution: None,
                call_sites: vec![ExtractedCallSite {
                    line: 10,
                    column: 4,
                    in_method: "run".into(),
                    context: "direct".into(),
                }],
            }],
            inherited_dependencies: vec![],
            deep_analysis: None,
            plan_status: PlanStatus::Planned,
            fixme_count: None,
            operator_opt_outs_used: vec![],
        };

        let mut questions: Vec<String> = vec![];
        surface_delegate_residue("extract_java_methods", &rp, &mut questions);

        // 2 leftovers + 1 external_calls count = 3 questions total
        assert_eq!(
            questions.len(),
            3,
            "expected 2 leftover questions + 1 external_calls question; got: {:?}",
            questions
        );

        assert!(
            questions[0].contains("leftover") && questions[0].contains("MyHelper.doWork()"),
            "first leftover must surface with kind prefix: {}",
            questions[0]
        );
        assert!(
            questions[0].contains("extract_java_methods"),
            "question must include delegate kind: {}",
            questions[0]
        );
        assert!(
            questions[1].contains("leftover") && questions[1].contains("AnotherHelper"),
            "second leftover must surface: {}",
            questions[1]
        );
        assert!(
            questions[2].contains("external_call") && questions[2].contains("1"),
            "external_calls count (1) must surface: {}",
            questions[2]
        );
    }

    #[test]
    fn delegate_residue_empty_plan_adds_no_questions() {
        // A plan with all-empty residue fields must not produce any questions.
        let rp = RefactorPlan {
            title: "clean plan".into(),
            kind: "create_file".into(),
            semantic_status: bbox_refactor::SemanticStatus::SyntaxOnly,
            dry_run: true,
            file_moves: vec![],
            file_creates: vec![],
            edits: vec![],
            validations: vec![],
            items: vec![],
            leftovers: vec![],
            captured_variables: vec![],
            remaining_source_accessors: vec![],
            remaining_source_constant_refs: vec![],
            external_calls: vec![],
            inherited_dependencies: vec![],
            deep_analysis: None,
            plan_status: bbox_refactor::PlanStatus::Planned,
            fixme_count: None,
            operator_opt_outs_used: vec![],
        };

        let mut questions: Vec<String> = vec![];
        surface_delegate_residue("create_file", &rp, &mut questions);

        assert!(
            questions.is_empty(),
            "all-empty residue must produce no questions; got: {:?}",
            questions
        );
    }

    // ── P4b: MockProbeRunner + probe execution tests ──────────────────────────

    /// Test-only probe runner that returns canned [`ProbeOutput`] by probe name.
    ///
    /// If the probe name is not in `canned`, returns an error so tests can
    /// exercise the fail-closed path. Also supports a `ctx_check` mode where
    /// probe B can assert that probe A's result is already in `ctx.probes`.
    struct MockProbeRunner {
        /// Canned outputs keyed by probe name.
        canned: std::collections::HashMap<String, crate::probe::ProbeOutput>,
    }

    impl MockProbeRunner {
        fn new() -> Self {
            Self {
                canned: std::collections::HashMap::new(),
            }
        }

        fn with(
            mut self,
            name: &str,
            value: serde_json::Value,
            status: MacroSemanticStatus,
        ) -> Self {
            self.canned.insert(
                name.to_string(),
                crate::probe::ProbeOutput {
                    value,
                    semantic_status: status,
                    truncated: false,
                    diagnostics: vec![],
                },
            );
            self
        }
    }

    impl crate::probe::ProbeRunner for MockProbeRunner {
        fn run_probe(
            &self,
            name: &str,
            _spec: &crate::probe::ProbeSpec,
            _ctx: &crate::expr::Context,
            _invocation: &crate::model::MacroInvocation,
        ) -> anyhow::Result<crate::probe::ProbeOutput> {
            self.canned.get(name).cloned().ok_or_else(|| {
                anyhow::anyhow!(
                    "mock: no canned output for probe '{}'; returning error",
                    name
                )
            })
        }
    }

    /// Build a `MacroPlannerContext` with a mock probe runner.
    fn ctx_with_mock(mock: MockProbeRunner) -> MacroPlannerContext {
        use crate::backend::UnavailableBackend;
        MacroPlannerContext::new(Box::new(UnavailableBackend), None, Box::new(mock))
    }

    fn probe_spec_code_symbols() -> serde_json::Value {
        json!({"kind": "code_symbols"})
    }

    // ── probe-driven refusal: exists=true fires ───────────────────────────────

    #[test]
    fn probe_driven_refusal_fires_when_exists_true() {
        let mut def = minimal_def();
        def.probes = vec![MacroProbe {
            name: "binding".into(),
            description: "Check binding".into(),
            spec: probe_spec_code_symbols(),
        }];
        def.refusals = vec![MacroRefusal {
            when: crate::expr::Predicate::Exists {
                path: "binding.exists".into(),
            },
            code: "error.already_bound".into(),
            message: "Already bound: ${binding.exists}".into(),
        }];

        let mock = MockProbeRunner::new().with(
            "binding",
            json!({"exists": true, "count": 1}),
            MacroSemanticStatus::SyntaxOnly,
        );
        let inv = minimal_invocation(&def, "/tmp");
        let ctx = ctx_with_mock(mock);

        let plan = MacroPlanner::plan(&inv, &def, &ctx)
            .expect("probe-driven refusal should return Ok(MacroPlan)");

        assert!(!plan.refusals.is_empty(), "refusal must fire");
        assert_eq!(plan.refusals[0].code, "error.already_bound");
        assert!(
            plan.refusals[0].message.contains("true"),
            "message must interpolate binding.exists: {}",
            plan.refusals[0].message
        );
        // Probe ops are still recorded in the plan for auditability.
        assert!(
            plan.operations.iter().any(|op| op.kind == "probe"),
            "probe op should appear in plan.operations even on refusal"
        );
    }

    // ── probe-driven refusal: exists=false does NOT fire ──────────────────────

    #[test]
    fn probe_driven_refusal_does_not_fire_when_exists_false() {
        let mut def = minimal_def();
        def.probes = vec![MacroProbe {
            name: "binding".into(),
            description: "Check binding".into(),
            spec: probe_spec_code_symbols(),
        }];
        def.refusals = vec![MacroRefusal {
            when: crate::expr::Predicate::Exists {
                path: "binding.exists".into(),
            },
            code: "error.already_bound".into(),
            message: "already bound".into(),
        }];

        // binding.exists is false → Exists predicate is true only when path
        // resolves. Since binding.exists = false (bool), it DOES resolve.
        // Use a path that won't resolve: "binding.nonexistent"
        let mut def2 = def.clone();
        def2.refusals[0].when = crate::expr::Predicate::Exists {
            path: "binding.nonexistent".into(),
        };

        let mock = MockProbeRunner::new().with(
            "binding",
            json!({"exists": false, "count": 0}),
            MacroSemanticStatus::SyntaxOnly,
        );
        let inv = minimal_invocation(&def2, "/tmp");
        let ctx = ctx_with_mock(mock);

        let plan = MacroPlanner::plan(&inv, &def2, &ctx)
            .expect("plan should succeed when refusal does not fire");

        assert!(plan.refusals.is_empty(), "refusal must NOT fire");
    }

    // ── ordered cross-probe: probe B sees probe A's result ────────────────────

    #[test]
    fn ordered_cross_probe_b_refusal_references_a_result() {
        let mut def = minimal_def();
        def.probes = vec![
            MacroProbe {
                name: "probe_a".into(),
                description: "First probe".into(),
                spec: probe_spec_code_symbols(),
            },
            MacroProbe {
                name: "probe_b".into(),
                description: "Second probe (references probe_a context)".into(),
                spec: probe_spec_code_symbols(),
            },
        ];
        // Refusal fires when BOTH probe_a.exists AND probe_b.exists are true.
        def.refusals = vec![MacroRefusal {
            when: crate::expr::Predicate::All {
                predicates: vec![
                    crate::expr::Predicate::Eq {
                        path: "probe_a.exists".into(),
                        value: json!(true),
                    },
                    crate::expr::Predicate::Eq {
                        path: "probe_b.exists".into(),
                        value: json!(true),
                    },
                ],
            },
            code: "error.both_probes_found".into(),
            message: "Both probes found".into(),
        }];

        let mock = MockProbeRunner::new()
            .with(
                "probe_a",
                json!({"exists": true, "count": 1}),
                MacroSemanticStatus::SyntaxOnly,
            )
            .with(
                "probe_b",
                json!({"exists": true, "count": 2}),
                MacroSemanticStatus::SyntaxOnly,
            );
        let inv = minimal_invocation(&def, "/tmp");
        let ctx = ctx_with_mock(mock);

        let plan =
            MacroPlanner::plan(&inv, &def, &ctx).expect("cross-probe refusal should return Ok");
        assert!(!plan.refusals.is_empty(), "cross-probe refusal must fire");
        assert_eq!(plan.refusals[0].code, "error.both_probes_found");
        // Both probe ops must be in the plan
        let probe_ops: Vec<_> = plan
            .operations
            .iter()
            .filter(|op| op.kind == "probe")
            .collect();
        assert_eq!(probe_ops.len(), 2, "both probe ops must appear");
    }

    // ── unknown-root typo in refusal predicate → planning ERROR ──────────────

    #[test]
    fn unknown_root_in_refusal_predicate_is_planning_error() {
        let mut def = minimal_def();
        def.probes = vec![MacroProbe {
            name: "binding".into(),
            description: "binding probe".into(),
            spec: probe_spec_code_symbols(),
        }];
        // Typo: "bindng" instead of "binding"
        def.refusals = vec![MacroRefusal {
            when: crate::expr::Predicate::Exists {
                path: "bindng.exists".into(), // typo
            },
            code: "error.bad_predicate".into(),
            message: "bad".into(),
        }];

        let mock = MockProbeRunner::new().with(
            "binding",
            json!({"exists": true}),
            MacroSemanticStatus::SyntaxOnly,
        );
        let inv = minimal_invocation(&def, "/tmp");
        let ctx = ctx_with_mock(mock);

        let err = MacroPlanner::plan(&inv, &def, &ctx).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("error.unknown_context_root"),
            "typo 'bindng' must be a planning error, got: {msg}"
        );
        assert!(
            msg.contains("bindng"),
            "error must name the bad root: {msg}"
        );
    }

    // ── probe runner error propagates as planning error ───────────────────────

    #[test]
    fn probe_runner_error_propagates_as_planning_error() {
        let mut def = minimal_def();
        def.probes = vec![MacroProbe {
            name: "failing_probe".into(),
            description: "This will fail".into(),
            spec: probe_spec_code_symbols(),
        }];

        // MockProbeRunner returns Err for "failing_probe" (no canned output).
        let mock = MockProbeRunner::new(); // empty → will error on any name
        let inv = minimal_invocation(&def, "/tmp");
        let ctx = ctx_with_mock(mock);

        let err = MacroPlanner::plan(&inv, &def, &ctx).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("failing_probe") || msg.contains("error.probe_failed"),
            "probe runner error must propagate, got: {msg}"
        );
    }

    // ── UnavailableProbeRunner default → error.probe_backend_unavailable ──────

    #[test]
    fn unavailable_probe_runner_fails_closed() {
        let mut def = minimal_def();
        def.probes = vec![MacroProbe {
            name: "syms".into(),
            description: "symbols probe".into(),
            spec: json!({"kind": "code_symbols"}),
        }];
        let inv = minimal_invocation(&def, "/tmp");
        let ctx = MacroPlannerContext::default(); // UnavailableProbeRunner

        let err = MacroPlanner::plan(&inv, &def, &ctx).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("error.probe_backend_unavailable"),
            "UnavailableProbeRunner must fail closed: {msg}"
        );
    }

    // ── semantic_status: worst-tier aggregation and lower() isolation ─────────

    #[test]
    fn probe_status_included_in_macro_plan_aggregate_but_excluded_from_lower() {
        // A probe with LspVerified status + a create_file delegate (SyntaxOnly).
        // MacroPlan.semantic_status → Mixed (LspVerified vs SyntaxOnly differ).
        // lower() → SyntaxOnly (worst of mutating ops only, probe excluded).
        let tmp = tempfile::tempdir().expect("tempdir");
        let project_dir = tmp.path().to_string_lossy().to_string();
        let target_path = tmp.path().join("Gen.java").to_string_lossy().to_string();

        let mut def = minimal_def();
        def.probes = vec![MacroProbe {
            name: "ws".into(),
            description: "workspace probe".into(),
            spec: json!({"kind": "code_symbols"}),
        }];
        def.operations = vec![MacroOperation::DelegateRefactor {
            refactor_kind: "create_file".into(),
            params: json!({"source": target_path, "new_text": "class Gen {}"}),
        }];

        let mock = MockProbeRunner::new().with(
            "ws",
            json!({"exists": true, "count": 1}),
            MacroSemanticStatus::LspVerified, // better than SyntaxOnly from create_file
        );
        let inv = minimal_invocation(&def, &project_dir);
        let ctx = ctx_with_mock(mock);

        let plan = MacroPlanner::plan(&inv, &def, &ctx).expect("plan should succeed");

        // MacroPlan aggregate: LspVerified (probe) + SyntaxOnly (delegate) → Mixed
        assert_eq!(
            plan.semantic_status,
            MacroSemanticStatus::Mixed,
            "aggregate must be Mixed when probe and op differ: {:?}",
            plan.semantic_status
        );

        // lower() uses only mutating ops → SyntaxOnly from create_file only
        let rp = MacroPlanner::lower(&plan).expect("lowering must succeed");
        assert_eq!(
            rp.semantic_status,
            SemanticStatus::SyntaxOnly,
            "lowered plan must use SyntaxOnly (probe status must not leak): {:?}",
            rp.semantic_status
        );
    }

    // ── probe-only plan: lower() to SyntaxOnly default ───────────────────────

    #[test]
    fn probe_only_plan_lowers_to_syntax_only_default() {
        // A plan with only probe ops (no mutating ops) should lower to SyntaxOnly.
        let mut def = minimal_def();
        def.probes = vec![MacroProbe {
            name: "ws".into(),
            description: "probe".into(),
            spec: json!({"kind": "code_symbols"}),
        }];
        // No operations → pure probe plan

        let mock = MockProbeRunner::new().with(
            "ws",
            json!({"exists": false, "count": 0}),
            MacroSemanticStatus::LspVerified,
        );
        let inv = minimal_invocation(&def, "/tmp");
        let ctx = ctx_with_mock(mock);

        let plan = MacroPlanner::plan(&inv, &def, &ctx).expect("plan should succeed");
        // LspVerified probe → aggregate = LspVerified (single status, all same)
        assert_eq!(plan.semantic_status, MacroSemanticStatus::LspVerified);

        // lower() sees no mutating ops → defaults to SyntaxOnly
        let rp = MacroPlanner::lower(&plan).expect("lowering probe-only plan must succeed");
        assert_eq!(rp.semantic_status, SemanticStatus::SyntaxOnly);
    }

    // ── FIX 3: refusal referencing inline-op probe name → error.unknown_context_root
    //
    // Inline-operation probe names are not in scope when refusal predicates
    // evaluate (refusals fire before operations run). A refusal that references
    // an inline probe name must surface as error.unknown_context_root — NOT
    // silently return false (which would hide the logic error).

    #[test]
    fn refusal_referencing_inline_probe_name_is_unknown_context_root() {
        let mut def = minimal_def();

        // Inline probe named "inline_sym" — NOT a top-level probe.
        def.operations = vec![MacroOperation::Probe {
            name: "inline_sym".into(),
            spec: json!({"kind": "code_symbols"}),
        }];

        // Refusal predicate references "inline_sym" which is only populated
        // by the inline probe op (executed AFTER refusals). This must error.
        def.refusals = vec![MacroRefusal {
            when: crate::expr::Predicate::Exists {
                path: "inline_sym.exists".into(),
            },
            code: "error.should_not_reach".into(),
            message: "inline probe result not available at refusal time".into(),
        }];

        let inv = minimal_invocation(&def, "/tmp");
        let ctx = MacroPlannerContext::default();

        let err = MacroPlanner::plan(&inv, &def, &ctx).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("error.unknown_context_root"),
            "refusal referencing inline probe name must be error.unknown_context_root, got: {msg}"
        );
        assert!(
            msg.contains("inline_sym"),
            "error must name the unknown root: {msg}"
        );
    }

    #[test]
    fn refusal_referencing_top_level_probe_name_succeeds() {
        // Sanity-check: top-level probe name IS in scope at refusal time.
        let mut def = minimal_def();
        def.probes = vec![MacroProbe {
            name: "top_probe".into(),
            description: "top-level".into(),
            spec: json!({"kind": "code_symbols"}),
        }];
        // Use Eq (not Exists): the normalized probe shape always has an
        // `exists` key, so Exists{"top_probe.exists"} is always true. To gate
        // on the boolean value we compare it. With exists=false below, the
        // refusal must NOT fire — proving the top-level probe name resolves.
        def.refusals = vec![MacroRefusal {
            when: crate::expr::Predicate::Eq {
                path: "top_probe.exists".into(),
                value: json!(true),
            },
            code: "error.already_exists".into(),
            message: "already exists".into(),
        }];

        let mock = MockProbeRunner::new().with(
            "top_probe",
            json!({"exists": false, "count": 0}),
            MacroSemanticStatus::SyntaxOnly,
        );
        let inv = minimal_invocation(&def, "/tmp");
        let ctx = ctx_with_mock(mock);

        // Should not error — top-level probe name is valid in refusal predicates.
        let plan = MacroPlanner::plan(&inv, &def, &ctx)
            .expect("top-level probe name must be valid in refusal predicate");
        assert!(
            plan.refusals.is_empty(),
            "refusal should not fire (exists=false)"
        );
    }

    // ── FIX 4: planner-side interpolation rejection (defense-in-depth)

    #[test]
    fn planner_rejects_interpolation_in_top_level_probe_spec() {
        let mut def = minimal_def();
        def.probes = vec![MacroProbe {
            name: "sym".into(),
            description: "bad spec".into(),
            spec: json!({
                "kind": "code_query",
                "file": "${inputs.file}",
                "query": "(class_declaration) @c"
            }),
        }];
        let inv = minimal_invocation(&def, "/tmp");
        let ctx = MacroPlannerContext::default();

        let err = MacroPlanner::plan(&inv, &def, &ctx).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("error.probe_spec_interpolation")
                || msg.contains("interpolation in probe/operation specs is not yet supported"),
            "planner must reject interpolation in probe spec, got: {msg}"
        );
    }

    #[test]
    fn planner_rejects_interpolation_in_inline_probe_spec() {
        let mut def = minimal_def();
        def.operations = vec![MacroOperation::Probe {
            name: "inline_sym".into(),
            spec: json!({
                "kind": "workspace_symbol",
                "query": "${inputs.class_name}"
            }),
        }];
        let inv = minimal_invocation(&def, "/tmp");
        let ctx = MacroPlannerContext::default();

        let err = MacroPlanner::plan(&inv, &def, &ctx).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("error.probe_spec_interpolation")
                || msg.contains("interpolation in probe/operation specs is not yet supported"),
            "planner must reject interpolation in inline probe spec, got: {msg}"
        );
    }

    // ── P3a: interpolate_value_strings helper ─────────────────────────────────

    #[test]
    fn interpolate_value_strings_expands_string_leaves() {
        let ctx = expr::Context {
            inputs: {
                let mut m = serde_json::Map::new();
                m.insert("pkg".into(), json!("com.example"));
                m.insert("name".into(), json!("PaymentService"));
                m
            },
            probes: std::collections::HashMap::new(),
            locals: std::collections::HashMap::new(),
        };
        let v = json!({
            "op": "emit_type",
            "source_root": "/repo/src/main/java",
            "package": "${inputs.pkg}",
            "name": "${inputs.name}",
            "kind": "interface",
            "source_text": "package ${inputs.pkg};\npublic interface ${inputs.name} {}",
            "count": 5,
            "flag": true
        });
        let out = interpolate_value_strings(&v, &ctx).expect("interpolation should succeed");
        assert_eq!(out["package"], json!("com.example"));
        assert_eq!(out["name"], json!("PaymentService"));
        // Non-string leaves unchanged
        assert_eq!(out["count"], json!(5));
        assert_eq!(out["flag"], json!(true));
        // source_text expanded
        assert!(
            out["source_text"].as_str().unwrap().contains("com.example"),
            "source_text leaf should be interpolated"
        );
    }

    #[test]
    fn interpolate_value_strings_passes_through_non_string_leaves() {
        let ctx = expr::Context::default();
        let v = json!({
            "count": 42,
            "active": false,
            "ratio": 2.5,
            "nothing": null,
            "arr": [1, 2, 3]
        });
        let out = interpolate_value_strings(&v, &ctx).expect("no strings to interpolate");
        assert_eq!(out, v);
    }

    #[test]
    fn interpolate_value_strings_errors_on_missing_path() {
        let ctx = expr::Context::default();
        let v = json!({"package": "${inputs.ghost}"});
        let err = interpolate_value_strings(&v, &ctx).unwrap_err();
        assert!(
            err.to_string().contains("ghost") || err.to_string().contains("interpolation"),
            "error should mention the missing path or interpolation"
        );
    }

    // ── P3a: probe spec ${inputs.*} interpolation ────────────────────────────

    /// A probe spec containing `"${inputs.method}"` should expand from inputs
    /// before the ProbeSpec is decoded — so the runner sees the real query
    /// string, not a literal placeholder.
    #[test]
    fn probe_spec_inputs_interpolation_runs_before_decode() {
        use crate::probe::{ProbeOutput, ProbeRunner, ProbeSpec};

        struct CapturingProbeRunner {
            captured_query: std::sync::Mutex<Option<String>>,
        }
        impl ProbeRunner for CapturingProbeRunner {
            fn run_probe(
                &self,
                _name: &str,
                spec: &ProbeSpec,
                _ctx: &crate::expr::Context,
                _inv: &crate::model::MacroInvocation,
            ) -> anyhow::Result<ProbeOutput> {
                if let ProbeSpec::CodeSymbols { query, .. } = spec {
                    *self.captured_query.lock().unwrap() = query.clone();
                }
                Ok(ProbeOutput {
                    value: json!({"exists": true, "count": 1, "items": []}),
                    semantic_status: crate::model::MacroSemanticStatus::SyntaxOnly,
                    truncated: false,
                    diagnostics: vec![],
                })
            }
        }

        let mut def = minimal_def();
        def.probes = vec![crate::model::MacroProbe {
            name: "method_probe".into(),
            description: "checks method exists".into(),
            // spec contains ${inputs.method_name} placeholder
            spec: json!({"kind": "code_symbols", "query": "${inputs.method_name}"}),
        }];

        let runner = std::sync::Arc::new(CapturingProbeRunner {
            captured_query: std::sync::Mutex::new(None),
        });

        // We need a ProbeRunner that is Send + Sync — wrap in a newtype.
        struct SharedRunner(std::sync::Arc<CapturingProbeRunner>);
        impl ProbeRunner for SharedRunner {
            fn run_probe(
                &self,
                name: &str,
                spec: &ProbeSpec,
                ctx: &crate::expr::Context,
                inv: &crate::model::MacroInvocation,
            ) -> anyhow::Result<ProbeOutput> {
                self.0.run_probe(name, spec, ctx, inv)
            }
        }

        let ctx = MacroPlannerContext::new(
            Box::new(crate::backend::UnavailableBackend),
            None,
            Box::new(SharedRunner(runner.clone())),
        );

        let mut inv = minimal_invocation(&def, "/tmp");
        inv.inputs
            .insert("method_name".into(), json!("processOrder"));

        let plan = MacroPlanner::plan(&inv, &def, &ctx).expect("plan should succeed");
        assert!(plan.refusals.is_empty(), "no refusals expected");

        // The runner should have received "processOrder", not "${inputs.method_name}"
        let captured = runner.captured_query.lock().unwrap().clone();
        assert_eq!(
            captured.as_deref(),
            Some("processOrder"),
            "probe spec query should be interpolated to the input value before decode"
        );
    }

    /// Splice-interpolation: a pure `"${path}"` element in an array whose
    /// resolved value is itself an array is flattened into the parent array.
    #[test]
    fn interpolate_value_strings_splices_array_valued_inputs() {
        let ctx = expr::Context {
            inputs: {
                let mut m = serde_json::Map::new();
                m.insert("param_types".into(), json!(["Order", "int"]));
                m
            },
            probes: std::collections::HashMap::new(),
            locals: std::collections::HashMap::new(),
        };
        let v = json!({"parameter_types": ["${inputs.param_types}"]});
        let out = interpolate_value_strings(&v, &ctx).expect("splice interpolation should succeed");
        assert_eq!(
            out["parameter_types"],
            json!(["Order", "int"]),
            "array-valued input should be spliced into the parent array"
        );
    }

    // ── P3a: Emit/Rewrite with UnavailableBackend still fails closed ──────────

    #[test]
    fn emit_with_unavailable_backend_yields_backend_unavailable() {
        let mut def = minimal_def();
        def.operations = vec![MacroOperation::Emit {
            name: "interface_file".into(),
            backend_op: json!({
                "op": "emit_type",
                "source_root": "/repo/src/main/java",
                "package": "com.example",
                "name": "Foo",
                "kind": "interface",
                "source_text": "package com.example;\npublic interface Foo {}"
            }),
            when: None,
        }];
        let inv = minimal_invocation(&def, "/tmp");
        // Default ctx uses UnavailableBackend
        let ctx = MacroPlannerContext::default();
        let err = MacroPlanner::plan(&inv, &def, &ctx).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("error.backend_unavailable"),
            "Emit with UnavailableBackend should yield error.backend_unavailable; got: {msg}"
        );
    }

    #[test]
    fn rewrite_with_unavailable_backend_yields_backend_unavailable() {
        let mut def = minimal_def();
        def.operations = vec![MacroOperation::Rewrite {
            targets: vec!["/repo/src/FooImpl.java".into()],
            backend_op: json!({
                "op": "insert_member",
                "target_file": "/repo/src/FooImpl.java",
                "target_type": "FooImpl",
                "member_text": "public void doWork() {}",
                "imports": []
            }),
            when: None,
        }];
        let inv = minimal_invocation(&def, "/tmp");
        let ctx = MacroPlannerContext::default();
        let err = MacroPlanner::plan(&inv, &def, &ctx).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("error.backend_unavailable"),
            "Rewrite with UnavailableBackend should yield error.backend_unavailable; got: {msg}"
        );
    }

    // ── P3a: malformed backend_op is a hard planning error ────────────────────

    #[test]
    fn emit_malformed_backend_op_is_hard_plan_error() {
        let mut def = minimal_def();
        def.operations = vec![MacroOperation::Emit {
            name: "bad_op".into(),
            // Missing required fields; unknown op tag
            backend_op: json!({"op": "unknown_unknown_op", "foo": "bar"}),
            when: None,
        }];
        let inv = minimal_invocation(&def, "/tmp");
        let ctx = MacroPlannerContext::default();
        let err = MacroPlanner::plan(&inv, &def, &ctx).unwrap_err();
        let msg = err.to_string();
        // Either the decode fails with error.macro_invalid or the UnavailableBackend
        // triggers error.backend_unavailable. Both are valid fail-closed outcomes;
        // the key is that neither silently produces a plan.
        assert!(
            msg.contains("error.macro_invalid")
                || msg.contains("error.backend_unavailable")
                || msg.contains("unknown variant"),
            "malformed emit backend_op must yield a hard plan error; got: {msg}"
        );
    }

    #[test]
    fn rewrite_malformed_backend_op_is_hard_plan_error() {
        let mut def = minimal_def();
        def.operations = vec![MacroOperation::Rewrite {
            targets: vec!["/repo/src/Foo.java".into()],
            backend_op: json!({"op": "totally_unknown_rewrite", "x": 99}),
            when: None,
        }];
        let inv = minimal_invocation(&def, "/tmp");
        let ctx = MacroPlannerContext::default();
        let err = MacroPlanner::plan(&inv, &def, &ctx).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("error.macro_invalid")
                || msg.contains("error.backend_unavailable")
                || msg.contains("unknown variant"),
            "malformed rewrite backend_op must yield a hard plan error; got: {msg}"
        );
    }

    // ── when-guard predicate on Emit/Rewrite ──────────────────────────────────

    /// When the `when` guard on an Emit op evaluates to `false`, the backend
    /// is NOT called (UnavailableBackend would return Err if called) and a
    /// "skipped (guard false)" summary is recorded in the plan.
    #[test]
    fn emit_with_false_guard_is_skipped_without_calling_backend() {
        use crate::expr::Predicate;
        let mut def = minimal_def();
        // inject probe result: service_type_exists.count = 1 (type already exists)
        def.probes = vec![]; // no real probes — we inject via inputs trick below
        def.operations = vec![MacroOperation::Emit {
            name: "interface_file".into(),
            backend_op: json!({
                "op": "emit_type",
                "source_root": "/repo/src/main/java",
                "package": "com.example",
                "name": "Foo",
                "kind": "interface",
                "source_text": "package com.example;\npublic interface Foo {}"
            }),
            // Guard: only emit when inputs.skip is NOT "true".
            // Here we use Eq on an input that equals "yes" → guard = false.
            when: Some(Predicate::Eq {
                path: "inputs.skip".into(),
                value: serde_json::json!("no"),
            }),
        }];
        let mut inv = minimal_invocation(&def, "/tmp");
        // Set inputs.skip = "yes" so the guard (Eq skip=="no") = false.
        inv.inputs.insert("skip".into(), serde_json::json!("yes"));
        // UnavailableBackend would fail if called — proves the backend is NOT reached.
        let ctx = MacroPlannerContext::default();
        let plan = MacroPlanner::plan(&inv, &def, &ctx).expect("plan should succeed");
        // Exactly one op recorded: the skipped emit.
        assert_eq!(plan.operations.len(), 1, "expected exactly one op entry");
        let op = &plan.operations[0];
        assert_eq!(op.kind, "emit");
        assert!(
            op.summary.contains("skipped (guard false)"),
            "op summary must say 'skipped (guard false)'; got: {}",
            op.summary
        );
        // No edits produced (backend was not called).
        assert!(
            plan.edits.file_creates.is_empty(),
            "no file_creates expected"
        );
        assert!(plan.edits.file_edits.is_empty(), "no file_edits expected");
    }

    /// When the `when` guard on a Rewrite op evaluates to `false`, the backend
    /// is NOT called and a "skipped (guard false)" summary is recorded.
    #[test]
    fn rewrite_with_false_guard_is_skipped_without_calling_backend() {
        use crate::expr::Predicate;
        let mut def = minimal_def();
        def.operations = vec![MacroOperation::Rewrite {
            targets: vec!["/repo/src/FooImpl.java".into()],
            backend_op: json!({
                "op": "insert_member",
                "target_file": "/repo/src/FooImpl.java",
                "target_type": "FooImpl",
                "member_text": "public void doWork() {}",
                "imports": []
            }),
            // Guard: only rewrite when inputs.enabled == "yes".
            when: Some(Predicate::Eq {
                path: "inputs.enabled".into(),
                value: serde_json::json!("yes"),
            }),
        }];
        let mut inv = minimal_invocation(&def, "/tmp");
        // inputs.enabled = "no" → guard is false → op skipped.
        inv.inputs.insert("enabled".into(), serde_json::json!("no"));
        // UnavailableBackend would fail if called.
        let ctx = MacroPlannerContext::default();
        let plan = MacroPlanner::plan(&inv, &def, &ctx).expect("plan should succeed");
        assert_eq!(plan.operations.len(), 1);
        let op = &plan.operations[0];
        assert_eq!(op.kind, "rewrite");
        assert!(
            op.summary.contains("skipped (guard false)"),
            "op summary must say 'skipped (guard false)'; got: {}",
            op.summary
        );
        assert!(plan.edits.file_edits.is_empty());
    }

    /// When the `when` guard evaluates to `true`, the op proceeds normally.
    /// With UnavailableBackend this means an error propagates — which confirms
    /// the backend WAS actually called (not skipped).
    #[test]
    fn emit_with_true_guard_calls_backend() {
        use crate::expr::Predicate;
        let mut def = minimal_def();
        def.operations = vec![MacroOperation::Emit {
            name: "interface_file".into(),
            backend_op: json!({
                "op": "emit_type",
                "source_root": "/repo/src/main/java",
                "package": "com.example",
                "name": "Foo",
                "kind": "interface",
                "source_text": "package com.example;\npublic interface Foo {}"
            }),
            // Guard: inputs.enabled == "yes" → true → backend is called.
            when: Some(Predicate::Eq {
                path: "inputs.enabled".into(),
                value: serde_json::json!("yes"),
            }),
        }];
        let mut inv = minimal_invocation(&def, "/tmp");
        inv.inputs
            .insert("enabled".into(), serde_json::json!("yes"));
        // UnavailableBackend returns Err — proves the backend was reached.
        let ctx = MacroPlannerContext::default();
        let err = MacroPlanner::plan(&inv, &def, &ctx).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("error.backend_unavailable"),
            "backend should be called when guard is true; expected backend_unavailable, got: {msg}"
        );
    }
}
