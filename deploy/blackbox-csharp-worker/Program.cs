// SPDX-FileCopyrightText: blackbox refactor track
//
// blackbox-csharp-worker — Roslyn sidecar for the C# refactor track.
//
// Speaks JSON-RPC 2.0 over stdio. One request per line (newline
// terminated); one response per line on stdout. Stderr is unstructured
// log output (the Rust LspSessionManager swallows it).
//
// Lifecycle:
//   - On launch: register MSBuildLocator so the workspace finds the
//     installed SDK.
//   - Loop: read line, parse RpcRequest, dispatch, write RpcResponse.
//   - On `shutdown`: respond OK then exit.
//   - On EOF: dispose workspace, exit.
//
// The sidecar does not initiate work — every operation is a request
// from the Rust client. There is no background indexing thread.

using Microsoft.Build.Locator;

namespace Blackbox.CSharpWorker;

public static class Program
{
    public static async Task<int> Main(string[] args)
    {
        try
        {
            MSBuildLocator.RegisterDefaults();
        }
        catch (Exception ex)
        {
            Console.Error.WriteLine($"[blackbox-csharp-worker] MSBuildLocator registration failed: {ex.Message}");
            // Continue anyway — most operations still work without
            // MSBuildWorkspace if the operator never calls
            // loadSolution.
        }

        await using var host = new WorkspaceHost();
        var dispatcher = new Dispatcher(host);

        Console.Error.WriteLine("[blackbox-csharp-worker] ready");
        // Flush so the Rust client sees the readiness banner before
        // any other output.
        await Console.Error.FlushAsync();

        using var stdin = new System.IO.StreamReader(Console.OpenStandardInput());
        await using var stdout = new System.IO.StreamWriter(Console.OpenStandardOutput()) { AutoFlush = true };

        while (true)
        {
            string? line = await stdin.ReadLineAsync();
            if (line is null) break; // EOF
            if (string.IsNullOrWhiteSpace(line)) continue;

            RpcRequest? request = null;
            try
            {
                request = Dispatcher.DeserializeRequest(line);
            }
            catch (Exception ex)
            {
                var err = new RpcResponse(
                    "2.0",
                    0,
                    null,
                    new RpcError(-32700, $"parse error: {ex.Message}", null)
                );
                await stdout.WriteLineAsync(Dispatcher.SerializeResponse(err));
                continue;
            }

            var response = await dispatcher.DispatchAsync(request, CancellationToken.None);
            await stdout.WriteLineAsync(Dispatcher.SerializeResponse(response));

            if (request.Method == Methods.Shutdown)
            {
                break;
            }
        }

        Console.Error.WriteLine("[blackbox-csharp-worker] exit");
        _ = args;
        return 0;
    }
}
