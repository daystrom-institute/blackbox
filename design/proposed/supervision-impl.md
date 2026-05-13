# Supervision — Implementation Plan

Date: 2026-05-10
Companion to: `design/supervision.md` (pure design — this is the build plan).

Each phase is independently testable. Phases 1-2 and 4 can proceed in
parallel. Phases 3 depends on 1. Phase 5 depends on 4. Phase 6 depends on
1-5. The DAG:

```
  Phase 1 ──▶ Phase 3
     │
     ├──────▶ Phase 2
     │
  Phase 4 ──▶ Phase 5
     │           │
     └──────┬────┘
            ▼
         Phase 6
```

---

## Phase 1: Mechanical anomaly counters

**Prerequisites:** none (the per-event hook seam already exists).

**What gets built:**

1.1 **Counter state struct on TaskInner.** Fields:
- `recent_hashes: VecDeque<(Instant, String)>` — time-stamped sliding
  window of `hash(tool_name, input)` tuples. Window size: 10.
- `last_event_at: Instant` — updated on every NDJSON line.
- `compaction_times: VecDeque<Instant>` — compact_boundary timestamps,
  pruned to 300s window.
- `total_input_tokens: u64`, `total_output_tokens: u64` — cumulative.
- Historical baseline per provider/model (loaded from config, optional).

1.2 **Per-event hook extension.** At `src/orchestration/mod.rs:1109`
   (`provider.parse_event`), after the existing sink update, call
   `update_counters(task, event)`. The function:

   **Coverage limitation:** The per-event NDJSON seam covers streaming
   providers only (`providers.rs:167`). Vibe and Gemini use bulk
   parsing after output is complete (`mod.rs:1161`). Mid-flight loop/
   stall counters do NOT work for bulk-parsed providers. Counters are
   effective for Claude, Codex, Copilot, OpenCode.

- Hashes `(tool_name, input)` for tool_use events. Appends to
  `recent_hashes`. Prunes to window.
- Updates `last_event_at`.
- On `compact_boundary` system message: appends to `compaction_times`,
  prunes to window.
- On usage update: accumulates `total_input_tokens`,
  `total_output_tokens`.

1.3 **Stall timer.** A `tokio::time::interval` spawned alongside the
   stdout reader. Fires every 180s (amber cadence). On tick: if
   `now - last_event_at >= threshold`, set `anomaly_stall_amber: true`
   (or red at 360s).

1.4 **Anomaly entity snapshot.** A function `anomaly_snapshot(task) ->
   Value` that produces:
```json
{
  "loop_hash_max": 7,
  "loop_hash_max_tool": "Edit",
  "seconds_since_last_event": 42,
  "compactions_in_window": 3,
  "token_burn_ratio": 2.4,
  "rate_limit_utilization": 0.92
}
```
Pre-computed by the counters — the packet sees a flat integer, not a
hash window to iterate.

**Deliverable:** Counters update on every event. Snapshot function
produces anomaly entity. No evaluation yet — counters just collect.

**Estimated size:** ~250-400 lines of Rust (struct, hook extension, stall
timer, snapshot) + ~50 lines for `SpawnTaskParams` extension (passing
`SharedState` handle, anomaly packet id, and oracle config to the
stdout reader task). The current `SpawnTaskParams` (`mod.rs:652`) only
carries task/process plumbing — needs server-state references for
`bbox_note`, `bro_arc_signal`, and `cancel_task` calls from within the
hook. `TaskInner` has no arc_id; task-scoped signals must close over an
`Arc<SharedState>` passed at spawn time.

---

## Phase 2: Anomaly packet evaluation

**Prerequisites:** Phase 1 (counters produce entity, but evaluation not
required for counter testing).

**What gets built:**

2.1 **Standalone evaluation function.** `evaluate_anomaly_packet(task,
   packet_id, entity) -> Option<verdict>`. Calls the packet store
   (`src/packets/apply.rs` — public function, usable from any context).
   Returns classification string or None.

2.2 **Per-event evaluation point.** After `update_counters` in the
   per-event hook, call `evaluate_anomaly_packet`. **Mutex boundary:**
   the event reader holds `TaskInner` while parsing (`mod.rs:1100`).
   `cancel_task` also locks `TaskInner` (`mod.rs:1423`). The evaluation
   must snapshot the verdict, drop the lock, then act. Do not call
   `cancel_task` / `bbox_note` / `bro_arc_signal` while holding the
   `TaskInner` lock. On verdict:
- `halt` → `cancel_task` (`mod.rs:1414-1439`) + task-scoped
  `bbox_note(kind=blocked)` + `bro_arc_signal`.
- `escalate` → task-scoped `bbox_note(kind=blocked)`.
- `warn` → task-scoped `bbox_note(kind=surprise)`.

2.3 **Stall timer evaluation.** Same evaluation on each stall timer tick.
   Same verdict routing.

2.4 **Default anomaly packet.** Compile `domain:supervision/anomaly-defaults`
   with daystrom thresholds:
```
{op: "Ge", field: "loop_hash_max", value: 6} → halt
{op: "Ge", field: "loop_hash_max", value: 3} → escalate
{op: "InRange", field: "compactions_in_window", min: 2, max: 4} → escalate
{op: "InRangeF", field: "token_burn_ratio", min: 2.0, max: 3.0} → warn
```

2.5 **Audit the default packet.** `bbox_audit` against known cases.
   First-mode audit (`src/packets/audit.rs:211`) marks no-match as
   uncovered, not as a nominal prediction. To verify no-match, use
   `mode=all` or include an explicit catch-all rule (e.g.
   `AlwaysTrue → nominal` at lowest priority):
- `loop_hash_max: 6` → `halt`
- `loop_hash_max: 2` → no match (add explicit `AlwaysTrue → nominal`
  rule at lowest priority if the audit shape requires a prediction)
- `compactions_in_window: 3` → `escalate`

2.6 **Per-workflow override field.** New field on `Workflow`
   (`anomaly_packet: Option<String>`) or on `TaskInner`. If set, the
   daemon-side evaluator uses this packet instead of the default.

2.7 **Cooldown.** After firing `halt` for `loop_red`, suppress
   re-evaluation of the same anomaly type+severity for 60s. Per
   `AnomalyDetectorConfig.cs:126`.

**Deliverable:** A compiled anomaly packet passes audit. A bro stuck in
a tool-calling loop is killed within 6 iterations. A bro that stalls for
360s is killed. Configurable per-workflow.

**Estimated size:** ~150-250 lines (evaluator, hook integration,
cooldown).

---

## Phase 3: Oracle co-session

**Prerequisites:** Phase 1 (event stream + per-event hook needed for
oracle sampling). Phase 2 not required but complementary.

**What gets built:**

3.1 **Oracle spawn at task start.** In the task spawn path
   (`mod.rs:1087+`), alongside the stdout reader and stall timer, spawn
   an oracle task. **Same coverage limitation as Phase 1:** streaming
   providers only. The oracle gets:
- The implementer's `task_id`.
- A sampling cadence (default: 10s).
- The anomaly packet id (if configured).

3.2 **Oracle session dispatch.** The oracle is a separate `bro_exec` call
   with a Haiku-class brofile. It uses its OWN session — not the
   implementer's (`bro_resume` is single-flight per session,
   `src/tools/dispatch.rs:119, 161`).

3.3 **Sampling loop.** The oracle:
1. Sleeps for cadence interval.
2. Calls `bro_status(task_id=<implementer>, tail=N)` to get recent events.
3. Classifies: `nominal | fabrication | scope_creep | stuck`.
4. On non-nominal: emits `bbox_note(kind=surprise|dispute,
   task_id=<implementer_task_id>)` — the IMPLEMENTER'S task ID, not
   the oracle's. The advisor checkpoint queries notes by the
   implementer's task ID (`roster.rs:902`). Oracle notes must carry
   that correlation.
5. Loops until the implementer's task status is terminal.

3.4 **Oracle agent manifest.** JSON file in `examples/agents/`:
   `behavioral-oracle.json`. Defines classification contract, read-only
   filter overlay, Haiku model, `cost_class: cheap`.

3.5 **Teardown.** The oracle task terminates when the implementer
   completes or is cancelled. The daemon waits for oracle completion
   alongside the implementer.

**Deliverable:** A running implementer has an oracle co-session
classifying its behavior on a 10s cadence. Oracle notes are queryable
via `bbox_notes`. Oracle signals route to workflow Wait nodes.

**Estimated size:** ~150-200 lines of Rust (spawn, sampling loop,
teardown) + agent manifest JSON.

---

## Phase 4: Advisor pipeline extraction

**Prerequisites:** none (can proceed in parallel with Phases 1-3). Working
with existing `src/tools/roster.rs:607-1099`.

**What gets built:**

4.1 **Extract checkpoint builder.** Move `build_advisor_checkpoint`
   (`roster.rs:926-1029`) to a standalone function. Accepts:
   `results: &[Value]`, `wait_kind: &str`, `team_name: &str`,
   `teamplate: &str`, `packet_id: Option<&str>`. The current function
   reads `team.name`, `team.teamplate`, and
   `team.advisor.config.packet_id` (`roster.rs:1004-1012`). Extraction
   must preserve these fields or accept them as parameters.

4.2 **Extract packet evaluator.** Move `apply_advisor_packet`
   (`roster.rs:1031-1054`) to a standalone function. Accepts:
   `packet_id: &str`, `checkpoint: &AdvisorCheckpoint`, `packet_store`.
   Returns `Value`. No longer requires `self`.

4.3 **Extract resume logic.** Move `maybe_resume_team_advisor`
   (`roster.rs:1056-1099`) to a standalone function. Removes the team
   load/save dance — just builds the checkpoint, evaluates the packet,
   dispatches the advisor, waits, returns the verdict. Accepts a brofile
   name instead of a team reference.

4.4 **Build `advise` subworkflow.** A workflow JSON artifact installed
   via `bro_workflow_install`. The subworkflow:
1. Imports `vars.implementer_task_id`, `vars.implementer_output`,
   `vars.acceptance_criteria`.
2. Calls `bbox_notes(task_id=...)` to collect oracle/counter notes.
3. Builds checkpoint from the implementer's task envelope.
4. Applies packet to checkpoint.
5. Dispatches advisor LLM with checkpoint + packet result.
6. Exports `vars.advisor_verdict` back to parent.

4.5 **NodeSpec advisor field.** New optional field on `NodeSpec`:
   `advisor: Option<NodeAdvisorConfig>`. Mirrors `TeamAdvisorConfig`
   (`team.rs:67-85`) but scoped to a node, not a team. Carries:
   `brofile`, `charter`, `halt_conditions`, `exit_conditions`,
   `packet_id`, `timeout_seconds`, `mode`. When set, the node runs the
   `advise` subworkflow after the actor completes.

**Deliverable:** `subworkflow_ref: "advise"` is callable. An implementer
node followed by an advisor node produces a structured verdict. The
pipeline no longer requires a `Team`.

**Estimated size:** ~200-300 lines (extraction + new subworkflow +
  NodeSpec field) + workflow JSON artifact.

---

## Phase 5: Verdict consumers

**Prerequisites:** Phase 4 (advisor produces verdicts; need routing).

**What gets built:**

5.1 **CONTINUE path.** The advisor says CONTINUE. The node's `gate`
   packet reads `vars.advisor_verdict`. If CONTINUE → `Branch` routes
   to acceptance evaluation node. Standard gate+branch pattern.

5.2 **ESCALATE path.** The advisor says ESCALATE. The workflow
   dispatches a **recovery bro** — an Executor with a
   `recovery_brofile` (stronger model, higher effort). The recovery bro
   inherits the implementer's worktree. It re-enters the supervision
   loop: counters + oracle observe it, advisor evaluates it. This is a
   `Goto` back-edge to the implementer dispatch node with a different
   brofile reference.

5.3 **REPLACE_BRO path.** Same as ESCALATE but with a different brofile
   (same effort tier, different provider or persona). No worktree change.

5.4 **CHARTER_DRIFT path.** The advisor says the implementer violated
   its charter. Route to a `Wait` node waiting on `bro_arc_signal` from
   a human operator. The signal carries instructions: retry, override,
   close, replan.

5.5 **EXIT_MET path.** Acceptance criteria satisfied, advisor agrees.
   Route to `Terminal`.

**Deliverable:** All five verdicts have concrete routing. A supervised
implementer that passes advisor + acceptance gate exits cleanly. A
failing implementer escalates to recovery bro and retries.

**Estimated size:** ~100-150 lines (branch routing + recovery bro
dispatch).

---

## Phase 6: Integration wiring

**Prerequisites:** Phases 1-5.

**What gets built:**

6.1 **Anomaly signal → advisor (post-completion only).** When counters
   or oracle emit `bro_arc_signal`, a `Wait` node catches it. But
   `wait_for` (`engine.rs:2188`) blocks until the fire-and-forget
   implementer task completes — it's a fan-in join, not a race. An
   anomaly signal that arrives AFTER the Wait node registers is queued,
   but the advisor is reached only after the implementer task completes.
   Signals arriving before the Wait registers go idle
   (`routes.rs:1883`, `no_matching_wait`). **Early advisor summoning
   (mid-flight, before implementer completion) is aspirational** — it
   needs a task-complete signal or a race primitive not present in the
   current engine.

   Post-completion pattern (implementable today):

```
Implementer (f-a-f) ──┐
                      ├── wait_for ──▶ Advisor (reads implementer
Wait anomaly:* ───────┘                 output + anomaly context)
```

   The advisor always sees the complete implementer output plus any
   anomaly signals that arrived during execution. It can judge whether
   the anomalies warrant ESCALATE (recovery bro) despite completion.

6.2 **Oracle notes → advisor checkpoint.** The checkpoint builder
   (Phase 4.1) calls `bbox_notes(task_id=<implementer>)` to collect
   oracle notes. Note counts by kind appear in the checkpoint. The
   advisor sees them alongside status counts.

6.3 **Recovery bro worktree inheritance.** When ESCALATE/REPLACE_BRO,
   the new dispatch uses the same `project_dir`. Executor dispatch uses
   `self.project_dir` (`engine.rs:1575`), not `meta.worktree` (set only
   by `WorktreeCreate` hook-op). If the workflow uses a dedicated
   worktree, the recovery bro's `project_dir` must point to it
   explicitly. Committed changes from the cancelled/failed implementer
   are on disk in that directory.

6.4 **Supervised subworkflow template.** A reusable subworkflow
   artifact that wraps any implementer dispatch with the full
   supervision loop (counters, oracle, advisor, recovery). Import the
   implementer brofile + prompt + acceptance criteria as vars. Export
   the advisor verdict + acceptance status. This is the building block
   `design/phase-decomposer.md` references for its foreach
   implementer dispatch.

6.5 **Foreach integration.** Test N supervised subworkflows running
   in parallel via `foreach`. Collect outcomes via
   `foreach.collect.into_var`. Verify each outcome carries the
   sub-unit's verdict in `exports`.

**Deliverable:** A complete supervised dispatch: implementer runs,
counters + oracle observe, advisor evaluates, verdict routes. N
parallel supervised subworkflows collect correctly.

**Estimated size:** ~100-150 lines (Wait integration, worktree
inheritance, subworkflow template) + workflow JSON artifact.

---

## Build sequence summary

| Phase | Can start after | Deliverable | Est. lines |
|---|---|---|---|
| 1. Counters | — | Anomaly entity snapshot | 250-400 |
| 2. Packet evaluation | Phase 1 | Loop detected → cancel_task within 6 iterations | 150-250 |
| 3. Oracle co-session | Phase 1 | Oracle classifying on 10s cadence | 150-200 |
| 4. Advisor extraction | — | `subworkflow_ref: "advise"` callable | 200-300 |
| 5. Verdict consumers | Phase 4 | All 5 verdicts have concrete routing | 100-150 |
| 6. Integration | Phases 1-5 | End-to-end supervised dispatch | 100-150 |

Total estimated new code: ~950-1450 lines of Rust + 3 JSON artifacts
(anomaly packet, oracle agent manifest, advise subworkflow).

## Testability

Each phase has a standalone test:
- Phase 1: Feed a sequence of NDJSON events to the counter state.
  Assert `loop_hash_max` counts correctly, stall timer fires.
- Phase 2: Compile the default anomaly packet. Audit against known
  cases. Feed the per-event hook with a loop of 6 identical tool calls.
  Assert `cancel_task` is called.
- Phase 3: Start an implementer task with a known-behavior brofile.
  Assert oracle emits `bbox_note` within the sampling cadence.
- Phase 4: Build a checkpoint from a mock task result. Apply a packet.
  Dispatch an advisor (can use a mock/fake session in test). Assert
  the verdict response format.
- Phase 5: Route each verdict through a mock workflow. Assert the
  correct branch is taken.
- Phase 6: Dispatch two supervised subworkflows in foreach. Assert
  both complete, collect correctly, verdicts in exports.
