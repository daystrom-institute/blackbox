//! `csharp_workspace_probe` — analysis-only workspace bootstrap probe.
//!
//! Phase 1 entry point. Parses a `.sln` or `.slnx` file (or a single
//! `.csproj`), enumerates the expected project set, classifies the
//! workspace shape, and reports whether MSBuildWorkspace will be able
//! to load it. Does not launch an LSP or sidecar — pure file
//! inspection. Other plan kinds consume its result before declaring
//! their own `lsp_verified` semantic status.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::RefactorPlanParams;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceShape {
    /// Classic `.sln` text file (enumeration via Project(...) lines).
    /// MSBuildWorkspace.OpenSolutionAsync is documented to load this.
    Sln,
    /// XML `.slnx` solution (SDK-style). MSBuildWorkspace support is
    /// version-dependent; the probe surfaces this so callers can pick
    /// the build-fallback path or open a single csproj instead.
    Slnx,
    /// Single `.csproj` workspace — `OpenProjectAsync` covers this.
    Csproj,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpectedProject {
    pub name: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceProbeReport {
    pub kind: String,
    pub source: String,
    pub shape: WorkspaceShape,
    pub expected_projects: Vec<ExpectedProject>,
    /// True when the workspace is `.slnx` and the operator should
    /// expect the documented MSBuildWorkspace constraint until the
    /// runtime verifies actual support. RX-V5 partial-load
    /// classification happens against the loaded set; this is just a
    /// pre-load heuristic.
    pub slnx_unverified_msbuild_workspace_support: bool,
    /// Quick sanity counts used by atom prompts to rank work.
    pub project_count: usize,
}

pub fn plan_workspace_probe(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let shape = classify(&source_path)?;
    let expected = match shape {
        WorkspaceShape::Sln => parse_sln(&source_path)?,
        WorkspaceShape::Slnx => parse_slnx(&source_path)?,
        WorkspaceShape::Csproj => vec![ExpectedProject {
            name: source_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("project")
                .to_string(),
            path: source_path
                .to_str()
                .ok_or_else(|| anyhow!("source path is not valid UTF-8"))?
                .to_string(),
        }],
    };
    let report = WorkspaceProbeReport {
        kind: "csharp_workspace_probe".to_string(),
        source: source_path
            .to_str()
            .ok_or_else(|| anyhow!("source path is not valid UTF-8"))?
            .to_string(),
        shape: shape.clone(),
        slnx_unverified_msbuild_workspace_support: matches!(shape, WorkspaceShape::Slnx),
        project_count: expected.len(),
        expected_projects: expected,
    };
    Ok(serde_json::to_string_pretty(&report)?)
}

fn resolve_path(project_dir: Option<&str>, source: &str) -> Result<PathBuf> {
    let candidate = PathBuf::from(source);
    if candidate.is_absolute() {
        return Ok(candidate);
    }
    let base = match project_dir {
        Some(p) => PathBuf::from(p),
        None => std::env::current_dir().context("getting current directory")?,
    };
    Ok(base.join(candidate))
}

fn classify(path: &Path) -> Result<WorkspaceShape> {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase());
    match ext.as_deref() {
        Some("sln") => Ok(WorkspaceShape::Sln),
        Some("slnx") => Ok(WorkspaceShape::Slnx),
        Some("csproj") => Ok(WorkspaceShape::Csproj),
        Some(other) => bail!(
            "error.unsupported_workspace_extension: `{}` (expected .sln, .slnx, or .csproj)",
            other
        ),
        None => bail!(
            "error.unsupported_workspace_extension: source has no extension; expected .sln/.slnx/.csproj"
        ),
    }
}

/// Parse a `.sln` file's `Project(...)` lines.
///
/// Format example: `Project("{guid}") = "Name", "Path/Name.csproj", "{guid}"`
fn parse_sln(path: &Path) -> Result<Vec<ExpectedProject>> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim_start();
        if !line.starts_with("Project(") {
            continue;
        }
        // Find the `= "name", "path",` segment.
        let Some(eq_pos) = line.find('=') else {
            continue;
        };
        let after_eq = line[eq_pos + 1..].trim();
        let parts = split_quoted_csv(after_eq);
        if parts.len() < 2 {
            continue;
        }
        let proj_path = parts[1].clone();
        if !proj_path.to_ascii_lowercase().ends_with(".csproj") {
            continue; // skip solution folders, shproj, etc.
        }
        out.push(ExpectedProject {
            name: parts[0].clone(),
            path: proj_path,
        });
    }
    Ok(out)
}

/// Parse a `.slnx` file's `<Project Path="..." />` entries.
///
/// Minimal text-shape parser — does not pull in an XML dep for v1. The
/// `.slnx` schema is small enough that a single-pass scan is robust:
/// `Path="..."` is the load-bearing attribute.
fn parse_slnx(path: &Path) -> Result<Vec<ExpectedProject>> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut out = Vec::new();
    let mut cursor = 0usize;
    while let Some(rel) = text[cursor..].find("<Project") {
        let start = cursor + rel;
        let Some(end_rel) = text[start..].find('>') else {
            break;
        };
        let tag = &text[start..start + end_rel + 1];
        cursor = start + end_rel + 1;
        let Some(path_attr) = extract_attr(tag, "Path") else {
            continue;
        };
        if !path_attr.to_ascii_lowercase().ends_with(".csproj") {
            continue;
        }
        let name = Path::new(&path_attr)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(&path_attr)
            .to_string();
        out.push(ExpectedProject {
            name,
            path: path_attr,
        });
    }
    Ok(out)
}

fn extract_attr(tag: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let pos = tag.find(&needle)?;
    let after = &tag[pos + needle.len()..];
    let end = after.find('"')?;
    Some(after[..end].to_string())
}

fn split_quoted_csv(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    for ch in s.chars() {
        match ch {
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                out.push(current.trim().to_string());
                current.clear();
            }
            _ if in_quotes => current.push(ch),
            _ => {} // ignore whitespace between fields
        }
    }
    if in_quotes || !current.is_empty() {
        out.push(current.trim().to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sln_extracts_csproj_entries() {
        let sln = r#"
Microsoft Visual Studio Solution File, Format Version 12.00
Project("{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}") = "Foo", "src/Foo/Foo.csproj", "{ABC}"
Project("{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}") = "Bar", "src\\Bar\\Bar.csproj", "{DEF}"
Project("{2150E333-8FDC-42A3-9474-1A3956D46DE8}") = "SolutionFolder", "SolutionFolder", "{GHI}"
"#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sln");
        std::fs::write(&path, sln).unwrap();
        let projects = parse_sln(&path).unwrap();
        assert_eq!(projects.len(), 2);
        assert_eq!(projects[0].name, "Foo");
        assert_eq!(projects[1].name, "Bar");
    }

    #[test]
    fn parse_slnx_extracts_path_attrs() {
        let slnx = r#"<Solution>
  <Project Path="src/Foo/Foo.csproj" />
  <Project Path="src/Bar/Bar.csproj" Type="Test" />
  <Project Path="docs/Notes.shproj" />
</Solution>"#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.slnx");
        std::fs::write(&path, slnx).unwrap();
        let projects = parse_slnx(&path).unwrap();
        assert_eq!(projects.len(), 2);
        assert_eq!(projects[0].name, "Foo");
        assert_eq!(projects[0].path, "src/Foo/Foo.csproj");
    }

    #[test]
    fn classify_rejects_unknown_extension() {
        let err = classify(Path::new("/tmp/unknown.txt")).unwrap_err();
        assert!(err.to_string().contains("unsupported_workspace_extension"));
    }
}
