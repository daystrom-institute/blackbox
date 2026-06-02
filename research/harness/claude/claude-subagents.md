---
title: "Claude · Subagents"
kind: research-finding
corpus: blackbox-research
track: harness
harness: claude
axis: subagents
version: "2.1.160"
last_verified: "2.1.160"
status: enriched
confidence: high
topic:
  - harness
  - claude
  - subagents
brief: "Claude Code 2.1.160 exposes subagents via the Agent tool and separately manages background sessions through claude agents. The Agent tool has typed agents, scoped tools, worktree isolation, background execution, model override, final-text-as-result, and SendMessage continuation; the CLI agent view adds JSON listing, dispatch defaults, worktree/tmux support, cwd filtering, and preserved permission/model/effort/plugin/MCP settings."
---

# Claude · Subagents

> **Provenance.** Direct observation of the running 2.1.160 harness — the Agent
> tool schema and agent-type registry as the model receives them.
> **confidence: high.** See [snapshot](claude-2.1.160.md).

See the axis: [Subagents](../subagents.md).

## Spawn interface (high)

The `Agent` tool launches a subagent. Args: `prompt`, `description` (3–5 words),
`subagent_type`, optional `model` override (`sonnet`/`opus`/`haiku`),
`isolation: "worktree"`, and `run_in_background`. *"The agent's final message is
returned to you as the tool result; it is not shown to the user"* — so subagents
return raw data to the parent, not human-facing prose.

## Agent-type registry (high)

Named types with **scoped tool access and bespoke prompts**:

- `claude` — catch-all default (all tools).
- `Explore` — read-only search agent (all tools *except* Agent/Edit/Write/
  NotebookEdit/ExitPlanMode); reads excerpts, locates code, does not mutate.
- `general-purpose` — research/multi-step (all tools).
- `Plan` — software-architect, returns implementation plans (read-only toolset).
- `statusline-setup`, `claude-code-guide` — narrow specialists.

The tool description steers *when to delegate* (broad fan-out searches → Explore;
"conclusion, not the file dumps") and *when not to* (single-fact lookup → search
directly).

## Isolation, parallelism, continuation (high)

- **Isolation.** `isolation: "worktree"` runs the agent in a fresh git worktree
  (auto-cleaned if unchanged).
- **Background.** `run_in_background: true` runs async; parent is notified on
  completion.
- **Parallelism.** *"When you launch multiple agents for independent work, send
  them in a single message with multiple tool uses so they run concurrently."*
- **Continuation.** A new `Agent` call starts fresh; `SendMessage` to an agent's
  id/name continues it *"with its context intact."*

## Background Agent CLI (2026-06-02 local pass)

claude agents is now a substantial orchestration surface, not only a viewer. Current help exposes --json for machine-readable live sessions, --cwd filtering, --add-dir, --settings, --mcp-config, --strict-mcp-config, --plugin-dir, --permission-mode, --allow-dangerously-skip-permissions, --dangerously-skip-permissions, --model, --effort, and --agent as dispatch defaults.

Changelog entries add lifecycle and isolation details: background sessions can be resumed, detached sessions preserve fallback model and permission settings, background subagents report running state on resume, worktree isolation guards are enforced for subagents/background sessions, and Claude-managed worktrees live under .claude/worktrees/ with cleanup/retention behavior. Background agents and bash commands can run asynchronously and send messages to wake the main agent.

The Agent tool and claude agents should be treated as related but distinct: Agent is an in-session delegation tool; claude agents is a process/session manager for background Claude sessions.

## Open

<!-- TODO(mine): concurrency cap value and queueing behavior; how subagent output
is folded into the parent window (verbatim? truncated?); the full per-type tool
allowlists and system-prompt deltas; relationship to the Task* todo tools. -->

## Feeds

`design/orchestration/agents/`. Note the bro-harness invariant: harness-side
subagent delegation must not become a daemon RPC backchannel
(`PROJECT.md` → bro-harness shares code, never runtime).
