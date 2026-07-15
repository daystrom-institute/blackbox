---
title: "Model-facing agent capability for bro-harness"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - bro-harness
  - orchestration
  - agents
brief: "Adds native spawn/message/followup/interrupt/list/wait tools through a session-bound AgentCapability. blackopsd owns logical identity, topology, mailboxes, and lifecycle intent; fleetd owns concrete attempts, workers, worktrees, and live control; bro-harness owns model-facing schemas and loop integration."
---

# Model-facing agent capability for bro-harness

## 0. Decision

Expose a small multi-agent v2 surface directly to bro-harness models, backed by
a session-bound `AgentCapability` defined in `bro-capabilities` and implemented
in the worker by an RPC client to fleetd. fleetd authorizes and forwards logical
agent operations to blackopsd while retaining concrete execution control. Do
not embed either service's scheduler, mailbox store, worktree manager, or agent
graph inside the harness.

The design follows the lifecycle semantics in the
[current Codex subagent finding](../../research/harness/codex/codex-subagents.md)
while preserving Blackbox's existing orchestration ownership and compile DAG.

## 1. Ownership split

| Concern | Owner |
|---|---|
| Model-facing names, descriptions, schemas, and tool results | bro-harness |
| Agent capability DTOs and trait | bro-capabilities |
| Authenticated worker/session binding | fleetd |
| Parent/root logical identity and canonical graph | blackopsd |
| Logical scheduling, teams, roles, mailbox policy | blackopsd |
| Concrete attempts, concurrency, provider/model allocation | fleetd |
| Worktree creation, cleanup, and live integration mechanics | fleetd |
| Mailbox storage and delivery | blackopsd |
| Loop wake-up and model-context injection | bro-harness plus capability events |
| Worker with no valid fleet capability | tools absent, fail closed |

The trait instance is session-bound. Model arguments never choose an arbitrary
parent session, fleet task, or worker connection.

## 2. Model-facing surface

### `spawn_agent`

```text
spawn_agent {
  task_name: string,
  message: string,
  fork_turns?: "none" | "all" | positive integer
}
```

Creates a child below the caller's canonical path. `task_name` is lowercase
letters, digits, and underscores. Default fork policy is all current history;
callers can request no history or only the most recent N turns.

blackopsd commits the logical child first and returns an idempotent execution-
attempt effect to fleetd. fleetd launches the concrete attempt after the
blackopsd request completes, avoiding a nested service-call cycle. A launch
failure leaves an addressable logical child with an unavailable attempt cause;
it does not erase identity.

The first version does not expose model or service-tier override. Those are
operator/blackops/fleet policy choices. They may be added later only as explicit
pass-through inputs, never inferred by the harness.

### `send_message`

Queues a message to an existing target. It does not start a new turn. Use it for
coordination or information delivery when the target's current lifecycle should
not change.

### `followup_task`

Adds work and triggers the target if idle. If running, delivery occurs at a safe
message boundary or after the pending tool call. It does not interrupt arbitrary
tool execution.

### `interrupt_agent`

Cancels the target's current turn and preserves the agent identity, mailbox, and
ability to receive later work. There is no model-facing destructive close tool
in v1 of this surface.

### `list_agents`

Returns live/addressable descendants, optionally filtered by canonical path
prefix. Results contain identity and status, not private task-message content.

### `wait_agent`

Waits for the caller's mailbox to change, for a descendant final-status notice,
for newly steered user input, or for timeout. It returns a summary of the wake
reason, not message bodies. The loop then drains typed inbound items through the
normal model-input path.

## 3. Contract-bottom shape

The exact Rust names may change, but the contract needs these concepts:

```rust
#[async_trait]
pub trait AgentCapability: Send + Sync {
    async fn spawn(&self, request: AgentSpawnRequest)
        -> CapabilityResult<AgentIdentity>;
    async fn send_message(&self, request: AgentMessageRequest)
        -> CapabilityResult<()>;
    async fn followup(&self, request: AgentMessageRequest)
        -> CapabilityResult<()>;
    async fn interrupt(&self, target: AgentTarget)
        -> CapabilityResult<AgentStatus>;
    async fn list(&self, prefix: Option<String>)
        -> CapabilityResult<Vec<AgentSummary>>;
    async fn wait(&self, request: AgentWaitRequest)
        -> CapabilityResult<AgentWake>;
}
```

DTOs use `bro-core` identifiers and serde only. `bro-capabilities` remains pure:
no tokio runtime types, filesystem handles, daemon structs, or I/O clients.

The worker-side capability object is a session-scoped RPC client. fleetd binds
the authenticated worker connection to a policy envelope. blackopsd binds that
session to the canonical root identity and verifies every target is the root or
a descendant. Transport framing and retries live in `bro-rpc`, not in the trait.

## 4. Canonical identity and lifecycle

Canonical names are hierarchical, for example `/root/research/api_scan`. A
spawn returns the canonical name and an opaque durable ID. Model-facing tools
prefer names; protocol/events may carry both.

Statuses distinguish at least:

- initializing;
- running;
- idle/addressable;
- interrupted;
- completed/addressable;
- errored/addressable;
- evicted/not loadable;
- not found.

Completed and interrupted describe turns, not object destruction. A follow-up
may start another turn on an addressable agent. Eviction is an orchestrator
retention decision and must remain distinguishable from completion.

## 5. Mailbox and loop integration

blackopsd stores messages as typed events with sender, recipient, kind, and
payload. The harness receives them through the session capability/event channel
and turns them into contextual model input at safe boundaries.

`wait_agent` must be interruptible by ordinary user steering. A user message has
higher priority than continuing to wait and becomes the next model-visible input
without requiring the user to know which agent tool is blocked.

Queue-only messages and follow-up tasks are separate event kinds. The target
loop can therefore decide whether to wake without parsing prose or inferring
intent.

## 6. Persistence and resume

blackopsd persists:

- canonical path and durable ID;
- parent edge and depth;
- role/config identity chosen by policy;
- mailbox sequence/cursor;
- last terminal turn status;
- underlying thread/session reference when resumable.

Cold root resume restores the identity graph first. Child runtimes load lazily
when targeted or explicitly continued. A missing runtime does not erase the
identity; it yields an evicted/unavailable status with a cause.

Agent availability and collaboration policy are represented in
[Model-visible World State](model-visible-world-state.md), so resume can tell the
model exactly which surface remains available without resending unrelated
context.

## 7. Policy and authority

- Register these tools only when an `AgentCapability` is connected and current
  collaboration policy permits model delegation.
- Tool filters apply to direct and code-mode invocation equally.
- AGENTS.md or a selected skill may authorize delegation, but cannot broaden the
  bound capability's target tree, concurrency, model, worktree, or provider
  policy.
- Model/reasoning/service-tier overrides are omitted initially. If later added,
  they pass through only explicit operator-authorized values.
- Message encryption at rest or over a remote transport is a storage/transport
  policy. Do not copy Codex's encrypted schema marker blindly into a same-host
  trait contract and mistake annotation for protection.

## 8. Prompt-cache and fork behavior

Forked agents should use a cache identity derived from the root session, not
their unique child thread IDs, when the provider supports prompt-cache keys.
This lets shared system instructions and forked history reuse the same stable
prefix while durable child identity remains distinct.

`fork_turns` controls only conversational history. World State and current
capability policy are rebuilt from the child's actual environment, not copied as
stale rendered text.

## 9. Phases

1. Add DTOs/trait, worker RPC client, blackopsd service, and fleetd execution
   adapter for spawn/list/status.
2. Add `send_message` and `followup_task` as distinct typed events.
3. Add non-destructive interrupt and balanced tool-result handling.
4. Add mailbox wait with user-steer wake-up.
5. Persist graph/mailbox cursors and implement lazy cold resume.
6. Add partial-history forks and shared root prompt-cache identity.

Each phase remains fail-closed when the worker has no valid fleet capability.

## 10. Verification contract

Tests must prove:

- a worker without a valid fleet capability exposes no agent tools;
- target paths cannot escape the injected root tree;
- duplicate sibling task names fail deterministically;
- `send_message` does not trigger an idle turn;
- `followup_task` does trigger an idle target;
- interrupt ends the current turn but preserves addressability;
- list omits message payloads;
- wait wakes on message, final status, user steering, and timeout with distinct
  typed results;
- partial fork policies include exactly the requested user-turn suffix;
- cold resume restores identity without eagerly starting every runtime;
- direct and code-mode calls share the same filter and authority checks;
- root/child prompt-cache identities share the intended stable prefix key.

## 11. Relationship

- [Harness-daemon boundary](harness-daemon-boundary.md) defines the capability
  inversion this design extends.
- [Worker protocol](worker-protocol.md) carries the session-scoped capability
  calls and fleet events.
- [Process topology](../daemon-runtime/process-topology.md) places canonical
  agent intent in blackopsd and concrete attempts in fleetd.
- [Blackops service boundary](../daemon-runtime/blackops-service-boundary.md)
  defines the logical-agent versus execution-attempt split.
- [Agent system](../orchestration/agents/agent-system.md) owns discoverable agent
  artifacts and dispatch policy. This doc owns the live model-facing lifecycle.
- [Supervision](../orchestration/supervision/supervision.md) owns higher-order
  allocation and oversight, not the primitive message verbs.
- [Remote-worker boundary](remote-worker-boundary.md) owns where a child executes
  and how artifacts return. The model-facing contract remains location-neutral.
