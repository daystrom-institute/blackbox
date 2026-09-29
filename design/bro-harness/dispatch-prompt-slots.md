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
brief: "The daemon never composes prompt text for a dispatch. It hands the harness a typed dispatch context (persona, the standing completion-contract directive, scope IDs) through one structured flag, and the operator's prompt rides verbatim. The harness owns composition through a per-transport strategy: on anthropic/responses, persona and the contract land in the stable system slot and scope is a marker-demarcated `<bbox_scope>` contextual-user fragment with change re-emit; on openai-chat (Mistral) everything context-shaped folds into the mutable leading system message and the user lane carries only the task. Grounded in source reads of codex, mistral-vibe, and opencode."
---

# Dispatch prompt slots

## 0. Invariant

The operator's task is the whole of its user message. Standing policy rides
system authority, situational context is demarcated and re-emitted only on
change, and placement is decided in one harness routing point keyed by
transport. The daemon supplies typed ingredients; it never glues prose onto
the prompt.

Every dispatch path (`bro_exec`, `bro_resume`, team dispatch, workflows, atoms,
the fleet cockpit via `/control/*`) builds the same payload through
`AmbientContext::dispatch_context` and passes it with the verbatim prompt:

```
-p <operator prompt> --dispatch-context <json>
```

## 1. Reference shapes (source-mined)

### 1.1 Codex: typed slots, split developer/contextual-user

`Session::build_initial_context` (codex-rs `core/src/session/mod.rs:2750-2952`)
assembles three section lists and routes them:

- **Developer message(s):** permissions, developer instructions, collaboration
  mode, realtime, personality, apps, skills, plugins, plus extension fragments
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
dimensions** (environment/settings and the developer dimensions); codex's own
update engine documents that it "does not cover every model-visible item"
(`context_manager/updates.rs:217`; the same caveat is recorded in
`codexification.md`). Delta discipline is real but partial, not universal.

The `PromptSlot` enum is the extension seam: contributors *declare* a slot;
one routing point places them. Placement knowledge lives with the session, not
with the thing contributing the fragment.

### 1.2 Vibe: everything in one mutable system message (the Mistral-native shape)

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

**This is the existence proof for the task-burial failure:** mistral-medium follows one-line
instructions fine under a heavyweight position-0 system message and a clean
task-only user message. The model's tolerance problem is not "long context":
it is policy and memory text in the user lane competing with the task.

### 1.3 opencode: per-model base prompts, one delivery-divergence point

- **Base prompt selected per model family**, not per provider:
  `SystemPrompt.provider(model)` maps gpt-4/o1/o3→`beast.txt`,
  gpt→`gpt.txt`, codex→`codex.txt`, gemini→`gemini.txt`, claude→
  `anthropic.txt`, kimi/trinity/default (`src/session/system.ts:25-39`).
- **Agent persona REPLACES the base prompt** rather than stacking
  (`agent.prompt ? [agent.prompt] : SystemPrompt.provider(model)`,
  `src/session/llm/request.ts:58-66`).
- System array = persona-or-base + env block + instructions (AGENTS.md /
  CLAUDE.md / config instructions, each labeled `Instructions from: <path>`,
  `src/session/instruction.ts:155-169`) + skills, recomputed **every step**
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

### 1.4 Claude Code: lane placement as an explicit strategy lever

From `research/harness/claude/claude-context-management.md` (2.1.160, binary +
live observation): CLAUDE.md overlays and stable instructions ride the system
prompt; volatile per-machine sections (cwd, env, git status) ride the system
prompt by default but `--exclude-dynamic-system-prompt-sections` **moves them
into the first user message** for cross-user cache reuse; lane placement is a
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

The daemon knows *what applies to this dispatch*: the persona resolved from the
brofile, the completion contract when the dispatch carries one (ordinary
dispatches do; `allow_recursion` dispatches do not), and the pre-bound scope
IDs. That is data, not shape.

The harness knows *where things go*: it owns `SystemPrompt{stable, ambient,
volatile}` and each transport's native rendering, the `ContextualUserFragment`
layer with markers, the turn-1 contextual message, the baseline/re-emit
machinery, and session persistence. Placement, ordering, demarcation, re-emit
cadence, and per-transport/model-family variation are harness `context/`
concerns.

The boundary stays argv-shaped (harness-daemon-boundary.md §2): the daemon
passes content down; the harness never reaches up. Directive prose stays
daemon-owned (it references bbox_*/bro_* vocabulary the harness must not know);
the harness never interprets directive text, only places it. Recurring
behavioral nudges are not directives: they belong in the harness
HookEngine/NudgeLedger, triggered and throttled by actual turn state.

## 4. Boundary surface: `--dispatch-context <json>`

One structured flag (parsed in-process via `Cli::try_parse_from`; env fallback
`BRO_HARNESS_DISPATCH_CONTEXT` for the standalone binary), typed by
bro-protocol `DispatchContext`. Versioned and strictly parsed: unknown fields
and unknown `v` are errors, because the payload is daemon-authored and garbage
is a bug, not input to tolerate.

```json
{
  "v": 1,
  "persona": "You are a reviewer...",
  "directives": [
    {"id": "contract", "cadence": "standing", "needs_scope": true,
     "text": "If something genuinely notable came up..."}
  ],
  "scope": {"task": "...", "session": "...", "project": "...", "bro": "...",
            "thread": "...", "work_item": "..."}
}
```

- `persona`: brofile lens, verbatim. Optional.
- `directives`: the ordered set the daemon selected for this dispatch. The
  completion contract (`id: "contract"`) is the only directive the daemon
  emits. `id` is a stable label for diffing and debugging. `cadence` is
  `standing`: deliver once per request at system authority. `needs_scope`
  declares that the text references the scope block's correlation keys; the
  harness drops `needs_scope` directives whenever no current scope exists,
  because a contract telling the model to copy `task:` from a block that does
  not render is worse than no contract.
- `scope`: typed key/value fields, not pre-rendered lines; the harness renders
  and re-renders them. Rendering order is fixed: task first, then session,
  project, bro, thread, work_item. Blank values are skipped.

The parser accepts two shapes that older daemons and persisted harness
side-state carry and drops them on conversion: a `pins` field, and directives
with `per_turn` cadence. Every other unknown field or cadence value still
fails the parse.

`-p` is the operator's prompt **verbatim**, with no wrapper.

**Persist/restore semantics:**

- Flag present, non-empty: the payload **replaces** the persisted persona and
  directives wholesale and sets the current scope. Partial payloads are not
  merged; the daemon composes the full context each dispatch.
- Flag present, empty string or `{}`: explicit clear (persisted context
  removed; nothing renders).
- Flag absent: persona and the **non-`needs_scope`** directives restore from
  session side-state (the bare `--resume` standalone case). **`scope` is never
  restored**: task_id is per-dispatch correlation data, and a stale
  `scope.task` would mis-route `bbox_note` keys that the contract tells the
  model to copy verbatim. With no current scope, `needs_scope` directives drop
  for the same reason. Side-state restore is tolerant: an unparseable cell
  restores nothing. The persisted last-emitted scope baseline survives for
  delta comparison only.

Daemon-side dispatch paths pass the flag on every exec **and** resume.

## 5. Harness composition: classes and per-transport strategy

Semantic classes: **persona**, **standing directives**, **memory** (AGENTS.md /
rendered repo memory, discovery-owned), **scope**, **environment**, **task**.

The strategy seam is one harness `context/` routing point keyed by transport
(`CompositionStrategy::for_transport`, the analog of opencode's
`provider(model)` + delivery branch and codex's PromptSlot router). Two
strategies ship, because the transports demand two.

**Codex-shaped (anthropic, openai-responses):**

| Class | Slot |
|---|---|
| persona | system **stable**, after base/override, before directives |
| standing directives | system **stable**, after persona |
| memory | contextual user fragment |
| scope | contextual user fragment, `<bbox_scope>` markers; re-emit on change |
| environment | contextual user fragment, last in the turn-1 contextual message |
| task | own user item, verbatim, last |

Turn-1 contextual user message ordering: `UserInstructions` (AGENTS.md) →
scope → environment context (environment last, codex order).

**Vibe-shaped (openai-chat, the Mistral lane):**

| Class | Slot |
|---|---|
| persona, standing directives | **leading system message** (stable slot), after base |
| memory (AGENTS.md) | **leading system message**; on this transport memory does not ride the user lane |
| environment context | leading system message (vibe puts project context in system) |
| scope | **leading system message**, rendered as a demarcated section, rebuilt in place when the dispatch context changes (vibe `update_system_prompt`, §1.2) |
| task | the only initial user message |

The leading system message is position 0 and rebuilt per request, so the
Mistral system-after-tool constraint never binds; mid-session context changes
mutate the leading block in place rather than appending anything.

**The emitter is strategy-aware.** `prepare_context_for_user_turn` →
`emit_initial_context_if_needed` resolves each class through the strategy. On
the vibe-shaped strategy memory, environment, and scope resolve to the stable
system slot, so the initial-context emitter contributes nothing to the user
lane and `compose_system` renders them into the leading block.

**Stable-system ordering** (both strategies; base instructions render before
all of this, transport-side): explicit `--system-prompt` override → persona →
standing directives → memory → pinned-tools → environment → scope. The
per-resume-mutable section (scope) sits at the suffix so the prefix stays
byte-identical across leading-block rebuilds on the chat lane, where
openai-chat has only a session-level `prompt_cache_key` and no block-level
cache separation. Persona-before-directives mirrors opencode's persona-leads
shape; both sit after base so the model-family prompt stays the cache-stable
prefix across brofiles. opencode's persona-*replaces*-base is the existing
`provider_defaults: suppress` brofile mode, which composes with this design
unchanged (§8).

**Cache note.** `SystemPrompt.stable` carries the cache breakpoint and is
constant within a session in the common case. A resume that re-passes a
changed persona or contract legitimately rewrites it: one cache re-prime per
resume boundary. On Responses the stable text feeds the `instructions` field
per request; a resume-boundary change alters the request body, not the session
cache key.

**Named strategy variants the seam exists for** (not shipped):

- *volatile-mirror*: additionally re-state a terse contract reminder in the
  volatile tail for model families shown to ignore stable-slot policy.
- *chat-user-fragments*: the codex-shaped routing on openai-chat, if a future
  chat-transport model family prefers user-lane context.

A per-provider fix is a strategy-arm change, not preamble surgery.

## 6. Daemon side

`AmbientContext` produces no prompt text. `dispatch_context(lens)` serializes
the typed payload (persona threaded from the brofile lens, the contract
directive when `completion_contract` is set, scope fields; a `pending` session
id is omitted from scope). `ProviderExec::build_exec_args` and
`build_resume_args` take the operator prompt and the payload separately and
emit `-p <task> --dispatch-context <json>`.

Every caller of `build_exec_args` / `build_resume_args` / `spawn_task*` that
launches an ordinary bro turn passes a dispatch context and a verbatim prompt.
**Fresh and resume branches both compose the full payload, including
persona.**

`DEFAULT_COMPLETION_CONTRACT` references the scope placement-neutrally ("copy
`task:` from the `bbox_scope` context block"), valid for both the
user-fragment and system-section renderings.

The workload-retro probe (`bro_prune(retro=true)`) deliberately carries no
dispatch context: its prompt is self-contained, with an inline scope line and
the exact `bbox_gap` call it wants.

The workflow provider ignores the payload.

## 7. Resume, compaction, and re-emit mechanics

**Resume.** Each `bro_resume` is its own dispatch with a fresh task_id. The
daemon re-passes the full dispatch context every resume:

- persona and the contract replace the persisted values and land in the next
  request's stable render in place (the vibe `update_system_prompt` move on
  chat; a stable-block rewrite on anthropic/responses). No transcript
  pollution.
- scope: codex-shaped strategies compare against the **last-emitted**
  baseline in side-state and emit one short `<bbox_scope>` fragment in the
  contextual user lane when it changed; a scope that disappears emits a
  one-line revocation (`Prior dispatch scope has been cleared.`). On the
  vibe-shaped strategy the leading system rebuild carries it and nothing
  enters the user lane.

**Compaction.** The loop resets `reference_context_item = None` after
compaction; the next user turn re-runs `emit_initial_context_if_needed`. On
codex-shaped strategies that re-emit renders the **current in-memory dispatch
context** (current scope) alongside AGENTS.md and environment in the
contextual user message and updates the emitted baseline. On the vibe-shaped
strategy the helper emits nothing user-lane: those classes live in the leading
system block, which compaction never touches. The harness fragment layer has
markers but no `matches_text` registry, so compaction does not recognize old
fragments in the summarized transcript; correctness comes solely from the
deterministic re-emit path.

**Persistence.** The harness persists the dispatch context minus scope in
`side["dispatch_context"]` and the last-emitted scope baseline in
`side["dispatch_emitted"]`, following the existing side-cell pattern
(todos/nudges/lsp_baselines/reference_context).

Net effect: a resume turn's user lane is the operator's follow-up plus at most
a scope-delta fragment (codex-shaped) or nothing extra at all (vibe-shaped).

## 8. Guardrails

- **Provider-defaults suppression semantics:** `--system-prompt ""` clears
  `explicit_system` and disables AGENTS.md discovery but does not remove
  model-family base instructions, which every transport renders. A
  suppressed-defaults dispatch with a dispatch context gets base + persona +
  directives and no AGENTS overlay. True base suppression would be a separate
  flag, not an overload of this one.
- No mid-session **system** injections on openai-chat beyond the existing
  leading/volatile handling; the leading block mutates in place, and nothing
  system-roled ever follows a tool message.
- The deferred-tool manifest, tail-nudge, and structured-output channels
  (`SystemPrompt.ambient` / `.volatile`) are independent of the dispatch
  context.
- Prompts that deliberately bypass dispatch composition (the workload-retro
  probe) keep bypassing.

## 9. Validation

- Unit: per-strategy slot routing (anthropic system-block order; openai-chat
  leading-system composition including memory and scope sections and no
  system-after-tool; responses instructions); scope render, change re-emit,
  revocation, and post-compaction re-emit; dispatch-context persistence
  round-trip with scope-restore exclusion; payload `v` and unknown-field
  rejection; legacy `pins` and `per_turn` acceptance; suppressed-defaults ×
  dispatch-context interaction; daemon payload construction (contract-only
  directive set, scope elision of pending sessions).
- Live probes: a one-line task on the openai-chat (Mistral) lane acts on the
  task; a brodex (responses) smoke run.
- Full gate: `cargo nextest run --workspace`.
