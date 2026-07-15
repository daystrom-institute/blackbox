---
title: "Harness Research — Tracks & Charter"
kind: research-hub
corpus: blackbox-research
track: harness
topic:
  - harness
  - charter
brief: "Hub and charter for the harness research track: the study of agentic coding CLIs (Claude Code, Codex, Gemini/Antigravity, Vibe, …). Defines the axes/dimensions we care about, the matrix ontology (subject × axis), the frontmatter schema and status/confidence model, the supersession pattern across versions, the shape and legal posture of binary/source mining, and the contract a researcher agent follows when it picks up a leaf. The north star: extract the idioms that let bro-harness steer agents toward tooling competently, without context bloat."
---

# Harness Research — Tracks & Charter

> **Scope.** This track studies *agentic coding CLIs* as **agent-facing
> systems** — everything a harness exposes to the model it drives: transport,
> the agent loop, context assembly, built-in and MCP tooling, subagents, hooks,
> skills. It does **not** study end-user UX, billing, or marketing surfaces
> except where they leak into agent-facing behavior. The reference subjects are
> Claude Code, Codex, Gemini/Antigravity, and Vibe; most are open source or
> reversible for interop.

This page is the **nav waypoint and the charter**. Start here. It defines *what*
we study (the axes), *how* findings are filed (the ontology + schema), and the
*contract* a researcher agent follows. Per-axis depth lives in the linked axis
docs; per-subject findings live in the subject folders.

## 1. Why this track exists

bro-harness (`crates/bro-harness`) is our custom headless coding agent. Its
quality bar is **"rock-solid, super-stable, highly idiomatic"** — it should
steer agents toward the right tool at the right moment *without* burdening them
with context bloat. That is a fine line: too little guidance and the agent
flails; too much and every turn pays a token tax for tools it won't use.

The harnesses that walk that line best — Claude Code, Codex — are mature,
heavily-iterated systems. Their idioms are recoverable: open source where
available, reverse-derivable from shipped binaries where not. This track mines
those idioms, grades them by confidence, and feeds the synthesis to the design
corpus. We adopt *idiom*, not transcription.

## 2. The matrix: subject × axis

Harness research is a **matrix**. Rows are subjects (a specific harness); columns
are axes (a dimension of agent-facing behavior). Every leaf doc is one cell.

| Layer | File | Role |
|---|---|---|
| **Track** (this) | `harness-tracks.md` | charter: axes, ontology, contract |
| **Axis** (invariant) | `compaction.md`, `transport.md`, … | the cross-harness model for one dimension — what is *common*; a living synthesis that cites the cells |
| **Subject snapshot** | `claude/claude-2.1.160.md` | a point-in-time index for one harness version: the **one** provenance block + the per-axis checklist |
| **Finding** (leaf) | `claude/claude-compaction.md` | one cell: how *this subject* does *this axis* |

Ownership is clean and non-duplicating:

- **Provenance lives once**, on the subject snapshot. A leaf points at its
  snapshot; it does not re-state the binary path, version, or extraction method.
- **Invariants live on axis docs.** "All harnesses converge on X; they diverge
  on Y" is axis-doc content, synthesized from the cells.
- **Findings are the cells.** Subject-specific evidence, confidence-tagged.

## 3. The axes (what we study)

Sixteen axes in four clusters. The first ten were defined top-down in the
initial pass; five (marked **†**) were added from a bottom-up **codex-lens
discovery pass**; one (marked **‡**) was added from a **comparative probe** of
Codex code-mode and Claude Code Workflows — see [§3.1](#31-provenance-of-the-axis-set). Each axis has an
invariant doc (the column header, elaborated) and one finding per subject (the
cells).

### Wire & session — the substrate

- **[Transport & feature flags](transport.md)** — API shape (Messages /
  Responses / Chat-Completions / custom), control headers and beta gates
  (`effort`, fast mode, "thinkiness"/reasoning budgets, cache TTL,
  token-efficient tools, interleaved thinking), streaming envelope, and
  transport-channel fallback (WS → SSE → HTTP).
- **[Robustness](robustness.md)** — retry/backoff (jitter, `Retry-After`),
  mid-stream error recovery, idle timeouts, `pause_turn`/resume, context-overflow
  recovery, role-alternation repair, spurious-stop detection. The qualities that
  separate a naive client from a production one.
- **[Compaction](compaction.md)** — shrinking a near-full window: summarizer
  prompts, trigger math (proactive pre-sampling vs reactive overflow), split
  point, verbatim-retention tail, server-side vs client-side, fit-trimming so the
  compaction request itself fits.
- **[Session lifecycle & history](session-lifecycle.md)** † — persisted sessions:
  resume, fork, and **rollback/rewind** of turn history (distinct from
  compaction's forward-summarize), plus spawn-tree topology (depth/path/graph).
  *(tier-2: overlaps compaction & subagents — de-conflict as cells land.)*

### The loop — control flow

- **[Agent loop](agent-loop.md)** — the core turn loop: turn boundaries,
  `end_turn`/stop detection, parallel tool calls, tool-result threading,
  interrupts, operator steering mid-flight, recursion/guard behavior.
- **[Context management](context-management.md)** — what enters and persists in
  the window across the turn lifecycle: system-prompt split, markdown overlays
  (`CLAUDE.md`/`AGENTS.md`/`GEMINI.md`), first-turn injections, subsequent-turn
  reminders/nudges, env context, todo reinjection, ordering and cadence. (Sibling
  of compaction: assembly is what goes *in*; compaction is what comes *out*.)
- **[Planning & goal state](planning-goals.md)** † — model-facing intent/progress
  the harness tracks across turns: a per-turn plan checklist and a durable,
  budgeted goal contract (status lifecycle, continuation injections). Distinct
  from the loop mechanics and from a flat todo.

### Tool surfaces — the agent-facing API

- **[Built-in tools](builtin-tools.md)** — the built-in tool suite: the *nature*
  of each tool, the **shape** of the surface (arg ergonomics, batching, return
  shapes, error feedback), and the **tooldoc steering language** — the wording
  that pushes the agent toward a tool and away from anti-patterns. *The single
  highest-value extraction for bro-harness.*
- **[MCP tooling](mcp.md)** — MCP integration: SSE/progress/streaming, and how
  tool **search / deferred loading / discovery** works (surface tools by name,
  load schemas on demand) — the anti-bloat keystone.
- **[Subagents](subagents.md)** — subagent/task systems: spawn, isolation
  (worktree), result return, parallelism, agent-type registries.
- **[Hooks](hooks.md)** — hook seams: events, payloads, blocking vs advisory,
  and how hook output is injected back into context.
- **[Skills](skills.md)** — skills/slash-commands: discovery, invocation,
  progressive disclosure, argument passing.
- **[Metatools](metatools.md)** ‡ — programmable tool composition: scriptable
  runtimes (JS in V8/Bun) that call tools, capture results in variables, apply
  logic (loops, conditionals, fan-out), and return only the final answer to
  context. The composition layer above the flat tool surface. Distinct from
  subagents (agent-spawns-agent) — metatools compose *tools* directly.

### Autonomy & governance † — long-horizon, bidirectional

*This cluster is the region the top-down first pass structurally missed (see
§3.1). It is where safe, long-running autonomy is governed.*

- **[Privilege, sandboxing & approvals](privilege-approvals.md)** † — the
  bidirectional permission model: the harness declares the operating envelope
  (sandbox mode, network, writable roots, denied reads); the model negotiates
  per-call escalation (with model-authored justification + reusable rules) and
  proposes durable policy amendments; a reviewer (human / auto-review /
  LLM-guardian) adjudicates with a decision cardinality the model branches on.
- **[Memory & persistence](memory-persistence.md)** † — durable cross-*session*
  memory the model reads/writes (extract→consolidate pipeline, model-writable
  notes, scoped injection at session start). Distinct from compaction
  (within-session, forward-summarizing).
- **[Modes, personas & roles](modes-personas.md)** † — named, swappable
  behavioral layers: operating modes (plan/execute/pair), persona
  (communication style, persisted), and agent roles (model+tools+sandbox+identity
  config layer). Distinct from context injection — these are behavior contracts.

> **The "agent-facing suite" is the union of these axes.** When a new capability
> none of the sixteen captures cleanly appears, propose a new axis here rather
> than wedging it into an ill-fitting cell — and record how it was surfaced
> (§3.1).

### 3.1 Provenance of the axis set

The axis set is itself a versioned, evolving artifact — not a fixed truth.

- **Pass 1 (top-down, 10 axes).** Derived from the request framing — "what does a
  harness expose to the model each turn?" Strong on the wire, the loop, and the
  tool surface.
- **Pass 2 (bottom-up, +5 axes †).** A codex-lens discovery pass mined
  `openai/codex` source *as a lens* (not to document codex) for agent-facing
  invariant shapes the first pass missed, with multi-agent convergence. It
  surfaced four clearly-missed first-class axes (privilege-approvals,
  planning-goals, memory-persistence, modes-personas) plus one tier-2
  (session-lifecycle), and confirmed two pass-1 axes (deferred tiering in `mcp`,
  channel fallback in `transport`) as genuine cross-harness invariants. Several
  existing axes also gained **Codex-lens extensions** sections.
- **Pass 3 (comparative probe, +1 axis ‡).** A live probe of Codex code-mode
  (`--enable code_mode --enable code_mode_only`, V8 `exec`/`wait` + `await
  tools.<name>(...)`) and a documentation review of Claude Code Workflows
  (`CLAUDE_CODE_WORKFLOWS=1`, JS orchestration scripts with `agent()`/
  `parallel()`/`pipeline()`) revealed a shared design pattern neither the
  pass-1 nor pass-2 axis set captured: a **programmable tool-composition
  layer** between the model and the flat tool surface. Codex composes raw tools
  from JS (fine grain); Claude Code composes subagents from JS (coarse grain);
  both converge on "keep intermediate results in script variables, not context."
  The **metatools** axis captures this dimension. Surfaced 2026-06-02 during a
  follow-on probe after the codex-lens discovery pass.
- **The meta-lesson.** The pass-1 framing (per-turn request→response) structurally
  under-weighted the **session-spanning, bidirectional-governance** region —
  exactly the "Autonomy & governance" cluster. Pass 3 reinforces the lesson from
  a different angle: the pass-1 and pass-2 axes captured *individual* tool
  surfaces but missed the **composition layer above them** — the programmable
  runtime that calls tools without round-tripping through the model. Future
  passes should keep triangulating against multiple harnesses (e.g. antigravity,
  mistral-vibe) before treating the axis set as settled.

## 4. Subjects

| Subject | Folder | Current snapshot | Backend / transport | State |
|---|---|---|---|---|
| Claude Code | `claude/` | [`claude-2.1.160.md`](claude/claude-2.1.160.md) | Anthropic Messages | **seeded + backfilled** (worked exemplar) |
| Codex | `codex/` | [`codex-main-8aae858958.md`](codex/codex-main-8aae858958.md) | OpenAI Responses | **enriched** source snapshot; supersedes 0.136.0 |
| Antigravity | `antigravity/` | _seeding_ | Google / Antigravity | **replaces deprecated Gemini CLI**; source: `~/repos/antigravity-cli` + installed 1.107.0 |
| Vibe | `vibe/` | [`vibe-2.9.6.md`](vibe/vibe-2.9.6.md) | Mistral (`mistral-vibe`) | stub; source: `~/repos/mistral-vibe` + installed 2.9.6 |

> **Subject churn (2026-06-02).** Gemini CLI is deprecated; the `gemini/` folder
> is being migrated to `antigravity/` (Antigravity 1.107.0). Vibe is Mistral's
> `mistral-vibe`. Both sources were cloned to `~/repos/`. New-axis cells (the
> five † axes) are seeded per subject as each is mined, not pre-generated.

this host), Copilot CLI, Cursor agent.

## 5. Frontmatter schema

All research docs share `corpus: blackbox-research` and a `topic` list (the
Obsidian tag spine). The `kind` selects the layer; typed scalar fields make the
matrix machine-queryable.

```yaml
# Hub (research-corpus.md, harness-tracks.md)
kind: research-hub
track: harness            # on track hubs

# Axis (invariant) doc
kind: research-axis
track: harness
axis: compaction          # the column key

# Subject snapshot
kind: research-subject
track: harness
harness: claude           # the row key
version: "2.1.160"
platform: linux-x86_64
captured: "2026-06-02"
supersedes: null          # or a prior snapshot path
status: stub|researching|enriched|verified

# Finding (leaf)
kind: research-finding
track: harness
harness: claude           # row
axis: compaction          # column
version: "2.1.160"        # snapshot observed against
last_verified: "2.1.160"
status: stub|researching|enriched|verified
confidence: high|medium|low|mixed|unknown
```

`topic` should carry at least `[harness, <harness>, <axis>]` on findings so the
Obsidian tag pane and graph wire them up.

## 6. Status lifecycle & confidence

**Doc-level `status`** tracks research progress (the pickup state):

- `stub` — frontmatter + skeleton only; nothing mined. **Pick these up.**
- `researching` — an agent is actively mining; partial content.
- `enriched` — content landed and sourced, but not independently cross-checked.
- `verified` — claims cross-checked (re-mined, live-probed, or confirmed against
  a second source).

**Inline per-claim confidence** (a doc mixes confidence, so tag at the claim,
not just the doc — `confidence:` in frontmatter is the document's *blend*):

- **high** — verbatim string literals from the binary/source (prompts, tool
  descriptions, reminder text); directly observed wire captures.
- **medium** — decoded minified/obfuscated logic (threshold math, call graphs);
  structure reliable, exact call graph best-effort.
- **low** — inferred from behavior without a confirming artifact.

Mark open questions explicitly (`<!-- TODO(mine): … -->` or an **Open** section)
so the next agent sees the frontier, not a false sense of completeness.

## 7. Supersession across versions

Leaves are **version-agnostic** — they hold our *current* understanding. The
**snapshot** is the temporal anchor.

- A snapshot carries `version` + `supersedes:` (the prior snapshot, or `null`).
- Each finding carries `last_verified:` (the version its claims were last
  confirmed against).
- When a new harness version ships: create a new snapshot (e.g.
  `claude-2.2.0.md`, `supersedes: claude-2.1.160.md`), walk its checklist, and
  **re-mine only the axes whose behavior changed**. Unchanged leaves keep their
  old `last_verified` (= "believed stable, last confirmed at X"); a stable leaf
  may simply note "no change since 2.1.160 — see prior snapshot." That is the
  natural supersession: most cells stay put, a few move.

## 8. The shape & nature of mining

How a finding gets enriched, and what is worth extracting.

**Provenance (on the snapshot, once).** Source (install path), version, arch,
binary size, extraction method (`strings -n1` + grep for binaries; direct source
read where open), and capture date.

**Extraction confidence** maps to §6: verbatim literals → high; decoded minified
logic → medium; behavioral inference → low.

**Legal & bloat posture.** Reverse-engineering a licensed binary for *interop
understanding* is the purpose. Capture verbatim evidence in the vault (it is
research, confidence:high), but **do not paste proprietary prompt prose into
shipped harness code** — adopt the *idiom*, the minimal steer, not the
transcription. Verbatim proprietary prose is both a legal and a context-bloat
liability.

**Best parts to extract** (ranked by value to bro-harness's "steer without
bloat" goal):

1. **Tooldoc steering language** — the exact "when to use / when NOT" wording,
   negative guidance ("avoid `cat`/`head`/`tail` → use Read"), the context hints
   embedded in tool descriptions. The most reusable artifact; it is idiom.
2. **System-reminder text & cadence** — what is injected, on which turn, on what
   trigger (todo-nag, deferred-tool disclosure, "N tools available"). The
   context-assembly levers.
3. **Deferred-tiering / tool-search mechanics** — how tools surface by name only
   until fetched. The anti-bloat keystone.
4. **Feature-flag betas + their headers**, **compaction prompts/trigger math**,
   **agent-loop control** (`end_turn`/`pause_turn`, parallel calls, interrupt
   repair).

**What not to extract:** harness-internal implementation detail that does not
generalize; anything whose only effect on our design is to bloat it.

## 9. Researcher contract (how to pick up a leaf)

When you take a `stub` (or refresh an `enriched`) finding:

1. **Read the axis doc** (`<axis>.md`) for the dimension definition and the
   questions the cell must answer. Read sibling cells for other subjects to keep
   vocabulary aligned and to feed the axis's convergence/divergence synthesis.
2. **Read the subject snapshot** for provenance — use the recorded source; do
   not re-derive the binary path.
3. **Mine** per §8. Tag every non-trivial claim with confidence. Keep verbatim
   proprietary prose minimal and clearly marked as evidence.
4. **Set frontmatter**: bump `status`, set `confidence` (the doc blend),
   `last_verified` = the snapshot version. Leave an **Open** section for the
   frontier.
5. **Update the snapshot checklist** row for this axis.
6. **Feed the axis doc**: if your cell reveals a convergence or a divergence,
   add it to the axis's synthesis (cite your cell).
7. **Cross-link**: relative links to the axis, the snapshot, sibling cells, and
   any `design/` doc the idiom should feed.

A finding is **done at `verified`** when its claims are cross-checked and the
axis synthesis reflects them — not merely when text exists.

## Cluster conventions

- One subject snapshot per harness *version*; provenance never duplicated into
  leaves.
- Findings are version-agnostic understanding; snapshots are the temporal anchor.
- Adopt idiom, not transcription. The vault stores evidence; `design/` stores
  what we build from it.
- Provider-agnostic ambient text uses **bare** tool names (`bbox_note`, not
  `mcp__blackbox__bbox_note`); FQDN surfacing is a per-CLI concern (mirrors the
  bro-harness cluster convention).
