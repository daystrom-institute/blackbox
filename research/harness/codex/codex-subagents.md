---
title: "Codex · Subagents"
kind: research-finding
corpus: blackbox-research
track: harness
harness: codex
axis: subagents
version: "main@8aae858958"
last_verified: "main@8aae858958"
status: enriched
confidence: high
topic:
  - harness
  - codex
  - subagents
brief: "Codex multi-agent v2 exposes spawn_agent, queue-only send_message, turn-triggering followup_task, non-destructive interrupt_agent, list_agents, and mailbox-oriented wait_agent. Canonical task paths persist across cold root resume, targeted descendants load lazily, partial history forks are supported, and an extension-level AgentRunner makes forked agent execution reusable outside the collaboration tool family."
---

# Codex - Subagents

See axis: [Subagents](../subagents.md) and snapshot:
[Codex main@8aae858958](codex-main-8aae858958.md).

## Finding

Codex retains legacy v1 and CSV worker surfaces, but its current v2 contract is
mailbox-oriented and keeps agent identity alive independently of the current
turn.

**Confidence: high.** The schemas, handlers, rollout items, and restore path are
open source at the captured revision.

### Current v2 lifecycle

- `spawn_agent` creates a child with a required lowercase task name, an initial
  message, and a history fork policy of none, all, or the most recent N turns.
- `send_message` queues a message promptly but does not trigger a new turn.
- `followup_task` triggers an idle target and otherwise delivers at a safe
  message boundary while sampling or after the pending tool call.
- `interrupt_agent` stops the target's current turn but leaves the agent alive
  and addressable for later messages or work.
- `list_agents` lists live descendants with an optional canonical path-prefix
  filter. It no longer exposes the last task message.
- `wait_agent` waits on the caller's mailbox. It returns a summary of which
  agents have updates, a user-steer interruption summary, or a timeout summary,
  not the message content itself.

The distinction between queueing, triggering, and interrupting is structural.
It avoids overloading one `send_input(interrupt=...)` call with three lifecycle
meanings.

### Identity, persistence, and configuration

V2 routes by canonical names such as `/root/task1/task_3`. Task and message
payload fields are marked encrypted in the model schema while routing metadata
remains available to the control plane. Agent communication is persisted as
typed rollout items.

Cold root resume restores descendant identities and graph position. A targeted
message can lazily load a descendant runtime rather than eagerly restarting the
whole tree. The `AgentRunner` extension encapsulates starting a resolved prompt
in a forked thread with trace propagation and returns thread and turn IDs.

Spawned agents inherit the parent model by default. Model and reasoning
overrides are separately gated, bounded, and filtered to models compatible with
the active multi-agent backend.

Delegation remains policy-controlled. The default guidance is conservative,
but AGENTS.md or an invoked skill may explicitly authorize delegation.

### Legacy and batch surfaces

V1 still exposes `send_input`, `resume_agent`, and close semantics under a
namespace. CSV fan-out remains a separate worker-job surface with bounded
concurrency and structured row reporting. Those are compatibility surfaces, not
the v2 lifecycle recommended for a new harness design.

## Evidence

- `codex-rs/core/src/tools/handlers/multi_agents_spec.rs` - v1/v2 schemas and
  steering text.
- `codex-rs/core/src/tools/handlers/multi_agents_v2/` - current handlers.
- `codex-rs/ext/agent/src/lib.rs` - reusable forked-thread runner.
- Commits `5b22a8e5b1`, `b4f0f3eff1`, `088239294a`, `ea15456284`,
  `92938d880e`, `64c0e2fa1b`, and `6629e08702`.

## Vs the axis

The refresh sharpens "continuation" into three independent verbs: message,
follow-up, and interrupt. It also shows that persisted identity plus lazy runtime
materialization is more useful than treating completion as object destruction.
Topology remains shared with [session lifecycle](../session-lifecycle.md).

## Open

- Worktree and sandbox isolation remain environment/orchestrator concerns, not
  properties of the generic v2 tool schema.
- Source presence does not establish which product configurations enable v2 by
  default.
