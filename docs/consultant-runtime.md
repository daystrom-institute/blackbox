# Consultant Runtime

The consultant runtime is the generic substrate for **stateful consultants**:
long-lived agent instances that carry identity, a resumable provider session,
a proposal lifecycle, and an action journal across turns. It was extracted
from Badgey (see `docs/badgey.md`), which is now its first configured
consumer rather than a bespoke daemon feature. Design record:
`design/orchestration/agents/consultant-runtime.md` (gap-9dae9a60).

## What the runtime owns

`src/orchestration/consultant/` holds the consumer-agnostic core:

- **Instance identity & registry** — `ConsultantId`
  (`<prefix>-<8hex>-<8hex>`), `ConsultantInstance`, `ConsultantRegistry`
  with per-instance resume queues that serialize turns.
- **Proposal store** — `Pending → Applying → Applied | Failed` state machine
  with `(kind, draft)` idempotency keys and CAS file locking. Proposal kinds
  are vocabulary-agnostic strings validated against the consumer.
- **Action journal** — first-write-idempotent intent journal
  (`Seen → Dispatching → Completed | Failed`), with archival.
- **Thread events** — the `ThreadEvent` vocabulary written to notes
  (Exec/Turn/Proposal lifecycle/Dismiss…). Notes are load-bearing: the
  registry is restored from them at daemon startup.
- **Turn loop** — descriptor-parameterized exec/resume
  (`src/tools/consultant/lifecycle.rs`): scope-bind block, dispatch,
  turn-event writes, and consumer hook dispatch.

## Consumer descriptors

A `ConsumerDescriptor` is a **code-owned constant** binding a consumer to the
runtime: name, id prefix, intent-note grammar prefix, persona/scout brofiles,
action and proposal-kind vocabularies, exec prompt, turn budget, hook set,
and state-dir layout. Descriptors are never loaded from data — the intent
post-processor is the recursion-guard security boundary, so consumers select
compiled-in vocabulary and hooks (`ConsumerHooks`), they cannot define new
ones. The registry is `orchestration::consultant::consumers` (currently:
`badgey`).

State layout: Badgey keeps its legacy `state_dir/badgey/` paths permanently;
any future consumer gets `state_dir/consultant/<name>/`.

## Surfaces

- **MCP tools** — `consultant_proposals_list`, `consultant_apply_proposal`,
  `consultant_proposal_begin_apply`, `consultant_proposal_complete_apply`
  take `consumer` + `consultant_id` and are the consumer-agnostic
  workflow-facing proposal surface. The `badgey_*` proposal tools are
  pinned shims for `consumer="badgey"` with their original wire format.
- **Atom backend** — `implementation: { kind: "consultant", consumer: ... }`
  runs **one turn per invocation**: without `consultant_id` it opens a new
  instance (brief/prompt as the initial brief); with `consultant_id` +
  `prompt` it resumes that instance for one turn. The instance outlives the
  invocation — `atom_status` reports the turn, not the consultation.
  Shipped example: `system-defaults/atoms/consultant/badgey-consult.json`.
- **Conversational lifecycle** — instance open/turn/dismiss remain on the
  consumer-prefixed tools (`badgey_exec` / `badgey_resume` / `badgey_ask` /
  `badgey_dismiss`), which delegate into the generic runtime with the
  Badgey descriptor.

## Adding a consumer

1. Define the descriptor constant (vocabulary module, like
   `orchestration::badgey::vocabulary::BADGEY`) and register it in
   `orchestration::consultant::consumers`.
2. Ship the persona/scout brofiles the descriptor references.
3. Select hooks: `ConsumerHooks::None` gives plain turns; a new hook set
   (wrapper-command grammar + intent post-processor) is a code change by
   design.
4. Atom/workflow access comes for free: a `consultant`-backed atom manifest
   naming the consumer, and the `consultant_*` proposal tools.
