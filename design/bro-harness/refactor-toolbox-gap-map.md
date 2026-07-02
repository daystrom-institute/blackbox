---
title: "Refactor toolbox gap map: what the isolate needs next"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - bro-harness
  - refactor-tools
brief: "The isolate cell-DSL (code./analysis./java./edits./lsp.) audited against real fanout campaign practice on a large private Java repo, generalized to shapes. Verdict: the five-namespace, two-tier, one-mutation-path architecture is right; the gaps are all missing atoms, not wrong shape. Maps the C1-C12 recon build order against the shipped 57 bindings (C1/C3/C4/C11 shipped, C5/C6 partial, C2/C7/C8/C9/C10/C12 open), and surfaces the load-bearing finding the C1-C12 list predates: the largest mechanical campaign (a hundreds-of-sites write-seam fanout) and every seam-inversion recipe run entirely on hand-rolled textual edits in JS with char-index byte math. Prioritizes the next build: (T1) anchored/unique textual-edit atoms + a structural-survey analysis reduction + the field_inject_to_constructor atom; (T2) clone-family/structural-similarity + hardened column-spec + caller-role/framework-reachability; (T3) non-contiguous region extract + JDTLS var-type resolution. Keeps the characterization harness out of the isolate (capability-driven, not a binding). Sibling of refactor-tools-v2.md and refactor-v2-pressure-test.md."
---

# Refactor toolbox gap map: what the isolate needs next

> **Status: proposed.** Derived from live fanout-campaign practice against a
> large private Java codebase (a Vaadin/Guice/jOOQ monolith, ~350k Java LOC),
> generalized to shapes. No client identifiers appear here by invariant; the
> practice repo is described only by structural shape. Companion to
> [`refactor-tools-v2.md`](./refactor-tools-v2.md) (the in-box DSL design) and
> [`refactor-v2-pressure-test.md`](./refactor-v2-pressure-test.md) (the binding
> spike). Where those two ask "what is the minimal binding set," this doc asks
> "having shipped ~57 bindings and driven them across dozens of real
> decomposition campaigns, what is the toolbox still missing, ranked by how much
> campaign weight each gap carries."

## 1. The toolbox is an algebra (and the algebra is right)

The `isolate` surface is five cell-DSL namespaces forming one pipeline, split by
a deliberate facts-vs-analysis philosophy:

```
code.*      facts     "where is X?"          hash-anchored Spans, aggregate-capped     (8)
analysis.*  reduce    "what is X's shape?"   small structured answers, computed Rust-side (6)
java.*      transform "rewrite X"            pure {changes,creates,findings}, never writes (30)
edits.*     apply     the ONE mutation path  begin -> merge/createFile -> apply, gated    (9)
lsp.*       authority "semantic truth"       rename/move/hover over JDTLS, fail-closed    (4)
```

The invariants that make this trustworthy are documented in
`crates/bro-harness/src/bindings/AGENTS.md` and hold regardless of what a cell
does: one writer (`edits.apply`), hash-anchored spans (drift fails as
`stale_span`, never a silent structural miss), host-computed provenance
(cell-authored bytes floor at `syntax_only`), LSP fails closed (no silent
text-match downgrade), and every binding is a pure function of the working set
plus harness-owned language servers (zero daemon reach-back).

**Audit verdict: the architecture is correct.** Every gap below is a *missing
atom* or a *missing reduction*, never a wrong-shaped seam. The two-tier split in
particular is load-bearing and should not be diluted: structural questions
(cohesion, references, clone families, shape counts) belong in `analysis.*` as
Rust-side reductions, never reconstructed in JS from `code.query` captures (that
path OOMs the V8 isolate on a repo sweep).

## 2. The four load-bearing activity shapes

Real campaigns exercise ~14 recipe shapes, but they cluster into four that carry
the weight:

1. **Survey / recon (read-only).** Every concern begins here: sweep the repo by a
   detection signature (grep/structural), rank targets. This is the entry
   activity for *all* concerns, run dozens of times.
2. **Class / method decomposition.** God-class extract, monolithic-method stage
   extract, query-object extract. Well served today.
3. **Seam rewrite / inversion at scale.** Route a UI write through a command
   facade; invert a direct infra call into a posted event; convert a scheduled
   job into a thin trigger; promote field injection to constructors. These are
   *fanouts*: hundreds of near-identical mechanical sites per campaign.
4. **Behavior gating.** Compile-only is insufficient for report / calculation /
   pipeline work; these need characterization or golden-output capture.

Shapes (2) and (3) are where the transform algebra lives; (1) and (4) are
under-served, and (3) is where the biggest surprise is.

## 3. The load-bearing finding: the largest campaign runs on hand-rolled JS

The single largest mechanical campaign observed (a write-command-seam fanout on
the order of hundreds of near-identical UI save-lambda sites) and *every*
seam-inversion recipe are driven entirely by textual edits composed in
JavaScript helper functions, which re-invent the same primitives and compute byte
offsets as **char indices over the source**. The helpers carry this note
verbatim:

> "byte offsets are char indices over the source (correct for ASCII Java). Flag a
> non-ASCII view ... a byte-accurate span helper would be the construct."

Four primitives get reinvented in every seam helper:

- **unique-string-match-or-refuse** (`indexOf`, then `indexOf` again to reject
  ambiguity, then hand-build a Span),
- **insert-after-anchor-line** (insert an `@Inject` field after an anchor field),
- **insert-import-after-last-import** (because `organizeImports` block-replace has
  *dropped unrelated imports* in practice),
- **whole-method-body / superclass-clause replacement.**

The `edits.*` algebra is deliberately Span-based (byte-exact, hash-anchored) and
that is correct. What is missing is a **text-anchored convenience layer on top of
it**, so agents stop hand-computing spans in JS and stop eating the ASCII
footgun. This shape is not in the original C1-C12 recon build order because that
recon predates the fanout campaigns; it is now the highest-leverage single gap.

## 4. Exists vs gap: the C1-C12 recon against the shipped 57

The original recon (`scratch/REFACTOR_V2_RECON.md`) proposed twelve constructs in
a build order. Status against the shipped surface:

| # | Construct | Status | Evidence / gap |
|---|---|---|---|
| C1 | Field write / state partition facts | shipped | `analysis.fieldClassification` + `fieldInitializerClosure` |
| C3 | Method region analyzer | shipped | `analysis.methodRegions` |
| C4 | Extract-method-from-region | shipped | `java.extractMethodCodeBlock` (+ result-record synthesis) |
| C11 | Cleanup bundle | mostly | hygiene / organizeImports / normalizeWhitespace / removeUnusedConstructorParams; missing region-scoped whitespace (gap-eaddf7aa), spacing-aware removal (gap-e3c9be8c), dead-field removal |
| C5 | Callback externals | partial | `java.synthesizeHelperWrappers` (same-class callers only; no functional-interface callback params) |
| C6 | Grid / column-spec extractor | shipped-buggy | `java.extractColumnSpec` exists, emits invalid patches (gap-b51e39e2) |
| C2 | Caller **role** classification | open | `analysis.references` gives prod/test counts, not UI/report/calc/**framework-dispatched** roles |
| C7 | Query-object w/ context preservation | open | recipe rides generic `extractClass`; no tx/query-context-aware transform |
| C8 | Parameter-object / record extractor | open | no `java.extractParameterObject`; wide-ctor pressure blocks decomposition |
| C9 | Characterization harness | open (by design external) | capability lenses only; see §6 |
| C10 | Framework-reachability policy | open | zero-syntactic-caller-but-runtime-reachable methods -> wrong wrapper decisions |
| C12 | Clone-family / structural-similarity | open | no cross-file skeleton clustering; blocks grid-dedup + sibling-class consolidation |

Two shapes not in C1-C12, surfaced by fanout practice:

- **NEW-1: anchored / unique textual-edit atoms** (§3). The substrate under the
  biggest campaign. Filed as gap-231fbcd4.
- **NEW-2: structural-survey / shape-count reduction.** The survey activity (§2.1)
  runs on rg + targeted reads; there is no `analysis.*` reduction that counts and
  lists classes/methods matching a *structural* predicate. Filed as gap-841b5854.

Two active frontiers already in the ledger: **var-type resolution** through
cross-file receiver calls and generic static-factory records
(gap-f9db476b, gap-dceee690), and **caller-aware move import correctness**
(gap-d6033dbb) — the recent JDTLS-moves work is on this frontier.

## 5. Prioritized build order

Ranked by campaign weight, biased toward the active test-enablement /
boundary-ratchet / write-seam frontier.

**Tier 1 — unblocks the biggest fanout and the entry activity of all concerns**

1. **Anchored / unique textual-edit atoms** (`edits.*` + `java.*`).
   `edits.replaceText({file, find, replace, occurrence:"unique"})` resolving
   find -> Span host-side (byte-accurate, refuses on 0 or >1 matches), and
   `java.addImport({file, imports})` that is idempotent, insertion-based, and
   import-shadow-aware. These two subsume every hand-rolled seam helper and
   retire the ASCII byte-offset footgun. **Start here.** (NEW-1.)
2. **Structural-survey / shape-count reduction** (`analysis.*`).
   `analysis.matchShape` / `structuralSurvey`: repo-bounded count + file list +
   exemplars for a structural predicate (extends / implements / annotated-with,
   "a UI class injecting a data-access type," "a transaction owned by a view,"
   field-injection sites). One tool serves both the skeleton-survey activity and
   the baseline-and-ratchet violation counter. Heap-safe by construction (a
   reduction, not a capture fan-out). (NEW-2.)
3. **`field_inject_to_constructor` atom** (`java.*`). Already filed
   (gap-48c722cc): promote `@Inject` fields to ctor params + assign + migrate
   `new` call-sites, with a `dependency_projection` stop and telescoping-ctor
   refusal. The DI-modernization sweep and the DI limb of the write-seam.

**Tier 2 — start cross-file, not by polishing one class**

4. **Clone-family / structural-similarity analyzer (C12)** + **harden
   `extractColumnSpec` (C6 / gap-b51e39e2).** Together they mechanize the
   least-mechanized recipe (grid/column dedup) and the sibling-class
   consolidation that pattern-to-library and vendored-module campaigns need.
5. **Caller-role + framework-reachability (C2 / C10)**, one reduction.
   Wrapper-vs-migrate is a *role* decision; "zero syntactic callers but
   annotation/event-bus reachable" is a correctness trap that compiles clean.

**Tier 3 — Spine and correctness depth**

6. Non-contiguous region extract (gap-49e2ce33) + JDTLS var-type resolution
   (gap-f9db476b / gap-dceee690) for calculation-pipeline stage work; the cleanup
   tail (region-scoped whitespace, spacing-aware removal).

## 6. What to keep out of the isolate

- **Characterization / behavior harness (C9)** stays a **capability lens**, not a
  binding. It needs a running app plus data clones, which violates the
  harness-native, zero-daemon-reach-back invariant. Keep it MCP-capability-driven
  (browser + fast-clone data tier); add a thin isolate-native capture/replay/diff
  only if a probe proves the choreography repeats.
- **The v1 daemon refactor catalog** (`system-defaults/atoms/refactor/*.json`,
  the `bbox_refactor_*` plan kinds) is the retiring path (decision af3c4783). New
  capability lands as cell bindings, never as a new plan `kind`. The campaigns
  correctly ride the cell bindings; do not invest in the catalog.

## 7. Design principles for the new atoms

Any atom added here inherits the trust model; the following are the specific
tensions the new work must respect:

- **The anchored-text layer resolves to Spans; it does not become a second
  writer.** `edits.replaceText` returns/queues a Span-shaped change through the
  existing `edits.*` set, so provenance and rollback are unchanged. The
  convenience is find->Span resolution done host-side (byte-accurate), not a new
  mutation path.
- **`java.addImport` is not `organizeImports`.** It is an insertion that adds
  missing imports and is a no-op when present; it must not reformat or
  block-replace the import region (the observed import-drop failure), and must
  skip an import shadowed by a same-compilation-unit nested type.
- **The survey reduction bounds its payload to the isolate heap** (the
  `MAX_AGGREGATE_QUERY_CAPTURES` discipline). It returns counts + a capped
  exemplar set, not a capture array.
- **`field_inject_to_constructor` emits a `dependency_projection` and stops before
  apply** on non-injectable captures, mirroring `extractClass`, and refuses a
  telescoping constructor above a parameter threshold (route to collaborator
  grouping first).

## 8. Validation loop

The acceptance test for each atom is first-hand friction, not unit coverage
alone: build the `isolate` bin (`cargo build -p bro-harness --bin isolate`),
exercise the atom against a disposable worktree of a real large-Java target with
`--cell` / `--cell-file`, and confirm the seam recipes that hand-rolled the JS
can drop it. An atom is done when the recipe's `recipes/functions/*.js` helper
shrinks to a call, the ASCII footgun is gone, and the composition reads clean
(no re-derivation of facts the algebra already holds).
