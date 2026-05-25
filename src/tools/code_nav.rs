use std::path::PathBuf;

use anyhow::{anyhow, Context};
use crate::code_nav::{
    CodeNodeDescribeParams, CodeQueryParams, CodeRefsParams, CodeSymbolSearchParams,
    code_node_describe, code_query, code_refs, code_symbols,
};
use crate::code_nav::semantic::{
    java_document_outline, java_find_implementations, java_find_usages, java_type_at,
    java_workspace_symbols, resolve_path_for_usages, resolve_project_dir_for_usages,
};
use crate::server::BlackboxServer;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use rmcp::schemars;
use serde::Deserialize;

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::code_nav_tools()
}

/// Parameters for `bbox_code_usages`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CodeUsagesParams {
    /// Source file containing the symbol. Absolute, or relative to
    /// `project_dir` when that is supplied.
    pub file: String,
    /// 1-based line number of the symbol position (same convention as
    /// `bbox_code_node_describe`). Converted to 0-based internally before
    /// the LSP request is issued.
    pub line: u32,
    /// 1-based UTF-16 character offset of the symbol position (same
    /// convention as `bbox_code_node_describe`). Converted to 0-based
    /// internally before the LSP request is issued.
    pub column: u32,
    /// Project root used to spawn the JDTLS session and resolve
    /// relative `file` paths. When omitted, the git root above
    /// `file` is used (or the file's parent directory as a fallback).
    #[serde(default)]
    pub project_dir: Option<String>,
}

/// Parameters for `bbox_code_implementations`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CodeImplementationsParams {
    /// Source file containing the symbol. Absolute, or relative to
    /// `project_dir` when that is supplied.
    pub file: String,
    /// 1-based line number of the symbol position (same convention as
    /// `bbox_code_node_describe`). Converted to 0-based internally before
    /// the LSP request is issued.
    pub line: u32,
    /// 1-based UTF-16 character offset of the symbol position (same
    /// convention as `bbox_code_node_describe`). Converted to 0-based
    /// internally before the LSP request is issued.
    pub column: u32,
    /// Project root used to spawn the JDTLS session and resolve
    /// relative `file` paths. When omitted, the git root above
    /// `file` is used (or the file's parent directory as a fallback).
    #[serde(default)]
    pub project_dir: Option<String>,
}

/// Parameters for `bbox_code_type_at`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CodeTypeAtParams {
    /// Source file containing the symbol. Absolute, or relative to
    /// `project_dir` when that is supplied.
    pub file: String,
    /// 1-based line number of the symbol position (same convention as
    /// `bbox_code_node_describe`). Converted to 0-based internally before
    /// the LSP request is issued.
    pub line: u32,
    /// 1-based UTF-16 character offset of the symbol position (same
    /// convention as `bbox_code_node_describe`). Converted to 0-based
    /// internally before the LSP request is issued.
    pub column: u32,
    /// Project root used to spawn the JDTLS session and resolve
    /// relative `file` paths. When omitted, the git root above
    /// `file` is used (or the file's parent directory as a fallback).
    #[serde(default)]
    pub project_dir: Option<String>,
}

/// Parameters for `bbox_workspace_symbols`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceSymbolsParams {
    /// Query string to search for symbols (e.g., class name, method name).
    pub query: String,
    /// Project root directory. Required — workspace/symbol is project-wide.
    /// When omitted, the current working directory is used as the project root.
    #[serde(default)]
    pub project_dir: Option<String>,
}

/// Parameters for `bbox_code_outline`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CodeOutlineParams {
    /// Source file to outline. Absolute, or relative to `project_dir`.
    pub file: String,
    /// Project root used to spawn the JDTLS session and resolve
    /// relative `file` paths. When omitted, the git root above
    /// `file` is used (or the file's parent directory as a fallback).
    #[serde(default)]
    pub project_dir: Option<String>,
}

#[tool_router(router = code_nav_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_code_query",
        description = "Run a tree-sitter query against one source file. Return syntactic matches plus handoff hints for refactor/status grounding."
    )]
    pub(crate) fn bbox_code_query(
        &self,
        Parameters(p): Parameters<CodeQueryParams>,
    ) -> CallToolResult {
        Self::run("bbox_code_query", || code_query(&p))
    }

    #[tool(
        name = "bbox_code_symbols",
        description = "Find refactorable syntax symbols across a project and return exact line ranges plus refactor/project-ref handoff hints."
    )]
    pub(crate) fn bbox_code_symbols(
        &self,
        Parameters(p): Parameters<CodeSymbolSearchParams>,
    ) -> CallToolResult {
        Self::run("bbox_code_symbols", || {
            let projects = self.state.projects.read().list();
            let idx = self.state.idx.read();
            code_symbols(&p, &projects, Some(&*idx))
        })
    }

    #[tool(
        name = "bbox_code_node_describe",
        description = "Describe the smallest named AST node at a source position and suggest the next refactor/status grounding call."
    )]
    pub(crate) fn bbox_code_node_describe(
        &self,
        Parameters(p): Parameters<CodeNodeDescribeParams>,
    ) -> CallToolResult {
        Self::run("bbox_code_node_describe", || code_node_describe(&p))
    }

    #[tool(
        name = "bbox_code_refs",
        description = "Extract syntactic references (calls, imports, fields, identifiers) from one source file. Per-language tree-sitter queries; identifiers fallback for unsupported languages. Records are syntax-only with edge_confidence=\"heuristic\"."
    )]
    pub(crate) fn bbox_code_refs(
        &self,
        Parameters(p): Parameters<CodeRefsParams>,
    ) -> CallToolResult {
        Self::run("bbox_code_refs", || code_refs(&p))
    }

    #[tool(
        name = "bbox_code_usages",
        description = "Resolve binding-aware usages of the Java symbol at a file position via JDTLS (LSP textDocument/references). Returns semantically-verified usage sites with semantic_status=\"lsp_verified\". Fail-closed: returns error.lsp_unavailable when JDTLS is absent or fails to initialise (RX-V3). Java only; use bbox_code_refs for syntax-only cross-language reference extraction."
    )]
    pub(crate) fn bbox_code_usages(
        &self,
        Parameters(p): Parameters<CodeUsagesParams>,
    ) -> CallToolResult {
        let lsp = self.state.lsp_sessions.clone();
        Self::run("bbox_code_usages", || {
            // Gate: Java only for now. Other languages → typed unsupported error.
            let source_path = resolve_path_for_usages(p.project_dir.as_deref(), &p.file)?;
            let ext = source_path
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            if ext != "java" {
                let resp = serde_json::json!({
                    "status": "error",
                    "code": "unsupported_language",
                    "message": format!(
                        "bbox_code_usages currently supports Java (.java) only; got extension {:?}",
                        ext
                    ),
                    "suggestion": "Use bbox_code_refs for syntax-only reference extraction across other languages.",
                    "semantic_status": "syntax_only"
                });
                return Ok(serde_json::to_string_pretty(&resp)?);
            }

            let project_dir =
                resolve_project_dir_for_usages(p.project_dir.as_deref(), &source_path);
            // Convert 1-based MCP coordinates → 0-based LSP coordinates,
            // mirroring code_node_describe's `p.line.saturating_sub(1)`.
            let lsp_line = p.line.saturating_sub(1);
            let lsp_col = p.column.saturating_sub(1);
            let report = java_find_usages(&lsp, &project_dir, &source_path, lsp_line, lsp_col)?;
            Ok(serde_json::to_string_pretty(&report)?)
        })
    }

    #[tool(
        name = "bbox_code_implementations",
        description = "Resolve implementations of the Java symbol at a file position via JDTLS (LSP textDocument/implementation). Returns semantically-verified implementor sites with semantic_status=\"lsp_verified\". Fail-closed: returns error.lsp_unavailable when JDTLS is absent or fails to initialise (RX-V3). Java only."
    )]
    pub(crate) fn bbox_code_implementations(
        &self,
        Parameters(p): Parameters<CodeImplementationsParams>,
    ) -> CallToolResult {
        let lsp = self.state.lsp_sessions.clone();
        Self::run("bbox_code_implementations", || {
            let source_path = resolve_path_for_usages(p.project_dir.as_deref(), &p.file)?;
            let ext = source_path
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            if ext != "java" {
                let resp = serde_json::json!({
                    "status": "error",
                    "code": "unsupported_language",
                    "message": format!(
                        "bbox_code_implementations currently supports Java (.java) only; got extension {:?}",
                        ext
                    ),
                    "suggestion": "Use bbox_code_refs for syntax-only reference extraction across other languages.",
                    "semantic_status": "syntax_only"
                });
                return Ok(serde_json::to_string_pretty(&resp)?);
            }

            let project_dir =
                resolve_project_dir_for_usages(p.project_dir.as_deref(), &source_path);
            let lsp_line = p.line.saturating_sub(1);
            let lsp_col = p.column.saturating_sub(1);
            let report =
                java_find_implementations(&lsp, &project_dir, &source_path, lsp_line, lsp_col)?;
            Ok(serde_json::to_string_pretty(&report)?)
        })
    }

    #[tool(
        name = "bbox_code_type_at",
        description = "Resolve the type/signature/documentation at a Java position via JDTLS (LSP textDocument/hover). Returns resolved type info with semantic_status=\"lsp_verified\". Fail-closed: returns error.lsp_unavailable when JDTLS is absent or fails to initialise (RX-V3). Java only."
    )]
    pub(crate) fn bbox_code_type_at(
        &self,
        Parameters(p): Parameters<CodeTypeAtParams>,
    ) -> CallToolResult {
        let lsp = self.state.lsp_sessions.clone();
        Self::run("bbox_code_type_at", || {
            let source_path = resolve_path_for_usages(p.project_dir.as_deref(), &p.file)?;
            let ext = source_path
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            if ext != "java" {
                let resp = serde_json::json!({
                    "status": "error",
                    "code": "unsupported_language",
                    "message": format!(
                        "bbox_code_type_at currently supports Java (.java) only; got extension {:?}",
                        ext
                    ),
                    "suggestion": "Use bbox_code_node_describe for syntax-only type information across other languages.",
                    "semantic_status": "syntax_only"
                });
                return Ok(serde_json::to_string_pretty(&resp)?);
            }

            let project_dir =
                resolve_project_dir_for_usages(p.project_dir.as_deref(), &source_path);
            let lsp_line = p.line.saturating_sub(1);
            let lsp_col = p.column.saturating_sub(1);
            let report = java_type_at(&lsp, &project_dir, &source_path, lsp_line, lsp_col)?;
            Ok(serde_json::to_string_pretty(&report)?)
        })
    }

    #[tool(
        name = "bbox_workspace_symbols",
        description = "Resolve workspace symbols matching a query via JDTLS (LSP workspace/symbol). Returns semantically-verified workspace symbol matches with semantic_status=\"lsp_verified\". Fail-closed: returns error.lsp_unavailable when JDTLS is absent or fails to initialise (RX-V3). Java only; project-wide query."
    )]
    pub(crate) fn bbox_workspace_symbols(
        &self,
        Parameters(p): Parameters<WorkspaceSymbolsParams>,
    ) -> CallToolResult {
        let lsp = self.state.lsp_sessions.clone();
        Self::run("bbox_workspace_symbols", || {
            // Resolve project_dir: if supplied use it, otherwise use cwd
            let project_dir: PathBuf = match p.project_dir {
                Some(ref dir) => PathBuf::from(dir),
                None => std::env::current_dir().map_err(|e| anyhow!(e))?,
            };

            let canonical = project_dir
                .canonicalize()
                .with_context(|| format!("failed to canonicalize {}", project_dir.display()))?;

            let report = java_workspace_symbols(&lsp, &canonical, &p.query)?;
            Ok(serde_json::to_string_pretty(&report)?)
        })
    }

    #[tool(
        name = "bbox_code_outline",
        description = "Return a file-scoped, hierarchical symbol outline for a Java source file via JDTLS (LSP textDocument/documentSymbol). Returns a recursive tree of OutlineSymbol nodes with semantic_status=\"lsp_verified\". Fail-closed: returns error.lsp_unavailable when JDTLS is absent or fails to initialise (RX-V3). Java only; no position anchor required — the whole file is outlined."
    )]
    pub(crate) fn bbox_code_outline(
        &self,
        Parameters(p): Parameters<CodeOutlineParams>,
    ) -> CallToolResult {
        let lsp = self.state.lsp_sessions.clone();
        Self::run("bbox_code_outline", || {
            let source_path = resolve_path_for_usages(p.project_dir.as_deref(), &p.file)?;
            let ext = source_path
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            if ext != "java" {
                let resp = serde_json::json!({
                    "status": "error",
                    "code": "unsupported_language",
                    "message": format!(
                        "bbox_code_outline currently supports Java (.java) only; got extension {:?}",
                        ext
                    ),
                    "suggestion": "Use bbox_code_symbols or bbox_refactor_status for syntax-only symbol extraction across other languages.",
                    "semantic_status": "syntax_only"
                });
                return Ok(serde_json::to_string_pretty(&resp)?);
            }

            let project_dir =
                resolve_project_dir_for_usages(p.project_dir.as_deref(), &source_path);
            let report = java_document_outline(&lsp, &project_dir, &source_path)?;
            Ok(serde_json::to_string_pretty(&report)?)
        })
    }
}
