---
title: "Axis: Metatools"
kind: research-axis
corpus: blackbox-research
track: harness
axis: metatools
status: enriched
topic:
  - harness
  - metatools
brief: "Cross-harness axis for programmable tool composition: scripts orchestrate tool or agent leaves while keeping intermediate results outside model context. Confirmed in Codex and bro-harness code mode and Claude Code Workflows. The current Codex refresh adds runtime ownership and failure domain as a dimension independent of the JavaScript substrate."
---

# Axis: Metatools

> **Scope.** Programmable tool-composition runtimes — the ability to author scripts
> (JavaScript or otherwise) that call tools, capture results in variables, apply
> logic (loops, conditionals, fan-out), and return only the final answer to the
> model's context. The script is the orchestrator; intermediate results never enter
> context. Distinct from [subagents](subagents.md) (agent-spawns-agent delegation),
> [builtin-tools](builtin-tools.md) (flat tool surface), and [skills](skills.md)
> (progressive-disclosure instruction bundles). Metatools are a **composition
> layer** between the model and the tool surface.

## The dimension

Flat tool calls have a structural problem: every intermediate result must round-trip
through the model's context window before the next tool call. This creates three
failure modes:

1. **Context pollution** — every result bloats the context window, crowding out
   reasoning space.
2. **Latency** — each round-trip costs a full inference.
3. **Non-determinism** — the model re-derives the orchestration plan each turn
   instead of executing a fixed plan.

Metatools solve all three by inserting a programmable layer between the model and
the tools. The model (or operator) authors a script. The script calls tools,
captures results in variables, applies control flow, and returns only the final
answer. The script's runtime is the orchestrator; the model sees only the output.

The design axis: **how much of the orchestration moves from model reasoning into
deterministic code, and what the script's tool-calling surface looks like.**

## Questions a finding must answer

- **Scripting language & runtime.** What language? What sandbox (V8 isolate, Bun
  bundle, custom)? What host APIs are available/denied (no fs, no network, no
  `Math.random()`)?
- **Tool-calling surface.** How are tools exposed to the script — as typed JS
  functions (`await tools.<name>(...)`)? As a single `agent()` spawn primitive?
  Both?
- **State model.** Can the script carry state across tool calls (JS variables)?
  Across script invocations (`store`/`load`)? Across runs (saved to disk)?
- **Control flow.** What primitives does the runtime provide? Loops/conditionals
  (plain JS)? Fan-out (`parallel()`/`pipeline()`)? Barriers vs streaming stages?
  Budget-limited loops?
- **Scope & lifetime.** Does the script run within a single turn (ephemeral, fresh
  isolate each call), across turns within a session (resumable, journaled), or
  across sessions (saved and re-run)?
- **Determinism.** Is the script reproducible? Are non-deterministic JS APIs
  banned? Is the runtime resumable (cached completed calls)?
- **Authoring surface.** Who writes the script — the model (inline, per-task),
  the operator (saved command), or both? Where is it stored?
- **Result folding.** How does the final output reach the model's context — as
  tool output? As a report? As structured data?
- **Tool surface gating.** In the radical mode (`code_mode_only` / workflow mode),
  are *all* tools gated behind the script layer, or is it additive?

## Convergence / divergence

| Subject | Runtime | Tool surface | State model | Scope | Determinism | Cell |
|---|---|---|---|---|---|---|
| Codex | V8 cell actor, in-process or supervised companion (`exec`/`wait`) | `await tools.<name>(args)`; admitted tools projected as typed JS functions | JS vars (ephemeral) + atomic `store`/`load` commit to per-session KV | Within a turn; fresh cell per `exec` | Linearized termination, hierarchical cancellation, preserved yield/output boundaries | [codex](codex/codex-metatools.md) |
| Claude Code | Bun-bundled runtime | `agent(prompt, opts)` — spawns subagent; `parallel()`/`pipeline()` for fan-out | JS vars (script scope) + journal (resume cache) | Across turns (resumable within session; saved scripts across sessions) | `Math.random()`/`Date.now()`/`new Date()` banned; completed agents cached | _stub_ |
| bro-harness | V8 isolate (`exec`/`wait`) | admitted built-ins and MCP tools projected through one capability seam | JS vars + per-session `store`/`load`, with local namespace/function-store additions | Within a dispatch | In-process provider; lifecycle predates Codex's cell-actor hardening | [design](../../design/bro-harness/code-mode-cell-dsl.md) |
| Antigravity | _TBD_ | _TBD_ | _TBD_ | _TBD_ | _TBD_ | _stub_ |
| Vibe | _TBD_ | _TBD_ | _TBD_ | _TBD_ | _TBD_ | _stub_ |

> The bro-harness row changed after the original axis capture: it now ships a
> Codex-derived V8 code-mode runtime. Clipboard/ref chaining remains a distinct
> model-driven data-passing mechanism, but it is no longer bro-harness's only
> answer to keeping intermediate values out of context.

### Runtime ownership is independent of scripting substrate

The current Codex refresh adds another orthogonal dimension. "V8 isolate" names
the JavaScript substrate, not the process failure domain. The same `exec`/`wait`
contract can be implemented by an in-process isolate or by a companion process
that delegates admitted tool calls back to the parent. A finding should record
both substrate and ownership, plus terminal-state and cancellation guarantees.

Blackbox's promoted design takes a different containment cut from Codex: it
makes the entire bro-harness session a worker process and keeps V8 in that
worker. This preserves the same lesson about isolate versus process ownership
while also separating provider, tool, context, and V8 lifecycle from the corpus
and operational/fleet daemons. See the
[process topology](../../design/daemon-runtime/process-topology.md).

### Key divergence: tool-calling granularity

- **Codex code-mode**: finest grain — every individual tool (`exec_command`,
  `mcp__*__*`, `apply_patch`, …) is a typed JS function. The script composes raw
  tools directly. No subagent overhead per call.
- **Claude Code Workflows**: coarser grain — the script spawns *subagents*
  (`agent()`), each a full model call with its own context. The script orchestrates
  agents, not raw tools. Higher latency per leaf but richer reasoning per call.

This divergence is usually read as a single spectrum (fine ↔ coarse) and a single
open question (tradeoff or maturity gradient?). The atoms/NARF analysis below
argues it is better modeled as a **separate, orthogonal dimension** — *leaf
grain* — with a third value that subsumes both endpoints.

### The orthogonal dimension: leaf grain

The convergence table characterizes runtimes by their **composition substrate**
(is there a programmable runtime — V8, Bun, none). But *what the script composes*
— the **leaf grain** — is independent of the substrate. You could run a V8
substrate over subagent leaves, or a Bun substrate over raw-tool leaves. The two
reference harnesses happen to pair one substrate with one grain, which hides the
independence.

Three leaf grains, the third dominating:

| Leaf grain | What the script composes | Seen in |
|---|---|---|
| **tool** | individual typed tool functions | Codex code-mode |
| **subagent** | `agent()` spawns, one model call each | Claude Code Workflows |
| **capability** (meta-grain) | a named, versioned, effect-declared contract whose *backend* may be a raw tool, a subagent, an ensemble, or an external adapter | bro-harness / NARF (proposed) |

The **capability** grain is not a point on the tool↔subagent line — it is a
*meta-grain* whose backend can be either, plus ensemble, plus external. In
blackbox this is the **atom** (`AtomImplementation::{Deterministic, Adapter,
Profile, Workflow}`, `../../src/orchestration/atoms/types.rs:136`): one call site,
backend-polymorphic, with grain chosen by the runtime and hidden behind the
manifest contract. A capability-grain runtime is strictly more expressive than
either reference harness, because its leaf is the union of (raw tool ∪ subagent ∪
ensemble ∪ external adapter). It also makes the leaf **self-hosting**: a proven
composition distills into a new capability (`AtomProvenance::Distilled`,
`types.rs:850`) that becomes a leaf in future scripts — bounded recursion, gated
by composition policy + ancestor depth/budget (`../../src/tools/atoms/composition.rs:62-88`).

> The full canon — capability-grain leaves, the deterministic↔bro↔ensemble
> continuum, supervision-in-the-manifest, and the self-hosting distill→reuse loop
> — lives in [narf-draft2.md](narf-draft2.md) (§2–§3). NARF is the concrete
> bro-harness answer to this axis's last open invariant.

### Boundary case: bro-harness clipboard (not itself a metatool)

bro-harness's clipboard still achieves the same *goal* (keep intermediate results out of
context) through ref-chaining within the normal model-driven tool-calling loop.
The model passes `ref:abc123` instead of inlining a 50KB tool result; the ref
resolves when the downstream tool reads it. But there is no scripting runtime, no
composition language, no programmable layer interposed between the model and the
tools. The model remains the orchestrator — refs are a data-passing optimization.
This is a different mechanism on a different axis; it's noted here to sharpen the
axis definition: **metatools require a scriptable composition runtime**, not just
a way to avoid inlining results.

### Convergence: "keep intermediate results out of context"

All three converge on the same goal — intermediate results should not pollute the
model's context window. They diverge on *how*:

- **Codex**: JS variables in a V8 isolate. The isolate dies; the vars die with it.
- **Claude Workflows**: JS variables in the script runtime. The script is
  persistent; vars are ephemeral per run but the journal caches completed agents.
- **bro-harness code mode**: JavaScript variables inside the shipped V8 runtime,
  with per-session storage and local function/namespace additions.
- **bro-harness clipboard**: refs remain a second, model-driven mechanism. The
  model passes `ref:abc123` instead of inlining a large tool result and the
  downstream tool resolves it.

## Open invariants

<!-- TODO(synthesis): -->
- Is "JS as the universal composition language" a genuine cross-harness invariant,
  or an accident of both Codex and Claude Code using JS/TypeScript internally?
- ~~Does the fine-grain vs coarse-grain split represent a fundamental tradeoff or a
  maturity gradient?~~ **Reframed (2026-06-02):** the split is better modeled as
  the orthogonal *leaf-grain* dimension above, not a single spectrum — and the
  apparent dichotomy dissolves once **capability** (atom) is admitted as a third,
  backend-polymorphic grain. The remaining tradeoff is real but narrower: a
  capability leaf must declare its grain (cost, effects, supervision) so the
  runtime can schedule it. See [narf-draft2.md](narf-draft2.md) §2.
- The `code_mode_only` radical-gating pattern now exists in Codex and
  bro-harness: `exec`/`wait` are the authorial surface and flat tools remain
  deferred/loadable behind code mode. Whether Claude Workflows has an equivalent
  remains open.
- ~~Is there a bro-harness path from ref-chaining to a scriptable composition
  runtime, or does daemon-independence preclude it?~~ **Answered (shipped):**
  bro-harness now carries a harness-local V8 runtime. Host tools enter through
  the filtered `ToolCapability` seam and daemon-owned features enter through
  bottom-contract traits that fail closed when absent. The proposed cell DSL now
  builds on a real substrate rather than defining one from scratch.

## See also

- [narf-draft2.md](narf-draft2.md) — the NARF canon: capability-grain leaves,
  bounded-recursion self-hosting, and the authoring layer as primary harness
  interface. The forward synthesis this axis feeds.
- [narf.md](narf.md) — the v1 NARF braindump (breadcrumb map + exploratory
  script sketches).
- [codex/codex-metatools.md](codex/codex-metatools.md) - Codex main refresh,
  including actor lifecycle and process-owned V8.
- [../../design/bro-harness/code-mode-runtime-lifecycle.md](../../design/bro-harness/code-mode-runtime-lifecycle.md)
  - the promoted bro-harness runtime design.

## Discovery provenance

This axis was surfaced during a comparative probe of Codex code-mode and Claude
Code Workflows, 2026-06-02. Codex code-mode was live-probed against the installed
`codex 0.135.0` (Homebrew) with `--enable code_mode --enable code_mode_only`;
claims are high-confidence from direct observation of `exec` tool calls, nested
tool dispatch from JS, `store`/`load` persistence, and `ALL_TOOLS` enumeration.
The runtime-ownership and lifecycle dimensions were refreshed from Codex source
at `main@8aae858958` on 2026-07-14.
Claude Code Workflows were confirmed against the official documentation at
`code.claude.com/docs/zh-CN/workflows` and the community skill API reference at
`github.com/ray-amjad/claude-code-workflow-creator`; the feature is pre-release
(gated behind `CLAUDE_CODE_WORKFLOWS=1`), so live probes were not performed.
