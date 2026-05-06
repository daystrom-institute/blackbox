Blocker: reference workflows read the wrong dispatch handle field. `bro_agent_dispatch` returns top-level `task_id` plus `session.task_id`; it does not return `taskId`. `chain.json`, `fan-out.json`, and `escalation.json` pass `${vars.*_handle.taskId}` into `bro_wait`, so runtime waits will get an unresolved/empty task id. Use `${vars.*_handle.task_id}` and add coverage for the actual handle shape.

Blocker: AS-C1 workflows still dispatch placeholder agent ids (`first-agent`, `second-agent`, `left-agent`, `right-agent`, `cheap-agent`, `expensive-agent`). AS-IaC1 expects the AS-C1 reference workflows to dispatch bundled reference agents. Wire them to `diff-narrator`, `code-reviewer`, and/or `badgey` with args matching those manifests.

Blocker: `fan-out.json` uses fork branches as `bro_wait` hook nodes, but fork branches are dispatched through `dispatch_fire_and_forget`; branch `on_enter` hooks do not execute. `Aggregate.wait_for` joins the branch actor tasks and records `outputs.WaitLeft` / `outputs.WaitRight`, not `vars.left_output` / `vars.right_output`. Reshape fan-out to match engine semantics or avoid `Fork` for MCP-hook waits.

Issue: `code-reviewer.json` leaves optional `{{context_refs}}` in the prompt when valid args omit `context_refs`. Either remove that placeholder, make the arg required, or add template/default support before using it in a reference agent.

Issue: the new noop dispatch coverage in `validate.rs` calls the adapter directly, not the `bro_agent_dispatch` tool path. Extend/add a server-level test that dispatches a reference agent through `bro_agent_dispatch` with a noop adapter and asserts the returned handle shape.
