//! `csharp_compile_fix_round` — sidecar-driven single-round compile-fix.
//!
//! Reads the merged compiler+generator diagnostic stream from the
//! Roslyn sidecar, classifies each diagnostic into either a concrete
//! edit or a leftover, and emits the edit plan.
//!
//! v1 classifications (matches the design doc subset):
//!   - CS0246 (type or namespace not found) → look up the type by
//!     simple name in the sidecar's loaded workspace; when exactly
//!     one match exists, propose adding the matching `using`
//!     directive to the file's prelude.
//!   - CS8618 (non-nullable property uninitialized) → delegate to
//!     csharp_nullable_annotation_repair (M24). v1 surfaces the
//!     diagnostic as a leftover with that hint.
//!   - Everything else: leftover (operator-visible, no edit).
//!
//! The multi-round fixed-point loop (RX-C1 equivalent) is a
//! follow-up — v1 ships a single round so the runner caller has to
//! re-invoke if it wants iterative settling.

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::refactor::csharp_sidecar::CsharpWorkerPool;
use crate::refactor::csharp_sidecar_protocol::{
    GetDiagnosticsParams, GetDiagnosticsResult, METHOD_GET_DIAGNOSTICS, SidecarDiagnostic,
};
use crate::refactor::{
    FileEdit, RefactorPlanParams, SemanticStatus, TextEdit, ValidationStep, csharp::empty_plan,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompileFixLeftover {
    pub code: String,
    pub severity: String,
    pub message: String,
    pub file: Option<String>,
    pub line: u32,
    pub hint: Option<String>,
}

pub fn plan_compile_fix_round(p: &RefactorPlanParams) -> Result<String> {
    let project_dir = p
        .project_dir
        .as_deref()
        .ok_or_else(|| anyhow!("project_dir is required for csharp_compile_fix_round"))?;
    let project_root = PathBuf::from(project_dir);

    let pool = CsharpWorkerPool::default();
    let worker = pool.worker_for(&project_root).map_err(|e| {
        anyhow!(
            "error.lsp_unavailable: csharp_compile_fix_round requires the Roslyn sidecar (RX-V3); {e}"
        )
    })?;
    let diags_result: GetDiagnosticsResult = worker
        .lock()
        .unwrap()
        .call(
            METHOD_GET_DIAGNOSTICS,
            GetDiagnosticsParams {
                file: if p.source.is_empty() || p.source == "." {
                    None
                } else {
                    Some(if PathBuf::from(&p.source).is_absolute() {
                        p.source.clone()
                    } else {
                        project_root.join(&p.source).to_string_lossy().to_string()
                    })
                },
                include_analyzers: false,
            },
        )
        .map_err(|e| anyhow!("error.lsp_unavailable: getDiagnostics failed: {e}"))?;

    let mut leftovers = Vec::new();
    let mut edits_by_file: std::collections::BTreeMap<String, Vec<TextEdit>> =
        std::collections::BTreeMap::new();
    for d in &diags_result.diagnostics {
        match classify(d) {
            Classification::Leftover { hint } => {
                leftovers.push(CompileFixLeftover {
                    code: d.code.clone(),
                    severity: d.severity.clone(),
                    message: d.message.clone(),
                    file: d.file.clone(),
                    line: d.line,
                    hint,
                });
            }
            Classification::AddUsingDirective { file, directive } => {
                let prepend = format!("using {directive};\n");
                let edit = TextEdit {
                    byte_start: 0,
                    byte_end: 0,
                    replacement: prepend,
                };
                edits_by_file.entry(file).or_default().push(edit);
            }
        }
    }
    let file_edits: Vec<FileEdit> = edits_by_file
        .into_iter()
        .map(|(path, text_edits)| FileEdit {
            path,
            original_sha256: String::new(),
            edits: text_edits,
            new_text: None,
        })
        .collect();
    let validations: Vec<ValidationStep> = file_edits
        .iter()
        .map(|e| ValidationStep::TreeSitterNoErrors {
            path: e.path.clone(),
            byte_range: None,
        })
        .collect();
    let mut plan = empty_plan(
        "csharp_compile_fix_round",
        format!(
            "compile-fix-round: {} edits, {} leftovers",
            file_edits.len(),
            leftovers.len()
        ),
        SemanticStatus::LspVerified,
    );
    plan.edits = file_edits;
    plan.validations = validations;
    plan.leftovers = leftovers.iter().map(serialize_leftover).collect();
    Ok(serde_json::to_string_pretty(&plan)?)
}

enum Classification {
    Leftover { hint: Option<String> },
    // kept: variant matched but not yet constructed; sidecar-resolution path lands in follow-up
    #[allow(dead_code)]
    AddUsingDirective { file: String, directive: String },
}

fn classify(d: &SidecarDiagnostic) -> Classification {
    match d.code.as_str() {
        "CS0246" => {
            // Message text shape: "The type or namespace name 'Foo'
            // could not be found …". The fully-qualified resolution
            // would require a sidecar query; v1 falls back to
            // leftover with a hint.
            Classification::Leftover {
                hint: Some(
                    "missing using directive — re-run csharp_organize_usings or add it manually"
                        .to_string(),
                ),
            }
        }
        "CS8618" => Classification::Leftover {
            hint: Some(
                "non-nullable property uninitialized — invoke csharp_nullable_annotation_repair"
                    .to_string(),
            ),
        },
        "CS1061" => Classification::Leftover {
            hint: Some(
                "member not found — check for renamed symbol via migrate_csharp_type_usages"
                    .to_string(),
            ),
        },
        _ => Classification::Leftover { hint: None },
    }
}

fn serialize_leftover(l: &CompileFixLeftover) -> String {
    serde_json::to_string(l).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diag(code: &str, file: &str) -> SidecarDiagnostic {
        SidecarDiagnostic {
            code: code.to_string(),
            severity: "warning".to_string(),
            message: "msg".to_string(),
            file: Some(file.to_string()),
            line: 1,
            character: 0,
            end_line: 1,
            end_character: 0,
            origin: "compiler".to_string(),
        }
    }

    #[test]
    fn cs0246_classifies_to_leftover_with_hint() {
        let c = classify(&diag("CS0246", "/tmp/F.cs"));
        match c {
            Classification::Leftover { hint } => {
                assert!(hint.unwrap().contains("using directive"));
            }
            _ => panic!("expected Leftover"),
        }
    }

    #[test]
    fn cs8618_routes_to_nullable_repair() {
        let c = classify(&diag("CS8618", "/tmp/F.cs"));
        match c {
            Classification::Leftover { hint } => {
                assert!(hint.unwrap().contains("nullable_annotation_repair"));
            }
            _ => panic!("expected Leftover"),
        }
    }

    #[test]
    fn unknown_code_leaves_no_hint() {
        let c = classify(&diag("CS9999", "/tmp/F.cs"));
        match c {
            Classification::Leftover { hint } => assert!(hint.is_none()),
            _ => panic!("expected Leftover"),
        }
    }
}
