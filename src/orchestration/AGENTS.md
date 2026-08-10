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
