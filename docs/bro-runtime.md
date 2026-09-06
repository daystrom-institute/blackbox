# Bro Runtime

`bro` is the runtime for dispatching and supervising provider sessions. Workflows
use it under the hood, but it is also useful directly when you want one agent,
a team, or a few parallel tasks without writing a workflow spec.

Use workflows for durable state machines. Use atoms for reusable capabilities.
Use `bro_*` when you are operating live tasks.

## Start, Resume, Wait

Start a fresh task:

```text
bro_exec(
  bro="executor",
  prompt="audit the tail module for stale assumptions",
  cwd="/repo/x"
)
```

Resume the same provider session:

```text
bro_resume(
  session_id="<session-id>",
  prompt="now implement the smallest safe patch"
)
```

Wait for completion:

```text
bro_wait(task_id="<task-id>", timeout_secs=3600)
```

Do not resume a session while its previous task is still running. Check with
`bro_status` or wait/cancel the active task first.

## Fanout

Broadcast to a team:

```text
bro_broadcast(team="reviewers", prompt="review this diff for correctness")
bro_when_all(task_ids=["task-a", "task-b"], timeout_secs=3600)
```

Use `bro_when_any` for races where the first useful answer wins. The losers keep
running until you cancel them.

## Status And Reporting

Use non-blocking reads when you are supervising:

```text
bro_status(task_id="<task-id>", tail=20)
bro_dashboard(team="bbox-red")
```

Dispatched agents and workflow hooks should report milestones:

```text
bro_report(
  task_id="<task-id>",
  message="tests are running",
  needs="none"
)
```

`bro_dashboard` is much more useful when tasks report what they are doing.

## Cancel And Prune

Cancel precisely:

```text
bro_cancel(task_id="<task-id>")
```

Do not cancel by port or broad process pattern. If you did not start the task,
inspect it first.

Prune old terminal task records:

```text
bro_prune(status="failed", dry_run=true)
bro_prune(status="failed", older_than_hours=72)
```

Running tasks are not pruned.

## Brofiles

A brofile is a reusable provider/persona/lens/filter bundle:

```text
bro_brofile(action="list")
bro_brofile(action="get", name="rust-refactor-persona")
```

List before create. Brofiles are often installed through the artifact catalog
from `system-defaults/brofiles/`.

## Teams

Teams are named sets of brofiles:

```text
bro_team(action="list")
bro_team(action="create", template="red-team", name="bbox-red", project_dir="/repo/x")
```

`list`, `list_templates`, and `roster` return summary pages, with default
`limit=20` (clamped to 1..100), a response byte cap, and `next_offset` for
continuation. `list` and `list_templates` accept an exact `name` filter.
`project_dir` filters live teams by their stored association; it is not a file
handle or evidence of daemon checkout access. Roster members carry stored brofile
names and session/task identities, without resolving configuration from a checkout.

```text
bro_team(action="list_templates")
bro_team(action="roster", name="bbox-red", limit=20)
bro_team(action="get_template", name="red-team")
bro_team(action="get", name="bbox-red")
```

`get_template` exposes the full template; `get` exposes the live team's stored
configuration, including advisor overrides and task history. These exact reads
return JSON text under `body`, even for small records. Concatenate `body.text`
while following `body.next_cursor` unchanged, then parse the resulting JSON.
Each body page is at most 4096 encoded bytes; `body_limit` can request a smaller
page. Changed records or selectors reject stale cursors; restart without one.
Summary pages do not hide full advisor charters, context, or brofile lenses in
structured content. Malformed stored records report errors instead of disappearing.

`scope` applies only to template actions and accepts `global` (default) or
`project`. Project template discovery requires an explicit absolute owner-host
`project_dir` in legacy bridge mode. Catalog mode refuses this legacy
`.bro/teamplates` read because it has no remote source lane. Inspect those files
with the owning checkout's file tools, or discover daemon-owned templates using
`scope="global"` without `project_dir`. No discovery path falls back to the
daemon's current directory.

Use teams for ensemble review, blind comparison, provider races, or repeated
panels. If the control flow has gates, retries, waits, or cleanup, write a
workflow instead of hand-driving the team.

## Provider Catalog

Check available providers and model/effort settings:

```text
bro_providers()
```

Provider binaries can be overridden with env vars such as `CLAUDE_BIN`,

## MCP Servers And Filters

`bro_mcp` manages MCP servers and dispatch-time tool filters:

```text
bro_mcp(action="list")
bro_mcp(action="add", name="blackbox-readonly", url="http://127.0.0.1:7264/mcp?surface=readonly")
bro_mcp(action="disallow", pattern="mcp__blackbox__bro_*", scope="global")
```

The recursion guard is mechanical. Dispatch-capable providers get deny arguments
at process launch so ordinary dispatched agents cannot recursively spawn more
agents. `bro_report` stays allowed because it is telemetry.

Only use `allow_recursion=true` for a bro whose job is explicitly to orchestrate
other bros.

## When To Move Up A Layer

| Situation | Better tool |
|---|---|
| Same prompt/persona repeated across sessions | Brofile or agent |
| Same capability with schemas and effect limits | Atom |
| Multi-step state machine with gates or waits | Workflow |
| Live one-off dispatch, review panel, or provider race | Bro runtime |
