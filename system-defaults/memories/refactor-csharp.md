---
tags:
  - refactor-tools
  - csharp
---
+++
title = "C# refactor mechanization — Roslyn-sidecar plan kinds, LSP-backed assists, and dotnet validation workflow"
tags = ["refactor", "refactoring", "mechanization", "restructure", "csharp", "c#", "cs", "roslyn", "msbuild", "tree-sitter", "bbox_refactor_status", "bbox_refactor_plan", "bbox_refactor_apply", "bbox_refactor_run", "csharp_lsp_rename", "csharp_workspace_probe", "csharp_partial_class_audit", "csharp_compile_fix_round", "csharp_nullable_annotation_repair", "csharp_to_record_migrate", "csharp_primary_ctor_migrate", "csharp_async_dispose_convert", "unseal_csharp_class", "move_csharp_members_to_partial", "dotnet"]
order = 10
template = false
+++
# C# Refactor Mechanization Runbook

Use this memory before moving, extracting, renaming, or migrating C# code with
blackbox refactor tools. The design surface is
`design/refactor-tools/csharp/refactor-csharp-expansion.md`.

## Atom signposts

For recurring C# refactor patterns, check `atom_search(query="<intent>")`
before re-deriving the whole tool sequence. The active catalog lives in
the installable manifests; use atoms as contextual shortcuts:

- `csharp-using-organize` — single-file Roslyn `source.organizeImports`.
- `csharp-filescoped-batch` — block-scoped → file-scoped namespace
  conversion; idempotent, single-namespace files only.
- `csharp-unseal-strangler` — strangler-fig unseal of ONE class; requires
  operator-set `acknowledge_subclass_surface_change=true`.
- `csharp-partial-sg-guard` — RX-V4 analysis: enumerate
  `IIncrementalGenerator` implementations (in-repo + AnalyzerReference +
  package), fingerprint generator inputs, surface protected partial sets.
- `csharp-awaited-query-audit` — IOperation walk: classify awaited calls
  inside `for` / `foreach` bodies as `per_iteration_await` (N+1 risk) vs
  `loop_collection_await` (benign single-call).
- `csharp-nullable-coverage-fix` — single-round CS8618 / CS8625 repair:
  insert `required` on offending property declarations.

Atom manifests bind through `brofile:csharp-refactor-persona@v1`. The
persona allow list mirrors the Rust and Java personas (refactor +
grounding tool surface only); `Bash`/`Write`/`Edit` are denied, so
toolchain commands flow exclusively through `bbox_refactor_run` command
steps.

## Plan kind catalog (v1)

`bbox_refactor_plan(kind="…")` dispatches the following C# kinds (see
`src/refactor/csharp/mod.rs` for the registry):

| Plan kind | Semantic tier | Notes |
|-----------|---------------|-------|
| `csharp_workspace_probe` | syntax_only | Expected-vs-loaded probe for `.sln`/`.slnx`. Analysis-only. |
| `migrate_csharp_to_filescoped_namespace` | syntax_only | Block-to-file-scoped namespace conversion; refuses multi-namespace files. |
| `csharp_lsp_rename` | lsp_verified | Workspace-wide rename through `Microsoft.CodeAnalysis.LanguageServer`. |
| `csharp_organize_usings` | lsp_verified | Per-file `source.organizeImports`. |
| `csharp_lsp_move_item` | lsp_verified | Roslyn `refactor.move`; mirrors `rust_ra_move_item_to_module`'s constraint set (RA-equivalent assists only). |
| `find_csharp_usages` | lsp_verified | Project-wide references through LSP `textDocument/references`. |
| `csharp_public_api_guard` | indexed_hints | Advisory severity for proposed visibility / signature changes. |
| `move_csharp_type_to_file` | syntax_only | One type out of a multi-type file into a sibling `.cs` file. |
| `migrate_csharp_type_usages` | syntax_only | Rewrite `Old.Type` qualified usages to a new type; conservative position checks. |
| `csharp_to_record_migrate` | indexed_hints | Convert POCO-style class to `record` declaration; preserves attributes; refuses `[JsonConstructor]`. |
| `csharp_primary_ctor_migrate` | indexed_hints | Convert ctor-assigning class to primary-ctor form; same `[JsonConstructor]` refusal. |
| `csharp_async_dispose_convert` | indexed_hints | Convert `IDisposable` chains to `IAsyncDisposable` where the call graph is await-safe. |
| `unseal_csharp_class` | indexed_hints | Remove `sealed` from one class; requires `acknowledge_subclass_surface_change=true` (RX-V1). |
| `move_csharp_members_to_partial` | lsp_verified_partial | Move members to a sibling partial declaration; consults RX-V4 SG guard. |
| `csharp_partial_class_audit` | lsp_verified / lsp_verified_partial | RX-V4 source-generator contract audit. Analysis-only. |
| `csharp_awaited_query_in_loop_audit` | lsp_verified | IOperation walk over loop bodies. Analysis-only. |
| `csharp_compile_fix_round` | lsp_verified | Classify sidecar-emitted diagnostics into use-decl-add / replace_text / nullable-repair proposals. Use only inside `bbox_refactor_run`. |
| `csharp_nullable_annotation_repair` | lsp_verified | CS8618 / CS8625 single round; insert `required` modifier. |

Tree-sitter language: `csharp`. Status (`bbox_refactor_status`) returns
inspect-only inventory for any `.cs` file regardless of sidecar
availability.

## Backend topology

C# spans two backends, both reached through the same `bbox_refactor_plan`
surface:

- **Roslyn LSP** (`Microsoft.CodeAnalysis.LanguageServer`) — backs the
  pure-LSP kinds (`csharp_lsp_rename`, `csharp_organize_usings`,
  `csharp_lsp_move_item`, `find_csharp_usages`). Goes through the warm
  `LspSessionManager` like rust-analyzer and JDTLS. Tunables:
  `BLACKBOX_ROSLYN_LSP_BIN` (binary override),
  `BLACKBOX_ROSLYN_INIT_TIMEOUT_SECS`.
- **Roslyn sidecar** (`blackbox-csharp-worker`, custom JSON-RPC over
  stdio) — backs the kinds that need `MSBuildWorkspace`, `IOperation`,
  source-generator enumeration, or solution-wide diagnostics:
  `csharp_partial_class_audit`, `csharp_awaited_query_in_loop_audit`,
  `csharp_compile_fix_round`, `csharp_nullable_annotation_repair`,
  `move_csharp_members_to_partial`. Pooled per
  `(project_root, workspace)` with idle eviction.

**RX-V3 fail-closed** applies to both. When the LSP or sidecar is
unavailable, the affected kinds return
`error.lsp_unavailable: <kind> requires the Roslyn <lsp|sidecar> (RX-V3); <cause>`
with no silent downgrade to a syntactic approximation. Callers that
need a syntactic shape should reach for the explicit syntax-only kinds
(`migrate_csharp_to_filescoped_namespace`, `move_csharp_type_to_file`,
`migrate_csharp_type_usages`).

## Operator-authority acknowledgments (RX-V1, RX-V4, RX-V5)

Atoms NEVER default these; operator passes them explicitly:

- `acknowledge_subclass_surface_change` — `unseal_csharp_class`. Removing
  `sealed` is a public-API change in the RX-V1 sense (subclass surface
  becomes part of the contract).
- `acknowledge_public_api_change` — any visibility-elevating plan kind.
- `acknowledge_equality_semantics_change` — `csharp_to_record_migrate`
  (value equality replaces reference equality).
- `acknowledge_query_semantics_change`,
  `acknowledge_tracking_semantics_change` —
  `csharp_awaited_query_in_loop_audit` rewrites (eager-once vs
  lazy-per-iteration; tracked vs no-tracking semantics).
- `acknowledge_generator_contract_change` — RX-V4: a plan touches a
  partial / member that an `IIncrementalGenerator` keys on. Required
  whenever the partial-class SG guard reports a non-empty protected set.
- `acknowledge_partial_workspace` — RX-V5: the sidecar's
  expected-vs-loaded probe reports projects that `MSBuildWorkspace`
  silently dropped. Without it, sidecar-backed kinds refuse against
  partial workspaces.

Plan responses carry `operator_opt_outs_used` (named individually, not
collapsed) on the durable RefactorPlan; saved plans preserve the audit
trail.

## Compose-run protocol

The dotnet command allowlist (RX-V2 extension) for atom-dispatched runs:

- Allowed unconditionally: `dotnet build`, `dotnet test`,
  `dotnet format whitespace`, `dotnet format style`.
- Allowed with `touches` declared: `dotnet format` (with rewrite),
  `dotnet roslynator <…>`.
- Denied in atom contexts: `dotnet ef migrations *`,
  `dotnet ef database *`, `dotnet publish`, `dotnet nuget *`,
  `dotnet pack`, `dotnet restore` (snapshot-incompatible or
  destination-touching).

Canonical refactor-run sequence for an LSP-backed kind plus diagnostic
repair:

```jsonc
bbox_refactor_run(dispatch_origin="agent", confirm=true, steps=[
  {"op": "plan", "kind": "csharp_lsp_rename", "source": "src/Foo.cs",
   "item_names": ["OldName"], "new_text": "NewName",
   "project_dir": "/abs/proj"},
  {"op": "command", "command": "dotnet",
   "args": ["build", "-bl:diag.binlog"],
   "capture": "msbuild_diag", "on_failure": "continue_for_repair"},
  {"op": "plan", "kind": "csharp_compile_fix_round",
   "diagnostics_ref": "last"},
  {"op": "command", "command": "dotnet", "args": ["build"],
   "required": true},
  {"op": "command", "command": "dotnet",
   "args": ["format", "style", "--verify-no-changes"],
   "required": true},
  {"op": "command", "command": "dotnet", "args": ["test", "--no-build"],
   "required": true}
])
```

The sidecar's own `getDiagnostics()` is **not** equivalent to
`dotnet build` — source generators and full analyzer packs run only on
the real MSBuild path, so the validation step matters.

## Cross-cutting safety rules

- **Do not apply Rust or Java plan kinds to C# files.**
- **Generated-file refusal.** Plan kinds refuse edits to files under
  `**/Generated/**`, `*.g.cs`, `*.Designer.cs`, or files whose header
  carries the standard generated-code marker. RX-V4 guards
  partial-class moves; this is the file-level analog.
- **Source-generator contract.** Attribute-driven generators
  (`[GraphPredicateAttribute]`-style markers, Wolverine v5 message
  routing, gRPC contracts) emit sibling partials on the containing type.
  Renaming the marked method, moving its containing type, or moving the
  enclosing file silently breaks the generator. Run
  `csharp-partial-sg-guard` before any plan touching `partial` types in
  those projects.
- **Attribute and `[Obsolete]` preservation.** Kinds that move members
  between types preserve `[JsonPropertyName]`, `[Obsolete]`,
  `[Required]`, `[Range]`, etc. on each moved member; the plan response
  carries `attribute_preservation_audit`.
- **`[JsonConstructor]` refusal.** `csharp_to_record_migrate` and
  `csharp_primary_ctor_migrate` refuse when the original ctor carries
  `[JsonConstructor]` — the attribute does not transfer.
- **`ConfigureAwait` parity.** Kinds that move or merge awaited calls
  preserve the original `ConfigureAwait(false)` / no-`ConfigureAwait`
  choice; mixed loop bodies refuse with
  `error.configureawait_asymmetry`.
- **CancellationToken propagation.** Any synthesized async call
  forwards the nearest in-scope `CancellationToken`; missing token
  refuses with `error.no_cancellation_token`.
- **Workspace-load gating (RX-V5).** When the sidecar's probe reports
  projects `MSBuildWorkspace` failed to load, sidecar-backed kinds
  refuse unless the operator passes `acknowledge_partial_workspace=true`.
- **Tree-sitter is syntactic.** Inspect-only inventory does not resolve
  partial classes, generated code, extension methods, using aliases,
  nullable flow, or project references. For binding authority, route
  through the LSP or sidecar.

## Common acceptance smoke

After applying any structural plan, the operator (or chained atom)
should confirm:

1. `dotnet build` returns 0 with no new analyzer warnings.
2. `dotnet format style --verify-no-changes` returns 0 (no formatter
   drift; source generators stable).
3. `dotnet test --no-build` returns 0 against the touched projects.

The canonical refactor-run sequence above wires all three.
