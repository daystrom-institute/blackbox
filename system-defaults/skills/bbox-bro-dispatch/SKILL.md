---
name: bbox-bro-dispatch
description: Start, resume, supervise and review Blackbox bro tasks (agents dispatched to other models or providers). Use before the first bro_* call in a session and whenever you delegate work, run a review panel or a race, send a correction or follow-up to a dispatched agent, or decide whether a quiet task is dead.
---

# Dispatching and supervising bros

Blackbox executes agent turns and reports their state. You own sequencing, review, retries and deciding what happens next. Nothing is launched automatically when a task finishes. Using this skill does not itself authorize delegation: dispatch when the user or your brief calls for it.

## The rules that get broken

**For Codex, background the wait and queue the next checkpoint.** When a worker can run until the next review without your attention, use `bro mcp call` with the appropriate blocking wait, then `codex queue` to wake the originating session. Inspect the returned snapshot, steer if needed, and rearm if still running. Do not keep the model active polling `bro_status` or the waiter process. Use direct blocking MCP waits when the callback path is unavailable or the work needs active observation. See the Codex recipe below; read `codex-queue` when available for host and session routing.

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

5. **Wait in the shape of the work** (see Shapes). For Codex, register the background wait and queued checkpoint below, then do independent work or yield. Choose a timeout for the next useful review; 300 seconds is a reasonable starting point. Each wait returns early when its completion condition is met and is capped at 840 seconds. Direct MCP waits are the fallback; use a blocking timeout, not rapid status polls.
6. **Review each checkpoint.** Read the snapshot and inspect progress with `bro_status(task_id=..., tail=10)` when needed to judge whether the work is on track. A timeout or `waitCapped` is not completion. If still running, steer if needed, register the next wait and yield again. Recover a truncated deliverable with `bro_status(task_id=..., detail="result")`, following the returned cursor; `detail="structured_exit"` returns omitted structured output as JSON text pages to concatenate before parsing.
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

## Codex: background wait, queue, review

Keep dispatch and steering as ordinary MCP calls. Run the wait on a host where a detached process survives the current turn. Check `bro mcp call --help` and `codex queue --help`; capture the originating thread UUID before launching. Preserve its Codex home and owning app-server route. A loaded, idle thread can process the queued message; a busy thread processes it later, and an unloaded thread retains it until resumed.

Run this recipe in Bash, replacing the task ID and selecting the same daemon and surface used for dispatch. The example uses the `ops` surface and inherits the daemon URL and credentials. If needed, add `--daemon-url` to the `bro` invocation and the verified `--remote` / `--remote-auth-token-env` options to `queue_route`.

```bash
parent_thread=${CODEX_THREAD_ID:?Use the originating session UUID}
bro_wait_dir=$(mktemp -d "${TMPDIR:-/tmp}/bro-wait.XXXXXX")
wait_tool=bro_wait
wait_args='{"task_id":"<task-id>","timeout_seconds":300}'
queue_route=()
nohup "$BASH" -c '
  set +e
  parent_thread=$1
  bro_wait_dir=$2
  wait_tool=$3
  wait_args=$4
  shift 4
  bro mcp call "$wait_tool" "$wait_args" --surface ops \
    >"$bro_wait_dir/result.json" 2>"$bro_wait_dir/wait.stderr"
  wait_exit=$?
  printf "%s\n" "$wait_exit" >"$bro_wait_dir/wait.exit"
  codex queue "$@" --thread "$parent_thread" --message \
    "$wait_tool returned (CLI exit $wait_exit). Read $bro_wait_dir/result.json and $bro_wait_dir/wait.stderr. Review progress and task outcomes; a timeout is only a checkpoint. Continue the existing task, steer if needed, and register another background wait if still running. Do not redispatch the work." \
    >"$bro_wait_dir/queue.stdout" 2>"$bro_wait_dir/queue.stderr"
  printf "%s\n" "$?" >"$bro_wait_dir/queue.exit"
' _ "$parent_thread" "$bro_wait_dir" "$wait_tool" "$wait_args" "${queue_route[@]}" \
  </dev/null >"$bro_wait_dir/waiter.log" 2>&1 &
bro_waiter_pid=$!
printf 'Waiter %s; checkpoint files: %s\n' "$bro_waiter_pid" "$bro_wait_dir"
```

For a group, use `wait_tool=bro_when_all` or `bro_when_any` and `wait_args='{"task_ids":["<task-id-1>","<task-id-2>"],"timeout_seconds":300}'`. Always name the tasks you own. When continuing an `any` wait after reviewing a finished task, wait on the remaining running tasks so the finished task does not cause immediate returns.

Confirm the waiter started, then yield with its result directory. The sequential commands queue a checkpoint even when the wait command fails; CLI exit zero alone does not mean the task finished. On wake, honor any later user cancellation or change of scope, inspect single-task status or `all_completed` / `any_completed`, then read the actual outcomes. If a task is terminal, review its result rather than rearming its wait. If the callback fails, its error is in `queue.stderr`; recover with a direct blocking MCP wait. Do not blindly repeat an enqueue whose delivery is uncertain.

## Retrying, steering and cancelling

After a lost reply, retry with the same `request_key`, arguments and workspace: you get the original admission back, not a duplicate. Changed arguments under the same key are a conflict. An admission reported as `execution_unknown` means inspect the original task; do not launch again under a new key.

`bro_steer(task_id=..., prompt=...)` queues guidance into a running task without cancelling its turn. Prefer it to cancel-and-relaunch when the bro is on the right task but drifting.

Cancel only your own work, and only when it is stuck, deliberately abandoned or a losing race. Check `bro_status` first unless the user explicitly asked you to stop it.
