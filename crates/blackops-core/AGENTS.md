# blackops-core invariants

- This crate owns durable operational intent, not live workers, worktrees, provider streams, transcript bytes, or corpus indexes.
- Every fleet mutation is first represented as a committed effect with a stable operation ID and idempotency key. Network I/O happens only in the daemon after that commit.
- Idempotency digests cover semantic effect intent, not request-observation
  timestamps. A replay after an ambiguous response naturally has a later
  timestamp and must still resolve to the original child, message, or effect;
  changing any semantic field under the same key remains a conflict.
- An operation whose fleet result is unknown remains requested. Recovery retries the same request and reconciles the returned attempt identity. It never invents a replacement operation.
- Logical agent identity, canonical path, parent graph, team role, mailbox sequence, and mailbox cursor survive every execution attempt.
- `send` appends mailbox data only. `followup` appends mailbox data and creates a durable execution operation against the same logical agent.
- Mailbox delivery admission advances only the session delivery cursor. It never terminalizes a followup operation or changes logical status; the accepted concrete attempt remains the sole terminal-status authority.
- A followup execution uses promptless `MailboxResume`; the typed mailbox owns the one model-visible body and wake decision.
- Definitions are immutable by `(kind, name, version)`. Activation changes a pointer and never mutates prior versions.
- Repository implementations provide cross-process compare-and-swap and atomic durable replacement. The authority never publishes in-memory state before persistence succeeds.
- Record publication is an idempotent outbox projection. A blackboxd outage must not roll back or block authoritative operational transitions.
- Keep this crate free of HTTP, Tokio, daemon implementations, `fleet-core`, and blackbox storage crates.
