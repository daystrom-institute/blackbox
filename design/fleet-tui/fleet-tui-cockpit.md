---
title: "Fleet TUI — multi-provider agent cockpit"
kind: design
lifecycle: archived
corpus: blackbox-design
topic:
  - fleet-tui
  - surfaces
brief: "A new top-level bro-client TUI: a human cockpit for dispatching and live-driving many concurrent TOP-LEVEL entrypoint agents across providers — GLM, DeepSeek, Brodex (all via bro-harness) and Claude as first-class peers. The TUI drives every entrypoint agent over ONE control protocol: the Claude Agent SDK bidirectional stream-json control scheme (control_request/control_response + stream-json events). The keystone: blackbox today dispatches EVERY provider one-shot (-p --output-format stream-json), so the cockpit needs the bidirectional input mode the claude CLI already supports but blackbox does not yet use, AND bro-harness must implement that mode (it is one-shot today). The TUI links the orchestration core as a library and spawns agents in-process; blackboxd is not in the execution path. Every claim below is cited to code."
---

# Fleet TUI — multi-provider agent cockpit

> **As-built record.** The v1 cockpit and its entire harness-side substrate
> (keystone §2, exec model §3, roster/nav/transcript §5) are built and runnable.
> The residual work was excised to the backlog:
> [`backlog-follow-ons.md`](./backlog-follow-ons.md) (§7 items 9/10/15/16:
> input-history persistence, allocator probe-core extraction, capability badges
> + headroom v2, Alerting reuse + `/resume`-deleted; §7.8 `@project`
> cwd+MCP config is now built)
> and [`backlog-standalone-view.md`](./backlog-standalone-view.md) (§5.5).
> Operator-feedback UX polish (focused executor/classifier activity strip, roster
> `report` teaser, compact tool-call rendering) has landed and is recorded in
> [`backlog-ux-polish.md`](./backlog-ux-polish.md). §7 is retained below as the
> as-built ledger.

> **Status.** Partial — **the v1 cockpit is built and runnable** (`bro fleet`,
> 2026-05-30, branch `feat/fleet-tui-cockpit`). The **entire harness-side
> substrate** is implemented in `crates/bro-harness`: SSE streaming across all
> three transports, model-keyed compaction, the bidirectional session + control
> protocol (interrupt / steer / `/compact`), the builtin `report` tool, and
> bounded tool results — with mock-transport integration tests. The **client
> side** is implemented: the daemon-drive seam (`spawn_task_interactive`, §7.2 /
> §7.6), the `FleetOrchestrator` façade (§7.7), and the TUI (§7.11–16) —
> dispatch, live steering, interrupt, `/compact`, the verbose transcript, the
> table roster with state buckets + timing columns, the navigation model +
> slash-command autocomplete, session persistence/reload, resume-on-steer, and
> Ctrl+X stop/delete. What remains are the **follow-ons**: input-history
> persistence (§7.9), the allocator probe-core extraction for provider-headroom
> v2 (§7.10), Alerting-bucket supervision reuse, and `/resume` of a *deleted*
> session. Per-item status (✅ done · ◑ partial ·
> ○ follow-on) is marked in §7.
>
> **Grounding.** Every claim is cited `file:line`. Blackbox claims cite this
> repo. The bidirectional transport flags are **verified live against the
> `claude` CLI 2.1.157**: `--input-format stream-json`, `--output-format
> stream-json`, and `--replay-user-messages` (re-emit stdin user messages, gated
> on both stream-json formats). The `control_request`/`control_response` and
> system-event *message* shapes ride inside that NDJSON stream and are grounded
> in the `hyperclaude` reference (`/home/invidious/repos/hyperclaude/docs/`), not
> separate CLI flags.

A new top-level surface in the `bro` client (`src/cli.rs`, subcommands at
`cli.rs:86-90`): a **human cockpit** for running and live-driving many
**top-level entrypoint agents** at once — the agents *I* dispatch and steer
directly, the top of the stack. One row per entrypoint agent. Modeled on Claude
Code's Agent View (FleetView; external reference), but **provider-agnostic**:
GLM, DeepSeek, Brodex, and Claude as first-class peers. All of GLM/DeepSeek/
Brodex dispatch through `bro-harness` (`providers.rs` `Provider` enum +
`PROJECT.md` routing), so "harness convergence" (§2) covers all three.

**Scope boundary.** This is *only* about driving top-level entrypoint agents.
What those agents spawn underneath them (nested bros, sub-dispatches) is a
separate, orthogonal concern handled elsewhere — `bro_dashboard`
(`src/tools/roster.rs:32`), `bro tail` (`cli.rs:86`), and the (designed,
those, but does not own them. Not the council board (`cli.rs:90`,
`council_tui.rs`). Not a workflow DSL.

## 1. The control protocol (the whole design)

The cockpit drives **every** entrypoint agent over **one** protocol: the **Claude
Agent SDK bidirectional stream-json control scheme** — `control_request` /
`control_response` plus stream-json events (hyperclaude `docs/SDK_PROTOCOL.md:11`,
`:253`). It targets one contract, not per-provider control adapters.

The channel carries, per session:

- **User input** — successive user-turn messages over stdin (persistent session,
  not one-shot).
- **`control_request`** — e.g. `interrupt` (`SDK_PROTOCOL.md:109`), `set_model`
  (`:127`), `set_max_thinking_tokens` (`:138`), `stop_task` (`:217`), each with a
  `control_response` (`:253`).
- **stream-json events** — assistant/tool/result blocks for the live transcript
  (the same envelope blackbox already parses; `providers/events.rs:62`).
- **system events** — `init` advertising `slash_commands` (which include
  `compact`; `NDJSON_FORMAT.md:170`) and `compact_boundary` on manual `/compact`
  or auto-compaction (`NDJSON_FORMAT.md:176-194`).

### 1.1 Verb mapping

| Verb | Mechanism | Cite |
|---|---|---|
| steer / reply | user-turn message into the live session | — |
| interrupt (`Esc`) | `control_request{subtype:"interrupt"}` — like the provider CLIs | `SDK_PROTOCOL.md:109` |
| /compact | in-stream slash command; agent emits `compact_boundary` | `NDJSON_FORMAT.md:170,176` |
| set model | `control_request{set_model}` | `SDK_PROTOCOL.md:127` |
| clear | session lifecycle — new `session_id` (v2 / standalone only, §5.5) | — |
| resume | session lifecycle — `--resume <id>` | `exec_args.rs:334` (`build_resume_args`) |

`clear` and `resume` are the cockpit's session-lifecycle concern, not protocol
messages.

## 2. The keystone: drive the bidirectional mode; the harness must implement it

Blackbox today dispatches **every** streaming provider **one-shot**: the Claude
arm builds `-p <prompt> --output-format stream-json` (`exec_args.rs:218-221`;
resume `:354-357`) with **no `--input-format`**, and `bro-harness` likewise
reads a single `-p`/stdin prompt (`crates/bro-harness/src/cli.rs:11-26`), runs
one tool-calling turn (`agent_loop.rs:34`, loop `:154`), emits a result, and
exits. So neither path is bidirectional today.

The asymmetry that matters:

- **Claude** — the `claude` CLI *supports* the bidirectional input mode the
  protocol needs (`--input-format stream-json` + `--replay-user-messages`,
  verified live in claude 2.1.157; control messages per hyperclaude
  `SDK_PROTOCOL.md`), blackbox just doesn't invoke it that way yet. Driving
  Claude in the cockpit is **new usage of an existing CLI capability**.
- **bro-harness (GLM/DeepSeek/Brodex)** — has no such mode. It must **implement**
  the scheme to converge:
  - a **persistent bidirectional stream-json session** (read input + control on
    stdin, keep the loop alive between turns — today it exits after one turn,
    `agent_loop.rs:34,154`);
  - the baseline `control_request`/`control_response` round-trip (`interrupt` at
    minimum);
  - `/compact` on its own conversation buffer + a `compact_boundary`-equivalent.

"Enhance" room: the harness may add its own `control_request` subtypes, **as long
as the baseline contract stays uniform across providers.**

This is the linchpin: until both the input mode is driven (Claude) and
implemented (harness), the cockpit can't live-steer at all.

### 2.1 Codex deferred (Brodex covers the backend)

Plain Codex (the `codex` CLI; `codex` → codex-CLI per `PROJECT.md`) is **deferred
for now**: whether the codex CLI speaks a compatible bidirectional control
protocol is unverified, and we don't need to find out. **Brodex** reaches the
same OpenAI/ChatGPT (Codex) backend through `bro-harness` on the Responses
transport (anthropic-harness.md; `Provider::Brodex`, `providers.rs:59`), so it
converges onto the protocol via §2 along with GLM/DeepSeek. The OpenAI backend is
therefore a first-class cockpit peer through Brodex; codex-CLI streaming is
revisited only if a need the harness path can't serve appears.

Companion lib change: the in-process spawn path moves large prompts to stdin then
does not keep a persistent writable stdin (`move_large_prompt_arg_to_stdin`,
`mod.rs:1217,1304`); a persistent session needs **child stdin kept open and
writable**.

### 2.2 Builtin `report` tool (fleet mode)

The harness gets a builtin `report` tool — the agent's status/needs signal,
emitted on the stream-json the cockpit reads (drives the Waiting bucket + the row
summary, §5). It is a harness builtin (registered in `registry.rs` beside
`todo_write`/`shell_run`), **unpinned in normal CLI mode and pinned in fleet
mode** (Pinned tier, `PinPolicy`/`BRO_HARNESS_PIN_TOOLS`, anthropic-harness.md) so
the agent reaches for it.

It is **not** the daemon's `bro_report` (`tools/roster.rs:170`), which serves
`bro_exec`/atom/workflow bros and is `?surface=`-gated off fleet agents
(`server/surface.rs`). No shared field, no convergence — fleet agents are not
bros.

### 2.3 Bounded tool results (cap + spill)

A harness constraint the fleet's verbose transcript (§5.4) surfaces — a general
win, not fleet-specific. Today tools self-truncate ad hoc and **discard** the
overflow (`shell.rs:162` `render()` ~10k tokens, `clipboard.rs:37` 256 KB,
`file_read` line caps); there is no spill or retrieval path (nothing in
`agent_loop.rs`/`tool.rs`). Add a uniform loop-level rule: **any tool result
> N kB → write the full payload to a harness-owned dump path and replace the
inline result with a head + rider** (`huge response (N kB) read from disk at
/x/y/z`). Lossless (retrieve via existing `file_read`/`smart_read`),
context-hygienic (the model isn't flooded, not just the TUI), uniform (all tools
incl. MCP results). Needs net-new **dump machinery**; `N` is a config knob.

### 2.4 Compaction machinery (for `/compact`)

`/compact` (§1.1) is **not free** for harness agents. When Claude receives the
compact control code, *Claude's own machinery* runs the summarization. bro-harness
has **no summarization machinery at all** — only the discard-truncation above — so
it literally cannot react to a compact code today. So `/compact` for harness
agents requires **net-new compaction machinery in bro-harness**: summarize older
turns → replace the prefix with the summary → continue, emitting a
`compact_boundary`-equivalent. This is substrate, not a flag (it's the "entire
compaction machinery" the design was glossing). Claude agents get `/compact` for
free; harness agents need this built.

## 3. Execution model (in-process, no daemon)

Reusing orchestration lib code in-process (`spawn_task`, `TaskStore`, supervision
logic, `parse_event`) is the point — the **only** hard line is **daemon RPC**: no
HTTP to a running `blackboxd`.

- The `bro` binary links the `blackbox` lib (same package `[[bin]]`; e.g.
  `council_tui.rs:261` calls `blackbox::config::load()`). The cockpit constructs
  its own `TaskStore` + tail `broadcast::Sender<TailEvent>` + `store_dir` and
  calls `orchestration::spawn_task` directly — its signature takes those as
  plain args (`mod.rs:1171`, `store_dir:1178`, `tail_tx:1180`), no
  `BlackboxServer`. **blackboxd is not in the execution path.**
- Each entrypoint agent is a subprocess (`spawn_task_reserved`, `mod.rs:1264`):
  bin resolved through a login shell (`mod.rs:1293,1302`), env hygiene
  (`NO_COLOR`/`TERM`/`FORCE_COLOR` at `:1315-1317`, strips
  `BLACKBOX_SERVICE_ENV_VARS` at `:1327`), large prompt → stdin (`:1304`).
- **Persistence/resume** without a daemon: `TaskStore::persist` (`mod.rs:334`) /
  `load` (`:383`); `load` flips a crashed `Running` task to `Failed` +
  `recoverable=true` (`:399-408`, field `:140`) — recovery already modeled.
- **Concurrency** is the cockpit's own (parallelism cap + queue) — net new.
- **Ownership** is clean: the cockpit owns exactly what it spawned.

## 4. Provider-first-class

- **Provider as a display dimension** — `Provider` enum (`providers.rs:48`);
  per-row glyph/color; filter and grouping key.
- **Capability badges** — `Provider::capabilities()` (`providers.rs:127-146`):
  e.g. Claude = ToolUse/Resume/Vision/StructuredOutput/LongContext;
  GLM/DeepSeek/Brodex = ToolUse/Resume. Dispatch UI reflects gaps.
- **Provider selection, sticky-next** — set via the provider selector (§5.1),
  applies to the *next* dispatch only; never re-providers a live agent (an
  agent's provider is fixed at spawn). v1 cycles a text field. **v2** shows
  per-account **headroom to route by** — "where do I have utilization left to
  run" — from the allocator probe core (5h/7d utilization + balance,
  `allocator.rs:224,226,228`; account-global via `QuotaConfidence::RuntimeRateLimit`
  `:941`), refreshed on selector-open, daemon-free (§7).
- **Normalized cost** — `cost_usd` is tracked per task (`Usage`/`cost_usd`
  carried on the task; harness synthesizes it where the provider doesn't bill,
  per anthropic-harness.md). Aggregate per provider in the title bar.

Note: `is_streaming_json` (`providers.rs:178`) covers Claude/GLM/DeepSeek/
Brodex/Codex/Copilot/Inception (not Gemini). Streaming output ≠ control-protocol
support. **Codex is deferred** (§2.1).

## 5. UX surface

```
┌ fleet · N active · M waiting · spend $X ───────────────────────────────┐ 1
├ roster (selectable, grouped) ──────────┬ detail ─────────────────────────┤
│   Alerting                              │  header: id · provider ·        │
│     ! glm   refactor parser  [loop] 6m │   model · cwd · cost · turns    │
│   Waiting                               │                                 │
│     ? ds    migrate schema   needs… 1m │  ─ live transcript (stream) ─   │
│   Idle                                  │                                 │
│     ○ cl    write spec              9m │  composer steers this session   │
│   Active                                │                                 │
│     ✽ bdx   port module             3m │                                 │
│   Interrupted                           │                                 │
├ composer (dispatch / steer) ───────────┴────────────────────────────────┤ 3
├ help / status ───────────────────────────────────────────────────────────┤ 1
```

- **Roster** — selectable, grouped, collapsible list. **Net new**: neither
  existing TUI uses a selectable list widget — no `ListState`/`TableState`/
  `.select(` in `cli.rs` or `council_tui.rs`; both store a `Vec` and track an
  index manually.
- **Detail** — live transcript from the agent's stream-json output; reuses
  `tui_markdown::from_str` (`cli.rs:2087`), `line_into_owned` (`cli.rs:2175`),
  `stitch_ordered_list_markers` (`cli.rs:2233`).
- **Composer** — dual-mode: with a roster agent selected, typing **steers** it
  (user-turn message into its live session); with none selected it **dispatches**
  a new entrypoint agent. **Enter stays on the roster** — dispatch does not jump
  into the single-agent view; an entrypoint agent needs minutes of grounding
  first, so you watch it surface in its state bucket.

#### State model & buckets

State is derived from the cockpit's **own reading of each agent's stream-json**
(daemon-RPC-free; reusing `parse_event`-style logic in-process is fine).
`TaskStatus` (`mod.rs:55-59`) only marks process exit; the live states sit on top:

- **Active** — turn in flight (events streaming, no `result`/`end_turn`).
- **Idle** — alive, turn finished, nothing pending; steer by typing, no respawn.
- **Waiting** — the builtin `report` tool (§2.2) flagged needs-input.
- **Alerting** — Active + the cockpit's own loop/stall/burn detection
  (supervision *logic* reused in-process, `supervision.rs`).
- **Interrupted** — process not live but session resumable: cockpit-restart orphan
  (`TaskStore::load` flip, `mod.rs:399-408`), a `Ctrl+X`-stop, or a crash (exit
  reason / stderr tail shown on the row). **Steering it auto-resumes** (`--resume`
  with the new input); or delete. (`/resume` reloads a *deleted* session; steering
  covers on-roster Interrupted ones.)

**Buckets** (group-by-state, top → bottom by attention demand): **Alerting ·
Waiting · Idle · Active · Interrupted**. No "Done" — an entrypoint agent never
self-completes; it rests at Idle until I act. Empty buckets hidden; headers carry
counts + collapse.

**Cleanup is manual** — `Ctrl+X` stops the agent (→ Interrupted), `Ctrl+X` again
deletes it from screen (Claude-agents idiom). Nothing auto-vanishes. A deleted
agent's session persists on disk; **`/resume`** (roster-only) reloads one — it
**requires a user input** to continue (not a blind restart) and drops back into
its bucket.

**Grouping**: state (default) · project · provider. **Sort within group**:
last-activity desc.

**Visual** — leading state glyph (color/animation) + a dim provider tag, per
`council_tui`'s colored-glyph idiom (`● ◌ ✗ ·`):

| State | glyph | color |
|---|---|---|
| Active | `✽` (spinner) | cyan |
| Idle | `○` | gray |
| Waiting | `?` | yellow |
| Alerting | `!` + `[loop\|stall\|burn]` | red |
| Interrupted | `↻` (red if crash) | amber |

Row: `‹glyph› ‹provider tag› name  summary  age` — **name** = first N chars of the
initial user turn (no LLM summarization), renamable via `Ctrl+R` (roster) or
`/rename` (single-agent, §5.4); summary = builtin `report` message if present,
else truncated last assistant line; age = last activity. A **queued-steer** badge
marks an agent with a steer waiting for its turn boundary (§5.4).

- **Attention** — Alerting + Waiting float to top; title counters (active /
  waiting / spend); transitions update live.

### 5.1 Navigation model

Empty-composer gate (the Claude-agents idiom): when the composer is empty the
arrows navigate; once you edit text they move the cursor. The vertical axis has
three carveouts where it rebinds without leaving the gate: **history mode** (§5.3),
and a **slash carveout** — when the composer's first char is `/`, `↑/↓` cycle
slash-command completions. **Left/right is a zoom axis; up/down selects within the
current zone:**

```
        ◀ left                            right ▶
 [provider selector]  ⟷  [ ROSTER ]  ⟷  [single-agent view]
  ↑/↓ cycle providers     ↑/↓ cycle agents     ← back to roster
  → confirm → roster       → enter agent        ↑/↓ recall input history
```

- Roster is home: `←` → provider selector, `→` → selected agent (fullscreen),
  `↑/↓` cycle agents.
- Provider selector: `↑/↓` cycle providers; `→` confirms (sticky-next, §4) and
  returns to roster. `tab` is the always-available cycle that works even with a
  non-empty composer.
- Single-agent view: `←` back to roster; `↑/↓` recall input history (§5.3).
- The vertical axis always belongs to the current zone's primary navigable thing
  (agents / providers / input history).

### 5.2 Dispatch, cwd & MCP config

`@<keyword> <prompt>` sets the new agent's cwd from a **TUI-local JSON map**
(`keyword → absolute dir`), with `@` typeahead over the map keys. No `@` → the
cockpit's launch cwd. No stickiness; resolved fresh per dispatch. This is the
cockpit's own light config — deliberately **not** the bbox project registry,
which would drag in the daemon plus per-project indexing/embedding
(`tools/projects.rs:151,167,198`), inappropriate for high-volume dispatch.

The same JSON config also holds **MCP server definitions**, injected into each
dispatched agent via `--mcp-config` (`exec_args.rs:243`, `claude_mcp_config_json`)
so agents have their MCP tools — e.g. the blackbox surface, which is *agent tool
access* and orthogonal to the cockpit's daemon-independence (it may even point at
the running daemon's MCP). **No MCP view/management TUI in v1** — registration is
config-only; a management surface is later.

### 5.3 Input history

Per-agent history of the user's inputs — the first-turn dispatch prompt plus
every subsequent steer. Recallable only in the single-agent view (`↑/↓`,
readline-style: recall populates the composer and keeps cycling; editing a
character drops into normal edit; clearing returns to the empty gate). No recall
in roster view, where `↑/↓` is agent navigation.

### 5.4 Single-agent view (transcript)

Reached with `→` from the roster. A **header/status line** leads: the agent
**name** (first N chars of the initial user turn, or a rename) · provider ·
model · cwd · cost · turns · state. `/rename` here sets the name (TUI-local
command — distinct from `/compact`, which passes through to the agent, §1.1).

A steer sent while the agent is **Active** queues until the turn boundary (shown
as a queued indicator). `Esc` interrupts the running turn (§1.1), like the
provider CLIs; **if a steer is queued when you hit `Esc`, the interrupt dequeues
and sends it immediately** — interrupt-and-redirect.

Below it: the transcript — **verbose, fully inline, linear** — the whole session
in temporal order (assistant text, tool calls *with* args, tool results *with*
responses, thinking, and my steers), no folding, no per-item selection.
Matches the verbose-CLI workflow where payloads and responses matter. Structure
is carried by markers + color, not collapse:

- **my steer** — `▌ you ›` (accent), so causal/temporal ordering is exact
- **assistant text** — markdown (`cli.rs:2087/2175/2233`)
- **thinking** — dim italic, `✻`
- **tool call** — `⏺ tool` header + args as a monospace block (raw, not markdown)
- **tool result** — indented monospace block; `is_error` red
- **report** (§2.2) — highlighted `◆`; **`compact_boundary`** — divider; **turn
  footer** — dim cost/usage

Render model reuses `transcripts/types.rs` kinds (message/tool_use/tool_result/
thinking, `:266,275`) fed by a **net-new live-stream parser** (`parse_event` only
extracts text + `result` today, `events.rs:76,151`). Scroll: pinned-to-bottom +
anchor-preserve when scrolled up — reuse `council_tui`'s `scroll_from_bottom`
logic. Oversized payloads are capped upstream by the harness (§2.3, head + rider);
the TUI keeps a render-side soft-cap only as a backstop for non-harness providers.

### 5.5 Standalone single-agent view (v2, deferred)

Deferred to v2; extracted to
[`backlog-standalone-view.md`](./backlog-standalone-view.md). In brief: the
single-agent view (§5.4) is a reusable component (transcript + composer + header)
that fleet embeds behind the roster; a standalone shell launching the harness
directly into it (no roster / no fleet chrome) is the only context where
`/clear` and a dedicated-view `/resume` are meaningful. The v1 win is multi-agent
management; the standalone shell reuses the same component with no new model.

## 6. Relationship to other surfaces

- **Nested bros** (what entrypoint agents spawn): orthogonal — `bro_dashboard`
  The cockpit may offer "drill into this agent's children" as a hand-off.
- **`bro council`** (`cli.rs:90`, `council_tui.rs`): orthogonal chat board.

## 7. What needs to be added (net new)

> Retained as the **as-built ledger**. The ✅ items are shipped. The residual
> ◑/○ items (9 input-history persist, 10 allocator probe-core,
> 15 capability badges + headroom v2, 16 Alerting reuse + `/resume`-deleted) are
> tracked as actionable work in
> [`backlog-follow-ons.md`](./backlog-follow-ons.md).

Substrate:

1. **bro-harness bidirectional stream-json** — persistent session +
   `control_request`/`control_response` + `interrupt` + `/compact`/
   `compact_boundary`. Today one-shot (`agent_loop.rs:34,154`). **Keystone (§2).**
   ✅ **Implemented** — `agent_loop.rs` `Session` / `session_loop` (one-shot is the
   degenerate case), `--input-format stream-json` reads NDJSON user turns +
   control_requests over stdin; interrupt cancels at await points with buffer
   reconciliation; steers queue to the turn boundary; `/compact` slash command;
   `emit.rs` control_response / replay_user / init `slash_commands`. SSE streaming
   added to all three transports (sink seam in `transport/mod.rs`). Mock-transport
   integration tests cover the loop.
2. **Drive Claude in `--input-format stream-json`** — blackbox dispatches it
   one-shot today (`exec_args.rs:218`); use the CLI's existing bidirectional mode.
   ✅ **Implemented** — `FleetOrchestrator::launch_interactive` appends
   `--input-format stream-json --replay-user-messages` for all bidi providers
   (Claude/GLM/DeepSeek/Brodex); `build_exec_args` deliberately omits the flag
   (guarded by a unit test) so it isn't doubled.
3. **Builtin `report` tool, fleet-pinned** (§2.2) — Waiting/summary signal on the
   stream; `registry.rs` builtin; not `bro_report`. ✅ **Implemented** —
   `report.rs` (`ReportTool` holds its own `Emitter`, emits a `report` line);
   registered always, pinned in fleet mode via `PinPolicy::also_pin`.
4. **Compaction machinery in bro-harness** (§2.4) — summarize older turns →
   replace prefix → `compact_boundary`, so the harness can react to `/compact`. It
   has none today (only discard-truncation). Net-new; Claude gets this for free.
   ✅ **Implemented** — `compaction.rs` ((provider,model)-keyed thresholds via the
   model id), `Transport::compact` per transport (pairing-safe prefix swap),
   auto-trigger on window occupancy + manual `/compact`.
5. **Bounded tool results (cap + spill + dump machinery)** (§2.3) — uniform
   harness-loop rule: result > N kB → spill full payload to a dump path, inline a
   head + rider. Net-new; general harness win. ✅ **Implemented** — `bound.rs`
   (`BRO_HARNESS_TOOL_RESULT_CAP_KB`, default 16; dumps under
   `$BRO_HOME/harness-dumps`), applied to every tool result in the dispatch loop.
6. **In-process spawn keeps child stdin open/writable** (`mod.rs:1304` closes it
   after the prompt). ✅ **Implemented** — `spawn_task_interactive` /
   `SpawnedTask` (an `interactive` flag on `SpawnTaskParams`); the writable
   `ChildStdin` is returned to the caller instead of write-once-and-dropped.
   One-shot dispatch is unchanged.
7. **`FleetOrchestrator` façade** in the lib owning `TaskStore`/tail/`store_dir`.
   ✅ **Implemented** — `orchestration/fleet.rs`, exposed as `blackbox::fleet`;
   opaque `AgentHandle` + `TaskSnapshot` keep `Task`/`TaskInner` out of the
   public API. Owns a **dedicated** `bro_home/fleet` store, loads/persists it
   (session reload), and provides `dispatch` / `resume` / `stop` / `forget`.
8. **TUI-local JSON config** (§5.2) — `@project` map (`keyword → absolute dir`) +
   typeahead, **and MCP server defs** injected via `--mcp-config`
   (`exec_args.rs:243`). Daemon-free; not the bbox project registry; no MCP mgmt
   UI in v1. ✅ **Implemented** — `fleet.json.mcpServers` is injected via
   `Provider::build_fleet_mcp_args`; `fleet.json.projects` drives
   `@<keyword> <prompt>` roster dispatch, validates the target cwd, creates the
   normal isolated fleet worktree from that project, and offers roster composer
   completion/callouts.
9. **Per-agent input-history store** (§5.3) — in-memory; optional persist to the
   cockpit's `store_dir`. ◑ **Partial** — in-memory recall implemented
   (single-agent ↑/↓, down-to-clear); on-disk persistence not yet.
10. **Extract the allocator probe core to a shared crate** (`ProbeStore`/
    `ProbeRecord` + `quota_capacity`, `allocator.rs:360,939,1286,1293`) for
    provider-selector v2. Cockpit links it, writes its **own** probe store from its
    own dispatch rate-limit telemetry + on-demand probe — daemon-free.
    ○ **Follow-on** — not built; v1 selector text-cycles the provider list.

Client (TUI):

11. **Selectable grouped/collapsible roster list** component (none exists; §5).
    ✅ **Implemented** — a ratatui `Table` (fixed-width columns + header):
    glyph · provider · agent · model · cost · turns · started · last, grouped
    into state buckets with blank-row separators; `TableState` selection.
12. **Roster + detail layout**; detail = live stream-json transcript.
    ✅ **Implemented** — roster is full-width/focus; the live transcript lives in
    the single-agent view (`→`), not beside the roster (per UX feedback).
13. **Navigation model + dual-mode composer** — zoom axis, empty-gate, history
    mode, slash carveout (§5.1, §5.3). ✅ **Implemented** — zoom axis
    (provider ⟷ roster ⟷ single-agent), empty-composer gate, history mode, and
    a slash-command autocomplete menu (`/compact`, `/rename`; ↑/↓ + Tab).
14. **Single-agent verbose transcript** (§5.4) — inline render of text/tool/
    result/thinking/steer; net-new live-stream→`transcripts/types.rs` parser.
    ✅ **Implemented** — `parse_transcript` (fleet-owned `TranscriptItem`, not
    the stored-transcript schema); markers + color; tool-result verbosity is
    tool-aware (Edit/MCP show bodies; Bash/Read suppressed; errors always shown);
    turn rules between user→assistant.
15. **Provider-first-class presentation** (glyph/filter/group, capability badges,
    cost; v1 selector text-cycle, v2 headroom §4). ◑ **Partial** — glyph / tag /
    grouping / per-provider cost done; v1 text-cycle selector with a flashing
    `next:` indicator. Capability badges + v2 headroom routing (§7.10) not built.
16. **Fleet-state taxonomy + attention surface** (Alerting/Waiting/Idle/Active/
    Interrupted; manual `Ctrl+X`+`Ctrl+X` cleanup; `/resume` deleted sessions).
    ◑ **Partial** — Waiting/Idle/Active/Interrupted derived from the stream
    (turn-in-flight + `report` needs-input); `Ctrl+X` stop→delete implemented;
    on-roster Interrupted sessions resume on steer. **Alerting**-bucket
    supervision reuse and `/resume` of a *deleted* session are not built.

Beyond the original list (implemented): **session persistence + reload** across
cockpit restarts (dedicated `bro_home/fleet` store; crashed sessions return as
recoverable/Interrupted), and **resume-on-steer** (`--resume <session_id>` swaps
a live handle back in, dropping the stale task).

Reused as-is (verified present): `spawn_task`/`TaskStore` (`mod.rs:1171,206`),
stream-json parse path (`events.rs:62`), markdown helpers (`cli.rs:2087,2175,2233`),
the SSE/scroll/compose machinery in `cli.rs` + `council_tui.rs`.

## 8. Decisions

1. **Command: `bro fleet`** — consistent with this doc's language (title, the
   `fleet · N active` counter, filename); avoids colliding with the agents catalog
   (`bro_agent_*`) that `bro agents` would, and with teams that `bro crew` would.
2. **`FleetOrchestrator` lives in the `blackbox` lib** — the `bro` binary already
   links it (`council_tui.rs:261`) and `spawn_task`/`TaskStore`/supervision/
   `parse_event` are there, so the façade reuses them in place with zero
   extraction (matches the reuse-in-process invariant, §3). A `bro-orchestrate-core`
   extraction is a *later* option only if weight/coupling warrants — not a v1 need.

Impl-time config knobs (not design forks): cap/spill `N kB` (§2.3), name length
"first N chars" (§5), render-side soft-cap (§5.4).

## 9. Non-goals

- Not a surface for nested/sub-bros (orthogonal; §6).
- Not a council/chat surface or a workflow DSL.
- Not an HTTP client of blackboxd for execution (in-process; §3).
- Not per-provider control adapters — one convergent protocol (§1, §2).
