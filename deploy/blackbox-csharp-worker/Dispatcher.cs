// SPDX-FileCopyrightText: blackbox refactor track
// JSON-RPC dispatch routing — one line per request, one line per response.

using System.Text.Json;

namespace Blackbox.CSharpWorker;

public sealed class Dispatcher
{
    private readonly WorkspaceHost _host;
    private static readonly JsonSerializerOptions JsonOptions = new()
    {
        PropertyNamingPolicy = null, // we name every property with [JsonPropertyName]
        DefaultIgnoreCondition = System.Text.Json.Serialization.JsonIgnoreCondition.WhenWritingNull,
        WriteIndented = false,
    };

    public Dispatcher(WorkspaceHost host)
    {
        _host = host;
    }

    public async Task<RpcResponse> DispatchAsync(RpcRequest req, CancellationToken ct)
    {
        try
        {
            object? result = req.Method switch
            {
                Methods.LoadSolution => await HandleLoad(req, ct).ConfigureAwait(false),
                Methods.LoadProject => await HandleLoad(req, ct).ConfigureAwait(false),
                Methods.GetLoadStatus => _host.GetLoadStatus(),
                Methods.GetDiagnostics => await HandleGetDiagnostics(req, ct).ConfigureAwait(false),
                Methods.EnumerateGenerators => _host.EnumerateGenerators(),
                Methods.UpdateDocumentText => HandleUpdateDocumentText(req),
                Methods.BeginTransaction => HandleBeginTransaction(req),
                Methods.ApplyPlanStep => HandleApplyPlanStep(req),
                Methods.ApplyCommandTouches => HandleApplyCommandTouches(req),
                Methods.CommitTransaction => HandleCommitTransaction(req),
                Methods.RollbackTransaction => HandleRollbackTransaction(req),
                Methods.Shutdown => new TransactionResult(true, "shutting down"),
                _ => throw new InvalidOperationException($"unknown method `{req.Method}`")
            };
            return new RpcResponse("2.0", req.Id, result, null);
        }
        catch (Exception ex)
        {
            return new RpcResponse(
                "2.0",
                req.Id,
                null,
                new RpcError(-32000, ex.Message, ex.GetType().FullName)
            );
        }
    }

    private async Task<LoadResult> HandleLoad(RpcRequest req, CancellationToken ct)
    {
        var p = DeserializeParams<LoadParams>(req);
        return await _host.LoadAsync(p.Path, p.Reset, ct).ConfigureAwait(false);
    }

    private async Task<GetDiagnosticsResult> HandleGetDiagnostics(RpcRequest req, CancellationToken ct)
    {
        var p = req.Params.HasValue
            ? DeserializeParams<GetDiagnosticsParams>(req)
            : new GetDiagnosticsParams(null, false);
        return await _host.GetDiagnosticsAsync(p.File, p.IncludeAnalyzers, ct).ConfigureAwait(false);
    }

    private UpdateDocumentTextResult HandleUpdateDocumentText(RpcRequest req)
    {
        var p = DeserializeParams<UpdateDocumentTextParams>(req);
        return new UpdateDocumentTextResult(_host.UpdateDocumentText(p.File, p.NewContent));
    }

    private TransactionResult HandleBeginTransaction(RpcRequest req)
    {
        var p = DeserializeParams<BeginTransactionParams>(req);
        return _host.BeginTransaction(p.RunId);
    }

    private TransactionResult HandleApplyPlanStep(RpcRequest req)
    {
        var p = DeserializeParams<ApplyPlanStepParams>(req);
        return _host.ApplyPlanStep(p);
    }

    private TransactionResult HandleApplyCommandTouches(RpcRequest req)
    {
        var p = DeserializeParams<ApplyCommandTouchesParams>(req);
        return _host.ApplyCommandTouches(p);
    }

    private TransactionResult HandleCommitTransaction(RpcRequest req)
    {
        var p = DeserializeParams<BeginTransactionParams>(req);
        return _host.CommitTransaction(p.RunId);
    }

    private TransactionResult HandleRollbackTransaction(RpcRequest req)
    {
        var p = DeserializeParams<BeginTransactionParams>(req);
        return _host.RollbackTransaction(p.RunId);
    }

    private static T DeserializeParams<T>(RpcRequest req)
    {
        if (req.Params is null)
        {
            throw new ArgumentException($"method `{req.Method}` requires params");
        }
        return JsonSerializer.Deserialize<T>(req.Params.Value.GetRawText(), JsonOptions)
               ?? throw new ArgumentException($"failed to deserialize params for `{req.Method}`");
    }

    public static string SerializeResponse(RpcResponse response)
    {
        return JsonSerializer.Serialize(response, JsonOptions);
    }

    public static RpcRequest DeserializeRequest(string line)
    {
        return JsonSerializer.Deserialize<RpcRequest>(line, JsonOptions)
               ?? throw new InvalidOperationException("invalid JSON-RPC request");
    }
}
