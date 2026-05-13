# Supervision — anomaly detection, oracle co-session, advisor evaluation

Date: 2026-05-10
Status: design proposal v1.
Depends on: `design/phase-decomposer.md` (references this as its supervision
substrate). Can be built and tested independently — decomposing phases
doesn't matter if dispatched bros can't be observed, steered, and
evaluated.

## 1. Problem

When a bro is dispatched into a workflow, the orchestrator loses visibility.
The bro runs. It might loop (same tool call with same input, 6 times in a
row). It might stall (no events for 6 minutes). It might fabricate (coherent
output, zero grounding). It might silently drop acceptance criteria. The
orchestrator learns none of this until the bro returns — and if the bro
never returns, the workflow hangs.

Three things are needed:
1. **Detection** — cheap, deterministic or cheap-LLM, fires quickly. A
   tripwire, not a judge.
2. **Judgment** — a smarter model making the call: cancel, steer, escalate,
   continue.
3. **Evaluation** — did the bro satisfy its acceptance criteria?

These compose as Swiss-cheese layers. Detection catches what it can
mechanically. Judgment handles what detection can't. Evaluation verifies
the result.

## 2. Architecture: packet-driven detection + advisor judgment

The insight: anomaly detection is a classification problem. The packet
system (`bbox_compile` / `bbox_apply` / `bbox_audit`) already does
deterministic classification against structured entities. Mechanical
counters become dumb data collectors; packets become the intelligence.

```
  ┌──────────────────────────────────┐
  │ Counters (per-task daemon state) │
  │                                  │
  │ loop_hash_window: {hash → count} │
  │ last_event_at: timestamp         │
  │ compaction_count: N              │
  │ token_burn_ratio: X              │
  └──────────────┬───────────────────┘
                 │ populate
  ┌──────────────▼───────────────────┐
  │ Entity (task-local anomaly       │
  │  snapshot, per-event hook)       │
  │                                  │
  │ {                                │
  │   "loop_hash_max": 7,            │
  │   "seconds_since_last_event": 42 │
  │   "compactions_in_window": 3,    │
  │   "token_burn_ratio": 2.4        │
  │ }                                │
  └──────────────┬───────────────────┘
                 │ evaluate
  ┌──────────────▼───────────────────┐
  │ Compiled packet                  │
  │ domain:supervision/anomaly       │
  │                                  │
  │ rules:                           │
  │   Ge loop_hash_max ≥ 6 → halt    │
  │   InRange compactions 2-4 → esc  │
  │   InRangeF burn 2.0-3.0 → warn   │
  └──────────────┬───────────────────┘
                 │ verdict
  ┌──────────────▼───────────────────┐
  │ Actions                          │
  │ halt  → cancel_task +            │
  │         bbox_note(blocked,       │
  │         task-scoped) +           │
  │         bro_arc_signal           │
  │ escalate → bbox_note(blocked)    │
  │ warn  → bbox_note(surprise)      │
  └──────────────────────────────────┘
```

This reuses `bbox_compile` (compile rules), `bbox_apply` (evaluate entity),
and `bbox_audit` (verify fidelity against known failure cases). No new
evaluation machinery. Counters are data collectors. Packets are the
intelligence. If the predicate AST lacks a primitive needed to express a
rule, we add it — a `bbox_packet_gap` log entry and a new predicate variant.

The anomaly packet is a SEPARATE packet from `policy_packet`
(`schema.rs:34`). `policy_packet` evaluates at node boundaries against the
workflow entity (`engine.rs:1098-1165`). The anomaly packet evaluates at
the per-event hook against the task-local anomaly snapshot. The distinction
is load-bearing: anomaly detection fires mid-dispatch, not at node
boundaries. A dedicated field on `Workflow` or `TaskInner` carries the
anomaly packet id — it does not overload `policy_packet`.

## 3. Layer 1: Mechanical anomaly counters

### 3.1 Counter state

Five anomaly patterns ported from daystrom's `AnomalyDetector.cs:14-20`
and `AnomalyDetectorConfig.cs:79-121`:

| Anomaly | Counter state | Amber threshold | Red threshold |
|---|---|---|---|
| Loop | Sliding window of `hash(tool_name, input)` — `src/orchestration/mod.rs:1097` already pushes every event to `inner.events` | Same hash ≥ 3 in window of 10 | Same hash ≥ 6 in window of 10 |
| Stall | `last_event_at` timestamp; timer fires on gap | No event for 180s | No event for 360s |
| Token burn | Cumulative input+output tokens vs rolling historical baseline per provider/model | 2.0× baseline | 3.0× baseline |
| Compaction | `compact_boundary` timestamp list, pruned to 300s window | ≥ 2 in window | ≥ 4 in window |
| Rate limit | Account utilization ratio | > 0.80 | > 0.95 |

Cooldown: same anomaly type+severity won't re-fire for 60s after the last
fire. This prevents a single stuck bro from flooding the signal channel
with duplicate anomalies.

### 3.2 Daemon implementation

The counters live at the existing per-event hook
(`src/orchestration/mod.rs:1097-1109`). Every NDJSON line from a running bro
is parsed and pushed to `inner.events` (`:1097`). The per-event callback
`provider.parse_event(&evt, &mut sink)` fires on each line (`:1109`).

Counters extend this seam:

```
parse_event (existing) → update counters → evaluate anomaly_packet → act on verdict
```

The counter state struct lives on `TaskInner` (or a sibling behind the same
`parking_lot::Mutex`). Fields:
- `recent_hashes: VecDeque<(Instant, String)>` — time-stamped hash window
- `last_event_at: Instant`
- `compaction_times: VecDeque<Instant>` — pruned to window
- `total_input_tokens: u64`, `total_output_tokens: u64`

A stall timer (`tokio::time::interval`) fires `check_stall` on amber
cadence. On each tick, if `now - last_event_at >= stall_threshold`, the
anomaly state is updated.

### 3.3 Entity shape

The counters materialize a snapshot into the entity:

```json
{
  "anomaly": {
    "loop_hash_max": 7,
    "loop_hash_max_tool": "Edit",
    "seconds_since_last_event": 42,
    "compactions_in_window": 3,
    "token_burn_ratio": 2.4,
    "rate_limit_utilization": 0.92
  }
}
```

The `loop_hash_max` is pre-computed by the counters — the packet doesn't
iterate the hash window; the counter finds the max count and the packet
compares against it. This keeps the predicate requirements simple: `Ge`,
`InRange`, `InRangeF`, `CountCmp` all exist in the AST (`src/packets/ast.rs`).

### 3.4 Packet evaluation timing

The anomaly packet evaluates at the per-event hook, not at node
boundaries (where `policy_packet` fires at `engine.rs:1090`). Two
evaluation points:

1. **Per-event evaluation.** The per-event hook evaluates the anomaly
   packet after updating counters. On `halt` verdict, `cancel_task` fires
   immediately (`mod.rs:1414-1439`) + task-scoped `bbox_note(kind=blocked)`
   + `bro_arc_signal` as the arc bridge. On `escalate` or `warn`,
   task-scoped `bbox_note` is written and the bro continues.

2. **Stall timer evaluation.** The stall timer evaluates the anomaly
   packet on each tick. Same verdict routing.

The evaluation function is a new daemon-side path — it cannot directly
reuse `apply_policy_packet` (`engine.rs:1098`) because that method is
private workflow-runner state requiring an arc_id. `TaskInner`
(`mod.rs:65`) has no `arc_id` field today. The daemon-side evaluation
needs a standalone function that takes the packet_id, the anomaly entity,
and the task handle, calls the packet store directly (via
`src/packets/apply.rs`), and routes verdicts to `cancel_task` or
`bbox_note`. The packet evaluation logic is a public function usable from
any context.

### 3.5 Configurable thresholds

Each workflow declares its anomaly packet on a dedicated field (not
`policy_packet`, which is the arc-level node-boundary policy at
`schema.rs:34`). The anomaly packet is evaluated at the per-event hook,
not at node boundaries. A new field on `Workflow` or `TaskInner` carries
the anomaly packet id.

Projects can compile stricter packets:
```
bbox_compile domain=supervision/anomaly-strict rules='[
  {id: "loop_red", antecedent: {op: "Ge", field: "anomaly.loop_hash_max", value: 6}, classification: "halt"},
  ...
]'
```

`bbox_audit` verifies the packet against known failure cases.
First-mode audit (`src/packets/audit.rs:201`) skips rows where `expected`
is null, so for no-match verification use a classification not emitted by
the packet:
```
bbox_audit packet_id=packet-<id> dataset=[
  {entity: {anomaly: {loop_hash_max: 6}}, expected: "halt"},
  {entity: {anomaly: {loop_hash_max: 2}}, expected: "nominal"},
]
```

### 3.6 Missing predicates

The current AST (`src/packets/ast.rs:114-333`) has `Ge`, `Gt`,
`InRange`, `InRangeF`, `KeyExists`, `All`, `Any`, `Not`, `ForAll`,
`Exists`, `CountCmp` (for array paths ending in `[*]` only). For the
anomaly entity above, no new predicates are needed —
`loop_hash_max` is a pre-computed integer that `Ge` compares
against; `seconds_since_last_event` is an integer for `InRange`;
`token_burn_ratio` is a float for `InRangeF`. All primitives exist.

If future anomaly patterns need something not expressible, log a
`bbox_packet_gap` and add the predicate. The AST is extensible by design.

## 4. Layer 2: Oracle co-session

### 4.1 What it is

A cheap LLM co-session (Haiku-class) that runs **alongside** the
implementer — daemon-side, not a workflow node. It observes the
implementer's event stream (`inner.events` at `mod.rs:1097`) on a sampling
cadence. It classifies: `nominal | fabrication | scope_creep | stuck`.

The oracle is a tripwire, not a judge. It detects semantic anomalies the
mechanical counters can't (fabrication, scope creep). Its output feeds the
advisor (§5). Read-only tool surface — no `bro_cancel`, no destructive tools.

Daystrom's `OracleSink.cs:9-26` shows the pattern: lazy session creation,
buffered executor context, structured JSON classification prompt.

### 4.2 Why daemon-side, not a workflow node

A bro stuck in a tool-calling loop never returns. A post-hoc oracle
(running after the implementer completes) would never fire. The oracle
must sample the implementer's event stream mid-flight — the same stream
the counters consume at `mod.rs:1097`.

The oracle is spawned when the implementer task starts, runs on a sampling
cadence (e.g., every 10s or every N events), and terminates when the
implementer completes or is cancelled. On non-nominal classification, it
emits `bbox_note(kind=surprise|dispute)` and optionally
`bro_arc_signal("oracle:<classification>", ...)`. The workflow's `Wait`
node or advisor consumes the signal.

### 4.3 Implementation

The oracle is a daemon-side task spawned alongside the implementer. It:
1. Opens a **separate** Haiku session via `bro_exec`. The implementer's
   session is single-flight (`bro_resume` takes a resume lease,
   `src/tools/dispatch.rs:119, 161`); the oracle must use its own task.
2. Polls `bro_status(task_id=<implementer>, tail=N)` on a cadence.
3. On each poll, classifies recent events.
4. On non-nominal classification, emits `bbox_note` + optionally
   `bro_arc_signal`.
5. Exits when the implementer's task status is terminal.

This is daemon-side infrastructure (~100-200 lines at the task spawn site,
alongside the counter state initialization). The oracle session is a
standard `bro_exec` dispatch — the daemon spawns it, waits on its
completion alongside the implementer, and tears it down.

The oracle agent manifest (JSON in `examples/agents/`) defines the
classification contract: inputs (sampled events), outputs (classification
+ confidence + reason), filter overlay (read-only). The manifest would be:

```json
{
  "kind": "agent",
  "name": "behavioral-oracle",
  "manifest": {
    "description": "Classifies dispatched-bro behavior: nominal, fabrication, scope_creep, stuck.",
    "filter_overlay": {
      "disallow": ["Edit", "Write", "Bash", "mcp__blackbox__bro_cancel",
                   "mcp__blackbox__bro_exec", "mcp__blackbox__bro_resume"]
    },
    "outputs": {
      "schema": {
        "properties": {
          "classification": {"enum": ["nominal", "fabrication", "scope_creep", "stuck"]},
          "confidence": {"type": "number"},
          "reason": {"type": "string"}
        }
      }
    },
    "cost_class": "cheap"
  }
}
```

Status: **aspirational.** Daemon-side oracle spawn + event stream
subscription + sampling cadence are new code. The agent manifest doesn't
exist yet. The bro_status polling surface exists
(`src/tools/dispatch.rs:674-683`).

## 5. Layer 3: Advisor

### 5.1 What it is

A smarter LLM (Sonnet/Opus class) that makes judgment calls. Summoned at
implementer completion boundaries or on signal from layers 1 or 2. Gets a
structured checkpoint. Emits one of five verdicts.

This is the judgment layer. Detection (layers 1+2) trips. Advisor judges.
Detection ≠ judgment.

### 5.2 Existing pipeline (team-scoped)

The advisor pipeline in `src/tools/roster.rs:607-1099` is substantial and
real:

1. **Init prompt** (`build_team_advisor_init_prompt`, `:607-668`):
   constructs the advisor's system prompt with charter, member list,
   halt_conditions, exit_conditions, packet_id. The five-verdict response
   format is embedded: `CONTINUE | ESCALATE | CHARTER_DRIFT | EXIT_MET |
   REPLACE_BRO`.

2. **Checkpoint** (`build_advisor_checkpoint`, `:926-1029`): structured
   snapshot of `{wait_kind, team_name, packet_id, monitored_task_ids,
   status counts (completed/failed/cancelled/timed_out/running), note
   counts by kind (dispute/assumption/surprise/followup/blocked/learned/
   done), per-member status + result_snippet}`.

3. **Packet pre-classification** (`apply_advisor_packet`, `:1031-1054`):
   runs the team's `packet_id` against the checkpoint entity, returns
   `{packetId, ruleId, classification, consequent, confidence}`. This is
   a mechanical pre-read — the LLM sees the packet result alongside the
   checkpoint JSON.

4. **Resume + verdict** (`maybe_resume_team_advisor`, `:1056-1099`):
   resumes the advisor's durable session with the checkpoint + packet
   result. In `Blocking` mode, waits for the response. The advisor emits
   the five-verdict line.

5. **Configuration** (`TeamAdvisorConfig`, `src/orchestration/team.rs:67-85`):
   `brofile`, `charter`, `context`, `halt_conditions`, `exit_conditions`,
   `packet_id`, `timeout_seconds`, `mode` (Blocking|Background).

The pipeline is wired. It works. But it's attached to team as a singleton —
`Team.advisor: Option<TeamAdvisor>` (`team.rs:119`), instantiated at team
creation time (`team.rs:260-265`), init/resume called from team dispatch
paths.

### 5.3 Rehousing to workflow-node level

The user's direction: rip advisor out of team, make 'advising' a workflow
verb. No migrations.

`NodeSpec` (`src/workflow/schema.rs:107-220`) already carries everything
needed for an advisor node:

- `actor` → the advisor brofile (Executor)
- `prompt` → the checkpoint prompt (templated from ArcContext vars)
- `gate` + `gate_mode` → packet-gated verdict routing
- `on_exit` → `parse_json` to normalize the verdict
- `wait_for` → join implementer outputs before evaluating
- `late_inject` → join async source output before rendering target
  prompt (`engine.rs:2142-2179`). Not mid-execution steering — it injects
  the source's completed output into the target's next prompt at the next
  turn boundary. The source must have completed before the target renders.
- `retry` → re-dispatch on refinement
- `actor_failure` → `Continue` to record verdict even on timeout

The `TeamAdvisorConfig` schema transfers to a `NodeAdvisor` field on
`NodeSpec` or to the node's `gate` packet. The checkpoint builder
(`roster.rs:926`) already exists. The packet evaluator (`:1031`) already
exists. The resume logic (`:1056`) already exists. They need a new call
site — advisor nodes in the workflow, not team-singleton on the dispatch
path.

### 5.4 Five verdicts

| Verdict | Meaning | Consumer |
|---|---|---|
| CONTINUE | Work is acceptable; proceed | Gate routes to next node |
| ESCALATE | Tier escalation needed — re-dispatch with stronger model | Gate routes to escalation node; re-dispatches implementer with different brofile |
| CHARTER_DRIFT | Bro is violating charter/spirit of the task | Gate routes to human escalation or replan |
| EXIT_MET | Acceptance criteria satisfied | Gate routes to terminal |
| REPLACE_BRO | Replace the bro agent entirely | Gate routes to replacement dispatch with different brofile/model |

The verdict vocabulary is defined in the advisor init prompt
(`roster.rs:657, 1085`). Consumer-side routing for `REPLACE_BRO`, `ESCALATE`,
and `CHARTER_DRIFT` does not exist in code yet — the prompt declares the
vocabulary, but the downstream nodes that act on each verdict need to be
built.

### 5.5 Advisor as subworkflow verb

A workflow node with `subworkflow_ref` pointing to an installed advisor
workflow runs the advisor subworkflow. The subworkflow:
1. Reads the implementer's output from imported vars
2. Builds the checkpoint (calls `bbox_notes`, reads `task_id` status)
3. Runs the packet against the checkpoint
4. Dispatches the advisor with checkpoint + packet result
5. Exports the verdict back to the parent

The checkpoint builder and packet evaluator currently live inside
`roster.rs` as team-scoped methods — they need extraction to standalone
functions before the subworkflow can call them. No installed `advise`
workflow exists today. This is structural work: extract the pipeline from
team-singleton, wrap it in a subworkflow, install it via
`bro_workflow_install`.

For multi-advisor panels: Ensemble actor over a team of advisor agents,
whiteboard deliberation, synthesizer reads `whiteboard_summarize` and
emits the final verdict. Same pattern as the whiteboard-arc example
(`examples/whiteboard/workflows/whiteboard-arc.json`).

### 5.6 Integrating anomaly signals

The anomaly packet's verdicts (§3.4) and the oracle's notes (§4.2) feed
into the advisor's checkpoint. The checkpoint (`roster.rs:926-1029`) counts
notes by kind for monitored task IDs only. Arc-level policy notes (written
by `arc_note` at `engine.rs:1036` with `task_id=None`) are NOT counted by
the checkpoint's note summary (`roster.rs:894-924` filters on
`note.task_id`). For anomaly signals to reach the advisor's checkpoint,
they must either use task-scoped `bbox_note` calls or be surfaced as
checkpoint fields (e.g., `anomaly_events` added to `AdvisorCheckpoint`).

A dedicated anomaly signal — `signal_arc_dispatch("anomaly:<kind>", ...)`
(`src/server/routes.rs:1871-1934`) — can also be consumed by a `Wait` node
that triggers an early advisor summoning, rather than waiting for the
implementer to complete.

## 6. Single-unit supervised dispatch

This is the primitive: one implementer, one advisor. Fan-out (§6.2) is N
copies of this.

### 6.1 Lifecycle

```
  ┌──────────────────────────────────────────────────┐
  │ DAEMON-SIDE (per-task, always running)           │
  │                                                  │
  │  ┌────────────┐    ┌──────────────┐              │
  │  │ Counters   │    │ Oracle       │              │
  │  │ (mechan-   │    │ (cheap LLM   │              │
  │  │  ical)     │    │  co-session) │              │
  │  └─────┬──────┘    └──────┬───────┘              │
  │        │ detection        │ detection            │
  │        │ (tripwire)       │ (tripwire)           │
  └────────┼──────────────────┼──────────────────────┘
           │                  │
           │    ┌─────────────┘
           │    │ anomaly signal or implementer done
           │    │
  ┌────────▼────▼────────────────────────────────────┐
  │ ADVISOR (per-implementer, always at completion,   │
  │         conditionally on anomaly signal)          │
  │                                                   │
  │  ┌──────────┐   ┌──────────┐   ┌──────────────┐  │
  │  │ CONTINUE │   │ ESCALATE │   │ REPLACE_BRO  │  │
  │  │ → accept │   │ → recov- │   │ → different  │  │
  │  │   gate   │   │   ery    │   │   brofile    │  │
  │  └──────────┘   │   bro    │   └──────────────┘  │
  │                 └────┬─────┘                      │
  │                      │ drop-in replacement:       │
  │                      │ same worktree, same        │
  │                      │ counters+oracle+advisor    │
  │                      │ loop                       │
  └──────────────────────┴────────────────────────────┘
```

**Counters** (daemon-side, deterministic): loop hash, stall timer, token
burn, compaction count. Red → `cancel_task` + `bro_arc_signal`. Amber →
`bbox_note(kind=surprise)`. Always running; zero LLM cost.

**Oracle** (daemon-side, cheap LLM): Haiku-class co-session. Observes the
implementer's event stream (`inner.events` at `mod.rs:1097`) on a sampling
cadence. Classifies: `nominal | fabrication | scope_creep | stuck`. On
non-nominal classification, emits `bbox_note(kind=surprise|dispute)` and
optionally `bro_arc_signal`. Read-only tool surface — no `bro_cancel`, no
destructive tools. Tripwire, not judge. The oracle is daemon machinery,
not a workflow node — it runs alongside the implementer, not after.

**Advisor** (workflow, smarter LLM): invoked at two points:
1. **Always** — when the implementer claims done.
2. **Conditionally** — when oracle or counters emit an anomaly signal.

For conditional (early) summoning, the implementer must be dispatched as
fire-and-forget (`mode: FireAndForget`). A normal sync actor node blocks
until the implementer task is terminal (`engine.rs:1575`); `Wait` nodes
register only when control reaches them (`engine.rs:1324`). With f-a-f,
the workflow continues to a `Wait` node that can catch the anomaly signal
before the implementer completes. Unmatched signals arriving before the
Wait node registers go idle (`routes.rs:1871`).

The advisor gets a structured checkpoint: implementer status, note counts
(including oracle's notes), per-member snippets. A packet pre-classifies
the checkpoint. The advisor LLM sees both and emits a verdict: `CONTINUE |
ESCALATE | CHARTER_DRIFT | EXIT_MET | REPLACE_BRO`.

- `CONTINUE` → acceptance gate (§6.3). If criteria met, done. If not,
  the gate routes back to advisor with the failing criteria surfaced.
- `ESCALATE` → recovery bro takes over. The workflow designates a
  `recovery_brofile` (stronger model, higher effort tier). The recovery
  bro inherits the implementer's worktree and partial progress. It enters
  the same supervision loop: counters + oracle observe it, advisor
  evaluates it. It is a drop-in replacement, not a special path.
- `REPLACE_BRO` → different brofile dispatched. Same loop. No worktree
  change — the replacement bro continues from the same state.
- `CHARTER_DRIFT` → human escalation (`Wait` on human signal).
- `EXIT_MET` → terminal. Acceptance criteria satisfied, advisor agrees.

If the advisor terminates the implementer mid-flight (via `bro_cancel`),
the recovery bro picks up from the implementer's worktree. `actor_results`
are recorded only after the actor wait returns
(`engine.rs:1575`, `context.rs:32`) — they are not available mid-flight.
The recovery bro gets the worktree state and any committed changes;
transcript/notes from the cancelled implementer are available via
`bbox_notes(task_id=...)`.

### 6.2 Fan-out: N implementers, N advisors

In the decomposer case, implementers fan out via `foreach` over DAG
sub-units. Each sub-unit is a subworkflow containing the full supervision
loop from §6.1: implementer dispatch → daemon counters + oracle observation
→ advisor evaluation → acceptance gate → recovery/escalation.

Advisors are 1:1 with implementers. An advisor evaluates its own
implementer's output. It does not see other implementers' work.

After foreach collects, the **recomposition council** (a durable ensemble,
`design/phase-decomposer.md` §4.5) evaluates the batch. The council
persists across epochs, maintaining context. If unsatisfied, it produces a
remediation packet that re-enters the pipeline — inlet → decomposer →
dispatch → advisor gate → council re-evaluates. The council is the only
entity that decides iterate vs halt. Advisors are per-bro judgment; the
council is batch-level judgment.

```
foreach over DAG sub-units:
  ┌─────────────────────────────┐
  │ Sub-unit subworkflow        │
  │                             │
  │  Implementer                │
  │    │                        │
  │    ├── counters (daemon)    │
  │    ├── oracle (daemon)      │
  │    │                        │
  │  Advisor ──┬── CONTINUE ──▶ Acceptance gate
  │            ├── ESCALATE ──▶ Recovery bro → loop
  │            ├── REPLACE_BRO ▶ New brofile → loop
  │            └── CHARTER_DRIFT/EXIT_MET
  └─────────────────────────────┘

collect into vars.sub_results

         ▼
┌─────────────────────┐
│ Recomposition Council│
│ (durable ensemble,   │
│  persists across     │
│  epochs)             │
│                      │
│  evaluates batch →   │
│  if unsatisfied →    │
│  remediation packet  │──▶ INLET (re-enters pipeline)
│  if satisfied → done │
│  if untenable → halt │
└─────────────────────┘
```

### 6.3 Acceptance evaluation within advisor

Acceptance evaluation is part of the advisor's `CONTINUE` path. When the
advisor says CONTINUE, a gate packet (`schema.rs:120-127`) evaluates the
implementer's output against `vars.acceptance_criteria`:

```
Advisor: CONTINUE
    │
    ▼
┌──────────┐
│ Gate     │
│ packet:  │
│ accept-  │
│ ance     │
└────┬─────┘
     │
┌────┴────┐
▼         ▼
pass      fail
│         │
▼         ▼
done    Advisor (re-summoned with
         failing criteria surfaced —
         may steer, escalate, or
         replace)
```

The gate entity (`src/workflow/context.rs:246-261`) includes `vars`,
`outputs`, and `node_output`. A packet rule like `StringContains{field:
"node_output", needle: "dark mode toggle implemented"}` verifies a
criterion. More rigorous: `on_exit: parse_json` normalizes structured
output, gate reads from `vars`.

### 6.4 Foreach + collect: fan-out supervision

`foreach` with `collect` (`schema.rs:193-276`, `engine.rs:1662-1874`) is
how the decomposer fans out implementer subworkflows. Each iteration is a
full supervised subworkflow (§6.1). Results collect as an array of
`FanoutChildOutcome` objects (`engine.rs:655-665`) with per-item
`{index, key, item, status, exports, outputs}`.

**Collect → checkpoint integration.** When a foreach iteration's advisor
needs checkpoint data about its implementer, the child subworkflow exports
the implementer's task ID (via `${actor_results.<NodeName>.taskId}`). The
advisor subworkflow calls `bbox_notes(task_id=...)` to collect oracle
notes and builds the checkpoint. The `FanoutChildOutcome` shape is the
foreach-collected wrapper; the per-implementer checkpoint is built inside
each child subworkflow, not across children.

**Why not a fleet-wide advisor.** A single advisor reading all sub-unit
results would be recomposition, not per-bro judgment. Advisors are 1:1.
Recomposition is downstream (§6.2).

| Primitive | Location | Status |
|---|---|---|
| Per-event hook seam | `src/orchestration/mod.rs:1097-1109` | implemented |
| cancel_task (SIGTERM) | `src/orchestration/mod.rs:1414-1439` | implemented |
| signal_arc_dispatch | `src/server/routes.rs:1871-1934` | implemented |
| Wait node (signal suspension) | `src/workflow/wait.rs` | implemented |
| policy_packet (arc-level gate) | `schema.rs:34`, `engine.rs:1098-1165` | implemented |
| Node gate + branch routing | `schema.rs:120-127, 389-395` | implemented |
| Fork (parallel fire-and-forget) | `schema.rs:396-403`, `engine.rs:1269` | implemented |
| late_inject (async feedback) | `schema.rs:355-374`, `engine.rs:2142-2179` | implemented |
| wait_for (fan-in join) | `schema.rs:190-191`, `engine.rs:1235-1265` | implemented |
| foreach + collect (fan-out/fan-in) | `schema.rs:193-276`, `engine.rs:1662-1874` | implemented |
| Subworkflow + imports/exports | `engine.rs:2401-2580` | implemented |
| Durable actor sessions | `schema.rs:64`, `engine.rs:2064-2068` | implemented |
| Advisor init prompt | `src/tools/roster.rs:607-668` | implemented |
| Advisor checkpoint builder | `src/tools/roster.rs:926-1029` | implemented |
| Advisor packet evaluator | `src/tools/roster.rs:1031-1054` | implemented |
| Advisor resume + verdict | `src/tools/roster.rs:1056-1099` | implemented |
| TeamAdvisorConfig schema | `src/orchestration/team.rs:67-85` | implemented |
| Packet predicate AST (Ge, InRange, CountCmp, etc.) | `src/packets/ast.rs:114-333` | implemented |
| bbox_compile / bbox_apply / bbox_audit | `src/tools/packets.rs` | implemented |
| bbox_note (side-channel records) | `src/notes.rs` | implemented |
| bro_status (tail=N for event polling) | `src/tools/dispatch.rs:674-683` | implemented |
| bro_arc_signal (arc signal emission) | `src/tools/orchestrate.rs:272-283` | implemented |
| Anomaly counter state + per-event extension | — | **aspirational** |
| Mid-dispatch packet evaluation point | — | **aspirational** |
| Advisor rehoused to NodeSpec (from team) | — | **aspirational** |
| Verdict consumers (REPLACE_BRO, ESCALATE, CHARTER_DRIFT routing) | — | **aspirational** |
| Daemon-side oracle co-session (event stream + sampling) | — | **aspirational** |
| Stall timer in daemon | — | **aspirational** |

## 8. Out of scope

- **Hard interception of provider built-ins.** Coercion is in
  `design/workspace-tools.md`. Enforcement is a separate concern.
- **Daystrom-style overminds.** Design artifacts in daystrom, not
  implemented in either repo.
- **Acceptance criteria as first-class schema objects.** Today criteria are
  prose strings. Formalizing them into typed entities is separate work.
- **Human-in-the-loop escalation UI.** The M6 human escalation node in the
  phase-decomposer doc is a `Wait` on a signal. The signal source (Slack,
  web, CLI) is out of scope.

## 9. Build sequence

Tests can start at step 3 — once an anomaly packet compiles and passes
audit, the evaluation path is independently testable.

1. **Anomaly counter state on TaskInner.** Struct with loop hash window,
   stall timer, compaction list, token tracking. Extend per-event hook to
   update counters on each NDJSON line. Populate anomaly entity snapshot.
2. **Mid-dispatch packet evaluation.** Build standalone evaluation function
   (packet store + anomaly entity → verdict). Call from per-event hook and
   stall timer. Route `halt` → `cancel_task`, `escalate` → blocked note,
   `warn` → surprise note.
3. **Daemon-side oracle co-session.** Spawn oracle task alongside
   implementer. Subscribe to `inner.events`, sample on cadence, emit
   `bbox_note` + `bro_arc_signal` on non-nominal classification.
4. **Default anomaly packet.** Compile `domain:supervision/anomaly-defaults`
   with daystrom thresholds. Audit against known failure cases (loop_6 =
   halt, loop_2 = continue, stall_400s = halt, etc.).
5. **Per-workflow packet override.** Dedicated anomaly packet field on
   Workflow. Test with strict/relaxed variants.
6. **Advisor as subworkflow verb.** Extract advisor pipeline from
   team-singleton. Build `subworkflow_ref: "advise"` with checkpoint →
   packet → resume → verdict → gate routing.
7. **Verdict consumers.** Build escalation node (re-dispatches implementer
   with stronger model as recovery bro), replace node (different brofile),
   charter-drift node (human escalation signal). Recovery bro inherits
   worktree + re-enters supervision loop.
8. **Anomaly signal → early advisor.** Wire anomaly packet and oracle
   verdicts through `bro_arc_signal` + `Wait` for early advisor summoning.
   Requires fire-and-forget implementer dispatch.
