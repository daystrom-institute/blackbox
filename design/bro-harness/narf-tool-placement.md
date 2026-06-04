---
title: "NARF tool placement — built-in parity and the box-edge taxonomy"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - bro-harness
  - narf
  - tools
  - tool-placement
  - box-edge
  - mcp
brief: "The next-steps plan to get NARF back to tool-calling MVP: restore parity with the pre-beta bro-harness built-in tool set (file/shell/search/git/web/promise/clipboard) inside the new runtime shapes, by classifying every built-in against the box-edge discriminator — in-box binding vs out-box (model-facing) tool vs both. Also proposes MCP config++: optional per-tool box-edge PLACEMENT in fleet.json so external MCP tools can be assigned a box side, defaulting fail-safe to out-box. 'Placement' is deliberately not called a 'surface' to avoid collision with MCP surfaces and the model-projection surface."
---

# NARF tool placement — built-in parity and the box-edge taxonomy

> **Status.** Proposed sibling to
> [`harness-daemon-boundary.md`](./harness-daemon-boundary.md) (topology),
> [`narf-capability-library.md`](./narf-capability-library.md) (authoring layer +
> the box-edge invariant §0.1), and
> [`narf-effects-and-safety.md`](./narf-effects-and-safety.md) (parked Tx/saga).
> This doc owns **tool placement**: which built-in capabilities live on which side
> of the box edge, and how external MCP tools get placed. It is the next-steps plan
> to reach **tool-calling MVP** in the NARF runtime.

> **A note on the word "placement."** "Surface" is already overloaded in this
> codebase — **MCP surfaces** (caller-selected catalog *visibility*,
> [`../surfaces/mcp/mcp-surfaces.md`](../surfaces/mcp/mcp-surfaces.md)), the
> **model-projection surface** (flat tools vs `narf_exec`, narf-draft2 §5), and the
> orientation/work surfaces (§9). This doc's concept — *which side of the sandbox
> boundary a tool's invocation lives on* — is orthogonal to all three, so it is
> called **placement**, never "surface." See §4.4 for the explicit distinction from
> MCP surfaces.

## 0. The gap (why this doc exists now)

NARF is live in the daemon (`narf_exec` → in-process V8 → exact capability
bindings), but the in-box surface today is **only** `atoms.invoke`,
`refactor.plan/materialize`, and the ref/egress family. A cell can invoke an atom
and plan a refactor — but it **cannot read a file, run a shell command, grep, glob,
edit, commit, fetch a URL, or join a promise.** Those are the bread-and-butter the
pre-beta bro-harness exposed as 35 built-in tools.

So NARF has *capability composition* without *tool-calling parity*. The goal here
is narrow and concrete: **restore the pre-beta built-in tool set inside the new
runtime shapes**, each tool placed on the correct side of the box edge. That is
"NARF back to MVP wrt tool calling."

This is *not* a feature expansion — it is parity. Every built-in already exists
(`crates/bro-tools/src/`, assembled in `builtin_tools()` at `lib.rs:40`); the work
is wiring the in-box-eligible ones as sandbox bindings and confirming the
model-facing ones project correctly.

## 1. The discriminator (recap)

From [`narf-capability-library.md`](./narf-capability-library.md) §0.1 /
[`narf-draft2.md`](../../research/harness/narf-draft2.md) §9 — a cell runs with
**no model in the loop**. The test for every tool:

> **Can this run to completion inside a cell with the model asleep, and can
> downstream code trust the result without the model having looked?**
> Yes → **in-box** (sandbox binding). No → **out-box** (model-facing direct tool).

Three placements fall out:

- **in-box** — composition over exact, already-adjudicated inputs; the result is
  mechanical and trustable without judgment.
- **out-box (model-facing)** — anything interpretive (the model must judge a fuzzy
  result), reviewable (the model must see it), or a control that enters/exits/
  reviews a cell. Results land in the model's context.
- **both** — placed on *both* sides: a model-facing flat tool **and** an in-box
  binding. Common for exact read/compute/effect tools the model also uses directly.

Governance is unchanged: a v1 cell is arbitrary code at **shell trust**
(`narf-effects-and-safety.md` §0) — no Tx, no new safety apparatus. In-box mutation
keeps only cheap *hygiene* defaults (don't clobber a dirty file, drift-guard), never
dressed as safety.

## 2. The taxonomy — every pre-beta built-in

`both` means: keep the flat model-facing tool (it already exists) **and** add an
in-box binding. `→ binding` names the proposed in-box shape (JSON-in, ref-out,
matching `atoms.invoke`).

| Built-in (file:line) | Kind | Placement | In-box binding | Why |
| --- | --- | --- | --- | --- |
| **file_read** / smart_read / list_dir (`workspace.rs:215/1096/431`) | read | **both** | `fs.read` / `fs.smartRead` / `fs.list` → ref | Path-keyed, exact, deterministic. Cell composes over file bytes; model reads to orient. |
| **content_search** (grep) (`workspace.rs:838`) | read | **both** | `search.content` → ref | Lexical regex = deterministic predicate, exact result. **Caveat:** in-box use is for *mechanical complete-set* work only; never enumerate-then-pick a single target (selection is interpretive → model-facing). |
| **glob** (`workspace.rs:1002`) | read | **both** | `search.glob` → ref | Deterministic path match; same selection caveat. |
| **file_write** / file_edit (`workspace.rs:346/731`) | mutate-local | **both** | `fs.write` / `fs.edit` | Exact operation (write bytes / replace exact string). Cell composes edits; recoverable via git. Hygiene defaults (drift-guard) stay; not safety. |
| **git_status/log/diff/show** (`workspace.rs:517–632`) | read | **both** | `git.status/log/diff/show` → ref | Exact, deterministic snapshots. Cell reads a diff to compose (e.g. summarize); model reads to orient. |
| **git_commit** (`workspace.rs:672`) | mutate-local | **both** | `git.commit` | Local, recoverable (git is the net). No `push` built-in exists — good; external push stays out of v1. |
| **shell_run** (`shell.rs:536`) | shell | **both** | `shell.run` → ref | **The anchor.** In-box `shell.run` *is* the arbitrary-execution surface that makes "cell = shell trust" true. Sync await or promise mode (§3). |
| **shell_poll / kill / list** (`shell.rs:734/841/912`) | shell/session | **both** | `shell.poll/kill/list` | Manage a cell's own shell sessions in-box; also flat tools for the conventional projection. |
| **promise_wait / when_all / when_any** (`promise.rs:428/454/485`) | async-join | **both** | `narf.promise.{all,any,pipeline}` (in-box join) + `narf_wait` (out-box, durable) | The Promise primitive. In-box: join a cell's own same-dispatch promises; add `pipeline` (no-barrier staging). Out-box `narf_wait`: durable cross-cell resume (lever). |
| **promise_status / list / cancel** (`promise.rs:402/536/516`) | async-mgmt | **both** | `narf.promise.status/list/cancel` | Introspect/control a cell's promises in-box; flat tools on the conventional projection. |
| **clip_*** (yank/set/paste/peek/list/clear/transform/slice/grep) (`clipboard.rs:359–963`) | ref ops | **in-box** (collapses into the ref substrate) | `narf.ref.*` (text/peek already exist; add put/transform/slice/grep/paste) | Clipboard registers *are* the ref precursor (narf-draft2 §4). In NARF these become the in-box ref surface; `clip:` stays a flat-projection alias. `clip_peek` = `narf.ref.text` (bounded egress). Transforms run server-side, exact. |
| **web_fetch** (`web.rs:39`) | network-read | **both** | `web.fetch` → ref | URL-keyed exact read. Cell fetches as composition; model fetches to orient. Network is an external read — allowed at shell trust, bounded egress applies. |
| **todo_write** (`todo.rs:86`) | model planning | **out-box** | — | The *model's* task tracking. A cell does not plan the model's work; nothing to compose. Model-facing only. |
| **enter_worktree / exit_worktree** (`fleet_worktree.rs:32/84`) | environment/orchestration | **out-box** | — | Worktree lifecycle is the *environment a cell runs in*, set up above it. `exit_worktree publish/merge` pushes — external, operator-attended. Not cell-composition primitives. |
| **tool_search** (registry meta) | discovery | **out-box** | — | Activates deferred tools for the model. The box never discovers. |

### 2.1 The interpretive set (already correctly out-box)

Semantic/ranked discovery — `atom_search`, `atom_describe`, `bbox_hybrid_search`,
schema/describe — is **out-box only**, by the §9.1 third-error-class guard: a fuzzy
result must reach the model's context to be adjudicated before anything depends on
it. The in-box exact counterpart is dereference-by-ref (`atoms.invoke(exactRef)`),
never search. This is settled (commit `b6abcc4`); listed here for completeness.

### 2.2 NARF controls (out-box by construction)

`narf_exec`, `narf_prepare`, `narf_run`, `narf_wait`, `narf_define` — the controls
that enter/exit/review a cell — are **model-facing tools**, never in-box bindings
(`narf-capability-library.md` §0.1). The current in-box `narf.prepare/run/define`
bindings are the mislayering to undo (wire the dead `ScriptRuntime::prepare/run`
methods to real harness Tools instead).

**`session.import` is the exception that stays in-box.** Recalling a cached helper
by exact name keeps its source host-side and out of context — the same
bloat-minimization discipline as refs — so it is an in-box *dereference*, not a
control. (No review guard on it until there is actual evidence of misresolution
harm; optimistic v1.) `trace` is recorded automatically (a host side-effect, no
binding) and read by the model out-box.

## 3. Cross-cutting notes

- **Shell is the trust anchor.** Once `shell.run` is in-box, the cell is provably
  shell-equivalent — which is exactly why no Tx/guard apparatus is built
  (`narf-effects-and-safety.md` §0): guarding `fs.edit` while `shell.run` grants
  arbitrary execution buys nothing.
- **Clipboard collapses into refs.** Don't port `clip_*` as a parallel in-box
  namespace; fold them into `narf.ref.*`. One ref substrate, not two. The flat
  `clip_*` tools remain for the conventional (non-NARF) projection.
- **The promise split is the immediate build.** In-box `narf.promise.{all,any,
  pipeline}` (harness-local, reusing `PromiseStore`) is buildable standalone now;
  out-box `narf_wait` over durable promises is the §10 lever (persisted store, home
  still open — see narf.md OQ#2). Build the in-box join first.
- **Selection is never in-box.** grep/glob/list in-box are for mechanical
  complete-set composition. The moment a result is used to *pick one target*, it is
  interpretive and must round-trip the model. Encode this in the binding docs, not
  as a runtime check (a "remember to inspect" rule dies at compaction).
- **Automatic ref resolution (parameter substitutability).** A ref in any
  *value-position* binding argument is resolved to its host-side bytes **at the host
  call boundary**, recursively through the input structure. The author passes the
  ref, never a `materialize` call, and the bytes never enter context or the JS heap
  — the value is *by-handle in the cell, by-value in the callee's arguments*. The
  mental model is a C#-style **implicit `Ref<T> → T` conversion**, with two
  refinements: (1) a `Ref` is a *settled* value, so resolution is a host-side
  **dereference**, not a forced computation — the `Lazy<T>`/await model belongs to
  `Promise` (the pending layer that resolves *to* a ref: `Promise<T>` --await-->
  `Ref<T>` --deref--> `T`); (2) the conversion marshals across the JS↔host boundary,
  not within JS. Resolution keys off the ref being a **typed value** (the
  `{ref,size,preview}` envelope or a `Ref` wrapper — matching the existing
  `refToken` normalization), never a string pattern-sniff, so a literal that reads
  `"ref:cap/3"` is passed untouched. The ref-management ops (`narf.ref.text/peek`)
  are the explicit exception — they consume the *handle*, not the value. This
  generalizes the built-ins' `from`/`stdin_from` (four hand-wired consumers today)
  to *every* in-box parameter. **Open fork:** whether a *pending* `Promise` in an
  argument position auto-awaits (one unified resolvable-handle rule) or requires an
  explicit `await` first (keeps the async boundary JS-idiomatic; the awaited promise
  yields a ref that then auto-resolves). Lean: implicit-resolve for refs,
  explicit-await for promises.

### 3.1 The type model — the Ref/Promise lattice

`Ref` and `Promise` are not a nesting choice (`Ref<Promise<T>>` vs
`Promise<Ref<T>>`); they are **orthogonal axes**:

- **`Promise` = temporal** — pending → settled (is the work done?).
- **`Ref` = locality** — by-reference (host-side, out of context) → by-value (in
  context / JS heap; are the bytes here?).

So the value space is a 2×2 product:

| | by-value (in context) | by-reference (host-side) |
| --- | --- | --- |
| **settled (now)** | `T` (a literal) | `Ref<T>` |
| **pending (later)** | `Promise<T>` (small control results — the `store`/`load` lane) | **`Promise<Ref<T>>`** (the sweet spot) |

The canonical NARF value is **`Promise<Ref<T>>`** — deferred *and* out-of-context —
exactly what an async producer (`shell.run(mode=promise)`, async `atoms.invoke`)
yields (narf-draft2 §4: "Promise … resolves to a `Ref<T>`"). It resolves
outer-to-inner: `await` (temporal) → `Ref<T>` → deref (locality) → `T`.

`Ref<Promise<T>>` is not a real second composition — "a settled handle to *pending*
work" fights the Ref-is-settled definition. Its only meaning is a **durable
promise** (addressable, survives a resume boundary, narf.md OQ#2), better modeled as
a *property of Promise* (an orthogonal third "durable" bit) than as a nesting.

This is why §3 auto-resolution and the §5 promise primitive are **one decision seen
twice**: parameter resolution collapses the **locality** axis (`Ref<T> → T`,
implicit) and *optionally* the **temporal** axis (`Promise → …` — the open fork:
implicit auto-await vs explicit `await`). Lean: implicit locality, explicit temporal.

## 4. The sticky edge — MCP tools (config++ proposal)

Built-ins we can classify by hand (above). **MCP tools are arbitrary and external;
the runtime cannot infer which side of the box edge they belong on.** A semantic
search MCP tool is interpretive (out-box); an exact `get_by_id` MCP tool is in-box
eligible. That distinction is a property of the tool's *semantics*, which only the
author knows. So it must be **declared**, not inferred.

### 4.1 Proposal

Extend fleet.json with a single **flat, top-level `tool_placement` map**, keyed by
fully-qualified tool name — mirroring the existing `pinTools` shape (a flat list of
names). It sits alongside `mcpServers` (which already carries per-server
`exclude_tools`, parsed at `mcp.rs:137`) and handles both MCP tools and built-in
re-placement in one key space:

```jsonc
{
  "mcpServers": { "blackbox": { "type": "http", "url": "...", "exclude_tools": ["..."] } },
  "pinTools": ["clip_yank", "clip_paste"],
  "tool_placement": {                                   // NEW — flat, like pinTools
    "mcp__blackbox__bbox_code_node_describe": "in-box",  // exact deref → composable
    "mcp__blackbox__bbox_hybrid_search":      "out-box",  // interpretive → model judges
    "mcp__blackbox__bbox_slice_read":         "both",
    "web_fetch":                              "out-box"   // re-place a built-in too
    // everything unlisted defaults to out-box
  }
}
```

**No per-server default.** Because the default is `out-box` and you only ever
*name the exceptions* you want pulled in-box, a per-server `placement: "out-box"`
default is a no-op, and `placement: "in-box"` for a whole server would contradict
the fail-safe rule (§4.2) by bulk-admitting unproven tools. So placement is a flat
list of named exceptions, nothing more.

### 4.2 Rules

- **Default fail-safe to `out-box`.** An undeclared MCP tool is model-facing only.
  We never silently admit an unknown tool in-box, because an unproven-exact result
  laundered into a cell is precisely the §9.1 third-error-class footgun. Unknown →
  the model judges it. This mirrors the absence-floor discipline (narf-draft2 §10):
  optimism touches movement, never soundness.
- **`in-box` MCP tools are auto-wrapped as ref-returning bindings, reusing the
  existing MCP client.** MCP is JSON-in/JSON-out, which maps cleanly onto the
  existing capability-op shape: `mcp.<server>.<tool>(input)` rides the same async
  bridge as `atoms.invoke` (add an `McpInvoke` `CapRequest` variant), stores its
  result host-side, and returns a `{ref,size,preview}` envelope. Bounded egress
  applies uniformly. The implementer is the **existing MCP client in
  `bro-harness/mcp.rs`** (which already runs in-daemon and owns fleet.json's
  `mcpServers` + `ToolFilter`) — *not* a new MCP-proxy service in the `blackbox`
  crate. bro-script sits at the bottom of the crate DAG and can't depend up, so the
  client arrives as an injected trait (`Arc<dyn McpInvoke>`); that injection is the
  only reason a trait exists, not a process boundary (harness and daemon are one
  process). One MCP client, one config path, in-box and out-box alike. (That bullet
  is ref-*out* — results → refs. Ref-*in* — **automatic ref resolution** of the
  tool's *arguments*, §3 — applies here too: an in-box MCP tool's inputs get ref
  substitution for free, so a ref can be passed as any argument.)
- **`both`** places the tool as a flat model-facing tool *and* an in-box binding —
  for tools the model uses to orient and a cell uses to compose (e.g. an exact
  code-graph deref).
- **Placement composes with filtering, but the filter wins.** `exclude_tools` /
  `pinTools` and the allow/disallow `ToolFilter` decide *whether a capability
  exists at all*; `placement` only decides the box side(s) of a survivor. A denied
  capability is neither in-box nor out-box. See §4.5 — this is a soundness
  invariant, not a convenience.

### 4.3 Decisions (resolved 2026-06-04)

- **Config shape → flat top-level `tool_placement`** (§4.1), no per-server default.
  The fail-safe default makes per-server defaults a no-op; a flat named-exception
  list mirrors `pinTools`.
- **Default → fully explicit, no heuristic.** A name-prefix heuristic
  (`get_`/`read_` → in-box) is itself an exactness inference, and name ≠ exactness
  (`get_recommendations` can be ranked; `read_summary` can be an LLM). It re-opens
  the §9.1 laundering hole. In-box admission is a trust decision — stated, not
  guessed. Same one-time config cost as `pinTools`.
- **In-box MCP home → reuse `bro-harness/mcp.rs`, injected as a trait** (§4.2). No
  new `blackbox`-crate MCP-proxy and no generic `bro-capabilities` MCP trait; the
  injection exists only because of the crate DAG, not a process split.

### 4.4 Placement is not an MCP surface

[`../surfaces/mcp/mcp-surfaces.md`](../surfaces/mcp/mcp-surfaces.md) defines an MCP
**surface**: a caller-selected *view of the catalog* (`?surface=readonly`,
packet-routed), answering **"may this caller see/call this tool at all?"** —
visibility/filtering. **Placement** answers a different question entirely:
**"for a tool that is visible, which side of the sandbox boundary does its
invocation live on?"** They are orthogonal and compose cleanly:

1. MCP surface decides the *visible* tool set (existing filter machinery).
2. `exclude_tools` / `pinTools` further filter / tier within that.
3. **Placement** then assigns each surviving tool a box side (this doc).

A tool hidden by surface is never placed (it does not exist for that caller); a
visible tool is placed `out-box` by default. Keep the vocabularies distinct:
*surface = what you can see; placement = which side of the box you use it from.*

### 4.5 The filter gates the capability, not a presence (the box is not a deny-bypass)

A `both` tool has **two presences**: the flat model-facing tool and the in-box
binding. The allow/disallow `ToolFilter` (`mcp.rs:77`; built-ins filtered at
registry construction, `registry.rs:109`) must therefore gate the **capability**,
not a single presence:

> **Invariant.** Denying a capability removes **both** its presences. If
> `content_search` is denied, there is no flat `content_search` tool **and** no
> in-box `search.content` binding. Otherwise an agent forbidden from grepping just
> greps from inside a cell, and the box becomes a silent route *around* the
> filter — the narf-draft2 §7 "authoring layer is never a host escape hatch" rule,
> violated.

This makes the gating usable for real policy — e.g. deny `content_search`/`glob` to
**force an agent toward a semantic search tool** you left allowed. For that to hold,
the in-box gating must be true in both directions: a denied capability is
unreachable in-box, and (the build requirement below) the in-box binding set is
constructed *behind the same filter* as the flat surface.

**Build requirement.** Today's in-box bindings (`atoms`/`refactor`) are injected via
`Capabilities`, a path that **bypasses `ToolFilter`** — harmless while those two are
the only bindings (intentionally present), but the moment `fs`/`shell`/`search`/
`git` go in-box (§5 steps 2–4), the in-box binding set **must** be filtered by the
same `ToolFilter` that gates the flat built-ins. An unfiltered in-box surface is a
deny-bypass.

**Ordering (refines §4.4).** filter (does this capability exist?) → placement (which
box side(s) for survivors). Out-box eligibility and in-box eligibility are gated by
the *same* filter.

**Future lever — per-presence targeting.** v1 keeps the simple model: a deny removes
the whole capability (both presences). If a real need appears to target *one*
presence — e.g. "keep the flat `content_search` tool for the model to use directly,
but deny the in-box `search.content` binding so cells can't grep blindly," or the
inverse — extend the filter syntax to address a presence (e.g. a `:in`/`:out`
suffix, `content_search:in`). Do **not** build this until a concrete policy needs
it; the whole-capability deny is the right default and the simpler surface.

### 4.6 Worked example — ref-in to an in-box dispatch

The payoff case, combining automatic ref resolution (§3), in-box MCP placement
(§4.1), and the recursion guard — compose a dispatch input from a ref so a large
prompt never enters the orchestrator's context:

```js
const prompt = await fs.read("prompts/review.md");  // settled ref, stays host-side
const out    = await bro.exec("reviewer", prompt);   // ref resolved at the host boundary
```

The prompt bytes never enter the orchestrating model's context or the JS heap; the
host materializes the ref into the dispatched bro's input at the call boundary.
Cross-dispatch, the value lands in the *new* bro's input — its ref store is separate,
so this is materialize-into-prompt, not a shared handle.

This requires `bro_exec` placed `in-box` via `tool_placement` (§4.1, fail-safe means
you name it explicitly), and it rides the **mechanical recursion guard** + ancestor
depth/budget tree **unchanged** — in-box dispatch is composition, never a guard
bypass (narf-draft2 §7). `bro_*` orchestration tools are *not* pre-beta bro-tools
built-ins (so they're absent from the §2 table); they reach the box only through
explicit MCP placement, recursion-guarded.

## 5. MVP sequencing (next steps)

> **Status (see `harness-daemon-boundary.md` §15 for the canonical ledger).**
> Steps 1–4 (host-access seam + read/shell/mutation bindings) and step 5
> (promise primitive) have **landed** on `beta/blackbox-v2`. Step 5 shipped the
> in-box join (`all`/`any`/`wait`/`status`/`list`/`cancel`) + a pure-JS
> `pipeline`; the strict per-promise `Promise<Ref<T>>` split is deferred as 5b.
> Remaining: step 6 (clip→ref fold), step 7 (MCP config++), and the independent
> authoring-mislayer fix.

Ordered by capability delivered, smallest correct increments. Each is
standalone-buildable in `bro-script` unless noted.

1. **Host-access seam (the real foundation).** Extend `Capabilities` with a
   workspace/shell capability backed by the existing bro-tools impls
   (`workspace.rs`, `shell.rs`), wired **behind the same `ToolFilter`** as the flat
   built-ins (§4.5), + bridge variant(s) + bootstrap bindings. This is what unblocks
   steps 2–4. *(Impl note for build time, not now: this may collapse to one "invoke
   a bro-tools builtin by name" bridge + ergonomic `fs.*`/`shell.*` wrappers rather
   than N bespoke traits — `Tool::call` already takes a `ToolCx`. Decide then.)*
2. **Read core** in-box: `fs.read/smartRead/list`, `search.content/glob`,
   `git.status/log/diff/show`, `web.fetch` → ref-returning bindings. Pure parity.
3. **In-box `shell.run`.** The trust anchor; unlocks real composition. Sync await
   first; promise mode folds into step 5.
4. **In-box mutation: `fs.write/edit`, `git.commit`.** Hygiene defaults only.
5. **Promise primitive:** in-box `narf.promise.{all,any,pipeline}` over
   `PromiseStore`, + `shell.run` promise mode depositing into refs. (`narf_wait` /
   durable = deferred lever.)
6. **Fold `clip_*` into `narf.ref.*`** (put/transform/slice/grep/paste).
7. **MCP config++** (§4): flat `tool_placement` parsing + in-box MCP bridge
   (reusing `mcp.rs`), default out-box, gated by `ToolFilter` (§4.5).

**Independent track (not a blocker): undo the authoring mislayer.** Pull
`narf.prepare`/`narf.run`/`narf.session.define` out of the bootstrap (keep in-box
`session.import`); wire `ScriptRuntime::prepare/run` to real model-facing
`narf_prepare`/`narf_run` tools that return the rendered source to context
(`narf-capability-library.md` §0.1/§6). This has **no parity value** and does not
block the bindings above; land it before leaning on prepared-script review for
multi-step *mutating* scripts.

Out of scope for MVP (stay out-box / deferred): `todo_write`, `enter/exit_worktree`,
`narf_wait`/durable promises, NARF-lib/decay, Tx.

## 6. Relationship

- **Owns** the tool-placement taxonomy; defers topology to
  [`harness-daemon-boundary.md`](./harness-daemon-boundary.md), authoring shape to
  [`narf-capability-library.md`](./narf-capability-library.md), and effect/safety
  disposition to [`narf-effects-and-safety.md`](./narf-effects-and-safety.md).
- **Distinct from** [`../surfaces/mcp/mcp-surfaces.md`](../surfaces/mcp/mcp-surfaces.md):
  surfaces decide tool *visibility*; placement decides box *side* (§4.4).
- **Applies** the [`narf-draft2.md`](../../research/harness/narf-draft2.md) §9
  surface split and §10 lever discipline to the concrete built-in set.
- **Grounded in** the bro-harness built-in inventory: `crates/bro-tools/src/`
  (`workspace.rs`, `shell.rs`, `promise.rs`, `clipboard.rs`, `web.rs`, `todo.rs`,
  `fleet_worktree.rs`), assembled in `lib.rs:40` `builtin_tools()`; MCP injection +
  `ToolFilter` in `crates/bro-harness/src/mcp.rs`.
