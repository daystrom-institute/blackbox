---
title: "bro-harness design map"
kind: design-hub
corpus: blackbox-design
topic:
  - orchestration
  - surfaces
brief: "Entry point and dependency/sequence map for the bro-harness design cluster: the custom provider harness, its built-in tool surface, and the capability layers built on top (clipboard, tool chaining, hooks/nudges, neuralyze). States what is built vs designed, the cross-doc dependency graph, and the recommended build order. Per-feature detail lives in the linked docs; this map is the spine."
---

# bro-harness design map

`bro-harness` (`crates/bro-harness`, `crates/bro-tools`) is the custom headless
coding agent that speaks provider APIs directly behind one `Transport`
interface, runs its own tool-calling loop, and emits the Claude stream-json
envelope so it slots into the existing dispatch seam (GLM/DeepSeek on the
Anthropic transport, Brodex on OpenAI Responses). See `PROJECT.md` →
"Provider & Agent Surfaces" for routing facts.

This page is the **map** for the design cluster — it owns the cross-doc
dependency graph and build order. It does **not** restate per-feature detail;
each linked doc is the authority for its own area. Keep coarse status here
(built / designed); let each doc own the specifics.

## The cluster

| Doc | Area | Status |
|---|---|---|
| [anthropic-harness](anthropic-harness.md) | transports, agent loop, deferral/tiering, recursion guard | **built** (live end-to-end; + SSE ×3, model-keyed compaction, bidirectional session/control protocol, report tool, bounded results) |
| [bro-harness-tool-surface](bro-harness-tool-surface.md) | the ideal built-in tool subset (read/search/glob, shell, todo, git, web) | **mostly built** — Tier A + shell quartet + todo done; per-call escalation deferred |
| [bro-harness-clipboard](bro-harness-clipboard.md) | `clip_*` registers (yank/paste/gather), the settled-ref store | **built + expanded** — 6 core tools plus `clip_transform`/`clip_slice`/`clip_grep` |
| [bro-harness-tool-chaining](bro-harness-tool-chaining.md) | the ref ABI; settled refs (clipboard) vs pending refs (Task) | **partial** — Stage 1+2 built (producers/consumers); Stage 3 (pending refs = Task) designed |
| [bro-harness-hooks](bro-harness-hooks.md) | internal hook seam + Nudger (steer toward the rich toolbox) | **partial** — §1 split + Nudger v1 built (engine, ledger, 4 rules); broader rules + adoption instrumentation remain |
| [bro-harness-neuralyze](bro-harness-neuralyze.md) | rewind context/files to a checkpoint + carry one message | designed |

## What is built (committed)

- **Transport / loop / tiering** — three transports behind `Transport`, each
  with SSE streaming (`anthropic`, `openai_chat`, `openai_responses`); deferred
  tool loading via `tool_search`; client-side allow/deny recursion guard.
- **Tool surface, core** — `file_read` (stream + line cap + line_numbers),
  `content_search` (mode/context_lines/case_insensitive), `glob` (mtime/name +
  cap), `file_edit`/`file_write`/`list_dir`, the git read tools + `git_commit`,
  `web_fetch`, `smart_read`.
- **Shell quartet** — `shell_run`/`shell_poll`/`shell_kill`/`shell_list`
  (Codex yield-poll model + correctness fixes: bounded reader drain,
  `close_stdin`, signal/escalation, session cap, `env`).
- **Clipboard + ref ABI** — the `clip_*` register store (`clipboard.rs`), six
  core tools plus `clip_transform`/`clip_slice`/`clip_grep`; ref-ABI producers
  (`shell_run{stdout_to,stdin_from}`, `web_fetch`/`content_search`/`file_read`
  producers) so tool output settles into a register instead of context.
- **`todo_write`** — durable across `exec → resume`.
- **Model-keyed compaction** — `compaction.rs`: threshold keyed on the model's
  context window, config-file overridable; manual `/compact` slash command plus
  auto-trigger on threshold breach across all three transports.
- **Bidirectional session + control protocol** — `session.rs` +
  `agent_loop.rs` `session_loop`: a persistent `--input-format stream-json`
  session that takes control requests (interrupt, model change, `/compact`) and
  emits `control_response`. Integration-tested against a mock transport.
- **Report tool** — `report.rs`: a harness builtin emitting a `report`
  stream-json line for the fleet cockpit; unpinned in normal CLI mode, pinned in
  fleet mode (`PinPolicy::also_pin`). Distinct from the daemon's `bro_report`.
- **Bounded tool results** — `bound.rs`: oversized tool output spills to a head
  + rider pointing at the full file; cap via `BRO_HARNESS_TOOL_RESULT_CAP_KB`.
- **Persistence spine** — `SaveState` + the transport-agnostic `side` cell in
  `SessionStore`; the load-bearing substrate everything stateful reuses.
- **System-prompt static/volatile split** — `compose_system → SystemPrompt`
  (cache-stable prefix + volatile tail); hooks-doc §1, a correctness fix in its
  own right and the delivery channel for volatile nudges.
- **Hooks Nudger v1** — `hooks.rs`: `HookEngine` + `NudgeLedger` + `Delivery`,
  with four shipped rules (`CopyPasteHook`, `ShellGrepHook`,
  `RefactorSignpostHook`, `HedgedConventionHook`) wired into all three loop hook
  points; one-nudge-per-turn gate, ledger on `side`, `BRO_HARNESS_NUDGES`
  kill-switch.
- **Note-contract softening** — notes are a signal channel, not a per-dispatch
  ritual. The `DEFAULT_COMPLETION_CONTRACT` text is **daemon-side**
  (`src/orchestration`, applied by `dispatch.rs`/`atoms.rs`/`roster.rs`) and
  surfaced to harness agents at dispatch time — not a harness-crate symbol.

## What is designed (not built)

- **Tool chaining, Stage 3** — pending refs = Task. Stages 1–2 (clipboard
  registers + `kind`-tagged producers/consumers) are built; Stage 3 enters only
  once an async producer (background shell / in-harness sub-dispatch) needs it.
- **Hooks Nudger, hardening** — the engine, ledger, delivery, and four rules
  ship (see "what is built"). What remains is the broader rule set and the
  adoption instrumentation (§ of the hooks doc that tracks whether a nudge was
  adopted, declined, or ignored).
- **Neuralyze** — auto-per-turn checkpoint substrate, context rewind, the file
  inverse-diff journal, self-invocation guards. Nothing built.

## Dependency graph

The arrows that actually constrain build order:

```
persistence spine (side cell)  ── BUILT ──┐
   ├─→ todo_write                  (built)│
   ├─→ clipboard registers                │  every stateful layer rides `side`;
   ├─→ hooks nudge ledger                 │  it is the one shared substrate
   └─→ neuralyze checkpoint store + budget┘

system-prompt static/volatile split ── BUILT ──→ hooks volatile nudge delivery

clipboard `side` register pattern ──→ hooks ledger, neuralyze stores
                                       (same "observer mutates side across turns")

ref ABI `kind` tag ──→ chaining Stage 2 ──→ chaining Stage 3 (Task)
clipboard (Stage 1) ──→ chaining Stage 2 producer/consumer args
                        (clip_* is the first ref backing store)

file inverse-diff journal (NOT built; reuse refactor::apply snapshot
   + shell_run `touches`) ──→ neuralyze keep_files=false (file revert)

checkpoint substrate (NOT built) ──→ neuralyze (all modes)

supervision::AlertKind::loop ── BUILT (daemon) ──→ neuralyze advised trigger,
                                                    hooks escalation signal
```

Two facts fall out of this graph:

- **The `side` spine is the keystone.** It is already built, and clipboard,
  hooks ledger, and neuralyze all hang off it. Nothing stateful needs new
  persistence machinery — that is the payoff of having built the spine first.
- **Detection is already solved.** `supervision.rs` (`AlertKind::loop`, daemon
  side) is the spiral detector for neuralyze and the escalation signal for
  hooks; neither re-derives it.

## Recommended build order

Ordered so each step unlocks the next and proves a pattern before it is reused.

1. **[done]** Transport/loop, Tier A surface, shell quartet, `todo_write`,
   persistence spine, system-prompt split, note-contract softening.
2. **[done]** Clipboard Stage 1 (`clip_*` on the `side` spine). Proves the
   register pattern and is the first ref backing store; unblocks chaining.
3. **[done]** Hooks Nudger v1 (scaffold + ledger on `side` + four behavioral
   rules). §1 was already built; the engine/ledger/delivery + rules now ship.
   Adoption logging is the remaining hardening step, not a blocker.
4. **[done]** Chaining Stage 2 (the `kind` tag + producer/consumer args —
   `shell_run{stdout_to,stdin_from}`, `file_read{into}`, `file_write{from}`).
   Generalizes the clipboard from file slices to any tool output.
5. **Neuralyze**, in its own internal order: checkpoint substrate →
   context-only rewind (delivers the conversation-steering case) → file
   inverse-diff journal + `keep_files=false` → self-invocation + guards.
6. **Chaining Stage 3** (pending refs = Task) — only once an async producer
   (background shell / in-harness sub-dispatch) actually exists to need it.

Steps 2–4 are independent enough to reorder by appetite; step 5 leans on the
journal pattern (`refactor::apply` snapshot) and the `side` spine but not on the
clipboard. Step 6 is gated on real async work, not on the others.

## Conventions for this cluster

- The async/temporal layer (sessions, checkpoints, pending refs) is
  **harness-owned**, never behind MCP; MCP tools stay synchronous unary.
- Privilege lives in `SafetyPolicy` + the brofile allow/deny layer. Nudges
  steer, they never gate; neuralyze rewinds, it never escalates privilege.
- Session-scoped state only (clipboard, todos, nudge ledger, checkpoints,
  `activated` set) — no cross-session / cross-bro sharing.
- Provider-agnostic ambient/contract text uses **bare** tool names
  (`bbox_note`, not `mcp__blackbox__bbox_note`); FQDN surfacing is a per-CLI
  concern, not the daemon's prompt text.
