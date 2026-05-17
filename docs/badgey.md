# Badgey

Badgey is the slow, evidence-carrying consultant for the blackbox corpus.

Give it a project scope and a hard question. It walks the typed graph, traces
provenance, reads related sessions and threads, and returns claims tied to
round-trippable entity refs. When it sees a reusable pattern, it can draft a
proposal: a knowledge entry, packet, workflow, brofile, agent, or atom candidate.

Badgey is not a code editor and not a direct knowledge-store CRUD shim.
It synthesizes evidence-grounded proposals; a human (or the
apply-proposal arc) executes them.

## Core concept: instances and proposals

A **Badgey instance** (`badgey_id`) is a persistent consultation session
with a work-item thread of record. Turns are serialized per instance, so
concurrent callers queue rather than corrupt the provider session.

Each turn can emit one or more **proposals**: structured action items
(artifact promotions, redispatch tasks, knowledge entries, workflow
installs) the instance believes should happen. Proposals sit in `pending`
state until someone applies or rejects them.

```
badgey_exec -> badgey_id (instance created, thread opened)
badgey_resume / badgey_ask -> turns (evidence walk, proposals emitted)
badgey_proposals_list -> read pending proposals
badgey_apply_proposal -> apply one
badgey_dismiss -> close instance, resolve thread
```

## Tools

### Starting and resuming

**`badgey_exec`** - Start a new Badgey instance for a project scope.
Returns `badgey_id`, provider session, task, and thread-of-record IDs.

```
badgey_exec(project_dir="/repo/x", brief="help me understand the graph edge schema")
```

**`badgey_resume`** - Send a follow-up turn to an existing instance.
Keeps thread-of-record context. Use for all follow-ups; never call
`badgey_exec` again for continuity.

```
badgey_resume(badgey_id="bg-0123abcd-4567ef89", prompt="trace the origin of this decision")
```

**`badgey_ask`** - Question-shaped alias for `badgey_resume`. Same
semantics; `question` field instead of `prompt`.

```
badgey_ask(badgey_id="bg-0123abcd-4567ef89", question="what should I inspect next?")
```

### Inspection

**`badgey_status`** - Inspect one instance: queue status, proposals,
provider/session/thread bindings. Without `badgey_id`, lists active
instances.

```
badgey_status(badgey_id="bg-0123abcd-4567ef89")
```

**`badgey_list`** - List instances and their thread/session bindings.
Pass `include_dismissed=true` for audit view.

### Scout mode

**`badgey_scout`** - Ask Badgey to decompose one question into focused
sub-charters and fan them out. The wrapper dispatches emitted scout
actions without exposing `bro_exec` to the Badgey provider session.

```
badgey_scout(badgey_id="bg-0123abcd-4567ef89", charter="compare these two graph paths")
```

**`badgey_collect`** - Read scout/sub-bro events for an instance: see
whether scout work is still walking or has produced dispatch records.

### Proposals

**`badgey_proposals_list`** - Read full proposal records (id, kind,
state, draft fields, created_at, events). Pass `since` to scope to a
specific synthesis turn's output.

```
badgey_proposals_list(
  badgey_id="bg-deadbeef-cafef00d",
  since="2026-05-07T08:00:00Z",
  only_pending=true
)
```

**`badgey_apply_proposal`** - One-shot apply: state-machine transition
`Pending/Failed -> Applying`, kind-specific dispatch, record
`applied_task_id`, transition `Applying -> Applied | Failed`.

```
badgey_apply_proposal(badgey_id="bg-deadbeef-cafef00d", proposal_id="P-3")
```

For the Slack-reaction flow, use the split pair instead (see below).

**`badgey_dismiss`** - Close an instance: drain queued turns, write a
dismiss event, resolve the thread of record. New resumes after dismissal
fail with `instance_dismissed`.

```
badgey_dismiss(badgey_id="bg-0123abcd-4567ef89", reason="work complete")
```

### Triage helpers

**`badgey_triage_inbox`** - Produce a proposal-sheet for stale/open
work in a scope. Morning-brief starting point; concrete actions still
go through the proposal gate.

```
badgey_triage_inbox(scope="/repo/x")
```

**`badgey_close_loops`** - Classify dispatched tasks without done
notes. Reports suspected completions, crashes, or stalls; does not
write `kind=done` on behalf of executors.

```
badgey_close_loops(window_days=14, project_dir="/repo/x")
```

## Proposal kinds

| Kind | What it does |
|---|---|
| `artifact_promotion` | Promote a proposed artifact into the installed catalog via `bbox_artifact_install` |
| `redispatch_task` | Spawn a privileged task with the proposal's prompt |
| `workflow_install` | Install a workflow via `bbox_artifact_install` |
| `agent_install` | Install an agent via `bbox_artifact_install` |
| `packet_install` | Install a rule-packet via `bbox_artifact_install` |

## Slack-driven daily brief

Badgey can post a daily per-channel triage brief to Slack. The full
pipeline:

```
daily cron tick
  → routing-packet: badgey/cron-routing
    → start_arc: badgey-triage-fanout-arc
        ├── ListBindings
        └── ForeachBinding (parallelism=2)
              └── badgey-triage-channel-arc  (per channel)
                    ├── EnsureInstance    ← badgey_ensure_for_channel
                    ├── TriageTurn        ← badgey_resume (corpus walk)
                    ├── ForeachScout      ← badgey-scout-arc (parallelism=3)
                    ├── SynthesisTurn     ← badgey_resume (proposals)
                    ├── ListProposals     ← badgey_proposals_list
                    └── ForeachPostProposal
                          └── badgey-slack-emit-proposal-arc
                                ├── Render
                                ├── Post    ← chat.postMessage
                                └── RecordLink ← bro_slack_link_record
```

Inbound signals:
- `:white_check_mark:` reaction → `proposal-approved` signal →
  `bro_slack_link_lookup` resolves `(BadgeyId, proposal_id)` →
  `badgey_apply_proposal`
- In-thread reply → `proposal-clarify` signal → `bro_resume` on the
  originating authoring session

### Slack-specific tools

**`badgey_ensure_for_channel`** - Get-or-create the system Badgey
instance for a channel binding. Idempotent; returns `created=false` for
an already-active instance.

```
badgey_ensure_for_channel(team_id="T0123ABCD", channel_id="C0123XYZ")
```

**`bro_slack_bind`** - Bind a Slack channel to a project. Required
before `badgey_ensure_for_channel` works; `bro_slack_bind(action=list)`
to inspect.

```
bro_slack_bind(
  action="bind",
  team_id="T0123ABCD",
  channel_id="C0123XYZ",
  channel_name="my-project",
  project="/home/me/repos/my-project"
)
```

**`bro_slack_link_record`** / **`bro_slack_link_lookup`** - Record and
resolve the mapping between a posted Slack message timestamp and its
`(BadgeyId, proposal_id)` pair. Used by the apply/refine arcs.

### Split apply path (for Slack workflows)

The workflow engine tracks the dispatched bro as a named actor node
when you use the split pair rather than the one-shot `badgey_apply_proposal`:

1. **`badgey_proposal_begin_apply`** - Transition `Pending|Failed → Applying`,
   return dispatch parameters without spawning. The workflow caller
   dispatches via an actor node or `bbox_artifact_install` mcp_call.

2. **`badgey_proposal_complete_apply`** - Given the dispatched work's
   outcome, transition `Applying → Applied | Failed` and write the audit
   decision.

Skip the complete call entirely on `already_applied` / `rejected`
short-circuit paths.

## Install

Install the Badgey system-default artifacts from `system-defaults/badgey/`:

```
# Brofiles and agents
bbox_artifact_install(kind="brofile", source="system-defaults/badgey/brofiles/badgey-persona.json")
bbox_artifact_install(kind="brofile", source="system-defaults/badgey/brofiles/badgey-scout-persona.json")
bbox_artifact_install(kind="agent", source="system-defaults/badgey/agents/badgey.json")
bbox_artifact_install(kind="agent", source="system-defaults/badgey/agents/badgey-scout.json")

# Workflows
bbox_artifact_install(kind="workflow", source="system-defaults/badgey/workflows/badgey-scout-arc.json")
bbox_artifact_install(kind="workflow", source="system-defaults/badgey/workflows/badgey-slack-emit-proposal-arc.json")
bbox_artifact_install(kind="workflow", source="system-defaults/badgey/workflows/badgey-triage-channel-arc.json")
bbox_artifact_install(kind="workflow", source="system-defaults/badgey/workflows/badgey-triage-fanout-arc.json")

# Routing packet
bbox_artifact_install(kind="packet", source="system-defaults/badgey/packets/badgey-cron-routing.json")

# Cron
bbox_artifact_install(kind="cron", source="system-defaults/badgey/crons/badgey-triage-daily.json")
```

Then bind your channels:

```
bbox_project_register(path="/home/me/repos/my-project")
bro_slack_bind(
  action="bind",
  team_id="T0123ABCD",
  channel_id="C0123XYZ",
  channel_name="my-project",
  project="/home/me/repos/my-project"
)
```

Prerequisites: daemon + `bro-slack.service` running, `SLACK_BOT_TOKEN`
in daemon environment.

## Anti-patterns

- Do not use for code generation, edits, refactors, or bug fixes -
  Badgey investigates and proposes, not implements.
- Do not use for direct `bbox_*` primitive lookups the caller can run
  themselves.
- Do not auto-apply proposals without reading them - the apply path
  requires explicit approval, deliberate or via the Slack-reaction hook.
- Do not use as a write-through proxy for `bbox_remember` /
  `bbox_decide` / `bbox_learn` - those are direct surfaces; Badgey
  synthesizes evidence-grounded proposals.
