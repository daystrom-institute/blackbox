//! `csharp_partial_class_audit` — RX-V4 driver, Phase 2 sidecar-backed.
//!
//! Pre-flight analysis for any structural refactor touching
//! `partial class` declarations or generator-keyed members. The
//! plan response is analysis-only — no edits — and the operator
//! consumes it before structural-move kinds.
//!
//! Inputs:
//!   - `project_dir` (required)
//!   - `source` (optional .sln / .slnx / .csproj path; defaults to
//!     auto-discover via csharp_workspace_probe)
//!
//! Output: JSON report with:
//!   - `expected_projects`, `loaded_projects`, `dropped`, `degraded`
//!     (RX-V5 workspace-load status)
//!   - `generators[]` — every discovered IIncrementalGenerator with
//!     name, assembly_identity, classification, fingerprint, source
//!   - `undetected_generators[]` — generators v1 cannot classify
//!     (classification != "attribute_metadata_name")
//!   - `unknown_external_generators[]` — package-shipped generators
//!     not in the curated registry (v1 has an empty registry; every
//!     analyzer-reference generator surfaces here until the operator
//!     declares them)
//!   - `manifest_path` — `.blackbox/csharp.json` if present, with
//!     declared generator_inputs entries
//!   - `partial_type_files[]` — files containing `partial`-modified
//!     class/record/struct/interface declarations
//!   - `semantic_status` — "lsp_verified" or "lsp_verified_partial"
//!     based on whether undetected_generators are covered by the
//!     operator manifest
//!
//! RX-V3 fail-closed: missing sidecar binary returns
//! `error.lsp_unavailable`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::refactor::RefactorPlanParams;
use crate::refactor::csharp_sidecar::CsharpWorkerPool;
use crate::refactor::csharp_sidecar_protocol::{
    DiscoveredGenerator, EnumerateGeneratorsResult, LoadParams, LoadResult, LoadStatusResult,
    METHOD_ENUMERATE_GENERATORS, METHOD_GET_LOAD_STATUS, METHOD_LOAD_SOLUTION,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneratorInputsManifest {
    #[serde(default)]
    pub generator_inputs: Vec<DeclaredGeneratorInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeclaredGeneratorInput {
    pub generator: String,
    pub fingerprint: String,
    #[serde(default)]
    pub attributes: Vec<String>,
    #[serde(default)]
    pub target_levels: Vec<String>,
    pub operator: Option<String>,
    pub rationale: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartialClassAuditReport {
    pub kind: String,
    pub project_dir: String,
    pub workspace_source: Option<String>,
    pub semantic_status: String,
    pub load_status: LoadStatusResult,
    pub generators: Vec<DiscoveredGenerator>,
    pub undetected_generators: Vec<DiscoveredGenerator>,
    pub unknown_external_generators: Vec<DiscoveredGenerator>,
    pub manifest_path: Option<String>,
    pub manifest_declared_count: usize,
    pub partial_type_files: Vec<String>,
    pub recommended_acknowledge_flag: Option<String>,
}

pub fn plan_partial_class_audit(p: &RefactorPlanParams) -> Result<String> {
    let project_dir = p
        .project_dir
        .as_deref()
        .ok_or_else(|| anyhow!("project_dir is required for csharp_partial_class_audit"))?;
    let project_root = PathBuf::from(project_dir);
    if !project_root.is_dir() {
        anyhow::bail!(
            "error.project_dir_not_a_directory: `{}`",
            project_root.display()
        );
    }
    let workspace_path = resolve_workspace_path(&project_root, p.source.as_str())?;
    let pool = CsharpWorkerPool::default();
    let worker_handle = pool.worker_for(&project_root).map_err(|e| {
        anyhow!(
            "error.lsp_unavailable: csharp_partial_class_audit requires the Roslyn sidecar (RX-V3); {e}"
        )
    })?;
    let load_result: LoadResult = worker_handle
        .lock()
        .unwrap()
        .call(
            METHOD_LOAD_SOLUTION,
            LoadParams {
                path: workspace_path
                    .to_str()
                    .ok_or_else(|| anyhow!("workspace path not valid UTF-8"))?
                    .to_string(),
                reset: true,
            },
        )
        .map_err(|e| anyhow!("error.lsp_unavailable: loadSolution failed: {e}"))?;
    let load_status: LoadStatusResult = worker_handle
        .lock()
        .unwrap()
        .call(METHOD_GET_LOAD_STATUS, ())
        .map_err(|e| anyhow!("error.lsp_unavailable: getLoadStatus failed: {e}"))?;
    let generators_result: EnumerateGeneratorsResult = worker_handle
        .lock()
        .unwrap()
        .call(METHOD_ENUMERATE_GENERATORS, ())
        .map_err(|e| anyhow!("error.lsp_unavailable: enumerateGenerators failed: {e}"))?;

    let manifest = load_manifest(&project_root)?;
    let manifest_lookup: HashMap<String, &DeclaredGeneratorInput> = manifest
        .as_ref()
        .map(|m| {
            m.generator_inputs
                .iter()
                .map(|d| (d.fingerprint.clone(), d))
                .collect()
        })
        .unwrap_or_default();

    let mut undetected = Vec::new();
    let mut unknown_external = Vec::new();
    for generator in &generators_result.generators {
        let declared = manifest_lookup.contains_key(&generator.fingerprint);
        let classifiable = generator.classification == "attribute_metadata_name";
        if classifiable && declared {
            continue;
        }
        if !classifiable && !declared {
            if generator.source == "package" {
                unknown_external.push(generator.clone());
            } else {
                undetected.push(generator.clone());
            }
        }
    }

    let semantic_status = if undetected.is_empty() && unknown_external.is_empty() {
        "lsp_verified".to_string()
    } else {
        "lsp_verified_partial".to_string()
    };
    let recommended_acknowledge_flag = if semantic_status == "lsp_verified_partial" {
        Some("acknowledge_generator_contract_change=true".to_string())
    } else {
        None
    };

    let partial_type_files = scan_partial_files(&project_root);

    let _ = load_result;
    let report = PartialClassAuditReport {
        kind: "csharp_partial_class_audit".to_string(),
        project_dir: project_dir.to_string(),
        workspace_source: Some(workspace_path.to_string_lossy().to_string()),
        semantic_status,
        load_status,
        generators: generators_result.generators.clone(),
        undetected_generators: undetected,
        unknown_external_generators: unknown_external,
        manifest_path: manifest.as_ref().map(|_| {
            project_root
                .join(".blackbox/csharp.json")
                .to_string_lossy()
                .to_string()
        }),
        manifest_declared_count: manifest_lookup.len(),
        partial_type_files,
        recommended_acknowledge_flag,
    };
    Ok(serde_json::to_string_pretty(&report)?)
}

fn resolve_workspace_path(project_root: &Path, source: &str) -> Result<PathBuf> {
    if !source.is_empty() && source != "." {
        let candidate = if PathBuf::from(source).is_absolute() {
            PathBuf::from(source)
        } else {
            project_root.join(source)
        };
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    // Auto-discover: prefer .slnx, then .sln, then any .csproj.
    let mut sln = None;
    let mut slnx = None;
    let mut csproj = None;
    for entry in std::fs::read_dir(project_root)
        .with_context(|| format!("reading {}", project_root.display()))?
        .flatten()
    {
        let path = entry.path();
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        match ext.to_ascii_lowercase().as_str() {
            "slnx" => slnx.get_or_insert(path),
            "sln" => sln.get_or_insert(path),
            "csproj" => csproj.get_or_insert(path),
            _ => continue,
        };
    }
    slnx.or(sln).or(csproj).ok_or_else(|| {
        anyhow!(
            "error.no_workspace_found: no .sln/.slnx/.csproj under {}",
            project_root.display()
        )
    })
}

fn load_manifest(project_root: &Path) -> Result<Option<GeneratorInputsManifest>> {
    let manifest_path = project_root.join(".blackbox/csharp.json");
    if !manifest_path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    let parsed: GeneratorInputsManifest = serde_json::from_str(&text)
        .with_context(|| format!("parsing {}", manifest_path.display()))?;
    Ok(Some(parsed))
}

fn scan_partial_files(project_root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for entry in WalkDir::new(project_root)
        .into_iter()
        .filter_entry(|e| !is_skipped_dir(e.path()))
        .filter_map(Result::ok)
    {
        let path = entry.path();
        if !is_csharp_source(path) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        if contains_partial_type(&text) {
            out.push(path.to_string_lossy().to_string());
        }
    }
    out
}

fn contains_partial_type(text: &str) -> bool {
    text.lines().any(|line| {
        let trimmed = line.trim_start();
        // Skip line comments + doc comments.
        if trimmed.starts_with("//") || trimmed.starts_with("/*") {
            return false;
        }
        let starts_partial = trimmed.starts_with("partial ")
            && (trimmed.starts_with("partial class ")
                || trimmed.starts_with("partial record ")
                || trimmed.starts_with("partial struct ")
                || trimmed.starts_with("partial interface "));
        if starts_partial {
            return true;
        }
        // Modifier-prefixed: `public partial class Foo`. Require the
        // `partial <keyword>` token sequence and ensure everything
        // before it is a recognized modifier (or empty).
        for kw in [
            " partial class ",
            " partial record ",
            " partial struct ",
            " partial interface ",
        ] {
            if let Some(pos) = trimmed.find(kw) {
                let prefix = trimmed[..pos].trim();
                if is_pure_modifier_prefix(prefix) {
                    return true;
                }
            }
        }
        false
    })
}

fn is_pure_modifier_prefix(prefix: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    let modifiers = [
        "public",
        "internal",
        "private",
        "protected",
        "static",
        "sealed",
        "abstract",
        "unsafe",
        "new",
        "ref",
    ];
    prefix
        .split_whitespace()
        .all(|tok| modifiers.contains(&tok))
}

fn is_skipped_dir(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    matches!(
        name,
        "bin" | "obj" | ".git" | ".worktrees" | "node_modules" | "target"
    )
}

fn is_csharp_source(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    if ext != "cs" {
        return false;
    }
    let name_lower = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    !(name_lower.ends_with(".g.cs") || name_lower.ends_with(".designer.cs"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contains_partial_type_matches_modifier_combinations() {
        assert!(contains_partial_type("public partial class Foo {}"));
        assert!(contains_partial_type("internal partial record Bar(int X);"));
        assert!(contains_partial_type("partial struct Baz {}"));
        assert!(!contains_partial_type("public class Foo {}"));
        assert!(!contains_partial_type("// partial class in a comment"));
    }

    #[test]
    fn scan_partial_files_finds_only_csharp_partials() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Plain.cs"), "public class Foo {}").unwrap();
        std::fs::write(dir.path().join("Partial.cs"), "public partial class Foo {}").unwrap();
        std::fs::write(dir.path().join("Skip.g.cs"), "public partial class Bar {}").unwrap();
        let files = scan_partial_files(dir.path());
        assert_eq!(files.len(), 1, "{files:?}");
        assert!(files[0].ends_with("Partial.cs"));
    }

    #[test]
    fn resolve_workspace_path_auto_discovers_slnx() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("My.slnx"), "<Solution/>").unwrap();
        let resolved = resolve_workspace_path(dir.path(), ".").unwrap();
        assert!(resolved.ends_with("My.slnx"));
    }

    #[test]
    fn load_manifest_parses_when_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".blackbox")).unwrap();
        std::fs::write(
            dir.path().join(".blackbox/csharp.json"),
            r#"{
                "generator_inputs": [
                    {
                        "generator": "MyGen",
                        "fingerprint": "sha256:abc",
                        "attributes": ["MyAttr"],
                        "target_levels": ["method"],
                        "operator": "alice",
                        "rationale": "test"
                    }
                ]
            }"#,
        )
        .unwrap();
        let manifest = load_manifest(dir.path()).unwrap().unwrap();
        assert_eq!(manifest.generator_inputs.len(), 1);
        assert_eq!(manifest.generator_inputs[0].fingerprint, "sha256:abc");
    }
}
