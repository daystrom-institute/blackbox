//! JSON-RPC protocol types for the Roslyn sidecar
//! (`blackbox-csharp-worker`).
//!
//! The sidecar is a small .NET process that holds an open
//! `MSBuildWorkspace` for one solution/project and answers requests
//! over stdio. The protocol uses JSON-RPC 2.0 framing — each request
//! is a JSON object on its own line, each response is the matching
//! JSON object with the same `id`. The dotnet implementation lives
//! at `deploy/blackbox-csharp-worker/` (M18); this module is the
//! Rust client's view of the wire format.
//!
//! Method names are camelCase to match LSP idiom and the dotnet
//! defaults. Per-message shapes match the design doc
//! (`design/refactor-tools/csharp/refactor-csharp-expansion.md`,
//! "Sidecar architecture (Phase 2)").
//!
//! This module is **types only** — the actual transport / spawn /
//! lifecycle is in `src/refactor/csharp_sidecar.rs` (M21).

#![allow(dead_code)]

use serde::{Deserialize, Serialize};

// ── JSON-RPC envelope ────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRequest<P> {
    pub jsonrpc: String,
    pub id: u64,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<P>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcResponse<R> {
    pub jsonrpc: String,
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<R>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

// ── loadSolution / loadProject ────────────────────────────────────

/// `loadSolution(path)` / `loadProject(path)` request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadParams {
    pub path: String,
    /// When true, the sidecar resets any previously loaded workspace
    /// before opening this one. Default true.
    #[serde(default = "default_true")]
    pub reset: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadResult {
    /// Projects MSBuildWorkspace actually loaded.
    pub loaded_projects: Vec<LoadedProject>,
    /// Projects from the solution file that did not load. Drives
    /// RX-V5 fail-closed.
    pub dropped_projects: Vec<String>,
    /// Per-project warnings from WorkspaceFailedHandler.
    pub workspace_warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadedProject {
    pub name: String,
    pub document_count: u32,
    pub file_path: Option<String>,
}

// ── getLoadStatus ─────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadStatusResult {
    pub expected_projects: Vec<String>,
    pub loaded_projects: Vec<LoadedProject>,
    pub dropped: Vec<String>,
    pub degraded: Vec<String>,
    pub warnings: Vec<String>,
}

// ── getDiagnostics ────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetDiagnosticsParams {
    /// When set, scope to a single file. When `None`, return
    /// solution-wide diagnostics (more expensive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// When true, also run `CompilationWithAnalyzers` to include
    /// analyzer-emitted diagnostics (Roslynator, IDisposableAnalyzers,
    /// custom DAY*). Default false to keep latency low.
    #[serde(default)]
    pub include_analyzers: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetDiagnosticsResult {
    /// Merged stream per the design doc: compiler diagnostics
    /// (`Compilation.GetDiagnostics`) UNIONed with
    /// generator-reported diagnostics
    /// (`GeneratorDriverRunResult.Diagnostics`), optionally with
    /// analyzer rules. Sidecar performs the merge; the client never
    /// sees the raw streams separately.
    pub diagnostics: Vec<SidecarDiagnostic>,
    /// Whether analyzer rules were actually included (sidecar may
    /// downgrade if `CompilationWithAnalyzers` failed).
    pub included_analyzers: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidecarDiagnostic {
    /// Roslyn diagnostic id (e.g. "CS8618", "IDISP004", "DAY0001").
    pub code: String,
    /// "error" | "warning" | "info" | "hidden".
    pub severity: String,
    pub message: String,
    pub file: Option<String>,
    pub line: u32,
    pub character: u32,
    pub end_line: u32,
    pub end_character: u32,
    /// "compiler" | "generator" | "analyzer" — which stream
    /// produced this diagnostic. Lets clients differentiate
    /// generator-reported issues from compiler-on-generated-syntax.
    pub origin: String,
}

// ── Symbol surfaces ───────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PositionParams {
    pub file: String,
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolInfo {
    pub qualified_name: String,
    pub kind: String,
    pub declaration_file: Option<String>,
    pub declaration_line: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FindReferencesParams {
    pub file: String,
    pub line: u32,
    pub character: u32,
    #[serde(default = "default_true")]
    pub include_declaration: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReferenceLocation {
    pub file: String,
    pub line: u32,
    pub character: u32,
    pub end_line: u32,
    pub end_character: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FindReferencesResult {
    pub references: Vec<ReferenceLocation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenameSymbolParams {
    pub file: String,
    pub line: u32,
    pub character: u32,
    pub new_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenameSymbolResult {
    /// File → list of text edits. Same shape as LSP `WorkspaceEdit`
    /// per-file, but simpler since the sidecar resolves to plain
    /// byte ranges for the Rust client.
    pub file_edits: Vec<SidecarFileEdit>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidecarFileEdit {
    pub file: String,
    pub edits: Vec<SidecarTextEdit>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidecarTextEdit {
    pub start_line: u32,
    pub start_character: u32,
    pub end_line: u32,
    pub end_character: u32,
    pub new_text: String,
}

// ── IOperation tree (for awaited-query-in-loop audit) ─────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetOperationsParams {
    pub file: String,
    /// Simple-name match — e.g. `"DoWork"`. Sidecar walks the
    /// SyntaxTree for the method declaration.
    pub method_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetOperationsResult {
    pub operations: Vec<OperationNode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationNode {
    /// Roslyn `IOperation` kind name, e.g. "Invocation",
    /// "Await", "ForEachLoop", "PropertyReference".
    pub kind: String,
    pub line: u32,
    pub character: u32,
    pub end_line: u32,
    pub end_character: u32,
    /// For invocations: containing type + method full name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_full_name: Option<String>,
    /// Nested operations.
    #[serde(default)]
    pub children: Vec<OperationNode>,
}

// ── Generator enumeration (RX-V4) ────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnumerateGeneratorsResult {
    pub generators: Vec<DiscoveredGenerator>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredGenerator {
    pub name: String,
    pub assembly_identity: String,
    pub source_path: Option<String>,
    /// "attribute_metadata_name" | "raw_syntax_provider" |
    /// "register_post_initialization".
    pub classification: String,
    /// Attribute fully-qualified names the generator binds against,
    /// when the classification is `attribute_metadata_name`.
    #[serde(default)]
    pub attributes: Vec<String>,
    pub fingerprint: String,
    /// "in_repo" | "analyzer_reference" | "package".
    pub source: String,
}

// ── Transaction surface ──────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BeginTransactionParams {
    pub run_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyPlanStepParams {
    pub run_id: String,
    pub edits: Vec<SidecarPlanFileEdit>,
    pub file_moves: Vec<SidecarFileMove>,
    pub created: Vec<String>,
    pub deleted: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidecarPlanFileEdit {
    /// Path on disk.
    pub file: String,
    /// New file content after applying the edits — sidecar uses this
    /// for Solution.WithDocumentText. Cheaper than re-reading from
    /// disk, and the runner already has the post-apply bytes in
    /// hand.
    pub new_content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidecarFileMove {
    pub source_path: String,
    pub target_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyCommandTouchesParams {
    pub run_id: String,
    pub touches: Vec<String>,
    pub succeeded: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitTransactionParams {
    pub run_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollbackTransactionParams {
    pub run_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionResult {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

// ── Update document (for sidecar diagnostic preflight) ────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateDocumentTextParams {
    pub file: String,
    pub new_content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateDocumentTextResult {
    pub updated: bool,
}

// ── Standard method names ────────────────────────────────────────

pub const METHOD_LOAD_SOLUTION: &str = "loadSolution";
pub const METHOD_LOAD_PROJECT: &str = "loadProject";
pub const METHOD_GET_LOAD_STATUS: &str = "getLoadStatus";
pub const METHOD_GET_DIAGNOSTICS: &str = "getDiagnostics";
pub const METHOD_GET_SYMBOL: &str = "getSymbol";
pub const METHOD_FIND_REFERENCES: &str = "findReferences";
pub const METHOD_RENAME_SYMBOL: &str = "renameSymbol";
pub const METHOD_GET_OPERATIONS: &str = "getOperations";
pub const METHOD_ENUMERATE_GENERATORS: &str = "enumerateGenerators";
pub const METHOD_BEGIN_TRANSACTION: &str = "beginTransaction";
pub const METHOD_APPLY_PLAN_STEP: &str = "applyPlanStep";
pub const METHOD_APPLY_COMMAND_TOUCHES: &str = "applyCommandTouches";
pub const METHOD_COMMIT_TRANSACTION: &str = "commitTransaction";
pub const METHOD_ROLLBACK_TRANSACTION: &str = "rollbackTransaction";
pub const METHOD_UPDATE_DOCUMENT_TEXT: &str = "updateDocumentText";
pub const METHOD_SHUTDOWN: &str = "shutdown";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpc_request_serializes_camel_case_method() {
        let req = RpcRequest::<LoadParams> {
            jsonrpc: "2.0".to_string(),
            id: 7,
            method: METHOD_LOAD_SOLUTION.to_string(),
            params: Some(LoadParams {
                path: "/tmp/x.sln".to_string(),
                reset: true,
            }),
        };
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"method\":\"loadSolution\""), "{s}");
        assert!(s.contains("\"path\":\"/tmp/x.sln\""), "{s}");
    }

    #[test]
    fn load_result_round_trips() {
        let result = LoadResult {
            loaded_projects: vec![LoadedProject {
                name: "Foo".to_string(),
                document_count: 12,
                file_path: Some("/tmp/Foo.csproj".to_string()),
            }],
            dropped_projects: vec!["Bar".to_string()],
            workspace_warnings: vec!["msbuild target import failed".to_string()],
        };
        let s = serde_json::to_string(&result).unwrap();
        let back: LoadResult = serde_json::from_str(&s).unwrap();
        assert_eq!(back.loaded_projects.len(), 1);
        assert_eq!(back.dropped_projects, vec!["Bar".to_string()]);
    }

    #[test]
    fn diagnostic_origin_is_required() {
        let s = r#"{
            "code": "CS8618",
            "severity": "warning",
            "message": "non-nullable property uninitialized",
            "file": "/tmp/Foo.cs",
            "line": 5,
            "character": 16,
            "end_line": 5,
            "end_character": 30,
            "origin": "compiler"
        }"#;
        let d: SidecarDiagnostic = serde_json::from_str(s).unwrap();
        assert_eq!(d.code, "CS8618");
        assert_eq!(d.origin, "compiler");
    }

    #[test]
    fn discovered_generator_classifies_three_sources() {
        let s = r#"{
            "name": "Wolverine.HandlerGenerator",
            "assembly_identity": "WolverineFx, Version=5.31.0.0, Culture=neutral",
            "source_path": null,
            "classification": "raw_syntax_provider",
            "attributes": [],
            "fingerprint": "sha256:abcdef0123",
            "source": "package"
        }"#;
        let g: DiscoveredGenerator = serde_json::from_str(s).unwrap();
        assert_eq!(g.classification, "raw_syntax_provider");
        assert_eq!(g.source, "package");
    }

    #[test]
    fn apply_plan_step_params_carry_moves_and_creations() {
        let params = ApplyPlanStepParams {
            run_id: "abc123".to_string(),
            edits: vec![SidecarPlanFileEdit {
                file: "/tmp/Foo.cs".to_string(),
                new_content: "public class Foo {}\n".to_string(),
            }],
            file_moves: vec![SidecarFileMove {
                source_path: "/tmp/old.cs".to_string(),
                target_path: "/tmp/new.cs".to_string(),
            }],
            created: vec!["/tmp/new.cs".to_string()],
            deleted: vec!["/tmp/old.cs".to_string()],
        };
        let s = serde_json::to_string(&params).unwrap();
        let back: ApplyPlanStepParams = serde_json::from_str(&s).unwrap();
        assert_eq!(back.file_moves.len(), 1);
        assert_eq!(back.created, vec!["/tmp/new.cs".to_string()]);
    }
}
