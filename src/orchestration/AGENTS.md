# orchestration/ — providers, brofiles, dispatch/resume, atoms, supervision

Domain home for the dispatch plane. Boundary contract:
`design/orchestration/provider-lanes.md`.

## Native provider dispatch

Only the Claude CLI and Codex app-server lanes execute. Archived provider
identities remain decodable but are refused by admission, re-adoption and both
executors. Model tools, code cells, compaction and refactoring belong to the
native provider. No daemon setting installs a custom model-turn runtime.

Resume re-resolves the original brofile from the session's recorded label.
Preserve its provider, account, model, effort, persona and tool restrictions.
An unavailable or mismatched brofile produces an explicit notice.

## Task terminal state follows the worker

- **A `result` event ends a turn, not the worker.** A harness worker takes a
  queued steer as its next turn after any result, including an error result.
  Ingest records an error result's message in stderr and leaves the task
  running. The worker's exit decides the outcome: exit code zero with a latest
  retained result that is not an error completes the task, anything else
  fails it (`latest_result_is_error`), so a worker that exits zero after an
  error result still fails.
- **A task goes terminal before its worker exits only by cancel or fork
  rejection, and both ask the worker to stop.** The kill switch stays
  registered until the terminal waiter observes the exit, so a registered
  switch on a terminal task means a live worker. Status shows
  `workerShutdown: "pending"` for that state, and `bro_cancel` on such a task
  sends the stop request again (`CancelOutcome::StopResent`) without changing
  the record. The exit of a task already published as terminal
  (`completed_at` set and not `recoverable`) keeps its record and publishes no
  second outcome.
- **Restart re-adoption stops a terminal task's live worker.** A task that is
  terminal while fleetd still reports its worker running is reattached with
  its record unchanged and its worker is asked to stop, so the exit is
  observed and the session's supervision key is freed for a resume.
- **A resume waits for a worker it already asked to stop.** A spawn whose
  supervision key is held by a worker with a pending stop request waits a
  bounded time for that worker's exit before refusing; the harness
  checkpoints on SIGTERM before exiting, so a resume issued right after a
  cancel lands in that window.

## Worker telemetry is not conversation

- A `harness_shell_sessions` envelope reports the worker's retained shell
  sessions. Supervision handles it apart from every other event: it never
  changes `tool_running`, loop hashes or compaction evidence, because the
  worker publishes it while a shell tool is still running. A report is
  accepted only when it is well formed, within the session and command-head
  bounds, and newer than the one held; anything else leaves the state as it
  was, so a bad report never reads as an empty one. The accepted sequence and
  the daemon-local receipt time are stored with it and a duplicate never
  moves them. A report loaded from a persisted record is history until this
  daemon process accepts a newer one. No stored report means unknown, not
  empty. Session ids are bounded as well.
- **A report is not activity.** Accepting one leaves the event count and
  `last_event_at_ms` alone; its receipt time lives on the stored observation
  (`received_at_ms`). The idle notice is computed from conversation events
  only, so a worker blocked in one long shell command goes idle whatever its
  reports do, which is the case the stored report exists to explain.
- **The stored report is shown where idleness is.** The full supervision
  snapshot carries it as `shell_sessions` whenever one is stored; the green
  response carries it only alongside `idle_seconds`, so a quiet worker's
  response says which commands it is still waiting on and a busy one stays
  the bare sentinel. The field states the report's own age
  (`report_age_seconds`) and whether this daemon process received it
  (`from_this_run`); row ages are as of the report.
- **Reports are change-driven, never periodic.** The worker publishes one
  when its process starts and when a shell session starts, ends or is
  removed, and at no other time. Do not add a timer-driven publisher.
- **Order reports by sequence, never by arrival.** The worker stamps an
  event with its sequence and then writes it, with no lock spanning both, so
  two emitters in one worker can put lines on stdout out of sequence order.
  A report that arrives after a newer one is stale and is ignored for that
  reason; do not replace the sequence check with arrival order.
- **The report sequence is the session's event sequence.** It only has to
  increase within one task: a task is one worker process, re-adoption after
  a daemon restart keeps that process and its counter, and a resumed session
  is a new task with fresh supervision state, so a new worker process never
  reports under an existing task's held sequence.

- **A model-request report says what the worker's provider request is
  waiting on.** `harness_model_request` is sent when a request goes out
  (`requesting`) and when a failed attempt starts its backoff (`retry_wait`,
  with the wait and a bounded reason). Like a shell report it is telemetry:
  it is never activity, it is not stored in the task's event ring, and a
  report with a lower sequence than the held one is ignored. It is shown, as
  `model_request`, only until the next conversation event, because after that
  it no longer describes what the worker is doing. It is held in memory only.
- **A steer is pending until the worker shows it to the model.** `bro_steer`
  records a digest of the steer's text before sending it; the harness logs
  the steer as an ordinary user event with that exact text when it injects
  it, which clears the entry. Status shows `pending_steers` (count and the
  oldest one's age) while any remain. Subtyped user events never clear one.

## Worker environment and binary ownership

A worker inherits the spawning process environment minus the spec's explicit
removals, then receives its scoped environment. Service credentials for daemon
MCP travel in secret environment fields, never command-line arguments.

The execution host resolves native provider binaries. Fleetd admission must not
probe the daemon container's PATH as evidence of worker-host availability.

## HTTP control dispatches use the interactive MCP surface

- `Origin::Control` and legacy `Origin::Cockpit` workers use
  `surface=interactive`, which retains the
  model-facing `bbox_render` capability required by the managed-workspace
  locality wrapper. The external-client `default` surface deliberately hides
  lifecycle tools including `bbox_render`, so routing a control worker there
  makes checkout-local render impossible even when workspace authority is
  valid. Workflow workers remain on `agent-internal`; every other origin
  dispatches on `default`.

## Persisted task compatibility and damaged snapshots

- Persisted provider/origin variants outlive their dispatch surfaces. Keep legacy
  workflow and atom records readable until an explicit data migration retires
  them. Loading a mixed snapshot must not erase otherwise readable tasks.
- Decode each array row independently. Unreadable rows and every row sharing a
  duplicate ID stay opaque in later snapshots; their IDs cannot be reused for
  executable tasks. They are not pruned by task TTL because their metadata is
  untrusted. Before permitting replacement of the snapshot, retain its exact
  bytes in a content-addressed `tasks.quarantine.<sha256>.json` beside
  `tasks.json`, with restricted permissions and durable file/directory sync.
- Whole-file parse/read failures or a failed quarantine block every normal
  snapshot path and emit an error. New task reservation/insertion refuses with
  `TaskStoreUnavailable` before executor admission. Existing readable tasks stay
  visible in memory, but their changes cannot become durable until an operator
  repairs the configured task store and restarts. Preserve the original and quarantine files during
  repair; never treat an empty in-memory store as authority to discard them.
  Quarantine backups require explicit operator cleanup after repair.
- Workflow/atom origin, workflow provider, or an explicit `workflow_owned` flag
  preserves owner-managed closeout protection. On restart, a running owned task
  becomes failed and is retained for inspection without ordinary bro recovery
  eligibility; older recoverable flags are cleared. Ordinary harness re-adoption
  refuses owned tasks before restoring workspace bindings. While an owning
  runtime remains installed, recovery belongs to that runtime's explicit path.
  Current ordinary bro tasks retain restart recovery and re-adoption behavior.

## Retired consultant runtime

Badgey and the stateful consultant runtime have no tool routes, agent adapter,
atom execution backend, registry reconstruction, or startup recovery. Legacy
consultant atom implementations and handles remain decodable for historical
records; new installation and execution refuse. Ordinary bro execution
remains available.

Existing `badgey/proposals` and `badgey/action_journal` beneath the configured
bro store are inactive archives. Startup does not open, reconcile, move, or
rewrite them. Preserve their bytes, proposal provenance, and historical
notes/threads/knowledge. Project-catalog legacy proposal inventory and stamping
remain separate durable-data ownership contracts and must not be removed with
runtime code. Explicit migration may still operate on these archived carriers.
