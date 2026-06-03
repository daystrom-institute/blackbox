---
title: "Mechanical supervision telemetry"
kind: design
lifecycle: archived
corpus: blackbox-design
topic:
  - orchestration
  - supervision
date: 2026-05-14
status: "archived as implemented substrate"
brief: "Records the implemented mechanical telemetry layer used by atom-era classifier and advisor supervision."
---

# Mechanical supervision telemetry

This document records the mechanical supervision layer that exists today. It is
archived because it is not the open design problem anymore; it is substrate for
the atom-era classifier/advisor design in `supervision.md`.

## 1. Scope

Mechanical supervision is task-local telemetry. It observes provider events and
bulk provider output, records simple anomaly measurements, and surfaces those
measurements in task status/result JSON.

It does not:

- cancel tasks
- steer primary atoms
- invoke packet gates
- invoke classifier atoms
- invoke advisor atoms
- choose recovery tiers
- decide whether work is acceptable

Those behaviors belong to optional orchestration above the telemetry layer.

## 2. Implemented code

`src/orchestration/supervision.rs` defines:

- `SupervisionConfig`
- `SupervisionState`
- `SupervisionAlert`
- `AlertKind`: `loop`, `compaction`, `token_burn` (the `stall` variant is
  retained only for backward-compatible deserialization of persisted state; it
  is no longer emitted)
- `AlertSeverity`: `amber`, `red`

Default thresholds:

| Signal | Amber | Red |
|---|---:|---:|
| Consecutive identical tool/input hash | 3 | 6 |
| Compaction markers in 300s window | 2 | 4 |
| Token burn ratio versus seeded baseline | 2.0x | 3.0x |

Idle is **not** an alert. Inferring "productively busy vs. wedged" from elapsed
time is unrecoverable once an agent chains commands (`build && test | tail` is
one open tool call of unbounded duration), so supervision does not classify it.
Instead, a single configurable threshold (`stall_notice_ms`, default 180s)
controls when the snapshot surfaces a neutral `idle_seconds` / `idle_notice`
fact. It carries no severity, never appears in `alerts`, and never flips a task
out of green.

The one honest disambiguation the snapshot does make is `tool_running`, a
tri-state derived from the event sequence (not a timing classifier):

- `true` — the last observed event dispatched a tool whose result has not yet
  arrived (idle here means "blocked on a child process");
- `false` — the last observed event was anything else (idle here means the
  model itself is quiet);
- `null` — no streaming visibility. Bulk-output providers parse only at
  completion, so "is a tool running right now" is unknowable; we report unknown
  rather than fake a `false`.

`tool_running` is always present in the full snapshot and rides alongside
`ok=true` in the response-optimized snapshot whenever the idle notice fires, so
an orchestrator can separate "blocked on a tool" from "model is quiet" without
re-investigating. The `idle_notice` string is labelled with this state, e.g.
`no activity for 185s (tool running)`.

Alert cooldown is 60 seconds per kind/severity. Recent tool hashes and alerts
are bounded.

## 3. Event observation

`src/orchestration/mod.rs` wires the telemetry into task execution:

- streaming providers parse each NDJSON event with `provider.parse_event`
  before `inner.supervision.observe_event(...)`
- bulk-output providers parse at completion and call
  `inner.supervision.observe_bulk_sink(...)`
- task result/status/timeout rendering reads the idle fact directly from
  `last_event_at_ms` at snapshot time — no separate `observe_stall` pre-call

This means supervision snapshots report idle duration even when no new provider
event has arrived, computed from the last event timestamp rather than tracked as
stateful alert.

## 4. Response shape

Green status is response-optimized:

```json
{
  "ok": true,
  "event_count": 12
}
```

When any alert or threshold condition is present, the full snapshot is returned:

```json
{
  "enabled": true,
  "event_count": 12,
  "loop_hash_max": 6,
  "loop_hash_max_tool": "Edit",
  "seconds_since_last_event": 42,
  "compactions_in_window": 0,
  "total_input_tokens": 10000,
  "total_output_tokens": 1200,
  "token_baseline": null,
  "alerts": []
}
```

Machine consumers that need the full shape should call the full snapshot path
rather than relying on response-optimized task result JSON.

## 5. Known limits

- Loop detection is based on consecutive identical tool/input hashes, not
  semantic equivalence.
- Token burn alerts require a seeded baseline; without a baseline the ratio is
  absent.
- Bulk-output providers cannot provide mid-flight semantic observation through
  this mechanism.
- Mechanical signals are evidence, not policy.

## 6. Test coverage

`src/orchestration/supervision.rs` contains unit tests for:

- repeated tool events emitting loop amber/red
- interleaved repeated tool calls not counting as a loop
- duplicate tool shapes in one event counting once
- alert cooldown
- neutral idle notice surfaced past threshold (never an alert, never breaks green)
- `tool_running` tri-state: flips on tool dispatch, off on result, `null` for
  bulk-only providers
- token burn with and without baseline
- old task-record serde defaults
- response-optimized green snapshots
- full snapshots on alerts/thresholds
- structural compaction marker detection

Future supervision docs should reference this layer as implemented telemetry,
not as the classifier/advisor design.
