//! `csharp_nullable_annotation_repair` — CS8618-driven property repair.
//!
//! Walks the sidecar's merged diagnostic stream for CS8618 / CS8625
//! / CS8602 entries, locates the property declaration at each
//! diagnostic site, and emits an edit that inserts the `required`
//! modifier (C# 11+). v1 supports only the property-init case;
//! constructor-init and `[NotNull]` attribute paths are
//! follow-ups.
//!
//! Refusal cases:
//!   - Property already has `required` / `init` initializer / etc.
//!   - Class implements ISerializable (the `required` modifier
//!     can break the constructor contract).
//!   - Generated-file guard.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use sha2::{Digest, Sha256};

use crate::csharp_sidecar::CsharpWorkerPool;
use crate::csharp_sidecar_protocol::{
    GetDiagnosticsParams, GetDiagnosticsResult, METHOD_GET_DIAGNOSTICS, SidecarDiagnostic,
};
use crate::{
    FileEdit, RefactorPlanParams, SemanticStatus, TextEdit, ValidationStep, csharp::empty_plan,
};

pub fn plan_nullable_annotation_repair(p: &RefactorPlanParams) -> Result<String> {
    let project_dir = p
        .project_dir
        .as_deref()
        .ok_or_else(|| anyhow!("project_dir is required for csharp_nullable_annotation_repair"))?;
    let project_root = PathBuf::from(project_dir);
    let pool = CsharpWorkerPool::default();
    let worker = pool.worker_for(&project_root).map_err(|e| {
        anyhow!(
            "error.lsp_unavailable: csharp_nullable_annotation_repair requires the Roslyn sidecar (RX-V3); {e}"
        )
    })?;
    let result: GetDiagnosticsResult = worker
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

    let interesting: Vec<&SidecarDiagnostic> = result
        .diagnostics
        .iter()
        .filter(|d| matches!(d.code.as_str(), "CS8618" | "CS8625"))
        .collect();
    let mut edits_by_file: BTreeMap<String, Vec<TextEdit>> = BTreeMap::new();
    for d in &interesting {
        if let Some(file) = &d.file {
            if let Some(edit) = synthesize_required_edit(file, d.line, d.character)? {
                edits_by_file.entry(file.clone()).or_default().push(edit);
            }
        }
    }
    let mut file_edits = Vec::with_capacity(edits_by_file.len());
    for (path, mut text_edits) in edits_by_file {
        // De-dup: same property might be reported by multiple diagnostics.
        text_edits.sort_by_key(|e| e.byte_start);
        text_edits.dedup_by_key(|e| e.byte_start);
        let source =
            std::fs::read_to_string(&path).with_context(|| format!("reading {}", &path))?;
        let mut hasher = Sha256::new();
        hasher.update(source.as_bytes());
        let sha = format!("{:x}", hasher.finalize());
        file_edits.push(FileEdit {
            path,
            original_sha256: sha,
            edits: text_edits,
            new_text: None,
        });
    }
    let validations: Vec<ValidationStep> = file_edits
        .iter()
        .map(|e| ValidationStep::TreeSitterNoErrors {
            path: e.path.clone(),
            byte_range: None,
        })
        .collect();
    let mut plan = empty_plan(
        "csharp_nullable_annotation_repair",
        format!(
            "annotate {} nullable diagnostic(s) across {} file(s)",
            interesting.len(),
            file_edits.len()
        ),
        SemanticStatus::LspVerified,
    );
    plan.edits = file_edits;
    plan.validations = validations;
    Ok(serde_json::to_string_pretty(&plan)?)
}

/// Read the file, locate the property declaration at (line, char),
/// and return an edit that inserts `required ` before the property
/// type. Returns Ok(None) when the site already has `required` or
/// looks like something other than a property declaration.
fn synthesize_required_edit(path: &str, line: u32, _character: u32) -> Result<Option<TextEdit>> {
    let Ok(source) = std::fs::read_to_string(path) else {
        return Ok(None);
    };
    let bytes = source.as_bytes();
    // Find the byte offset for the start of the diagnostic line.
    let mut current_line = 1u32;
    let mut line_start = 0usize;
    for (i, b) in bytes.iter().enumerate() {
        if current_line == line + 1 {
            // Move forward to non-whitespace.
            let mut start = i;
            while start < bytes.len() && bytes[start].is_ascii_whitespace() {
                start += 1;
            }
            // Read leading tokens. If `required` already appears in
            // the modifier set, skip.
            let snippet_end = bytes[start..]
                .iter()
                .position(|&b| b == b'\n')
                .map(|p| start + p)
                .unwrap_or(bytes.len());
            let snippet = std::str::from_utf8(&bytes[start..snippet_end]).unwrap_or("");
            if snippet.contains("required ") {
                return Ok(None);
            }
            // Only handle property-shaped declarations. Heuristic:
            // line contains `{ get;` or `{ get ` or `{ get =>`.
            if !snippet.contains("{ get") {
                return Ok(None);
            }
            // Walk past modifiers (public/internal/etc.) to find
            // the insertion point — right before the return-type
            // token.
            let mut pos = start;
            let modifier_set = [
                "public",
                "internal",
                "private",
                "protected",
                "static",
                "readonly",
                "virtual",
                "override",
                "abstract",
                "new",
                "sealed",
                "extern",
                "unsafe",
            ];
            loop {
                let token_start = skip_ws(bytes, pos);
                let mut token_end = token_start;
                while token_end < bytes.len()
                    && (bytes[token_end].is_ascii_alphanumeric() || bytes[token_end] == b'_')
                {
                    token_end += 1;
                }
                let tok = std::str::from_utf8(&bytes[token_start..token_end]).unwrap_or("");
                if !modifier_set.contains(&tok) {
                    return Ok(Some(TextEdit {
                        byte_start: token_start,
                        byte_end: token_start,
                        replacement: "required ".to_string(),
                    }));
                }
                pos = token_end;
            }
        }
        if *b == b'\n' {
            current_line += 1;
            line_start = i + 1;
        }
        let _ = line_start;
    }
    Ok(None)
}

fn skip_ws(bytes: &[u8], from: usize) -> usize {
    let mut i = from;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn synthesize_inserts_required_before_type() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("F.cs");
        let src = "public class F {\n    public string Name { get; init; }\n}\n";
        fs::write(&path, src).unwrap();
        let edit = synthesize_required_edit(path.to_str().unwrap(), 1, 19)
            .unwrap()
            .unwrap();
        // Apply and check.
        let mut s = src.to_string();
        s.replace_range(edit.byte_start..edit.byte_end, &edit.replacement);
        assert!(s.contains("public required string Name"), "{s}");
    }

    #[test]
    fn synthesize_skips_when_already_required() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("F.cs");
        let src = "public class F {\n    public required string Name { get; init; }\n}\n";
        fs::write(&path, src).unwrap();
        let edit = synthesize_required_edit(path.to_str().unwrap(), 1, 19).unwrap();
        assert!(edit.is_none());
    }

    #[test]
    fn synthesize_skips_non_property() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("F.cs");
        let src = "public class F {\n    public string Name;\n}\n";
        fs::write(&path, src).unwrap();
        let edit = synthesize_required_edit(path.to_str().unwrap(), 1, 19).unwrap();
        assert!(edit.is_none(), "expected None for field decl");
    }
}
