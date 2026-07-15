---
title: "Codex - main@8aae858958 (snapshot)"
kind: research-subject
corpus: blackbox-research
track: harness
harness: codex
version: "main@8aae858958"
platform: macos-aarch64
captured: "2026-07-14"
supersedes: codex-0.136.0.md
status: enriched
topic:
  - harness
  - codex
brief: "Point-in-time source snapshot of Codex main at 8aae858958, compared with the 0.136.0 capture. The material deltas are a hardened and optionally process-owned V8 code-mode runtime, persisted model-visible World State, a resumable multi-agent v2 lifecycle, context-window agency, extension-owned skills with shadow selection, and reusable MCP tool catalogs."
---

# Codex - main@8aae858958 (snapshot)

This snapshot supersedes [Codex 0.136.0](codex-0.136.0.md). It is a source
revision snapshot, not a release claim: the captured `main` commit is newer than
the most recent merged Rust prerelease tag.

## Provenance

- **Subject:** OpenAI Codex.
- **Source:** `git@github.com:openai/codex.git`, checked out at
  `/Users/invidious/repos/codex`.
- **Revision:** `8aae85895809601a055902f1b85647620e01a523`, matching
  `origin/main` with a clean worktree at capture time.
- **Revision timestamp:** `2026-07-15T04:44:20Z`.
- **Nearest merged Rust release tag:** `rust-v0.143.0-alpha.10`; the snapshot
  intentionally uses the commit identity because `HEAD` is untagged.
- **Baseline:** `rust-v0.136.0`, the source basis for the 2026-06-02 snapshot.
  `git rev-list --left-right --count rust-v0.136.0...HEAD` returned `1 1232`.
- **Extraction:** direct source reading, focused `git diff` and `git log` over
  the baseline range, plus the fresh official Codex manual captured on the same
  research pass. Internal-runtime claims below come from source, since the
  public manual does not specify these implementation contracts.

**Confidence: high** for source-level behavior at this exact revision. Product
availability and feature rollout are not inferred from source presence.

## Delta from 0.136.0

### 1. Code mode became a supervised runtime

The model-facing `exec` and `wait` idea is stable, but its implementation is no
longer the earlier monolithic in-process service. Codex now separates:

- a transport-neutral protocol and session runtime;
- a per-cell actor that owns the serialized lifecycle;
- linearized completion, termination, and stored-value commit semantics;
- hierarchical session and cell cancellation;
- preservation of initial yield boundaries and unobserved terminal output;
- host-failure supervision;
- an optional process-owned V8 companion with a versioned handshake and a
  delegated nested-tool-call lane back to the parent process;
- optional JIT-less V8.

The separate process is failure containment, not a new authority boundary. It
still reaches tools only through the parent session's admitted tool surface.
See [Codex metatools](codex-metatools.md).

### 2. Context deltas became persisted World State

The earlier `reference_context_item` diff pattern has been generalized into
typed model-visible sections. Each section has a stable identifier, a
serializable snapshot, legacy/retained-fragment matchers, and a section-owned
`render_diff`. Rollouts persist full snapshots or RFC 7386 merge patches and
replay them on resume, fork, rollback, and compaction. A known snapshot whose
rendered fragment disappeared from retained history is re-injected once.

Environment, AGENTS.md, app guidance, plugin guidance, extension-contributed
state, and selected skill instructions now use this mechanism. See
[Codex context management](codex-context-management.md).

### 3. Multi-agent v2 is resumable and mailbox-oriented

The current v2 surface distinguishes queue-only messages from turn-triggering
follow-up work, interrupts a turn without destroying the target, and waits on a
mailbox rather than harvesting result text directly. `wait_agent` also wakes on
new user steering. Descendant identities can be restored on cold root resume,
with runtimes loaded lazily on targeted communication. Model/reasoning overrides
are separately gated and filtered for backend compatibility.

Codex also added an extension-level `AgentRunner`, making "run a resolved agent
in a forked thread" a reusable host capability rather than only a hardwired
collaboration-tool implementation. See [Codex subagents](codex-subagents.md).

### 4. Context windows gained identity and model agency

Context windows now carry UUIDv7 identities and first/previous/current lineage
through compaction and reconstruction. Two new model-facing tools expose the
remaining token count and request a new context window without clearing
environment state. Resume reconstruction can begin from a bounded safe suffix
after the most recent compaction checkpoint instead of always replaying the
entire rollout. See [Codex compaction](codex-compaction.md) and
[Codex session lifecycle](codex-session-lifecycle.md).

### 5. Skills became an extension-owned, measurable disclosure system

Skill discovery, state, tools, rendering, and invocation accounting moved into
an extension-owned subsystem. A deterministic weighted lexical selector now
runs in shadow mode against eligible prompt-visible skills. Its output remains
non-authoritative while metrics compare selected rank with actual invocation.
This is a rollout discipline for shrinking catalogs safely, not only a ranking
algorithm. See [Codex skills](codex-skills.md).

### 6. MCP startup gained reusable catalog metadata

Codex now caches sanitized stdio MCP tool catalogs process-wide in a 32-entry,
30-minute LRU. Cached definitions can shape an early tool surface while the live
connection starts, but live connection metadata remains authoritative for
approval, parallelism, server instructions, and execution. HTTP and
remote-environment-dependent catalogs are excluded until they have a safe
identity. See [Codex MCP tooling](codex-mcp.md).

### 7. Small model-facing tools were added

New built-ins include `get_context_remaining`, `new_context`, current UTC time,
interruptible sleep, and a transient `wait_for_environment` surface while a
deferred executor environment starts. Code mode also gained a dedicated
generated-image result helper. See [Codex built-in tools](codex-builtin-tools.md).

## Axis checklist

"Stable" means no material delta was re-mined in this pass; its leaf retains
the older `last_verified` value.

| Axis | Leaf | This snapshot | Confidence |
|---|---|---|---|
| Transport & Feature Flags | [codex-transport](codex-transport.md) | stable from 0.136.0 | high |
| Robustness | [codex-robustness](codex-robustness.md) | stable; code-mode supervision filed under metatools | high |
| Compaction | [codex-compaction](codex-compaction.md) | refreshed | high |
| Session Lifecycle & History | [codex-session-lifecycle](codex-session-lifecycle.md) | refreshed | high |
| Agent Loop | [codex-agent-loop](codex-agent-loop.md) | stable; mailbox changes filed under subagents | high |
| Context Management | [codex-context-management](codex-context-management.md) | refreshed | high |
| Planning & Goal State | [codex-planning-goals](codex-planning-goals.md) | stable from 0.136.0 | high |
| Built-in Tools | [codex-builtin-tools](codex-builtin-tools.md) | refreshed | high |
| MCP Tooling | [codex-mcp](codex-mcp.md) | refreshed | high |
| Subagents | [codex-subagents](codex-subagents.md) | refreshed | high |
| Hooks | [codex-hooks](codex-hooks.md) | stable from 0.136.0 | high |
| Skills | [codex-skills](codex-skills.md) | refreshed | high |
| Metatools | [codex-metatools](codex-metatools.md) | new dedicated cell | high |
| Privilege, Sandboxing & Approvals | [codex-privilege-approvals](codex-privilege-approvals.md) | stable from 0.136.0 | high |
| Memory & Persistence | [codex-memory-persistence](codex-memory-persistence.md) | stable from 0.136.0 | high |
| Modes, Personas & Roles | [codex-modes-personas](codex-modes-personas.md) | stable from 0.136.0 | high |

## Design promotion

The descriptive findings from this snapshot feed:

- [Codex mainline adoption map](../../../design/bro-harness/codex-mainline-adoption.md)
- [Code-mode runtime lifecycle](../../../design/bro-harness/code-mode-runtime-lifecycle.md)
- [Model-visible World State](../../../design/bro-harness/model-visible-world-state.md)
- [Model-facing agent capability](../../../design/bro-harness/model-facing-agent-capability.md)
- [Process topology](../../../design/daemon-runtime/process-topology.md)
- [Blackops service boundary](../../../design/daemon-runtime/blackops-service-boundary.md)
- [Agent runtime implementation strategy](../../../design/daemon-runtime/agent-runtime-program.md)
