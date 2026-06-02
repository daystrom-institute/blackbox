---
title: "Codex · Subagents"
kind: research-finding
corpus: blackbox-research
track: harness
harness: codex
axis: subagents
version: "0.136.0"
last_verified: "0.136.0"
status: enriched
confidence: high
topic:
  - harness
  - codex
  - subagents
brief: "Codex subagents (v1/v2): full lifecycle verbs — spawn_agent, send_input (interrupt flag), send_message (queue, no turn) vs followup_task (triggers turn), resume_agent, wait_agent, list_agents, close_agent; hierarchical canonical task names (/root/task1/task_3). CSV fan-out (spawn_agents_on_csv, max_concurrency 16, worker-only report_agent_job_result). SessionSource-gated visibility; depth-limited; rich status enum. Anti-delegation-by-default steering."
---

# Codex · Subagents

> Mined from codex-rs source (`~/repos/codex/codex-rs`) by DeepSeek-v4-pro / GLM-5.1 bros, 2026-06-02. **confidence: high** (file:line).
See axis: [Subagents](../subagents.md) · snapshot: [Codex 0.136.0](codex-0.136.0.md).

**Finding.** Two API versions. **Lifecycle verbs:** `spawn_agent`, `send_input` (v1, `interrupt=true` for immediate redirect), `send_message` (v2, queue-only, no new turn) vs `followup_task` (v2, triggers a turn), `resume_agent`, `wait_agent` (min/default/max timeout), `list_agents` (`path_prefix` filter), `close_agent`. V2 uses **hierarchical canonical task names** (`/root/task1/task_3`). **CSV fan-out:** `spawn_agents_on_csv` (`{column}` templating, `max_concurrency`=16, `max_runtime`=1800s, optional output_schema); workers must `report_agent_job_result` (worker-only tool; missing = failure). **Role-gated visibility** via `SessionSource`/`SubAgentSource`; sub-agents inherit parent config + can nest; `exceeds_thread_spawn_depth_limit` enforces depth. Status enum: pending_init/running/interrupted/shutdown/not_found/{completed}/{errored}. Steering is **anti-delegation-by-default** ("Only use spawn_agent if and only if the user explicitly asks").

**Evidence.**
- `core/src/tools/handlers/multi_agents_spec.rs:113-260` — lifecycle verbs (send_input interrupt, wait, resume, close)
- `core/src/tools/handlers/agent_jobs_spec.rs:6-97` — CSV fan-out + worker-only report
- `core/src/tools/handlers/multi_agents.rs:28` — `exceeds_thread_spawn_depth_limit`

**Vs the axis.** Confirms ALL the subagent extensions: lifecycle verbs, role-differentiated visibility, CSV fan-out. Topology (path/depth) is shared with [session-lifecycle](../session-lifecycle.md).

## Open
<!-- v1↔v2 migration; mailbox interplay with send_message. -->
