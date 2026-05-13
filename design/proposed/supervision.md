# Supervision — task anomaly detection and advisor-ready evidence

Date: 2026-05-12
Status: design v2, aligned to the current tree.
Implementation companion: `design/proposed/supervision-impl.md`.

This document intentionally uses symbol/file anchors instead of numeric line
references. The previous version cited exact line numbers in `mod.rs`,
`roster.rs`, `schema.rs`, and `engine.rs`; those references drifted. The stable
anchors are the Rust item names listed below.

## Problem

Dispatched bros are currently observable mostly at completion or through
manual `bro_status` polling. During a long-running dispatch, the daemon stores
events and exposes tail snippets, but it does not classify repeated tool loops,
silence, compaction churn, or excessive token burn as structured supervision
evidence.

The missing toolset is:

- **Task-local counters** that update as provider events arrive.
- **Anomaly snapshots** that are machine-readable and packet-ready.
- **Deterministic alerts** with cooldowns, visible in `bro_status`.
- **Advisor-ready evidence** so team advisors, workflow advisor nodes, and
  follow-up drones can judge the bro using the same task/task-id trail.

## Current Anchors

Verified current symbols:

| Area | Current anchor | Status |
|---|---|---|
| Task state | `TaskInner` in `src/orchestration/mod.rs` | implemented |
| Task persistence | `PersistedTask`, `TaskStore::{persist,load}` | implemented |
| Streaming event hook | `spawn_task_reserved` streaming stdout reader | implemented |
| Bulk provider parse | `spawn_task_reserved` bulk stdout reader | implemented |
| Cancellation | `cancel_task` | implemented |
| Status output | `task_status_json`, `task_result_json` | implemented |
| Arc policy packets | `Workflow::policy_packet`, `WorkflowRunner::apply_policy_packet` | implemented |
| Node control flow | `NodeSpec`, `NodeTransition` | implemented |
| Advisor prompt/session | `build_team_advisor_init_prompt`, `dispatch_team_advisor_prompt` | implemented |
| Advisor checkpoint | `summarize_notes_for_tasks`, `build_advisor_checkpoint` | implemented |
| Advisor packet eval | `apply_advisor_packet` | implemented |
| Advisor resume | `maybe_resume_team_advisor` | implemented |

## Design

Supervision v1 is daemon-local and task-local. It does not introduce a new
workflow actor kind. It attaches supervision state to each `TaskInner`, updates
that state from the existing provider event stream, and exposes a compact
snapshot in status output.

```text
provider event
    -> provider.parse_event / parse_bulk_output
    -> supervision.observe_event / observe_bulk_sink
    -> supervision.snapshot()
    -> bro_status(...).supervision
```

The snapshot shape is flat enough for `bbox_apply` packets:

```json
{
  "enabled": true,
  "event_count": 42,
  "loop_hash_max": 6,
  "loop_hash_max_tool": "Edit",
  "seconds_since_last_event": 14,
  "compactions_in_window": 2,
  "total_input_tokens": 120000,
  "total_output_tokens": 9000,
  "token_burn_ratio": 2.4,
  "alerts": [
    {
      "kind": "loop",
      "severity": "red",
      "message": "same tool/input hash observed 6 times in the recent window"
    }
  ]
}
```

### Counters

The first implemented counters are deliberately mechanical:

| Counter | Signal | Amber | Red |
|---|---|---:|---:|
| Loop | repeated hash of `{tool_name,input}` in recent window | 3 | 6 |
| Stall | wall time since last observed event | 180s | 360s |
| Compaction churn | compaction-like events in 300s window | 2 | 4 |
| Token burn | cumulative tokens / configured baseline | 2.0x | 3.0x |

Streaming providers update all counters during execution. Bulk-parsed providers
get post-hoc token/event snapshots after stdout completes; they cannot have
true mid-flight loop or stall detection until they provide a streaming event
surface.

### Alerts

Alerts are records, not policy by themselves. They carry:

- `kind`: `loop | stall | compaction | token_burn`
- `severity`: `amber | red`
- `message`
- `at_ms`
- relevant measurements

Cooldown is per `(kind,severity)` and defaults to 60 seconds so one stuck task
does not spam status or notes.

### Packet Role

The daemon snapshot is packet-ready, but the first implementation does not
overload `Workflow::policy_packet`. Arc policy packets still run at workflow
node boundaries. Supervision snapshots belong to tasks and can be evaluated by
callers through `bbox_apply`, by advisor checkpoints, or by a future automatic
mid-dispatch packet evaluator.

This separation matters:

- `policy_packet` judges arc state at node boundaries.
- `supervision.snapshot` describes task behavior during or after dispatch.

### Advisor Integration

The existing team advisor pipeline remains valid. The supervision addition is
that task status envelopes now contain `supervision`, so advisor checkpoints and
workflow prompts can include deterministic evidence instead of prose guesses.

Future work can extract the team advisor helpers into a reusable workflow
advisor node, but v1 does not require that extraction to ship useful
supervision. A workflow can already dispatch an advisor actor and feed it
`${actor_results.<node>.supervision}` after the implementer completes.

### Out of Scope For V1

- Automatic daemon-side cancellation on red alerts.
- Daemon-spawned oracle co-sessions.
- Mid-flight workflow races between a running implementer and anomaly wait
  nodes.
- First-class typed acceptance criteria.

Those are still plausible next phases, but the immediate stale-doc problem was
that the previous design mixed implemented surfaces, aspirational surfaces, and
drifting line references. V1 makes the implemented surface concrete and
testable first.

## Acceptance

The design is implemented when:

- `TaskInner` carries persisted supervision state.
- Streaming provider events update loop, compaction, token, and last-event
  counters.
- `bro_status` and `bro_wait` task envelopes include a `supervision` object.
- Unit tests cover repeated tool-loop detection, stall snapshot computation,
  token burn ratio, cooldown behavior, and persisted task reload.
- The implementation plan in `design/proposed/supervision-impl.md` has no
  stale numeric line references and matches the shipped code.
