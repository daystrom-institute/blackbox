//! MCP tool surface for the macro layer.
//!
//! This module implements the `macro_*` tool namespace:
//! - `macro_list`, `macro_describe`, `macro_validate`, `macro_register`,
//!   `macro_unregister` — registry read/write surface (M2).
//! - `macro_plan`, `macro_apply`, `macro_run` — planner and apply surface (M3).

use std::path::PathBuf;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, IntoContents};
use rmcp::schemars;
use rmcp::{tool, tool_router};
use serde::Deserialize;
use serde_json::json;

use crate::macros::model::{MacroAnchors, MacroDefinition, MacroInvocation, MacroPlan};
use crate::macros::planner::{MacroPlanner, build_macro_apply_params};
use crate::macros::planner_ctx::MacroPlannerContext;
use crate::macros::probe::CodeNavProbeRunner;
use crate::macros::registry::{MacroRegistry, RegistryError};
use crate::macros::sidecar_backend::SidecarBackend;
use crate::refactor;
use crate::server::state::BlackboxServer;

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::macro_tools()
}

// ---------------------------------------------------------------------------
// Params structs
// ---------------------------------------------------------------------------

/// Parameters for `macro_list`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MacroListParams {
    /// Project root directory. When provided, project-scoped macros from
    /// `.bbox/macros/` are included in the merged result.
    #[serde(default)]
    pub project_dir: Option<String>,
}

/// Parameters for `macro_describe`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MacroDescribeParams {
    /// Stable identifier of the macro to describe.
    pub id: String,
    /// Optional version pin. Not yet consumed — the registry's version-aware
    /// lookup lands with the planner milestone (M3); accepted now so the tool
    /// schema is stable.
    #[serde(default)]
    #[allow(dead_code)]
    pub version: Option<String>,
    /// Project root directory. When provided, project-scoped macros are
    /// searched before user/builtin scopes.
    #[serde(default)]
    pub project_dir: Option<String>,
}

/// Parameters for `macro_validate`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MacroValidateParams {
    /// Inline macro definition to validate. Either this or `id` + `project_dir`
    /// must be provided.
    pub definition: Option<serde_json::Value>,
}

/// Parameters for `macro_register`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MacroRegisterParams {
    /// The full macro definition to register.
    pub definition: serde_json::Value,
    /// Project root directory where the macro will be written to
    /// `.bbox/macros/<id>.json`.
    pub project_dir: String,
    /// When `true`, silently overwrite an existing macro with the same `id`.
    #[serde(default)]
    pub overwrite: bool,
}

/// Parameters for `macro_unregister`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MacroUnregisterParams {
    /// Stable identifier of the macro to remove.
    pub id: String,
    /// Project root directory containing `.bbox/macros/<id>.json`.
    pub project_dir: String,
}

/// Parameters for `macro_plan`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MacroPlanParams {
    /// Stable identifier of the macro to plan.
    pub macro_id: String,
    /// Optional pinned version. When set, must exactly match the registered
    /// version (registry resolves by id only). Omit to accept whatever
    /// version is registered.
    #[serde(default)]
    pub version: Option<String>,
    /// Absolute path to the project root directory. Used to resolve
    /// project-scoped macros and to set `project_dir` on delegates.
    pub project_dir: String,
    /// Typed input values. Must conform to the macro's `inputs_schema`.
    #[serde(default)]
    pub inputs: serde_json::Map<String, serde_json::Value>,
    /// Optional source anchors (files, symbols, byte/line ranges).
    #[serde(default)]
    pub anchors: Option<MacroAnchors>,
    /// Operator-supplied authority opt-out flags (e.g.
    /// `"acknowledge_public_api_change"`). Agents MUST NOT default or infer
    /// these (RX-V1). Pass only when the operator explicitly provides them.
    #[serde(default)]
    pub operator_opt_outs: Option<Vec<String>>,
}

/// Parameters for `macro_apply`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MacroApplyParams {
    /// `MacroPlan` JSON returned by `macro_plan`. The plan is lowered to a
    /// `RefactorPlan` and applied to disk.
    pub plan: serde_json::Value,
    /// Must be `true` to execute. When omitted or `false`, returns an error.
    #[serde(default)]
    pub confirm: Option<bool>,
    /// Working directory for path resolution. Optional; when absent, `refactor::apply`
    /// uses its own cwd logic. Note: `MacroPlan` does not record a `project_dir` field,
    /// so there is no plan-recorded default to fall back to — pass the project directory
    /// explicitly when worktree isolation checks (G15) are needed.
    #[serde(default)]
    pub cwd: Option<String>,
}

/// Parameters for `macro_run`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MacroRunParams {
    /// Stable identifier of the macro to run.
    pub macro_id: String,
    /// Typed input values. Must conform to the macro's `inputs_schema`.
    #[serde(default)]
    pub inputs: serde_json::Map<String, serde_json::Value>,
    /// Absolute path to the project root directory.
    pub project_dir: String,
    /// Must be `true` to execute. When omitted or `false`, returns an error.
    #[serde(default)]
    pub confirm: Option<bool>,
    /// Optional pinned version. Must exactly match the registered version
    /// when provided.
    #[serde(default)]
    pub version: Option<String>,
    /// Operator-supplied authority opt-out flags (RX-V1). Pass only when the
    /// operator explicitly provides them.
    #[serde(default)]
    pub operator_opt_outs: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// Response helpers
// ---------------------------------------------------------------------------

/// Returns a `CallToolResult` with a JSON body. Wraps `Self::ok_json` for
/// ergonomics in the handlers below.
fn ok_json(value: &serde_json::Value) -> CallToolResult {
    CallToolResult::success(
        BlackboxServer::cap_response_text(&serde_json::to_string_pretty(value).unwrap_or_default())
            .into_contents(),
    )
}

fn err_text(msg: &str) -> CallToolResult {
    let mut r = CallToolResult::success(BlackboxServer::cap_response_text(msg).into_contents());
    r.is_error = Some(true);
    r
}

/// Resolve `project_dir` from an optional string: canonicalize if it exists,
/// otherwise return as-is (may be a path that hasn't been created yet).
fn resolve_project_dir(input: Option<&str>) -> Option<PathBuf> {
    let s = input?;
    if s.trim().is_empty() {
        return None;
    }
    let p = PathBuf::from(s);
    if p.is_absolute() && p.is_dir() {
        // Canonicalize to resolve symlinks (mirrors entity_ref behavior)
        return Some(p.canonicalize().unwrap_or(p));
    }
    Some(p)
}

// ---------------------------------------------------------------------------
// Tool router
// ---------------------------------------------------------------------------

#[tool_router(router = macro_tools)]
impl BlackboxServer {
    /// List macros from all scopes, merged and deduplicated by `id`.
    ///
    /// Returns a summary (id, version, scope, title, language, effects) for
    /// each macro, omitting the full definition bodies. Use `macro_describe`
    /// for the full definition of a specific macro.
    #[tool(
        name = "macro_list",
        description = "List macros from all scopes, merged and deduplicated by id. Returns id, version, scope, title, language, and effects for each macro. Use macro_describe for the full definition."
    )]
    pub(crate) async fn macro_list(
        &self,
        Parameters(p): Parameters<MacroListParams>,
    ) -> CallToolResult {
        Self::run_blocking("macro_list", move || {
            let project_dir = resolve_project_dir(p.project_dir.as_deref());
            let macros = MacroRegistry::list(project_dir.as_deref());

            let summaries: Vec<serde_json::Value> = macros
                .iter()
                .map(|m| {
                    json!({
                        "id": m.id,
                        "version": m.version,
                        "scope": m.scope,
                        "title": m.title,
                        "language": m.language,
                        "effects": m.effects,
                    })
                })
                .collect();

            Ok(serde_json::to_string_pretty(&json!({
                "macros": summaries,
                "count": summaries.len(),
            }))?)
        })
        .await
    }

    /// Describe a single macro by `id`: returns the full definition plus a
    /// human-readable summary of its operations, probes, and refusals.
    #[tool(
        name = "macro_describe",
        description = "Describe a macro by id: returns the full MacroDefinition plus a human-readable summary of operations, probes, and refusals."
    )]
    pub(crate) async fn macro_describe(
        &self,
        Parameters(p): Parameters<MacroDescribeParams>,
    ) -> CallToolResult {
        Self::run_blocking("macro_describe", move || {
            let project_dir = resolve_project_dir(p.project_dir.as_deref());

            let def = match MacroRegistry::get(project_dir.as_deref(), &p.id) {
                Ok(Some(d)) => d,
                Ok(None) => anyhow::bail!("macro '{id}' not found in any scope", id = p.id),
                Err(e) => anyhow::bail!("{e}"),
            };

            let op_kinds: Vec<&str> = def
                .operations
                .iter()
                .map(|op| match op {
                    crate::macros::model::MacroOperation::Probe { .. } => "probe",
                    crate::macros::model::MacroOperation::Emit { .. } => "emit",
                    crate::macros::model::MacroOperation::Rewrite { .. } => "rewrite",
                    crate::macros::model::MacroOperation::DelegateRefactor { .. } => {
                        "delegate_refactor"
                    }
                    crate::macros::model::MacroOperation::Validate { .. } => "validate",
                    crate::macros::model::MacroOperation::Record { .. } => "record",
                    crate::macros::model::MacroOperation::ForEach { .. } => "for_each",
                })
                .collect();

            let summary = json!({
                "id": def.id,
                "version": def.version,
                "language": def.language,
                "scope": def.scope,
                "title": def.title,
                "operation_count": def.operations.len(),
                "operation_kinds": op_kinds,
                "probe_count": def.probes.len(),
                "refusal_count": def.refusals.len(),
                "validation_count": def.validations.len(),
                "effects": def.effects,
                "authority_gates": def.authority_gates,
            });

            Ok(serde_json::to_string_pretty(&json!({
                "summary": summary,
                "definition": def,
            }))?)
        })
        .await
    }

    /// Validate a macro definition without registering it.
    ///
    /// Accepts an inline `definition` JSON object. Returns a validation report
    /// listing all issues (missing fields, bad predicates, etc.). Does NOT
    /// write anything.
    #[tool(
        name = "macro_validate",
        description = "Validate a macro definition without registering it. Accepts an inline definition JSON object. Returns a validation report listing all issues."
    )]
    pub(crate) fn macro_validate(
        &self,
        Parameters(p): Parameters<MacroValidateParams>,
    ) -> CallToolResult {
        let definition_value = match p.definition {
            Some(v) => v,
            None => {
                return err_text("`definition` field is required for validation");
            }
        };

        // Parse the Value into a MacroDefinition
        let def: MacroDefinition = match serde_json::from_value(definition_value) {
            Ok(d) => d,
            Err(e) => {
                return ok_json(&json!({
                    "valid": false,
                    "issues": [{
                        "severity": "error",
                        "field": "definition",
                        "message": format!("failed to parse as MacroDefinition: {e}")
                    }]
                }));
            }
        };

        let report = MacroRegistry::validate(&def);
        ok_json(&json!(report))
    }

    /// Register a macro in the project scope.
    ///
    /// Writes the definition to `<project_dir>/.bbox/macros/<id>.json`.
    /// Validates the definition before writing. When `overwrite` is `false`
    /// (default) and a macro with the same `id` already exists, returns a
    /// conflict error.
    #[tool(
        name = "macro_register",
        description = "Register a macro in the project scope. Writes to `<project_dir>/.bbox/macros/<id>.json`. Validates before writing. Refuses duplicate id+version unless overwrite=true."
    )]
    pub(crate) async fn macro_register(
        &self,
        Parameters(p): Parameters<MacroRegisterParams>,
    ) -> CallToolResult {
        Self::run_blocking("macro_register", move || {
            let project_dir = PathBuf::from(&p.project_dir);

            if !project_dir.is_dir() {
                anyhow::bail!(
                    "project_dir '{}' does not exist or is not a directory",
                    p.project_dir
                );
            }

            let def: MacroDefinition = serde_json::from_value(p.definition)
                .map_err(|e| anyhow::anyhow!("failed to parse definition: {e}"))?;

            // List-before-register: the registry handles conflict detection.
            // Capture the id before moving `def` so we can report it in the response.
            let def_id = def.id.clone();
            match MacroRegistry::register(&project_dir, def, p.overwrite) {
                Ok(()) => Ok(serde_json::to_string_pretty(&json!({
                    "registered": true,
                    "id": def_id,
                    "scope": "project",
                }))?),
                Err(e @ RegistryError::Conflict { .. }) => {
                    anyhow::bail!("{e} (set overwrite=true to replace)")
                }
                Err(e) => anyhow::bail!("{e}"),
            }
        })
        .await
    }

    /// Unregister (remove) a project-scope macro by `id`.
    ///
    /// Deletes `<project_dir>/.bbox/macros/<id>.json`. Does NOT affect
    /// user-scope or built-in macros with the same `id`.
    #[tool(
        name = "macro_unregister",
        description = "Remove a project-scope macro by id. Deletes `.bbox/macros/<id>.json` from the project directory. Does not affect user or builtin macros."
    )]
    pub(crate) async fn macro_unregister(
        &self,
        Parameters(p): Parameters<MacroUnregisterParams>,
    ) -> CallToolResult {
        Self::run_blocking("macro_unregister", move || {
            let project_dir = PathBuf::from(&p.project_dir);

            if !project_dir.is_dir() {
                anyhow::bail!(
                    "project_dir '{}' does not exist or is not a directory",
                    p.project_dir
                );
            }

            match MacroRegistry::unregister(&project_dir, &p.id) {
                Ok(()) => Ok(serde_json::to_string_pretty(&json!({
                    "unregistered": true,
                    "id": p.id,
                }))?),
                Err(e) => anyhow::bail!("{e}"),
            }
        })
        .await
    }

    /// Plan a macro invocation without writing anything to disk.
    ///
    /// Resolves the macro definition from the registry, validates inputs,
    /// evaluates refusals, and runs the planner pipeline. Returns a
    /// `MacroPlan` review artifact that can be inspected before applying.
    ///
    /// # READ-ONLY
    ///
    /// This tool never writes files. Use `macro_apply` or `macro_run` to
    /// execute the plan.
    #[tool(
        name = "macro_plan",
        description = "Plan a macro invocation: resolve the named macro, validate inputs, run the planner pipeline, and return a MacroPlan review artifact. Read-only — never writes files."
    )]
    pub(crate) async fn macro_plan(
        &self,
        Parameters(p): Parameters<MacroPlanParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("macro_plan", move || {
            let project_dir = resolve_project_dir(Some(&p.project_dir));

            let def = match MacroRegistry::get(project_dir.as_deref(), &p.macro_id) {
                Ok(Some(d)) => d,
                Ok(None) => {
                    anyhow::bail!(
                        "macro '{}' not found in any scope (project_dir={:?})",
                        p.macro_id,
                        p.project_dir
                    )
                }
                Err(e) => anyhow::bail!("{e}"),
            };

            // Build a planner context: sidecar backend (fails at call time when JAR
            // is absent — fail-closed per RX-V3) + LSP from daemon state.
            // P4b: wire the real CodeNavProbeRunner backed by lsp_sessions + project list.
            let probe_runner = {
                let projects = server.state.projects.read().list();
                CodeNavProbeRunner::new(Some(server.state.lsp_sessions.clone()), projects)
            };
            let backend = {
                let project_path = resolve_project_dir(Some(&p.project_dir))
                    .unwrap_or_else(|| std::path::PathBuf::from(&p.project_dir));
                SidecarBackend::new(project_path)
            };

            let invocation = MacroInvocation {
                macro_id: p.macro_id,
                version: p.version,
                project_dir: p.project_dir,
                inputs: p.inputs,
                anchors: p.anchors,
                operator_opt_outs: p.operator_opt_outs.unwrap_or_default(),
            };
            let ctx = MacroPlannerContext::new(
                Box::new(backend),
                Some(server.state.lsp_sessions.clone()),
                Box::new(probe_runner),
            );

            let plan = MacroPlanner::plan(&invocation, &def, &ctx)?;
            Ok(serde_json::to_string_pretty(&serde_json::to_value(&plan)?)?)
        })
        .await
    }

    /// Lower a `MacroPlan` to a `RefactorPlan` and apply it to disk.
    ///
    /// Accepts a `MacroPlan` JSON value (as returned by `macro_plan`). Lowers
    /// it to a `RefactorPlan` and delegates to `refactor::apply`. No bypass
    /// flags (`allow_dirty_worktree`, `allow_unregistered_paths`, `force_path`)
    /// are set — the caller must use `bbox_refactor_apply` directly if bypass
    /// flags are needed.
    ///
    /// Requires `confirm=true` to execute (mirrors `bbox_refactor_apply`).
    #[tool(
        name = "macro_apply",
        description = "Lower a MacroPlan to a RefactorPlan and apply it to disk. Wraps refactor::apply — no bypass flags are set by default. Requires confirm=true."
    )]
    pub(crate) async fn macro_apply(
        &self,
        Parameters(p): Parameters<MacroApplyParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("macro_apply", move || {
            let plan: MacroPlan = serde_json::from_value(p.plan).map_err(|e| {
                anyhow::anyhow!(
                    "failed to parse `plan` as MacroPlan: {e}. \
                     Use macro_plan to generate a valid MacroPlan JSON."
                )
            })?;

            if !plan.refusals.is_empty() {
                let codes: Vec<&str> = plan.refusals.iter().map(|r| r.code.as_str()).collect();
                anyhow::bail!(
                    "macro '{}' has unresolved refusals: {}. \
                     Resolve the refusal conditions before applying.",
                    plan.macro_id,
                    codes.join(", ")
                );
            }

            let refactor_plan = MacroPlanner::lower(&plan)?;

            let plan_value = serde_json::to_value(&refactor_plan)
                .map_err(|e| anyhow::anyhow!("failed to serialize RefactorPlan: {e}"))?;

            let apply_params = build_macro_apply_params(plan_value, p.confirm, p.cwd);
            let projects = server.state.projects.read().list();
            refactor::apply(&apply_params, &projects)
        })
        .await
    }

    /// Plan and apply a macro in a single step.
    ///
    /// Equivalent to calling `macro_plan` followed by `macro_apply`. The plan
    /// is returned alongside the apply result for audit purposes. Requires
    /// `confirm=true` to execute.
    ///
    /// Short-circuits and returns the plan (without applying) if the macro
    /// produces refusals.
    #[tool(
        name = "macro_run",
        description = "Plan and apply a macro in a single step. Equivalent to macro_plan followed by macro_apply. Requires confirm=true to execute."
    )]
    pub(crate) async fn macro_run(
        &self,
        Parameters(p): Parameters<MacroRunParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("macro_run", move || {
            let project_dir = resolve_project_dir(Some(&p.project_dir));

            let def = match MacroRegistry::get(project_dir.as_deref(), &p.macro_id) {
                Ok(Some(d)) => d,
                Ok(None) => {
                    anyhow::bail!(
                        "macro '{}' not found in any scope (project_dir={:?})",
                        p.macro_id,
                        p.project_dir
                    )
                }
                Err(e) => anyhow::bail!("{e}"),
            };

            let invocation = MacroInvocation {
                macro_id: p.macro_id,
                version: p.version,
                project_dir: p.project_dir.clone(),
                inputs: p.inputs,
                anchors: None,
                operator_opt_outs: p.operator_opt_outs.unwrap_or_default(),
            };

            // P4b: wire the real CodeNavProbeRunner backed by lsp_sessions + project list.
            let probe_runner = {
                let projects = server.state.projects.read().list();
                CodeNavProbeRunner::new(Some(server.state.lsp_sessions.clone()), projects)
            };
            let backend = {
                let project_path = resolve_project_dir(Some(&p.project_dir))
                    .unwrap_or_else(|| std::path::PathBuf::from(&p.project_dir));
                SidecarBackend::new(project_path)
            };
            let ctx = MacroPlannerContext::new(
                Box::new(backend),
                Some(server.state.lsp_sessions.clone()),
                Box::new(probe_runner),
            );

            let plan = MacroPlanner::plan(&invocation, &def, &ctx)?;

            if !plan.refusals.is_empty() {
                let codes: Vec<&str> = plan.refusals.iter().map(|r| r.code.as_str()).collect();
                let v = serde_json::to_value(&plan)?;
                return Ok(serde_json::to_string_pretty(&json!({
                    "status": "refused",
                    "refusals": codes,
                    "plan": v,
                }))?);
            }

            let refactor_plan = MacroPlanner::lower(&plan)?;

            let plan_value = serde_json::to_value(&refactor_plan)
                .map_err(|e| anyhow::anyhow!("failed to serialize RefactorPlan: {e}"))?;

            let apply_params =
                build_macro_apply_params(plan_value, p.confirm, Some(p.project_dir.clone()));

            // Capture a serialized copy of the plan for the response envelope
            let plan_json = serde_json::to_value(&plan).unwrap_or(serde_json::Value::Null);

            let projects = server.state.projects.read().list();
            let apply_result = refactor::apply(&apply_params, &projects)?;
            let envelope = json!({
                "plan": plan_json,
                "apply_result": apply_result,
            });
            serde_json::to_string_pretty(&envelope)
                .map_err(|e| anyhow::anyhow!("failed to serialize macro_run result: {e}"))
        })
        .await
    }
}
