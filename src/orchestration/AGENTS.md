# orchestration/ — providers, brofiles, dispatch/resume, atoms, supervision

Domain home for the dispatch plane. Boundary contract:
`design/bro-harness/harness-process-boundary.md`; RX-V1 trust model:
`design/refactor-tools/rust/rust-isolate-surface.md` §2.4/§8.2.

## Dispatch tool_defaults (operator-authority lane)

- **`tool_defaults` is the operator-authority delivery channel for harness
  bindings** (RX-V1 `acknowledge_*` grants, gap-ead94671). Two lanes merge
  over the ambient map at every dispatch/resume: the brofile's durable
  `tool_defaults` (persona-bound, versioned) then per-dispatch
  `ExecParams`/`ResumeParams.tool_defaults` (ad-hoc). Precedence is
  ambient < brofile < per-dispatch; most specific wins on key conflict.
- The merged map reaches the harness child verbatim via
  `--additional-context` (harness ladder: explicit map > CLI JSON >
  `BRO_HARNESS_TOOL_DEFAULTS` env). Bindings read it host-side via
  `cx.tool_arg_defaults.lookup(tool, param)`; a cell-authored
  `acknowledge_*` is a schema error by design.
- **Resume restores the brofile from the session, not from the caller.**
  `bro_resume` takes no brofile selector. The session's most recent task
  that carries a `bro_label` names the brofile, and it re-resolves in that
  task's working-directory scope. When it resolves to the same provider, the
  resume gets its account, model, effort, persona, filters and tool defaults
  again and the resumed task keeps the label. Code mode and service tier are
  not re-sent: the harness saved them with the session, and only a per-resume
  parameter overrides that. A session with an allocator
  lease keeps the lease's lane and takes persona, filters and tool defaults
  from the brofile. Any other outcome is one decision point
  (`ResumeBrofile::unrestored`): the resume runs without the brofile's
  policy and its result carries `brofile.notice`.
- **Edit discipline is chosen at dispatch and owned by the session.**
  `edit_discipline` (`free` or `structured`) resolves per-dispatch value,
  then brofile value, then nothing, and reaches the harness as
  `--edit-discipline` on a fresh dispatch only. Resume never passes it: the
  harness saved it with the session, so a later brofile edit does not flip a
  running session. The daemon refuses `structured` with a resolved
  `code_mode: off` before spawning; enforcement of the discipline itself
  lives in the harness. The harness binary must understand the flag before a
  daemon that sends it is deployed.
  A brofile's value is a default for callers that name none, not a boundary:
  a per-dispatch `free` overrides it, as per-dispatch tool filters do. The
  admin upsert route rewrites a brofile from a few fields. It carries the
  existing tool filters and `edit_discipline` over and lists them under
  `kept` in its response, because both are restrictions; the other fields it
  does not name (tool defaults, surface, context, code mode) are still reset
  by that route.
- **Every new dispatch path must thread both lanes, not just the ambient
  map.** The merge helper exists because the direct, workflow, agent, and
  atom dispatch sites each grew the call separately; a new site that passes
  only `ambient_ctx.tool_arg_defaults()` silently strips operator grants
  (that is the hole that made every RX-V1 consumer refusal-only in live
  dispatches until the channel landed).
- bro-fleet-client forwards `DispatchSpec`/`ResumeSpec.tool_defaults` as
  the per-dispatch lane on `/control/exec` and `/control/resume`, which
  deserialize the same `ExecParams`/`ResumeParams` as the MCP tools. The
  cockpit sets no per-dispatch defaults of its own, so its grants come from
  the brofile lane unless a caller fills the spec field.

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

## Worker environment inheritance

- A worker inherits the spawning process's environment minus the spec's
  `env_unset`, then gets the spec env. `env_unset` is composed in one place
  (`prepare_harness_child_launch`): the service variables plus
  `WORKER_UNINHERITED_ENV_VARS`, the harness switches that must come only
  from an explicit account or per-dispatch env
  (`BRO_HARNESS_MCP_HTTP_LIFECYCLE`). Add a harness switch there when a
  stray export in the daemon's or fleetd's environment must not change
  every worker on the host.

## Allocator binary eligibility follows the executor boundary

- Harness provider binaries are resolved on the host that actually spawns the
  worker. `LocalExecutor` may use daemon-host PATH availability as allocator
  eligibility; `FleetdExecutor` must not. Fleetd performs final login-shell
  resolution against its own worker-host PATH, so a containerized daemon that
  lacks `bro-harness` locally must still admit otherwise-eligible fleetd
  lanes. The pseudo-provider `workflow` remains non-dispatchable in either
  mode.

## Cockpit dispatches use the interactive MCP surface

- `Origin::Cockpit` workers use `surface=interactive`, which retains the
  model-facing `bbox_render` capability required by the managed-workspace
  locality wrapper. The external-client `default` surface deliberately hides
  lifecycle tools including `bbox_render`, so routing a cockpit worker there
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
