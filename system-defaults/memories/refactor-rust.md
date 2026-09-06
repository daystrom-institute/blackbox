---
tags:
  - refactor-tools
  - rust
---
+++
title = "Rust refactor mechanization - isolate transforms, rust-analyzer, and compiler repair"
tags = ["refactor", "refactoring", "mechanization", "restructure", "rust", "rs", "tree-sitter", "rust-analyzer", "code-mode", "isolate", "rust.describe", "rust.fixRound", "rust.extractItems", "rust.extractImplMethods", "rust.extractTrait", "rust.moveStructFields", "rust.migrateErrorType", "build.gate", "lsp.references", "lsp.assist", "lsp.definition", "edits.apply"]
order = 8
template = false
+++
# Rust Refactor Mechanization Runbook

The daemon refactor MCP surface is retired. `bbox_refactor_*` and `bbox_code_*`
spellings below identify historical engine operations, not callable MCP tools.
Use the current harness catalog (`isolate --list`, then `isolate --describe <tool>`)
for exact native names and schemas. Compose operations in the caller; atom and
workflow wrappers are retired. Plan kinds and safety invariants below remain
reference material where the native binding uses that engine.

Use this memory before moving, extracting, renaming, splitting, or migrating
Rust code with the harness-native isolate bindings.

The retired daemon refactor MCP surface is not part of this workflow. Do not
call `bbox_refactor_status`, `bbox_refactor_plan`, `bbox_refactor_apply`,
`bbox_refactor_run`, or old snake-case Rust plan kinds. Rust refactoring runs
inside code-mode cells through `code.*`, `analysis.*`, `rust.*`, `lsp.*`,
`edits.*`, and `build.gate`.


## Trust model

The isolate has one write path:

1. Facts and analyses return values and hash-anchored spans.
2. Rust and LSP transforms return changes, creates, findings, and leftovers.
3. `edits.begin`, `edits.merge`, `edits.createFile`, and related operations
   assemble an EditSet.
4. `edits.apply` is the only binding that writes.

`edits.apply` validates hashes and parses touched files before committing the
write. A bounce returns repairable findings such as `stale_span`,
`create_exists`, `invalid_edits`, or `parse_error_after_apply`. Repair the
reported condition, re-derive fresh facts, and build a new EditSet.

Every successful apply invalidates every older span for each touched file.
After an edit, re-run `code.items`, `code.query`, `code.readLines`, or another
fact-producing call before aiming a later edit or LSP request. `lsp.*` spans
are position-sensitive as well as hash-anchored. Use fresh `code.*` facts and
`code.read` on the fresh span before a later semantic request.

Transforms are not idempotent over their own output. A target-exists refusal
after a successful extraction is normally the DONE signal, not a reason to
delete the target and retry.

## Provenance tiers

Provenance is computed by the host from edit lineage. A cell cannot claim or
upgrade it.

- `syntax_only`: Rust planner changes, classifier-synthesized fixes, and any
  cell-authored replacement bytes.
- `compiler_suggested`: a `rust.fixRound` change whose span and replacement
  are passed through verbatim from a rustc or Clippy
  `MachineApplicable` suggestion.
- `lsp_verified`: unmodified WorkspaceEdits from rust-analyzer through
  `lsp.rename`, `lsp.assist`, or another LSP authority.

`edits.apply` reports the weakest tier in the EditSet. A compiler or LSP tier
describes authorship, not final correctness. The terminal compiler gate remains
the outcome authority.

## The canonical fix loop

Every structural Rust transform follows the same compile-repair loop:

```text
rust.<transform>
  -> edits.merge / edits.createFile
  -> edits.apply
  -> build.gate("cargo check --message-format=json")
  -> rust.fixRound(diagnostics)
  -> edits.merge
  -> edits.apply
  -> build.gate again
```

Stop when the build succeeds, when `rust.fixRound` returns no changes, or after
about five repair rounds. The hard cap prevents an agent from oscillating on
diagnostics it cannot reason about.

`rust.fixRound` separates two outputs:

- `changes`: mechanical proposals that can be reviewed and merged.
- `leftovers`: the manual punch list. Borrow-checker, trait-bound, move, and
  unclassified failures belong here and must not be retried blindly.

Load-bearing cell shape:

```ts
const transformed = await rust.extractItems(args);
const es = await edits.begin();
await edits.merge({ es, changes: transformed.changes });
for (const created of transformed.creates) {
  await edits.createFile({
    es,
    path: created.path,
    content: created.content
  });
}
const applied = await edits.apply({ es });

let gate = await build.gate({
  command: "cargo check --message-format=json",
  anchor_spans: true
});

for (let roundNumber = 0; !gate.ok && roundNumber < 5; roundNumber++) {
  const round = await rust.fixRound({ diagnostics: gate.diagnostics });
  if (!round.changes.length) {
    text(JSON.stringify({ applied, gate, leftovers: round.leftovers }, null, 2));
    break;
  }

  const repair = await edits.begin();
  await edits.merge({
    es: repair,
    changes: round.changes.map(change => ({
      span: change.span,
      new_text: change.new_text
    }))
  });
  await edits.apply({ es: repair });
  gate = await build.gate({
    command: "cargo check --message-format=json",
    anchor_spans: true
  });
}
```

Use the project's real check command and feature flags. `cargo check
--message-format=json` is the normal repair gate because `build.gate` can turn
its diagnostics and suggestions into bounded structured values.

## Runtime contracts

Call `rust.describe({ transform: "<name>" })` before the first use of a Rust
transform. The namespace description is intentionally compact; the describe
contract is the authoritative parameter, return, refusal, and composition
reference.

Use `analysis.describe({ analysis: "<name>", language: "rust" })` for Rust
analysis contracts.

The live Rust transform set is:

- `rust.fixRound`
- `rust.extractItems`
- `rust.inlineModToFile`
- `rust.moduleWiring`
- `rust.setVisibility`
- `rust.extractImplMethods`
- `rust.rewriteModuleCallers`
- `rust.organizeImports`
- `rust.moveStructFields`
- `rust.updateCallers`
- `rust.extractTrait`
- `rust.liftToFree`
- `rust.migrateErrorType`
- `rust.migrateTypeUsages`

`rust.describe` provides the depth-on-demand contracts for all of them.

## Survey before synthesis

Use syntax facts for edit addresses and host-side analyses for structural
decisions:

- `code.files({ language: "rust" })`: enumerate the working set.
- `code.items({ file })` or `code.items({ files })`: top-level items, impl
  methods, visibility, attributes, and anchored spans.
- `code.signature({ span })`: callable parameters, return type, generics, and
  qualifiers.
- `code.query`: narrow tree-sitter facts. Avoid broad repository sweeps that
  materialize large capture sets.
- `analysis.topLevelDeps({ file, projectDir? })`: dependency graph, external
  references, and suggested clusters before top-level extraction.
- `analysis.implPartition({ file, implName? })`: impl-method call and state
  graph before splitting a large impl.
- `analysis.references({ symbols, language: "rust" })`: compact syntactic
  reference counts when only blast radius is needed.
- `lsp.references({ span })`: authoritative project-wide usages when exact
  semantic locations matter.
- `lsp.definition({ span })`: goto-definition for semantic classification.
- `lsp.assist({ span })`: list rust-analyzer code actions; select one in a
  second call and merge its returned changes without rewriting them.

LSP calls fail closed when rust-analyzer is unavailable. Do not silently
downgrade an LSP-backed operation to text matching.

## Move and extraction transforms

### `rust.extractItems`

Use for top-level structs, enums, functions, constants, statics, traits, or
type aliases.

Default compound mode creates or appends the child module, adds the `mod`
declaration, widens moved items and fields as needed, and adds an auto-pruned
source-side `use`. `withLocalDeps` moves the private dependency closure.
`section` selects items by markers or line bounds. Host dependency analysis
always runs; there is no deep-analysis toggle.

Run `analysis.topLevelDeps` first when the item boundary is not already known.
Use `previewOnly` for a risky boundary. Apply the returned `changes` and
`creates` together, then enter the fix loop.

### `rust.extractImplMethods`

Use for moving named methods out of one impl block. It preserves attributes
and qualifiers, rebases `super::` paths for a child module, appends to a
matching target impl when present, and can widen private methods still used
from the parent.

Run `analysis.implPartition` first. Pass `impl_name` when the method name is
ambiguous across impl blocks. The transform does not generate project-specific
router wrappers.

### `rust.inlineModToFile`

Use for `mod foo { ... }` to file-module extraction. It preserves outer
attributes such as `#[cfg(test)]`, derives the normal Rust 2018 target path, and
refuses a non-empty target.

### `rust.extractTrait`

Use for moving selected inherent methods into a trait plus
`impl Trait for Type`. Inspect `dyn_compatible`, `object_safety_report`,
`trait_in_scope_required`, and call-site warnings before applying. A selected
method that depends on a private, non-selected inherent helper can refuse with
`extract_trait_orphaned_call`.

The v1 call-site scanners behind these planners recognize UFCS-style
`Type::method(...)` and `<Type as Trait>::method(...)` sites. They do not track
ordinary receiver calls such as `value.method(...)`. Use `lsp.references` and
the compiler gate to cover receiver-call fallout.

### `rust.liftToFree`

Use only for inherent methods that operate on arguments and do not depend on
instance state. Methods with `self.field` access refuse with
`method_lift_refused`. Most methods on state containers are therefore
unliftable. Select calculation, parsing, formatting, or normalization helpers
that use only their arguments.

The transform does not rewrite call sites and searches only the first inherent
impl block in the source. The compiler gate and manual punch list are part of
the operation.

## Module wiring and hygiene

### `rust.moduleWiring`

Performs one conservative action per call:

- `add_mod`
- `remove_mod`
- `add_use`
- `remove_use`

Duplicate additions and missing removals refuse. Use this for explicit
module-graph changes around larger recipes.

### `rust.setVisibility`

Rewrites item, impl-method, or struct-field visibility while preserving
`async`, `unsafe`, and `const`. Prefer visibility already synthesized by
`rust.extractItems` when it matches the desired boundary.

### `rust.rewriteModuleCallers`

Rewrites simple `<old_module>::<item>` prefixes to a new module prefix across
project Rust files. It is bounded and syntax-only. It is not alias-aware and
does not split complex `use` trees.

### `rust.organizeImports`

Only `mode: "minimize"` is live. It converts resolvable local wildcard imports
to explicit names.

Important limitations:

- It is in-project only.
- It assumes file or `mod.rs` module geometry.
- It cannot descend into an inline module declared inside another file module.
  For example, a wildcard targeting an inline child of `providers.rs` is
  unresolvable even when the source is in the same crate.
- `mode: "organize"` is not implemented by this binding. Use rust-analyzer
  assists when the server offers an applicable import action.

Treat an unresolvable wildcard finding as a limitation, not permission to
guess the export set.

## State movement

### `rust.moveStructFields`

Moves named fields from one struct to another and reports remaining source
accessors and inherited generics.

This is an RX-V1 transform. `acknowledge_repr` is operator authority and must
never appear in cell-authored arguments. A source with a non-default `#[repr]`
refuses without the dispatch-side default:

```text
default:rust.moveStructFields.acknowledge_repr=true
```

The binding reads the grant from `ToolArgDefaults`. A consumed grant appears in
`operator_opt_outs_used`.

### `rust.updateCallers`

Run after a field move to rewrite conservative `self.field` reads and
unambiguous calls through a delegate field. Writes, destructures, spreads,
ambiguous calls, and other unsafe cases remain in `unrewriteable_accessors`.
They are the manual punch list.

There is no live Rust transform that constructs an arbitrary delegate field and
all constructor wiring. Build that small structural addition through fresh
`code.*` spans and the `edits.*` algebra, then compile before running caller
rewrites.

## Type and error migration

### `rust.migrateErrorType`

Rewrites selected function error signatures and mapped
`OldError::Variant` construction sites. It reports every `?` site because the
planner cannot prove all conversion behavior.

This is an RX-V1 transform. Public error migrations require the dispatch-side
default:

```text
default:rust.migrateErrorType.acknowledge_public_api_change=true
```

Never add `acknowledge_public_api_change` to the cell input. A refusal names the
dispatch-side grant required. After apply, run the fix loop and treat unresolved
`?` conversions as manual work.

### `rust.migrateTypeUsages`

Migrates supported type positions to one of the contract's replacement kinds.
Unsupported positions are reported as `migration_skipped`.

This transform can change public signatures and always uses the RX-V1
dispatch-side channel:

```text
default:rust.migrateTypeUsages.acknowledge_public_api_change=true
```

The cell cannot author the grant.

## RX-V1 operator authority

`acknowledge_repr` and `acknowledge_public_api_change` arrive only through
dispatch configuration as `ToolArgDefaults`. Atomic agents may consume an
operator-supplied default, but may not:

- add the flag to a cell call;
- infer the grant from a small diff;
- retry a refusal by silently setting the flag;
- default the flag in an atom prompt.

The refusal hint names the exact dispatch-side default. Return that hint to the
operator. `operator_opt_outs_used` is the audit record for a consumed grant.

## Rust-analyzer practice rules

- Spans are exact positions. Re-derive them after every edit.
- Rust-analyzer does not reliably goto-definition from the type name at a
  struct literal site. This is rust-analyzer behavior, not a binding defect.
  Aim at a declaration, import, signature, or another semantically resolved
  occurrence instead.
- On large workspaces, a cold rust-analyzer can exceed the default readiness
  budget. Use a warm checkout/build-data cache or raise `wait_ready_ms`.
- Repeated one-shot isolate invocations discard the in-memory language server.
  Prefer one session or a warm root for a sequence of LSP probes.

## Common recipes

### Monster file split

1. `analysis.topLevelDeps`
2. `rust.extractItems`
3. `edits.apply`
4. compiler fix loop

### God impl split

1. `analysis.implPartition`
2. `rust.extractImplMethods`
3. `rust.moduleWiring` if the target needs explicit graph wiring
4. `edits.apply`
5. compiler fix loop

### Test island extraction

Use `rust.inlineModToFile` for an inline test module. Use
`rust.extractItems` for loose test helpers and test items. Preserve
`#[cfg(test)]` placement and compile the relevant targets.

### File or module move

1. Use `lsp.willRenameFiles` when a physical file rename needs semantic path
   updates.
2. Apply the LSP changes unmodified.
3. Use `rust.moduleWiring` for `mod` and `use` changes.
4. Use `rust.rewriteModuleCallers` for remaining simple module prefixes.
5. Enter the compiler fix loop.

### Trait boundary extraction

1. `analysis.implPartition`
2. `lsp.references` on candidate methods
3. `rust.extractTrait`
4. apply changes and creates
5. add imports identified by `trait_in_scope_required`
6. compiler fix loop

### State extraction

1. `analysis.implPartition`
2. `rust.moveStructFields`
3. apply
4. add delegate field and constructor wiring through anchored `edits.*`
5. `rust.updateCallers`
6. apply
7. compiler fix loop

### Error migration

1. Obtain explicit operator approval for the RX-V1 default when required.
2. `rust.migrateErrorType`
3. apply
4. compiler fix loop
5. resolve `?`-site leftovers manually

## Safety rules

1. Read the transform contract before first use.
2. Ground names and spans with `code.*`; do not invent item names.
3. Use `analysis.*` reductions instead of reconstructing large dependency
   graphs from raw captures.
4. Apply only through `edits.apply`.
5. Re-derive every span after an apply.
6. Keep LSP-produced changes unmodified to preserve `lsp_verified` lineage.
7. Keep compiler `MachineApplicable` changes verbatim to preserve
   `compiler_suggested` lineage.
8. Never cell-author an RX-V1 acknowledgement.
9. Cap the compile-repair loop at about five rounds.
10. Surface `leftovers`, skipped sites, unresolved accessors, and call-site
    warnings. Do not hide them behind a successful partial edit.
11. Inspect the final diff and run the real project compiler gate.
12. Do not use the retired daemon refactor MCP plan/apply surface.
