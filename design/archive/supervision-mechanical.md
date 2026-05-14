# Mechanical supervision telemetry

Date: 2026-05-14
Status: archived as implemented substrate.

This document records the mechanical supervision layer that exists today. It is
archived because it is not the open design problem anymore; it is substrate for
the atom-era classifier/advisor design in `design/partial/supervision.md`.

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
- `AlertKind`: `loop`, `stall`, `compaction`, `token_burn`
- `AlertSeverity`: `amber`, `red`

Default thresholds:

| Signal | Amber | Red |
|---|---:|---:|
| Consecutive identical tool/input hash | 3 | 6 |
| Stall since last event | 180s | 360s |
| Compaction markers in 300s window | 2 | 4 |
| Token burn ratio versus seeded baseline | 2.0x | 3.0x |

Alert cooldown is 60 seconds per kind/severity. Recent tool hashes and alerts
are bounded.

## 3. Event observation

`src/orchestration/mod.rs` wires the telemetry into task execution:

- streaming providers parse each NDJSON event with `provider.parse_event`
  before `inner.supervision.observe_event(...)`
- bulk-output providers parse at completion and call
  `inner.supervision.observe_bulk_sink(...)`
- task result/status/timeout rendering calls `observe_stall(...)` before
  writing the response snapshot

This means supervision snapshots can show stall state even when no new provider
event has arrived.

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
- stall amber/red snapshots
- token burn with and without baseline
- old task-record serde defaults
- response-optimized green snapshots
- full snapshots on alerts/thresholds
- OpenCode-style tool extraction
- structural compaction marker detection

Future supervision docs should reference this layer as implemented telemetry,
not as the classifier/advisor design.
