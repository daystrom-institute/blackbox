//! MCP tool surface for the macro registry.
//!
//! This module implements the read/registration half of the `macro_*` tool
//! namespace: `macro_list`, `macro_describe`, `macro_validate`,
//! `macro_register`, and `macro_unregister`.
//!
//! Planning (`macro_plan`), apply (`macro_apply`), and runner (`macro_run`)
//! land in the next milestone — do NOT add stubs for them here.

use std::path::PathBuf;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, IntoContents};
use rmcp::schemars;
use rmcp::{tool, tool_router};
use serde::Deserialize;
use serde_json::json;

use crate::macros::model::MacroDefinition;
use crate::macros::registry::{MacroRegistry, RegistryError};
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

fn registry_err(e: RegistryError) -> CallToolResult {
    err_text(&format!("registry error: {e}"))
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
    pub(crate) fn macro_list(&self, Parameters(p): Parameters<MacroListParams>) -> CallToolResult {
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

        ok_json(&json!({
            "macros": summaries,
            "count": summaries.len(),
        }))
    }

    /// Describe a single macro by `id`: returns the full definition plus a
    /// human-readable summary of its operations, probes, and refusals.
    #[tool(
        name = "macro_describe",
        description = "Describe a macro by id: returns the full MacroDefinition plus a human-readable summary of operations, probes, and refusals."
    )]
    pub(crate) fn macro_describe(
        &self,
        Parameters(p): Parameters<MacroDescribeParams>,
    ) -> CallToolResult {
        let project_dir = resolve_project_dir(p.project_dir.as_deref());

        let def = match MacroRegistry::get(project_dir.as_deref(), &p.id) {
            Ok(Some(d)) => d,
            Ok(None) => {
                return err_text(&format!("macro '{id}' not found in any scope", id = p.id));
            }
            Err(e) => return registry_err(e),
        };

        // Build a human summary
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

        ok_json(&json!({
            "summary": summary,
            "definition": def,
        }))
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
    pub(crate) fn macro_register(
        &self,
        Parameters(p): Parameters<MacroRegisterParams>,
    ) -> CallToolResult {
        let project_dir = PathBuf::from(&p.project_dir);

        if !project_dir.is_dir() {
            return err_text(&format!(
                "project_dir '{}' does not exist or is not a directory",
                p.project_dir
            ));
        }

        // Parse the definition
        let def: MacroDefinition = match serde_json::from_value(p.definition) {
            Ok(d) => d,
            Err(e) => {
                return err_text(&format!("failed to parse definition: {e}"));
            }
        };

        // List-before-register: the registry handles conflict detection.
        // Capture the id before moving `def` so we can report it in the response.
        let def_id = def.id.clone();
        match MacroRegistry::register(&project_dir, def, p.overwrite) {
            Ok(()) => ok_json(&json!({
                "registered": true,
                "id": def_id,
                "scope": "project",
            })),
            Err(e @ RegistryError::Conflict { .. }) => {
                // user-friendly conflict message
                err_text(&format!("{e} (set overwrite=true to replace)"))
            }
            Err(e) => registry_err(e),
        }
    }

    /// Unregister (remove) a project-scope macro by `id`.
    ///
    /// Deletes `<project_dir>/.bbox/macros/<id>.json`. Does NOT affect
    /// user-scope or built-in macros with the same `id`.
    #[tool(
        name = "macro_unregister",
        description = "Remove a project-scope macro by id. Deletes `.bbox/macros/<id>.json` from the project directory. Does not affect user or builtin macros."
    )]
    pub(crate) fn macro_unregister(
        &self,
        Parameters(p): Parameters<MacroUnregisterParams>,
    ) -> CallToolResult {
        let project_dir = PathBuf::from(&p.project_dir);

        if !project_dir.is_dir() {
            return err_text(&format!(
                "project_dir '{}' does not exist or is not a directory",
                p.project_dir
            ));
        }

        match MacroRegistry::unregister(&project_dir, &p.id) {
            Ok(()) => ok_json(&json!({
                "unregistered": true,
                "id": p.id,
            })),
            Err(e) => registry_err(e),
        }
    }
}
