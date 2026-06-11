---
title: Badgey — implementation skeleton
kind: design
lifecycle: archived
corpus: blackbox-design
topic:
  - corpus
tags:
  - integrations
---

# Badgey — implementation skeleton

Companion to `design/corpus/badgey.md`. Each phase below names a discrete
implementation chunk: scope, components, gates (what proves it's done),
known follow-ups, and the design-doc sections it realizes. Phases are
dependency-ordered. No timelines — landing one phase unblocks
dependents, landing all phases realizes the design.

Phases tagged `[marker]` sit at known dependency positions but await
detail fleshing. Deliberation rounds will pick which markers to flesh
out next.

This skeleton assumes `design/corpus/agentic-corpus/agentic-corpus-impl.md` Phase F4 (artifact
catalog) has landed; badgey's brofiles + workflows + packets all install
through the existing `bbox_artifact_install`.

This skeleton ALSO assumes the **agent-system infra** (see
`design/agent-system-impl.md`) has landed in two slices:

- **A0-core** (agent dispatch core) — `ArtifactKind::Agent` variant,
  `agent_manifest` embedding bucket, `agent` entity type +
  EntityRef variant, foundation types, install pipeline, MCP
  surface (`bro_agent_*`), dispatch adapter mechanism. Required
  for B1b (badgey manifest install) and M1 (badgey adapter
  registration + sugar surface).
- **A0-distill** (distillation-only) — Rust-internal
  `embed_iterate_internal` primitive (agent-system-impl phase
  AS-D4). Required ONLY for badgey's distillation pipeline (the
  fifth proposal kind `Agent` mining loop). NOT required for
  badgey core.

The split mirrors agent-system-impl's own phase ordering, where
AS-D4 sits off the agent-system critical path.

---

## Implementation status — 2026-05-06

Badgey core is implemented on `agent-system-duplex` through the wrapper,
registry, proposal, action-journal, lifecycle-tool, agent-adapter, and
IaC/eval scaffold layers. The branch passed full `cargo test`
(`945 passed`) after rebasing on `origin/main`, and Claude review of the
Badgey diff returned `NO BLOCKING FINDINGS`.

Completed / materially satisfied:

- **A0-core dependency:** agent-system substrate is present: agent
  artifact kind, manifest validation/install, embedding bucket, agent
  entity projection, MCP search/list/get/dispatch surface, and adapter
  registry.
- **D1:** Rust-level spawn helper accepts caller-provided task ids and
  rejects duplicates; Badgey privileged dispatch uses it for sub-bros
  and redispatch.
- **B1a / P1 / P2 / I1:** Badgey and Badgey-scout brofile artifacts
  exist under `examples/badgey/brofiles/` with bro-recursion filters and
  functional persona lenses.
- **B1b:** Badgey and Badgey-scout agent manifests exist under
  `examples/badgey/agents/`; Badgey routes through the dedicated wrapper
  adapter.
- **B2 / B3 / B4:** Badgey ids/scopes/proposals/action journal types and
  durable stores are implemented with transition checks, file locks, and
  non-terminal listing for recovery.
- **R1:** Thread-of-record event schemas are implemented and serialized
  through structured notes (`exec`, `turn`, `path_cached`,
  `scout_dispatched`, `subbro_spawned`, proposal events, disputes,
  dismiss).
- **W1 / W2 / W3 / W4 / W5 / W6:** Wrapper registry, exec/resume/dismiss,
  strict command parser, per-instance resume queue, turn post-processor,
  proposal apply/reject/retry, and privileged bg-action handling are
  wired. Badgey exec waits for an observed provider session id and never
  persists `provider_session_id="pending"` as a live instance.
- **W7 partial:** Budget is durable and visible as a numeric advisory
  counter (`budget_extended` events, scope-bind/status exposure). Hard
  token enforcement is not implemented.
- **M1:** `badgey_exec`, `badgey_resume`, `badgey_ask`,
  `badgey_dismiss`, `badgey_status`, `badgey_list`, and the
  `bro_agent_dispatch(agent="badgey")` adapter are wired.
- **M2 partial:** `badgey_scout` creates durable scout ids/events and the
  wrapper dispatches emitted `bg-action-spawn-subbro` actions. `collect`
  waits for every spawned task id to have a done note unless an explicit
  aggregate `scout_done` / `subbro_done` event exists.
- **M3 / M4 partial:** `badgey_triage_inbox` and `badgey_close_loops`
  exist, emit structured proposal/classification shapes, and ship cron
  artifacts. They are useful first cuts, not full autonomous arcs.
- **P3:** Scope-bind includes badgey id, thread, project, current time,
  queue status, recent cached paths, recent proposals, and budget.
- **R2 / H2 partial:** Startup replay restores live registry state from
  thread notes, skips unobserved `pending` sessions with surprise notes,
  and reconciles non-terminal journal/proposal entries conservatively.
- **C1 / C2 / C3 / C4 base:** Answer/teach/narrated/proposal behavior is
  represented through persona + wrapper mechanics; proposal drafts route
  through the gated ProposalStore and apply path. This is capability
  plumbing, not a quality guarantee.
- **E1 / E2 / I2 partial:** Badgey eval checker, nine query manifests,
  cron/workflow/packet examples, and parse/shape tests are present.
- **H3 partial:** `badgey_status` exposes queue/proposal/observability
  counters and learning-loop eligibility.

Known remaining work:

- **A0-distill / C5:** The bounded learning loop is not a full
  threshold-driven lens/propose-agent system. Status exposes eligibility;
  Badgey must still draft and user-gate any lens/brofile proposal.
- **W7:** Token-budget accounting and hard enforcement remain future
  work. The current implementation records extensions and exposes an
  advisory budget.
- **M2:** There is no long-running independent scout monitor/aggregator
  loop beyond note/event collection and wrapper-dispatched sub-bros.
- **M3 / M4 / E3 / E4:** The cron/workflow examples are installable
  scaffolds, not full nightly eval/baseline/regression engines.
- **E1:** The eval suite has 3 manifests per mode, not the roughly 20 per
  mode described below.
- **H3:** Observability is local to `badgey_status`; there are no
  per-scope dashboards, spend rollups, or trend histories yet.

Interpretation: the branch is positioned to use Badgey as a real
wrapper-managed consultant and to build the remaining quality/automation
loops incrementally. The original phase text below remains the target
spec; the bullets above describe what is actually landed now.

---

## Substrate dependencies

### Phase A0-core — Agent dispatch infrastructure (upstream)

**Scope.** Core agent-system phases that badgey core consumes:
agent-system-impl AS-D1, AS-D2, AS-D3 (substrate extensions),
AS-F1, AS-F2, AS-F3 (foundation), AS-I1, AS-I2, AS-I3 (install
pipeline), AS-T1, AS-T2, AS-T3, AS-T4 (MCP surface).

**Realizes.** `design/agent-system.md` §16.1 substrate dependencies
(except #4) + dispatch surface.

**Components.** Out of this doc's scope; see
`design/agent-system-impl.md`.

**Gates.** `bbox_artifact_install(kind="agent", source=...)` works
end-to-end; `bro_agent_search` returns ranked manifests;
`bro_agent_dispatch` returns an `AgentSession` handle; the
`AgentAdapterRegistry` accepts adapter registrations; the
`agent_manifest` embedding bucket accepts agent installs.

**Follow-ups.** Unblocks badgey B1a / B1b / W1 / M1.

---

### Phase A0-distill — Distillation primitives (upstream, async)

**Scope.** Agent-system-impl phase AS-D4: Rust-internal
`embed_iterate_internal` (and optional `cluster_neighbors_within`)
primitive on the vector store.

**Realizes.** `design/agent-system.md` §16.1 #4 + §8.2.

**Components.** Out of this doc's scope; see agent-system-impl
AS-D4.

**Gates.** Internal iteration over the `transcripts` bucket yields
vector pairs without crossing MCP; memory-safe streaming.

**Follow-ups.** Unblocks badgey distillation arc only — not
required for B1a / B1b / W1 / M1 / W6 / C4 base apply path. Land
asynchronously when badgey's `propose-agent` mining loop is being
implemented.

---

### Phase D1 — Internal pre-mint-task-id spawn helper

**Scope.** Surface a Rust-level bro-spawn helper that accepts a
caller-provided `task_id` and rejects duplicates. The existing
`orch::spawn_task(task_id, ...)` already accepts a caller-supplied
id; this phase is mainly extracting a clean helper signature + adding
duplicate-id rejection at `TaskStore::insert` (which currently
overwrites). Required by badgey's exactly-once recovery contract
(`design/corpus/badgey.md` §2.2 dispatching-state recovery).

**Realizes.** `design/corpus/badgey.md` §15 OQ #1.

**Components.**
- `pub fn spawn_with_pre_minted_id(task_id: TaskId, params: ExecParams)
  -> Result<(), BroSpawnError>` — wrapper around existing
  `orch::spawn_task` with explicit id semantics.
- `TaskStore::insert` change: detect existing key, return
  `BroSpawnError::DuplicateTaskId` instead of overwriting silently.
- Visibility: crate-internal; not surfaced via MCP.

**Gates.**
- Existing `bro_exec` MCP path continues to work unchanged (regression).
- Helper test: `spawn_with_pre_minted_id(known_id, ...)` →
  `bro_status(known_id)` round-trip.
- Duplicate task_id returns the documented error variant; the
  pre-existing record is untouched.

**Follow-ups.** Unblocks W5 + M2 + scout dispatch.

**Risk.** Low. The existing code path supports caller-supplied ids;
this is largely a tightening of insert semantics.

---

## Foundation

### Phase B1a — Brofile artifacts + filter chain wiring

**Scope.** Author `badgey-persona.json` + `badgey-scout-persona.json`
brofile artifacts with the personas + lens text from
`design/corpus/badgey.md` §7.1, §7.2 and filter-chain rules denying
`mcp__blackbox__bro_*` for both.

**Realizes.** `design/corpus/badgey.md` §2.2, §6.3, §7.

**Components.**
- `examples/badgey/brofiles/badgey-persona.json` — main persona + lens.
- `examples/badgey/brofiles/badgey-scout-persona.json` — sub-bro persona.
- Filter rules embedded in each brofile artifact:
  ```json
  "tool_filters": {
    "disallow": ["mcp__blackbox__bro_*"]
  }
  ```
- Confirm `apply_brofile_lens` propagates the disallow patterns into
  the per-dispatch filter merge (verify badgey artifacts go through
  the same path).

**Gates.**
- `bbox_artifact_install(kind="brofile", source=examples/badgey/brofiles/badgey-persona.json)`
  succeeds and the catalog reflects it.
- A bro spawned with `brofile=badgey` cannot call any `bro_*` MCP tool
  (filter chain returns the standard denial).
- Same for `brofile=badgey-scout`.
- `apply_brofile_lens` test confirms the lens text composes onto a turn
  prompt without truncation.

**Follow-ups.** P1 + P2 ship the canonical lens text content; this
phase only needs functional placeholders so the rest of the skeleton
can boot. B1b (agent manifests) consumes these brofiles via
`brofile_ref`.

---

### Phase B1b — Agent manifests for badgey + badgey-scout

**Scope.** Install the badgey and badgey-scout entries in the agent
registry. Each is a manifest JSON referencing the corresponding
brofile via `brofile_ref`.

**Realizes.** `design/agent-system.md` §11 (badgey migration).

**Components.**
- `examples/badgey/agents/badgey.json` — manifest with
  `brofile_ref="badgey-persona"`, `cost_class=expensive`,
  `provenance.kind=hand_authored`, `dispatch_adapter="badgey"` (so
  `bro_agent_dispatch(agent="badgey")` routes through the wrapper
  per agent-system §11.4), description + when_to_use +
  anti_patterns describing the consultant role.
- `examples/badgey/agents/badgey-scout.json` — manifest with
  `brofile_ref="badgey-scout-persona"`, `cost_class=normal`,
  `dispatch_adapter=null` (sub-bro brofile; spawned by the badgey
  wrapper directly via the daemon's internal spawn helper, NOT
  through `bro_agent_dispatch`). Description marks this as an
  internal collaborator surface — generic callers should not
  dispatch it directly.

**Gates.**
- `bbox_artifact_install(kind="agent", source=...)` succeeds for both.
- Installation order: the badgey adapter (registered in W1) MUST be
  available before installing `badgey.json`; otherwise install
  rejects with `error.bad_input(code=adapter_unknown)`. Test this
  rejection.
- `bro_agent_search(query="agentic-corpus consultant")` returns
  `badgey` in the top result.
- `bro_agent_dispatch(agent="badgey", args={...})` routes through
  the badgey adapter (verify by checking the returned task's
  `bro_label` carries the agent prefix AND the underlying badgey
  wrapper has registered a `badgey_id` for the new instance).
- `merged_filters` in the dispatch result show the `bro_*` denial
  inherited from the brofile.

**Follow-ups.** M1 (`badgey_exec` direct wrapper invocation) and the
badgey dispatch adapter (also wired in M1) both call into the same
`Wrapper::exec` / `Wrapper::resume_existing` functions, so
`badgey_exec` and `bro_agent_dispatch(agent="badgey")` create or
resume the same wrapper-managed badgey instance.

---

### Phase B2 — Types: BadgeyId, BadgeyScope, ProposalKind/State, BadgeyProposal

**Scope.** Concrete Rust types under `src/orchestration/badgey/types.rs`
covering the design's data model.

**Realizes.** `design/corpus/badgey.md` §4.2, §4.3, §6.5, §8.3.

**Components.**
- `BadgeyId` — newtype over `String` with parse/render + format
  invariant (`bg-<8hex>-<8hex>`).
- `BadgeyScope { project_id, initial_brief }` — v1 has no visibility.
- `ProposalKind` enum — `Workflow | Packet | Brofile | Lens | Agent |
  RedispatchTask | ArtifactPromotion`.
- `ProposalState` enum — `Pending | Applying | Applied | Failed`.
- `ProposalEvent { at, from, to, note }`.
- `BadgeyProposal` struct (full schema from §8.3) with serde + helper
  methods (`is_terminal()`, `can_transition_to(next)`).
- `ActionId` — newtype over `Uuid` (UUIDv4) with serde.
- `ActionJournalState` enum — `Seen | Dispatching { task_id } |
  Completed { result_ref } | Failed { reason }`.

**Gates.**
- Parse / render round-trip tests for `BadgeyId`.
- ProposalState transition validator rejects invalid transitions
  (`pending → applied`, `applied → applying`, etc.).
- Serde round-trip for `BadgeyProposal` + `ActionJournalState`.

**Follow-ups.** Consumed by B3, B4, W*.

---

### Phase B3 — BadgeyProposalStore

**Scope.** On-disk store at
`$BLACKBOX_STATE_DIR/badgey/proposals/<instance_id>/<P-N>.json`
with per-proposal flock + atomic tempfile-rename + fsync writes.

**Realizes.** `design/corpus/badgey.md` §8.3, §6.3 apply mechanics.

**Components.**
- `src/orchestration/badgey/proposals.rs` with:
  - `ProposalStore::new(state_dir: PathBuf)` — opens / creates the
    store root.
  - `ProposalStore::create(instance_id, kind, draft, idempotency_key)`
    → returns `BadgeyProposal` in `Pending`.
  - `ProposalStore::get(instance_id, proposal_id)`.
  - `ProposalStore::transition(instance_id, proposal_id, from, to)` —
    flocked CAS; returns conflict error if `from` doesn't match
    current state.
  - `ProposalStore::list_by_instance(instance_id)`.
  - `ProposalStore::list_non_terminal()` — used by restart recovery.
- `flock` integration via `fs2` crate (already a workspace dep, or
  add).

**Gates.**
- Concurrent CAS test: two threads racing to transition same proposal;
  exactly one wins.
- Crash-mid-write test (kill process between tempfile and rename) —
  reopen sees prior state, no partial file.
- Per-instance directory isolation: `P-1` in instance A and instance B
  do not collide.

**Follow-ups.** W3 (apply command parser) consumes
`ProposalStore::transition`; W5 (turn post-processor) consumes
`create`.

---

### Phase B4 — Action journal store

**Scope.** On-disk journal at
`$BLACKBOX_STATE_DIR/badgey/action_journal/<action_id>.json` with
state machine `seen → dispatching → completed/failed` and
non-terminal-scan helper.

**Realizes.** `design/corpus/badgey.md` §2.2 wrapper post-processing recovery.

**Components.**
- `src/orchestration/badgey/journal.rs` with:
  - `Journal::new(state_dir)`.
  - `Journal::record_seen(action_id, action_kind, body)` — atomic
    write, idempotent on action_id (re-record returns existing entry).
  - `Journal::transition(action_id, from, to)` — same CAS shape as
    proposals.
  - `Journal::list_non_terminal()` — used by restart recovery.
  - `Journal::archive_expired(older_than)` — moves to `_archive/`.
- Same flock + tempfile-rename + fsync pattern as B3.

**Gates.**
- Idempotent `record_seen` confirmed: second call with same action_id
  returns existing entry without overwriting.
- State transitions follow the documented edges only.
- Archive sweep doesn't lose non-terminal entries even if their
  timestamps are old.

**Follow-ups.** W5 consumes for turn post-processing.

---

## Wrapper core

### Phase W1 — Wrapper module scaffolding + instance registry

**Scope.** `src/orchestration/badgey.rs` (or `src/orchestration/badgey/mod.rs`
if multi-file) with the `BadgeyRegistry` holding `badgey_id ↔ (provider,
provider_session_id, scope, thread_of_record_id)` mappings.

**Realizes.** `design/corpus/badgey.md` §2, §8.

**Components.**
- `BadgeyInstance` struct — the in-memory record per instance.
- `BadgeyRegistry` — `RwLock<HashMap<BadgeyId, BadgeyInstance>>` plus
  helpers for lookup, create, dismiss.
- Wiring into `AppState` (or wherever the daemon's shared state lives).
- `RegistryError` enum — `NotFound | AlreadyExists | Dismissed`.

**Gates.**
- Existing daemon tests pass.
- Unit test: register an instance, look it up, dismiss it, confirm
  subsequent lookup returns `Dismissed`.

**Follow-ups.** W2-W5 + R2 consume the registry.

---

### Phase W2 — exec / resume / dismiss lifecycle

**Scope.** Wrapper-side functions (Rust, not yet MCP-exposed) that
implement the badgey instance lifecycle on top of `bro_exec` /
`bro_resume`.

**Realizes.** `design/corpus/badgey.md` §2.3, §2.5, §8.

**Components.**
- `Wrapper::exec(scope, brief?)` — opens thread-of-record (`bbox_thread`
  with `kind=work_item, name=badgey:<...>`), writes the `exec` event
  note (incl. provider + provider_session_id), spawns the bro via
  `bro_exec(brofile=badgey, cwd=scope.project_root, ...)`,
  records (badgey_id, session_id) in registry, returns badgey_id +
  session_id.
- `Wrapper::resume(badgey_id, prompt)` — looks up registry, calls
  `bro_resume(session_id, prompt)`, awaits completion, runs turn
  post-processor (W5), returns enriched result.
- `Wrapper::dismiss(badgey_id)` — writes the `dismiss` event note on
  thread-of-record, drains pending scout monitors (per §2.4 dismiss
  rules), removes from registry, marks thread `resolved`.

**Gates.**
- End-to-end smoke test: exec → resume(simple-question) → dismiss; all
  three thread-of-record events present.
- Confirm `provider_session_id` lands in the `exec` event body
  (required by R2 restart replay).

**Follow-ups.** W3 + W4 + W5 wrap these. M1 surfaces them via MCP.

---

### Phase W3 — Wrapper-direct command parser

**Scope.** Prefix-strict matcher that intercepts known mechanical
commands at the wrapper layer before invoking the badgey bro.

**Realizes.** `design/corpus/badgey.md` §2.2 (wrapper-direct commands).

**Components.**
- `WrapperCommand` enum: `ApplyProposal(P-N)`, `RejectProposal(P-N)`,
  `Dismiss`, `ExpandPath(bg-path-N)`, `RetryApply(P-N)`,
  `RevertBrofileTo(version)`, `TrustSubBro(A|B)`, `BudgetExtend`.
- `parse_command(prompt: &str) -> Option<WrapperCommand>` with
  prefix-strict + regex anchors; ambiguous matches return None and
  fall through to badgey.
- `Wrapper::resume` checks the parser before dispatch; if a command
  matches, the wrapper handles it directly and returns without
  spawning the bro.

**Gates.**
- Parser unit tests: each command form's positive case + at least one
  near-miss that should fall through.
- Integration: `Wrapper::resume(id, "apply P-3")` updates proposal
  state without touching `bro_resume`.

**Follow-ups.** Apply-proposal handling depends on B3 + this; surfaced
via M1.

---

### Phase W4 — Per-instance resume queue

**Scope.** Serialize concurrent `Wrapper::resume` calls per
`badgey_id`. Read-only operations bypass.

**Realizes.** `design/corpus/badgey.md` §9.

**Components.**
- `BadgeyInstance.resume_queue: Mutex<VecDeque<PendingTurn>>` (or
  tokio `Notify` + queue). Soft cap of 3; exceeding returns
  `error.bad_input(code=queue_full)`.
- `Wrapper::resume` enqueues; awaits its turn; runs.
- `Wrapper::status` and `Wrapper::list` read state without enqueueing.
- `Wrapper::dismiss` enqueues at head with priority; queued resumes
  after dismiss return `instance_dismissed`.

**Gates.**
- Concurrent test: two `resume` calls against same instance serialize
  (second waits).
- 4th concurrent resume returns `queue_full`.
- `status` during a busy resume returns immediately with the live
  queue depth.

**Follow-ups.** Scout (M2) deliberately bypasses this for sub-bro
execution.

---

### Phase W5 — Turn post-processor (bg-action-* dispatch + journal)

(see also W6 for the proposal-apply executor that handles user
"apply P-N" commands; W5 handles badgey-emitted bg-action-* notes.)

**Scope.** After each `bro_resume` completion, scan thread-of-record
for new `bg-action-*` notes posted during the turn; for each, drive
through the action journal state machine and dispatch the privileged
action.

**Realizes.** `design/corpus/badgey.md` §2.2 mechanical model.

**Components.**
- `TurnPostProcessor::run(instance_id, turn_start_ts)`:
  1. `bbox_notes(thread_id=instance.thread_of_record, since=turn_start_ts)` —
     filter `body.event LIKE 'bg-action-%'`, parse `action_id`.
  2. for each action: validate body shape against per-event JSON
     schema; on bad shape, journal `failed(invalid_shape)` and
     emit a `bg-action-failed` follow-up note.
  3. transition journal `→ seen` (idempotent on action_id).
  4. dispatch:
     - `bg-action-spawn-subbro` → call D1 spawn helper with pre-minted
       task_id, write `dispatching.task_id`, fsync, spawn.
     - `bg-action-emit-proposal` → `ProposalStore::create` in
       `pending`, journal `→ completed(proposal_ref)`.
     - `bg-action-escalate-dispute` → write follow-up dispute prompt
       into instance state, journal `→ completed`.
     - `bg-action-extend-budget` → bump per-turn budget, journal `→ completed`.
  5. write `bg-action-completed` or `bg-action-failed` follow-up note
     per action.

**Gates.**
- Replay test: simulate a turn that emits 3 actions; processor
  dispatches all 3, journal entries land in `completed`.
- Crash recovery test: kill process after journal `dispatching` write
  but before `completed`; restart sees `dispatching`, queries
  `bro_status` on recorded task_id, recovers.
- Bad shape rejection: invalid action body produces journal
  `failed(invalid_shape)` and a single `bg-action-failed` note (no
  duplicate dispatches).

**Follow-ups.** Required by C1+ capabilities (badgey's actions only do
something once W5 lands).

---

### Phase W6 — Proposal apply executor

**Scope.** The component that owns the user-driven `apply P-N` path
end-to-end: state-machine transitions, kind-specific dispatch
(`bbox_artifact_install` / pre-mint spawn helper), audit writes
(`bbox_decide` + thread-of-record post), retry / reject semantics.

W3 (command parser) recognizes `apply P-N` and routes here; B3 is
the underlying store; this phase wires them together with the
mechanics from `design/corpus/badgey.md` §6.3 apply-proposal mechanics.

**Realizes.** `design/corpus/badgey.md` §6.3 apply mechanics, §11.6
recovery semantics.

**Components.**
- `src/orchestration/badgey/apply.rs`:
  - `ApplyExecutor::apply(instance: &BadgeyInstance, proposal_id) -> ApplyOutcome`
    — instance is resolved by the caller (W3 / M1) via the W1
    registry before invocation, so the executor never re-resolves
    badgey_id. Drives the §6.3 step list under the proposal's flock:
    1. read proposal; route by `state` (already-applied,
       already-in-progress, failed, pending).
    2. CAS `pending → applying`.
    3. dispatch by `kind`:
       - `Workflow|Packet|Brofile|Lens|Agent` →
         `bbox_artifact_install(kind, source=draft_path)`.
       - `ArtifactPromotion` → install at new scope, then
         `bbox_artifact_supersede` on prior.
       - `RedispatchTask` → call D1 helper with the proposal's
         `idempotency_key`; record dispatched task id under
         `applied_task_id` BEFORE the spawn returns control.
    4. CAS `applying → applied` AND write `bbox_decide` citing
       proposal_id AND post `proposal_applied` event on
       thread-of-record.
    5. on action failure: CAS `applying → failed`; surface error.
  - `ApplyExecutor::reject(badgey_id, proposal_id, reason)` —
    transitions `pending → failed(reason)`.
  - `ApplyExecutor::retry(badgey_id, proposal_id)` — only valid
    from `failed`; re-checks underlying state (artifact installed?
    task spawned?) before dispatching, per §11.6.
  - `ApplyOutcome` enum — `Applied { artifact_ref, task_id, decide_id }
    | AlreadyApplied { prior } | InProgress | Failed { reason } |
    NotFound`.

**Gates.**
- Apply each `kind` end-to-end; verify state transitions, audit
  trail, idempotency on re-apply.
- `RedispatchTask` apply pre-mints task_id via D1, records in
  proposal record before D1 returns.
- Crash mid-step-3 (action committed, audit not written) leaves
  proposal `applying`; R2 + retry recovery completes audit
  idempotently.
- Reject moves to `failed` cleanly; retry from `failed` re-validates.

**Follow-ups.** M1 surfaces this through the wrapper-direct command
parser (W3); C4 validation covers this end-to-end via dogfood.

---

### Phase W7 — Budget policy

**Scope.** Per-turn / per-instance / per-scope budget enforcement
plus the `bg-action-extend-budget` action handler stubbed in W5.

**Realizes.** `design/corpus/badgey.md` §13.2 cost controls.

**Components.**
- `BudgetConfig` from `~/.bro/badgey.toml`:
  - `per_turn_soft_tokens` (default 50_000)
  - `per_instance_soft_tokens` (default 500_000)
  - `per_scope_monthly_advisory_tokens` (advisory-only in v1).
- `BudgetTracker` per instance:
  - tracks tokens consumed (input + output, separately) per turn
    and accumulated per instance.
  - `approaching_per_turn_cap()` returns true when ≥80% of soft cap.
  - `approaching_per_instance_cap()` returns true when ≥80%.
- W5 integration: when scope-bind is composed (P3), the
  `budget_remaining` field reads from `BudgetTracker`.
- W5 turn post-processor: if a turn exceeded the per-turn soft cap,
  surfaces `degraded.budget_exhausted=true` in the result.
- `bg-action-extend-budget` handler: bumps current-turn cap (single
  turn only) and re-resumes the bro for one continuation.
- Per-instance: at 100% soft, dismissal warning is posted to
  thread-of-record; subsequent resumes return
  `degraded.budget_exhausted` until user extends or dismisses.
- Per-scope monthly: tracked, surfaced via `badgey_status` /
  `badgey_list` / `bro_dashboard`; no enforcement in v1.

**Gates.**
- A turn deliberately driven past `per_turn_soft_tokens` returns
  `degraded.budget_exhausted` and a partial bundle.
- `bg-action-extend-budget` from badgey raises the cap for that turn
  only; the next turn's cap is back to default.
- Per-instance soft hit posts the warning and gates further resumes.
- `~/.bro/badgey.toml` overrides take effect.

**Follow-ups.** H3 surfaces the metrics; lens rule §7.2 #9 references
this enforcement.

---

## MCP surface

### Phase M1 — Core lifecycle MCP tools

**Scope.** Expose `badgey_exec` / `badgey_resume` / `badgey_ask` /
`badgey_dismiss` / `badgey_status` / `badgey_list` via the bbox MCP
registry.

**Realizes.** `design/corpus/badgey.md` §4.1.

**Components.**
- `#[tool]`-annotated handlers in `src/main.rs` (or new
  `src/badgey_tools.rs`) wrapping the W2 / W3 / W4 functions.
- Param structs (`BadgeyExecParams`, `BadgeyResumeParams`, etc.)
  with derives.
- The badgey **dispatch adapter** (§agent-system.md §11.4) is
  registered with the daemon's `AgentAdapterRegistry` at startup
  BEFORE the artifact catalog opens for validation. Its
  `dispatch()` impl invokes the wrapper's W2 lifecycle functions
  (`Wrapper::exec` / `Wrapper::resume_existing` based on whether
  `args.badgey_id` is supplied), so both surfaces converge.
- **`badgey_exec` is direct wrapper invocation** (not sugar over
  `bro_agent_dispatch`). It calls `Wrapper::exec` directly. The
  inverse — `bro_agent_dispatch(agent="badgey")` — calls the
  badgey adapter, which calls the same `Wrapper::exec`. Both
  paths converge at the wrapper, not at `bro_agent_dispatch`. The
  wrapper owns the `badgey_id ↔ AgentSession` mapping; both
  surfaces produce the same `AgentSession` for the same instance.
- Tool descriptions per `design/corpus/badgey.md` §4.4 — load-bearing cuing.
- Compile-time `tool_docs.rs` stanza for each new tool (per the
  existing convention; missing stanzas fail the build).

**Gates.**
- `tool_docs.rs` compile-time check passes.
- MCP-roundtrip test: client calls `badgey_exec` → `badgey_resume(id,
  "ping")` → `badgey_dismiss(id)`; all return well-formed responses.
- Convergence test: dispatching via `bro_agent_dispatch(agent="badgey",
  args={...})` returns an `AgentSession` whose underlying
  provider_session_id matches what `badgey_exec` would produce for
  the same scope. Both paths exercise `Wrapper::exec` once.
- Resume convergence: `bro_agent_dispatch(agent="badgey",
  args={"badgey_id":"existing"})` resumes the existing instance
  (no new wrapper record); same as `badgey_resume(badgey_id="existing")`.
- Adapter unavailability test: temporarily de-register the badgey
  adapter, attempt `bro_agent_dispatch(agent="badgey")`; assert
  hard fail with `error.bad_input(code=adapter_unavailable)` per
  agent-system §11.4 (no fallback).
- Tool descriptions render correctly in `bbox_describe_schema`.

**Follow-ups.** M2-M5 add the remaining tools.

---

### Phase M2 — Scout tools + scout dispatcher

**Scope.** `badgey_scout` / `badgey_collect` MCP tools plus the
wrapper-owned scout dispatch loop that lives off the resume queue.

**Realizes.** `design/corpus/badgey.md` §5.3, §9.3.

**Components.**
- `badgey_scout(badgey_id, charter)` MCP tool — enqueues a "charter
  authoring" turn; the W5 post-processor sees `bg-action-spawn-subbro`
  emissions for each authored sub-charter and hands them to the scout
  dispatcher.
- `ScoutDispatcher` background task per scout:
  - polls scout thread for un-dispatched authored charters
  - calls D1 spawn helper for each (subject to budget cap)
  - polls each sub-bro's thread for `kind=done` note
  - writes aggregated results back to scout thread
- `badgey_collect(scout_id)` MCP tool — reads scout thread state,
  returns `still_walking | done`.

**Gates.**
- End-to-end: scout with 2 sub-bros completes; `collect` returns
  aggregated bundle.
- Budget enforcement: scout requesting 5 parallel sub-bros gets 3
  dispatched and 2 rejected with the documented error.
- Resume queue not blocked: during an active scout, unrelated
  `badgey_resume` calls still serve.

**Follow-ups.** Triage (M3) and closer (M4) reuse the scout pattern.

---

### Phase M3 — `badgey_triage_inbox` MCP tool + cron arc

**Scope.** Structured-output tool that runs an inbox classification +
sub-bro fanout + proposal aggregation. Plus the cron-installable arc
that runs it daily.

**Realizes.** `design/corpus/badgey.md` §6.3.

**Components.**
- `badgey_triage_inbox(scope, since)` MCP tool — kicks badgey into
  triage mode (pre-canned charter + scout fanout), returns the
  proposal sheet shape from §6.3.
- `examples/badgey/crons/badgey-triage-daily.json` — daily 06:00 local
  cron arc that calls the tool and posts a morning-brief note.

**Gates.**
- Dogfood test: triage on a project with known stale threads produces
  expected proposal kinds.
- Cron installs cleanly via `bro_cron_install`; daemon registers the
  schedule.
- Proposal sheet output passes JSON schema validation.

**Follow-ups.** C4 (producer-side proposals) consumes the same proposal
emission path.

---

### Phase M4 — `badgey_close_loops` MCP tool + cron arc

**Scope.** Completion-contract auditor; weekly cron.

**Realizes.** `design/corpus/badgey.md` §6.4.

**Components.**
- `badgey_close_loops(window=14d)` MCP tool — finds tasks with
  `completion_contract`, classifies (stalled / crashed / pivoted /
  forgot-emit-done), emits proposals; never synthesizes `kind=done`,
  always uses `kind=learned` with `does_not_replace_executor_done=true`.
- `examples/badgey/crons/badgey-close-loops-weekly.json` — Sunday
  weekly cron.

**Gates.**
- Dogfood test on a corpus with deliberately-orphaned dispatched tasks
  produces expected classifications.
- Confirm no `kind=done` notes are written by the closer (lens rule).

---

### Phase M5 — `bbox_describe_schema` consultants additions

**Scope.** Extend `bbox_describe_schema` output with a `consultants`
section listing badgey + its tools + use cases + anti-patterns.

**Realizes.** `design/corpus/badgey.md` §10.2.

**Components.**
- New `Consultant` struct serialized into the schema response.
- Hard-coded entry for badgey in the daemon.
- (later) auto-discovery from registered consultants — out of v1.

**Gates.**
- Cold Codex / Gemini provider sees the consultants section in
  `bbox_describe_schema` output.
- Section format passes the existing schema output snapshot test.

---

## Persona

### Phase P1 — Badgey persona + lens (durable artifact)

**Scope.** Concrete prose for the persona + lens content from
`design/corpus/badgey.md` §7.1, §7.2. Installed as the canonical
`brofile:badgey` artifact.

**Realizes.** `design/corpus/badgey.md` §7.

**Components.**
- `examples/badgey/brofiles/badgey.json` filled with §7.1, §7.2 text
  verbatim (or close; small edits permitted to fit the brofile schema).
- Versioning: `version: 1`, install via `bbox_artifact_install`
  supersedes the placeholder from B1.

**Gates.**
- Manual eval: a freshly-exec'd badgey, given an answer-mode question,
  follows the seed→inspect→traverse→bundle protocol and returns an
  EvidenceBundle.
- Lens rule #2 (round-trip citations) is observable: weak citations
  surface in `degraded.weak_citations[]`.

**Follow-ups.** C1-C3 capabilities exercise the persona under eval.

---

### Phase P2 — Badgey-scout persona

**Scope.** Concrete prose for the sub-bro brofile.

**Realizes.** `design/corpus/badgey.md` §6.3 sub-bro pattern.

**Components.**
- `examples/badgey/brofiles/badgey-scout.json` — one-question,
  structured-return, no-recursion.
- Charter shape spec: prompt-template the wrapper uses when invoking
  scout sub-bros.

**Gates.**
- Sub-bro spawned by the scout dispatcher returns a `kind=done` note
  with body matching the expected_return schema.
- Sub-bro filter chain blocks any `bro_*` call (regression on B1).

---

### Phase P3 — Scope-bind composer

**Scope.** Wrapper-side function that composes the ambient
`[scope]` block (`design/corpus/badgey.md` §7.3) at exec / resume time.

**Realizes.** `design/corpus/badgey.md` §7.3.

**Components.**
- `compose_scope_bind(instance: &BadgeyInstance, budget_remaining: u32)`
  → `String` injected as ambient prefix into the bro's prompt.
- Pulls `recent_proposals` from BadgeyProposalStore, `recent_paths`
  from path-cache mirror.
- Wires through `bro_exec` / `bro_resume` ambient mechanism (existing
  `apply_ambient` pathway).

**Gates.**
- A resumed badgey turn sees the scope block in its first turn input.
- Updated `recent_proposals` reflect the latest store state.

---

## State + recovery

### Phase R1 — Thread-of-record post format + writes

**Scope.** Structured note bodies for every event type
(`design/corpus/badgey.md` §8.1 table).

**Realizes.** `design/corpus/badgey.md` §8.1.

**Components.**
- `ThreadEvent` enum + serde — one variant per row in §8.1's table.
- `BadgeyInstance::post_event(event)` writes via `bbox_note` with the
  appropriate `kind` per the event.
- Wrapper writes events at the documented points (W2 exec, W2 dismiss,
  W5 dispatch outcomes, etc.).

**Gates.**
- Schema test: each event variant round-trips through the structured
  body.
- A full instance lifecycle (exec → 3 turns → 1 scout → dismiss)
  produces the expected event sequence on the thread.

---

### Phase R2 — Restart replay

**Scope.** On daemon start, scan badgey threads, restore registry
mappings, recover non-terminal proposals + journal entries.

**Realizes.** `design/corpus/badgey.md` §8.2, §11.6.

**Components.**
- `BadgeyRegistry::restore_from_durable_stores()` invoked at daemon
  startup.
- Replay logic: per §8.2 step list. Extract `provider_session_id` from
  `exec` event; cross-check active proposals against
  `ProposalStore::list_non_terminal`; cross-check active actions
  against `Journal::list_non_terminal`.
- Handle missing-fields (legacy instance pre-this-doc): auto-dismiss
  with a `surprise` event.

**Gates.**
- Cold start with 3 active instances on disk reconstructs the registry
  with correct mappings.
- Stuck `applying` proposal (manually planted) is recovered per §11.6
  (transitions to `applied` if action committed; `pending` if not).
- Stuck `dispatching` journal entry is recovered: `bro_status` on
  recorded task_id determines outcome.

---

### Phase R3 — DELETED (substrate TTL only)

Per `agent-system.md` §1.2 and badgey doc §2.4 (post-agent-system),
there is no badgey-specific idle eviction. Lifecycle is bounded by:
- substrate `TaskStore` 24h-since-last-activity load TTL
- provider session GC (provider-specific)

The wrapper does not run a sweep. Stale instances become
unreachable when the substrate evicts their task metadata; the
underlying provider session can still be resumed via stored
`AgentSession` handle until the provider GCs.

This phase is retained in the skeleton as a deletion marker so the
phase summary stays consistent with prior numbering.

---

## Capabilities

### Phase C1 — Answer mode

**Scope.** Validate that a fresh badgey, given an answer-mode question,
produces an EvidenceBundle that passes the eval suite.

**Realizes.** `design/corpus/badgey.md` §5.1.

**Components.** No new code — capability emerges from P1 lens + W*
plumbing. This phase is the validation gate.

**Gates.**
- Eval suite (E1+E2) runs all 20 answer-mode queries; pass rate ≥
  baseline (50% initially per §12.5).
- Manual review: 5 random queries' bundles read coherently and have
  zero weak citations on at least 3 of 5.

---

### Phase C2 — Teach mode

**Scope.** Validate teach-mode bundles include `steps`,
`result_in_your_corpus`, `next_time_skip_badgey_when`.

**Realizes.** `design/corpus/badgey.md` §5.2.

**Components.** No new code (lens-driven). Phase is the validation
gate.

**Gates.**
- Eval suite teach-mode queries pass structural checks.
- Manual review: at least 3 of 5 walkthroughs have substantive
  `next_time_skip_badgey_when` content (not boilerplate).

---

### Phase C3 — Narrated provenance

**Scope.** Validate narrated-blame and explain-decision queries
produce expected entity-ref chains with citations.

**Realizes.** `design/corpus/badgey.md` §6.2.

**Components.** No new code (lens-driven).

**Gates.**
- 5 dogfood queries (`why does X exist`, `explain decision Y`)
  produce bundles whose entity_refs include the expected ground-truth
  refs.

---

### Phase C4 — Producer-side proposals

**Scope.** Validate badgey emits proposal drafts that correctly route
through W5 → ProposalStore → user-tap apply.

**Realizes.** `design/corpus/badgey.md` §6.5, §6.3 apply mechanics.

**Components.** No new code (lens-driven + W5 + B3 wiring).

**Gates.**
- Dogfood: prompt badgey to "propose a packet for pattern X seen N
  times in last 30d"; resulting proposal lands in ProposalStore
  pending; `apply P-N` installs the artifact.
- Apply state machine fully exercised: pending → applying → applied;
  audit trail (`bbox_decide` + thread post) present.
- Repeat apply on same proposal returns `already_applied`.

---

### Phase C5 — Bounded learning loop

**Scope.** Self-tuning: badgey tracks accept/reject, drafts a
`propose brofile lens` artifact when threshold fires.

**Realizes.** `design/corpus/badgey.md` §6.5 bounded learning loop.

**Components.**
- `LearningLoop::evaluate(instance, accept_log)` — fires on threshold
  (10 accept/reject decisions OR 30 days).
- Emits `bg-action-emit-proposal` for the lens delta.
- Lens delta proposal targets `brofile:badgey` artifact via
  `propose-brofile-lens` kind.

**Gates.**
- Force the threshold: 10 simulated accept/reject decisions trigger a
  lens proposal.
- Approving the lens proposal (via §6.3 apply path) installs a new
  badgey brofile version; subsequent exec sees the new lens.

---

## Eval

### Phase E1 — Eval suite skeleton

**Scope.** Define ~20 queries per category (answer / teach / scout)
with gold-standard EvidenceBundles. Hand-authored on the dogfood
corpus.

**Realizes.** `design/corpus/badgey.md` §12.1, §12.5.

**Components.**
- `eval/badgey/queries/answer/*.toml` — 20 manifests.
- `eval/badgey/queries/teach/*.toml` — 20 manifests.
- `eval/badgey/queries/scout/*.toml` — 20 manifests (smaller acceptable
  initially).
- Gold bundle includes: `required_entity_refs[]`, acceptable
  narrative regex, required path edges (for multi-hop), etc.

**Gates.**
- All manifests parse.
- Hand-trace at least 5 per category to confirm gold answers exist in
  the corpus.

---

### Phase E2 — Eval check + structural-first pass criteria

**Scope.** `eval/badgey/check.rs` implementing structural-first pass
criteria (`design/corpus/badgey.md` §12.2).

**Realizes.** `design/corpus/badgey.md` §12.2.

**Components.**
- Per-query `check_pass(bundle, gold) -> (PassFail, Vec<String>)`.
- Order of checks: required refs → citation kinds → path coverage →
  narrative regex → mode-specific (teach steps, scout result-set).
- Short-circuit on first structural failure; narrative regex never
  alone-passes.

**Gates.**
- Hand-crafted bad bundles (wrong refs, weak citations, missing path
  edges) fail at the expected stage.
- Hand-crafted good bundles pass all stages.

---

### Phase E3 — Nightly badgey-eval-arc + baseline tracking

**Scope.** Workflow that runs the suite nightly, tracks baseline,
alerts on regression.

**Realizes.** `design/corpus/badgey.md` §12.3.

**Components.**
- `examples/badgey/workflows/badgey-eval-arc.json` — nightly arc.
- Per-query node: `mcp_call` → `badgey_ask` → `check_pass`.
- Aggregator node: posts pass-rate to `badgey-eval-baseline` thread.
- Regression detector: if pass-rate < baseline - 5%, emit
  `bbox_note(kind=blocked, tag=badgey-eval-regression)` linked to
  most-recent accepted lens proposals.

**Gates.**
- Manual run completes; baseline thread populated.
- Forced regression (install a deliberately-bad lens) triggers the
  regression note.

---

### Phase E4 — Teach-mode graduation eval

**Scope.** Weekly arc that validates teach-mode walkthroughs actually
graduate callers.

**Realizes.** `design/corpus/badgey.md` §12.4.

**Components.**
- `examples/badgey/workflows/badgey-graduation-eval.json` — weekly.
- For each teach query: take the walkthrough, dispatch a clean codex
  bro (no badgey access) with the walkthrough's tool steps, compare
  resulting bundle to gold answer-mode bundle.
- If badgey's teach walkthrough doesn't enable a clean bro to produce
  the same bundle: fail that query.

**Gates.**
- Manual run completes.
- A deliberately-vague teach walkthrough fails the graduation eval.

---

## IaC examples

### Phase I1 — `examples/badgey/brofiles/`

**Scope.** Final brofile artifact files (badgey + badgey-scout) with
production-ready persona + lens text.

**Realizes.** `design/corpus/badgey.md` §7, §10.3.

**Components.** Already covered in B1, P1, P2 — this phase is the
ship-ready packaging step.

**Gates.**
- `bbox_artifact_install` succeeds on both files.
- A fresh-machine install path (no prior badgey state) produces a
  working badgey instance.

---

### Phase I2 — Crons + workflows + packets

**Scope.** All shippable badgey IaC.

**Realizes.** `design/corpus/badgey.md` §10.3.

**Components.**
- `examples/badgey/crons/badgey-triage-daily.json` (M3).
- `examples/badgey/crons/badgey-close-loops-weekly.json` (M4).
- `examples/badgey/workflows/badgey-eval-arc.json` (E3).
- `examples/badgey/workflows/badgey-graduation-eval.json` (E4).
- `examples/badgey/packets/badgey-self-eval.json` — gates structural
  checks during eval-arc node execution.

**Gates.**
- Each artifact installs cleanly via `bbox_artifact_install`.
- None fire by default (opt-in).

---

### Phase I3 — Install path tested end-to-end

**Scope.** Smoke test on a fresh machine state.

**Components.** Manual / shell-script test.

**Gates.**
- From clean state: install brofiles, install crons, run one
  `badgey_ask` query; bundle returns coherently.

---

## Hardening

### Phase H1 — Failure-mode validation

**Scope.** Each failure mode in `design/corpus/badgey.md` §11 has a test
exercising the documented mitigation.

**Realizes.** `design/corpus/badgey.md` §11.

**Components.**
- §11.1 weak citations: deliberately weak gold bundle → bundle returns
  with `weak_citations[]` populated; >3 weak triggers
  `degraded.unreliable_bundle`.
- §11.2 sub-bro disagreement: dispatch 2 sub-bros with conflicting
  prompts → `degraded.dispute_pending` returned, dispute note posted.
- §11.3 lens drift: install a degrading lens → eval-arc regression
  alert fires; revert path works.
- §11.4 budget exhaustion: deliberately-low budget → partial bundle
  with `degraded.budget_exhausted`.
- §11.5 daemon restart mid-scout: kill daemon during scout → R2
  recovers; `badgey_collect` returns `scout_recovered_with_losses` if
  applicable.
- §11.6 stuck applying: manually plant stuck state → recovery
  mechanism completes the transition.

**Gates.**
- Each scenario has an integration test that passes.

---

### Phase H2 — Apply-proposal recovery hardening

**Scope.** Stress-test the §11.6 recovery path.

**Components.** Targeted tests around B3 + W5 + R2 interactions.

**Gates.**
- Concurrent apply attempts on the same proposal: exactly one
  succeeds, others see `already_in_progress` or `already_applied`.
- Crash mid-`bbox_artifact_install`: recovery either completes or
  rolls back cleanly.
- Crash after `bro_exec` but before journal `→ completed`: recovery
  reads `bro_status` and reconciles.

---

### Phase H3 — Observability

**Scope.** Metrics emission per `design/corpus/badgey.md` §13.1.

**Realizes.** `design/corpus/badgey.md` §13.

**Components.**
- Per-instance counters: turns, tool calls, sub-bros, proposals,
  weak-citations, disputes, tokens, elapsed time.
- Per-scope aggregates: instances exec'd / dismissed / restored,
  monthly token spend, accept rate, eval pass-rate trend.
- `badgey_status(id)` exposes per-instance snapshot.
- `badgey_list(scope)` exposes per-scope rollup.
- `bro_dashboard` integration.

**Gates.**
- All metrics fields present and non-zero after a non-trivial
  workload.
- Cost rollup matches actual provider billing within ±5%.

---

## Phase summary

```
Substrate dependencies (must precede everything)
  A0-core (agent dispatch: artifact_install kind=agent,
      agent_manifest bucket, agent entity type, foundation
      types, install pipeline, bro_agent_* MCP surface,
      AgentAdapterRegistry)       — see agent-system-impl.md
  A0-distill (Rust-internal embed_iterate; ASYNC; blocks
      badgey distillation only, NOT badgey core)
  D1 (internal pre-mint-task-id spawn helper)

Foundation (no order-dependency among each other except as noted)
  B1a (brofile artifacts + filter wiring) ◄── A0-core
  B1b (agent manifests for badgey + badgey-scout) ◄── A0-core, B1a
  B2 (types incl. ProposalKind::Agent) ──► B3, B4, R1, W*, M*
  B3 (proposal store)    ──► W3, W5, W6, C4, R2, H2
  B4 (action journal)    ──► W5, R2

State schema (must precede wrapper writes)
  R1 (post format)       ◄── B2

Wrapper core
  W1 (registry)          ◄── B2
  W2 (lifecycle)         ◄── W1, B1, R1
  W3 (command parser)    ◄── B3
  W4 (queue)             ◄── W1
  W5 (post-processor)    ◄── B3, B4, W2, R1, D1
  W6 (apply executor)    ◄── W1, B3, W3, R1, D1
  W7 (budget policy)     ◄── W1, W5

MCP surface
  M1 (lifecycle tools)   ◄── W2, W3, W4, W5, W6, W7
  M2 (scout)             ◄── W5, D1
  M3 (triage)            ◄── M2
  M4 (closer)            ◄── M3
  M5 (schema)            ◄── M1

Persona
  P1 (badgey)            ◄── B1a, B1b
  P2 (badgey-scout)      ◄── B1a, B1b
  P3 (scope-bind)        ◄── W2, B3, W7

Recovery
  R2 (restart replay)    ◄── B3, B4, W1, R1
  R3 — DELETED (substrate TTL only; no badgey-side eviction)

Capabilities (lens-driven; mostly validation gates)
  C1 (answer)            ◄── P1, P3, M1, E1, E2
  C2 (teach)             ◄── P1, P3, M1, E1, E2
  C3 (narrated)          ◄── P1, P3, M1
  C4 (proposals)         ◄── P1, P3, M1, B3, W5, W6
  C5 (learning loop)     ◄── C4

Eval
  E1 (suite skeleton)
  E2 (check + criteria)  ◄── E1
  E3 (nightly arc)       ◄── E2, M1
  E4 (graduation)        ◄── E3

IaC examples
  I1 (brofiles)          ◄── P1, P2
  I2 (crons + workflows) ◄── M3, M4, E3, E4
  I3 (install end-to-end)◄── I1, I2

Hardening
  H1 (failure-mode)      ◄── all capability + recovery phases
  H2 (apply recovery)    ◄── B3, W5, W6, R2
  H3 (observability)     ◄── all instrumented phases, W7
```

Critical path: D1 → B1+B2 → R1+B3+B4 → W1→W2→W4→W5→W6→W7 → M1+M2 →
P1+P2+P3 → R2 → E1+E2 → C1 → I1+I2 → H1.

C3-C5 + M3-M4 + R3 + E3-E4 + H2-H3 are parallelizable once their
direct upstream lands.
