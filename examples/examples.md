# Examples

Tutorial configs and integration demos that demonstrate how to wire blackbox
(`bbox_*`, `bro_*`, and `atom_*` MCP tools) into the CLIs it orchestrates.
Copy or adapt these into your own project or agent directories.

Blackbox-owned installable defaults now live in
[System Defaults](../system-defaults/system-defaults.md). That tree contains
atoms, Badgey artifacts, refactor personas, agentic-corpus machinery, legacy
registered agents, and MCP surface packets.

## Workflows

JSON specs consumed by `bro orchestrate run`. Each one declares actors and nodes, and each node carries a typed `next` transition (`goto` / `branch` / `fork` / `terminal`); the daemon validates every transition target, actor reference, and reachability before any dispatch. JSON Schema at [`schema/workflow.schema.json`](../schema/workflow.schema.json).

| File | Purpose |
|---|---|
| [`workflows/e2e-smoke.json`](workflows/e2e-smoke.json) | Minimal two-turn durable-actor probe. Proves the pipeline end-to-end — same bro session across both nodes, prompt substitution between turns. |
| [`workflows/e2e-gated.json`](workflows/e2e-gated.json) | Gate packet + `branch` transition routing to one of two branches based on the gate's verdict. Unpicked cases never dispatch. |
| [`workflows/e2e-async-review.json`](workflows/e2e-async-review.json) | `fork` + `fire_and_forget` + `late_inject` — optimistic async-steering where a background reviewer's output joins a downstream node at its entry. |
| [`workflows/e2e-composition.json`](workflows/e2e-composition.json) | Sub-workflow as a node. Parent arc embeds a full workflow inline; the sub-arc opens its own `bbox_thread` and its output flows back into the parent. |
| [`workflows/e2e-policy.json`](workflows/e2e-policy.json) | Workflow-level policy packet — rule-packet applied to the arc's own state at every boundary. Halts, escalates, or warns based on the packet's verdict. "Advisor as packet" without any LLM in the decision loop. |
| [`workflows/e2e-ensemble-vote.json`](workflows/e2e-ensemble-vote.json) | Ensemble actor — concurrent team dispatch via `bro_broadcast`, member outputs aggregated into a labeled block, downstream synthesizer consumes the merged panel response. |
| [`workflows/e2e-self-audit.json`](workflows/e2e-self-audit.json) | Durable-session multi-phase critique arc with gate + choice + back-edge. The live-validation workflow that surfaced the subworkflow depth-threading bug. |
| [`workflows/e2e-atom-binding.json`](workflows/e2e-atom-binding.json) | Workflow-local atom binding. Invokes `atom:echo@v1` through the workflow engine; install `system-defaults/atoms/basic/echo.json` first. |
| [`workflows/optimistic.json`](workflows/optimistic.json) | Ensemble version of the async-review pattern — needs an ensemble team on the daemon. |
| [`workflows/blind.json`](workflows/blind.json) | Blind converge-then-execute pattern — needs an ensemble team on the daemon. |

See [Workflow Examples](workflows/workflow-examples.md) for the authoring
guide, the transition catalog, and the common traps.

## End-to-end demos

Operator-installable example arcs that wire a real upstream (Forgejo
container) into the engine. Each ships a `docker-compose.yaml`,
bootstrap, install, and run script.

| Path | Trigger | What it does |
|------|---------|--------------|
| [Keystone](keystone/keystone-example.md) | Forgejo `issues.opened` webhook (or poller alternative) | Implementer fixes the bug, opens a PR, reviewer ensemble votes, auto-merge on approval. The reference arc - most engine features touch it. |
| [Sastquatch](sastquatch/sastquatch-example.md) | Cron tick (calendar-driven; sibling primitive of webhook + poller) | Analyzer arc grounds itself by calling biofilter `sast_*` via `mcp_call`, executor picks a finding cluster, fixer arc applies the fix and opens a PR, ensemble reviewer votes, auto-merge. Exercises three primitives keystone does not: cron inlet, `mcp_call` op, persona-via-brofile dispatch. |
| [Whiteboard](whiteboard/whiteboard-example.md) | ADR-tagged issue webhook | Three specialist agents (security / performance / design) post stances blind to a phaser-style whiteboard, transition through phases, debate + vote. Facilitator synthesizes an ADR markdown file, opens a PR, auto-merges on consensus. Exercises the engine's whiteboard primitive, multi-round durable ensemble dispatch, and human-in-the-loop via the same `whiteboard_*` MCP surface specialists use. |
| [Slack](slack/slack-example.md) | Slack Socket Mode events | Sidecar normalizes Slack events into daemon webhooks, routing packets classify mentions/slash commands/reactions, and workflows dispatch Badgey or command arcs. |
| [System Events](system-events/system-events-example.md) | EventHub reaction examples | Reaction and packet artifacts for `task.completed` and Forgejo identity provisioning flows. |

## Agents

Drop-in subagent definitions for Claude Code. Install by copying into
`.claude/agents/` in your repo (project-scoped) or `~/.claude/agents/` (user-scoped).

| File | Purpose |
|---|---|
| [`agents/session-searcher.md`](agents/session-searcher.md) | Read-only subagent that searches indexed CLI transcripts across every provider / account on the host. Traces rules to their origin turn, summarizes sessions for takeover, audits what another agent did, samples topics across sessions. Scoped so it can only call `bbox_*` readers — never mutates. Use it to keep transcript digging out of your main context window. |

## Skills / Slash Commands

Workflow definitions for Claude Code, invocable as `/user:<name>` (user-scoped) or
`/project:<name>` (project-scoped). Install by copying into `~/.claude/commands/` or
`.claude/commands/` in your repo.

| File | Purpose |
|---|---|
| [`skills/crucible.md`](skills/crucible.md) | Orchestrator-led implementation workflow. Main-session Claude drives; a durable implementer bro (Opus 4.7 xhigh, held across rounds via `bro_resume`) carries mechanical code context the main session would otherwise lose to compaction; a continuous red-team ensemble (codex + gemini via `red_team` teamplate by default) reviews plan and work product with sustained per-member context. All coordinated through a `bbox_thread(kind="work_item")` and structured `bbox_note` signals (`dispute` / `surprise` / `blocked` / `followup` / `done`) so the orchestrator scans a signal trail instead of parsing prose. Use when context compartmentalization is the primary benefit and cross-provider consensus at bookends is worth the ceremony. |
| [`skills/takeover.md`](skills/takeover.md) | Take over driving an existing agent session. Composes **thread init** (find-or-open a `bbox_thread` with full scope context — handoff docs, source docs, prior takeover notes) and **thread run** (resume the target session via `bro_resume`, drive it iteratively against an authoritative scope checklist until halt conditions are met). Pairs well with the `session-searcher` agent for transcript recon. Use when an agent session stalled, got handed off, or was interrupted mid-work and you need to pick it up without losing scope. |
| [`skills/overmind.md`](skills/overmind.md) | Meta-orchestration — strategic Advisor layer one level above crucible. Main-session Claude holds the arc's charter and a durable **spine doc** (markdown + `bbox_decide` entries); a dispatched orchestrator bro runs crucible internally; ensemble + implementer sit under the orchestrator. Phase 0 is a takeover-style charter dialogue with the user — scope, halt conditions, exit conditions locked upfront. Orchestrator reports at phase boundaries only; Advisor updates the spine doc, records decisions, and steers via `bro_resume`. When the orchestrator compacts or drifts, Advisor retires it and spins a fresh one bootstrapped solely from the spine doc. Use on multi-phase arcs where strategic continuity needs to survive orchestrator compaction. See the recursion note below. |

### Recursion nuance (overmind)

Overmind is one of the rare legitimate uses of `bro_exec(..., allow_recursion=true)`.

By default, the daemon applies a mechanical recursion guard to every dispatched bro: the provider CLI gets filter args at argv construction (a concrete deny list for recursive `bro_*` orchestration/control tools, excluding `bro_report`, plus equivalent provider-specific filters) so dispatched bros cannot recurse into nested dispatches while still being able to publish progress telemetry. This is on for every `bro_exec` and `bro_resume` unless you explicitly opt out.

Overmind's orchestrator is itself a dispatcher — it runs crucible, which fans out an ensemble via `bro_broadcast` and manages a durable implementer via `bro_exec`/`bro_resume`. It needs the `bro_*` surface available. So the **orchestrator dispatch** uses `allow_recursion=true`:

```
bro_exec(
  bro="overmind-orchestrator",
  prompt=<brief>,
  project_dir=<cwd>,
  allow_recursion=true     // legitimate meta-orchestration exception
)
```

Everything *inside* the orchestrator — the ensemble members it broadcasts to, the implementer it exec's and resumes — gets the default guard like any other bro. Only the single orchestrator dispatch bypasses it.

If you adapt this pattern to your own skill, keep `allow_recursion=true` narrowly scoped — only to the one bro that legitimately needs to dispatch further. Apply it to ensemble members or implementers and you've given a code-writing or review-writing bro the ability to fan out uncontrolled; that's not the pattern.

## Adding your own

PRs welcome. Keep examples self-contained — no references to private / project-specific
tooling, no references to corpora blackbox doesn't actually index. User-facing
examples belong here; daemon-owned installable artifacts belong under
`system-defaults/`.
