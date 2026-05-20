---
title: "C# Refactor Expansion - Roslyn-backed third language track"
kind: design
lifecycle: archived
corpus: blackbox-design
topic:
  - refactor-tools
  - csharp
tags:
  - refactor-tools
  - csharp
  - roslyn
  - msbuild
date: 2026-05-15
revision: "rev 4.1 - codex round-4 converge cleanups (Phase 1/2 wording, lsp_verified_partial breadth, AppliedPlanDelta consistency)"
status: "implemented; archived after code audit"
brief: "Designs the C# refactor track — plan kinds, RX-V4/V5 invariants, Roslyn sidecar architecture, and atoms calibrated to net10.0 + Roslyn source-generator pain."
---

# C# Refactor Expansion — Roslyn-backed third language track

Related: `../ast-refactor-mechanization.md`, `../refactor-compound-runs.md`,
`../refactor-agents.md`, `../rust/refactor-rust-expansion.md`,
`../java/java-refactor-gaps.md`, `sm-refactor`

## Problem

Blackbox refactor tooling covers Rust (`src/refactor/rust.rs` + 23
submodules) and Java (`src/refactor/java.rs` + the `java/` submodule tree).
A third large class of operator codebases — net10.0 services with EF Core,
gRPC, Wolverine, and Roslyn source generators — sits outside that coverage.
Operators on these codebases currently fall back to `Edit` / `Bash` /
`bbox_refactor_run` with `replace_text` steps, which gives up the
semantic safety the Rust and Java tracks provide.

The reference codebase for this design is **daystrom-mk2**
(`../daystrom-mk2/`): 30 csproj projects in `Daystrom.slnx` (some
multi-targeted; SG projects target `netstandard2.0`, runtime projects
target `net10.0`), `<Nullable>enable</Nullable>` globally, file-scoped
namespaces as `.editorconfig` *suggestion* (not enforced), source
generators producing partial-class extensions, Wolverine v5 orchestration,
gRPC contracts, Roslyn 5.3.0 already in use
(`daystrom-mk2/src/Daystrom.Worker/Lsp/RoslynAdapter.cs`).

Pain surfaced by verified counts (re-grepped 2026-05-15 with
`bin/`/`obj/`/`.worktrees/` excluded):

- **278 `sealed class` files / 386 declarations** under `src/` plus a
  standing CLAUDE.md directive (`daystrom-mk2/CLAUDE.md:306–312`) to
  unseal services as test pressure appears. The directive is a
  *conditional guideline*, not a Fowler strangler-fig migration — one
  class at a time, when concrete test pressure exists.
- **36 partial-type files** across `src/` (any-modifier `partial class|record|struct|interface`; 30 when the regex requires an explicit `public|internal|private|protected` prefix). Topology matters: most live in
  `Daystrom.Contracts/Messages/` and are coupled to **Wolverine v5**'s
  message-routing infrastructure (which generates handler code at
  build time), *not* to `Daystrom.Graph.SourceGenerators`. Only a
  smaller subset is bound to `[GraphPredicateAttribute]`, and the
  binding is at the **method** level — the generator emits a sibling
  partial for the *containing type* of the marked method (see
  "GraphPredicateGenerator shape" below). Renaming the method, moving
  its containing type, or moving the enclosing file all break the
  generator contract silently. Wolverine generators arrive via
  `PackageReference` and are not in-repo, so RX-V4 must enumerate
  analyzer references and package generators to protect them — see
  the invariant below.
- **Awaited-collection iteration patterns** in graph services —
  `FileOverlapService.cs:54,61,88,91`, `EpistemologyQueryService.cs:635,647`,
  `SpecGraphService.cs:1526,1540`, `WhiteboardGraphService.cs:829`.
  These are *not* EF Core N+1 (the codebase has **zero** `DbSet<T>` /
  `Microsoft.EntityFrameworkCore` references) — they are custom
  graph-store async helpers that may or may not be hot-path
  pessimisations. Surfacing the pattern as an analysis kind is
  appropriate; mechanizing a rewrite is not, because the semantics of
  each helper (single-call returning a collection vs. per-iteration
  store hit) differ.
- **Uninitialized non-nullable properties** under `<Nullable>enable</Nullable>`
  (AUDIT T1-010, T2-024). Compiler emits CS8618 but the fix is mechanical.
- **MSBuildWorkspace silently drops projects.** Worker logs show
  `Daystrom.Gateway` (and historical `Daystrom.Migration`, since removed)
  missing across 200K+ log lines because
  `RoslynAdapter.OnWorkspaceFailed` (`RoslynAdapter.cs:127`) only
  collects diagnostics, it does not gate downstream analysis.

### GraphPredicateGenerator shape (verified ground truth)

`Daystrom.Graph.SourceGenerators/GraphPredicateGenerator.cs:25–42`:

```csharp
[Generator(LanguageNames.CSharp)]
public sealed class GraphPredicateGenerator : IIncrementalGenerator {
  public const string PredicateAttributeMetadataName =
      "Daystrom.Graph.SchemaModel.GraphPredicateAttribute";

  public void Initialize(IncrementalGeneratorInitializationContext context) {
    var candidates = context.SyntaxProvider
        .ForAttributeWithMetadataName(
            PredicateAttributeMetadataName,
            predicate: static (node, _) => node is MethodDeclarationSyntax,
            transform: static (ctx, _) => ExtractCandidate(ctx))
        ...
  }
}
```

Two facts the design must respect: the attribute targets
**`MethodDeclarationSyntax`**, not type declarations; and the generator
emits a sibling **partial of the containing type** (and its outer
type frames, for nested cases). RX-V4 below scans at the right level.

This design proposes the C# plan kinds, sidecar architecture, invariants,
and atoms needed to lift these from manual edits to operator-driven
mechanized refactors with the same dry-run-plus-validate contract as the
Rust and Java tracks.

## Non-goals

- **A new analyzer ecosystem.** Existing Roslyn analyzers
  (IDisposableAnalyzers 4.0.8, Roslynator.Analyzers 4.15.0) already ship
  diagnostics; this design *consumes* them, it does not replace them.
- **Source-generator authoring.** Plan kinds may inspect generators to
  protect their input contracts; they will not generate or edit
  `IIncrementalGenerator` implementations.
- **Cross-solution refactors.** All operations are scoped to one `.sln` /
  `.slnx` / single-project workspace. Solution-of-solutions topologies are
  out.
- **Architectural rewrites.** No "service-to-microservice", no
  "REST-to-gRPC", no DDD layering moves. Mechanical extraction and
  semantic edits only — same scope as the Rust and Java tracks.
- **Workspace-wide formatting.** `dotnet format` runs as a validation
  command inside compound runs but is not its own plan kind.

## Backend choice — in-process Roslyn sidecar vs subprocess LSP

The Rust and Java tracks both speak LSP JSON-RPC over stdio to
language-server subprocesses managed by `src/lsp/session_manager.rs`
(`LspSessionManager` keyed by `(project_root, Language)`, 760 lines, lazy
session pool, idle eviction).

C# has two viable backends:

### Option A — `Microsoft.CodeAnalysis.LanguageServer`

The Roslyn team ships an LSP server binary. It speaks standard LSP and
slots into `LspSessionManager` with the smallest possible delta:

1. Add `Language::CSharp` to the enum and routing tables.
2. Add env vars `BLACKBOX_ROSLYN_LSP_BIN`,
   `BLACKBOX_ROSLYN_INIT_TIMEOUT_SECS`.
3. RX-V3 (fail-closed on LSP unavailability) applies unchanged.
4. Plan-kind backends call `request_*_actions` helpers in the same shape
   as `src/refactor/rust_ra_move_item.rs:69–117`
   (`request_move_actions` builds a `CodeActionRequest` with kind
   `refactor.move` and converts the returned `WorkspaceEdit` to blackbox
   `FileEdit { path, byte_start, byte_end, replacement }`).

Pro: zero new build artifact, identical operational shape to rust-analyzer
and jdtls, parity with RX-V3.

Con: standard LSP gives the client a fixed surface — rename, code
actions, organize imports, diagnostics. The richer plan kinds need
things the LSP wire format does not carry:

- **Custom plan JSON.** Blackbox plans are not LSP `WorkspaceEdit`s;
  they include `external_calls`, `captured_variables`, `validations`,
  `operator_opt_outs_used`. Either we wrap every plan kind in a custom
  code-action request and post-process the result, or the sidecar
  speaks our shape directly.
- **Run-scoped transaction control.** `bbox_refactor_run` needs to push
  edits, query diagnostics, push more edits, and roll back the whole
  set on failure. Standard LSP has no notion of an open transaction
  across multiple requests.
- **Workspace-load state inspection.** RX-V5 below depends on comparing
  `.sln/.slnx` expected projects to actually loaded projects and
  surfacing per-project failures. The LSP `workspace/configuration`
  surface is not the right shape.
- **Source-generator and load-state APIs.** RX-V4 walks
  `IIncrementalGenerator` implementations to find the attribute markers
  the generator keys on. Standard LSP does not expose this.

A Roslyn analyzer / code-action provider behind the LSP *could* use
`IOperation` internally — but the result it returns over LSP is still a
`WorkspaceEdit`, not the structured plan we need. So the LSP path
necessarily forces a custom code-action protocol on top of the LSP
plumbing, which is most of the work of the sidecar anyway.

### Option B — Custom dotnet sidecar

A small `blackbox-csharp-worker` process speaking custom JSON-RPC over
stdio, modeled directly on
`daystrom-mk2/src/Daystrom.Worker/Lsp/RoslynAdapter.cs`. That file already
proves every load-bearing primitive:

- `MSBuildWorkspace.Create()` + `OpenSolutionAsync` /
  `OpenProjectAsync` (`RoslynAdapter.cs:106–125`).
- In-memory dry-run via `Solution.WithDocumentText`
  (`RoslynAdapter.cs:135–142`) — the snapshot lives in the sidecar, no
  disk write.
- Per-file and solution-wide compiler diagnostics via
  `SemanticModel.GetDiagnostics` and `Compilation.GetDiagnostics`
  (`RoslynAdapter.cs:149–212`). **Note**: these cover compiler
  diagnostics on user code **and** on generator-produced syntax trees,
  but they do **not** include diagnostics that generators themselves
  report (those live on `GeneratorDriverRunResult.Diagnostics`).
  Analyzer rules require `CompilationWithAnalyzers` and MSBuild-only
  diagnostics require an actual `dotnet build`. See "Compound runs"
  for the full coverage matrix.
- Cross-project rename via `Renamer.RenameSymbolAsync`
  (`RoslynAdapter.cs:77`) returning a diff against the original solution.
- Call-edge extraction over `IInvocationOperation` /
  `IObjectCreationOperation` / `IPropertyReferenceOperation`
  (`RoslynAdapter.cs:281–321`) — needed for the awaited-query-in-loop
  detector and similar operation-tree walks.

Pro: any plan kind that needs semantic depth has it; transaction control
and load-state inspection are first-class; protocol shape matches
blackbox plans directly.

Con: a new dotnet binary to build, ship, and version. Requires a
`RoslynSessionManager` sibling to `LspSessionManager` (the two cannot
share a pool because the sidecar protocol is not standard LSP).

### Decision

**Phase 1: Option A.** Ship the LSP-only plan kinds first
(`csharp_lsp_rename`, `csharp_organize_usings`, `csharp_lsp_move_item`,
`find_csharp_usages`). Prove the LSP integration end-to-end, exercise
RX-V3, fill out tool docs. `csharp_lsp_move_item` must probe the
server's code-action capability at session init and fail closed with
`error.lsp_code_action_unavailable` if `refactor.move` is not advertised
— `Microsoft.CodeAnalysis.LanguageServer` does not guarantee the same
action set rust-analyzer exposes.

Note: `migrate_csharp_to_filescoped_namespace` is `syntax_only` and does
not need the sidecar or the LSP. It can ship in parallel with Phase 1 as
a tree-sitter / Roslyn-SyntaxTree-only kind.

**Phase 2: Option B.** Add the sidecar for plan kinds that need custom
plan JSON, transaction control, generator-input inspection, or
`IOperation` walks — `csharp_partial_class_audit`,
`csharp_awaited_query_in_loop_audit`, `csharp_compile_fix_round`,
`csharp_nullable_annotation_repair`, `unseal_csharp_class` (the
inheriting-candidates report), `move_csharp_type_to_file` (namespace
rewrite + reference repair), and any future `csharp_ef_hoist_query`
rewrite. The forcing function is the combination of custom plan JSON +
transaction control; `IOperation` is merely a downstream enabler.

## Boundary — three semantic tiers

Same tier vocabulary as the Rust expansion design — preserves contract
parity for atomic-agent prompts.

- **`syntax_only`** — Roslyn `SyntaxTree` (or tree-sitter C# grammar) only.
  No semantic model. Used for batch syntactic rewrites
  (block-to-file-scoped namespaces, trivia normalization).
- **`indexed_hints`** — `syntax_only` plus best-effort lookup through a
  project-local symbol index (e.g. `RoslynIndexService` already in
  daystrom-mk2 at `Daystrom.Worker/Services/RoslynIndexService.cs`).
  Reports may include false negatives (external-assembly symbols not
  resolvable from the index) and false positives (name collisions).
- **`lsp_verified`** — backed by a live Roslyn `SemanticModel` /
  `Compilation`. Symbol resolution, type binding, and diagnostic
  attribution are authoritative within the loaded workspace, modulo
  RX-V5 (see below).
- **`lsp_verified_partial`** — same authority as `lsp_verified`, but
  some semantic caveat applies that the operator has explicitly
  acknowledged. Two triggers in v1:
  1. **RX-V5** — the workspace failed expected-vs-loaded comparison
     and the operator passed `acknowledge_partial_workspace=true`.
  2. **RX-V4** — a raw-classification or unknown-package source
     generator is in scope and the operator declared coverage via
     `generator_inputs` manifest (the audit cannot fully verify the
     declaration matches reality).
  Downstream consumers (atom dispatchers, compound-run validation,
  operator tooling) MUST treat this tier as semantically weaker than
  `lsp_verified` for any consumer that aggregates across projects
  (e.g. cross-project rename, public-API guard). The plan's
  `semantic_caveats` audit field enumerates which trigger(s) apply
  and the relevant manifest/load-state details.

Adding `lsp_verified_partial` requires extending the existing
`SemanticStatus` enum (currently `syntax_only` / `indexed_hints` /
`lsp_verified`) and its tool-doc surface. Cross-cutting edit; listed in
the file-layout section.

Most C# plan kinds will land at `lsp_verified` because Roslyn is in-process
and cheap; `syntax_only` is reserved for codebase-wide
mechanical sweeps and `indexed_hints` for plan kinds invoked before the
sidecar has finished loading.

## Plan kinds

Ranked by daystrom-mk2 pain leverage. Each entry: name, summary, semantic
tier, operator-authority flags, related Rust/Java analog.

### Tier 1 — daystrom-killer kinds

#### `csharp_awaited_query_in_loop_audit` (analysis-only)

`semantic_status: lsp_verified` (Phase 2 sidecar required).

**Analysis-only**, no edits. Walks method bodies via `IOperation` trees
(`model.GetOperation(node)`) and reports every `IAwaitOperation` whose
operand is an `IInvocationOperation` appearing inside an
`IForEachLoopOperation` or `IForLoopOperation` body. Per finding,
reports:

- The loop header (collection expression + iteration variable).
- The awaited call's target method, declaring type, and assembly.
- Whether the call is **inside** the loop body (per-iteration await,
  the actual N+1 risk) vs the loop header's collection expression (a
  single async call returning a collection — not N+1, benign pattern).
- Captured variables from the enclosing scope.

This is the right primitive for the Daystrom codebase, which has **zero
EF Core usage** (`DbSet<T>` / `Microsoft.EntityFrameworkCore` references
are absent) and whose graph services use custom async store helpers
whose hot-path semantics vary case by case. The cited sites
(`FileOverlapService.cs:54,61,88,91`, `EpistemologyQueryService.cs:635,647`,
`SpecGraphService.cs:1526,1540`, `WhiteboardGraphService.cs:829`) are
**collection-expression awaits**, not per-iteration awaits — the audit
classifies them as benign and surfaces only the truly per-iteration
cases for operator review.

No operator-authority flag — no mutation.

#### `csharp_ef_hoist_query` (rewrite, EF Core only) — deferred

`semantic_status: lsp_verified` (Phase 2 sidecar required). **Status:
designed, not in v1 catalog.** Held back until exercised on an
EF-using codebase.

Narrowly-scoped EF Core N+1 rewrite. Detection requires:

1. The awaited call's target is an extension method on
   `EntityFrameworkQueryableExtensions` (this is where `AnyAsync`,
   `ToListAsync`, `FirstOrDefaultAsync` actually live — they are *not*
   members of `DbSet<T>` or `IQueryable<T>` directly).
2. The `this` receiver of the extension method resolves through
   `IQueryable<T>` or `DbSet<T>`.
3. The query predicate is a closure over the loop iteration variable
   (so the rewrite has a join key).
4. No `ConfigureAwait(false)`-required-then-otherwise asymmetry inside
   the loop body (refusal rule).
5. The `DbContext` change-tracker behavior is preserved — hoisting a
   query with `AsTracking()` semantics into a single batch changes which
   entities the context tracks. Refuses unless `AsNoTracking()` or
   `acknowledge_tracking_semantics_change=true`.
6. Cancellation token propagation is preserved (the hoisted call must
   accept the same `CancellationToken` as the original calls).

Plan response includes the generated SQL preview (via
`EF.CompileQuery`-style analysis when feasible) so the operator can
confirm the hoisted query is not pathological.

Operator-authority flags: `acknowledge_query_semantics_change=true`
(eager-once vs lazy-per-iteration), `acknowledge_tracking_semantics_change=true`
(only when tracking is in play).

Defer rationale: the rewrite is risky enough that it should not ship
until there is an EF-using reference codebase to exercise it on. The
audit kind above is the cheap, safe primitive for v1; the rewrite is a
v2 follow-up once we have ground truth.

#### `unseal_csharp_class`

`semantic_status: lsp_verified`.

Removes the `sealed` modifier from a single class declaration and
optionally marks operator-named methods `virtual` so test subclasses can
override. Plan response reports:

- `external_callers`: every site that constructs the class (visibility
  change is benign here, but reported for audit).
- `inheriting_candidates`: types in `tests/` that could plausibly subclass
  (heuristic on naming).
- `affected_diagnostics`: any analyzer warnings the unseal would trigger
  (CA1052 static-only types, MA0053 design analyzers).

Operator-authority flag: `acknowledge_subclass_surface_change=true`.
Required — unsealing is a public-API change in the same RX-V1 sense as
`acknowledge_repr` for Rust struct field moves.

Evidence: 278 sealed-class files / 386 declarations; CLAUDE.md:306–312
"prefer public class over sealed class" directive.

#### `csharp_partial_class_audit`

`semantic_status: lsp_verified`.

Pre-flight analysis for any structural refactor touching `partial class`
declarations. Discovery is more nuanced than the v0 design assumed —
generators key on attributes at three possible levels:

1. **Type-level attributes.** `ForAttributeWithMetadataName(...)` with
   `predicate: node is TypeDeclarationSyntax`. Generator emits a sibling
   partial of the marked type.
2. **Method-level attributes** (the `GraphPredicateGenerator` shape).
   `predicate: node is MethodDeclarationSyntax`. Generator emits a
   sibling partial of the **containing type** of the marked method (and
   its outer type frames for nested types).
3. **Member-level attributes** on properties, fields, enums. Same
   pattern.

The audit must therefore scan attribute targets at all three levels and
trace each finding up to the *containing-type chain* that owns the
emitted partial. Reports:

- `generator_bound_partials`: list of
  `(file, type_chain, attribute, attribute_target_level, generator,
   attribute_targets[])` tuples, where `attribute_targets` lists every
  member/method/type holding the attribute.
- `safe_to_move`: partial types with no generator-bound members and no
  type-level marker.
- `requires_generator_review`: partials whose move/rename/extract would
  detach a marked member from its containing type, or move the type out
  of the generator's scanned scope.
- `undetected_generators`: list of generators (in-repo, analyzer-reference,
  or package-shipped) whose pipeline shape is not
  `ForAttributeWithMetadataName` — these are not classifiable by v1
  and trigger RX-V4 fail-closed unless `generator_inputs` is
  operator-declared. See the RX-V4 enumeration spec for the full
  three-source discovery and fingerprinting protocol.
- `unknown_external_generators`: list of generator-shipping packages
  not in the curated known-package registry. Treated equivalently to
  `undetected_generators` — structural partial edits are refused
  until the registry adds coverage or the operator supplies a
  matching `generator_inputs` manifest entry.

Standalone analysis kind — does not produce edits. Other plan kinds
(`csharp_lsp_rename`, `move_csharp_type_to_file`,
`move_csharp_members_to_partial`) consume it as a precondition under RX-V4.

Evidence: 36 partial-type files in `daystrom-mk2/src/`. Most are in
`Daystrom.Contracts/Messages/` and are extended by Wolverine v5's
generator (not by `Daystrom.Graph.SourceGenerators`). The
`GraphPredicateGenerator` binding is method-level
(`Daystrom.Graph.SchemaModel.GraphPredicateAttribute`) and emits a
partial of the method's containing type. The audit handles both shapes;
RX-V4 treats both as protected.

#### `csharp_nullable_annotation_repair`

`semantic_status: lsp_verified`.

Consumes CS8618 / CS8625 / CS8602 diagnostics from the Roslyn compilation
(via `SemanticModel.GetDiagnostics`) and proposes the smallest mechanical
fix per site:

- Uninitialized non-nullable property → add `required` modifier (net7+)
  or initialize in primary/parameter constructor.
- Nullable dereference → add null check or `?.` access.
- Possible null assignment → narrow type or change to nullable
  reference (`T?`).

Pairs with `csharp_compile_fix_round` (below) as the multi-round driver.

Evidence: AUDIT T1-010, T2-024.

### Tier 2 — parity with Rust / Java baselines

#### `csharp_lsp_rename`

`semantic_status: lsp_verified`. Direct analog of `rust_lsp_rename` /
`rename_java_symbol`. Backed by `Renamer.RenameSymbolAsync`
(`RoslynAdapter.cs:77`). RX-V3 fail-closed on Roslyn unavailability.

#### `csharp_organize_usings`

`semantic_status: lsp_verified`. Roslyn `organize imports` code action.
Analog of `rust_organize_imports` / `java_lsp_organize_imports`.

#### `csharp_lsp_move_item`

`semantic_status: lsp_verified`. Move a top-level type or member between
files via Roslyn code action. Analog of `rust_ra_move_item_to_module`.

#### `move_csharp_members_to_partial`

`semantic_status: lsp_verified`. Split a large class into multiple
`partial class` files. Pre-flight: `csharp_partial_class_audit` must pass.

#### `move_csharp_type_to_file`

`semantic_status: indexed_hints`. Move a type to a different file/folder
and rewrite the containing namespace block. Folder-aware: derives the new
namespace from the project root + folder path (configurable via
`csharp_root_namespace` plan param).

#### `migrate_csharp_type_usages`

`semantic_status: lsp_verified`. Cross-solution type replacement. Analog
of `migrate_rust_type_usages` / `migrate_java_type_usages`. Supports
`replacement_kind` discriminator: `concrete`, `interface`, `nullable`,
`required_member`.

#### `find_csharp_usages` / `csharp_public_api_guard`

`semantic_status: lsp_verified` (analysis-only). Parity surfaces — usage
search and public-API change detection. The latter wraps Roslyn's
`PublicAPI.Shipped.txt` / `PublicAPI.Unshipped.txt` if present, falls
back to declared-accessibility comparison otherwise.

### Tier 3 — mechanical sweeps

#### `migrate_csharp_to_filescoped_namespace`

`semantic_status: syntax_only`. Block-to-file-scoped namespace conversion.
Codebase-wide batch. Idempotent; no semantic risk.

Evidence: daystrom-mk2 `.editorconfig` has the rule as `suggestion`, not
`warning` — codebase is mixed.

#### `csharp_to_record_migrate`

`semantic_status: lsp_verified`. POCO/DTO class → `record` migration.
Detects classes that satisfy the record shape: all properties get-only,
no inheritance, no virtual members, no method bodies beyond `ToString` /
`Equals` / `GetHashCode`. Analog of `lombokify_java_class`.

**EF entity guard (refusal rule).** Refuses with
`error.ef_entity_candidate` when the type satisfies any of:

- Has a property attributed `[Key]` or `[ForeignKey]`.
- Is referenced from a `DbSet<T>` declaration anywhere in the
  workspace.
- Implements `IEntityTypeConfiguration<T>` for itself.
- Has any property whose type is a `DbContext`-tracked navigation
  target (heuristic: another type in the same set of `DbSet<T>` ones).

Converting EF entities to records breaks change-tracking proxies and
navigation property loading. The refusal is hard — no operator opt-out.
(Daystrom-mk2 has no EF Core so this guard fires on no current targets;
it exists for safety on other codebases.)

**Serialization-attribute guard (refusal rule).** Refuses when the type
or any property carries `[JsonConstructor]`,
`[OnDeserializing]` / `[OnDeserialized]` callbacks, or non-default
`[JsonPropertyName]` patterns that depend on class identity. These
silently break under records.

Operator-authority flag: `acknowledge_equality_semantics_change=true` —
records use structural equality; classes use reference equality.
Required.

#### `csharp_primary_ctor_migrate`

`semantic_status: lsp_verified`. Multi-arg constructor → primary
constructor (net8+ for classes, net6+ for records). Applies when *every*
constructor body in the class is parameter-assignment-only. DI-heavy
classes are the target.

**Per-constructor decision.** A class with N constructors where some are
assignment-only and some have logic does *not* satisfy the migration —
primary constructors are mutually exclusive with explicit constructors
of the same arity. The plan kind refuses with
`error.constructor_logic_present { constructors: [...] }` listing the
offending ctors. No blanket operator opt-out; the operator must extract
the logic-bearing ctors into factory methods first.

Operator-authority flag: not required when all ctors are
assignment-only. When any ctor contains a base()/this()-chaining call
beyond pure parameter forwarding, plan refuses regardless.

#### `csharp_async_dispose_convert`

`semantic_status: lsp_verified`. `IDisposable` → `IAsyncDisposable` or
both. Consumes IDisposableAnalyzers diagnostics. Inserts standard
`DisposeAsyncCore` pattern.

### Tier 4 — compound / repair

#### `csharp_compile_fix_round`

`semantic_status: lsp_verified`. Post-apply diagnostic-driven repair.
Analog of `rust_compile_fix_round` (`src/refactor/rust_compile_fix.rs`).

Critical difference: Rust shells out to `cargo check --message-format=json`
and parses stdout. C# does **not** — the Roslyn sidecar already holds an
open `Compilation` and answers `GetDiagnostics()` in-process. The
diagnostics-settling fixed-point loop (RX-C1 in the Rust v2 invariants
doc) is the same; the per-round cost is lower.

Diagnostic-to-fix classifications. Each row either produces a concrete
edit or marks the diagnostic as `leftover` (operator-visible, no edit
proposed):

- **CS0246** (type or namespace not found) → query
  `Compilation.GetSymbolsWithName(name)` across the workspace + every
  `MetadataReference`; if exactly one match, insert the matching `using`
  directive. Multiple matches or zero matches → leftover.
- **CS1061** (member not found) → `SymbolFinder.FindSimilarSymbolsAsync`
  on the receiver type's accessible members + extension methods in
  scope; if exactly one Levenshtein-1 match, propose the renamed-call
  edit. Else → leftover (no spelling guesses).
- **CS8618** (non-nullable property uninitialized) → delegate to
  `csharp_nullable_annotation_repair`.
- **CS0535** (interface member not implemented) → stub with
  `throw new NotImplementedException()` *only* when the operator passes
  `allow_stub_throws=true`; otherwise leftover (stubs are silent
  behaviour changes).
- **CS0103** (name does not exist) → check for renamed-symbol candidates
  via `SymbolFinder.FindSimilarSymbolsAsync` over enclosing scopes; same
  edit-or-leftover rule as CS1061.

Diagnostics not in the table classify as `leftover` and the round stops
producing edits for them — they surface to the operator.

Hard cap: 5 rounds. Stops on settled (same diagnostic codes + spans
between rounds), cap-reached, or all remaining diagnostics classified
as leftover.

## Invariants

### RX-V4 — C# source generator contract guard

Plan kinds touching `partial class` declarations, the methods/members
that carry generator-keyed attributes, or any type the audit identifies
as `requires_generator_review` MUST refuse with
`error.generator_contract_break` listing the generator, attribute, and
each affected target — unless the operator passes
`acknowledge_generator_contract_change=true`.

The flag, when consumed, lands in the durable `RefactorPlan`'s
`operator_opt_outs_used` audit field (same shape as `acknowledge_repr`
in RX-V1) — saved plans preserve the trail.

**Discovery scope and v1 limitation.** The audit walks
`*.SourceGenerators/` projects for `ForAttributeWithMetadataName(...)`
calls at type/method/member predicate levels and traces each finding
to the containing-type partial that the generator emits. Generators
using raw `SyntaxProvider.CreateSyntaxProvider(...)` with custom
predicates **are not classifiable by v1** — they evade the
metadata-name scan.

The escape hatch is operator-declared manifest, with **mandatory
enumeration and fingerprinting** to prevent the silent-bypass
foot-gun:

1. The audit enumerates every `IIncrementalGenerator` implementation
   in scope, from **three sources** — restricting enumeration to
   in-repo `*.SourceGenerators/` projects misses generators delivered
   as analyzer/package references (Wolverine v5, MediatR, Mapperly,
   System.Text.Json source generation, etc.):

   a. **In-repo source-generator projects** under `*.SourceGenerators/`.
      Walk their syntax trees for `IIncrementalGenerator` implementers.
   b. **`AnalyzerReference`s on every project's `Compilation`.**
      `Project.AnalyzerReferences` carries every analyzer assembly,
      including those contributed by `PackageReference` packages.
      Reflectively enumerate `IIncrementalGenerator` implementations
      via `AnalyzerReference.GetGenerators(LanguageNames.CSharp)`.
   c. **`<PackageReference>` items in csprojs.** Cross-reference
      against a curated registry of known generator-shipping packages
      (`WolverineFx.*`, `MediatR.SourceGenerator`, `Mapperly`,
      `Microsoft.Extensions.Logging.Abstractions` LoggerMessage gen,
      `System.Text.Json.SourceGeneration`, etc.) so the audit can
      classify each known package's pipeline shape *and* surface
      unknown packages as `unknown_external_generator` for operator
      review.

   For each enumerated generator (from any source), compute a SHA-256
   fingerprint over the generator type's full name + containing
   assembly identity (name, version, public key token). For in-repo
   generators, also fingerprint the source file. Package-shipped
   generators move version-by-version; fingerprint changes on package
   upgrade and force a manifest refresh.
2. For each enumerated generator, the audit classifies its discovery
   shape: `attribute_metadata_name` (covered by v1),
   `raw_syntax_provider` (not covered), or `register_post_initialization`
   (also not covered).
3. Generators classified `raw_syntax_provider` or
   `register_post_initialization` are listed in the audit response's
   `undetected_generators` array as
   `{ name, fingerprint, source_path, classification }` tuples.
4. The operator-declared `generator_inputs` manifest MUST list every
   `undetected_generators` entry by name and fingerprint:

```jsonc
// .blackbox/csharp.json
{
  "generator_inputs": [
    {
      "generator": "MyCustomGenerator",
      "fingerprint": "sha256:abc123...",
      "attributes": ["MyNamespace.MyMarkerAttribute"],
      "target_levels": ["method", "type"],
      "operator": "alice@2026-05-15",
      "rationale": "Generator scans by type-name suffix instead of attribute."
    }
  ]
}
```

5. Missing fingerprint, mismatched fingerprint (generator source
   changed since manifest was authored), undeclared generator from
   the enumerated set, or an `unknown_external_generators` package
   not in the registry all cause RX-V4 to refuse with
   `error.undeclared_generator { name, fingerprint, source }`
   (source ∈ `in_repo` | `analyzer_reference` | `package`) — even
   with `acknowledge_generator_contract_change=true`. The operator
   must refresh the manifest first. This is the load-bearing
   fail-closed behavior; without it, a `dotnet add package
   WolverineFx` followed by a refactor would silently bypass the
   guard.
6. When the manifest is consumed, every declared generator's name +
   fingerprint lands in the durable `RefactorPlan`'s
   `operator_opt_outs_used` audit field. The plan's
   `semantic_status` is set to `lsp_verified_partial` if any
   raw-classification generator is in scope (the audit can't fully
   verify it didn't change the contract).

Closing the v1 limitation (heuristic walk of raw
`CreateSyntaxProvider` predicates to recover automatic discovery) is
a v2 follow-up.

**Concrete grounded case.** `Daystrom.Graph.SchemaModel.GraphPredicateAttribute`
is method-level. The `GraphPredicateGenerator`
(`daystrom-mk2/src/Daystrom.Graph.SourceGenerators/GraphPredicateGenerator.cs:25–42`)
emits a sibling partial of the **containing type** for each marked
method. Renaming the method, moving the containing type to a different
file or namespace, or extracting the method into a separate type all
break the contract silently. RX-V4 protects all three.

This mirrors the RX-V1 operator-authority pattern: the planner refuses
by default and the operator must explicitly opt in.

### RX-V5 — MSBuild partial-load fail-closed

The workspace-load check has three inputs:

1. **Expected projects.** Parse the `.sln` / `.slnx` to enumerate every
   `csproj` reference (`MSBuild.ProjectRootElement` or a slnx parser;
   the sidecar produces an `expected_projects: [name, path]` list).
2. **Loaded projects.** After `OpenSolutionAsync` /
   `OpenProjectAsync`, query `_solution.Projects` for actually loaded
   projects (the sidecar's `GetLoadedProjectInfo()` analog —
   `RoslynAdapter.cs:384–394` already returns this).
3. **Workspace diagnostics.** The handler registered at
   `RoslynAdapter.cs:127` collects `WorkspaceDiagnostic` events
   classified as `WorkspaceDiagnosticKind.Warning` or
   `WorkspaceDiagnosticKind.Failure`.

A project counts as **dropped** iff it appears in `expected_projects`
but not in `_solution.Projects`. A project counts as **degraded** iff
it appears in both but the diagnostic stream contains a
`WorkspaceDiagnosticKind.Failure` referencing it. Warnings alone are
not blocking but are reported.

All `lsp_verified` plan kinds MUST return
`error.workspace_partial_load { dropped: [...], degraded: [...],
warnings: [...] }` when `dropped` or `degraded` is non-empty, unless
the operator passes `acknowledge_partial_workspace=true`. When the flag
is consumed, it lands in the durable `RefactorPlan`'s
`operator_opt_outs_used` audit field alongside the dropped/degraded
lists — saved plans preserve the trail and the plan response's
`semantic_status` is downgraded from `lsp_verified` to
`lsp_verified_partial` so downstream consumers can see the asterisk.

Without this guard, plans silently analyze a *subset* of the codebase.
Worker logs from daystrom-mk2 show `Daystrom.Gateway` (and historical
`Daystrom.Migration`, since removed from the slnx) missing across 200K+
log lines — every plan that ran during those windows operated on an
incomplete graph and the operator had no signal that results were
degraded.

This is the C# analog to RX-V3 — both are "silent semantic downgrade"
guards. RX-V3 fires at LSP-availability granularity (server up/down);
RX-V5 fires at workspace-load granularity (which projects loaded
successfully).

### RX-V2 extension — dotnet command allowlist

For atom-dispatched compound runs, the cargo-only allowlist in RX-V2
extends to dotnet:

- Allowed unconditionally: `dotnet build`, `dotnet test`, `dotnet format
  whitespace`, `dotnet format style`.
- Allowed with `touches` declared: `dotnet format` (with rewrite), any
  `dotnet roslynator` invocation.
- Denied in atom contexts: `dotnet ef migrations *`, `dotnet ef database
  *`, `dotnet publish`, `dotnet nuget *`, `dotnet pack`, `dotnet
  restore` (snapshot-incompatible or destination-touching).

Enforcement parity with RX-V2 requires a code change to
`enforce_agent_command_allowlist`. The current implementation accepts
**only** cargo subcommands for `dispatch_origin=agent` and rejects
everything else; without the change, an atom-dispatched run that
shells `dotnet build` would be **rejected** (not silently allowed —
the prior wording was wrong). The needed change: add a dotnet branch
that matches the allowlist above, with `touches`/capture semantics
preserved.

Brofile (`csharp-refactor-persona` denying `Bash`/`Write`/`Edit`) is
necessary but not sufficient — without the allowlist extension, no
atom can run `dotnet build` at all, which breaks the recipes above.

## Safety rules (additive, per kind)

The Rust expansion doc enumerates additive safety rules per kind. The
C# track inherits the same convention; cross-cutting rules:

- **`ConfigureAwait(false)` parity.** Plan kinds that move or merge
  awaited calls (`csharp_awaited_query_in_loop_audit` rewrites, any
  future `csharp_ef_hoist_query`) MUST preserve the
  `ConfigureAwait(false)` / no-`ConfigureAwait` choice from each
  original call site. Refuse with `error.configureawait_asymmetry` when
  the loop body mixes both.
- **CancellationToken propagation.** Any plan kind that synthesizes a
  new async call (compile-fix CS0246 / CS1061 fixes, EF hoist)
  MUST forward the nearest in-scope `CancellationToken`. Refuse with
  `error.no_cancellation_token` when none is in scope.
- **Generated-file edits.** Plan kinds MUST refuse edits to files
  matching `**/Generated/**`, `**/*.g.cs`, `**/*.Designer.cs`, or files
  whose header carries the standard generated-code marker. RX-V4 already
  guards partial-class moves; this is the file-level analog for any
  edit kind.
- **Serialization-attribute and `[Obsolete]` propagation.** Any kind
  that moves members between types MUST preserve attributes (`[JsonPropertyName]`,
  `[Obsolete]`, `[Required]`, `[Range]`, etc.) on the moved member;
  losing them is a silent behavior change. The plan response carries an
  `attribute_preservation_audit` field per moved member.
- **`[JsonConstructor]` refusal.** `csharp_to_record_migrate` and
  `csharp_primary_ctor_migrate` MUST refuse when the original ctor
  carries `[JsonConstructor]` — the attribute does not transfer to a
  primary or implicit ctor.
- **Public-API stability.** Plan kinds that change visibility
  (`unseal_csharp_class` removing `sealed`, future visibility-elevation
  kinds) MUST consult `csharp_public_api_guard` and either refuse or
  consume `acknowledge_public_api_change=true` (RX-V1 parity).
- **Nullable annotation safety.** `csharp_nullable_annotation_repair`
  MUST NOT change a property's effective nullability when the property
  is part of a serialization contract (System.Text.Json /
  Newtonsoft.Json + `[JsonRequired]` or non-nullable settings).
- **Workspace consistency.** Every `lsp_verified` plan kind MUST pass
  RX-V5 before producing edits. The check is mechanical and cheap; the
  cost is one sidecar `getLoadStatus()` RPC.

## Compound-run recipes (canonical step sequences)

Atom prompts use these recipes as templates. Each shows the
`bbox_refactor_run` step list for a representative atom, using the
existing `RefactorRunStep` schema (`#[serde(tag = "op")]`):

- `op: "plan"` for plan-kind steps (`required` is implicit; `optional:
  true` marks soft-fail).
- `op: "command"` for command steps, with `required` / `on_failure` /
  `capture` / `touches`.
- `OnFailure` variants are `required` (default: rollback on failure),
  `optional` (log and continue), `continue_for_repair` (open repair
  obligation). There is **no `rollback` literal** — `required` is the
  rollback-on-failure semantics.

**Schema extensions required for the recipes below:**

- `CaptureSpec::Binlog` — a new variant beside `RustcJson`, with a
  parser using `Microsoft.Build.Logging.StructuredLogger` (or stdout
  scrape fallback). Listed in the file-layout cross-cutting edits.
- No new `OnFailure` variant is needed; `required` covers the
  rollback-on-failure cases.

### Recipe: `csharp-unseal-strangler`

```jsonc
{
  "steps": [
    { "op": "plan",
      "kind": "csharp_partial_class_audit",
      "source": "<class>" },
    { "op": "plan",
      "kind": "csharp_public_api_guard",
      "source": "<class>" },
    { "op": "plan",
      "kind": "unseal_csharp_class",
      "source": "<class>",
      "virtualize_methods": ["<m1>", "<m2>"],
      "acknowledge_subclass_surface_change": true },
    { "op": "command",
      "command": "dotnet",
      "args": ["build", "<project>", "/clp:NoSummary",
               "-bl:diag.binlog"],
      "capture": "binlog",
      "on_failure": "continue_for_repair" },
    { "op": "plan",
      "kind": "csharp_compile_fix_round",
      "source": "<project>",
      "optional": true },
    { "op": "command",
      "command": "dotnet",
      "args": ["test", "<test_project>",
               "--filter", "<class_test_filter>"],
      "on_failure": "required" }
  ]
}
```

### Recipe: `csharp-nullable-coverage-fix`

```jsonc
{
  "steps": [
    { "op": "plan",
      "kind": "csharp_nullable_annotation_repair",
      "source": "<project>",
      "diagnostic_codes": ["CS8618", "CS8625"] },
    { "op": "command",
      "command": "dotnet",
      "args": ["build", "<project>", "/clp:NoSummary",
               "-bl:diag.binlog"],
      "capture": "binlog",
      "on_failure": "continue_for_repair" },
    { "op": "plan",
      "kind": "csharp_compile_fix_round",
      "source": "<project>",
      "max_rounds": 5,
      "optional": true },
    { "op": "command",
      "command": "dotnet",
      "args": ["test", "<test_project>"],
      "on_failure": "required" }
  ]
}
```

### Recipe: `csharp-partial-sg-guard` (analysis-only)

```jsonc
{
  "steps": [
    { "op": "plan",
      "kind": "csharp_partial_class_audit",
      "source": "<scope>" }
  ]
}
```

Analysis-only — no command steps, no validation gate. Output consumed
by the operator before structural moves.

## Plan persistence

Plans persist under the existing shared plans directory used by the
Rust and Java tracks (`plan_slot.rs`), keyed by plan ID. Saved plans
preserve `operator_opt_outs_used`, the `semantic_status` (including
`lsp_verified_partial` for RX-V5 partial loads), and the sidecar
`workspace_load_signature` (a hash over loaded project IDs +
diagnostic fingerprints) so replays can detect workspace drift between
plan and apply.

No storage migration is required for v1: the C# track's plans use the
same on-disk format as Rust/Java plans (kind discriminator +
`RefactorPlan` JSON), so they coexist in the shared directory. A
future per-language partitioning under `csharp/`, `rust/`, `java/`
subdirectories is an optional cleanup, not a v1 prerequisite.

## Sidecar architecture (Phase 2)

The `blackbox-csharp-worker` sidecar speaks JSON-RPC over stdio. Process
shape:

- One sidecar per `(workspace_root, solution_path)` pair, managed by a
  new `RoslynSessionManager` parallel to `LspSessionManager` (the two
  cannot share a session pool because the sidecar protocol is not
  standard LSP).
- Cold start: `MSBuildWorkspace.Create()`, `OpenSolutionAsync(path)` or
  `OpenProjectAsync(path)`. Register `WorkspaceFailedHandler` to capture
  load failures (RX-V5).
- Idle eviction: same `BLACKBOX_LSP_IDLE_SECS` knob (or a sibling
  `BLACKBOX_ROSLYN_IDLE_SECS`).
- Cold-start cap: `BLACKBOX_ROSLYN_INIT_TIMEOUT_SECS`, parallel to
  `BLACKBOX_RUST_ANALYZER_INIT_TIMEOUT_SECS` and
  `BLACKBOX_JDTLS_INIT_TIMEOUT_SECS`.
- Binary path: `BLACKBOX_ROSLYN_WORKER_BIN`.

Request surface (initial):

- `loadSolution(path)` / `loadProject(path)` → `{ projects: [...],
  dropped: [...] }` (RX-V5 evidence).
- `getSymbol(file, line, character)` → qualified symbol name + kind.
- `findReferences(symbol)` → file/line list.
- `renameSymbol(file, line, character, newName)` → `WorkspaceEdit`-style
  diff.
- `getOperations(file, methodName)` → flattened `IOperation` tree, used
  by `csharp_awaited_query_in_loop_audit`, `csharp_compile_fix_round`,
  and any future `csharp_ef_hoist_query`.
- `updateDocumentText(file, content)` → in-memory snapshot update
  (RoslynAdapter.cs:135–142).
- `getDiagnostics(file?)` → file-scoped or solution-wide diagnostic list.

`updateDocumentText` is the load-bearing primitive for the compound-run
integration below.

## Compound runs

`bbox_refactor_run` is language-agnostic (`src/refactor/mod.rs:1703+`).
The C# integration adds three things: a tiered diagnostic story, a
run-scoped sidecar transaction model, and an MSBuild fallback.

### Diagnostic coverage matrix (load-bearing — do not skip)

The Rust track has one diagnostic source: `cargo check
--message-format=json`. The C# track has several, each with different
coverage and cost. Picking the wrong one silently drops failures.

The key distinction the matrix below makes explicit: **compiler
diagnostics arising from generator-produced syntax trees** are visible
to `SemanticModel.GetDiagnostics` and `Compilation.GetDiagnostics`
(because the generated trees are added to the compilation by the
generator driver), but **diagnostics that generators themselves
report** (descriptor IDs like DAY0001 emitted via
`SourceProductionContext.ReportDiagnostic`) live on
`GeneratorDriverRunResult.Diagnostics` and are only included in
`GetRunResult().Diagnostics`, not in the host compilation. The sidecar
`getDiagnostics()` API must merge both streams explicitly.

| Source | Compiler errors on user code | Compiler errors on generator-emitted syntax | Diagnostics generators reported | Analyzer rules (Roslynator, IDisposableAnalyzers, custom DAY*) | MSBuild-target errors | Emit / IL diagnostics | NuGet restore failures | Cost |
|---|---|---|---|---|---|---|---|---|
| `SemanticModel.GetDiagnostics(file)` | ✓ | ✓ if generated tree is the queried file | ✗ (not in this stream) | ✗ | ✗ | ✗ | ✗ | ms (in-memory) |
| `Compilation.GetDiagnostics()` | ✓ | ✓ for all generated trees in the compilation | ✗ (separate stream) | ✗ | ✗ | partial (declaration-time) | ✗ | sub-s on warm workspace |
| `GeneratorDriverRunResult.Diagnostics` | ✗ | ✗ | ✓ | ✗ | ✗ | ✗ | ✗ | already paid by driver |
| `CompilationWithAnalyzers.GetAllDiagnosticsAsync(...)` | ✓ | ✓ | ✗ | ✓ | ✗ | partial | ✗ | seconds (analyzer-dependent) |
| `dotnet build -bl:diag.binlog` | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | seconds–minutes (subprocess + MSBuild) |

The sidecar's `getDiagnostics(file?)` MUST return the merged set:
`Compilation.GetDiagnostics()` (or the file-scoped equivalent) ∪
`GeneratorDriverRunResult.Diagnostics` filtered by file. The merge is
the only correct shape — exposing either stream in isolation silently
drops one half. When `include_analyzers=true` is requested, the sidecar
adds `CompilationWithAnalyzers.GetAllDiagnosticsAsync` at a higher
cost.

Two implications:

1. **Sidecar `getDiagnostics()` is not equivalent to `dotnet build`.** It
   is **fast preflight** — fine for the inner repair loop of
   `csharp_compile_fix_round` and for plan dry-run validation. It is
   **not** sufficient as the final validation gate.
2. **The required validation gate for an applied compound run is
   `dotnet build`.** The post-apply `validations` array on every
   `lsp_verified` plan kind defaults to `dotnet build` (analyzers on,
   `-warnaserror` policy per project). Operators may downgrade to
   `dotnet build --no-restore` or skip via explicit `skip_validation:
   true`, but the default is the strict gate.

Implementation guidance per layer:

- **Inner repair loop** (`csharp_compile_fix_round`): sidecar merged
  diagnostics (compiler + generator-reported). Add
  `CompilationWithAnalyzers` only when an analyzer-attributable code
  (e.g. IDisposableAnalyzers `IDISP*`) is the operator's target.
- **Atom validation step**: invoke `dotnet build` on the touched
  projects, parse the binlog via
  `Microsoft.Build.Logging.StructuredLogger`, surface MSBuild + emit +
  restore diagnostics that sidecar Roslyn cannot see.
- **Plan dry-run validation** (the `validate_plan_shape` step before
  apply): sidecar merged diagnostics only — this gates the plan being
  shown to the operator, not the post-apply commit.

### Run-scoped sidecar transaction

The sidecar holds an open `MSBuildWorkspace` for the duration of a
compound run. Disk and in-memory workspace must stay coherent across
every plan step, not just rollback.

#### Sidecar protocol surface

- `beginTransaction(run_id)` — snapshots the current `Solution` (cheap;
  `Solution` is immutable, sidecar holds the reference).
- `applyPlanStep(run_id, delta)` — `delta` mirrors the
  `AppliedPlanDelta` struct passed by the runner: `edits[]`,
  `file_moves[]`, `created[]`, `deleted[]`. The sidecar applies each
  entry via `Solution.WithDocumentText` (existing files),
  `Solution.AddDocument` (created files), `Solution.RemoveDocument`
  (deleted files), or `Solution.WithDocumentFilePath` (moved/renamed
  files). Returns the new diagnostic delta.
- `applyCommandTouches(run_id, touches[])` — for `Command` steps with
  declared `touches`, re-reads the named files from disk and updates
  the sidecar to match. Without this, a `dotnet format` step or a
  generated-file edit would leave the sidecar with stale text.
- `commitTransaction(run_id)` — drops the snapshot reference; the
  current `Solution` becomes the new baseline.
- `rollbackTransaction(run_id)` — restores the snapshot `Solution`
  reference.

The transaction protocol is single-threaded per `(workspace_root,
solution_path)` and re-entrant per `run_id` — concurrent runs on the
same workspace serialize.

#### Integration with the language-agnostic runner

`src/refactor/mod.rs` is language-agnostic and has no language-scoped
sidecar selector. The integration is an **optional adapter trait**
implemented per language, looked up by `RefactorPlanParams.kind`'s
prefix (`rust_*`, `java_*`, `csharp_*`) and the resolved project:

```rust
// new in src/refactor/mod.rs (sketch)

/// Full diff that an applied plan step produces, mirrored to the adapter.
pub struct AppliedPlanDelta<'a> {
    pub edits: &'a [FileEdit],
    pub file_moves: &'a [FileMove],          // from RefactorPlan.file_moves
    pub created: &'a [PathBuf],              // new files from edits/writes
    pub deleted: &'a [PathBuf],              // files removed by the plan
}

/// Mirror set for a Command step whose touches list mutated on disk.
/// `succeeded=false` is set when the command failed-but-continued
/// (`optional` or `continue_for_repair`) so the sidecar still mirrors
/// the touched files; only a rollback path skips this call.
pub struct AppliedCommandTouches<'a> {
    pub touches: &'a [PathBuf],
    pub succeeded: bool,
}

pub trait WorkspaceTransactionAdapter: Send + Sync {
    fn begin(&self, run_id: &str, project: &Path) -> Result<()>;
    fn apply_plan_step(&self, run_id: &str, delta: &AppliedPlanDelta<'_>) -> Result<()>;
    fn apply_command_touches(&self, run_id: &str, touches: &AppliedCommandTouches<'_>) -> Result<()>;
    fn commit(&self, run_id: &str) -> Result<()>;
    fn rollback(&self, run_id: &str) -> Result<()>;
}

fn resolve_workspace_adapter(steps: &[RefactorRunStep], project: &Path)
    -> Option<Arc<dyn WorkspaceTransactionAdapter>>;
```

`AppliedPlanDelta` carries `file_moves` (from
`RefactorPlan.file_moves`), created paths, and deleted paths in
addition to in-file `edits`. Without these, move/rename plan kinds
(`csharp_lsp_move_item`, `move_csharp_type_to_file`) would mutate
disk but the sidecar's in-memory `Solution` would still hold the old
document IDs at the old paths. The Roslyn adapter maps each entry to
`Solution.AddDocument` (created), `Solution.RemoveDocument` (deleted),
or `Solution.WithDocumentFilePath` (moved/renamed).

The Rust and Java tracks return `None` from `resolve_workspace_adapter`
and the runner short-circuits every call — no behavior change for
those tracks.

**Adapter selection across mixed-language step lists.** The runner
pre-scans the full `steps` list before calling `begin`:

1. Collect every `RefactorRunStep::Plan` step's kind prefix.
2. If all prefixes resolve to the same language adapter (or all
   resolve to `None`), use it.
3. If multiple incompatible adapters are required (e.g. a mix of
   `csharp_*` and `rust_*` plan kinds in the same run), the runner
   rejects with `error.mixed_workspace_adapters` — compound runs
   cross language boundaries at the operator's risk, not implicitly
   inside a single transaction.
4. If the step list is **command-only** (no Plan steps), the runner
   skips adapter resolution entirely; `apply_command_touches` is
   suppressed because there is no transaction to mirror into.

The C# track returns a `RoslynWorkspaceTransactionAdapter` that wires
through to the sidecar protocol above. The runner calls each method at
the following sites in `src/refactor/mod.rs:1703+`:

- `adapter.begin(run_id, project)` immediately after the adapter pre-scan
  succeeds, before the steps loop.
- `adapter.apply_plan_step(run_id, &delta)` immediately after a
  successful `RefactorRunStep::Plan` write to disk (right after the
  existing apply hash-check passes). `delta` is built from the applied
  `RefactorPlan` — `edits`, `file_moves`, derived `created`/`deleted`.
- `adapter.apply_command_touches(run_id, &touches)` after every
  `RefactorRunStep::Command` with a non-empty `touches` array whose
  outcome was **not a rollback**. That includes:
  - Successful commands.
  - `optional` commands that failed but did not roll back.
  - `continue_for_repair` commands that failed but kept the obligation
    open.
  The mirror is required because such commands may have mutated the
  touched files before failing; without it the sidecar holds stale
  text. The `succeeded` flag is forwarded so the adapter can log the
  failure context.
- `adapter.commit(run_id)` on the success exit path.
- `adapter.rollback(run_id)` on **every** rollback path. The runner has
  multiple rollback exits (required-command failure,
  continue_for_repair obligation left open at end, plan-shape
  validation failure mid-stream); each must call `adapter.rollback`
  before returning.

#### Rollback ordering (mandatory)

When a rollback path fires, the runner MUST execute in this order:

1. Restore disk from the in-memory snapshot stack
   (`restore_snapshots_from` at `src/refactor/mod.rs:1774`). Disk is
   the canonical state — restore it first so any observer (the
   sidecar, the operator, a tail-running editor) sees consistent
   content.
2. Call `adapter.rollback(run_id)`. The sidecar restores its snapshot
   `Solution`, which now matches the just-restored disk.

If the sidecar has crashed mid-transaction (RPC fails), the runner MUST
NOT skip the disk rollback — disk consistency is the load-bearing
property. The sidecar can be re-loaded lazily on the next request via
`loadSolution`; its in-memory drift is recoverable, a corrupted disk is
not.

This ordering is the inverse of apply (where the disk write happens
first, then the sidecar `applyPlanStep` mirrors it). Documented here
because the symmetry temptation is to roll back the sidecar first; that
is wrong.

### MSBuild fallback when the sidecar is unavailable

Phase 1 ships without the sidecar, and sidecar crash / operator
opt-out scenarios need a viable fallback. In those cases the C#
compound-run path collapses to a `dotnet build` shell-out per round —
slower, out-of-process, no in-memory snapshot, but compatible with the
Rust pattern. Gated by `csharp_use_dotnet_build_diagnostics=true` or
implied when no sidecar is registered for the workspace.

Binlog parsing uses `Microsoft.Build.Logging.StructuredLogger`
(operator-installed nuget; fallback to scraping `dotnet build` stdout
when unavailable).

### `.slnx` compatibility note

`MSBuildWorkspace.OpenSolutionAsync` is documented for `.sln` files;
`.slnx` (the XML-formatted SDK-style solution Daystrom uses) requires a
runtime check. If the sidecar's load attempt fails for `.slnx`, the
plan responds with `error.slnx_unsupported_by_workspace` and the
operator can either:

1. Run the build fallback path (`dotnet build` handles `.slnx`
   natively).
2. Point the sidecar at the root `csproj` via `OpenProjectAsync`
   instead — loses cross-project resolution but works.

The audit kind `csharp_workspace_probe` (analysis-only,
`syntax_only`) reports which path applies for a given workspace and is
the recommended first call when bootstrapping the C# track in a new
repo.

## Atoms

Brofile: `csharp-refactor-persona` — denies `Write`, `Edit`, `Bash`,
`bro_*`, `bbox_learn`; allows `bbox_refactor_*`, `bbox_code_*`,
`bbox_inspect_entity`, `bbox_hybrid_search`, `bbox_find_paths`,
`bbox_note`. Mirrors `rust-refactor-persona` shape.

Initial atom catalog (parallel to Rust's 7-atom Batch 1 and Java's
single-atom appendix). Each entry classifies its motivation: **daystrom
pain** (addresses a quantified pain in the reference codebase) vs
**parity** (mirrors a Rust/Java capability for cross-language symmetry).

1. **`csharp-partial-sg-guard`** *(daystrom pain — protects 36 partial
   types, including the GraphPredicateGenerator-bound subset and
   Wolverine package-generator-bound partials)*. Wraps
   `csharp_partial_class_audit`. Analysis-only. **Phase 2** (the audit
   depends on the sidecar for generator enumeration + fingerprinting +
   RX-V4 manifest enforcement; the LSP surface cannot enumerate
   `AnalyzerReference` generators).
2. **`csharp-unseal-strangler`** *(daystrom pain — 278 sealed files,
   386 declarations, CLAUDE.md:306–312 directive)*. One class per
   invocation. Wraps `unseal_csharp_class`. Requires
   operator-supplied `acknowledge_subclass_surface_change`.
3. **`csharp-nullable-coverage-fix`** *(daystrom pain — AUDIT.md
   T1-010 / T2-024 sites)*. Wraps `csharp_nullable_annotation_repair`
   + `csharp_compile_fix_round`. Bounded to one project per
   invocation.
4. **`csharp-awaited-query-audit`** *(daystrom pain — surfaces the
   awaited-loop pattern in graph services for operator review,
   without attempting a rewrite)*. Analysis-only. Wraps
   `csharp_awaited_query_in_loop_audit`.
5. **`csharp-filescoped-batch`** *(daystrom pain — mixed namespace
   styles)*. Wraps `migrate_csharp_to_filescoped_namespace`.
   Codebase-wide, syntax-only. **Phase 1.**
6. **`csharp-using-organize`** *(parity)*. Wraps
   `csharp_organize_usings`. Folder-scoped. **Phase 1.**
7. **`csharp-record-modernize`** *(parity — lombokify analog)*. Wraps
   `csharp_to_record_migrate`. Operator names one class. EF-entity
   guard active.
8. **`csharp-primary-ctor-migrate`** *(parity)*. Wraps
   `csharp_primary_ctor_migrate`. DI-heavy classes only.

**Atom-vs-plan-kind coverage gap (acknowledged).** Seventeen plan
kinds, eight atoms. The uncovered nine are:

- Pure analysis (no atom needed; called directly or as preconditions
  within other atoms): `find_csharp_usages`, `csharp_public_api_guard`,
  `csharp_workspace_probe`.
- Move/migrate primitives that compose under operator control rather
  than as atomic agents (`move_csharp_type_to_file`,
  `move_csharp_members_to_partial`, `migrate_csharp_type_usages`,
  `csharp_lsp_move_item`): atom candidates for Batch 2 once Phase 1
  proves the dispatch path.
- `csharp_lsp_rename` and `csharp_async_dispose_convert`: parity atoms
  for Batch 2 — useful but not daystrom-killers.
- `csharp_compile_fix_round`: consumed *inside* other atoms (recipe
  above), not exposed as a standalone atom — same pattern as
  `rust_compile_fix_round` in the Rust track.

The Java track ships 1 atom over 20 plan kinds; the Rust track ships
7 atoms over ~57 plan kinds. The C# Batch 1 ratio (8 over 17) is
deliberately denser because the Daystrom pain is well-quantified and
several daystrom-specific atoms (partial-sg-guard,
unseal-strangler, nullable-coverage-fix, awaited-query-audit) have no
direct Rust/Java analog to inherit from.

Atom composition (`composition.chainable_after`, `parallel_safe`) follows
the v1 advisory pattern from `../refactor-agents.md`. Runtime composition
awaits the same `bro_agent_compose` work as Rust/Java atoms.

## File layout

```
src/refactor/
  csharp.rs                              (entry, dispatcher arm)
  csharp/
    awaited_query_audit.rs               (csharp_awaited_query_in_loop_audit)
    ef_hoist_query.rs                    (csharp_ef_hoist_query — v2)
    unseal.rs                            (unseal_csharp_class)
    partial_audit.rs                     (csharp_partial_class_audit)
    nullable_repair.rs                   (csharp_nullable_annotation_repair)
    lsp_rename.rs                        (csharp_lsp_rename)
    organize_usings.rs                   (csharp_organize_usings)
    lsp_move_item.rs                     (csharp_lsp_move_item)
    move_members_partial.rs              (move_csharp_members_to_partial)
    move_type_to_file.rs                 (move_csharp_type_to_file)
    migrate_type_usages.rs               (migrate_csharp_type_usages)
    find_usages.rs                       (find_csharp_usages)
    public_api_guard.rs                  (csharp_public_api_guard)
    workspace_probe.rs                   (csharp_workspace_probe)
    filescoped_namespace.rs              (migrate_csharp_to_filescoped_namespace)
    record_migrate.rs                    (csharp_to_record_migrate)
    primary_ctor.rs                      (csharp_primary_ctor_migrate)
    async_dispose.rs                     (csharp_async_dispose_convert)
    compile_fix.rs                       (csharp_compile_fix_round)

src/lsp/
  csharp.rs                              (Phase 1: LSP init params for
                                          Microsoft.CodeAnalysis.LanguageServer)
  csharp_sidecar.rs                      (Phase 2: RoslynSessionManager)

system-defaults/agents/csharp/
  csharp-partial-sg-guard.json
  csharp-unseal-strangler.json
  csharp-nullable-coverage-fix.json
  csharp-awaited-query-audit.json
  csharp-filescoped-batch.json
  csharp-using-organize.json
  csharp-record-modernize.json
  csharp-primary-ctor-migrate.json

system-defaults/brofiles/
  csharp-refactor-persona.json
```

Cross-cutting edits:

- `src/refactor/mod.rs` — add `"csharp_*"` arms to the plan-kind
  dispatcher (around the existing rust/java arms at lines 1146–1216).
- `src/refactor/mod.rs` — introduce `WorkspaceTransactionAdapter`
  trait + `resolve_workspace_adapter` lookup; wire the adapter calls
  into the runner at the documented sites (begin / apply_plan_step /
  apply_command_touches / commit / rollback). Rust and Java return
  `None` from the resolver, no behavior change.
- `src/refactor/mod.rs` — extend `SemanticStatus` enum with
  `LspVerifiedPartial` variant; update serde rename + tool_docs.
- `src/refactor/mod.rs` — extend `CaptureSpec` enum with `Binlog`
  variant; add binlog parser branch in the capture switch (currently
  cargo-only at line 2232).
- `src/refactor/mod.rs` — extend `enforce_agent_command_allowlist`
  with a dotnet allowlist branch keyed on `dispatch_origin=agent` per
  RX-V2 extension.
- `src/tool_docs.rs` — add C# stanzas under the Refactor category;
  document `LspVerifiedPartial` semantics.
- `src/projects.rs` (or wherever `Language` is defined) — add
  `Language::CSharp`.
- `CLAUDE.md` — add RX-V4 and RX-V5 invariants alongside RX-V1/V2/V3.
- `design/refactor-tools/refactor-tools.md` — link the new cluster.

## Open questions

1. **Sidecar binary distribution.** A dotnet sidecar is a real
   build/release artifact. Bundled with the daemon, downloaded on demand,
   or operator-supplied via env var (same pattern as `BLACKBOX_JDTLS_BIN`
   / `BLACKBOX_RUST_ANALYZER_BIN`)? Recommend env-var + optional bundled
   binary in `deploy/`.
2. **MSBuild on Linux gitignore quirk.** Per project CLAUDE.md, Roslyn's
   MSBuild host creates directories with literal backslashes on Linux
   (`bin\Debug/`). The sidecar should warn (not error) if the workspace
   root has these without matching gitignore entries.
3. **Source generator analysis depth.** RX-V4 v1 statically classifies
   `ForAttributeWithMetadataName(...)` pipelines and requires
   operator-declared `generator_inputs` manifests for any generator
   that uses raw `SyntaxProvider.CreateSyntaxProvider` or
   `register_post_initialization` shapes (mandatory in v1, not
   optional). v2 may add a heuristic walk of raw predicates to
   recover automatic discovery and reduce manifest burden.
4. **EF Core version coupling.** `csharp_ef_hoist_query` (deferred)
   keys on `EntityFrameworkQueryableExtensions` extension methods. EF
   Core 9 vs EF Core 10 may rename or restructure these. Detection
   should be type-symbol-based (containing assembly), not name-based.
5. **Wolverine / MediatR handler binding guard.** Wolverine v5 binds
   handlers by convention (class name, method name). Renaming a handler
   silently detaches it from the message bus. The daystrom Wolverine
   message-routing generator (which extends 20+ partials in
   `Daystrom.Contracts/Messages/`) is exactly the case RX-V4 should
   protect, but its discovery shape is generator-internal (Wolverine v5
   does not use `[Attribute]` markers for handler discovery — it scans
   types by interface implementation and method-name conventions).
   Warrants a future `csharp_wolverine_handler_audit` plan kind paired
   with an RX-V4 extension covering convention-based generators. Out of
   scope for v1.
6. **gRPC / protobuf contract evolution.** Daystrom uses
   `protobuf-net.Grpc` for service contracts. Renaming a contract
   method or changing its signature breaks wire compatibility.
   `csharp_grpc_contract_audit` (warn on contract-attributed method
   moves/renames) is a v2 candidate parallel to RX-V4.
7. **`.slnx` MSBuildWorkspace compatibility.** Documented in Compound
   Runs above. The `csharp_workspace_probe` kind reports the path; the
   long-term fix is upstream Roslyn support.

## Picking the next one

Phase 1 boot: `csharp_workspace_probe`. Smallest possible LSP-backed
plan kind — analysis-only, exercises sidecar/LSP launch, session pool,
RX-V5 expected-vs-loaded comparison, and `.slnx` compatibility
detection. Proves the bootstrap without touching any file. Does **not**
exercise the transaction protocol (no edits, no commits) — that's the
next step.

Phase 1 write smoke: `csharp_lsp_rename`. The smallest plan kind that
actually writes — exercises `WorkspaceEdit` → `FileEdit` conversion,
the apply path, hash check, and disk-snapshot rollback (induced
failure). The `WorkspaceTransactionAdapter` is **not** wired in Phase 1
(no sidecar yet) — `resolve_workspace_adapter` returns `None` for C# in
Phase 1, exactly as it does for Rust/Java. The adapter smoke is a
Phase 2 milestone.

Phase 2 transaction smoke: re-run `csharp_lsp_rename` against the live
sidecar to exercise `apply_plan_step` (with `file_moves` for a
follow-up rename that moves a file), `apply_command_touches`,
`commit`, and `rollback`. This is the validation gate for the
adapter design before more complex kinds land.

Phase 2 entry point: `csharp_partial_class_audit`. Forcing function
for the sidecar architecture (custom plan JSON + generator-input
inspection + fingerprinting + RX-V4 manifest enforcement).
Daystrom-relevant from day one because 36 partial-type files exist and
most are Wolverine-bound, the other minority
`[GraphPredicateAttribute]`-bound.

Atom entry point: `csharp-using-organize` (Phase 1) or
`csharp-partial-sg-guard` (Phase 2). Phase 1 atom proves the brofile,
manifest, dispatch, and grounding protocol against the LSP surface;
Phase 2 atom proves the full sidecar + manifest + RX-V4 enumeration
stack. `csharp-partial-sg-guard` is pure analysis, no edits, no
operator-authority flags, so it's safe to ship as the first
sidecar-touching atom.

**`csharp_ef_hoist_query` is explicitly excluded from Batch 1 atoms.**
The rewrite is too risky to ship without an EF-using reference
codebase. The audit kind (`csharp_awaited_query_in_loop_audit`) ships
in v1; the rewrite waits on real ground truth.

## Changelog

- **2026-05-15 r1** — initial draft.
- **2026-05-15 r4.1** — round-4 codex converge (deepseek converged at r2).
  Non-blocking cleanups: Phase 1 vs Phase 2 transaction-smoke wording
  separated (Phase 1 smokes WorkspaceEdit→FileEdit→apply with no
  adapter; Phase 2 re-smokes with sidecar adapter). Sidecar protocol
  bullet now uses `AppliedPlanDelta` consistently. `lsp_verified_partial`
  broadened to cover both RX-V4 and RX-V5 triggers with a
  `semantic_caveats` audit field. `csharp_partial_class_audit` text
  aligned with the three-source enumeration including
  `unknown_external_generators` payload. RX-V4 refusal error broadened
  to include unknown-package case with `source` discriminator.

- **2026-05-15 r4** — round-3 codex iterate (deepseek converged):
  `AppliedPlanDelta` now carries `file_moves` + `created` + `deleted`
  alongside `edits` so move/rename plan kinds keep the sidecar
  coherent; `AppliedCommandTouches` carries a `succeeded` flag and
  the runner mirrors touches on any non-rollback outcome (including
  `optional` / `continue_for_repair` failures). Adapter selection
  is now pre-scan based with `error.mixed_workspace_adapters`
  refusal for cross-language step lists and skip for command-only
  runs. RX-V4 enumeration extended to three sources: in-repo
  `*.SourceGenerators/`, project `AnalyzerReference`s (catches
  package-shipped generators like Wolverine v5 / MediatR /
  Mapperly), and a known-package registry. Wolverine claim in the
  Problem section is now correctly attributed to a package
  generator. Stale backend-choice bullet on
  `Compilation.GetDiagnostics` coverage corrected.
  `csharp-partial-sg-guard` reclassified Phase 2 (requires sidecar
  + analyzer-reference enumeration). Atom filename `csharp-loop-query-hoist.json`
  removed from file layout (replaced with `csharp-awaited-query-audit.json`).
  Open Question #3 tightened: manifest is mandatory in v1, not optional.

- **2026-05-15 r3** — round-2 codex iterate + deepseek converge.
  Hardened the diagnostic-coverage matrix to make the compiler-vs-generator
  diagnostic split explicit and require the sidecar's
  `getDiagnostics()` to merge `Compilation.GetDiagnostics()` with
  `GeneratorDriverRunResult.Diagnostics`. Reworked the run-scoped
  transaction integration as an optional `WorkspaceTransactionAdapter`
  trait with explicit call sites in `src/refactor/mod.rs`; Rust and
  Java tracks no-op. Specified rollback ordering (disk first, then
  sidecar). Hardened RX-V4 manifest opt-out: mandatory generator
  enumeration + SHA-256 fingerprinting, refusal on undeclared /
  fingerprint-mismatched generators, downgrades plan to
  `lsp_verified_partial` when raw-classification generators are in
  scope. Rewrote compound-run recipes against the actual
  `RefactorRunStep` schema (`op:` tag, valid `OnFailure` variants).
  Added `LspVerifiedPartial` to the SemanticStatus enum extension.
  Listed required schema/code changes in cross-cutting edits:
  `CaptureSpec::Binlog` variant + parser, `OnFailure` left
  unchanged (no rollback literal needed), allowlist dotnet branch.
  Corrected partial-type count to 36 (any-modifier regex) with
  filter caveat. Fixed remaining stale evidence in unseal kind
  (301 → 278/386, 288–292 → 306–312). Phase 1 reordered:
  `csharp_workspace_probe` first (bootstrap), then `csharp_lsp_rename`
  as the transaction smoke test. `csharp_ef_hoist_query` explicitly
  excluded from Batch 1 atoms.

- **2026-05-15 r2** — round-1 deepseek + codex review convergence:
  recalibrated daystrom evidence (278 sealed files / 386 declarations,
  30 partial-type files, 30 csproj projects, 0 EF Core usage —
  previous counts were stale or grep-noise-inflated); split
  `csharp_hoist_loop_query` into analysis-only
  `csharp_awaited_query_in_loop_audit` (ships v1) and deferred
  `csharp_ef_hoist_query` (correct EF mechanics via
  `EntityFrameworkQueryableExtensions`, tracking/cancellation guards);
  rewrote RX-V4 to scan attributes at type/method/member level and
  trace to containing-type partials (the
  `GraphPredicateAttribute` shape); rewrote compound-runs section with
  a diagnostic-coverage matrix and `dotnet build` as the required
  validation gate; added run-scoped sidecar transaction model with
  `applyPlanStep` / `applyCommandTouches` / commit / rollback;
  tightened RX-V5 to expected-vs-loaded comparison with audit field
  and `lsp_verified_partial` downgrade; added Safety Rules and
  Compound-Run Recipes sections; added `csharp_workspace_probe` kind;
  per-constructor decision and EF-entity guard on record migrate;
  classified atoms as daystrom-pain vs parity; added open questions
  for Wolverine handler audit and gRPC contract evolution. Fixed
  CLAUDE.md and RoslynAdapter.cs line references.
