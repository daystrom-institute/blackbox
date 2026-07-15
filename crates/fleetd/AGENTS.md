# fleetd

`fleetd` hosts the live execution authority from `fleet-core`. It owns HTTP,
the private worker socket, worker processes, live connection routing, and
capability clients. It does not interpret brofiles or own agents, workflows,
atoms, schedules, corpus stores, or harness implementation code.

## Invariants

- Shadow mode is read-only. It never opens the authority repository, worker
  socket, or process launcher.
- Authority mode holds an exclusive lifecycle lock for its state root. There
  is one writer for attempts, workers, leases, worktrees, commands, and roster.
- Synchronous `fleet-core` repository work runs only on the authority actor
  thread. Tokio workers never perform repository I/O.
- A worker connects outbound over a user-private Unix socket and authenticates
  with a task-scoped proof. Confirm a rotated proof only after the first valid
  post-handshake frame.
- Persist an initial user command in the same authority generation that
  provisions its worker. Never pass the initial prompt on worker argv.
  Mailbox-resume provisions no synthetic user turn; it reuses the named
  session and starts the harness in resume mode for mailbox delivery.
- Provider terminal results remain staged until the worker emits the durable
  session-snapshot commit marker.
- The blackbox record pump is asynchronous to worker acknowledgement. Outages
  leave immutable records in fleet-core's outbox for retry; only a complete
  `/internal/records` receipt removes a contiguous submitted prefix.
- Capability calls are authorized by fleet-core before leaving the process.
  Missing operational or corpus routes fail closed. Per-worker concurrency is
  bounded, calls run outside the sole receive loop, and one deadline covers
  connection, headers, response body, and decode so capability stalls cannot
  starve heartbeats, events, outcomes, or control commands.
- Configured capability authorization and downstream service availability are
  separate policy facts. Fleetd probes blackopsd and the corpus service, then
  durably advances the complete monotonic session policy before pushing it on
  the connected worker's generation-fenced control lane. Delivery failure
  closes that generation so reconnect recovers the persisted revision.
- A reconnect may revoke or restore allowed capabilities only through the next
  monotonic policy revision. Required protocol features and worker, task, and
  session identities remain fixed across live and reconnect revisions.
- Every HTTP route except health and readiness requires the shared same-host
  bearer. Trusted thin clients and peer daemons attach it; worker launch
  environments never do.
- Roster HTTP reads come from the materialized view, never the authority actor.
- Queue lag produces an explicit SSE resync event. It never silently skips a
  roster generation.
- `/mcp` exposes only low-level `bro_*` execution and fleet-control operations.
  Tool calls use the same authority and materialized-roster paths as HTTP, with
  bounded request bodies and wait deadlines. They never proxy operational work
  through blackboxd.
- Phased closeout runs fleetd's own driver locally, without a `bro-tools` or
  `bro-harness` dependency. When the driver removes a fleet-owned worktree,
  fleetd releases the generation-fenced durable ownership record without
  firing project closeout hooks a second time.
- The harness process is not killed when a fleetd handle is dropped. Service
  managers must preserve worker process groups across a fleetd restart.
