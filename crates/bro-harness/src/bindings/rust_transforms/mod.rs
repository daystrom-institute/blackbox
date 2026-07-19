//! `rust.*` - the rust transform cell bindings.
//!
//! Design home: `design/refactor-tools/rust/rust-isolate-surface.md`.
//! Trust model + porting recipe: `crates/bro-harness/src/bindings/AGENTS.md`.
//!
//! Directory layout (§7: do not write one giant file): one module per
//! concern. Today only `fix` (the compile-fix loop engine); move/wiring/
//! migrate/crates modules land as the surface grows toward the full ~15
//! transforms the design curates.
//!
//! Every transform follows the `java.*` precedent: thin adapter that NEVER
//! writes, returns `{changes, findings, leftovers}` for `edits.merge`, and
//! records host-authored changes in the session-shared provenance ledger so
//! `edits.apply` can compute `semantic_status` lineage. The namespace index
//! is a compact one-line-per-tool description; full contracts come from
//! `rust.describe` at runtime (values stay in the isolate, no prompt bloat).

mod fix;
mod helpers;
mod impl_extract;
mod imports;
mod r#move;
mod rewrite_callers;
mod wiring;

use std::sync::Arc;

use async_trait::async_trait;
use bro_code_mode::ToolNamespaceDescription;
use bro_tools::{Tool, ToolAnnotations, ToolCx, ToolResult};
use serde_json::{Value, json};

use crate::bindings::ledger::ProvenanceLedger;

/// `rust.describe` - depth-on-demand contract for one rust transform
/// (matches the `analysis.describe` / `java.describe` pattern; the namespace
/// index stays a compact one-liner).
pub struct RustDescribe;

const FIX_ROUND_CONTRACT: &str = r#"rust.fixRound - classify rustc/clippy diagnostics into reviewable edit proposals + explicit leftovers.

WHAT IT DOES
  Takes the `build.gate` diagnostics shape verbatim (the `diagnostics[]` array
  from a `cargo check --message-format=json` gate), OR raw rustc JSON-lines
  via `raw_json`, and ports the v1 `rust_compile_fix_round` classifier:

    - Verbatim `MachineApplicable` `suggested_replacement` spans become
      `{changes}` entries recorded at `compiler_suggested` provenance. These
      are compiler-authored bytes (the compiler asserts they compile) and
      keep their tier through `edits.merge` -> `edits.apply` content-digest
      recognition.
    - Classifier-synthesized proposals (add-use from E0432/E0433/E0282,
      visibility-bump from E0603/E0624/E0616, add-use from E0599) are
      synthesized at `syntax_only`. The replacement text may come from the
      compiler, but the INSERT POSITION is a planner guess - honesty about
      sourcing is what the provenance ladder preserves.
    - Borrow-checker / trait-bound / move errors (E0277, E0382, E0502,
      E0507, E0596) and uncategorized diagnostics become `leftovers`. They
      are surfaced, never blindly retried.

  Clippy rides free: it emits the same JSON shape, and `clippy::*` codes fall
  through to the leftover branch (closing gap G13 as the lint-classification
  mode of the same tool).

PARAMS
  diagnostics: BuildDiagnostic[]   The `diagnostics[]` array from build.gate.
                                       Each entry may carry `code`, `file`,
                                       `message`, and `suggestions[]`.
  raw_json?: string                 Optional raw `cargo --message-format=json`
                                       / `rustc --error-format=json` stdout;
                                       parsed and appended to `diagnostics`.
  restrict_to_files?: string[]      Optional file path substrings;
                                       diagnostics whose `file` does not
                                       match any are skipped (mirrors v1
                                       `restrict_to_files`).

RETURNS { changes, findings, leftovers, counts, issuance }
  changes[]: each is `{ span, new_text, provenance, code? }` -
    span          hash-anchored Span (from the build.gate suggestion span;
                  re-derive via code.read if absent)
    new_text      verbatim `suggested_replacement` bytes (compiler_suggested)
                  or planner-synthesized text (syntax_only)
    provenance    "compiler_suggested" | "syntax_only"
    code?         the diagnostic code that motivated the proposal
  findings[]: review notes (e.g. visibility_bump_proposed)
  leftovers[]: each is `{ message, code?, reason }` - diagnostics the cell
    must address by hand.
  counts: { changes, leftovers }

NEVER WRITES. Feed `changes` into `edits.merge`; the ledger recognition keeps
the provenance tier through to `edits.apply`. The multi-round discipline is:
  rust.<transform> -> edits.apply -> build.gate("cargo check --message-format=json")
    -> rust.fixRound -> edits.merge -> edits.apply -> (fixed point, cap ~5)
Leftovers are the manual punch list; do not retry them blindly.

IDIOM
  const gate = await build.gate({ command: "cargo check --message-format=json",
                                   anchor_spans: true });
  const round = await rust.fixRound({ diagnostics: gate.diagnostics });
  if (round.changes.length) {
    const es = await edits.begin();
    await edits.merge({ es, changes: round.changes.map(c => ({span: c.span, new_text: c.new_text})) });
    await edits.apply({ es });
  }
  // round.leftovers is the manual punch list.
"#;

const EXTRACT_IMPL_METHODS_CONTRACT: &str = r#"rust.extractImplMethods - move named Rust impl methods from one file into another.

WHAT IT DOES
  Ports the v1 `extract_rust_impl_methods` synthesis. Moves named methods out of
  one `impl` block into another file, preserving attributes/modifiers (async
  etc.), rebasing `super::` paths one module deeper when the target is a child
  module of the source (foo.rs -> foo/bar.rs), and applying visibility overrides.

  - Explicit `visibility` overrides every moved method regardless of original.
  - Without explicit visibility, only originally-private moved methods widen to
    `pub(super)` when the parent still references them after deletion; existing
    `pub`/`pub(crate)`/`pub(super)` are preserved.
  - If the target already has a matching impl block (same type name), moved
    methods are appended inside it; otherwise a new impl block is created.
  - `target_prelude` text (e.g. `use` statements) is inserted after shebang /
    inner attrs / inner doc comments in the target file, when absent.
  - rmcp `tool_router` wrapper generation is deliberately NOT ported (repo-
    specific; recipe material).

PARAMS
  source: string           Source file path (relative to worktree root, or absolute).
  target: string           Target file path.
  item_names: string[]     Names of the impl methods to move (required, non-empty).
  impl_name?: string       Optional impl block name disambiguator (the type name in
                               `impl Foo`). Required when a method name matches
                               multiple impl blocks.
  visibility?: string      Optional explicit visibility for every moved method
                               (e.g. "pub", "pub(crate)", "pub(super)").
  target_prelude?: string  Optional text to insert after shebang/inner attrs/doc
                               comments in the target (e.g. use statements).

RETURNS { changes, creates, findings, leftovers, counts, provenance }
  changes[]:  SpanChange for edits.merge (source deletions + target insertions).
  creates[]:  { path, content } for edits.createFile (only when the target file
              is new/empty; otherwise the target edits are inline).
  findings[]: always [] (reserved for future findings).
  leftovers[]: string descriptions of methods NOT moved.
  counts: { moved: number, leftovers: number }
  provenance: "syntax_only"

NEVER WRITES. Feed `changes` into `edits.merge` and `creates` into
`edits.createFile`, then `edits.apply`. The transform is NOT idempotent over its
own output: target-exists refusal during the next extract is the DONE signal.
"#;

const REWRITE_MODULE_CALLERS_CONTRACT: &str = r#"rust.rewriteModuleCallers - rewrite caller prefixes after a module move.

WHAT IT DOES
  Ports the caller-prefix rewrite half of v1 `move_rust_items_with_callers`,
  decomposed per design §3.1 (composable after any extract or move; NOT fused
  with extraction). For each named moved item, rewrites every
  `<source_simple>::<item>` occurrence in other project .rs files to
  `<target_simple>::<item>`, including inside `use` declarations.
  Word-boundary checked.

  Known v1 limits (documented):
  - Simple-name segment match only. `crate::foo::source_simple::Item` gets
    rewritten; `crate::foo::Item` (where the canonical path skipped
    `source_simple`) does not.
  - No splitting of multi-import use trees.
  - No alias awareness.

PARAMS
  project_dir: string       Project directory for the caller walk (relative to
                               worktree root, or absolute). Skips target/,
                               build/, node_modules/, .git/.
  item_names: string[]      Names of the moved items (required, non-empty).
  module_name?: string      Source module's simple name. Required in the
                               decomposed model (no attached source file).
  target_prelude?: string   Target module's simple name. Required in the
                               decomposed model.
  skip_files?: string[]     File paths to skip during the walk (source/target
                               of the extract/move; already covered).

RETURNS { changes, findings, counts, provenance }
  changes[]:  SpanChange for edits.merge (caller prefix rewrites).
  findings[]: capped-change notices when the 2000-change limit is hit.
  counts: { files_touched: number, rewrites: number }
  provenance: "syntax_only"

BOUNDED: the 2000-change cap honors the isolate-heap discipline. A cap finding
means narrow the file set and re-run.
"#;

const EXTRACT_ITEMS_CONTRACT: &str = r#"rust.extractItems - move top-level Rust items into a (new) submodule.

WHAT IT DOES
  Ports v1 extract_rust_items_to_submodule + extract_rust_items +
  move_rust_items_with_local_deps + extract_rust_section as ONE transform
  (design/refactor-tools/rust/rust-isolate-surface.md §3.1). Plain extract
  when wiring knobs unset; compound mode (default) does scaffolded target +
  `mod <name>;` in parent + visibility bumps on moved items AND their struct
  fields + extract + auto-pruned `use <module>::{...};` re-import.

  Knob boundary (§8.3): the five knobs select the SYNTHESIS SHAPE; the host
  dependency analysis runs ALWAYS and reports in findings (never knob-gated).
  Per-item visibility maps are orchestration and live in rust.setVisibility
  after; any deep_analysis-style toggle is rejected categorically.

PARAMS
  source: string                     Parent module file (workspace-relative).
  target: string                     New submodule file (workspace-relative).
  itemNames: string[]                Top-level item names to move.
  itemKinds?: string[]               Optional syntax item kinds to narrow names.
  moduleName?: string                Module name for `mod <name>;`. Defaults to
                                       the target file stem (must match it).
  visibility?: string                Visibility floor for items + fields.
                                       Compound mode. Defaults to `pub(super)`.
  targetPrelude?: string             New file prelude. Defaults to `use super::*;`.
  withLocalDeps?: boolean            Move the exclusive private dependency
                                       closure of the seeds (not just the seeds).
  section?: {startMarker?, endMarker?,  Section addressing: select items by
              startLine?, endLine?}     source-region bounds.
  mergeIntoExistingTarget?: boolean  Append to a non-empty target instead of
                                       refusing.
  useDeclVisibility?: string         Visibility of the parent re-export
                                       (private | pub | pub(crate) | pub(super)).
  useDeclItems?: string[]            Explicit re-export subset of itemNames
                                       (defaults to auto-prune: only names still
                                       referenced in the post-deletion source).
  previewOnly?: boolean              Return findings + metadata but zero
                                       changes/creates.

RETURNS { title, changes, creates, findings, preview_only, mode,
           would_change_files, would_create_files, provenance }
  changes[]:   { span, new_text } for edits.merge (source-side edits)
  creates[]:   { path, content } for edits.createFile (new target file)
  findings[]:  always-on dependency analysis (local_dependency_closure when
               withLocalDeps, external_references, suggested_clusters) +
               planner notes + moved_item entries
  mode:        "compound" | "with_local_deps" | "section"

NEVER WRITES. Feed `changes` into edits.merge and `creates` into
edits.createFile, then edits.apply. NOT idempotent over its own output: a
re-call after a successful apply hits the target-exists refusal - that is
the DONE signal, not a retry. store() the result if you need it in later
cells.

IDIOM
  const r = await rust.extractItems({ source: "src/big.rs",
    target: "src/big/helpers.rs", itemNames: ["Helper", "build"] });
  const es = await edits.begin();
  await edits.merge({ es, changes: r.changes });
  for (const c of r.creates) await edits.createFile({ es, path: c.path, content: c.content });
  await edits.apply({ es });
  // re-run cargo check to verify; rust.fixRound handles follow-up diagnostics.
"#;

const INLINE_MOD_TO_FILE_CONTRACT: &str = r#"rust.inlineModToFile - inline `mod foo { ... }` body to a sibling submodule file.

WHAT IT DOES
  Ports v1 inline_mod_to_file_submodule (design §3.1). Extracts the body of
  an inline `mod foo { ... }` into a submodule file and replaces the block
  with `mod foo;`. Outer attributes such as `#[cfg(test)]` stay attached to
  the retained declaration - they are written above `mod foo`, not inside
  the body, so the in-place rewrite naturally preserves them.

  Target auto-derivation (Rust 2018+ module layout):
    `parent.rs` + `mod tests` -> `parent/tests.rs`
    `lib.rs` / `main.rs` / `mod.rs` + `mod foo` -> `foo.rs` (flat sibling)
  Explicit `target` overrides the derivation.

  Refuses non-empty targets (operator-scaffolded empty file is accepted).
  Body de-indentation strips the longest common run of leading spaces.

PARAMS
  source: string       File containing the inline mod (workspace-relative).
  moduleName: string   The mod name to inline.
  target?: string      Optional explicit target path (auto-derived when unset).
  previewOnly?: boolean

RETURNS { title, changes, creates, findings, preview_only, target,
           would_change_files, would_create_files, provenance }
  target: the resolved target path (useful when auto-derived).

NEVER WRITES. Feed changes -> edits.merge, creates -> edits.createFile.
NOT idempotent: re-call after apply hits the target-exists refusal (DONE).
"#;

const MODULE_WIRING_CONTRACT: &str = r#"rust.moduleWiring - one conservative Rust module-graph edit.

WHAT IT DOES
  Ports v1 rust_module_wiring + the absorbed mod/use micro-kinds (design
  §3.1). One action per call:

    add_mod     insert `<vis>mod <name>;` (idempotent: refuses duplicates)
    remove_mod  delete an existing `mod <name>;` declaration
    add_use     insert `<vis>use <path>;` (idempotent: refuses verbatim dups)
    remove_use  delete an existing `use <path>;` line (any visibility)

  Tree-sitter validated. The planner refuses duplicates and missing targets
  with actionable errors.

PARAMS
  source: string        File to edit (workspace-relative).
  action: string        "add_mod" | "remove_mod" | "add_use" | "remove_use".
  moduleName?: string   Required for mod actions.
  usePath?: string      Required for use actions (e.g. `std::collections::HashMap`,
                        `child::{A, B}`).
  visibility?: string   Optional prefix (`pub`, `pub(crate)`, `pub(super)`).

RETURNS { title, changes, findings, action, would_change_files, provenance }

NEVER WRITES. Feed changes -> edits.merge -> edits.apply.
"#;

const SET_VISIBILITY_CONTRACT: &str = r#"rust.setVisibility - rewrite visibility of items, impl methods, or struct fields.

WHAT IT DOES
  Ports v1 rewrite_rust_item_visibility + rewrite_rust_field_visibility as
  one transform with a targetKind selector (design §3.1). Only the
  visibility prefix is rewritten; async/unsafe/const qualifiers are
  preserved (the planner rewrites the prefix up to the keyword byte).

PARAMS
  source: string       File to edit (workspace-relative).
  visibility: string   New visibility: `pub`, `pub(crate)`, `pub(super)`,
                       or `private` (empty prefix).
  targetKind?: string  "item" (default) | "method" | "field".
  itemNames: string[]  Item / struct / method names.
  implName?: string    Impl name disambiguator for method targets when
                       multiple impls define the same method name.

RETURNS { title, changes, findings, target_kind, would_change_files, provenance }
  findings[] includes one `visibility_rewritten` entry per rewritten item.

NEVER WRITES. Feed changes -> edits.merge -> edits.apply. For a moved-item
visibility bump baked into an extract, prefer rust.extractItems (compound
mode bakes item + field visibility in one pass); use this transform for
standalone visibility edits or post-extract adjustments.
"#;

const ORGANIZE_IMPORTS_CONTRACT: &str = r#"rust.organizeImports - minimize Rust wildcard imports.

WHAT IT DOES
  Ports v1 rust_minimize_imports (design section 3.1). mode="minimize"
  (default) rewrites wildcard `use foo::*;` declarations whose source module
  resolves to a local Rust file into explicit `use foo::{A, B};` for the
  directly-referenced names. Only wildcards whose target module is resolvable
  and whose names are referenced get rewritten; the rest surface as notes in
  findings (or get deleted when removeUnusedWildcards=true).

  mode="organize" is the future rust-analyzer source.organizeImports path
  (lsp_verified). It lands with lsp.assist (phase 2); this binding REFUSES
  it with a structured error rather than stubbing a fake organize, so a
  caller never silently gets a different operation than they asked for.

PARAMS
  source: string                  File to edit (workspace-relative, no `..`).
  mode?: "minimize"|"organize"    Default "minimize".
  allowWildcards?: string[]       Wildcard base paths to preserve verbatim
                                  (e.g. ["std::io", "crate::prelude"]).
  removeUnusedWildcards?: boolean When true, wildcards with no
                                  directly-referenced names are deleted
                                  instead of left as leftovers.

RETURNS
  { title, changes, creates, findings, mode, would_change_files,
    would_create_files, provenance:"syntax_only" }
  findings[] includes one `note` entry per preserved/unresolvable wildcard.

NEVER WRITES. Feed {changes} into edits.merge, {creates} into edits.createFile,
then edits.apply. NOT idempotent over its own output: if a re-call reports no
wildcard imports, the work is DONE (verify with code.items on the source).
"#;

#[async_trait]
impl Tool for RustDescribe {
    fn name(&self) -> &str {
        "rust.describe"
    }
    fn description(&self) -> &str {
        "Full contract for one rust.* binding (params, result vocabulary, recipe). The namespace index lists transforms one line each; call this before first use."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "transform": { "type": "string", "description": "Transform name, e.g. \"fixRound\"." }
            },
            "required": ["transform"]
        })
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }
    fn namespace_binding(&self) -> Option<(String, String)> {
        Some(("rust".to_string(), "describe".to_string()))
    }
    async fn call(&self, input: Value, _cx: &ToolCx) -> ToolResult {
        let transform = input
            .get("transform")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match transform {
            "fixRound" => ToolResult::Json(json!({ "contract": FIX_ROUND_CONTRACT })),
            "extractItems" => ToolResult::Json(json!({ "contract": EXTRACT_ITEMS_CONTRACT })),
            "inlineModToFile" => {
                ToolResult::Json(json!({ "contract": INLINE_MOD_TO_FILE_CONTRACT }))
            }
            "moduleWiring" => ToolResult::Json(json!({ "contract": MODULE_WIRING_CONTRACT })),
            "setVisibility" => ToolResult::Json(json!({ "contract": SET_VISIBILITY_CONTRACT })),
            "extractImplMethods" => {
                ToolResult::Json(json!({ "contract": EXTRACT_IMPL_METHODS_CONTRACT }))
            }
            "organizeImports" => ToolResult::Json(json!({ "contract": ORGANIZE_IMPORTS_CONTRACT })),
            "rewriteModuleCallers" => {
                ToolResult::Json(json!({ "contract": REWRITE_MODULE_CALLERS_CONTRACT }))
            }
            other => ToolResult::Error(format!(
                "rust.describe: unknown transform `{other}` (available: fixRound, extractItems, inlineModToFile, moduleWiring, setVisibility, extractImplMethods, organizeImports, rewriteModuleCallers)"
            )),
        }
    }
}

/// The `rust.*` binding set.
pub fn tools(ledger: Arc<ProvenanceLedger>) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(RustDescribe) as Arc<dyn Tool>,
        Arc::new(fix::RustFixRound(Arc::clone(&ledger))) as Arc<dyn Tool>,
        Arc::new(r#move::RustExtractItems(Arc::clone(&ledger))) as Arc<dyn Tool>,
        Arc::new(r#move::RustInlineModToFile(Arc::clone(&ledger))) as Arc<dyn Tool>,
        Arc::new(wiring::RustModuleWiring(Arc::clone(&ledger))) as Arc<dyn Tool>,
        Arc::new(impl_extract::RustExtractImplMethods) as Arc<dyn Tool>,
        Arc::new(imports::RustOrganizeImports(Arc::clone(&ledger))) as Arc<dyn Tool>,
        Arc::new(rewrite_callers::RustRewriteModuleCallers) as Arc<dyn Tool>,
        Arc::new(wiring::RustSetVisibility(ledger)) as Arc<dyn Tool>,
    ]
}

/// Hand-authored namespace documentation + TS declarations (cell-DSL §5.2).
/// Compact index (§6.5 surface economics): one line per transform; depth on
/// demand via rust.describe.
pub fn namespace_description() -> ToolNamespaceDescription {
    ToolNamespaceDescription {
        name: "rust".to_string(),
        description: "Rust transform bindings ported from the v1 bbox-refactor rust catalog (design/refactor-tools/rust/rust-isolate-surface.md). Each transform NEVER writes: it returns {changes, creates, findings} for edits.merge/createFile and records host-authored changes in the provenance ledger so edits.apply computes semantic_status lineage. Call rust.describe({transform}) for the full contract. Transforms: fixRound - classify rustc/clippy build.gate diagnostics into compiler_suggested edits (verbatim MachineApplicable suggestions) + syntax_only synthesized proposals (add-use, visibility-bump) + explicit leftovers (borrow-checker, trait-bound); the compile-fix loop engine. Clippy diagnostics classify the same way (same JSON shape). extractItems - move top-level items into a (new) submodule; compound mode (default) does scaffolded target + `mod <name>;` in parent + visibility bumps on moved items and struct fields + auto-pruned `use <module>::{...};` re-import; knobs select synthesis shape (withLocalDeps, section, mergeIntoExistingTarget, useDeclVisibility, useDeclItems) while dependency analysis runs always and reports in findings. inlineModToFile - inline `mod foo { ... }` body to a sibling submodule file; outer attrs like #[cfg(test)] stay attached. moduleWiring - one conservative module-graph edit (add_mod/remove_mod/add_use/remove_use, idempotent). setVisibility - rewrite visibility of items, impl methods, or struct fields; preserves async/unsafe/const qualifiers. extractImplMethods - move named Rust impl methods from one file into another; preserves attributes/modifiers, rebases super:: paths, applies visibility overrides. organizeImports - minimize wildcard `use foo::*;` into explicit `use foo::{A, B};` for directly-referenced names (mode=minimize, default; mode=organize is the future rust-analyzer source.organizeImports path, lands with lsp.assist phase 2). rewriteModuleCallers - rewrite caller prefixes after a module move; <source_simple>::<item> to <target_simple>::<item> in all project .rs files, word-boundary checked."
            .to_string(),
        declarations: r#"type RustChangeProposal = { span: Span; new_text: string; provenance: "compiler_suggested" | "syntax_only"; code?: string };
type RustLeftover = { message: string; code?: string; reason: string };
type RustFixRoundResult = { changes: RustChangeProposal[]; findings: ({ finding: string } & Record<string, unknown>)[]; leftovers: RustLeftover[]; counts: { changes: number; leftovers: number }; issuance: string };
type RustSpanChange = { span: Span; new_text: string };
type RustCreate = { path: string; content: string };
type RustWouldChangeFile = { path: string; edit_count: number; replacement_bytes: number };
type RustWouldCreateFile = { path: string; bytes: number };
type RustSectionBounds = { startMarker?: string; endMarker?: string; startLine?: number; endLine?: number };
type RustExtractItemsResult = { title: string; changes: RustSpanChange[]; creates: RustCreate[]; findings: ({ finding: string } & Record<string, unknown>)[]; preview_only: boolean; mode: "compound" | "with_local_deps" | "section"; would_change_files: RustWouldChangeFile[]; would_create_files: RustWouldCreateFile[]; provenance: "syntax_only" };
type RustInlineModToFileResult = { title: string; changes: RustSpanChange[]; creates: RustCreate[]; findings: ({ finding: string } & Record<string, unknown>)[]; preview_only: boolean; target: string; would_change_files: RustWouldChangeFile[]; would_create_files: RustWouldCreateFile[]; provenance: "syntax_only" };
type RustModuleWiringResult = { title: string; changes: RustSpanChange[]; findings: ({ finding: string } & Record<string, unknown>)[]; action: "add_mod" | "remove_mod" | "add_use" | "remove_use"; would_change_files: RustWouldChangeFile[]; provenance: "syntax_only" };
type RustSetVisibilityResult = { title: string; changes: RustSpanChange[]; findings: ({ finding: string } & Record<string, unknown>)[]; target_kind: "item" | "method" | "field"; would_change_files: RustWouldChangeFile[]; provenance: "syntax_only" };
type RustExtractImplMethodsResult = { changes: SpanChange[]; creates: { path: string; content: string }[]; findings: Record<string, unknown>[]; leftovers: string[]; counts: { moved: number; leftovers: number }; provenance: "syntax_only" };
type RustRewriteModuleCallersResult = { changes: SpanChange[]; findings: Record<string, unknown>[]; counts: { files_touched: number; rewrites: number }; provenance: "syntax_only" };
type RustOrganizeImportsResult = { title: string; changes: RustSpanChange[]; creates: RustCreate[]; findings: ({ finding: string } & Record<string, unknown>)[]; mode: "minimize"; would_change_files: RustWouldChangeFile[]; would_create_files: RustWouldCreateFile[]; provenance: "syntax_only" };
declare const rust: {
  /** Full contract (params, result vocabulary, recipe) for one rust transform. Call before first use. */
  describe(args: { transform: string }): Promise<{ contract: string }>;
  /** Classify rustc/clippy build.gate diagnostics into edit proposals + leftovers. Verbatim MachineApplicable suggestions become compiler_suggested changes; add-use/visibility-bump proposals are syntax_only; borrow-checker/trait-bound errors are leftovers. NEVER writes: feed {changes} into edits.merge. */
  fixRound(args: { diagnostics: Record<string, unknown>[]; raw_json?: string; restrict_to_files?: string[]; restrictToFiles?: string[] }): Promise<RustFixRoundResult>;
  /** Move top-level Rust items into a (new) submodule. Compound mode (default): scaffolded target + `mod <name>;` + visibility bumps + auto-pruned use decl. Knobs select synthesis shape; dependency analysis runs always. Feed {changes, creates} into edits.merge/createFile. NOT idempotent: target-exists refusal after apply is the DONE signal. */
  extractItems(args: { source: string; target: string; itemNames: string[]; itemKinds?: string[]; moduleName?: string; visibility?: string; targetPrelude?: string; withLocalDeps?: boolean; section?: RustSectionBounds; mergeIntoExistingTarget?: boolean; useDeclVisibility?: string; useDeclItems?: string[]; previewOnly?: boolean }): Promise<RustExtractItemsResult>;
  /** Inline `mod foo { ... }` body to a sibling submodule file; outer attrs like #[cfg(test)] stay attached. Target auto-derived (parent.rs -> parent/<name>.rs; lib.rs/main.rs/mod.rs -> flat sibling). Refuses non-empty targets. Feed {changes, creates} into edits.merge/createFile. */
  inlineModToFile(args: { source: string; moduleName: string; target?: string; previewOnly?: boolean }): Promise<RustInlineModToFileResult>;
  /** One conservative Rust module-graph edit: add_mod, remove_mod, add_use, or remove_use. Idempotent (rejects duplicates and missing targets). Feed {changes} into edits.merge. */
  moduleWiring(args: { source: string; action: "add_mod" | "remove_mod" | "add_use" | "remove_use"; moduleName?: string; usePath?: string; visibility?: string }): Promise<RustModuleWiringResult>;
  /** Rewrite visibility of top-level items, impl methods, or struct fields. Preserves async/unsafe/const qualifiers (only the visibility prefix is rewritten). targetKind: item (default), method, or field. implName disambiguates methods. Feed {changes} into edits.merge. */
  setVisibility(args: { source: string; visibility: string; itemNames: string[]; targetKind?: "item" | "method" | "field"; implName?: string }): Promise<RustSetVisibilityResult>;
  /** Move named Rust impl methods from one file into another. Preserves attributes/modifiers, rebases super:: paths one module deeper, applies visibility overrides. NEVER writes: feed {changes} into edits.merge, {creates} into edits.createFile. */
  extractImplMethods(args: { source: string; target: string; item_names: string[]; impl_name?: string; visibility?: string; target_prelude?: string }): Promise<RustExtractImplMethodsResult>;
  /** Minimize Rust wildcard imports (mode="minimize", default): rewrite resolvable `use foo::*;` into explicit `use foo::{A, B};` for directly-referenced names. mode="organize" (rust-analyzer source.organizeImports) lands with lsp.assist (phase 2). NEVER writes: feed {changes} into edits.merge. */
  organizeImports(args: { source: string; mode?: "minimize" | "organize"; allow_wildcards?: string[]; remove_unused_wildcards?: boolean }): Promise<RustOrganizeImportsResult>;
  /** Rewrite caller prefixes after a module move: <source_simple>::<item> -> <target_simple>::<item> in all project .rs files, word-boundary checked. Composable after any extract/move. NEVER writes: feed {changes} into edits.merge. */
  rewriteModuleCallers(args: { project_dir: string; item_names: string[]; module_name?: string; target_prelude?: string; skip_files?: string[] }): Promise<RustRewriteModuleCallersResult>;
};"#
            .to_string(),
    }
}
