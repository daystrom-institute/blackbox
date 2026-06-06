---
title: "Brodex agent-loop learnings from codex (env context, end_turn, parallel tools)"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - bro-harness
  - brodex
  - agent-loop
brief: "Three agent-loop (not transport) patterns worth adopting into bro-harness, mined from the openai/codex CLI: (1) structured <environment_context> injection — the standout gap; (2) honoring the Responses `end_turn` signal; (3) intra-turn parallel tool execution, reconciled against bro-harness's existing Promise async-concurrency layer. Compaction is deliberately out of scope (separate effort). Cites codex source at the local clone."
---

# Brodex agent-loop learnings from codex

> **Method / scope.** Source-mined from the local `openai/codex` clone at
> `/home/invidious/repos/codex` (`codex-rs/…`, HEAD `c955f730`, fetched
> 2026-06-02). This is the agent-loop / turn-machinery companion to
> `brodex-responses-deep-dive.md` (transport) and `brodex-websocket-transport.md`
> (WS). **Compaction is explicitly out of scope here** — a canonical OAI
> compaction extraction is a separate effort. bro-harness paths are relative to
> `crates/`. Findings are ranked by value; #3 includes the requested
> `parallel_tool_calls` ↔ Promise-system reconciliation.

## 1. Structured environment-context injection — HIGH value (the standout gap)

**Codex.** Each turn injects a `<environment_context>` block as a context item
(a `ContextualUserFragment`), carrying the model's operating reality:

- Tags: `codex-rs/protocol/src/protocol.rs:92,94,95`
  (`USER_INSTRUCTIONS_OPEN_TAG`, `ENVIRONMENT_CONTEXT_OPEN_TAG/CLOSE_TAG`).
- Renderer: `codex-rs/core/src/context/environment_context.rs` —
  `<cwd>` (`:545-546`), `<shell>` (`:549`), `<current_date>` (`:567`),
  `<timezone>`, `<network>` with allowed/denied domains (`:315,324-326`), and
  `<filesystem>` with `<workspace_roots>` (`:145-154`) and a
  `<permission_profile>` that is `managed` / `restricted` / `unrestricted`
  (`:181-222`).
- Cadence: assembled in `Session::build_initial_context`
  (`codex-rs/core/src/session/mod.rs:2636`, invoked at `:2905`); base
  instructions are selected once per session (per-model, per-personality), while
  the volatile context (date/cwd/permissions) is rebuilt per turn.

**bro-harness today.** `compose_system` (`bro-harness/src/agent_loop.rs:1077`)
builds the system prompt from `base` (the AGENTS.md overlay discovered by
`project_doc::discover`, `agent_loop.rs:323`, or an explicit `--system-prompt`)
plus the pinned-tools / ref-ABI guidance. **No structured environment block is
injected.** The harness *knows* the cwd — `ToolCx.root =
std::env::current_dir()` (`agent_loop.rs:378`) — but never surfaces it to the
model, and nothing tells the model the date, OS/arch, shell, or its
safety/network posture.

**Why it matters.** A coding agent with no environment grounding hallucinates
the date, doesn't know its cwd (wrong relative paths, redundant `pwd`/`ls`
probes), and doesn't know it is non-interactive and safety-filtered (so it
attempts operations the `bro-tools` denylist will reject). This is the
agent-loop analog of the transport modernization and the change most likely to
move output quality.

**Recommendation.** Inject a stable `<environment_context>` into the
**cache-stable** system prefix at session start (it fits the existing
stable/volatile split — `SystemPrompt.stable`). Fields brodex can honestly
populate: `<cwd>` (`ToolCx.root`), OS/arch (`std::env::consts::{OS,ARCH}`),
shell, and `<current_date>` at session start (a session is short enough that
start-of-session date is fine; refresh into the volatile tail only if long
sessions warrant it). The one honest adaptation vs codex: brodex's "sandbox" is
not seatbelt/landlock — it is the `bro-tools` safety denylist + sensitive-file
guard (`crates/bro-tools/src/safety.rs`). The block should describe *that*
posture (commands filtered by a denylist, secrets guarded, no approval prompts —
non-interactive), not an OS sandbox. **This is the finding I'd build first.**

## 2. Honor the Responses `end_turn` signal — MEDIUM value, low effort

**Codex.** `end_turn: Option<bool>` is a real field on the Responses
`response.completed` payload — defined at
`codex-rs/codex-api/src/common.rs:90` and `codex-rs/codex-api/src/sse/responses.rs:105`,
parsed from `resp.end_turn` at `codex-rs/codex-api/src/sse/responses.rs:365`. The
turn loop continues when it is `Some(false)`:

```
// codex-rs/core/src/session/turn.rs:2015
if let Some(false) = end_turn {
    needs_follow_up = true;
}
```

and the outer loop only stops when `!needs_follow_up`
(`codex-rs/core/src/session/turn.rs:321`). So the model can say *"I'm not done,
call me again"* **without emitting a tool call**. (Tool-call presence is the
other driver: `needs_follow_up |= output_result.needs_follow_up`,
`turn.rs:1883`.)

**bro-harness today.** `parse_sse`
(`crates/bro-harness/src/transport/responses_common.rs`) reads
`response.completed` / `response.incomplete` but **ignores `end_turn`**, and the
loop terminates whenever there are no tool calls:

```
// crates/bro-harness/src/agent_loop.rs:609
if out.stop != StopReason::ToolCalls || out.tool_calls.is_empty() {
    break ...
}
```

So a server-driven "keep going" turn (`end_turn:false`, no tool call) is
**wrongly terminated** by our heuristic.

**Recommendation.** In `parse_sse`, read `r["end_turn"]`; when it is
`Some(false)`, surface a "continue" signal (e.g. a `StopReason::Continue`, or a
flag on `TurnOutput`) so the loop re-invokes the model rather than breaking. The
model's text output is already replayed into the buffer, so the next call simply
continues. Low effort; verify how often the live backend actually sends
`false` (test fixtures show `None`, so it may be rare) before investing further.

## 3. Intra-turn parallel tool execution — reconcile with the Promise layer

> **As-built (2026-06, supersedes the analysis below).** Both halves of this
> section have since landed, and the reconciliation resolved the *opposite* way
> from this doc's "Promise layer is the load-bearing concurrency story":
> - **Intra-turn parallel dispatch shipped** as codex's read=shared/write=exclusive
>   idiom, adapted to bro-harness's single-owner LSP state as two phases:
>   read-only tools (per `Tool::annotations().read_only`, via `Registry::read_only`)
>   dispatch **concurrently** (`agent_loop.rs`, `join_all`); mutators stay serial.
>   Validated live (GLM Anthropic transport, `parallel=3`).
> - **The Promise push-system was retired** (`PromiseStore`, the 6 `promise_*`
>   tools, `shell_run mode="promise"`, the auto-injected `HARNESS_EVENT` turns).
>   On a codex-convergence branch it was a redundant second mechanism for the
>   long-running axis that codex covers with pull-based yield-poll
>   (`shell_run` cooperative yield + `shell_poll`, code-mode `wait`) — which
>   bro-harness already had. Long-running concurrency is now codex-shaped:
>   in-turn parallel reads + cooperative-yield/poll for long commands. So where
>   the analysis below treats the Promise layer as the keeper and
>   `parallel_tool_calls` as optional, the realized design is the inverse.
> See `design/bro-harness/codexification.md` and the Stage A/B commits. The
> original analysis is retained below for provenance.

**Codex.** When the model emits several tool calls in one assistant message,
codex dispatches them **concurrently** and joins before the next model turn:

- `use futures::stream::FuturesOrdered;` (`codex-rs/core/src/session/turn.rs:108`)
- in-flight queue: `turn.rs:1737-1738`; tool futures pushed on
  `OutputItemDone`: `turn.rs:1878` (`in_flight.push_back(tool_future)`)
- drained in order at turn end: `drain_in_flight` (`turn.rs:1665-1689`)
- gated by model capability: `parallel_tool_calls:
  turn_context.model_info.supports_parallel_tool_calls` (`turn.rs:906`).

This is **intra-turn, synchronous-join** concurrency: N short calls run at once,
all results return together, then the model continues.

**bro-harness today.** Tool calls are dispatched **strictly sequentially**
(`'dispatch: for tc in &out.tool_calls` at `crates/bro-harness/src/agent_loop.rs:623`)
and the request sets `parallel_tool_calls: false`
(`crates/bro-harness/src/transport/responses_common.rs`, `build_body`).

### The crosscut: bro-harness already has a *different* concurrency axis — Promises

bro-harness's Promise layer (`crates/bro-tools/src/promise.rs`) is a separate,
**cross-turn, asynchronous** concurrency model:

- `PromiseStore` (`promise.rs:133`) with `start` (`:158`), `settle_*`
  (`:185-193`), `cancel` (`:216`), `status` (`:228`), `list` (`:235`),
  `all_terminal`/`any_terminal` (`:244,258`), `drain_completion_events`
  (`:270`), and a completion `Notify` (`:154`). Running-progress heartbeat
  (elapsed / last-output / byte counts) via `PromiseProgress` (`:46,64`).
- Promise tools: `promise_status` / `promise_wait` / `promise_when_all` /
  `promise_when_any` / `promise_cancel` / `promise_list` (`promise.rs:363-368`).
- In fleet mode, `shell_run` starts a command **as a harness-local Promise** and
  returns `{promise_id,state,running,next_step}` immediately
  (`crates/bro-harness/src/agent_loop.rs:68,84`); the blocking yield-poll path is
  intentionally unavailable there. Completion **auto-injects a hidden
  `HARNESS_EVENT` turn** — `promise_completion_event_prompt`
  (`agent_loop.rs:849-852`) drains completion events, and the session loop wakes
  on `promise_notifier()` (`agent_loop.rs:153-173,520,845`) to deliver them.

So bro-harness *already* overlaps work: a single tool call kicks off background
execution, the model regains control immediately, multiple Promises run at once,
and the model joins later (`promise_when_all`/`when_any`) or is notified. This is
the **long-running** concurrency need (builds, servers, long commands), and it is
arguably stronger than `parallel_tool_calls`, which blocks the turn until every
call in the batch finishes.

### Reconciliation — two axes, mostly complementary

| | `parallel_tool_calls` (codex) | Promise layer (bro-harness) |
|---|---|---|
| Granularity | N calls in **one** assistant message | **one** call → background handle |
| Join | synchronous, **same turn** | async, **cross-turn** (`when_all`/notified) |
| Best for | a batch of **short** calls (reads/greps) | **long-running** work (build/server) |
| Status today | off (`parallel_tool_calls:false`, sequential) | built + live (fleet `shell_run`, `promise_*`) |

The Promise layer already covers the long-running axis, so
`parallel_tool_calls` is **not** a prerequisite for concurrency in brodex; its
only marginal value is batching **short synchronous** calls. And it interacts
with the rest of the loop in ways that make a naive flip unsafe:

1. **Promise-producing tools are already non-blocking** — fleet `shell_run`
   returns instantly. Parallelizing them is redundant (the Promise model already
   overlaps them) and harmless (independent background processes; their batch
   "result" is just the `promise_id`, settled later via the injected turn).
2. **Synchronous read-only tools** (`file_read`, `content_search`, `glob`, git
   reads) are the genuine, safe win — no mutation, true latency reduction.
3. **Synchronous mutating tools** (`file_write`, `file_edit`, clip writes, a
   blocking `shell_run`) must stay **serialized**. The safety model is a
   denylist, not isolation, so concurrent edits to the same path or racing shells
   can clobber; and the **window-0 diagnostics** seam
   (`append_window0_diagnostics`, `agent_loop.rs:859`) drains a per-dispatch edit
   sink after each tool result, which assumes edits are serialized — concurrent
   mutators would interleave the sink.
4. **Interrupt handling** pads not-yet-resolved calls with
   `INTERRUPTED_TOOL_RESULT` in dispatch order (`agent_loop.rs:656-676`);
   concurrent dispatch complicates cancel-and-pad bookkeeping.

**Recommendation.** Treat `parallel_tool_calls` as **low-to-medium priority and
optional**, distinctly *behind* findings #1 and #2. The Promise layer is the
load-bearing concurrency story and is already shipped. If we do adopt intra-turn
parallelism, do it **selectively**: classify tools via the existing
`Tool::annotations()` hook (`crates/bro-tools/src/tool.rs`) into read-only vs
mutating, dispatch read-only concurrently while serializing mutators (and let
promise-producers run as-is), and only then set `parallel_tool_calls:true`.
Flipping the flag without that classification invites file/shell races and
breaks the window-0 edit-sink assumption.

## Not gaps (bro-harness is at parity or ahead)

- **Abort/interrupt consistency.** Codex uses a `CancellationToken` +
  `drain_in_flight`; bro-harness cancels mid-dispatch and pads unresolved calls
  with `INTERRUPTED_TOOL_RESULT`, then repairs role alternation via
  `note_interrupted` (`agent_loop.rs:656-690`). Equivalent intent.
- **Per-turn observability.** Codex emits a `TurnDiff`; bro-harness already has
  LSP baselines + window-0 diagnostics + a suspicious-turn-end heuristic
  (`turn_end_diagnostics`, `agent_loop.rs:733`) — a richer, diagnostics-first
  take, and the Promise summary surfaces outstanding async work
  (`agent_loop.rs:789-795`).
- **Tool-call argument streaming** (`ResponseEvent::ToolCallInputDelta`,
  `codex-rs/core/src/session/turn.rs:2056`). Cosmetic for a headless harness;
  brodex materializes the full `function_call` at `response.output_item.done`.
  Skip.

## Priority summary

1. **Environment-context injection (#1)** — highest value; clean fit in the
   stable system prefix; brodex-honest posture (denylist, not OS sandbox).
2. **Honor `end_turn` (#2)** — cheap correctness tidy-up; verify live frequency.
3. **Selective intra-turn parallelism (#3)** — optional, behind the Promise
   layer which already owns long-running concurrency; only safe with a
   read-only/mutating tool classification.
