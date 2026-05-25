---
title: "Hoisting Java to First-Class Code Navigation"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - refactor-tools
  - code-navigation
  - java
tags:
  - code-navigation
  - refactor-tools
  - java
  - lsp
  - jdtls
  - mcp
date: 2026-05-25
status: "design proposal"
brief: "Brings Java to parity with Rust/C# semantic navigation by adding a jdtls-backed semantic tier to bbox_code_* — References, Implementation, WorkspaceSymbol, DocumentSymbol, Hover — over the already-generic LSP substrate, with a shared lsp::convert hoist, capability expansion, the prefilter-then-resolve idiom, and a worked bbox_code_usages caller. Serves macro probes, refactor plan-shaping, and bare LLM due-diligence."
---

# Hoisting Java to First-Class Code Navigation

## Thesis

`bbox_code_*` is today a syntax-only tree-sitter surface. For Java that leaves a
gap that bare LLMs hit constantly during due diligence and plan-shaping: "who
actually calls this overload?", "which classes implement this interface?",
"what is the resolved type of this expression?" — questions tree-sitter
structurally cannot answer.

The good news from grounding: **the LSP substrate is already generic and
proven, and Java is simply behind.** Adding semantic Java navigation is
*composition over existing machinery*, not new engine work. This capability is
Phase 0 of the [Unified Code Synthesis Model](../unified-code-synthesis-model.md):
its macro `probe` operations bind to the semantic tier specified here. But the
tier is independently valuable for refactor plan-shaping and for any LLM
figuring out *what and how* to call before it acts.

## Current state (grounded)

### Syntactic code-nav — fast, syntax-only, the right default

`src/code_nav/mod.rs` + `src/tools/code_nav.rs` expose `bbox_code_query`
(raw tree-sitter S-expressions), `bbox_code_symbols` (project inventory; live
tree-walk or tantivy-indexed lane), `bbox_code_refs` (curated calls/imports/
fields/identifiers), and `bbox_code_node_describe` (AST context at a position).
Every result carries `semantic_status: "syntax_only"` (`mod.rs:27`) and a
`CodeRefactorHandoff` (`mod.rs:635`) that pre-fills the next
`bbox_refactor_status` / `bbox_refactor_project_refs` call. File-size cap 2 MB,
scan cap 5000 files. This is correct as the *fast default* and stays unchanged.

Per [code-nav-depth-axis1.md](../../corpus/code-navigation/code-nav-depth-axis1.md),
the synthetic-kind table (`refactor_kind_for`) must **not** be expanded — the
only synthetic kind is Rust `impl_method`, and the two lanes are pinned equal by
test. The semantic tier proposed here is **orthogonal**: it is symbol/type
*resolution* via LSP, not tree-sitter *synthesis*. It adds a new
`semantic_status` value to results, not new synthetic kinds.

### Java's LSP footprint — one capability

Java has exactly one LSP-backed capability: `jdtls_organize_imports`
(`src/refactor/java/leaf_plans.rs:465`), which opens the file, waits for
`publishDiagnostics`, requests `source.organizeImports`, and maps the
`WorkspaceEdit` back to `FileEdit`s. By contrast Rust has rename, RA move-item,
and `classify_callbacks` (`GotoDefinition`); C# has rename, move-item,
organize-usings, and `find_usages` (`References`). **Java is behind both.**

### The LSP substrate is generic and done

`src/lsp/session_manager.rs` is fully language-agnostic:

- `LspClient::send_request<R: Request>` (`:205`) accepts *any* `lsp_types`
  request. Nothing is Java- or Rust-specific.
- Sessions spawn/pool/evict/respawn per `(project_root, Language)` via
  `with_session` → `spawn_session` → `launch_argv`; `Broken` drops the session
  and the next call respawns. jdtls cold-start timeout is 60s (`:88`).
- The bidirectional bridges already exist: `byte_to_lsp_position` /
  `lsp_position_to_byte` (`src/refactor/rust.rs:2476/2483`) and
  `workspace_edit_to_file_edits` (`:2518`, handling `changes`,
  `document_changes`, and annotated edits). They are `pub(crate)` and already
  shared by Rust + C#.
- The caller pattern is proven three times (C# `References`, Rust
  `GotoDefinition`, Rust `Rename`, Java/C# `CodeActionRequest`): build params
  with a byte-derived `Position` → `with_session(dir, Language, |client| {
  didOpen; wait_for_diagnostics; send_request::<R>; read_response::<R> })` → map
  → **fail closed with `error.lsp_unavailable` (RX-V3)**.

### The one real substrate gap — declared capabilities

`build_init_params` (`:696`) declares a minimal `ClientCapabilities`: only
`workspace.workspace_edit` and `textDocument.code_action` (organize-imports
literal). It does **not** advertise `documentSymbol`, `references`, `definition`,
`typeDefinition`, `implementation`, `callHierarchy`, `workspaceSymbol`, or
`hover`. jdtls serves most regardless (caps are advisory), but a few features
gate on declared support (`hierarchicalDocumentSymbolSupport`, call-hierarchy).
Step zero is growing this block.

## The semantic query menu

What jdtls answers that tree-sitter cannot, mapped to all three dependent
consumers:

| LSP request | Answers | Macro probe use | Plan-shaping / due-diligence use |
|---|---|---|---|
| **`References`** | resolved usages (overload-aware) | blast-radius refusal | "if I change this, what breaks?" before tool choice |
| **`Implementation` / type-hierarchy** | implementors of X / subclasses of Y | `java.search.type` over contracts | "who depends on this contract" |
| **`WorkspaceSymbol`** | resolved symbol-by-name across project | `java.search.type` (find Guice module) | locate canonical declaration without grep |
| **`DocumentSymbol`** | hierarchical symbols + resolved signatures | accurate `emit`/`insert_member` targeting | semantic upgrade to syntax-only `code_symbols` |
| **`Hover`** | resolved type/signature at a position | closes the `java.search.member` erased-return-type gap | "what is this expression's actual type" |
| **`CallHierarchy` in/out** | semantic call graph | backs `behavior_source` extraction | reason about call flow before extract/move |

`References` and `WorkspaceSymbol` are the two highest-leverage additions.

## Architecture — semantic tier as a sibling of the syntactic one

Expose semantic queries through `code_nav`, not buried in the macro engine,
sharing the two seams that already exist.

### The `semantic_status` ladder is the contract

Syntactic results stay `SyntaxOnly`. Semantic results return `LspVerified` (or
`LspVerifiedPartial`), and **fail closed** to `error.lsp_unavailable` when jdtls
is unavailable — never a silent downgrade. The precedent for this is the
**RX-V3 Rust LSP-backed kinds** (`rust_lsp_rename`, `rust_organize_imports`,
etc.), which fail closed by design — **not** `jdtls_organize_imports`, which
does the opposite: on JDTLS failure it falls back to a heuristic syntax-only
edit (`src/refactor/java/leaf_plans.rs:557`). That fallback is acceptable for an
*edit* but is exactly what a *query* promising resolution must not do.

The hard split (this tier's core contract):

- A query that promises `lsp_verified` MUST return `error.lsp_unavailable` when
  jdtls is down. It must never relabel a syntactic guess as resolved.
- A code-nav tool MAY still offer a syntactic answer in that case, but only as a
  **separately labeled `syntax_only` result with an explicit caveat**, so
  plan-shaping is not blocked while the resolution promise stays honest.

Caveat to fix in passing: the existing Rust LSP kinds fail closed but do not
consistently prefix the missing-manager failure with `error.lsp_unavailable`
(`src/refactor/rust.rs:1631`); the new tier should use the typed error
uniformly and not inherit that inconsistency.

### The `CodeRefactorHandoff` seam is the stitch

Every syntactic result already pre-fills a refactor/status call. Semantic
results pre-fill the same way: a `References` result hands back resolved sites
*and* a ready-to-run refactor/macro invocation. This is the macro/refactor
stitching — it falls out of the existing design.

### Tool shape

- **Resolved-only queries** (no syntactic equivalent): sibling tools —
  `bbox_code_usages` (`References`), `bbox_code_implementations`
  (`Implementation`/type-hierarchy), `bbox_code_type_at` (`Hover`),
  `bbox_workspace_symbols` (`WorkspaceSymbol`).
- **Where a syntactic version exists**: a `mode: "semantic"` flag —
  `bbox_code_symbols(mode="semantic")` issues `DocumentSymbol` instead of the
  tree-sitter walk.

All semantic tools are **opt-in**: the LLM escalates from syntactic deliberately
(see cost model). All carry `CodeRefactorHandoff`.

## Prefilter-then-resolve

The bounded-cost idiom, already demonstrated by `rust_ra_classify_callbacks`
(tree-sitter finds call sites → `GotoDefinition` classifies each):

> **Narrow to candidates with `code_query` (milliseconds, no LSP), then resolve
> only the candidates with jdtls.**

Resolving 4 candidate methods is fine; resolving every method in a project is
not (per-anchored-query cost below). This idiom is the correct resolution of the
`java.search.member` ontology gap from the unified model: `@Provides` + return
shape via `code_query`; erased/generic resolved return type via `Hover` on the
bounded candidate set.

## Implementation

### Step 1 — hoist `lsp::convert`

Move `byte_to_lsp_position`, `lsp_position_to_byte`, and
`workspace_edit_to_file_edits` out of `src/refactor/rust.rs` into a shared
`src/lsp/convert.rs`. They are already `pub(crate)` and used by Rust + C#; the
current cross-language dependency on `rust.rs` is an accident of history.

### Step 2 — expand declared capabilities

Grow `build_init_params` (`session_manager.rs:696`) to advertise
`textDocument.{documentSymbol (hierarchical), references, definition,
typeDefinition, implementation, callHierarchy, hover}` and
`workspace.symbol`.

### Step 3 — per-query Java callers

Each ~150 lines, modeled verbatim on `src/refactor/csharp/find_usages.rs`,
fail-closed (RX-V3). Add `References`, `Implementation`, `WorkspaceSymbol`,
`Hover`, `DocumentSymbol`.

### Step 4 — expose in `code_nav`

New sibling tools + the `mode: "semantic"` flag, each returning `lsp_verified`
status + `CodeRefactorHandoff`, with the syntactic-fallback-with-caveat behavior
when jdtls is down.

### Step 5 — document the idiom

Codify prefilter-then-resolve in both the `code_nav` tool docs and the macro
probe spec so calling LLMs adopt it.

## Worked caller — `bbox_code_usages` (proof of pattern)

`References` is the template for every other semantic query. End-to-end shape,
mirroring `csharp/find_usages.rs`:

```rust
// 1. Require the session manager — fail closed (RX-V3).
let manager = ctx.lsp.as_ref().ok_or_else(|| {
    anyhow!("error.lsp_unavailable: bbox_code_usages requires the LSP session manager (RX-V3)")
})?;

// 2. Resolve the anchor position from line/col or a symbol the caller passed.
let source_uri = Url::from_file_path(&source_path)?;
let source = fs::read_to_string(&source_path)?;
let position = byte_to_lsp_position(&source, anchor_byte); // hoisted lsp::convert

let params = ReferenceParams {
    text_document_position: TextDocumentPositionParams {
        text_document: TextDocumentIdentifier { uri: source_uri.clone() },
        position,
    },
    context: ReferenceContext { include_declaration: true },
    work_done_progress_params: Default::default(),
    partial_result_params: Default::default(),
};

// 3. One pooled session round-trip: open, wait for diagnostics, request, read.
let locations = manager.with_session(&project_dir, Language::Java, |mut client| {
    client.send_notification::<DidOpenTextDocument>(&DidOpenTextDocumentParams {
        text_document: TextDocumentItem {
            uri: source_uri.clone(),
            language_id: "java".to_string(),
            version: 0,
            text: source.clone(),
        },
    })?;
    client.wait_for_diagnostics(source_uri.as_str(), Duration::from_secs(60));
    let id = client.send_request::<References>(&params)?;
    client.read_response::<References>(id)
}).map_err(|e| anyhow!("error.lsp_unavailable: {e}"))?;

// 4. Map Location[] -> resolved usage records + CodeRefactorHandoff per site.
//    semantic_status = LspVerified. On None: symbol_resolved = false.
```

The result is `lsp_verified` (resolved, overload-aware) where syntactic
`code_refs` could only return name-matched call sites. Same anchor, strictly
more truth.

## Cost model and gotchas

- **Cold start is 60s** (`jdtls_init_timeout`). The first semantic query for a
  project pays it; the pooled session makes subsequent queries warm. This is the
  reason the semantic tier is opt-in and syntactic stays the default.
- **Per anchored query**, the proven pattern does `didOpen` +
  `wait_for_diagnostics` (up to 60s) so jdtls has type-checked the file. Fine for
  single-anchor queries (`References`/`Hover` from a position). For
  whole-project work, prefer `WorkspaceSymbol` (no `didOpen` needed) or
  prefilter-then-resolve over a bounded candidate set — never fan a per-file
  open across the whole tree.
- **Fail closed, not silent.** A semantic query that cannot reach jdtls returns
  `error.lsp_unavailable`; a code-nav tool may then offer the syntactic answer
  *clearly labeled* `syntax_only`. It must not pretend resolution happened.
- **Shared-service hygiene:** sessions are daemon-pooled. Do not assume a fresh
  session per call; respect the existing eviction/respawn semantics.

## Dependents

- **[Unified Code Synthesis Model](../unified-code-synthesis-model.md)** — macro
  `probe` operations bind to this semantic tier (Phase 0 dependency); the
  `java.search.member` / `java.search.type` ontology gaps resolve here.
- **Refactor plan-shaping** — `bbox_refactor_*` callers can ground blast-radius
  and type questions before choosing a plan kind.
- **Bare LLM due-diligence** — any agent figuring out *what and how* to call
  gains resolved usage/type/implementor queries.
- **Crosscut:** [Code Navigation](../../corpus/code-navigation/code-navigation.md)
  hub; [Code Navigation Depth — Axis 1](../../corpus/code-navigation/code-nav-depth-axis1.md)
  (orthogonal: synthesis vs resolution).

## Open questions

- Sibling tools vs a single `mode`-switched surface for the resolved-only
  queries — how much does catalog surface area matter here?
- Should `WorkspaceSymbol` be the default project-wide locator, demoting the
  tantivy `code_symbols` indexed lane to a fast pre-warm fallback?
- Call-hierarchy: worth wiring in Phase 0, or defer until a macro/refactor
  consumer concretely needs it?
