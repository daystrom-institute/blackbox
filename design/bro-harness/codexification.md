---
title: "Codexification: aligning bro-harness context construction + agent loop with codex"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - bro-harness
  - brodex
  - agent-loop
  - context-construction
brief: "A convergence charter: make bro-harness's model-facing machinery — base instructions, context construction, the turn loop, and compaction — a near-verbatim port of openai/codex's, treating Anthropic-shaped APIs as explicit cutpoints rather than the default. Codex puts almost nothing in the system/instructions slot except a model-family base prompt; AGENTS.md, environment, skills, permissions, etc. are typed fragments injected into the conversation as user/developer messages on turn 1 and re-emitted as diffs when state changes. bro-harness today does the opposite (AGENTS.md IS the system prompt; no environment block; no base prompt). This doc maps codex's architecture, names the Anthropic cutpoints, and stages the port."
---

> **Method / scope.** Source-mined from the local `openai/codex` clone at
> `/Users/invidious/repos/codex` (`codex-rs/…`, HEAD `d45cd262` on `main`,
> committed 2026-06-02). bro-harness paths are relative to `crates/bro-harness/`.
> Codex is Apache-2.0 and already attributed in this repo. Its harness is
> open-sourced under that license **in full** — mechanics *and* prompt prose
> (the base instructions and the model-family `*_prompt.md` files). All of it is
> fair game to port wholesale with attribution; the prose is not "proprietary"
> just because it's text the model reads. (The limiting case is forking codex
> outright and grafting the Anthropic transport on — same bits, same license;
> this charter is that, done in-tree.) This doc is the umbrella charter; it
> **subsumes and extends** the narrower
> [`brodex-agent-loop-learnings.md`](./brodex-agent-loop-learnings.md) (env
> context / `end_turn` / parallel tools) and is the context-construction
> companion to [`anthropic-harness.md`](./anthropic-harness.md) (transports),
> [`brodex-responses-deep-dive.md`](./brodex-responses-deep-dive.md), and
> [`compaction-canonical-anthropic.md`](./compaction-canonical-anthropic.md).
> Verify every codex line cited against the clone before implementing — the
> upstream moves fast.
>
> **Review pass.** Source-checked over multiple rounds by a `codex` `gpt-5.5`
> (effort high) bro grounded in this same clone (session
> `019e9aae-a23b-7743-98b7-815777ac72aa`, 2026-06-05). Its corrections are folded
> in throughout; §1, §3.1, §3.2, §3.5, §3.6, §5, and §6 were materially revised as
> a result.

# Codexification

## 0. Thesis

**Adopt codex's model-facing machinery as verbatim as the two API shapes allow.**
Codex's harness is the reference implementation; bro-harness should track it for
everything the *model* sees and does — base instructions, how context is laid
out, the turn loop, tool dispatch, compaction. The only sanctioned divergences
are **cutpoints**: the seams where the Anthropic Messages API genuinely cannot
represent a Responses-API construct (no `instructions` field, no `developer`
role, no replayed encrypted reasoning). Everywhere else, when in doubt, do what
codex does.

This is a *starting-point* alignment, not a permanent fork: as code-mode and
other surfaces land, the harness keeps codex as its upstream reference for the
model-facing layer.

## 1. The core architectural delta

Codex and bro-harness place context in **different regions of the request**.
That single difference generates almost every gap below.

### Codex (the target)

- The request `instructions` field carries **only** a model-family **base
  prompt** — `prompt.base_instructions.text`
  (`codex-rs/core/src/client.rs:746`, rendered into `ResponsesApiRequest`
  at `:775`). The default is `prompts/base_instructions/default.md`
  (`codex-rs/protocol/src/models.rs:907`, `BaseInstructions` struct `:912`).
- **Everything else** — AGENTS.md, environment, skills, plugins, permissions,
  personality, collaboration mode — is a typed **context fragment** injected
  into the conversation `input[]` as `user`/`developer` messages, established on
  turn 1 (`build_initial_context`, `session/mod.rs:2728`). A *subset* of these
  (environment + the settings the diff engine covers) is re-emitted as **diffs**
  when state changes (`record_context_updates_and_set_reference_context_item`,
  `mod.rs:2987`; `context_manager/updates.rs:209-243`) — but the diff engine is
  **not yet complete even in codex** (TODO at `updates.rs:217`, `mod.rs:1618`), so
  "everything re-diffs" overstates it. See §3.5.

### bro-harness (today)

- The AGENTS.md overlay **is** the base system prompt — there is no model base
  prompt at all (`agent_loop.rs:564-568`; `project_doc::discover`,
  `project_doc.rs:243-296`). The system text is the global `$CODEX_HOME/AGENTS.md`
  + repo `AGENTS.md` chain.
- Environment (cwd / date / sandbox / network) is **never injected**. The model
  cannot tell that it `cd`'d.
- The deferred-tool manifest rides the volatile system tail
  (`compose_system`, `agent_loop.rs:1393-1451`); the stable/volatile split is the
  only structure (`transport/mod.rs:185`, `SystemPrompt`).

### What "codexify" means, concretely

1. Introduce a real **base prompt** in the system/instructions slot.
2. **Demote AGENTS.md** from system prompt to an in-history `user_instructions`
   fragment.
3. Adopt the **fragment + reference-context-diff** machinery for AGENTS.md,
   environment, and (later) skills/permissions/etc.
4. Bring the **turn loop, tool dispatch, and compaction** into structural parity.

## 2. Codex agent loop (reference)

`run_turn()` (`session/turn.rs:133-419`) is a request → stream → dispatch →
append → re-request cycle:

- **Per-turn prompt build** — `build_prompt()` (`turn.rs:900-917`):
  `input = session.clone_history().for_prompt(modalities)`,
  `tools = router.model_visible_specs()`,
  `base_instructions = session.get_base_instructions()`. History cloned fresh
  every turn; instructions/tools recomputed.
- **Sampling** — `run_sampling_request()` (`turn.rs:929`) →
  `try_run_sampling_request()` (`turn.rs:1703`) consumes the SSE stream:
  `OutputItemDone` finalizes items and dispatches tool calls; text / reasoning /
  tool-arg deltas stream to clients; `Completed { token_usage, end_turn }` ends
  the turn.
- **Centralized history mutation** — `record_conversation_items()`
  (`session/mod.rs:2565`) atomically: appends to the in-memory `ContextManager`,
  persists to rollout, emits a `RawResponseItem` event. Tool outputs flow through
  the same path, so they are in history for the next request.
- **Order within a turn matters** — pre-turn compaction runs *before* context
  updates (`turn.rs:147` then `:164`). Any port that emits context diffs must
  slot them after the compaction check, or a compaction can strand a just-emitted
  diff. (Staging implication: Stage 2's diff step depends on Stage-2 baseline
  plumbing existing first.)
- **Auto-compact mid-loop** — after each sampling request,
  `auto_compact_token_status()` (`turn.rs:662`) checks the budget; if reached and
  the model wants a follow-up, `run_auto_compact()` rewrites history and the loop
  continues.
- **First turn ≠ subsequent** — `record_context_updates_and_set_reference_context_item()`
  (`mod.rs:2987`): turn 1 → `build_initial_context()` (full fragment bundle);
  later turns → diffs against the stored `reference_context_item`.

bro-harness's `user_turn()` (`agent_loop.rs:783`) already has the same skeleton —
loop, dispatch, append, proactive+reactive compaction. The structural deltas:

| Concern | Codex | bro-harness today | Action |
|---|---|---|---|
| History store | `ContextManager` (`context_manager/history.rs:34-51`), owns `items`, `reference_context_item`, token info | Transport owns the buffer; harness appends via `push_user_text` / assistant-message emit | Keep transport-owned buffer (cutpoint), but add a harness-side `reference_context_item` baseline. |
| Append path | one `record_conversation_items` (mem + rollout + event) | scattered (`push_user_text`, assistant emit, tool-result append) | Optional: centralize, to make the diff/rollout hooks single-sited. |
| Context diff step | top-of-turn diff vs reference | none | **Add** (Stage 2). |
| `end_turn` honoring | `Completed.end_turn` is an **explicit follow-up signal** (`turn.rs:2003`), not advisory | partial — see brodex-agent-loop-learnings.md | Honor `end_turn=false` as "model wants another turn." |
| Parallel tools | `FuturesOrdered`, `parallel_tool_calls` | serial dispatch + Promise layer | Reconcile per brodex-agent-loop-learnings.md §3 (out of scope here). |

## 3. Codex context construction (the part bro-harness lacks)

### 3.1 The fragment abstraction

`ContextualUserFragment` trait (`context/fragment.rs:32-98`): `role()` (`user` /
`developer`), `markers()` (XML-ish `<environment_context>…</environment_context>`),
`body()`, `render()`. Concrete fragments live in `context/*_instructions.rs`.

Assembly is **role-split**, and this is easy to get wrong:
`build_contextual_user_message(text_sections)` (`context_manager/updates.rs:187`)
builds **only** a `role:"user"` message (multiple `InputText` blocks — parallel
XML in one user turn). It does **not** emit developer fragments. Developer-role
fragments (permissions, skills catalog, apps, collaboration mode) are accumulated
separately in `build_initial_context` and emitted via `build_developer_update_item`
(`session/mod.rs:2916`). So a turn-1 bundle has (at least) *two* kinds of message:
a developer message (capability/policy instructions) **and** a user contextual
message (AGENTS.md + environment). The ordering is **not** strictly
"developer-first, user-last": guardian sessions emit aggregated-developer,
separate-developer, multi-agent-hint-developer, the user contextual message, **and
then** a guardian developer message *after* it (`session/mod.rs:2915-2951`). Port
the real emission order from source. The role split is load-bearing regardless —
it's exactly what §6 has to preserve under Anthropic. There are currently **28**
`impl ContextualUserFragment for` sites in `context/` (the trait itself is defined
at `context/fragment.rs:41`).

### 3.2 The fragment catalog and when each fires

| Fragment | File | Role | When |
|---|---|---|---|
| UserInstructions (AGENTS.md) | `user_instructions.rs` | user | turn 1 + whenever instructions present |
| EnvironmentContext | `environment_context.rs` | user | turn 1; re-emitted on **change** (cwd/network/fs) |
| AvailableSkills / Skill | `available_skills_instructions.rs`, `skill_instructions.rs` | dev / user | turn 1 if skills fit budget |
| Permissions | `permissions_instructions.rs` | dev | turn 1 + on permission change |
| Apps / Plugins | `apps_instructions.rs`, `available_plugins_instructions.rs` | dev | when enabled |
| CollaborationMode | `collaboration_mode_instructions.rs` | dev | turn 1 + on switch |
| ModelSwitch | `model_switch_instructions.rs` | dev | on model switch (not turn 1) |
| PersonalitySpec | `personality_spec_instructions.rs` | dev | on personality change |
| Hook/AdditionalContext | `hook_additional_context.rs`, `fragments.rs` | dev/user | on hook contribution |

Turn-1 ordering (`build_initial_context`, `mod.rs:2728`) — the catalog above is a
**subset**; the real bundle also threads explicit developer instructions, realtime
fragments, extension-contributed fragments, multi-agent usage hints, and a
possible separate guardian developer message (`mod.rs:2751`, `:2866`, `:2916`,
`:2928`, `:2941`). Roughly: developer-role policy/capability block(s), then
the **user** contextual message ending in **user_instructions (AGENTS.md) →
environment_context** — except guardian sessions append a further developer
message *after* the user contextual message (§3.1). Port the real ordering from
source, not this summary.

### 3.3 AGENTS.md discovery

`agents_md.rs`: project-root marker walk, root→cwd chain joined `\n\n`,
`AGENTS.override.md` preferred, 32 KiB cap, plus global `~/.codex/AGENTS.md`.
**Codex does not support `@`-imports.** bro-harness's `project_doc.rs:194-227`
**does** recursive `@`-include expansion — a deliberate superset to **retain** and
document as an intentional divergence, not regress.

### 3.4 environment_context

Fields (`context/environment_context.rs`): environments (cwd + shell),
current_date, timezone, network (allow/deny), filesystem (workspace roots,
permission profile), subagents → rendered as `<environment_context>…`.
Re-emitted via `diff_from_turn_context_item()` (`:378-418`) **only when changed**
— with two quirks to port faithfully: the comparison **ignores shell changes**
(`equals_except_shell`, `:366`) and **subagents are not diffed** (`:416`). This is
the mechanism that tells the model it `cd`'d — bro-harness has no equivalent and
this is the single highest-value behavioral gain.

### 3.5 The diff engine

`build_settings_update_items()` (`updates.rs:209-243`) emits small developer+user
diff messages on model-switch / permission / env / personality / collaboration /
realtime change, keyed off the `reference_context_item` baseline stored in
`ContextManager` (`history.rs:34-51`). The *shape* is provider-agnostic ("append a
small message when state changes" maps onto Anthropic `messages[]` directly), but
**do not assume it's complete even in codex**: the code carries a TODO that it is
not yet a pure persisted diff and does not cover every model-visible initial-context
item (`updates.rs:217`, `mod.rs:1618`). Port the working subset (env / model-switch
/ permissions / personality / collaboration / realtime); don't design bro-harness as
if apps/skills/plugins already have full steady-state diff coverage.

### 3.6 Compaction (already largely mirrored)

Codex: inline client-side (`compact.rs` — a local summarization turn; the ≤20k
figure is the budget for *retained recent user messages* in the compacted history,
`compact.rs:49`/`:500`, not a cap on the mechanism), remote v1
(`compact_remote.rs` — `/responses/compact`), remote v2
(`compact_remote_v2.rs` — streamed, ≤64k retained-message budget at `:48` + one
`Compaction` item, **but** retention is filtered through
`should_keep_compacted_history_item` (`compact_remote.rs:293`), which drops
developer messages and most non-real user messages). bro-harness already mirrors
the inline + server `responses/compact` split (`compaction.rs`, `transport`
`compact`). Track v2's retention shape as a follow-on; see `brodex-compaction.md` /
`compaction-canonical-anthropic.md`. **Cutpoint reminder:** the Anthropic path
cannot replay encrypted `reasoning` or codex `Compaction` items, so the
textual-summary compaction path must stay explicitly separate from the Responses
replay path (§3.7, §4.iii).

### 3.7 Mechanics the model depends on (don't skip these)

The review flagged four codex mechanics that the context model silently relies on
and that a naive port would drop:

- **`for_prompt` normalization** (`context_manager/history.rs:115`, `:362`). Codex
  does **not** send raw history. Before each request it repairs/removes orphaned
  call↔output pairs and strips images for text-only models. Skipping this produces
  malformed tool-call histories that 400 or confuse the model. bro-harness already
  has an analog in spirit (it must keep tool_use/tool_result balanced for Anthropic
  too); make it an explicit normalize seam.
- **Encrypted reasoning replay** (`client.rs:750`, `protocol/src/models.rs:761`).
  On Responses, codex requests `reasoning.encrypted_content` and replays
  `ResponseItem::Reasoning` across turns. This is a **Responses-only** capability
  and a hard Anthropic cutpoint (§4.iii) — keep the brodex replay path and the
  Anthropic "thinking is display-only" path explicitly separate.
- **`prompt_cache_key`** keyed by thread (`client.rs:371`), and the WS transport
  only sends incremental input when the non-input request fields match exactly
  (`client.rs:1016`). bro-harness already sets a session-stable cache key; preserve
  that invariant when the base prompt / fragment layout changes (a layout change
  that perturbs the cached prefix silently tanks cache hit-rate).
- **Rollout reconstruction is core, not optional** (`session/rollout_reconstruction.rs:87`).
  Resume rebuilds `reference_context_item`, previous model settings, and
  compaction-cleared baselines from persisted rollout items. The diff engine (§3.5)
  is meaningless on resume without it — so Stage 2's baseline and a rollout/replay
  story are the same work, not separable.

## 4. The Anthropic cutpoints

Codex's model is Responses-API-shaped. Three constructs don't exist on Anthropic
and define the sanctioned abstraction seams. **These are the only places we
intentionally diverge.**

| Codex construct | Anthropic reality | Cutpoint |
|---|---|---|
| `instructions` = base prompt | `system` array of blocks w/ `cache_control` | One `BaseInstructions` concept; each transport renders to its native slot — Responses `instructions`, Anthropic first **stable** `system` block (bro-harness `transport/anthropic.rs:95-105`). |
| `developer`-role items in `input[]` (permissions, skills, apps) | No developer role (only user/assistant) | **Resolved in §6:** render developer fragments as **ordered Anthropic `system` blocks** (preserving their higher-than-user authority), keep user fragments (AGENTS.md, env) as user messages, and hold an internal role-tagged pseudo-history for diff/compaction/rollout. |
| Encrypted `reasoning` / `Compaction` items replayed in `input[]` | Thinking blocks not replayed (already dropped, bro-harness `agent_loop.rs:930-935`) | Keep the existing "thinking is display-only" rule; orthogonal to fragments. |

Everything else — the fragment trait, contextual-user-message assembly, the
reference-context **diff** re-injection, AGENTS.md-as-fragment,
environment_context — is provider-agnostic and maps onto Anthropic `messages[]`
cleanly.

**New internal contract.** Split today's `SystemPrompt { stable, volatile }`
(`transport/mod.rs:185`) into two concepts:

1. `BaseInstructions` — the model/transport base prompt → native
   system/instructions slot.
2. `ContextFragments` — ordered, role-tagged, diffable fragments → front of the
   message buffer (+ developer→system collapse on Anthropic per §6).

The volatile tool-manifest tail stays as-is — but call it what it is: a
**divergence, not "unchanged."** Codex passes tools as *native* tool specs
(`router.model_visible_specs()`, `turn.rs:908`), never as prompt text. bro-harness
also passes native specs (`reg.wire_specs()`); the deferred-tool *manifest* is a
separate harness-ism (the tool-search tiering layer) that lists not-yet-loaded
tools as prose. It has no codex equivalent and doesn't conflict with the fragment
model, but it is an intentional bro-harness addition on top of codex, not part of
the verbatim port.

## 5. Staged convergence plan

**Stage 0 — base prompt.** Port codex's base-instruction **selection**, not a
single hardcoded file. Codex resolves a model's base instructions from its model
catalog — `ModelInfo::base_instructions` / `model_messages`, with template
handling in `protocol/src/openai_models.rs:383` — under a session precedence of
config override → resumed-session metadata → current model's
instructions/personality (`session/mod.rs:551`). Vendor whatever prose that
resolves to — the default base (`protocol/src/prompts/base_instructions/default.md`)
plus the model-family instructions carried in the catalog — wholesale under
Apache-2.0 with attribution. brodex must resolve its `gpt-5.5`/`gpt-5.x-codex`
models through this same catalog-driven base-instruction mechanism (selecting the
codex-family instructions for codex-family models) rather than hardcoding
`default.md`. Introduce `BaseInstructions`
in `transport/mod.rs` beside `SystemPrompt`. Each transport renders it: Anthropic →
stable system block; Responses → `instructions` (replacing the "helpful coding
assistant" stub at `responses_common.rs:219-222`).
*Load-bearing: frees the system prompt from doubling as the AGENTS overlay.*

**Stage 1 — fragment layer + baseline shape.** Port `ContextualUserFragment` and
the role-split assembly (user contextual message **and** developer
update-item, per §3.1) into a new `crates/bro-harness/src/context/` module. Seed
with `UserInstructions` (wrapping `project_doc::discover`) and `EnvironmentContext`.
**Critical (review):** introduce the internal `reference_context_item` /
`TurnContextItem` baseline shape *now*, even if nothing diffs against it yet.
Adding fragments without the baseline makes Stage 2 a rewrite. Also land the
`for_prompt`-style normalize seam here (§3.7).

**Stage 2 — baseline persistence/restore.** *(Reshaped during implementation,
2026-06: the diff engine moved to Stage 4 — see below.)* Persist the
`reference_context_item` baseline through the session side-state and restore it on
resume (the harness analog of codex reconstructing it from rollout), so a resumed
session restores the prior baseline instead of re-emitting environment context.
**The diff-and-re-emit engine is deferred to Stage 4**, because bro-harness's
environment is effectively static today (fixed session root, `$SHELL`, timezone)
and codex's other diff dimensions (permissions/collaboration/realtime/personality)
don't exist here until Stage 4 — there is nothing to diff yet, so building the
engine in Stage 2 would be speculative. Pair it with the developer fragments that
actually mutate. *(As-built: `git log` Stage 2 commit; the diff covered
environment / model-switch / permissions / collaboration / realtime / personality
per `updates.rs:221` in codex, deferred here.)*

**Stage 3 — demote AGENTS.md (can move up).** Flip `compose_system`
(`agent_loop.rs:1393`) so the stable prefix is `BaseInstructions + pinned-tools`
only; AGENTS.md moves entirely into the `UserInstructions` fragment. **Review note:**
this is the central semantic change — once Stage 1's `UserInstructions` fragment
exists, do this immediately rather than carrying AGENTS.md in the system prompt
through two more stages. Keep the `@`-import superset, documented as intentional
divergence.

**Stage 4 — developer fragments + diff engine + rollout parity (NOT optional for
fidelity).** Permissions / skills / apps / plugins are developer-role *first-turn*
context in codex today, not extras; under §6 they become ordered Anthropic system
blocks. **The diff-and-re-emit engine (moved here from Stage 2) belongs in this
stage**: these developer fragments are the first context that actually mutates
mid-session, so the diff mechanism (top-of-turn, after the compaction check; covers
environment + the developer dimensions per codex `updates.rs:221`) has something to
diff once they exist. First-class per-item rollout persistence
(`record_conversation_items` shape) is *required* for resume/compaction
correctness, not a nice-to-have. Treat this as finishing the port, not breadth.

Stages 0–3 reach "verbatim codex context model for AGENTS.md + environment,
Anthropic-mapped." Stage 4 closes the developer-fragment + persistence surface to
reach full fidelity.

## 6. Resolved: developer-role fragments on Anthropic

Anthropic has no `developer` role. The naive options are (A) collapse developer
fragments into extra system blocks, or (B) keep them as user-role contextual
messages. An earlier draft leaned (B) for surface "fidelity." **Review overturned
that — adopt (A), in a stricter shape.**

**Decision.** Render codex `developer` fragments (permissions, skills catalog,
apps, plugins, collaboration mode, model-switch, personality) as **ordered
Anthropic `system` blocks** — base instructions first, then the developer-fragment
blocks in codex's turn-1 order. Keep **user**-role fragments (AGENTS.md
`user_instructions`, `environment_context`) as **user messages**. Maintain an
internal **role-tagged pseudo-history** of all fragments so the diff engine (§3.5),
compaction, and rollout reconstruction operate on a codex-shaped item list
regardless of how each transport renders it.

**Why (A) is the faithful answer, not (B).** Codex deliberately assigns
permissions/skills/apps/plugins/collaboration/model-switch/personality the
`developer` role — e.g. permissions (`context/permissions_instructions.rs:5`),
skills (`available_skills_instructions.rs:23`), plugins
(`available_plugins_instructions.rs:24`). On the Responses API, `developer`
outranks `user` in instruction authority. Mapping those to Anthropic *user*
messages (option B) is mechanically "history-shaped" but **demotes policy and
capability instructions to the same authority level as AGENTS.md and end-user
content** — a real semantic regression. Anthropic's analog to "above the user" is
the `system` array, so developer→system is the *more* faithful mapping, not the
less. It also caches better.

**Caching shape.** Cache the stable system blocks (base + unchanged developer
fragments); emit changed/diff blocks as volatile/uncached. AGENTS.md and
environment ride user messages (already uncached, consistent with codex treating
them as conversation). This keeps the existing `SystemPrompt{stable,volatile}`
breakpoint working and extends it from one stable block to an ordered list.

## 7. Non-goals / guardrails

- Port codex's prompt prose — base instructions and model-family `*_prompt.md` —
  wholesale under Apache-2.0 with attribution; keep the upstream `NOTICE` /
  attribution intact. It is part of the open-sourced harness, not proprietary;
  do not hand-rewrite it to "avoid copying."
- Do **not** regress the `@`-import superset (§3.3).
- Parallel-tool execution reconciliation with the Promise layer is tracked in
  `brodex-agent-loop-learnings.md` §3, not here.
- Compaction v2 retention shape is a follow-on (`brodex-compaction.md`).
- Respect the multi-tenant worktree convention: this is a model-facing-machinery
  effort; coordinate with the concurrent code-mode import.

## 8. Provenance

- codex clone: `/Users/invidious/repos/codex`, `main` @ `d45cd262` (2026-06-02).
- Keystone codex refs: `client.rs:371,746,775,1016`; `protocol/src/models.rs:907,912`;
  `protocol/src/openai_models.rs:383`; `compact.rs:49`;
  `session/turn.rs:133,147,662,900,929,1703,2003`;
  `session/mod.rs:551,2565,2728,2916,2987`; `context/fragment.rs:41`;
  `context_manager/updates.rs:187,209,217`; `context_manager/history.rs:115,362`;
  `session/rollout_reconstruction.rs:87`; `compact_remote.rs:293`;
  `context_manager/history.rs:34`; `context/environment_context.rs:378`;
  `agents_md.rs`.
- bro-harness refs: `agent_loop.rs:564,783,847,1393`; `project_doc.rs:243`;
  `transport/mod.rs:185`; `transport/anthropic.rs:95`;
  `transport/responses_common.rs:219`.
