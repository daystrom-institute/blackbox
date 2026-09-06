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
- **Every new dispatch path must thread both lanes, not just the ambient
  map.** The merge helper exists because the direct, workflow, agent, and
  atom dispatch sites each grew the call separately; a new site that passes
  only `ambient_ctx.tool_arg_defaults()` silently strips operator grants
  (that is the hole that made every RX-V1 consumer refusal-only in live
  dispatches until the channel landed).
- bro-fleet-client does not yet forward per-dispatch defaults
  (`dispatch_body`); fleet-side grants need the brofile lane. Deferred
  while fleetd extraction is in flight.

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
  valid. Workflow workers remain on `agent-internal`; recursive agent and atom
  dispatches remain on `default`.

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
