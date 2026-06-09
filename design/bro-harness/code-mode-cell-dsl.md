---
title: "The cell DSL: composable in-box infrastructure for code mode"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - bro-harness
  - code-mode
brief: "The platform layer under refactor-tools-v2.md and any later in-box domain: what makes V8-cell bindings COMPOSE. Five pieces. (1) Value model: values, not refs — salvaged from the retired narf-data-model and since validated by the codex-native fallback; the cell body is out-of-context, so tools return JSON values, JS is the transform/chaining language, and context discipline bounds the cell's RETURN. (2) Addressing: the hash-anchored Span {file, byte_range, content_sha256} is the composability quantum — every fact binding returns them, every algebra consumes them, every finding carries them; drift-guarding is structural. (3) Provenance: a host-side issuance ledger (opaque ids on host-produced values, lineage recomputed host-side at the choke point) makes weakest-link semantic_status computable for adversarially careless cell code WITHOUT taint-tracking or trusting cell-supplied tags — the ref idea returns for integrity, not context economy. (4) Namespace contract: a domain ships as bindings + hand-authored TS declarations + provenance tiers + choke-point policy, composed at dispatch (absence beats filtering), never by editing the runtime. (5) Sessions/time: pure vs session-backed bindings, quiescent-boundary batching as the parallelism model, no durable promises in-box (parking is the dispatch layer's job). Tenant test: refactor (first) and diagnostics (second) must ship as namespaces with zero runtime changes. Successor to the retired narf-data-model/narf-typed-cells value-model content; explicitly does NOT resurrect box-edge selection rules or the verb taxonomy."
---

# The cell DSL: composable in-box infrastructure for code mode

> **Status: proposed; pure design, not an implementation plan.** Grounded in
> the codex-native code mode that shipped (`crates/bro-code-mode`, commit
> `9fcbd5f`). This doc owns the **platform layer**:
> [`refactor-tools-v2.md`](./refactor-tools-v2.md) is its first tenant and
> defers to it for everything below; tenant-specific design stays in tenant
> docs. Where this doc salvages from the retired NARF cluster
> ([`narf-data-model.md`](./narf-data-model.md),
> [`narf-typed-cells.md`](./narf-typed-cells.md)) it says so explicitly and
> says what it is *not* salvaging.

## 0. Thesis

A binding surface becomes a DSL when its values compose: when what one binding
returns is what the next consumes, when provenance survives the composition,
and when a new domain can join without touching the runtime. The codex-native
runtime already settled the substrate questions — values not refs, JS as the
chaining language, one trampoline through the `ToolCapability` seam. What it
has not settled, and what the first real tenant (refactor) forces, is the
**composability contract**: a shared addressing primitive (the hash-anchored
`Span`), a provenance mechanism that survives careless cell code (the
host-side ledger), a namespace contract for shipping domains, and explicit
session/persistence semantics. Decide these once, here, or decide them three
times in three tenant docs and discover they don't agree.

The standing discipline carried over from the adjudication-boundary
post-mortem (refactor-tools-v2 §2): **no rules the model must remember, only
invariants that hold regardless of what the cell does.** Every mechanism below
passes that filter; anything that required the model to cooperate was left in
the NARF graveyard.

## 1. The runtime as it ships (constraints, not aspirations)

**Terminology, pinned** (these three words are load-bearing and easy to
misread into a daemon-residency claim):

- **Host** — the Rust side of the JS↔host trampoline, inside
  `bro-harness`/`bro-code-mode`: the same process that runs the agent loop and
  the V8 isolate. The ledger (§4), the EditSet builder, and every binding
  implementation live here.
- **Language server** — rust-analyzer/jdtls/roslyn, a **child process of the
  harness**, sitting next to the working set (in the container, on the
  remote-worker gradient). When this doc says a value was "server-authored,"
  it means the language server computed it — never the daemon.
- **Daemon** — appears nowhere in this doc's mechanisms. No binding, ledger
  entry, or choke-point check references daemon state; everything travels
  with the harness when the harness leaves the box. (In today's consolidated
  deployment it all happens to *execute* inside the blackboxd process because
  the daemon links the harness in-process — but by crate ownership it is
  harness-side, and the boundary is the compile DAG, not the deployment.)

What a binding author gets today (`crates/bro-code-mode`):

- An async JS module per `exec` call, in a V8 isolate with `console`,
  `Atomics`, `SharedArrayBuffer`, `WebAssembly` deleted
  (`runtime/globals.rs:14-19`).
- `tools.*` — every enabled tool projected as an async function; calls
  trampoline through `tool_callback` to the host and resolve as promises
  (`runtime/callbacks.rs:14-73`), via the same `ToolCapability` seam and deny
  filter as the flat surface (`bro-harness/src/code_mode.rs`).
- **Batched resolution at quiescent boundaries**: the cell parks when all
  in-flight tool calls are pending (`ExecuteToPendingOutcome::Pending`,
  `runtime/mod.rs:68-81`); the harness resolves a batch and resumes. This is
  the parallelism model — `Promise.all` over bindings is real concurrency.
- `store(key, value)` / `load(key)` — JSON values persisted across cells
  (`callbacks.rs:133-191`).
- `text()` / `image()` — the only egress into model context.
- `yield_control()` / `exit()` / timers; soft timeout `yield_time_ms`
  (default 10s, `runtime/mod.rs:31-37`).
- Typing is TS declarations rendered from JSON Schema into the exec tool's
  *description* (`description.rs:442-705`) — prose the model reads, not a
  checked artifact.

Constraints that bind everything below: values crossing the JS↔host boundary
are JSON (serde); there is no host-held mutable state visible to the cell
except what bindings choose to hold (sessions, the KV, the ledger); the model
sees nothing the cell does not `text()` or return.

## 2. The value model: values, not refs — settled

Salvaged from [`narf-data-model.md`](./narf-data-model.md) §0–§1, whose
load-bearing argument has since been **validated by the shipped runtime**:

> A cell runs with the model asleep, so the entire cell body is
> out-of-context. A value in a local JS variable, passed to an in-box binding,
> never enters the model's context. Tools return values; JS is the
> transform/query/chaining language; the context discipline is to bound the
> cell's *return*, not its internal reads.

This dissolves ref handles, by-reference argument splicing, and register
vocabularies as *data-composition* mechanisms. A `WorkspaceEdit` is a JSON
value in a variable; `edits.merge(wsEdit)` is a function call; nothing crossed
into context. The residual niche narf-data-model identified — host-side splice
for values exceeding isolate heap comfort — remains a deferrable memory
optimization, not an architecture.

**Not salvaged:** the box-edge split of the KV surface ("in-box deref by known
name only, enumeration is out-box"). That was the adjudication boundary
wearing a different hat — a selection rule the model had to respect — and it
died with the rest of them. `store`/`load` is a plain KV; if a cell wants to
iterate its own keys, nothing of value is protected by stopping it.

`ref:` survives **only as an identifier** (an atom handle, a library-script
name) — never as a unit of data composition. One exception-shaped addition in
§4: host-issued values carry an opaque *issuance id* — but it rides along
inside the value envelope for provenance integrity, and a cell that ignores it
loses nothing but a provenance tier.

## 3. Addressing: the hash-anchored Span is the composability quantum

Cross-binding composition needs one answer to "where in the code?" The answer,
already implicit in v1's `FileEdit`/`TextEdit` + `original_sha256` and made
explicit here:

```ts
type Span = {
  file: string;          // workspace-relative
  byte_start: number;
  byte_end: number;
  content_sha256: string; // hash of the file content the span was cut from
};
```

Every fact binding returns Spans (`code.query` nodes carry them, `lsp.refs`
locations resolve to them, `analysis.captures` findings embed them). Every
algebra operation consumes them (`edits.replace(span, text)`). Every bounce
finding carries them, which is what makes findings *repairable
programmatically* — the repair cell addresses exactly what detection saw.

Two structural consequences:

- **Drift-guarding is free, everywhere.** The hash is captured where the bytes
  were read; `apply()` verifies it where the bytes are written. No binding
  author implements drift checks; no cell remembers to. (v1 implemented this
  once, inside refactor apply; the Span makes it a property of the address
  itself.)
- **Spans are workspace-portable.** Relative path + hash means a Span is
  meaningful across the worktree/container/machine gradient of
  [`remote-worker-boundary.md`](./remote-worker-boundary.md) — the EditSet
  artifact crossing the integration boundary is a bag of Spans plus
  replacements, validatable wherever it lands.

Position types (line/column for LSP interop) convert to/from Spans at the
binding edge; the cell-visible currency is the Span.

## 4. Provenance: the host-side ledger, not tags, not taint

Lineage-computed `semantic_status` (refactor-tools-v2 §3.3) needs an honest
mechanism. Name the threat model precisely: not malice — *carelessness*. A
cell that hand-constructs a `WorkspaceEdit`-shaped JSON object with
`provenance: "lsp_verified"` written into it, because the model pattern-matched
an example. Cell-supplied tags are therefore worthless as provenance, and full
taint-tracking through a JS heap is impossible. The mechanism that works:

- Every host-produced composite value (Span batch, WorkspaceEdit, EditSet,
  Finding) carries an opaque **issuance id**, recorded in a per-cell,
  host-side **ledger**: `id → (producing binding, authority tier, content
  digest)`.
- Algebra operations are host calls, so the host *watches composition*: when
  `edits.merge(wsEdit)` consumes a value whose id is in the ledger and whose
  digest still matches, the EditSet's lineage extends with that entry's tier.
- At the choke point, lineage is **recomputed from the ledger**, never read
  from the value. An edit assembled from unledgered material doesn't fail —
  it floors at `syntax_only`, the tier for "bytes a program manipulated
  without semantic authority."

Properties worth stating:

- **Invariant-shaped.** No cooperation required; laundering is possible and
  *priced* (you can hand-build anything, it just can't claim `lsp_verified`),
  which is the correct incentive — exactly the RX-V1 pattern of making
  authority non-authorable rather than forbidding behavior.
- **The ref idea returns, demoted and repurposed.** NARF refs existed for
  context economy and died because the cell body is out-of-context (§2). The
  issuance id is *not* a handle the cell derefs — values still travel by
  value — it is an integrity anchor for provenance. Locality economics killed
  refs; integrity resurrects only their name.
- **One algebra, three tenants.** Refactor's `semantic_status` weakest-link;
  diagnostics' truth tiers
  ([`bro-harness-diagnostics.md`](./bro-harness-diagnostics.md) — instant/
  error-tier vs flycheck vs full check is the same authority-tier idea applied
  to compiler output); and the corpus generation stamp of
  remote-worker-boundary §2 (an `indexed_hints` value carrying *which
  snapshot*) are all entries in the same `(tier, source, generation)`
  vocabulary. Define the vocabulary once, here; tenants define their tiers.

Ledger scope: per-cell by default, extended across cells through the KV — a
`store()`d value keeps its issuance ids; a `load()` in a later cell re-enters
the ledger with a `recalled` mark (tier preserved, staleness checkable via the
content digests it carries). Cheap, bounded, garbage-collected with the cell.

## 5. The namespace contract: how a domain ships

A domain (refactor, diagnostics, git, …) joins the DSL by shipping a
**namespace**, not by editing the runtime. The contract, stated as the four
things a namespace owns:

1. **Bindings** — host functions grouped under a JS namespace object
   (`code.*`, `lsp.*`, `analysis.*`, `edits.*`). Mechanically these are
   `Tool`s on the existing registry, projected into the cell the way
   `tools.*` already projects; a namespace is a *naming and documentation*
   unit over the same trampoline, composed with the same deny filter. A
   binding may also project as a flat model-facing tool (the existing `both`
   pattern) — one implementation, two projections.
2. **Declarations** — a hand-authored TS declaration block for the
   namespace's value types and signatures (`Span`, `EditSet`, `Finding`,
   …), composed into the exec description alongside the schema-rendered
   declarations of flat tools. Hand-authored is a deliberate choice over
   generated: the schema-renderer (`description.rs:442-705`) is fine for
   leaf tools, but DSL value types that flow *between* bindings need curated
   names, doc-comments, and examples — this is the v2 home of what the
   `sm-refactor-*` runbooks were for v1. Drift between declarations and serde
   shapes is a real cost; §9 carries it as an open tension rather than
   pretending generation solves it.
3. **Provenance tiers** — the namespace declares which of its bindings issue
   ledgered values and at what authority tier (`lsp.* → lsp_verified`,
   `code.* → syntax_only`, corpus reads → `indexed_hints@generation`).
4. **Choke-point policy** — if the namespace mutates anything, it routes
   through a single guarded binding and declares its detections and
   dispatch-supplied authority inputs (refactor's `apply()`; a future
   namespace's analog). Pure namespaces (diagnostics) skip this.

Composition is **at dispatch** (remote-worker-boundary §4.1): the namespaces a
cell sees are decided by brofile/surface/dispatch identity and enforced by
what is bound — absence beats filtering. Nothing evaluates policy inside the
cell.

What this contract deliberately does **not** include: a placement taxonomy.
Whether a binding "belongs" in-box is no longer a governance question
(§2 of refactor-tools-v2 buried that); it is an ergonomics question the
namespace author answers by whether cells benefit from composing it. Corpus
search can project into a cell exactly like any MCP tool already does through
the seam — what keeps corpus *residency* in the daemon is topology
(remote-worker-boundary §5), not a rule about selection.

## 6. Sessions, batching, and time

Three binding temporalities, made explicit so tenant docs stop re-deciding
them:

- **Pure** (`code.query`, `edits.*`): a function of its arguments and the
  filesystem. No session, no lifetime questions. Default; prefer it.
- **Session-backed** (`lsp.*`): the binding's first use in a workspace warms a
  session (rust-analyzer, jdtls) owned by the **harness**, keyed by
  workspace identity + language, idle-evicted — the session outlives the
  cell and the dispatch turn, never the workspace. Cold-start cost is paid
  once per workspace, which is what makes `lsp.*` viable against the 10s
  default `yield_time_ms`; long warmups ride the pending/batching path
  rather than sync awaits.
- **Cross-cell state** (the KV): values, not handles (§2); survives resume;
  carries provenance ids (§4).

**Parallelism is the batching boundary.** `await Promise.all(spans.map(s =>
analysis.captures(s)))` parks the cell once and resolves as a batch — the
runtime's quiescent-boundary design (`runtime/mod.rs:68-81`) is the
concurrency model, and binding authors should shape APIs for it (accept
batches where the host can fan out internally; never force per-item
round-trips for work the host could batch).

**No durable promises in-box.** A cell that needs work to outlive the dispatch
does not get a park/resume primitive; it returns, and *the dispatch layer*
(`bro_exec`/`bro_resume`, workflows, atoms) owns durability. This is the one
NARF fork this doc closes deliberately: the durable tier belongs to the
orchestration plane, which already has parking, signals, and supervision —
rebuilding it inside the cell runtime would be the workflow engine's inverse,
implemented twice. (Cf. [`narf-typed-cells.md`](./narf-typed-cells.md) §4,
whose durable-tier analysis is correct *about the need* and answered here by
"that tier is the daemon's existing job.")

## 7. Cross-cell composition: KV, library scripts, atoms

The persistence ladder for *code*, replacing the retired verb taxonomy with
the two rungs that survived:

- **Improvised cell** — written by the model for this task, dies with it.
  The default; most refactors stay here.
- **Library script** — a proven cell promoted to named, versioned, curated JS
  source the model recalls instead of re-derives (refactor-tools-v2 §5). Its
  contract: declared namespace dependencies, declared inputs/outputs (typed
  in the declaration surface of §5.2), and the same provenance behavior as
  improvised code — a library script earns trust by *review and versioning*,
  not by a different runtime.
- **Atom** — where an external contract is earned: a canned capability whose
  *implementation* is a cell + dispatch, consumed by MCP-only agents
  (refactor-tools-v2 §7's decided external interface). Carrying forward
  narf-typed-cells §0.1 verbatim because the misread it warns against is
  still available: **the cell is the primary abstraction; do not extend the
  atom backend taxonomy so atoms can be "backed by" cells — the atom is a
  contract wrapper over a cell, never a runtime over it.**

The KV (`store`/`load`) is the data rung of the same ladder: task-scoped
values across cells. Durable *facts* exit the cell world entirely — into the
knowledge/notes lanes via the normal out-box tools — and durable *work* exits
into dispatch (§6). The cell layer persists code and task-local values,
nothing else.

## 8. The tenant test

The platform claim is falsifiable: **a new domain ships as a namespace —
bindings + declarations + tiers + (optional) choke point — with zero runtime
changes.** Two tenants are already designed against it:

| | refactor (first) | diagnostics (second) |
|---|---|---|
| Bindings | `code.*`, `lsp.*`, `analysis.*`, `edits.*`, `apply` | `diag.instant(span)`, `diag.check(scope)`, per-mutation rider |
| Value types | Span, EditSet, WorkspaceEdit, Finding | Diagnostic (carrying Spans) |
| Provenance tiers | `lsp_verified` / `syntax_only` (+ `indexed_hints` via corpus) | truth tiers: instant/error-tier vs flycheck vs full check |
| Choke point | `apply()` with detections + dispatch-supplied authority | none (pure) |

Diagnostics is the load-bearing second tenant precisely because it is *pure*:
if the platform only works for domains with a mutation choke point, §4–§5 are
refactor-shaped, not general. Candidate third tenants (git operations beyond
the existing flat tools; corpus reads projected in-box with generation
stamps) should be evaluated with the same table — and a candidate that needs a
new *runtime* concept is the signal to come back to this doc, not to improvise
in a tenant doc.

## 9. Tensions and open questions

- **Declaration drift.** Hand-authored TS declarations vs serde reality, with
  no compiler in the loop. Mitigations are partial: round-trip tests that
  parse examples from the declarations against the serde types; generating the
  *leaf* shapes and hand-authoring only the cross-binding value types. Carried
  open — the failure mode is the model fumbling shapes, which shows up fast
  and cheap in practice, but "we'll notice" is not a design.
- **Ledger ergonomics.** The issuance envelope must survive idiomatic JS
  (`JSON.parse(JSON.stringify(x))`, spread, `.map`) well enough that *normal*
  cells keep their provenance. Digest-based re-matching at consumption (the
  host recognizes a value it issued by content, not only by id) is the likely
  backstop; cost unknown. The floor behavior (unrecognized → `syntax_only`)
  bounds the damage either way.
- **Heap pressure.** Values-not-refs means big values live in the isolate
  heap. narf-data-model's deferred host-splice niche stays deferred until a
  real workload hits it; the KV gives an escape hatch (store, drop, re-load
  slices) before any new mechanism is justified.
- **KV growth.** Task-scoped values with no decay story yet. Likely answer:
  KV lifetime = dispatch lifetime unless explicitly promoted; decide when the
  first long-running consumer appears.
- **Namespace granularity.** `code.*` vs `lsp.*` vs `analysis.*` is asserted
  by the refactor tenant, not derived. If tenants proliferate namespaces the
  way v1 proliferated plan kinds, the declaration surface bloats the exec
  description — the same prompt-budget discipline that governs tool docs
  applies, and deferral/tool-search may need an in-box analog. Watch, don't
  pre-build.

## 10. Relationship

- **Platform under** [`refactor-tools-v2.md`](./refactor-tools-v2.md) (first
  tenant; owns the refactor algebra, trust model, catalog inversion,
  migration) and [`bro-harness-diagnostics.md`](./bro-harness-diagnostics.md)
  (second tenant; its truth tiers are §4's provenance vocabulary applied to
  compiler output).
- **Sibling of** [`remote-worker-boundary.md`](./remote-worker-boundary.md) —
  Spans/EditSets are the artifacts its integration boundary accepts;
  namespace composition at dispatch is its §4.1; corpus residency vs in-box
  projection split per its §5.
- **Successor to** the retired [`narf-data-model.md`](./narf-data-model.md)
  (salvages values-not-refs, the KV, provenance-on-put; drops box-edge
  selection rules) and [`narf-typed-cells.md`](./narf-typed-cells.md)
  (salvages the atom-collapse warning §0.1 and the durable-tier *need*,
  answered by the dispatch plane; drops the verb taxonomy).
- **Builds on** the codex-native code mode (commit `9fcbd5f`,
  `crates/bro-code-mode`) and the `ToolCapability` seam
  ([`harness-daemon-boundary.md`](./harness-daemon-boundary.md) §6).
- **Touches** [`bro-harness-tool-chaining.md`](./bro-harness-tool-chaining.md)
  / [`bro-harness-clipboard.md`](./bro-harness-clipboard.md) — the ref ABI and
  registers were the pre-cell approximations of §2/§4; their context-economy
  role is dissolved, their integrity role returns as the issuance ledger.
