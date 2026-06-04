---
title: "NARF data model: one durable KV, values not refs"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - bro-harness
  - narf
  - store
  - kv
  - values
  - box-edge
brief: "Collapses NARF's two half-built value systems — the bro-script `RefState`/ref-envelope substrate (§9-1) and the clipboard `Registers`/`clip_*` chaining ABI — into ONE durable session KV. The load-bearing argument: a cell runs with the model asleep, so the entire cell body is out-of-context; a value in a local JS variable passed to an in-box tool never touches the model's context. That dissolves the need for ref handles, by-reference argument splicing, and the whole clip_* chaining vocabulary. Tools return values; JS does transforms/chaining over values the cell already holds (the box runs JS, which subsumes jq/grep/slice) — but discovery/enumeration over the store is out-box, never in the cell; a small `narf.encode` covers non-JS-native output formats (yaml/csv/…); the context discipline is to bound the cell's RETURN, not internal reads (re-scoping the mis-targeted egress budget). The KV is `<name, JSON value>`, persistent across resume AND daemon restart, with optional author tags/provenance on `put`. Its surface is box-edge-split on the 'box never selects' invariant: in-box `narf.kv.*` is exact-deref-by-known-name only — set/get/peek/delete, NO enumeration — while `list`/`keys` are out-box `narf_kv_*` (the model surveys + selects which keys a cell derefs; enumeration is the front half of selection, so it stays model-facing, like corpus.search per b6abcc4). 'ref' survives only as an identifier (`atom:reviewer@v1`), never as a unit of data composition. Supersedes narf-tool-placement.md (clip-fold + Ref/Promise lattice + auto ref-resolution) and the §9-1 ref substrate."
supersedes: "narf-tool-placement.md (clip-fold §5.6 + Ref/Promise lattice §3.1 + automatic ref-resolution §3); the §9-1 ref substrate (bro-script RefState / ref envelopes, commit ba2ae02)"
---

# NARF data model: one durable KV, values not refs

> **Status.** Proposed; this is the converged design from a live jam. It replaces
> the ref-as-data-composition model. The *code* still has refs everywhere (the
> §9-1 substrate, the host-access seam returning ref envelopes, the ref-out
> promise join) — §8 is the unwind path from there to here. Verify against code;
> treat §1–§7 as the target, not current behavior.

## 0. Thesis

> NARF had **two** value systems, each half-built: the bro-script `RefState` ref
> substrate (handles to tool output, in-box, **per-dispatch — dies on resume**)
> and the clipboard `Registers` + `clip_*` chaining ABI (named registers,
> out-box, `side`-persisted). They do the same job — *a host-side value behind a
> handle, read on demand* — neither completely, and one of them is a restart
> failure pit. Collapse them into **one durable session KV**.
>
> The unlock: **a cell runs with the model asleep, so the whole cell body is
> out-of-context.** A value in a local JS variable, passed to an in-box tool,
> never enters the model's context — the model only ever sees what the cell
> *returns*. That single fact dissolves ref handles, by-reference argument
> splicing, and the entire `clip_*` chaining vocabulary. **Tools return values.
> JS is the transform/query/chaining language. The context discipline is: bound
> the cell's *return*, not its internal reads.** "ref" survives only as an
> *identifier* (`atom:reviewer@v1`), never as a unit of data composition.

## 1. Why refs and clip_* both go

Walk the capabilities of the retired systems and test each against a plain
durable KV + JS:

| ref / clip_ capability | covered by KV + JS? |
| --- | --- |
| big value held out of context | ✓ KV stores host-side; only a handle/summary need cross |
| bounded read | ✓ `get` is egress-bounded |
| inspect without materializing | ✓ `peek` (size + head/tail summary) |
| a handle crosses, not the bytes | ✓ the **name** is the handle (human-readable — better) |
| stash a processed tool result / file read | ✓ it is just a value |
| chaining (`into`/`from`/`stdout_to`/`stdin_from`) | ✓ store the value, read it next step |
| `clip_transform` (jq) | ✓ JS — reshaping JSON *is* what JS does |
| `clip_grep` / `clip_slice` | ✓ JS — `.filter(l => /re/.test(l))`, `.slice()` (JS has RegExp natively) |
| provenance | ✓ an `origin` field + optional `put(value, { tags })` |

Nothing survives the test. Two claims people reach for, and why they fail:

- **"By-reference passing is unique to refs."** The supposed win — hand a tool a
  handle and have the host splice the full bytes in *at the call boundary* so the
  value never enters the cell — buys **nothing for context here**, because the
  cell is already out-of-context. `const x = kv.get('p'); await tool({ p: x })`
  keeps `x` out of the model's context exactly as well; `x` touches the V8 heap
  transiently, but the V8 heap during a cell **is not** the model's context. The
  only residual value of a host-side splice is **heap pressure for values larger
  than the isolate limit** — a niche memory optimization, deferrable, not a
  context necessity. (And it was never built — refs don't auto-resolve into args
  today either.)
- **"Refs guarantee exactness the agent can't forge."** True, but it only ever
  protected the (unbuilt) auto-resolution path, and a full-JS cell can pass any
  value to any tool regardless. The "box never selects → effect" rule
  (`narf-capability-library.md` §0.1, commit `b6abcc4`) stays a **cell-authoring
  discipline**, not a store-enforced property. Provenance you still *want* is an
  `origin` field + author tags, not a second immutable system.

And refs as built are **broken on restart**: a ref handle is a per-dispatch
pointer into `RefState`, which is rebuilt empty on resume — a serialized handle
becomes a dangling pointer. (Checked: the `RefEnvelope` is `Serialize`, but its
backing `RefEntry` lives in the per-runtime store and is never persisted.)

## 2. The one durable KV

A single session store, conceptually `Map<name, Entry>`:

```
Entry = {
  value:        JSON value,          // string | number | object | array
  origin:       "tool" | "agent",    // who wrote it (audit; not a trust gate)
  tags?:        object,              // author-supplied provenance/metadata
  content_type: string,
  size:         bytes,
  summary:      { lines, head[], tail[], truncated },   // peekable, cheap
}
```

- **Named by the author.** Agent slots get human names; tool/capability output
  that a tool chooses to deposit gets an auto-name. Same store, `origin`
  distinguishes.
- **Values are plain JSON** — string, object, array. The point (the use cases
  that justify it): stash something **expensive to build**, **pass it more than
  once**, or **accumulate across rounds** (e.g. push records into an array over
  several cells).
- **Durable.** Survives `exec→resume` *and* daemon restart (JSON serializes
  trivially). This is the whole reason it replaces refs — see §6 for the home.
- **`get(name)` is an exact deref** by the name you chose — same shape as the old
  ref read, just your namespace. No taint, no policing.

### 2.1 Box-edge placement (the load-bearing distinction)

Same discriminator as every other surface (`narf-capability-library.md` §0.1,
commit `b6abcc4`): **in-box is exact deref of names the author already holds; the
box never enumerates, discovers, or selects.** Enumeration is the front half of
selection, so it is model-facing. The KV surface splits on that line:

| op | in-box `narf.kv.*` (cell; model asleep) | out-box `narf_kv_*` (model tool) |
| --- | --- | --- |
| `set(name, value, {tags}?)` | ✓ store under a name you chose | — (model writes by authoring a cell; OQ §9) |
| `get(name, maxBytes?)` | ✓ exact deref of a **known** name | ✓ **bounded** retrieval into context |
| `peek(name)` | ✓ exact (metadata) of a **known** name | ✓ inspect a known entry (summary, no bytes) |
| `delete(name)` | ✓ known name | — (OQ §9) |
| **`list()` / `keys()`** | **✗ — enumeration is discovery/selection** | ✓ the model surveys and **selects** which keys a cell will use |

Reading it:

- **The cell operates only on keys it already holds** — names it `set` itself, or
  names the model chose out-of-box and wrote into the cell source. It **cannot ask
  "what's in the store?"** That is discovery, and discovery is the model's job.
- **No in-box `list` ⇒ no in-box search/filter-and-pick — by design, not by
  omission.** A cell that could enumerate could enumerate-then-select, which is
  exactly the interpretive act the box must not do (the reason `corpus.search` is
  out-box, `b6abcc4`). The model discovers and selects via the out-box tools and
  hands the cell exact names; the cell derefs them. Filtering is not an in-box
  capability.
- **Accumulation needs no enumeration.** `get('records') ?? []` → push →
  `set('records', …)` works on a name *you chose*. A cell that juggles many
  dynamic keys keeps its own index under a known name (`get('record_keys')`); it
  still never enumerates the store.
- **Out-box `list`/`keys` is therefore load-bearing, not sugar** — it is the
  *only* place enumeration/selection over the store lives, and it is the
  principled successor to `clip_*`'s model-facing inspect role. `peek`/`get` are
  available out-box too (inspect/retrieve a known key, bounded), but the model
  **never bulk-pulls a value to "see" memory** — it `list`s/`peek`s (summaries).

## 3. Tools return values

In-box tools (`fs.read`, `search.content`, `git.diff`, `shell.run`, `web.fetch`,
`atoms.invoke`, …) **return their result value directly into the cell** — out of
context during execution, so no handle indirection is required:

```js
const text = await fs.read('big.json');   // the bytes, in the cell's JS — not in context
const obj  = JSON.parse(text);
```

The KV is **opt-in persistence**, reached only when the agent wants to *keep*,
*re-pass*, or *accumulate* a value:

```js
const recs = narf.kv.get('records') ?? [];
recs.push(obj.summary);
narf.kv.set('records', recs);             // survives into the next round / resume
```

(A tool whose result is enormous may still choose to deposit into the KV and
return a name instead of materializing in the isolate heap — that is the §5 heap
concern, an optimization, not the default contract.)

## 4. Transforms are JS; `narf.encode` for formats

- **Transforms = JS.** The box runs JS, which strictly subsumes `clip_transform`
  (jq), `clip_grep` (regex — `RegExp` is native), and `clip_slice` (`.slice`).
  Reshaping a value the cell *already holds* is `obj.items.map(x => x.title)` —
  mechanical composition over an exact input, in-box.
- **Search/enumeration is NOT in-box** (§2.1). The box transforms values it holds;
  it does not discover what's in the store. Surveying/selecting *which* entries to
  operate on is the model's job, via the out-box `narf_kv_list`/`peek` — it then
  hands the cell exact names. (This is the §2.1 invariant, restated so "transforms
  are JS" is not misread as "queries over the store are JS in-box.")
- **The one genuinely new primitive — `narf.encode`.** Once the lingua franca is
  JS, "turn this object into another format" recurs. Most of it is already JS
  (`JSON.stringify`, template strings → markdown/prose). The only gap is
  **non-JS-native encoders** — `narf.encode.{yaml,csv,toml}` and a markdown-table
  helper. A small pure library, **not** a store and not a chaining system. (Open:
  the exact format set — §9.)

## 5. The context discipline: bound the return, not the reads

The §9-1 egress budget was mis-scoped: it bounded bytes pulled into the JS heap
"(i.e. model context)" — but **JS-heap-during-a-cell is not the model context.**
The model's only context surface is **what the cell returns** (the `narf_exec`
tool_result; plus an uncaught error string, an edge the author controls). So:

- **Internal reads** (`kv.get`, a tool's returned value) are bounded only by
  **memory** (the isolate heap limit) — they never reach context.
- **The cell's return** is what must be bounded: a cap on the returned value, and
  the discipline "**return a summary or a kv name, not a blob**." Previews/
  summaries are small by construction.
- The out-box `narf_kv_get` retrieval *does* enter context, so it stays
  **egress-bounded** — that is the one place a "context budget" genuinely applies.

Net: one rule — *bound what crosses back to the model* — replaces a cumulative
read budget that was policing the wrong boundary.

## 6. Serialization, resume, restart

- KV values are JSON → serialize trivially → ride the harness session **`side`
  spine**, which already survives `exec→resume` *and* daemon restart (resumable
  sessions persist to disk). This is the same spine the clipboard used; `clip_*`
  dies, the spine stays (it is the cluster keystone — `bro-harness.md`).
- **Store home is the open fork (§9):** the `side`-backed harness store (reuse
  the restart-proof spine, least new code) vs a **daemon-side** durable store
  (the boundary doc §9 "durable refs/state are daemon-side"; more central, better
  for cross-bro coordination). Both survive restart.
- Because there is no separate ref store, there is **no dangling-handle class** —
  a stored value is the value, and it persists. (The niche exception: a value
  bigger than the isolate heap; see §5/§9.)

## 7. Retired vs retained

**Retired:**

- bro-script `RefState` + `RefEntry` + `RefEnvelope` (the §9-1 substrate).
- `narf.ref.*` as a data surface (`text`/`peek`/the never-built `put`).
- The **Ref/Promise lattice** + `Promise<Ref<T>>` framing (narf-draft2 §4/§3.1)
  and **automatic ref-resolution** (narf-tool-placement §3) — by-reference arg
  splicing (deferred to a niche heap optimization, not a data model).
- `clip_*` tools (yank/set/paste/peek/list/clear/transform/slice/grep).
- The `into`/`from`/`stdout_to`/`stdin_from` chaining ABI + clipboard `Registers`.
- "ref" as a unit of data composition. **Kept:** "ref" as an *identifier* for a
  thing (`atom:reviewer@v1`, a name a tool resolves).

**Retained (unchanged by this doc):**

- The **box-edge invariant** (`narf-capability-library.md` §0.1) — indeed this
  doc *applies* it to the KV surface (§2.1).
- The **in-box tool taxonomy** (which built-ins are in-box: fs/search/git/shell/
  web) — they now **return values** instead of ref envelopes.
- The **promise primitive** (`narf.promise.{all,any,…}`) — but join results are
  **values / kv entries**, not ref envelopes (§8).
- The model-facing **controls** `narf_exec`/`narf_prepare`/`narf_run`/
  `narf_define` and in-box `narf.session.import` (the mislayer fix stands).
- **MCP placement** (`tool_placement`, narf-tool-placement §4) — still a valid
  pending concern; see §9.

## 8. Implications for landed code (the unwind)

This reverses direction on code that shipped this session; most of it is
mechanical (swap "return/consume a ref envelope" for "return/consume a value,
opt-in kv"):

1. **Delete** bro-script `RefState`/envelope/egress-budget-on-reads; **add** the
   KV (entries + `set`/`get`/`peek`/`list`/`delete`, summaries) and the
   bound-the-return cap.
2. **Host-access seam (steps 1–4):** `fs.*`/`search.*`/`git.*`/`shell.*`/
   `web.fetch` return values, not `{ref,size,preview}` envelopes. `op_tool_invoke`
   loses its ref-wrapping; the cell gets the value (heap-bounded).
3. **Promise primitive (step 5):** join returns values; the ref-out variant goes.
   `op_tool_invoke_inline` stays the shape for control results.
4. **`narf_prepare`** keeps a prepared-script *handle*, but it is a KV/store entry
   (`narf-script` origin) rather than a ref — same mechanics, unified store.
5. **Out-box tools:** add `narf_kv_list`/`narf_kv_peek`/`narf_kv_get`.
6. **`clip_*` + `into`/`from`:** retire (a deletion-ledger arc; touches
   `file_read`/`shell_run`/`content_search`/`web_fetch`/`file_write`).
7. **Docs:** archive narf-tool-placement.md (superseded); refine the boundary doc
   §15 ledger + §9 ref taxonomy to point here.

## 9. Open decisions

- **Store home** — `side`-backed harness store vs daemon-side durable (§6).
- **Out-box writes** — out-box `list`/`keys`/`peek`/`get` are required (the only
  enumeration/selection path, §2.1). The open part is *writes*: expose
  model-facing `narf_kv_set`/`delete`, or keep out-box inspect-only and route all
  writes through cells? (Lean: inspect-only first.)
- **`narf.encode` format set** — yaml/csv/toml/markdown-table? Which ship v1.
- **Return cap** — the concrete bound on a cell's return value, and how summaries
  are surfaced when it is exceeded (no silent truncation).
- **Large-value heap path** — the niche where a value exceeds the isolate heap:
  raise the limit, a host-side splice for that case, or a tool that deposits to
  the store and returns a name. Deferred until it bites.
- **Provenance depth** — what `origin`/`tags` carry, and whether anything reads
  them beyond audit.

## 10. Relationship

- **Supersedes** [`narf-tool-placement.md`](./narf-tool-placement.md): its
  clip-fold (§5 step 6), Ref/Promise lattice (§3.1), and automatic ref-resolution
  (§3) are retired here. Its still-live parts carry forward: the in-box tool
  taxonomy (§2 — implemented) and **MCP placement** (§4 — pending; see §9 here).
- **Supersedes** the **§9-1 ref substrate** (commit `ba2ae02`) recorded in
  [`harness-daemon-boundary.md`](./harness-daemon-boundary.md) §15, and refines
  that doc's §9 ref taxonomy (durable KV replaces the `ref:*` namespaces;
  durable-vs-ephemeral collapses into "the store persists").
- **Refines** [`../../research/harness/narf-draft2.md`](../../research/harness/narf-draft2.md)
  §4 (`Ref` substrate) and §7 (bounded egress): the substrate is a durable KV, and
  egress bounds the cell's *return*, not internal reads.
- **Applies** [`narf-capability-library.md`](./narf-capability-library.md) §0.1:
  the KV surface is box-edge-split (§2.1), and authoring controls stay model-facing.
- **Hub:** [`bro-harness.md`](./bro-harness.md).
