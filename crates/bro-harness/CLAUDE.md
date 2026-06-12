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

## Session + loop model

- One `user_turn` = one model conversation turn, possibly many model steps
  (tool loop). Mid-turn operator inputs queue and are injected at the next
  model-call boundary inside the same turn; leftovers become new pending
  turns. Interrupt-with-redirect cancels the step and front-queues the
  redirect.
- Sessions persist after every turn (a bidi session is routinely SIGTERMed);
  resume reconstructs from the session file. Anything that must survive a
  resume belongs in persisted session state, not loop locals.

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

## Cell bindings (`src/bindings/`)

Dense leaf with its own CLAUDE.md: `src/bindings/CLAUDE.md` carries the
refactor cell-DSL invariants (cell-only placement, one-mutation-path trust
model, hash-anchored span discipline, host-computed lineage, probe-derived
shapes). Read it before touching the namespaces.
