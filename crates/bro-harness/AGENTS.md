# bro-harness — the custom headless coding agent

Invariants for the harness slice. Deep design lives in `design/bro-harness/`;
the daemon boundary contract is `design/bro-harness/harness-daemon-boundary.md`.

## The session event log is a product surface, not a debug artifact

- `$BRO_HOME/harness-sessions/<session_id>.events.jsonl` is THE transcript:
  the fleet cockpit renders it directly, `bro agent` renders it, the
  transcript indexer ingests it, and closeout handshakes read agent replies
  out of it. Treat every emit decision as user-visible.
- The emitter tees every envelope event to the log EXCEPT `stream_event`
  partials and `isReplay` echoes. The corollary that has bitten: **every path
  that injects a user-visible turn must append its user event at the position
  the model saw it** — turn start, mid-turn steer drain, synthetic nudges.
  A consumed-but-unlogged input renders downstream as "operator input
  ignored" (and pins the cockpit's queued-steer echo forever).
- Events are complete, append-only steps — one assistant message per model
  step, tool-result batches, never a revision of an earlier line. Downstream
  renderers commit parsed events to terminal scrollback immediately on this
  guarantee. Do not emit partial-then-replace shapes.
- Steer/user-turn text travels RAW end-to-end (no ambient wrapping on
  steers). The cockpit reconciles queued echoes by exact text match.
- **Scoped AGENTS riders are transcript events, not sidecars.** When a flat
  file/edit/patch tool first touches a directory with a more-specific
  `AGENTS.md`, the harness appends that doc as a rider to the successful
  `tool_result.content` before emitter/transport delivery. The event log is
  the durable record of delivery; live sessions only keep an in-memory dedupe
  set rebuilt from startup docs plus prior rider blocks on resume. `shell_run`
  is deliberately outside this path.

## Compaction policy

- Compaction thresholds are model-family properties in `compaction.rs`:
  `default_entries()`. The default ratio is 0.75 of the context window.
  MiniMax-M* is an exception: `compact_at: 0.45` (450K threshold on a 1M
  window) per official recommendation — sparse-attention effective range
  benefits from earlier compaction.
- When a downstream consumer (cockpit, etc.) needs the current threshold,
  **emit it into the session event log** (`compaction_threshold` on the
  `result` event). Never let the consumer duplicate the lookup table — that
  table drifts immediately.
- The `threshold()` function returns `window × ratio` rounded down.
  Resolution walks exact-model → longest-glob → "default" independently for
  each field, so a glob can set the window while inheriting the ratio.

## Session + loop model

- One `user_turn` = one model conversation turn, possibly many model steps
  (tool loop). Mid-turn operator inputs queue and are injected at the next
  model-call boundary inside the same turn; leftovers become new pending
  turns. Interrupt-with-redirect cancels the step and front-queues the
  redirect.
- Sessions persist after every turn (a bidi session is routinely SIGTERMed);
  resume reconstructs from the session file. Anything that must survive a
  resume belongs in persisted session state, not loop locals.
- Typed agent mailbox bodies and their contiguous cursor persist before worker
  admission is acknowledged. Queue-only sends never wake an idle loop;
  followups wake once, and replay after restart cannot append the body twice.
- Every worker reconnect projects a fresh monotonic service policy before
  replayed input can start a provider turn. Disconnect revokes remote tool
  visibility and held proxies immediately; a later grant restores the same
  session proxy and updates typed service availability before the next turn.
- Provider tool-call identity is durable effect identity. Flat dispatch scopes
  `ToolCx` with the provider ID; nested code-mode calls derive an ID from the
  outer call plus their deterministic runtime ordinal. Capability RPC keeps
  that identity separate from its ephemeral request/response `call_id`.

## Deferred tool surface

- `tool_search` is activation, not a schema dump. Default results stay compact
  (names/descriptions + remaining count); the next turn's wire tool list is the
  schema authority. Use explicit `include_schemas=true` only for callers that
  need schema details inside the search result itself. The ambient manifest is a
  bounded preview, not the full catalog — widening it reintroduces
  gap-a05b8afd's context noise.

## Boundary invariants (compiler-enforced; don't negotiate)

- bro-harness never depends on `blackbox`. Daemon capabilities arrive only
  through `bro-capabilities` traits and fail closed when absent.
- The daemon runs harness providers IN-PROCESS (library link, event callback
  delivery). The `bro-harness` binary on PATH is only the allocator's
  availability probe — changing subprocess arg shapes does not change daemon
  dispatch, and vice versa.
- Tests must not touch real `$HOME`/`$BRO_HOME` state: `EventLog::disabled()`
  / explicit paths exist for exactly this; the sessions dir resolves from
  `BRO_HOME`, so a leaked env var writes into the operator's real session
  store.
- Daemon workers capture provider credentials into task-local transport state,
  then remove the complete spawn scrub manifest from process-global env before
  any shell or MCP child can start. Service bearer values and paths are never
  worker inputs.

## Responses transport

- The Brodex/OpenAI Responses path owns two wire modes under one transport:
  ChatGPT-OAuth uses the Codex-style Responses WebSocket first, while API-key
  auth goes straight to HTTP-SSE. WebSocket transport faults are session-
  permanent fallback to HTTP-SSE; `response.failed`/API errors are surfaced as
  API failures, not replayed over HTTP to hide the real cause.
- The transcript/input buffer is authoritative across WS -> HTTP fallback.
  WebSocket turns commit to `ResponsesState` only after a terminal event parses
  successfully; fallback full-replays from that pristine state. Do not add a
  second conversation buffer or commit partial WS state before parse success.

## Cell bindings (`src/bindings/`)

Dense leaf with its own AGENTS.md: `src/bindings/AGENTS.md` carries the
refactor cell-DSL invariants (cell-only placement, one-mutation-path trust
model, hash-anchored span discipline, host-computed lineage, probe-derived
shapes). Read it before touching the namespaces.

## Isolate CLI cell mode

`src/bin/isolate.rs` is a command-line probe surface over the same harness
tool/runtime contract, not its own JS embedding. Cell mode must go through
`code_mode_tools` + `HostTools`, so repeated `--cell` / `--cell-file` inputs in
one process share the code-mode session KV/function store and nested `tools.*`
/ namespace calls honor the same callable surface as harness consumers.
