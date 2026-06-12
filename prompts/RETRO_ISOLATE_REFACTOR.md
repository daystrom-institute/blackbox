---
title: "Retro: Isolate Refactor Bindings"
kind: operator-prompt
corpus: blackbox-prompts
audience: interactive
topic:
  - prompts
  - refactor-tools
brief: "Post-probe retro for a code-mode session that drove the refactor namespace bindings (code.*/lsp.*/analysis.*/edits.*). Harvests binding-level friction for the refactor-tools-v2 build-out; files gaps in the */refactor-tools/* dedupe namespace."
---

# Retro: Isolate Refactor Bindings

A retrospective pass for an agent session that used the **refactor namespace
bindings inside code-mode cells** — `code.*` facts (and, as they land,
`lsp.*`, `analysis.*`, `edits.*`, `apply`). The sibling of
`RETRO_HARNESS.md`, narrowed to the isolate/refactor surface: where
RETRO_HARNESS reflects on the whole harness, this one interrogates whether
the *binding algebra* composes — the live-probe instrument for
`design/bro-harness/refactor-v2-pressure-test.md` §7.

Run it after a probe or real refactor session where cells called the
namespace globals. Standalone probe agents have no `bbox_*` tools; in that
case the agent returns structured findings and the orchestrator files the
gaps on its behalf (same dedupe namespace: `*/refactor-tools/*`).

## What this reflects on

- **Discoverability** — did you find the namespace globals (`code.*`, …) and
  understand they are NOT `tools.*` properties? Did the `## code namespace`
  declarations in the exec description carry enough signal, or did you
  discover by error? (`code_mode=optional` renders namespace declarations but
  not the flat catalog — note which mode you ran under.)
- **Declaration fidelity** — did the TS declarations match the serde reality?
  Any field you had to guess, any shape that surprised you at runtime?
- **Query authoring** — for `code.query`: did you know the tree-sitter node
  grammar for the target language, or did you burn cells on `Invalid node
  type` / `Impossible pattern` errors? What would have prevented that —
  example queries in the declarations, a node-kind inventory binding, kinds
  surfaced by `code.items`?
- **Span discipline** — did hash-anchored Spans compose cleanly
  (items → query → read → union)? Did you hit `stale_span`, and was the
  recovery path (re-derive facts) obvious? Did you ever hand-construct a
  Span rather than passing one through?
- **Batching** — did you fan out with `Promise.all` over bindings, or await
  sequentially? If sequential: didn't know, didn't think of it, or the API
  shape made it awkward?
- **Context discipline** — what did you `text()` into model context vs. keep
  in cell variables / `store()`? Anything you echoed wholesale that should
  have stayed in the isolate?
- **Error shapes** — when a binding refused (bad query, stale span, path
  escape, bounds), did the error tell you what to do next without
  re-discovery? Which error cost you the most cells to get past?
- **Missing algebra** — what fact or operation did you want that the
  namespace didn't have (signatures, references, node-kind listing, edit
  building, a mutation choke point)? What did you do instead?

## The prompt

> This is a retrospective on the refactor namespace bindings you just used
> inside exec cells (`code.items` / `code.query` / `code.read` /
> `code.spanUnion`, plus whatever else was bound). Walk your actual cells in
> order. Where did the binding surface get in your way? How did you discover
> the namespace globals, and did the TS declarations match what came back at
> runtime? For every failed cell: what error did you get, and what would
> have prevented it at authoring time? Did you batch independent binding
> calls with Promise.all or await them one at a time — and why? What did you
> text() into context that could have stayed in a variable? What fact or
> operation was missing from the algebra, and what was your workaround?
> Return your findings as a JSON array under the key `findings`, each:
> `{ "area": "discoverability|declarations|query-authoring|spans|batching|context|errors|missing-algebra", "observation": "...", "cost": "none|minor|cells-burned|wrong-result", "wanted": "concrete capability or doc change that would have prevented it" }`,
> followed by a short prose summary. Be concrete and adversarial toward the
> surface, not toward the task.

## Filing

Dedupe first (`bbox_gaps`, filter `domain=refactor-tools` /
`dedupe_key=*/refactor-tools/*`), then file per reusable capability with
`bbox_gap` — `gap_kind` is the capability type (`tooling` for bindings,
`docs_runbook` for declaration/doc fixes, `refactor_primitive` for missing
algebra), `domain` is `refactor-tools` (or `refactor-tools/<area>`), and
evidence cites the probe session id + cells. Findings that are one-off task
noise (not reusable) go in the summary, not gaps.
