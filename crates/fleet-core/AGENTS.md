# fleet-core

`fleet-core` is the sole live execution authority below transport and service
hosting. It owns attempts, tasks, sessions, worker leases, acknowledgements,
roster projection, and worktree ownership. It does not own HTTP, local-socket
framing, provider execution, blackops policy, corpus storage, or harness code.

## Invariants

- Persist authority changes before returning them to callers. A failed write
  must leave the in-memory snapshot unchanged.
- The bundled file repository and migration readers are synchronous. Async
  service hosts must invoke them through a blocking-I/O lane or persistence
  actor, never directly on a runtime worker.
- Fence every write with the snapshot generation. Fence reconnect-sensitive
  worker writes with the connection generation as well.
- Store only hashes of worker authentication proofs. A raw proof may cross a
  return value once, but it must never enter a snapshot, log, or debug output.
- Event and command acknowledgements are highest contiguous durable sequence
  numbers. Duplicates are idempotent; gaps fail closed.
- Agent mailbox delivery is a durable worker command bound to the session's
  immutable logical agent id and canonical path. Fleet acknowledges admission
  only after the worker persists the cursor and body; reconnect and restart
  replay the same delivery id without crossing a target binding.
- Attempt transitions and bounded session-event coordinates enter the fleetd
  record outbox in the same durable generation as their authority mutation.
  Producer cursors are contiguous; only a complete blackboxd receipt advances
  the acknowledged cursor or a worker's last-indexed event sequence.
- A worktree has one active owner. Cleanup and release require both owner
  identity and ownership generation so stale actors cannot reclaim new work.
- The roster is a persisted projection of the task record. It carries bounded
  summaries and transcript coordinates, never transcript event payloads.
- Idempotent attempt admission stores a canonical request digest, not secrets
  such as shell environment values or full prompts.
- Startup converts active worker connections to reconnectable state and keeps
  their leases during the grace window. It does not invent terminal outcomes.
- Migration readers are read-only and tolerate additive legacy fields. They
  fail closed on unsupported authority versions or malformed identifiers.

## Dependency boundary

This crate may depend on the pure contract crates `bro-core`, `bro-protocol`,
and `bro-capabilities`, plus implementation-neutral utilities. It must never
depend on blackbox server modules, `SharedState`, `bro-harness`, `bro-rpc`,
fleet clients, HTTP frameworks, databases, or provider implementations.

## Testing

Exercise persistence failure and stale-generation paths, not only successful
state changes. Migration fixtures must cover the current `tasks.json` array and
`worker-authority.json` version 1 object, including additive unknown fields.
