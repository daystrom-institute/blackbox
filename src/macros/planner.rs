//! Macro planner (M3).
//!
//! Converts a [`MacroInvocation`] + registry-resolved [`MacroDefinition`] into
//! a [`MacroPlan`] review artifact, and lowers [`MacroPlan`] to a
//! [`RefactorPlan`] for [`crate::refactor::apply`].
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
//!    follows: any predicate or interpolation referencing a root that is neither
//!    `"inputs"` nor a declared probe name is a hard `error.unknown_context_root`.
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

use crate::macros::backend::{JavaEmitOp, JavaRewriteOp};
use crate::macros::expr;
use crate::macros::model::{
    EditSet, MacroDefinition, MacroInvocation, MacroOperation, MacroPlan, MacroPlanCheck,
    MacroPlanOperation, MacroRefusalHit, MacroSemanticStatus,
};
use crate::macros::planner_ctx::MacroPlannerContext;
use crate::macros::probe::ProbeSpec;
use crate::refactor::{
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
/// Both methods are pure (no I/O, no side effects on `self`).
pub struct MacroPlanner;

impl MacroPlanner {
    /// Plan a macro invocation.
    ///
    /// Returns a [`MacroPlan`] review artifact describing what the macro will do.
    /// Never writes files. Call [`MacroPlanner::lower`] on the result, then
    /// [`crate::refactor::apply`] to execute.
    ///
    /// # Errors
    ///
    /// - Version mismatch (`invocation.version` vs `def.version`)
    /// - Missing required inputs or type mismatch
    /// - Non-empty `def.probes` (Phase 4 gated)
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
        let mut expr_ctx = expr::Context {
            inputs: invocation.inputs.clone(),
            probes: HashMap::new(),
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
        // Probe provenance summaries for MacroPlan.provenance
        let mut probe_summaries: Vec<Value> = vec![];

        for probe in &def.probes {
            let spec: ProbeSpec =
                serde_json::from_value(probe.spec.clone()).with_context(|| {
                    format!(
                        "error.probe_spec_invalid: macro '{}' probe '{}' has an invalid or \
                         unknown spec kind. Spec: {}",
                        def.id, probe.name, probe.spec
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

        // ── Unknown-root validation ───────────────────────────────────────────
        //
        // Collect every declared probe name (from def.probes AND inline
        // MacroOperation::Probe entries). Any path root that is neither
        // "inputs" nor a declared probe name is a hard planning error.
        let allowed_roots: HashSet<String> = {
            let mut s: HashSet<String> = HashSet::new();
            s.insert("inputs".to_string());
            for p in &def.probes {
                s.insert(p.name.clone());
            }
            for op in &def.operations {
                if let MacroOperation::Probe { name, .. } = op {
                    s.insert(name.clone());
                }
            }
            s
        };
        validate_context_roots(def, &allowed_roots)?;

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
                    let spec: ProbeSpec =
                        serde_json::from_value(spec_val.clone()).with_context(|| {
                            format!(
                                "error.probe_spec_invalid: macro '{}' inline Probe operation \
                                 '{}' has an invalid spec: {}",
                                def.id, name, spec_val
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

                MacroOperation::Emit { name, .. } => {
                    // Constraint 6 (Emit): always call the backend and propagate its
                    // error. UnavailableBackend always returns error.backend_unavailable.
                    // We pass a dummy op because the backend_op Value is opaque at this
                    // layer and the UnavailableBackend ignores op contents.
                    let dummy_emit = JavaEmitOp::EmitType {
                        package: String::new(),
                        name: String::new(),
                        kind: "interface".to_string(),
                        source_text: String::new(),
                    };
                    let bes = ctx.backend.emit(&dummy_emit).with_context(|| {
                        format!(
                            "error.backend_unavailable: Emit operation '{}' in macro '{}' \
                             requires the Java macro backend (Phase 3); the backend is not connected",
                            name, def.id
                        )
                    })?;
                    backends_used.insert("java_poet".to_string());
                    for fc in &bes.file_creates {
                        register_path(&mut touched_paths, &fc.path)?;
                    }
                    for fe in &bes.file_edits {
                        register_path(&mut touched_paths, &fe.path)?;
                    }
                    edit_set.file_edits.extend(bes.file_edits);
                    edit_set.file_creates.extend(bes.file_creates);
                    // Backend-produced output: SyntaxOnly minimum when backend is live
                    op_statuses.push(MacroSemanticStatus::SyntaxOnly);
                    plan_ops.push(MacroPlanOperation {
                        kind: "emit".to_string(),
                        name: Some(name.clone()),
                        semantic_status: MacroSemanticStatus::SyntaxOnly,
                        summary: format!("Emit artifact '{}'", name),
                    });
                }

                MacroOperation::Rewrite { targets, .. } => {
                    let target_file = targets.first().cloned().unwrap_or_default();
                    let dummy_rewrite = JavaRewriteOp::InsertMember {
                        target_file,
                        target_type: String::new(),
                        member_text: String::new(),
                        imports: vec![],
                    };
                    let bes = ctx.backend.rewrite(&dummy_rewrite).with_context(|| {
                        format!(
                            "error.backend_unavailable: Rewrite operation in macro '{}' \
                             requires the Java macro backend (Phase 3); the backend is not connected",
                            def.id
                        )
                    })?;
                    backends_used.insert("open_rewrite".to_string());
                    for fe in &bes.file_edits {
                        register_path(&mut touched_paths, &fe.path)?;
                    }
                    for fc in &bes.file_creates {
                        register_path(&mut touched_paths, &fc.path)?;
                    }
                    edit_set.file_edits.extend(bes.file_edits);
                    edit_set.file_creates.extend(bes.file_creates);
                    op_statuses.push(MacroSemanticStatus::SyntaxOnly);
                    plan_ops.push(MacroPlanOperation {
                        kind: "rewrite".to_string(),
                        name: None,
                        semantic_status: MacroSemanticStatus::SyntaxOnly,
                        summary: format!("Rewrite {} target(s) via backend", targets.len()),
                    });
                }

                MacroOperation::DelegateRefactor {
                    refactor_kind,
                    params,
                } => {
                    // plan_delegate enforces RX-V1 authority boundary (see fn doc).
                    let (rp, consumed_flags) = plan_delegate(
                        refactor_kind,
                        params,
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

    /// Lower a [`MacroPlan`] to a [`RefactorPlan`] for [`crate::refactor::apply`].
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
            plan_status: crate::refactor::PlanStatus::Planned,
            fixme_count: None,
            operator_opt_outs_used: plan.operator_opt_outs_used.clone(),
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Private helpers
// ─────────────────────────────────────────────────────────────────────────────

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
        refactor::plan_with_ctx(&rpp, &plan_ctx).context("delegate_refactor plan failed")?;

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
        crate::refactor::PlanStatus::Blocked => {
            bail!(
                "error.delegate_plan_blocked: DelegateRefactor kind '{}' returned a Blocked \
                 plan (deep-analysis findings must be resolved before the macro can proceed). \
                 Title: {}",
                refactor_kind,
                rp.title
            );
        }
        crate::refactor::PlanStatus::Errored => {
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
        ProbeSpec::WorkspaceSymbol { .. } => "workspace_symbol",
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
fn collect_predicate_roots(pred: &crate::macros::expr::Predicate, out: &mut Vec<String>) {
    use crate::macros::expr::Predicate;
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
/// Record operation bodies are members of `allowed_roots`.
///
/// Any root that is neither `"inputs"` nor a declared probe name is a hard
/// planning error (`error.unknown_context_root`). Missing *nested* fields under
/// a valid root remain soft-false (existing [`expr::eval`] behaviour).
///
/// This check is performed after all probe names are known (static) but before
/// any predicate evaluation so typos surface immediately.
fn validate_context_roots(def: &MacroDefinition, allowed_roots: &HashSet<String>) -> Result<()> {
    let mut bad: Vec<String> = vec![];

    // Refusal predicates and messages
    for refusal in &def.refusals {
        let mut roots = vec![];
        collect_predicate_roots(&refusal.when, &mut roots);
        for r in roots {
            if !allowed_roots.contains(&r) {
                bad.push(format!(
                    "refusal '{}' predicate references unknown root '{}' \
                     (not 'inputs' or a declared probe name)",
                    refusal.code, r
                ));
            }
        }
        let mut msg_roots = vec![];
        collect_interpolation_roots(&refusal.message, &mut msg_roots);
        for r in msg_roots {
            if !allowed_roots.contains(&r) {
                bad.push(format!(
                    "refusal '{}' message interpolation references unknown root '{}' \
                     (not 'inputs' or a declared probe name)",
                    refusal.code, r
                ));
            }
        }
    }

    // Validation guards
    for val in &def.validations {
        if let Some(guard) = &val.when {
            let mut roots = vec![];
            collect_predicate_roots(guard, &mut roots);
            for r in roots {
                if !allowed_roots.contains(&r) {
                    bad.push(format!(
                        "validation guard predicate references unknown root '{}' \
                         (not 'inputs' or a declared probe name)",
                        r
                    ));
                }
            }
        }
    }

    // Record operation bodies
    for op in &def.operations {
        if let MacroOperation::Record { label, body } = op {
            let mut roots = vec![];
            collect_interpolation_roots(body, &mut roots);
            for r in roots {
                if !allowed_roots.contains(&r) {
                    bad.push(format!(
                        "Record operation '{}' body interpolation references unknown root '{}' \
                         (not 'inputs' or a declared probe name)",
                        label, r
                    ));
                }
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

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::macros::model::{MacroProbe, MacroRefusal, MacroScope};
    use crate::macros::planner_ctx::MacroPlannerContext;
    use crate::refactor;

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
            when: crate::macros::expr::Predicate::Exists {
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
            when: crate::macros::expr::Predicate::Exists {
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
            backend_op: json!({"backend": "java_poet", "class": "PaymentService"}),
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
            backend_op: json!({"recipe": "AddImport"}),
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
        let apply_result = refactor::apply(&apply_params, &projects).expect("apply should succeed");
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

    // ── Lowering: template_only refuses lowering ──────────────────────────────

    #[test]
    fn template_only_status_refuses_lowering() {
        // Manually construct a MacroPlan with a non-record op that has template_only status
        use crate::macros::model::MacroPlan;
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
                file_creates: vec![crate::refactor::FileCreate {
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
        use crate::macros::model::MacroPlan;
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
        use crate::refactor::{ExternalCall, ExtractedCallSite, PlanStatus};

        // Construct a RefactorPlan with leftovers + external_calls residue.
        // The dup-check and edit-merge paths in plan() are not exercised here —
        // we test surface_delegate_residue directly (same file, accessible via
        // `use super::*`).
        let rp = RefactorPlan {
            title: "test delegate plan".into(),
            kind: "extract_java_methods".into(),
            semantic_status: crate::refactor::SemanticStatus::SyntaxOnly,
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
            semantic_status: crate::refactor::SemanticStatus::SyntaxOnly,
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
            plan_status: crate::refactor::PlanStatus::Planned,
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
        canned: std::collections::HashMap<String, crate::macros::probe::ProbeOutput>,
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
                crate::macros::probe::ProbeOutput {
                    value,
                    semantic_status: status,
                    truncated: false,
                    diagnostics: vec![],
                },
            );
            self
        }
    }

    impl crate::macros::probe::ProbeRunner for MockProbeRunner {
        fn run_probe(
            &self,
            name: &str,
            _spec: &crate::macros::probe::ProbeSpec,
            _ctx: &crate::macros::expr::Context,
            _invocation: &crate::macros::model::MacroInvocation,
        ) -> anyhow::Result<crate::macros::probe::ProbeOutput> {
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
        use crate::macros::backend::UnavailableBackend;
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
            when: crate::macros::expr::Predicate::Exists {
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
            when: crate::macros::expr::Predicate::Exists {
                path: "binding.exists".into(),
            },
            code: "error.already_bound".into(),
            message: "already bound".into(),
        }];

        // binding.exists is false → Exists predicate is true only when path
        // resolves. Since binding.exists = false (bool), it DOES resolve.
        // Use a path that won't resolve: "binding.nonexistent"
        let mut def2 = def.clone();
        def2.refusals[0].when = crate::macros::expr::Predicate::Exists {
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
            when: crate::macros::expr::Predicate::All {
                predicates: vec![
                    crate::macros::expr::Predicate::Eq {
                        path: "probe_a.exists".into(),
                        value: json!(true),
                    },
                    crate::macros::expr::Predicate::Eq {
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
            when: crate::macros::expr::Predicate::Exists {
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
}

// ── Helpers exposed for tool-layer tests ────────────────────────────────────

/// Build a `RefactorApplyParams` for `macro_apply`.
///
/// Critical: `allow_dirty_worktree`, `allow_unregistered_paths`, and `force_path`
/// are all `None` (= false in `refactor::apply`). `macro_apply` is an envelope
/// over `refactor::apply` — it MUST NOT set bypass flags by default.
pub(crate) fn build_macro_apply_params(
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
