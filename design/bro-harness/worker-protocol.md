---
title: "Bro-harness worker protocol"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - bro-harness
  - daemon-runtime
  - orchestration
  - fleet-tui
tags: [rpc, uds, workers, reconnect, replay, capabilities, control]
brief: "Defines the versioned same-host RPC between fleetd and one bro-harness session worker: worker-initiated reconnect, authenticated identity, sequenced event replay from the session log, idempotent controls, multiplexed capability calls, leases, backpressure, and explicit restart behavior."
---

# Bro-harness worker protocol

## 0. Decision

Use one authenticated, full-duplex Unix-domain socket connection between fleetd
and each `bro-harness` worker. The worker initiates and re-establishes the
connection. Protocol types live in `bro-protocol`; framing and I/O live in a
separate `bro-rpc` crate.

The protocol replaces three current process-local mechanisms:

- in-process session-input senders;
- callback delivery of harness events;
- process-global installed daemon capabilities.

It is not a transcript transport, provider protocol, or remote-worker protocol.
The same-host transcript remains a file owned by the worker.

## 1. Transport

The first transport is a local Unix-domain stream socket owned by fleetd:

- socket directory and file are user-private;
- the worker connects outbound, so fleetd can restart independently;
- each frame is a bounded 32-bit length followed by UTF-8 JSON;
- unknown additive fields are tolerated within a protocol generation;
- invalid length, invalid JSON, or an unknown required message closes the
  connection with a typed protocol cause;
- writes use bounded queues and explicit overflow policy.

Length framing is preferred to newline framing because tool results and model
events may contain arbitrary newlines. JSON keeps the first implementation
inspectable. The framing layer may later negotiate another encoding without
changing message semantics.

## 2. Identity and handshake

fleetd creates a session record and a one-time bootstrap secret before spawning
the worker. The worker starts with only:

- fleet socket path;
- task and session IDs;
- bootstrap secret or reconnect credential;
- session configuration path or inherited descriptor;
- transcript/event-log path;
- explicit working directory.

The first exchange is:

```text
WorkerHello {
  protocol_versions,
  worker_build,
  task_id,
  session_id,
  bootstrap_or_resume_proof,
  last_local_event_seq,
  last_fleet_command_seq,
  worker_capabilities
}

FleetWelcome {
  selected_protocol,
  connection_generation,
  event_ack,
  next_command_seq,
  lease,
  session_policy,
  fleet_build
}
```

fleetd validates that the credential belongs to the session, rotates bootstrap
material into a reconnect credential, and rejects duplicate live ownership
unless the existing lease is provably stale.

The reconnect credential is scoped to one session and stored as a verifier by
fleetd. It never authorizes another session or a corpus call by itself.

## 3. Envelope and channels

Every post-handshake frame uses a common envelope:

```text
Envelope {
  protocol_version,
  connection_generation,
  message_id,
  reply_to?,
  body
}
```

The connection multiplexes four logical channels:

1. worker events to fleetd;
2. fleet commands to the worker;
3. capability request/response calls initiated by the worker;
4. lease, acknowledgement, drain, and protocol control.

`message_id` correlates unary replies. Ordered event and command streams carry
their own monotonic sequence numbers and are not ordered by message ID.

## 4. Worker event stream

The worker assigns a strictly increasing `event_seq` before appending each
event to its session log. The log append precedes network delivery. This makes
the event log the durable outbox.

```text
WorkerEvent {
  event_seq,
  occurred_at,
  event
}

EventAck {
  through_event_seq
}
```

fleetd processes events idempotently and acknowledges the highest contiguous
sequence durably reflected in its task/roster state. After reconnect the worker
replays all logged events after that acknowledgement.

The event schema includes compact lifecycle, usage, tool, turn, output, and
terminal events needed by fleetd. The complete provider transcript stays in the
worker file. fleetd stores the path and compact projections, not a duplicate
full transcript.

If the network queue fills, the worker continues appending to the bounded-on-
disk event log and stops admitting unbounded memory. A disk budget failure is a
visible session failure, not permission to drop lifecycle events silently.

## 5. Fleet command stream

Commands carry a fleet-assigned monotonic `command_seq` and stable command ID:

```text
WorkerCommand {
  command_seq,
  command_id,
  command
}

CommandOutcome {
  command_seq,
  command_id,
  accepted,
  terminal?,
  result_or_error
}
```

Initial commands are:

- user turn or steer;
- interrupt;
- set model when supported;
- compact;
- drain after the current safe boundary;
- shutdown;
- request status snapshot.

The worker persists or otherwise remembers the highest applied command sequence
and stable outcomes needed across reconnect. A duplicate command returns its
prior outcome and never applies twice.

`accepted` and `terminal` are distinct. A steer can be accepted into the next
turn boundary before its effects are terminal. Interrupt acknowledgement means
the cancellation request reached the session state machine, not that every
external effect was rolled back.

## 6. Capability calls

The worker receives no blackboxd address or daemon implementation object.
Session-scoped clients implement the `bro-capabilities` traits over this
connection:

```text
CapabilityRequest {
  call_id,        // ephemeral request/response correlation
  invocation_id?, // stable originating provider tool-call identity
  capability,
  operation,
  bounded_payload,
  deadline?
}

CapabilityResponse {
  call_id,
  result_or_error
}
```

`invocation_id` and `call_id` are deliberately different lanes. Reconnect or
an ambiguous lost response may replay one provider tool invocation through a
new RPC request, so effectful agent and atom operations derive idempotency only
from the stable invocation identity. A legacy request without it may use
`call_id` for compatibility, but the worker's model-facing path always sends
it. Flat tools use the provider call ID directly. A nested code-mode call uses
the outer `exec` call ID plus its deterministic cell-runtime call ordinal.

fleetd binds each connection to a policy envelope and ignores any caller-
supplied attempt to select another root session, worktree, provider identity, or
authority scope.

An operational response may include a typed fleet effect such as
`RequestAttempt`. fleetd applies that effect only after the blackopsd call has
returned, using the operation ID for deduplication. This avoids a synchronous
fleetd to blackopsd to fleetd call cycle.

Routing is explicit:

- task attempt, worker, worktree, and live-control calls terminate in fleetd;
- logical agent, mailbox, workflow, atom, schedule, and operational calls are
  authorized by fleetd and forwarded to blackopsd;
- corpus, transcript, knowledge, and evidence calls are authorized by fleetd
  and forwarded to blackboxd;
- file, shell, Git, V8, and working-copy LSP calls never cross this protocol.

Capability calls are bounded and cancelable. Connection loss fails outstanding
calls with a typed unavailable cause. Only operations explicitly defined as
idempotent may be retried automatically, and retries preserve `invocation_id`
while minting a fresh `call_id`.

## 7. Lease and reconnect state machine

```text
Starting -> Connecting -> Active -> Disconnected -> Reconnecting
                 |           |             |              |
                 |           v             v              v
                 +-------> Draining ----> Terminal <-------+
```

- **Starting:** worker initializes its local session and log.
- **Connecting:** no remote capability calls are admitted yet.
- **Active:** lease and heartbeat are healthy.
- **Disconnected:** local/provider work may reach a safe boundary; remote calls
  return retryable unavailable or wait within a bounded policy.
- **Reconnecting:** handshake, acknowledgement reconciliation, event replay, and
  command replay occur before returning to Active.
- **Draining:** no new user turn begins; accepted local work reaches a defined
  boundary and terminal state is reported.
- **Terminal:** the event log is closed after the final event is durable.

Workers send heartbeats even while a provider request is quiet. fleetd does not
declare a worker dead until both the connection and lease grace have expired.
After fleetd restart it waits a configured reattach grace before marking
persisted live workers lost.

## 8. Failure behavior

| Failure | Behavior |
|---|---|
| fleetd socket disappears | Worker retains local state, logs events, and reconnects |
| blackopsd unavailable | Operational and collaboration calls fail closed or retry by policy |
| blackboxd unavailable | Routed corpus calls fail closed or retry by operation policy |
| worker process exits | fleetd expires lease and marks the session interrupted/resumable |
| malformed frame | Close generation, record protocol error, reconnect only if policy permits |
| handshake version mismatch | Reject with supported versions; no silent downgrade |
| duplicate worker for live lease | Reject newer connection unless explicit takeover policy applies |
| event gap | fleetd requests replay from last contiguous acknowledgement |
| stale-generation reply | Ignore and record; it cannot satisfy a current call |
| command replay | Worker returns the recorded outcome without reapplying |

An ongoing provider HTTP stream may continue while fleetd is briefly absent.
The worker must not begin new fleet-authorized side effects when it cannot prove
current authority.

## 9. Security and policy

- Use filesystem permissions and per-session credentials, not socket location
  alone, as authentication.
- Do not place provider secrets or blackboxd credentials in protocol events.
- Redact capability payloads from ordinary info logs.
- Bind every request to the authenticated session policy.
- Apply the same `ToolFilter`, collaboration policy, and operator-authority
  rules before and after reconnect.
- Rotate reconnect credentials on explicit session takeover or compromise.
- Refuse cross-user socket clients even when they know a session ID.

This is a same-user local RPC, not a hostile-network security boundary. Remote
workers require transport encryption, host identity, artifact transfer, and a
different trust analysis.

## 10. Backpressure and budgets

The protocol defines limits for:

- maximum frame size;
- queued outbound bytes per logical channel;
- concurrent capability calls;
- unacknowledged event bytes and on-disk retention;
- command backlog;
- handshake, call, drain, and shutdown deadlines;
- heartbeat and reattach grace.

Control frames have priority over bulk event replay. Capability replies cannot
starve interrupt or drain commands. Replay is chunked so a long-disconnected
worker does not monopolize fleetd.

## 11. Compatibility rules

- Protocol major versions change only for incompatible semantics.
- Additive fields and message variants use negotiated capability flags.
- fleetd supports a bounded worker-version window for rolling replacement.
- A worker build remains usable for existing sessions until fleet policy drains
  it or its protocol version leaves the support window.
- The handshake records both builds in system events for diagnosis, not as a
  durable user identity.

## 12. Verification contract

Tests must prove:

- worker reconnect after fleetd restart with no duplicated event projection;
- replay from every possible acknowledgement boundary;
- duplicate command idempotency;
- interrupt priority during large event replay;
- stale generation replies cannot complete current calls;
- credential and session mismatch fail before capability admission;
- blackboxd outage does not terminate a local-only turn;
- blackopsd outage does not terminate a local-only turn;
- worker death affects only one session;
- transcript tailing works while the RPC is disconnected;
- old and new supported worker versions can coexist;
- unsupported versions fail with a precise diagnostic;
- bounded queues and disk budgets have explicit terminal behavior.

## 13. Relationship

- [Process topology](../daemon-runtime/process-topology.md) owns service
  authority and restart behavior.
- [Blackops service boundary](../daemon-runtime/blackops-service-boundary.md)
  owns operational intent behind the fleet broker.
- [Fleet extraction](../daemon-runtime/fleet-extraction.md) owns rollout.
- [Harness-daemon boundary](harness-daemon-boundary.md) owns the contract and
  compile constraints.
- [Code-mode runtime lifecycle](code-mode-runtime-lifecycle.md) owns cell-level
  state and cancellation inside the worker.
- [Remote-worker boundary](remote-worker-boundary.md) owns the later off-host
  trust and filesystem split.
