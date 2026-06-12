---
title: "Dispatch prompt slots: harness-owned composition of the dispatch context"
kind: design
lifecycle: archived
corpus: blackbox-design
topic:
  - bro-harness
  - orchestration
  - context-construction
  - dispatch
brief: "Stop gluing the daemon's ambient directives, brofile lens, scope block, and pins onto the operator's prompt as one -p string — and stop composing prompt text daemon-side at all. The daemon hands the harness a typed dispatch context (persona, directives with declared cadence, scope IDs, pin block) via one structured flag; the harness owns composition through a per-transport strategy. On anthropic/responses: persona+standing directives → stable system slot, scope/pins → marker-demarcated contextual-user fragments with deltas. On openai-chat (Mistral): the vibe-faithful shape — context folds into the mutable leading system message; the user lane carries the task. Grounded in source reads of codex, mistral-vibe, and opencode; reviewed adversarially (brodex/gpt-5.5/high, session 6f2e4ddc). Fixes gap-00efeb12."
---

> **Provenance.** Direct fallout of gap-00efeb12 (session `7476e678` forensics:
> vibebh/mistral on openai-chat answered "the user hasn't actually provided a
> task yet" to a one-line instruction buried under a 2,845-char ambient preamble
> and a 15,434-char rendered-memory user turn). Operator review redirected v1
> twice: (1) composition is a **harness** concern — the daemon supplies typed
> ingredients only; (2) the design must be grounded in the reference sources.
> §1 is mined directly from the local clones: `~/repos/codex` (codex-rs),
> `~/repos/mistral-vibe`, `~/repos/opencode`, cross-checked against
> `research/harness/{codex,vibe,claude}/…-context-management.md` and
> [`codexification.md`](./codexification.md) §3/§6. Adversarial review round 1
> (brodex gpt-5.5 effort-high, session `6f2e4ddc-6025-44c0-b8e0-8db7780a2e00`,
> verdict needs-changes, 9 findings) is folded in throughout — notably the
> openai-chat default (§5), restore semantics (§4), compaction mechanism (§7),
> suppression semantics (§8), and the migration audit (§6). Verify cited lines
> against the clones before implementing — upstreams move fast.

# Dispatch prompt slots

## 0. Problem

Every dispatch path (bro_exec, bro_resume, broadcast, agent dispatch, atoms,
badgey, workflows, fleet cockpit via `/control/*`) funnels through two string
concatenators in `src/orchestration/mod.rs`:

```
apply_brofile_lens(apply_ambient(operator_prompt, &ambient_ctx), lens)
```

`apply_ambient` prepends `[scope]`, `[scoped pins]`, `[recall before acting]`,
`[task shape]`, `[orchestrator]`, `[completion contract]`,
`[milestone reporting]`, and the workspace-tools appendix; `apply_brofile_lens`
prepends the persona. The result rides `-p` as the **first user turn**, with the
operator's instruction as the bare last line. On resume the ambient preamble is
re-glued onto every follow-up (without persona — resume branches drop the lens).

Failure modes:

1. **Task burial.** Smaller models (observed: mistral-medium on openai-chat)
   read the directive wall as the message and the trailing one-liner as noise —
   "no task provided." The failure trace shows BOTH parts: the directive-buried
   task user turn AND a 15,434-char rendered-memory user turn directly before
   the "no task" reply. The same model follows the same instruction fine under
   its native harness (vibe), whose prompt shape is §1.2.
2. **Authority inversion.** Daemon standing policy rides the *user* lane at the
   same authority as the task itself. No reference harness does this (§2).
3. **Whack-a-mole calibration.** All providers share one glued string, so every
   per-provider fix (TASK_SHAPE_HINT's E12 calibration bound, contract wording,
   milestone placement) is wording surgery on a shared preamble. The reference
   harnesses each have an explicit per-model/per-transport composition seam
   (§1.3, §1.4); we have none.
4. **Token waste.** The full preamble re-rides every resume turn in the user
   lane, when the system slot already re-sends stable text every request.
5. **Composition in the wrong layer.** The daemon composes prompt text while
   knowing nothing about transports; the harness, which owns the slot machinery
   and per-transport rendering, receives an opaque blob it can only forward.

## 1. Reference shapes (source-mined)

### 1.1 Codex — typed slots, split developer/contextual-user

`Session::build_initial_context` (codex-rs `core/src/session/mod.rs:2750-2952`)
assembles three section lists and routes them:

- **Developer message(s):** permissions, developer instructions, collaboration
  mode, realtime, personality, apps, skills, plugins — plus extension fragments
  tagged `PromptSlot::DeveloperPolicy | DeveloperCapabilities` (one combined
  developer item) or `PromptSlot::SeparateDeveloper` (own items)
  (`mod.rs:2875-2886`). Base instructions ride the Responses `instructions`
  request field, not a message.
- **One contextual user message:** extension `PromptSlot::ContextualUser`
  fragments → `UserInstructions` (AGENTS.md, `mod.rs:2888-2897`) →
  `EnvironmentContext` **last** (`mod.rs:2898-2910`).
- **The operator prompt is its own item**, after the context items.

Fragment identity: every fragment type carries start/end markers and a
`matches_text` registration so injected context is re-identifiable in the
transcript lifecycle (`core/src/context/fragment.rs:6-114`). Subsequent turns
emit delta updates against a stored `reference_context_item` **for the covered
dimensions** (environment/settings and the developer dimensions) — codex's own
update engine documents that it "does not cover every model-visible item"
(`context_manager/updates.rs:217`; the same caveat is recorded in
`codexification.md`). Delta discipline is real but partial, not universal.

The `PromptSlot` enum is the extension seam: contributors *declare* a slot;
one routing point places them. Placement knowledge lives with the session, not
with the thing contributing the fragment.

### 1.2 Vibe — everything in one mutable system message (the Mistral-native shape)

`get_universal_system_prompt` (`vibe/core/system_prompt.py:308-394`)
concatenates ~10 sections into ONE system string: base prompt (selected by
`system_prompt_id`), headless note, commit signature, model info, OS, per-tool
prompt docs, skills, subagents, scratchpad, project context (cwd + git
branch/status/commits), and **all AGENTS.md content** (user-level + project,
labeled per path, `system_prompt.py:374-392`). The message buffer starts
`[system_message]` (`vibe/core/agent_loop.py:319-329`); the system message is
**rebuilt and replaced in place** mid-session when tool/skill state changes
(`refresh_system_prompt` / `update_system_prompt`, `agent_loop.py:612-623`).
Extra context enters the user lane only as explicit user-role injected
messages (`inject_user_context`, `agent_loop.py:640-651`) or lazily attached
per-directory AGENTS.md on `read_file` results.

**This is the existence proof for the gap:** mistral-medium follows one-line
instructions fine under a heavyweight position-0 system message and a clean
task-only user message. The model's tolerance problem is not "long context" —
it is policy and memory text in the user lane competing with the task.

### 1.3 opencode — per-model base prompts, one delivery-divergence point

- **Base prompt selected per model family**, not per provider:
  `SystemPrompt.provider(model)` maps gpt-4/o1/o3→`beast.txt`,
  gpt→`gpt.txt`, codex→`codex.txt`, gemini→`gemini.txt`, claude→
  `anthropic.txt`, kimi/trinity/default (`src/session/system.ts:25-39`).
- **Agent persona REPLACES the base prompt** rather than stacking
  (`agent.prompt ? [agent.prompt] : SystemPrompt.provider(model)`,
  `src/session/llm/request.ts:58-66`).
- System array = persona-or-base + env block + instructions (AGENTS.md /
  CLAUDE.md / config instructions, each labeled `Instructions from: <path>`,
  `src/session/instruction.ts:155-169`) + skills — recomputed **every step**
  (`src/session/prompt.ts:1327-1335`), then normalized to `[header, rest]`
  (two cache-shaped blocks, `request.ts:68-78`).
- **Delivery divergence handled centrally:** OpenAI-OAuth → Responses
  `instructions` field with no system messages, default → leading system
  messages (`request.ts:99-112`); the GitLab-workflow `systemPrompt` field
  branch lives in the caller (`src/session/llm.ts:119-127`). Per-model wire
  mangling + cache-control pinning (first 2 system + last 2 non-system
  messages) in `ProviderTransform.message`
  (`src/provider/transform.ts:323-372,430-459`).
- Mid-task queued user messages are wrapped in `<system-reminder>` markers in
  the user lane (`src/session/prompt.ts:1313-1320`).
- Lazy idiom: reading a deep file attaches nearby AGENTS.md once per message
  via tool results (`instruction.ts:179-221`).

### 1.4 Claude Code — lane placement as an explicit strategy lever

From `research/harness/claude/claude-context-management.md` (2.1.160, binary +
live observation): CLAUDE.md overlays and stable instructions ride the system
prompt; volatile per-machine sections (cwd, env, git status) ride the system
prompt by default but `--exclude-dynamic-system-prompt-sections` **moves them
into the first user message** for cross-user cache reuse — lane placement is a
configurable strategy, not a fixed truth. Steering is trigger-gated
`<system-reminder>` user-lane nudges (todo reminders, deferred manifests,
plan-mode), never always-on policy re-injection.

## 2. Convergent invariants (what the design must honor)

1. **The task user message is sacred.** No reference harness prepends standing
   policy to the operator's prompt. Codex gives context its own items; vibe
   and opencode put it in system; claude wraps even its own steering in
   demarcated `<system-reminder>` blocks.
2. **Standing policy rides at system/developer authority.** Persona and
   harness/host policy are "above the user" in all four.
3. **Situational context is demarcated and re-emit-disciplined.** Codex:
   marker-wrapped fragments, partial delta engine. Vibe: in-place system
   rebuild. Claude/opencode: `<system-reminder>` wrappers, trigger-gated.
   Nothing re-sends the world per turn in the user lane.
4. **Composition is keyed by model family / transport, in one routing point.**
   opencode `provider(model)` + the delivery branch; codex model catalog base
   instructions + slot routing; claude's strategy flag. The seam is always
   *inside* the harness, next to the wire knowledge.
5. **Cache discipline shapes placement.** Stable prefix vs volatile tail is
   explicit everywhere (opencode cache-control pinning; claude's flag exists
   *because* of cache reuse; codex instructions field + delta engine).

## 3. Ownership split

**The daemon owns content selection; the harness owns composition.**

The daemon knows *what applies to this dispatch*: which directives fire
(allow_recursion → orchestrator hint, coerce_workspace → workspace appendix,
the completion contract), each directive's empirically-calibrated cadence
(§4 — the daemon owns the calibration history in its doc comments), the
persona resolved from the brofile, the pre-bound scope IDs, the pin block
resolved from bbox_pin. That is data, not shape.

The harness knows *where things go*: it owns `SystemPrompt{stable, ambient,
volatile}` and each transport's native rendering, the `ContextualUserFragment`
layer with markers, the turn-1 contextual message, the baseline/re-emit
machinery, and session persistence. Placement, ordering, demarcation, re-emit
cadence, and per-transport/model-family variation are harness `context/`
concerns — the codex/opencode shape.

The boundary stays argv-shaped (harness-daemon-boundary.md §2 unchanged): the
daemon passes content down; the harness never reaches up. The directive prose
stays daemon-owned (it references bbox_*/bro_* vocabulary the harness must not
know); the harness never interprets directive text, only places it.

## 4. Boundary surface: `--dispatch-context <json>`

One structured flag (parsed in-process via `Cli::try_parse_from`; env fallback
`BRO_HARNESS_DISPATCH_CONTEXT` for the standalone binary). Typed ingredients,
not composed prose. Versioned, strictly parsed (unknown fields and unknown
`v` are errors — the payload is daemon-authored; garbage is a bug, not input
to tolerate):

```json
{
  "v": 1,
  "persona": "You are a reviewer…",
  "directives": [
    {"id": "recall",     "cadence": "per_turn", "text": "Recall: early in tasks…"},
    {"id": "task_shape", "cadence": "standing", "text": "Task-shape check…"},
    {"id": "contract",   "cadence": "standing", "needs_scope": true, "text": "If something genuinely notable…"},
    {"id": "milestone",  "cadence": "per_turn", "needs_scope": true, "text": "Report at major milestones…"}
  ],
  "scope": {"task": "…", "session": "…", "project": "…", "bro": "…",
            "thread": "…", "work_item": "…"},
  "pins": "…"
}
```

- `persona` — brofile lens, verbatim. Optional.
- `directives` — the ordered set the daemon selected for THIS dispatch. `id`
  is a stable label for diffing/debugging. `cadence` is the daemon's declared
  reinforcement need — `standing` (deliver once per request at system
  authority; the default) or `per_turn` (**uncached per-request
  reinforcement**; placement is per-transport, §5 — late where the transport
  allows, folded into the leading system block on openai-chat after-tool
  turns, where Mistral forbids trailing system messages). The cadence values
  exist because the current constants carry *empirical* calibration:
  RECALL_DIRECTIVE's doc comment records that session-start guidance
  attention-decays within-session and per-turn injection survives;
  MILESTONE_REPORT_HINT records that late placement is deliberate and only
  per-turn delivery got weak models reporting (src/orchestration/mod.rs:1549,
  1654). Moving those two to a once-per-request stable slot would discard
  that evidence; declaring cadence lets the harness honor it in the right
  native lane (§5) without learning what the text means. `needs_scope`
  (default false) declares that the text references the scope block's
  correlation keys; the harness drops `needs_scope` directives whenever no
  current scope exists (review round 2 finding 2: a restored contract telling
  the model to copy `task:` from a block that doesn't exist is worse than no
  contract).
- `scope` — typed key→value fields, NOT pre-rendered lines; the harness
  renders and re-renders them.
- `pins` — resolved pin block text, verbatim. Optional.

`-p` becomes the operator's prompt **verbatim** — no `[task]` wrapper (§2.1:
the clean prompt IS the convention; a wrapper is one more referent to drift).

**Persist/restore semantics** (deliberately NOT a copy of `code_mode`'s
session-intrinsic restore — review finding 5):

- Flag present, non-empty ⇒ the payload **replaces** the persisted
  persona/directives/pins wholesale and sets the current scope. Partial
  payloads are not merged; the daemon composes the full context each dispatch.
- Flag present, empty string / `{}` ⇒ explicit clear (persisted context
  removed; nothing renders).
- Flag absent ⇒ persona/pins and the **non-`needs_scope`** directives restore
  from session side-state (the bare-`--resume` standalone case). **`scope` is
  NEVER restored**: task_id is per-dispatch correlation data (a stale
  `scope.task` would mis-route bbox_note/bro_report keys, which the contract
  directive tells the model to copy verbatim). With no current scope,
  `needs_scope` directives are dropped for the same reason — they instruct
  the model to copy keys from a block that wouldn't render. The persisted
  last-emitted baseline survives for future delta comparison only.

Daemon-side dispatch paths MUST pass the flag on every exec AND resume
(the lens is already resolved in `resolve_resume_target`; today's resume
branches discard it as `_lens` — under this design that is a migration bug,
not a choice; see §6).

## 5. Harness composition: classes and per-transport strategy

Semantic classes: **persona**, **standing directives**, **per-turn
directives**, **memory** (AGENTS.md / rendered repo memory, discovery-owned),
**scope**, **pins**, **task**.

The strategy seam is one harness `context/` routing point keyed by transport
(the analog of opencode's `provider(model)` + delivery branch and codex's
PromptSlot router). v1 ships TWO strategies, because the evidence demands two
(review finding 1 — a single codex-shaped default does not fix the observed
Mistral failure):

**Codex-shaped (anthropic, openai-responses):**

| Class | Slot |
|---|---|
| persona | system **stable**, after base/override, before directives |
| standing directives | system **stable**, after persona |
| per-turn directives | **volatile** tail — uncached, re-sent every request. Native placement: Anthropic trailing uncached system block; Responses trailing developer item appended after the input buffer (responses_common.rs:259). Late relative to the task on these transports; the §4 definition is reinforcement cadence, not a positional guarantee |
| memory | contextual user fragment (shipped; unchanged) |
| scope | contextual user fragment, `<bbox_scope>` markers; re-emit on change |
| pins | contextual user fragment, `<bbox_pins>` markers; re-emit on change |
| task | own user item, verbatim, last |

Turn-1 contextual user message ordering: `UserInstructions` (AGENTS.md) →
scope → pins → environment context (environment last, codex order).

**Vibe-shaped (openai-chat — the Mistral lane):**

| Class | Slot |
|---|---|
| persona, standing directives | **leading system message** (stable slot), after base |
| memory (AGENTS.md) | **leading system message** — vibe-faithful: the 15,434-char memory user turn was part of the failure; on this transport memory does NOT ride the user lane |
| scope, pins | **leading system message**, rendered as demarcated sections, rebuilt in place when the dispatch context changes (vibe `update_system_prompt`, §1.2) |
| per-turn directives | volatile tail — trailing system message on normal turns; on after-tool turns the existing Mistral fold relocates it into the leading block (transport/openai_chat.rs:136-184), so "late" does NOT hold on exactly the turns that follow tool output. Accepted: the fold is the transport's only legal shape; §9(b) validates reporting compliance on after-tool turns specifically |
| environment context | leading system message (vibe puts project context in system) |
| task | the only initial user message |

The leading system message is position 0 and rebuilt per request, so the
Mistral system-after-tool constraint never binds; mid-session context changes
mutate the leading block in place rather than appending anything.

**The emitter must be strategy-aware (review round 2 blocker).** Today
`prepare_context_for_user_turn` → `emit_initial_context_if_needed`
unconditionally pushes UserInstructions + EnvironmentContext into the user
lane via `push_user_text_blocks` (agent_loop.rs:1528-1556). The strategy is
not just a routing table for new fragments — it owns that emitter: each class
resolves through the strategy to a slot, and on the vibe-shaped strategy
memory/environment/scope/pins resolve to SystemStable, so the initial-context
emitter contributes NOTHING to the user lane and `compose_system` (widened to
take the strategy-routed sections) renders them into the leading block.
Without this, the chat lane keeps the 15k memory user turn and the blocker
failure mode survives the redesign.

**Leading-block ordering on chat (cache vs salience):** openai-chat has one
leading system string and only a session-level `prompt_cache_key`
(openai_chat.rs:206,263) — no block-level cache separation like Anthropic.
The vibe-shaped strategy deliberately trades cache granularity for
instruction salience. To keep what prefix caching can still give: order the
leading block stable-first — base → persona → standing directives → memory →
pinned-tools → environment → scope → pins — so the per-resume-mutable
sections (scope/pins) sit at the suffix and everything before them stays
byte-identical across rebuilds.

Stable-system ordering (both strategies): base instructions (model-family) →
explicit `--system-prompt` override (when supplied) → persona → standing
directives → pinned-tools section — this requires widening `compose_system`
(today it takes only the explicit-system text and appends pinned tools;
agent_loop.rs:1962). Persona-before-directives mirrors opencode's
persona-leads shape; both after base so the model-family prompt stays the
cache-stable prefix across brofiles. opencode's persona-*replaces*-base is
our existing `provider_defaults: suppress` brofile mode, which composes with
this design unchanged (§8 for what suppress actually does).

**Cache note (review finding on stable semantics):** `SystemPrompt.stable` is
documented as session-constant and carries the cache breakpoint
(transport/mod.rs:207-230). This design keeps it constant *within* a session
in the common case, but a resume that re-passes changed directives (e.g.
contract flips with allow_recursion) legitimately rewrites it — one cache
re-prime per resume boundary, identical in cost to what AGENTS-in-system did
before codexification stage 3, and strictly cheaper than today's per-turn
user-lane re-glue. On Responses the stable text feeds the `instructions`
field per request; a resume-boundary change alters the request body, not the
session cache key. Document, accept, move on.

**Named strategy variants the seam exists for** (not in v1):

- *volatile-mirror*: additionally re-state a terse contract reminder in the
  volatile tail for model families shown to ignore stable-slot policy.
- *chat-user-fragments*: the codex-shaped routing on openai-chat, if a future
  chat-transport model family prefers user-lane context (the inverse of
  today's evidence).

A per-provider fix becomes a strategy-arm change, not preamble surgery.

## 6. Daemon side after the cut

`AmbientContext` stops producing prompt text. It serializes the typed payload
(persona threaded from the brofile, selected directive set with cadence,
scope fields, pin block); `ProviderExec::build_exec_args`/`build_resume_args`
take the operator prompt and the payload separately and emit
`-p <task> --dispatch-context <json>`.

`apply_ambient` and `apply_brofile_lens` are **deleted** — the old shape
becomes unrepresentable, so no path can keep gluing by inertia.

**Migration audit rule (review finding 7):** the migration is complete when
every caller of `build_exec_args` / `build_resume_args` / `spawn_task*`
passes a dispatch context and a verbatim prompt — not when a named list is
done. Known sites at time of writing: dispatch exec (src/tools/dispatch.rs
~350-420) and resume (~628-760), **bro_broadcast fresh AND resume branches**
(dispatch.rs ~1342, ~1400, ~1444 — the resume branch today sends the raw
prompt with no ambient at all), roster.rs:946,1007, atoms.rs:712,
badgey/proposals.rs:670, badgey/lifecycle.rs:174,612,
workflow_runtime.rs:212-216. Fleet `/control/*` routes through
bro_exec/bro_resume (server/routes.rs:873) and is covered by those. Each
site follows one rule: **fresh and resume branches both compose the full
payload, including persona** — today's resume branches drop the lens
(`_lens`), which this design classifies as a bug being fixed, not behavior
being preserved.

Wording follow-through: DEFAULT_COMPLETION_CONTRACT's "copy `task:` from
[scope] above" becomes placement-neutral ("from the `bbox_scope` context
block"), valid for both the user-fragment and system-section renderings. The
workload-retro prompt keeps its self-contained inline scope (it deliberately
bypasses dispatch composition; mod.rs:1733-1847). No other directive wording
changes (§8).

Workflow provider ignores the payload (as it ignores args today).

## 7. Resume, compaction, and re-emit mechanics

**Resume.** Each `bro_resume` is its own dispatch with a fresh task_id. The
daemon re-passes the full dispatch context every resume:

- persona/standing directives: replace the persisted values; land in the next
  request's stable render in place (the vibe `update_system_prompt` move on
  chat; a stable-block rewrite on anthropic/responses). No transcript
  pollution.
- scope: codex-shaped strategies — compare against the **last-emitted**
  baseline in side-state and emit one short `<bbox_scope>` fragment in the
  contextual user lane when changed; vibe-shaped — the leading system rebuild
  carries it, nothing enters the user lane.
- pins: same mechanics; usually unchanged → nothing emitted.

**Compaction (review finding 6 — the mechanism, not hand-waving).** The loop
resets `reference_context_item = None` after compaction (agent_loop.rs:985,
1087, 1159); the next user turn re-runs `emit_initial_context_if_needed`
(agent_loop.rs:1536). Scope and pins join that helper **through the strategy
routing (§5)**: on codex-shaped strategies the post-compaction re-emit
renders the **current in-memory dispatch context** (current scope, current
pins) alongside AGENTS.md and environment in the contextual user message and
updates the emitted baselines; on the vibe-shaped strategy the helper emits
nothing user-lane (those classes live in the leading system block, which
compaction never touches). The harness fragment layer has markers but NO `matches_text`
registry today (context/mod.rs:21-35, vs codex fragment.rs:6-30) — this
design does NOT claim compaction recognizes old fragments in the summarized
transcript; correctness comes solely from the deterministic re-emit path
above. A codex-style fragment registry is a named follow-on, not load-bearing
here. On the vibe-shaped strategy compaction needs nothing: the leading
system message is not part of the compacted buffer.

**Persistence.** The harness persists the dispatch context (minus any notion
of restorable scope, §4) and the last-emitted scope/pins baselines in the
session side-state (`side["dispatch_context"]`, `side["dispatch_emitted"]`),
following the existing side-cell pattern (todos/nudges/lsp_baselines/
reference_context, agent_loop.rs:1721-1736).

Net effect: a resume turn's user lane is the operator's follow-up plus at
most a few scope-delta lines (codex-shaped) or nothing extra at all
(vibe-shaped), versus today's full re-glued preamble.

## 8. Non-goals / guardrails

- No wording changes to RECALL_DIRECTIVE / TASK_SHAPE_HINT / etc. — placement
  only (the contract's scope reference is the one exception, §6). The E12
  calibration bound concerns wording force; cadence declarations (§4) carry
  the existing empirical placement constraints into the new model. The
  §9 live probes are the recalibration gate.
- **Provider-defaults suppression semantics, stated precisely (review finding
  4):** `--system-prompt ""` clears `explicit_system` AND disables AGENTS.md
  discovery (agent_loop.rs:666-670) but does NOT remove model-family base
  instructions — those are set unconditionally (agent_loop.rs:865) and
  rendered by every transport (openai_chat.rs:263, responses_common.rs:365).
  This design does not change that; a suppressed-defaults dispatch with a
  dispatch context gets base + persona + directives and no AGENTS overlay.
  If true base suppression is ever wanted, that is a separate flag, not an
  overload of this one.
- No mid-session **system** injections on openai-chat beyond the existing
  leading/volatile handling; the leading block mutates in place, nothing
  system-roled ever follows a tool message.
- Deferred-tool manifest, tail-nudge, structured-output channels
  (SystemPrompt.ambient/.volatile) untouched; per-turn directives share the
  volatile lane with them, after them.
- The opencode/vibe lazy per-directory AGENTS.md idiom and the
  `<system-reminder>` steer-wrapping idiom are noted, NOT in scope here.
- Workload-retro and other deliberately-bypassing prompts keep bypassing.

## 9. Validation

- Unit: per-strategy slot routing (anthropic system-block order; openai-chat
  leading-system composition incl. memory/scope/pins sections + no
  system-after-tool; responses instructions + trailing developer volatile),
  scope/pins render + change re-emit + post-compaction re-emit, dispatch-
  context persistence round-trip with scope-restore exclusion, payload `v`/
  unknown-field rejection, suppressed-defaults × dispatch-context
  interaction, per-call-site payload construction incl. broadcast fresh AND
  resume branches.
- Live probes (gates, not smoke): (a) the gap's own reproduction —
  vibebh/mistral one-line task through the new path, assert the model acts on
  the task; (b) per-turn-cadence check — a GLM or DeepSeek multi-turn run
  confirming bro_report milestones still fire at depth with milestone riding
  the volatile tail (the empirical claim §4 carries), AND a vibebh after-tool
  sequence confirming per-turn directives folded into the leading block still
  bind on the turns that follow tool output (§5 chat fold caveat); (c) brodex
  (responses) smoke.
- Full gate: `cargo nextest run --workspace`.
