// SPDX-FileCopyrightText: blackbox refactor track
// JSON-RPC protocol types matching src/refactor/csharp_sidecar_protocol.rs.
// Keep these in lockstep — the wire format is the contract.

using System.Text.Json.Serialization;

namespace Blackbox.CSharpWorker;

public sealed record RpcRequest(
    [property: JsonPropertyName("jsonrpc")] string Jsonrpc,
    [property: JsonPropertyName("id")] long Id,
    [property: JsonPropertyName("method")] string Method,
    [property: JsonPropertyName("params")] System.Text.Json.JsonElement? Params
);

public sealed record RpcResponse(
    [property: JsonPropertyName("jsonrpc")] string Jsonrpc,
    [property: JsonPropertyName("id")] long Id,
    [property: JsonPropertyName("result"), JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] object? Result,
    [property: JsonPropertyName("error"), JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] RpcError? Error
);

public sealed record RpcError(
    [property: JsonPropertyName("code")] int Code,
    [property: JsonPropertyName("message")] string Message,
    [property: JsonPropertyName("data"), JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] object? Data
);

public sealed record LoadParams(
    [property: JsonPropertyName("path")] string Path,
    [property: JsonPropertyName("reset")] bool Reset = true
);

public sealed record LoadedProject(
    [property: JsonPropertyName("name")] string Name,
    [property: JsonPropertyName("document_count")] int DocumentCount,
    [property: JsonPropertyName("file_path")] string? FilePath
);

public sealed record LoadResult(
    [property: JsonPropertyName("loaded_projects")] IReadOnlyList<LoadedProject> LoadedProjects,
    [property: JsonPropertyName("dropped_projects")] IReadOnlyList<string> DroppedProjects,
    [property: JsonPropertyName("workspace_warnings")] IReadOnlyList<string> WorkspaceWarnings
);

public sealed record LoadStatusResult(
    [property: JsonPropertyName("expected_projects")] IReadOnlyList<string> ExpectedProjects,
    [property: JsonPropertyName("loaded_projects")] IReadOnlyList<LoadedProject> LoadedProjects,
    [property: JsonPropertyName("dropped")] IReadOnlyList<string> Dropped,
    [property: JsonPropertyName("degraded")] IReadOnlyList<string> Degraded,
    [property: JsonPropertyName("warnings")] IReadOnlyList<string> Warnings
);

public sealed record GetDiagnosticsParams(
    [property: JsonPropertyName("file")] string? File,
    [property: JsonPropertyName("include_analyzers")] bool IncludeAnalyzers = false
);

public sealed record SidecarDiagnostic(
    [property: JsonPropertyName("code")] string Code,
    [property: JsonPropertyName("severity")] string Severity,
    [property: JsonPropertyName("message")] string Message,
    [property: JsonPropertyName("file")] string? File,
    [property: JsonPropertyName("line")] int Line,
    [property: JsonPropertyName("character")] int Character,
    [property: JsonPropertyName("end_line")] int EndLine,
    [property: JsonPropertyName("end_character")] int EndCharacter,
    [property: JsonPropertyName("origin")] string Origin
);

public sealed record GetDiagnosticsResult(
    [property: JsonPropertyName("diagnostics")] IReadOnlyList<SidecarDiagnostic> Diagnostics,
    [property: JsonPropertyName("included_analyzers")] bool IncludedAnalyzers
);

public sealed record PositionParams(
    [property: JsonPropertyName("file")] string File,
    [property: JsonPropertyName("line")] int Line,
    [property: JsonPropertyName("character")] int Character
);

public sealed record DiscoveredGenerator(
    [property: JsonPropertyName("name")] string Name,
    [property: JsonPropertyName("assembly_identity")] string AssemblyIdentity,
    [property: JsonPropertyName("source_path")] string? SourcePath,
    [property: JsonPropertyName("classification")] string Classification,
    [property: JsonPropertyName("attributes")] IReadOnlyList<string> Attributes,
    [property: JsonPropertyName("fingerprint")] string Fingerprint,
    [property: JsonPropertyName("source")] string Source
);

public sealed record EnumerateGeneratorsResult(
    [property: JsonPropertyName("generators")] IReadOnlyList<DiscoveredGenerator> Generators
);

public sealed record BeginTransactionParams(
    [property: JsonPropertyName("run_id")] string RunId
);

public sealed record SidecarPlanFileEdit(
    [property: JsonPropertyName("file")] string File,
    [property: JsonPropertyName("new_content")] string NewContent
);

public sealed record SidecarFileMove(
    [property: JsonPropertyName("source_path")] string SourcePath,
    [property: JsonPropertyName("target_path")] string TargetPath
);

public sealed record ApplyPlanStepParams(
    [property: JsonPropertyName("run_id")] string RunId,
    [property: JsonPropertyName("edits")] IReadOnlyList<SidecarPlanFileEdit> Edits,
    [property: JsonPropertyName("file_moves")] IReadOnlyList<SidecarFileMove> FileMoves,
    [property: JsonPropertyName("created")] IReadOnlyList<string> Created,
    [property: JsonPropertyName("deleted")] IReadOnlyList<string> Deleted
);

public sealed record ApplyCommandTouchesParams(
    [property: JsonPropertyName("run_id")] string RunId,
    [property: JsonPropertyName("touches")] IReadOnlyList<string> Touches,
    [property: JsonPropertyName("succeeded")] bool Succeeded
);

public sealed record TransactionResult(
    [property: JsonPropertyName("ok")] bool Ok,
    [property: JsonPropertyName("message")] string? Message
);

public sealed record UpdateDocumentTextParams(
    [property: JsonPropertyName("file")] string File,
    [property: JsonPropertyName("new_content")] string NewContent
);

public sealed record UpdateDocumentTextResult(
    [property: JsonPropertyName("updated")] bool Updated
);

public static class Methods
{
    public const string LoadSolution = "loadSolution";
    public const string LoadProject = "loadProject";
    public const string GetLoadStatus = "getLoadStatus";
    public const string GetDiagnostics = "getDiagnostics";
    public const string GetSymbol = "getSymbol";
    public const string FindReferences = "findReferences";
    public const string RenameSymbol = "renameSymbol";
    public const string GetOperations = "getOperations";
    public const string EnumerateGenerators = "enumerateGenerators";
    public const string BeginTransaction = "beginTransaction";
    public const string ApplyPlanStep = "applyPlanStep";
    public const string ApplyCommandTouches = "applyCommandTouches";
    public const string CommitTransaction = "commitTransaction";
    public const string RollbackTransaction = "rollbackTransaction";
    public const string UpdateDocumentText = "updateDocumentText";
    public const string Shutdown = "shutdown";
}
