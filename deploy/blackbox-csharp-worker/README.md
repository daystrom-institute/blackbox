# blackbox-csharp-worker

Roslyn sidecar for the blackbox C# refactor track. Long-lived stdio
process that holds an open `MSBuildWorkspace` and answers JSON-RPC
2.0 requests from the Rust client (`src/refactor/csharp_sidecar.rs`).

Built and managed by the Rust `LspSessionManager` (Phase 2 — see
`design/refactor-tools/csharp/refactor-csharp-expansion.md`).

## Build

```sh
dotnet publish -c Release -r linux-x64 --self-contained false \
    deploy/blackbox-csharp-worker/blackbox-csharp-worker.csproj
```

Sets `PublishSingleFile=true` so the output is one
`blackbox-csharp-worker` binary; `SelfContained=false` so it picks
up the operator's installed .NET 8 runtime instead of bundling
one. Path the binary at `$BLACKBOX_ROSLYN_WORKER_BIN`.

## Protocol

Each request is one JSON line:

```json
{"jsonrpc":"2.0","id":1,"method":"loadSolution","params":{"path":"/repo/My.sln"}}
```

Each response is the matching JSON-RPC 2.0 envelope on stdout. See
`Protocol.cs` for the typed shapes; the matching Rust types live at
`src/refactor/csharp_sidecar_protocol.rs`.

Supported methods:

| method                | purpose                                                          |
|-----------------------|------------------------------------------------------------------|
| `loadSolution`        | Open a `.sln` / `.slnx` via MSBuildWorkspace.                    |
| `loadProject`         | Open a single `.csproj`.                                         |
| `getLoadStatus`       | RX-V5 expected-vs-loaded comparison + workspace warnings.        |
| `getDiagnostics`      | Compiler ∪ generator (∪ analyzer opt-in) diagnostics.             |
| `enumerateGenerators` | RX-V4 generator discovery (analyzer-reference scan).             |
| `updateDocumentText`  | In-memory `Solution.WithDocumentText` for dry-run preflight.     |
| `beginTransaction`    | Snapshot the immutable `Solution` reference.                     |
| `applyPlanStep`       | Mirror edits + file_moves + created + deleted into the snapshot. |
| `applyCommandTouches` | Re-read declared touches from disk into the snapshot.            |
| `commitTransaction`   | Drop the snapshot.                                               |
| `rollbackTransaction` | Restore the snapshot.                                            |
| `shutdown`            | Respond OK + exit.                                               |

## RX-V4 / RX-V5 caveats

- `enumerateGenerators` currently scans only `AnalyzerReferences`.
  In-repo `*.SourceGenerators/` syntax inspection (which would
  classify generators as `attribute_metadata_name`,
  `raw_syntax_provider`, or `register_post_initialization`) is a
  follow-up. The Rust side already treats `classification:
  "unknown"` as fail-closed under RX-V4, so the v1 sidecar's
  conservative output is safe.
- `getLoadStatus`'s expected list comes from a minimal `.sln`/`.slnx`
  parser. Roslyn's own resolution may pull in `<ProjectReference>`d
  projects beyond the solution declaration; those count as loaded
  but not expected, which is benign.

## Smoke testing

A working environment needs the .NET 8 SDK installed. Quick check:

```sh
echo '{"jsonrpc":"2.0","id":1,"method":"shutdown"}' | \
    dotnet run --project deploy/blackbox-csharp-worker
```

Should print `{"jsonrpc":"2.0","id":1,"result":{"ok":true,"message":"shutting down"}}`.
