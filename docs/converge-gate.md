# Convergence drain gate

Converging or cycling a blackbox daemon while operator sessions are mid
bro-wave sandbags live orchestration state: running bro tasks, `bro_wait` /
`bro_when_all` / `bro_when_any` long-polls, workflow arcs in flight, and
sessions still writing threads, notes, and knowledge. The protected resource
is that live state, not build capacity. This page describes the daemon-side
probe and drain mode, and the wrapper script an external converge path calls.

Code: `src/server/drain.rs`. Wrapper: `scripts/converge-gate`.

## Intended call order

```text
1. scripts/converge-gate --drain --timeout 900   # gate: set maintenance pending,
                                                 # wait (bounded) for in-flight
                                                 # work, exit 0 only when quiet
2. <converge / restart / redeploy the daemon>    # only if step 1 exited 0
3. scripts/converge-gate --clear                 # clear drain
```

Step 3 is not optional: the drain marker is persisted and deliberately
survives a daemon restart, so the freshly converged daemon boots draining
(and logs a warning naming the clear path) until it runs. A plain
`scripts/converge-gate` with no flags is the read-only check: it probes,
prints who would be sandbagged, and exits nonzero if orchestration is
active. It never toggles drain.

Exit codes: `0` quiescent (safe to converge), `1` orchestration active,
`2` daemon unreachable or bad probe, `3` `--drain` timed out while still
active (drain is left set unless `--release-on-timeout`).

## Probe: `GET /admin/orchestration-activity`

Loopback-only admin route (same trust model as `/admin/runtime-metrics`).
Cheap: in-memory reads only, no I/O. Query: `writes_window_minutes`
(default 10, clamped to 24h).

```bash
curl -s "http://127.0.0.1:${BBOX_PORT:-7264}/admin/orchestration-activity?writes_window_minutes=10"
```

Payload shape:

| Field | Meaning |
|---|---|
| `quiescent` | `true` iff no running tasks, no arcs in flight, no long-poll waiters |
| `quiescent_scope` | Always `"tasks,arcs,waiters"`; states what `quiescent` covers |
| `recent_writes_total` | Thread/note/knowledge writes inside the window (same level as `quiescent`, deliberately not folded into it) |
| `drain` | `{draining, set_at, reason, set_by, marker_path}` |
| `running_tasks` | `count` + `tasks[]` (`task_id`, `session_id`, `provider`, `origin`, `bro`, `agent`, `name`, `cwd`, `started_at_ms`, `age_secs`) |
| `workflows_in_flight` | `count` + `arcs[]` (`arc_id`, `arc_thread_id`, `workflow`, `status`, `current_node`, `in_flight_nodes`, `started_at`, `age_secs`) |
| `long_poll_waiters` | `count` + `waiters[]` (`id`, `tool`, `task_ids`, `age_secs`) |
| `recent_writes` | `window_minutes`, `total`, `threads[]`, `notes[]`, `knowledge[]` |

Write recency is DELIBERATELY excluded from `quiescent`. A chatty operator
session cannot be drained, only observed, so the daemon reports it and the
wrapper enforces the policy: writes inside the window block by default,
`--writes-window N` tunes it, `--writes-window 0` disables it. A raw consumer
that skips the wrapper must read `recent_writes_total` alongside `quiescent`.

Running arcs are read from the engine's live cancel-token map (registered at
arc start, dropped at terminus) and decorated from the `running_arcs`
snapshot; long-poll waiters are RAII-registered inside `bro_wait`,
`bro_when_all`, and `bro_when_any` so a client disconnect unregisters them.

## Drain: `GET|POST /admin/drain`

```bash
curl -s http://127.0.0.1:7264/admin/drain
curl -s -X POST -H 'content-type: application/json' \
  -d '{"draining": true, "reason": "converge 1.2.3", "set_by": "me"}' \
  http://127.0.0.1:7264/admin/drain
curl -s -X POST -H 'content-type: application/json' \
  -d '{"draining": false}' http://127.0.0.1:7264/admin/drain
```

While draining:

- Fresh dispatches are refused with a retryable error whose text starts with
  `error.maintenance_pending` and names the window (`set_at`, `reason`) plus
  `retryable=true`. Covered: `bro_exec` (including the cockpit control
  handler), `bro_agent_dispatch`, `atom_invoke`, cron/webhook-originated
  dispatches, and top-level workflow arc starts (`bro_orchestrate_run`,
  `/orchestrate`, routed `start_arc`).
- In-flight work continues: an already-running arc's nodes, nested
  sub-workflows and fanout children, and auto-supervision atom invocations
  are exempt (dispatch origins `workflow` and `atom` bypass the gate;
  external atom invocations are gated at the `atom_invoke` tool instead).
  One known edge: a workflow-implemented atom invoked mid-drain by in-flight
  work starts a fresh top-level arc and is refused; profile-backed atoms are
  not.
- `bro_resume` of existing sessions stays allowed.
- Both toggles are idempotent. Setting persists `<BRO_HOME>/maintenance-drain.json`
  (atomic rename) BEFORE the flag flips in memory; clearing removes the
  marker. The route answers only after the marker write.

Startup behavior is explicit: if the marker exists (or is unreadable) at boot
the daemon starts draining and logs a `blackbox::drain` warning; a corrupt
marker fails closed. There is no auto-clear. Clear with `POST /admin/drain
{"draining": false}` or `scripts/converge-gate --clear`; deleting the marker
file by hand only takes effect at the next daemon start.

## Wrapper reference

`scripts/converge-gate [--drain|--clear|--status] [--timeout S] [--interval S]
[--writes-window MIN] [--reason TEXT] [--release-on-timeout] [--json]
[--url URL]`. Daemon URL defaults to `$BBOX_URL`, else
`http://127.0.0.1:${BBOX_PORT:-7264}`. Dependencies: bash, curl, python3
(no jq). The header of the script carries the same reference.
