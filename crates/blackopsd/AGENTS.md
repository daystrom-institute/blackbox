# blackopsd invariants

- blackopsd is the operational-intent service. It links `blackops-core` and contract crates, never `fleet-core`, blackbox storage implementations, provider transports, or harness code.
- HTTP handlers commit logical state and durable effects before returning. They do not hold the authority mutex across network calls.
- The fleet effect pump submits the exact committed `ExecutionRequest`. Ambiguous failures remain retryable under the same idempotency key.
- Worker-originated agent calls arrive through `/internal/capability`. The handler finishes the logical transition and returns before fleetd executes the derived effect, preventing a synchronous fleetd to blackopsd to fleetd cycle.
- The record pump sends `RecordIngestRequest` batches to blackboxd and removes records only after a complete ingest receipt. Outage and retry are normal.
- Agent capability handlers use the shared `bro-capabilities` DTOs. Session
  identity supplied by fleetd determines parent scope; provider invocation
  identity determines durable effect idempotency. RPC `call_id` only
  correlates one request and response and must not mint logical identity.
- The catalog adapter imports the exact build-embedded shipped atom, brofile,
  and workflow sources plus bounded installed artifacts. An atom backend is
  semantic authority: profile, workflow, deterministic, adapter, and consultant
  definitions keep distinct execution paths and retain input/output schemas,
  effects, composition, and trace metadata. Never flatten a non-profile backend
  into a generic model prompt or silently substitute backend semantics.
- Service status exposes operational counts and backlog only. Live worker and worktree state remains in fleetd.
- Every non-health route requires the shared same-host bearer. Outbound fleetd
  and blackboxd clients attach it. Neither token contents nor path may enter a
  worker environment or protocol message; fleetd's inherited OS sandbox blocks
  worker access to the canonical token path.
- A logical session root binds to the first authenticated worker for its accepted attempt. A new durable attempt clears that binding exactly once; otherwise reusing a session id with a different worker identity fails closed.
- `wait_agent` observes mailbox sequence only on the caller logical agent. Its optional path prefix scopes descendant terminal-status claims and never combines independent child mailbox cursors.
