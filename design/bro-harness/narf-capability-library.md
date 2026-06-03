---
title: "NARF capability library and prepared scripts"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - bro-harness
  - narf
  - authoring-layer
  - capabilities
  - compaction
brief: "A proposed middle layer for NARF: agent-authored JS helpers move from cell-local code to session-local helpers to decay-managed reusable library functions, with capability discovery as an agentic navigation loop and mutating scripts executed only after prepare-time rendering, diagnostics, and ref creation."
---

# NARF capability library and prepared scripts

> **Status.** Proposed sibling to
> [`harness-daemon-boundary.md`](./harness-daemon-boundary.md). The boundary doc
> owns topology: in-process harness placement, `bro-capabilities` traits,
> durability placement, and daemon/harness crate rules. This doc owns the NARF
> authoring-library layer: how agents discover, keep, prepare, execute, and
> eventually publish reusable JS helpers inside the V8 sandbox.

## 0. Thesis

NARF needs a layer between "a helper I wrote in this one cell" and "a published
atom with schemas, effects, supervision, versioning, and provenance." That layer
is a **capability library** over sandboxed JS/TS modules:

```text
cell-local helper
  -> session-local helper/import
    -> NARF-lib candidate
      -> hot NARF-lib function
        -> atom, when it deserves a full capability contract
```

The library is not a package registry bolted onto search. Its front door is an
**agentic capability negotiation loop**: the agent states "I need to do X", the
runtime searches atoms, library helpers, prior prepared scripts, traces, and
tool-sequence recipes, then returns compact route cards with signposts for the
next query/action round.

The runtime should make the right path easy to continue, not merely rank text
matches.

## 1. Why this exists

NARF draft 2 already has the promotion endpoint: a proven program can be
distilled into an atom. That is the right final form for stable, budgetable,
supervisable capabilities, but it is too heavy for every useful composite an
agent discovers mid-session.

Agents repeatedly need smaller reusable shapes:

- "run cargo check, capture rustc JSON, repair if possible"
- "ground a Rust item, inspect exact kind, then plan the semantic refactor"
- "open the project context, recall relevant rules, and bundle evidence"
- "after a tool sequence succeeds, summarize only the trace and refs"

If every such shape stays in model memory, compaction loses it. If every shape
is published immediately, the shared library fills with unproven residue. The
middle tier gives agents working memory without durable library pollution.

## 2. Persistence tiers

NARF state should not rely on hidden V8 heap surviving across turns. Correctness
and replay should behave as if every authoring turn starts from a fresh isolate
plus an explicit session frame.

The tiers:

| Tier | Lifetime | Discoverability | Use |
| --- | --- | --- | --- |
| `cell-local` | One prepared/executed cell | Not discoverable | Local variables, temporary closures, one-off composition |
| `session-local` | Current NARF session | Visible only in the session frame | Selected routes, import aliases, draft helper functions, breadcrumbs, refs |
| `NARF-lib` | Cross-session | Searchable by capability scout | Reusable helpers with telemetry, decay, and promotion history |
| `atom` | Cross-session, contractual | Atom catalog | Stable capability with schemas/effects/supervision/versioning |

Session-local state is explicit:

```text
session imports
  Stable aliases to atoms or NARF-lib functions.

session routes
  Chosen capability route cards and why they were selected.

session refs
  Handles to values, plans, diagnostics, traces, and prepared scripts.

session draft functions
  JS helpers authored in this session and callable by later cells.

session breadcrumbs
  Compact notes such as "route X chosen because LSP is available; public API
  opt-out was not supplied."
```

Mutable JS module globals are not semantic state. A helper can persist state only
by writing refs, traces, session metadata, or library telemetry.

## 3. Capability negotiation, not plain search

> **Correction (2026-06-03) — scout is not an in-box service.** The
> `narf.capabilities.scout(...)` and route-card shapes below are the right
> *contract* (intent → signposted routes with `fit` / `requires` / `effects` /
> `next` / `stop_if`) and the right *discipline* (the grounding sequence, §3.1) —
> but they are **not** an in-box sandbox binding. Discovery is *interpretive*, so
> it lives on the **direct orientation surface**: normal model-facing tool calls
> (`atom_search` / `atom_describe` / `bbox_hybrid_search`) whose responses the
> model reads in context and adjudicates. The in-box surface exposes only *exact*
> dereference-by-ref (`atom("atom:foo@v1")`), never search or
> enumerate-and-filter — the box dereferences, never selects. See
> [`../../research/harness/narf-draft2.md`](../../research/harness/narf-draft2.md)
> §9 (orientation/work surface split; the exact-vs-interpretive / CQRS boundary;
> facade enforcement). Read the route-card fields below as the shape of what those
> *direct* tools return / what the model reads — not as a `narf.capabilities.*`
> binding callable from authored JS.

The opening move is a statement of intent, not a package lookup:

```ts
const scout = await narf.capabilities.scout({
  intent:
    "Move a Rust item into a new module, preserve imports, run cargo check, " +
    "and repair compiler fallout if possible.",
  context: {
    project,
    language: "rust",
    expectedEffects: ["edit", "shell"],
    constraints: [
      "do not infer operator opt-outs",
      "LSP-backed refactors fail closed"
    ]
  }
});
```

The response is a set of route cards, not a ranked list of documents:

```json
{
  "routes": [
    {
      "kind": "existing_atom",
      "handle": "atom:rust.move-item@v2",
      "fit": "high",
      "summary": "RA-backed move-item flow with import preservation.",
      "requires": ["rust-analyzer available"],
      "effects": ["writes_files", "runs_shell"],
      "next": [
        {"tool": "atoms.inspect", "reason": "review schema/effects"},
        {"tool": "code.symbols", "reason": "confirm exact item identity"}
      ]
    },
    {
      "kind": "narf_lib_function",
      "handle": "narf:rust.checkWithRepair@v3",
      "fit": "medium",
      "summary": "Cargo-check plus compile-fix loop; compose after an edit plan.",
      "next": [
        {"tool": "narf.lib.inspect", "reason": "review examples and effects"}
      ]
    },
    {
      "kind": "tool_sequence",
      "fit": "fallback",
      "summary": "No single capability covers the full request.",
      "next": [
        {"tool": "code.symbols", "reason": "ground item identity"},
        {"tool": "refactor.plan", "reason": "produce typed edit plan"},
        {"tool": "narf.session.define", "reason": "save successful composition"}
      ]
    }
  ],
  "missing_facts": ["exact item name", "destination module"]
}
```

This is the Daystrom `AgenticTools` lesson applied to NARF. The useful response
surface is compact data plus next-step breadcrumbs: inspect this, traverse that,
confirm this precondition, bundle this evidence, or do not use this route unless
a condition holds. Hybrid retrieval and decay affect ranking; the agentic
surface affects convergence.

### 3.1 Grounding sequence as tool surface

> **See** [`../../research/harness/narf-draft2.md`](../../research/harness/narf-draft2.md)
> §9 (the orientation/work surface split) and §10 (v1 scope): the grounding
> *sequence* below is v1 discipline and "step 2 / scout" is performed on the
> **direct orientation surface** (model-facing `atom_search`/`atom_describe`/
> `bbox_hybrid_search`), not an in-box runtime call; only the retrieval *engine*
> that pre-narrows a large catalog is a deferred lever.

The scout surface should encode the opening discipline directly. Future agents
should not have to remember a prose runbook or re-derive the phase order after
context compaction. The tool and its responses carry the sequence.

Canonical NARF grounding sequence:

```text
1. State intent
   Agent says the work it is trying to do, in natural language, with light
   structured context: project, language, expected effects, constraints, and
   known missing facts.

2. Scout capability routes
   Runtime searches atoms, NARF-lib helpers, session helpers, prepared-script
   traces, and tool-sequence recipes. Search can use RRF/vector/BM25/decay, but
   the returned object is not a search result page.

3. Return signposts
   Each route card says what it is, why it fits, why it may not fit, what it
   requires, what effects it may produce, and the exact next tool calls likely
   to disambiguate or prepare it.

4. Inspect narrowly
   Agent follows signposts: inspect atom/helper schema, confirm LSP/capability
   availability, ground symbols, fetch examples, or ask the operator for an
   authority flag. The inspected tool responses return their own next-step
   breadcrumbs.

5. Select or compose
   Agent chooses an existing atom, imports a NARF-lib helper, keeps a
   session-local helper, composes a tool sequence, or authors a new helper.

6. Prepare
   Agent submits JS for `narf.prepare`; the runtime renders the assembled script,
   resolves aliases, validates policy/effects/syntax/imports, and returns a
   prepared-script ref.

7. Run and record
   Agent runs the prepared ref. The trace records route choice, helper imports,
   refs, effects, success/failure, and promotion/decay telemetry.
```

The route card is therefore a prompt/tool hybrid: it is compact enough to stay in
context, but directive enough to improve search success. The important fields are
not only `score` and `summary`; they are `fit_reason`, `nonfit_reason`,
`requires`, `missing_facts`, `effects`, `next`, and `stop_if`.

Example compact card:

```json
{
  "kind": "narf_lib_function",
  "handle": "narf:rust.checkWithRepair@v3",
  "fit": "medium",
  "fit_reason": "Matches the validation/repair half of the intent.",
  "nonfit_reason": "Does not perform the initial move-item refactor.",
  "requires": ["cargo available", "Tx already open"],
  "effects": ["runs_shell", "may_write_files"],
  "missing_facts": ["whether previous edit plan already ran"],
  "next": [
    {
      "tool": "narf.lib.inspect",
      "args": {"handle": "narf:rust.checkWithRepair@v3"},
      "reason": "Review examples, source digest, and effect declaration."
    },
    {
      "tool": "narf.session.keepImport",
      "args": {"alias": "repair", "handle": "narf:rust.checkWithRepair@v3"},
      "reason": "Keep alias for later prepared cells if selected."
    }
  ],
  "stop_if": [
    "No Tx is available and the active surface forbids mutation outside Tx.",
    "The current brofile disallows cargo commands."
  ]
}
```

Tool descriptions should also carry directive prompts. A capability-scout tool
description should say "call this before authoring a multi-step NARF script";
an inspect tool should say "use this before import/run"; a prepare tool should
say "mutating scripts run by prepared ref, not by reconstructed source." These
are not documentation flourishes. They are part of the navigation contract.

## 4. Session-local helpers

Agents should be able to keep a helper for the session without publishing it:

```ts
await narf.session.define("rustCheckAndRepair", {
  source: `
    export async function run(narf, { project, tx }) {
      const check = await narf.shell.run({
        command: "cargo",
        args: ["check", "--message-format=json"],
        capture: "rustc_json",
        captureTo: "ref:diag/rustc/last",
        onFailure: "continue_for_repair"
      });

      if (await tx.hasOpenObligations()) {
        const fix = await narf.refactor.compileFixRound({
          project,
          diagnostics: "ref:diag/rustc/last"
        });
        await tx.apply(fix);
      }

      return await tx.summary({ maxBytes: 4000 });
    }
  `,
  exports: ["run"],
  effects: ["shell", "refactor_plan", "writes_files"],
  reason: "Used after structural Rust edit plans in this session."
});
```

Later cells import it by session name:

```ts
const repair = await narf.session.import("rustCheckAndRepair");
await repair.run(narf, { project, tx });
```

The session helper can be promoted to a NARF-lib candidate when it is used
successfully or accepted by the operator. Promotion is explicit; session-local
helpers are not globally discoverable by default.

## 5. NARF-lib lifecycle and decay

> **Deferred lever.** Cross-session NARF-lib + decay/telemetry is not v1 — it is
> not-yet-applicable rather than merely heavy: there is nothing to decay until
> cross-session usage accumulates. It arrives with cross-session persistence; see
> [`../../research/harness/narf-draft2.md`](../../research/harness/narf-draft2.md)
> §10 (levers). v1 keeps only session-local helpers (§4).

Published library functions are cross-session, searchable, and decay-managed.
They are lighter than atoms but still structured:

```json
{
  "handle": "narf:rust.checkWithRepair@v3",
  "exports": ["run"],
  "source_digest": "sha256:...",
  "summary": "Run cargo check with rustc JSON capture and optional compile-fix round.",
  "effects": ["shell", "refactor_plan", "writes_files"],
  "examples": ["ref:trace/prep-1234"],
  "telemetry": {
    "successful_uses": 14,
    "failed_uses": 2,
    "accepted_uses": 6,
    "rejected_uses": 1,
    "last_used_at": "2026-06-03T00:00:00Z"
  },
  "state": "active"
}
```

Lifecycle states:

| State | Meaning |
| --- | --- |
| `candidate` | Proposed from a session trace; searchable only when candidate results are requested |
| `active` | Used enough to appear in normal capability scout results |
| `pinned` | Operator/project-pinned; no decay out of hot results |
| `dormant` | Still inspectable, but removed from hot suggestions |
| `retired` | Hidden unless explicitly requested |
| `promoted` | Superseded by an atom or newer helper |

Decay affects visibility, not provenance. A stale helper should become dormant,
not disappear. Deletion is a separate operator action.

Example scoring shape:

```text
score =
  accepted_uses * 3
  + successful_uses
  + recent_use_bonus(half_life_days)
  - failed_uses * 2
  - rejected_uses * 5
  - stale_age_penalty
```

This score is one ranking feature inside capability scout. It should not replace
semantic fit, policy fit, effect fit, or route completeness.

## 6. Prepare before run

Compaction boundaries make hidden state dangerous. For mutating scripts, NARF
authoring should be two-step:

> **See** [`../../research/harness/narf-draft2.md`](../../research/harness/narf-draft2.md)
> §10 floor #2 (the freeze defends against compaction corrupting *what executes* —
> an agent-memory problem, not a world-movement one, so it survives the optimistic
> v1 cut) and §9.2 (the model is the only bridge collapsing non-exact → exact; the
> resolved artifact carries exact refs only, and `@latest` is pinned to a concrete
> `@vN` here at prepare).

```text
prepare(source)
  -> resolves imports/session aliases/refs/policy
  -> renders the full assembled script
  -> validates syntax, imports, effects, refs, and policy
  -> stores a prepared script ref

run(prepared_ref)
  -> executes exactly the prepared artifact
  -> records trace and telemetry
```

The agent submits authored JS:

```ts
const prepared = await narf.prepare({
  source: `
    const plan = await route.plan({ item, destination });
    await tx.apply(plan);
    await repair.run(narf, { project, tx });
  `,
  imports: ["route", "repair"],
  tx: "required"
});
```

The tool returns a prepared ref plus compact diagnostics:

```json
{
  "ref": "ref:narf-script/prep-8f31",
  "status": "ready",
  "diagnostics": [],
  "effects": ["writes_files", "runs_shell"],
  "next": {"tool": "narf.run", "args": {"ref": "ref:narf-script/prep-8f31"}}
}
```

For read-only one-liners, direct `narf.exec` may be allowed, but internally it
should still create an implicit prepared ref for trace and replay.

## 7. Inline annotations in rendered scripts

The full prepare envelope can become too bulky, and compaction can separate an
agent from the import table. The dry-run render should therefore annotate the
assembled script inline:

```ts
// @narf.import repair narf:rust.checkWithRepair@v3 sha256:8f31...
const repair = await narf.lib.bind("narf:rust.checkWithRepair@v3", {
  digest: "sha256:8f31..."
});

// @narf.session route session:route/rust-move-selected
// @narf.resolves route atom:rust.move-item@v2
const route = await narf.session.bind("route");

// @narf.effect writes_files
// @narf.effect runs_shell
// @narf.requires tx
const plan = await route.plan({ item, destination });
await tx.apply(plan);
await repair.run(narf, { project, tx });
```

The compact response envelope can then stay small, while the prepared script is
self-describing at the point of use.

Rules:

- Inline annotations are generated by `prepare`, not trusted as authority.
- The stored prepared artifact metadata remains the authority.
- If the agent writes annotations, `prepare` validates, rewrites, or rejects
  them.
- Include annotations only for imports, session aliases, refs, effects, and
  policy caveats that change how the script should be reviewed.
- Full metadata remains inspectable through `narf.prepare.inspect(ref,
  full=true)`.

## 8. Prepare-time validation

`prepare` should reject or warn before effects happen:

- illegal JS/TS syntax
- unknown import aliases
- stale helper digest
- absent daemon/capability binding
- disallowed tool under the active surface
- write effect outside a `Tx`
- shell command without required declaration
- LSP-backed operation when LSP is unavailable
- missing operator-authority opt-out for a route that requires it
- ref handle not found or wrong kind
- route/atom/helper effect mismatch

The output should be compact but actionable:

```json
{
  "status": "blocked",
  "diagnostics": [
    {
      "severity": "error",
      "code": "missing_operator_authority",
      "message": "Route atom:rust.move-public-item@v1 requires acknowledge_public_api_change.",
      "next": "Ask the operator; do not infer this flag."
    }
  ]
}
```

### 8.1 Two axes of validation — what `prepare` may guarantee

The list above mixes two kinds of check with opposite freshness properties, and
they must be treated differently or the green light lies.

- **Axis 1 — the authored surface (settled at prepare, a real guarantee).**
  Subject is the NARF script itself: parse, typecheck against the pinned capability
  `.d.ts`, alias/digest resolution, effect-annotation match, policy (RX-V1
  pass-through, surface allowlist, write-outside-`Tx`). These have **no TOCTOU** —
  the script is frozen and the declarations are digest-pinned, so nothing moves
  between prepare and run. This is where `prepare` earns a green light it can stand
  behind. (Note the gradient the §8 list under-specifies: "illegal JS/TS syntax" is
  parse-level; the load-bearing check is *typecheck against the capability schemas*
  — declaration-as-enforced-gate, not declaration-as-prompt-hint.)
- **Axis 2 — executing effects (recorded assumptions, re-verified at apply).**
  Subject is the target tree: LSP availability, file hashes, dirty/stale state,
  capability-bound-at-run. These are time-sensitive; `prepare` can record the
  world-version it assumes but **cannot guarantee** it. The guarantee arrives only
  at the version-correlated apply (window=0 / DX-2), never from a prepare-time
  precondition.

Consequence for `status`: it must be **scope-honest** — a green `ready` is a
*scoped* claim ("ready under {worktree-hash, LSP-as-of-prepare}, axis-2 re-checked
at apply"), never "all good." Don't lie in the one-liner; push the scope detail into
the artifact. And **inherit the diagnostics invariants rather than re-derive a
weaker freshness story**: DX-1 (synchronous, rides the action), DX-2
(version-correlate, drop-stale — "silence-or-truth, never stale-lie"), DX-3
(precision over recall), DX-4 (scope-honest payload) from
[`bro-harness-diagnostics.md`](./bro-harness-diagnostics.md). Axis 1's window=0
analogue is a synchronous typecheck rider on the authoring result; axis 2's is the
window=0 instant-tier rider on the apply.

Per [`../../research/harness/narf-draft2.md`](../../research/harness/narf-draft2.md)
§10, v1 runs axis-1 to completion and treats axis-2 as advisory pre-flight (the
optimistic stance), with the version-correlated apply as the lever that upgrades
axis-2 from assumption to guarantee.

## 9. Promotion to atom

NARF-lib helpers are code modules; atoms are capability contracts. Promotion is
appropriate when a helper needs one or more of:

- a stable input/output schema
- declared effects enforced independently of source review
- supervision or reviewer/advisor policies
- durable versioning and supersession
- composition-budget accounting
- non-JS backend selection (deterministic Rust, profile bro, workflow, adapter)
- operator-visible catalog entry

Promotion should preserve evidence:

```text
NARF-lib function
  -> prepared script traces
  -> accepted/rejected telemetry
  -> AtomProvenance::Distilled { distilled_by: "narf-lib", ... }
```

The atom envelope is the graduation path, not the default persistence mechanism.

## 10. Placement constraints

This doc must not weaken the harness/daemon boundary:

- The V8 sandbox sees explicit bindings only.
- In consolidated mode, daemon-backed capabilities arrive through
  `bro-capabilities` traits, not by the harness depending on `blackbox`.
- In standalone/degraded mode, absent capability bindings fail closed.
- Session-local helpers and library functions inherit the same surface
  evaluator and call-time checks as flat tools.
- Mutating helpers execute through prepared refs and `Tx` discipline.
- Cross-turn state lives in refs, traces, session frame entries, or library
  metadata, not in hidden V8 heap.

The design does not decide the nested atom `Tx` vs saga fork. Prepared scripts
should record enough effect metadata that either execution semantic can be
implemented later without changing the authoring surface.

## 11. First spike

A narrow spike should prove the lifecycle without building the whole catalog:

1. Surface discovery through the **direct orientation tools** (§3 correction) over
   a small fixed universe — one atom, one session helper, one tool-sequence
   recipe — returning route-card-shaped responses the model reads. Not an in-box
   `narf.capabilities.scout` binding.
2. Add `narf.session.define` / `narf.session.import` for session-local helpers.
3. Add `narf.prepare` that renders a full script with inline annotations and
   syntax/import diagnostics.
4. Add `narf.run(ref)` for prepared refs, read-only first, then one `Tx`-bounded
   mutating flow.
5. Record helper-use telemetry in the trace.
6. Add manual `narf.lib.promote_candidate(ref)` from a successful session helper.

Success is not a polished library. Success is proving that an agent can move
from intent negotiation to session-local helper reuse to prepared execution
without relying on post-compaction memory of hidden imports or tool layering.

## 12. Relationship

- **Sibling to** [`harness-daemon-boundary.md`](./harness-daemon-boundary.md):
  this doc owns authoring-library behavior; the boundary doc owns topology and
  durability placement.
- **Extends** [`../../research/harness/narf-draft2.md`](../../research/harness/narf-draft2.md):
  it fills the gap between ephemeral `Script` and promoted `Atom`. Corrected/scoped
  by draft2 §9 (orientation/work surface split, exact-vs-interpretive boundary),
  §10 (v1 scope and deferred levers), and §7.1 (stale-hash vs dirty-worktree).
- **Builds on** [`bro-harness-tool-chaining.md`](./bro-harness-tool-chaining.md):
  refs, promises, and hidden value flow are the substrate that make helpers
  compact and replayable.
- **Learns from** Daystrom's `AgenticTools.cs`: tool descriptions and tool
  results should carry next-step breadcrumbs, typed handles, and evidence
  closure hooks instead of returning search hits alone.
