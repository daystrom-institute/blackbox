---
title: "Agentic Corpus \u2014 Tier B AST Depth (per-language LSP)"
kind: design
lifecycle: archived
corpus: blackbox-design
topic:
  - corpus
  - agentic-corpus
---

# Agentic Corpus — Tier B AST Depth (per-language LSP)

Status: proposed (deferred from agentic-corpus-impl Y-* markers).
Related: `design/corpus/agentic-corpus/agentic-corpus.md` §7.4 (Tier A vs Tier B),
`design/corpus/agentic-corpus/agentic-corpus-impl.md` Phase S3 (Tier A AST edges
via tree-sitter-language-pack — landed),
`design/corpus/agentic-corpus/agentic-corpus-impl.md` (Y-* markers section).

## Thesis

Phase S3 landed **Tier A** AST extraction: structural edges (`IMPORTS`,
`DEFINES`, `CONTAINS_DECL`) emitted by tree-sitter parsers built into
the daemon for `rust, python, csharp, java, go, typescript, javascript,
c, cpp`. That gives skeleton-level navigation but stops short of
resolved call edges.

**Tier B** replaces naive symbol-table CALLS resolution with
per-language LSP integration. Each language gets one phase that wires
a long-lived LSP server (warmed via `LspSessionManager`, already used
by `rust_analyzer_rename` and `rust_analyzer_organize_imports`) and
extracts resolved-callee edges into the EdgeIndex.

## Phases (deferred; ordering opportunistic)

Build-prop selects which compile in. Rust first per the impl skeleton
(largest representation in the corpus on this host).

| Phase | Language | LSP server |
|-------|----------|------------|
| Y-Rust  | Rust              | `rust-analyzer` |
| Y-Python | Python           | `pyright` |
| Y-CSharp | C#               | `omnisharp` |
| Y-Java   | Java             | `jdtls` |
| Y-Go     | Go               | `gopls` |
| Y-TS     | TypeScript / JS  | `tsserver` |
| Y-CCpp   | C / C++          | `clangd` |

## Shape (sketch, common across phases)

Each Y-* phase:
- Reuses the warm `LspSessionManager` session for the project.
- On chunker run, for every function/method chunk: issue
  `textDocument/references` (or `callHierarchy/incomingCalls` where
  supported) to resolve call sites.
- Emit `CALLS` edges with `resolution: tier_b_lsp` provenance.
- Fall back to **no Tier A CALLS edges** when Tier B is enabled — the
  two should not coexist on the same chunk pair (avoid stale-low-tier
  edges shadowing higher-quality resolution).
- Fail closed when the LSP is unavailable (consistent with the RX-V3
  invariant pattern from the refactor surface).

## Open questions

- Per-language CI cost: integration tests need each LSP server in the
  test image. The current `cargo test --bin blackboxd` flow does not
  start LSPs; Y-* phases will introduce per-language `#[ignore]` gates
  unless CI grows LSP-aware harnesses.
- Edge volume: at corpus scale, resolved CALLS could push EdgeIndex
  past the 5M-edge ceiling flagged in S4 release notes
  (`HashSet<Edge>` rework needed). Y-* phases should land **after** or
  **with** the EdgeIndex memory model rework — see
  `agentic-corpus-followups.md`.
- Daemon footprint: one warm LSP per registered project per language
  multiplies memory. Consider on-demand spin-up + idle-eviction.

## Picking the next one

Rust is the default first cut. After that, picking is driven by which
language's corpus is widest on the host (TypeScript and Python are
likely tied for second). C/C++ via clangd needs `compile_commands.json`
to function, which is a per-project setup burden — defer unless a
target project actually has one.
