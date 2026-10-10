# Bro Runtime

`bro` launches, observes, and controls provider sessions. Callers compose those
operations in their own code; Blackbox owns task identity, provider routing,
worker transport, and execution recovery. Waits never choose follow-up work.
Dispatch is ad hoc (`provider`) or through a named brofile (`bro`).

## Retrying an uncertain dispatch

Pass a unique `request_key` to `bro_exec` or `bro_resume` before the first
attempt. Retrying the same key and inputs returns the original task identity;
changed inputs or bound workspace refuse reuse. Use a new key for an intentional
new task or continuation. Keys have no automatic expiry, even after task details
age out. Calls without a key retain their existing behavior and can duplicate
work when retried.

The durable claim precedes worker admission. A crash or cancelled call between
that claim and its receipt can leave `admission_incomplete` with
`execution_unknown=true`. Inspect its `taskId` with `bro_status`; absence of a
retained task is not proof that the worker never ran. Retrying the same key never
launches again. Blackbox does not promise exactly-once worker execution or
silently recover an unknown admission by starting another worker.

Journal records hold request hashes, identities, and minimal outcomes, not raw
prompts or credentials. An unavailable or corrupt journal refuses keyed admission.

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
  provider="brodex",
  prompt="now implement the smallest safe patch"
)
```

Wait for completion:

```text
bro_wait(task_id="<task-id>", timeout_seconds=60)
```

Do not resume a session while its previous task is still running. Check with
`bro_status` or wait/cancel the active task first.

## Fanout

Dispatch one named brofile per role, then wait on the returned task ids:

```text
bro_exec(bro="correctness-reviewer", prompt="review this diff for correctness")
bro_exec(bro="security-reviewer", prompt="review this diff for security")
bro_when_all(task_ids=["<task-id-1>", "<task-id-2>"], timeout_seconds=60)
```

Later rounds resume each recorded session with
`bro_resume(session_id=..., provider=...)`.

Use `bro_when_any` for races where the first useful answer wins. The losers keep
running until you cancel them.

## Status

Use non-blocking reads when you are supervising:

```text
bro_status(task_id="<task-id>", tail=20)
bro_dashboard(status="running")
```

## Cancel And Prune

Cancel precisely:

```text
bro_cancel(task_id="<task-id>")
```

Do not cancel by port or broad process pattern. If you did not start the task,
inspect it first.

Prune old terminal task records with the `ops`-surface `bro_prune` (for
example `bro mcp call bro_prune '{"status":"failed","dry_run":true}' --surface ops`):

```text
bro_prune(status="failed", dry_run=true)
bro_prune(status="failed", older_than_hours=72)
```

Running tasks are not pruned.

## Brofiles

A brofile is a reusable provider/persona/lens/filter bundle:

```text
bro_brofile(action="list")
bro_brofile(action="get", name="java-refactor-persona")
```

List before create. Brofiles are often installed through the artifact catalog
from `system-defaults/brofiles/`.

## Waits

`bro_wait`, `bro_when_all`, and `bro_when_any` only observe existing tasks.
Neither completion nor timeout launches a continuation. Dispatch a review
explicitly with `bro_exec` or `bro_resume` when the caller needs one. The caller
owns gates, retries, subsequent work, and cleanup in ordinary code, using
explicit bro control operations.

## Provider Catalog

Check available providers and model/effort settings:

```text
bro_providers()
```

Provider binaries can be overridden with env vars such as `CLAUDE_BIN`,

## MCP Servers And Filters

`bro_mcp`, on the `ops` surface, manages MCP servers and dispatch-time tool
filters:

```text
bro_mcp(action="list")
bro_mcp(action="add", name="blackbox-readonly", url="http://127.0.0.1:7264/mcp?surface=readonly")
bro_mcp(action="disallow", pattern="mcp__blackbox__bro_*", scope="global")
```

The recursion guard is mechanical. Dispatch-capable providers get deny arguments
at process launch so ordinary dispatched agents cannot recursively spawn more
agents.

Only use `allow_recursion=true` for a bro whose job is explicitly to orchestrate
other bros.

## When To Move Up A Layer

| Situation | Better tool |
|---|---|
| Same prompt/persona repeated across sessions | Brofile |
| Multi-step sequencing with gates or waits | Caller-owned composition of bro calls |
| Live one-off dispatch, review panel, or provider race | Bro runtime |
