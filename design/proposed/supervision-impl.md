# Supervision — Implementation Plan

Date: 2026-05-12
Companion: `design/proposed/supervision.md`.
Status: v2 implementation plan.

This plan is scoped to the v1 supervision toolset that can be fully shipped in
the current codebase without inventing a new workflow race primitive or
daemon-side oracle runner.

## Rule For References

Do not cite exact source line numbers in this document. Use symbols:
`TaskInner`, `PersistedTask`, `spawn_task_reserved`, `task_status_json`,
`task_result_json`, `cancel_task`, `Workflow::policy_packet`,
`WorkflowRunner::apply_policy_packet`, `NodeSpec`, and the advisor helper functions in
`src/tools/roster.rs`.

## Deliverables

1. New `src/orchestration/supervision.rs`.
2. `TaskInner` and `PersistedTask` carry `SupervisionState`.
3. Provider streaming events update supervision state from
   `spawn_task_reserved`.
4. Bulk providers update a post-hoc snapshot from the final `EventSink`.
5. `task_status_json`, `task_result_json`, and timeout snapshots expose
   `supervision`.
6. Unit tests cover detection and persistence.
7. `design/proposed/supervision.md` and this file describe the shipped scope.

## Data Model

Create `src/orchestration/supervision.rs` with:

- `SupervisionState`
- `SupervisionConfig`
- `SupervisionAlert`
- `AlertKind`
- `AlertSeverity`

`SupervisionState` stores:

- `enabled: bool`
- `event_count: u64`
- `recent_hashes: VecDeque<ToolHashObservation>`
- `last_event_at_ms: Option<u64>`
- `compaction_times_ms: VecDeque<u64>`
- `total_input_tokens: u64`
- `total_output_tokens: u64`
- `token_baseline: Option<u64>`
- `alerts: Vec<SupervisionAlert>`
- `last_alert_at_ms: BTreeMap<String, u64>`

Use serde defaults so old `tasks.json` files load without migration.

## Event Observation

Add these methods:

- `SupervisionState::observe_event(&mut self, event: &Value, sink: &EventSink, now_ms: u64)`
- `SupervisionState::observe_bulk_sink(&mut self, sink: &EventSink, now_ms: u64)`
- `SupervisionState::snapshot(&self, now_ms: u64) -> Value`

Observation extracts:

- Tool name and input from known event shapes (`tool_use`, `function_call`,
  `toolCall`, and provider-compatible nested shapes).
- Usage from `EventSink::usage`.
- Compaction markers from event text/type fields containing
  `compact_boundary`, `compaction`, or `context compaction`.
- Last event timestamp from daemon wall clock, not provider time.

Hash the normalized `{tool_name,input}` JSON with `DefaultHasher`. The hash only
needs stable equality within a process lifetime; persisted state keeps the
observed hash string for status continuity, not cryptographic identity.

## Alert Rules

Default thresholds:

| Kind | Amber | Red |
|---|---:|---:|
| loop | 3 repeated hashes in the recent window | 6 repeated hashes |
| stall | 180 seconds since last event | 360 seconds |
| compaction | 2 compactions in 300 seconds | 4 compactions |
| token_burn | 2.0x baseline | 3.0x baseline |

Cooldown defaults to 60 seconds per `(kind,severity)`.

V1 alerts do not automatically call `cancel_task`. They are exposed to
`bro_status`, persisted, and available to callers/advisors. Automatic halt is a
follow-up once packet-driven action has a clean daemon-state dependency.

## Integration Points

### Task State

Add `supervision: SupervisionState` to:

- `TaskInner`
- `PersistedTask`
- every test/helper construction of `TaskInner`
- task persistence load/save

Current `TaskInner` construction sites are all in `src/orchestration/mod.rs`.
Find them with `rg 'TaskInner \\{' src/orchestration/mod.rs`; the current set
includes the loaded-task path, duplicate/spawn-failure paths, in-process task
path, provider spawn path, and local unit-test fixtures.

### Streaming Provider Hook

Inside the streaming stdout reader in `spawn_task_reserved`, after
`provider.parse_event` and accepted `apply_sink_updates`, call
`inner.supervision.observe_event(&evt, &sink, now_ms())` while the same
`TaskInner` lock is held.

Do not perform cancellation, note writes, packet evaluation, or any async work
while holding `TaskInner`. V1 observation is pure state update.

### Bulk Provider Hook

After `provider.parse_bulk_output` produces a final `EventSink`, call
`inner.supervision.observe_bulk_sink(&sink, now_ms())` before applying sink
updates. Bulk providers get token/result visibility but not real mid-flight loop
or stall detection.

### Status Output

Add `supervision` to:

- `task_result_json`
- `task_status_json` by inheritance from `task_result_json`
- `timeout_snapshot_json`

The object must be compact and JSON-serializable. Include `alerts` with the most
recent few alerts, not an unbounded log.

## Tests

Add focused unit tests under `src/orchestration/mod.rs` or a new
`src/orchestration/supervision.rs` test module:

- repeated identical tool events produce amber at 3 and red at 6.
- alert cooldown suppresses duplicate loop alerts.
- stall snapshot reports amber/red from `seconds_since_last_event`.
- token burn ratio is computed from usage totals and baseline.
- persisted tasks survive missing `supervision` in old JSON.
- `task_status_json` includes a `supervision` object.

Run:

```bash
rtk cargo test --bin blackboxd supervision
rtk cargo test --bin blackboxd
```

## Future Phases

These are intentionally not in the v1 completion bar:

- Automatic packet evaluation of supervision snapshots.
- Automatic red-alert cancellation.
- `bbox_note` emission for alerts.
- Daemon-side oracle co-session.
- Advisor helper extraction from team singleton into a dedicated workflow
  helper.
- Mid-flight race/wait primitive for early advisor summoning.

The v1 implementation is still useful without those phases because it gives
every orchestrator, advisor, reviewer, and drone a consistent deterministic
task-behavior snapshot.
