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

## Cell bindings (`src/bindings/`) — the refactor DSL namespaces

The `code.*` / `edits.*` / `lsp.*` namespace globals projected into code-mode
cells. Design home: `design/bro-harness/refactor-v2-pressure-test.md` (read
it before extending); trust model from refactor-tools-v2 §3–4.

- **Cell-only constructs.** Bindings join the code-mode callable set + seam
  (ToolFilter still gates by canonical name, e.g. `"code.items"`) and NEVER
  the flat wire registry. They are harness-native: pure functions of the
  working set, zero daemon reach-back (decision af3c4783 — the container
  test). A binding that needs daemon state is in the wrong layer.
- **One mutation path, no confirm flags.** `edits.apply` is the only write;
  the gate is detection (stale spans, invalid edits, create collisions,
  post-write parse errors) bouncing with structured findings + rollback. A
  confirm/ack flag a cell can author is theater — operator authority arrives
  dispatch-side (RX-V1), never as a cell argument.
- **Spans are hash-anchored at read time**; an EditSet pins one content hash
  per file; after a successful apply every older Span for that file is stale
  BY DESIGN — consumers re-derive facts, and bindings check expected hashes
  BEFORE interpreting byte ranges (drift must fail as `stale_span`, not as a
  structural miss against the new tree).
- **Provenance is host-computed lineage**, never cell-supplied tags: the
  ledger records what authority produced which changes; the choke point
  recomputes the set's `semantic_status` as the weakest link. Cell-authored
  bytes floor at `syntax_only`; laundering is possible and priced, not
  forbidden.
- **Session-scoped state** (EditStore, LSP pool, ledger) is scoped by
  `binding_tools()` being called once per session — construct it twice and
  EditSets/provenance silently fork.
- **Probe-first evolution.** Binding ergonomics changed only through live
  probes + `prompts/RETRO_ISOLATE_REFACTOR.md` retros; fix at the source
  (binding > declarations > gap note in `*/refactor-tools/*`). Several
  current shapes are probe-derived and look arbitrary without that history:
  `edits.begin()` returning a bare string, lenient input normalization,
  `lsp.rename` snapping item spans to the name identifier, the namespace
  declarations rendering in every code mode.
