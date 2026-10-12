---
name: bbox-bro-dispatch
description: Start, resume, supervise and review Blackbox bro tasks (agents dispatched to other models or providers). Use before the first bro_* call in a session and whenever you delegate work, run a review panel or a race, send a correction or follow-up to a dispatched agent, or decide whether a quiet task is dead.
---

# Dispatching and supervising bros

Blackbox executes agent turns and reports their state. You own sequencing, review, retries and deciding what happens next. Nothing is launched automatically when a task finishes. Using this skill does not itself authorize delegation: dispatch when the user or your brief calls for it.

## The rules that get broken

**`bro_exec` starts a fresh session. `bro_resume` continues one.** A second `bro_exec` has no memory of the first. Corrections, extensions and questions about work a bro already did go back to the session that did it: it still holds the context, and a fresh bro has to rediscover all of it. Start fresh only when the session is polluted, lost, or the new work is deliberately independent, such as a blind review.

**Record `taskId`, `sessionId` and provider from every `bro_exec` and `bro_resume`.** Waits take the task id; resumes take the session id and provider. Without the handles you cannot continue a session or tell your tasks from another caller's.

**One session runs one turn at a time.** `bro_resume` on a session whose previous task is still running is refused. Wait for that task to finish, or cancel it, before you resume.

**A wait that returns while the task is still running is a snapshot, not a death certificate.** Before calling a bro dead, cancelling it or replacing it, call `bro_status(task_id=..., tail=10)` and read what it is doing. It may be thinking, running tests, rate limited or waiting on a slow provider. Do the same when a result comes back empty or suspicious.

**Terminal does not mean successful.** A finished task may have failed or been cancelled, and `all_completed` or `any_completed` on a group says only that tasks ended. Read the deliverable against what you asked for.

**The dashboard is lookup, not ownership.** `bro_dashboard` shows every task on the daemon. Do not resume, cancel or prune a task another session created unless the user asks you to operate on that work.

**Pruning is not routine cleanup.** A finished task's session may still be worth resuming, and `bro_prune` is an operator operation on the `ops` surface. Leave tasks in place unless the operator asks for cleanup or has already authorized it.

## Procedure

1. **Define the work.** State the outcome, what is already known or ruled out, the files worth reading, the files and actions the bro may touch, how its work will be validated, and what its report must contain. The bro also receives its brofile's lens, the provider's defaults, its MCP configuration and the checkout's own instructions, but nothing from your conversation. Keep the user's model, account and provider constraints. Give an independent reviewer the task and the evidence, not the conclusion you expect.
2. **Check for an existing session.** If this work already has a recorded session, resume it.
3. **Choose one selector family for fresh work:** `bro` for a named brofile, `provider` for an ad hoc provider, or allocation fields such as `tier` and `pool_name`. Prefer a named brofile when one fits, since it fixes provider, account, model and lens. Inspect with `bro_brofile(action="list")` and `action="get"`; use `bro_providers` for available providers and usage advisories. Read [brofile-context.md](brofile-context.md) only when creating or changing a brofile, account routing or provider-default suppression.
4. **Dispatch** with an explicit absolute `cwd`. Pass `request_key` on any launch you might need to retry after a lost reply.

   ```text
   bro_exec(bro="<brofile>", cwd="<absolute checkout path>", prompt="<bounded task>", request_key="<unique turn key>")
   ```

5. **Wait in the shape of the work** (see Shapes). Long polls return as soon as the task finishes, so use the longest timeout your harness permits. The server caps one call at 840 seconds; a `waitCapped` or still-running snapshot means wait again.
6. **Check in** with `bro_status(task_id=..., tail=10)` occasionally, and after any timeout. Recover a truncated deliverable with `bro_status(task_id=..., detail="result")`, following the returned cursor; `detail="structured_exit"` returns omitted structured output as JSON text pages to concatenate before parsing.
7. **Review, then continue the same session** once its turn is terminal:

   ```text
   bro_resume(session_id="<recorded sessionId>", provider="<recorded provider>", prompt="<correction or next step>", request_key="<new turn key>")
   ```

8. **Report** the outcome, the evidence you reviewed, what was not validated, and the resumable handles. If the work needs a durable record across sessions, you open and link the `bbox_thread`; the bro's final answer is only its report.

## Shapes

- **One worker**: `bro_exec`, then `bro_wait(task_id=...)`.
- **Panel, ensemble or blind deliberation**: one `bro_exec` per member, each naming its brofile, then `bro_when_all(task_ids=[...])` (at most 64 ids). Compare the answers yourself. For another round, resume each member's own session after its task is terminal.
- **Race**: several `bro_exec` calls, then `bro_when_any(task_ids=[...])`. Cancel the others only when their work is clearly wasted.
- **Long-running**: `bro_exec`, occasional `bro_status`, and `bro_wait` when you need completion.

Use `bro_when_all` and `bro_when_any` instead of polling tasks one at a time. The wait tools only observe; none of them launches a review or a follow-up.

## Retrying, steering and cancelling

After a lost reply, retry with the same `request_key`, arguments and workspace: you get the original admission back, not a duplicate. Changed arguments under the same key are a conflict. An admission reported as `execution_unknown` means inspect the original task; do not launch again under a new key.

`bro_steer(task_id=..., prompt=...)` queues guidance into a running task without cancelling its turn. Prefer it to cancel-and-relaunch when the bro is on the right task but drifting.

Cancel only your own work, and only when it is stuck, deliberately abandoned or a losing race. Check `bro_status` first unless the user explicitly asked you to stop it.
