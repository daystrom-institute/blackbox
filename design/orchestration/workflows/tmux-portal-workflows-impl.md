---
title: "Tmux Portal Workflows - Implementation Plan"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - orchestration
  - workflows
date: 2026-05-12
status: "implementation proposal v1"
brief: "Implementation plan for workflow-native tmux portal handoff, focus, and shared live run state."
---

# Tmux Portal Workflows: Implementation Plan

Companion to: `tmux-portal-workflows.md`
Depends on: `design/archive/provider-transcript-read-plane.md`

This plan turns the tmux portal design into blackbox workflow machinery. The
implementation should keep one invariant strict:

```text
tmux is the live control plane; provider transcript adapters are the read plane.
```

Pane capture can explain what the operator saw. It must not become the source
of truth for workflow gates, task results, or transcript indexing.

## Current Substrate

The transcript read plane has landed in `c3022b5`:

- `src/transcripts/` owns `TranscriptLocation`, `TranscriptCursor`,
  `NormalizedTranscriptEvent`, adapters, projections, cursor persistence, and
  OpenCode SQLite reads.
- `TaskInner` already stores `transcript_location` and `transcript_cursor`
  (`src/orchestration/mod.rs`).
- `task_result_json` already exposes `transcriptLocation` and
  `transcriptCursor`.
- `WaitSpec.provider_event` is implemented in `src/workflow/wait.rs`.
- `WorkflowRunner::run_provider_event_wait_node` polls the transcript adapter,
  advances durable cursors, and blocks after repeated adapter failures
  (`src/workflow/engine.rs`).

The workflow engine also has the right side-effect seam:

- `ActorSpec` in `src/workflow/schema.rs` is the correct place for
  actor-level portal intent.
- `HookOp` / `OpKind` in `src/workflow/ops.rs` is the correct place for
  deterministic portal transitions.
- `bro_arc_status` in `src/tools/orchestrate.rs` is the correct read surface
  for active portal state.

What is missing is tmux ownership, portal metadata, TUI launch/steer profiles,
and recovery semantics.

## Phase 0: Schema and Document Hygiene

**Prerequisites:** none.

**What gets built:**

0.1 **Archive completed read-plane docs.** The landed read-plane design and
implementation docs live under `design/archive/`. Active tmux docs should link
to the archived read-plane docs, not to stale proposed paths.

0.2 **Actor schema extension.** Add typed fields to `ActorSpec`:

```rust
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum TerminalMode {
    #[default]
    Native,
    Tmux,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum PortalPolicy {
    #[default]
    Never,
    RecordOnly,
    Overview,
    OnAttention,
    AlwaysFocus,
}
```

Fields:

```rust
pub terminal_mode: TerminalMode,
pub portal_policy: PortalPolicy,
pub portal_target: Option<String>,
pub portal_cleanup: PortalCleanupPolicy,
```

`portal_cleanup` should be typed, not a string:

```rust
pub enum PortalCleanupPolicy {
    OnArcExit,
    KeepOnFail,
    KeepAlways,
}
```

0.3 **Schema and examples.** Update `schema/workflow.schema.json` and add a
minimal tmux example under `examples/workflows/` that validates with
`bro_orchestrate_run(dry_run=true)`.

**Deliverable:** Workflows can declare tmux intent without runtime behavior.

**Tests:**

- workflow schema parse tests for all enum values
- dry-run validation accepts `terminal_mode="tmux"` and rejects unknown values

## Phase 1: Portal State Model

**Prerequisites:** Phase 0.

**What gets built:**

1.1 **Portal types.** Add a small module, likely `src/workflow/portal.rs` or
`src/portal.rs`, with:

```rust
pub struct PortalHandle {
    pub arc_id: String,
    pub node: String,
    pub actor: String,
    pub task_id: String,
    pub provider: Provider,
    pub session_id: String,
    pub tmux: TmuxHandle,
    pub state: PortalState,
}

pub struct TmuxHandle {
    pub socket: Option<String>,
    pub container_session: String,
    pub window_id: String,
    pub pane_id: String,
    pub portal_session: Option<String>,
    pub portal_window_id: Option<String>,
}

pub enum PortalState {
    Hidden,
    Overview,
    Focused,
    AttentionQueued,
    Released,
    Orphaned,
}
```

Store both the canonical actor window and the portal projection. Linked
windows can share the same tmux window id across sessions, so status must
preserve the session/window pair instead of assuming `window_id` alone is an
ownership key.

Do not duplicate transcript ownership in `PortalHandle`. `TaskInner` already
owns `transcript_location` and `transcript_cursor`; portal status should join
the handle back to the task when it needs read-plane state. A status response
may render both portal and transcript summaries together, but persistence
should keep tmux state and transcript cursor state in their current owners.

1.2 **Task metadata extension.** Extend `TaskInner` with:

```rust
pub portal_handle: Option<PortalHandle>,
```

Persist it through the existing task store record so daemon restarts can
reconcile still-running tmux sessions.

1.3 **Arc status extension.** Extend `bro_arc_status` by composing portal
state at query time from the running arc snapshot plus task-store portal
handles:

- active portal handles for tasks in the arc
- attention queue entries
- portal target session
- stale/orphaned handle warnings

Do not put portal state directly into `ArcSnapshot` for v1 unless a later
phase proves the runner must mutate it at every boundary. The task store owns
task/portal handles; `bro_arc_status` is the aggregation surface.

Do not stuff pane captures into ordinary task results. Expose captures only as
bounded debug artifacts or notes.

**Deliverable:** Status surfaces can show portal state even before tmux mode
actually launches actors.

**Tests:**

- JSON round-trip for `PortalHandle`
- `bro_arc_status` includes portal handles when a task has one
- persisted task records survive unknown future portal fields

## Phase 2: Tmux Backend

**Prerequisites:** Phase 1.

**What gets built:**

2.1 **Backend trait.** Blackbox should own the tmux operations it depends on
instead of depending on Codex's MCP client configuration. Use direct `tmux`
CLI invocation behind a trait:

```rust
#[async_trait]
pub trait TmuxBackend {
    async fn ensure_session(&self, name: &str) -> Result<TmuxSession>;
    async fn create_window(&self, session: &str, name: &str, command: &[String]) -> Result<TmuxHandle>;
    async fn link_window(&self, source: &TmuxHandle, target_session: &str) -> Result<TmuxHandle>;
    async fn unlink_window(&self, linked: &TmuxHandle) -> Result<()>;
    async fn select_window(&self, target: &TmuxHandle) -> Result<()>;
    async fn select_layout(&self, session: &str, layout: PortalLayout) -> Result<()>;
    async fn zoom_pane(&self, pane_id: &str, zoomed: bool) -> Result<()>;
    async fn capture_pane(&self, pane_id: &str, limit: usize) -> Result<String>;
    async fn send_text(&self, pane_id: &str, text: &str) -> Result<()>;
    async fn send_enter(&self, pane_id: &str) -> Result<()>;
    async fn send_escape(&self, pane_id: &str) -> Result<()>;
}
```

The experimental tmux MCP remains useful for operator-side experiments, but
daemon workflow execution should not require one MCP server to call another.

2.2 **Command allowlist.** Implement the backend as a narrow command wrapper
over known tmux subcommands. Do not expose arbitrary shell execution through
portal args.

2.3 **Session naming.** Use deterministic names:

```text
bb-actors:<arc_id>
bb-portal:<target>
```

The default portal target can be `admin` when present; otherwise create
`bb-portal:default`. The target should also be configurable per workflow or
daemon config.

2.4 **Link-first projection.** Prefer `link-window` for projection. Fall back
to `move-window` only behind an explicit config flag, because moving mutates
ownership and can delete empty source sessions. If `link-window` is
unavailable, `portal_focus` should return a clear unsupported error unless
the operator opted into move fallback.

`link-window` is a native tmux command, not a dependency on the experimental
tmux MCP. The patched MCP validated the semantics during the spike; the
blackbox backend should depend on the host tmux binary supporting the command.

**Deliverable:** Unit-tested tmux backend with no workflow integration yet.

**Tests:**

- command builder tests for every tmux operation
- fake backend tests for link/unlink/focus
- live ignored/manual smoke that links a window into `admin`, unlinks it, and
  verifies the source actor window remains alive

## Phase 3: Portal Hook Ops

**Prerequisites:** Phases 1-2.

**What gets built:**

3.1 **Hook op enum variants.** Add these `OpKind` variants:

```rust
PortalRegister,
PortalOverviewAdd,
PortalFocus,
PortalRelease,
PortalStatusSnapshot,
PortalCleanup,
```

The hook implementation should resolve actor/task references from
`ArcContext`, `actor_results`, and the task store. Hooks mutate portal state
and metadata, not workflow control flow.

3.2 **Arguments.**

`portal_focus`:

```json
{
  "actor": "implementer",
  "target": "admin",
  "mode": "link_window",
  "zoom": true,
  "reason": "review feedback arrived"
}
```

`portal_status_snapshot`:

```json
{
  "actor": "implementer",
  "lines": 80,
  "into_var": "implementer_visible_state"
}
```

3.3 **Failure policy.** Portal hooks should usually use
`on_failure="warn"`. A tmux projection failure should not normally fail the
workflow unless the node is explicitly an operator-attention node.

3.4 **Attention queue.** `portal_focus` records an attention event even if
the window is already focused. Multiple events are ordered by time and shown
in `bro_arc_status`.

**Deliverable:** Workflows can focus/release/snapshot an already-registered
portal handle through hooks.

**Tests:**

- hook arg rendering with `${last_signal.*}` and `${vars.*}`
- missing actor produces warning/halt according to `on_failure`
- focus adds exactly one ordered attention event per hook execution

## Phase 4: Terminal-Mode Dispatch MVP

**Prerequisites:** Phases 1-3 and the landed transcript read plane.

**What gets built:**

4.1 **Provider TUI profiles.** Add provider profiles separate from transcript
adapters:

```rust
pub struct TuiProfile {
    pub provider: Provider,
    pub start_args: fn(&DispatchSpec) -> Vec<String>,
    pub submit: SubmitStrategy,
    pub interrupt: InterruptStrategy,
    pub cleanup: CleanupStrategy,
    pub readiness: TuiReadinessProfile,
}
```

Initial profiles:

- Codex: use `codex --no-alt-screen`; submit text then Enter; interrupt via
  Escape and provider-specific `/stop` cleanup when needed.
- Claude: normal TUI; submit text then Enter; support queued-message state.
- OpenCode: normal TUI; submit text then Enter; recognize `QUEUED`.

Gemini/Copilot/Vibe can remain unsupported until their TUI behavior has a
manual smoke record.

4.2 **Task launch path.** When `terminal_mode=tmux`, create the task record
first, then launch the provider TUI in the actor container session. The task
still owns provider/session metadata, but the child process lives inside tmux.

4.3 **Prompt submission.** After the TUI is ready, send the rendered actor
prompt through tmux. Capture the transcript cursor before submission so the
turn resolver only reads new provider events.

4.4 **Session id discovery.** Reuse existing provider session discovery and
the transcript adapter registry. Do not infer session identity from pane text
unless there is no provider store yet; pane text is only diagnostic.

4.5 **MVP completion rule.** For v1, terminal-mode actor nodes complete from
the transcript read plane only:

1. Capture the actor's current `TranscriptCursor` before prompt submission.
2. Poll the provider transcript adapter until at least one new assistant event
   appears after the submitted user prompt.
3. Return the node output from those normalized transcript events.

Transcript failure follows the existing provider-event retry/block rule. Do
not require prompt-ready TUI detection in Phase 4; the TUI processor contract
is Phase 5. A minimal backend-level "pane still exists" check is acceptable
for process health, but it is not a completion gate.

Phase 5 may refine completion by adding TUI readiness as an advisory boundary
hint. Even then, TUI state remains a boundary hint, not content truth.

**Deliverable:** A Codex terminal-mode actor can run one workflow node in tmux,
return the assistant output from transcript events, and expose a portal handle.

**Tests:**

- fake backend terminal dispatch lifecycle
- fake transcript adapter turn completion
- timeout when no post-prompt assistant event arrives
- no tmux pane text is used as node output

## Phase 5: TUI Processors

**Prerequisites:** Phase 4.

**What gets built:**

5.1 **Advisory processor contract.**

```rust
pub struct TuiSnapshot {
    pub provider: Provider,
    pub pane_id: String,
    pub captured_at_ms: u64,
    pub queue_state: QueueState,
    pub prompt_ready: bool,
    pub background_processes: u32,
    pub interrupt_hint: Option<InterruptHint>,
    pub warnings: Vec<String>,
    pub last_visible_assistant_line: Option<String>,
}
```

5.2 **Provider processors.**

- Codex: queued-message marker, background terminal marker, `/stop` hint,
  prompt-ready footer.
- Claude: queued-message footer, shell/tool-running state, prompt-ready state.
- OpenCode: `QUEUED`, thinking/status chrome, interrupt footer.

5.3 **Noise boundaries.** Processors emit small advisory structs. They do not
write transcript events and do not drive gates directly.

**Deliverable:** Portal status can show useful live TUI state without making
pane capture canonical.

**Tests:**

- golden pane captures for Codex/Claude/OpenCode
- width-wrapped captures do not panic and degrade to `unknown`
- noisy status chrome is not returned as assistant output

## Phase 6: Workflow and Keystone Integration

**Prerequisites:** Phases 3-5.

**What gets built:**

6.1 **Keystone examples.** Update `examples/keystone/workflows/`:

- implementer actor uses `terminal_mode="tmux"` and
  `portal_policy="on_attention"` in the reference variant
- mechanical failure nodes call `portal_focus`
- parse-failure nodes call `portal_status_snapshot`
- `on_arc_exit` calls `portal_cleanup`

Keep non-tmux Keystone examples available so CI and headless deployments do
not require tmux.

6.2 **Portal policy automation.**

- `record_only`: create/register handle only
- `overview`: add to overview when actor starts, release on node completion
- `on_attention`: register only; hooks focus explicitly
- `always_focus`: focus on dispatch and release according to cleanup policy

6.3 **Status and tail.**

- `bro_arc_status` shows portal state and attention queue
- `bro_status` includes task portal handle summary
- `bro tail` can include portal handle refs but should continue reading clean
  events through the transcript read plane

**Deliverable:** Keystone has a tmux-enabled reference path that can hand a
live actor TUI to the operator without changing workflow state ownership.

**Tests:**

- Keystone tmux variant dry-runs
- hook-only failure path focuses the right actor in a fake backend
- `on_arc_exit` cleanup runs after success, failure, and cancel

## Phase 7: Reconciliation and Cleanup

**Prerequisites:** Phases 1-6.

**What gets built:**

7.1 **Startup reconciliation.** On daemon startup:

- load persisted task portal handles
- list daemon-owned tmux sessions/windows
- mark handles `Orphaned` when task state and tmux state disagree
- unlink stale portal projections
- preserve actor windows for recoverable running tasks

7.2 **Arc cancellation.** `bro_arc_cancel` already trips the arc cancellation
token. Portal cleanup should run from `on_arc_cancel` / `on_arc_exit` and
must not kill a provider TUI unless the underlying task is being cancelled.

7.3 **Operator escape hatch.** Add a read-only status plus a targeted cleanup
command or hook. Avoid broad "cleanup all tmux" behavior; cleanup must be
scoped by arc id or task id.

**Deliverable:** Restart/cancel paths do not leave hidden sessions or linked
portal windows accumulating silently.

**Tests:**

- fake tmux inventory reconciliation
- stale linked window is unlinked, source actor window preserved
- cancelled arc calls cleanup exactly once

## Phase 8: Hardening and Provider Expansion

**Prerequisites:** Phases 4-7.

**What gets built:**

8.1 **Provider matrix.** Promote providers from manual-smoke to supported only
after they have:

- launch args
- submit strategy
- interrupt strategy
- prompt-ready detection
- transcript adapter coverage
- one fixture for TUI capture noise

8.2 **Security.**

- tmux command wrapper uses fixed argv, never shell-concatenated strings
- portal target session allowlist defaults to `admin` and `bb-portal:*`
- prompt submission logs metadata, not prompt bodies; body logging requires an
  explicit daemon config flag, not `RUST_LOG` level alone
- pane snapshots are bounded and treated as diagnostic artifacts

8.3 **Performance.**

- provider-event polling should back off while transcript mtime/row cursor is
  unchanged
- pane capture should be on-demand or low cadence, not per loop tick
- overview layout churn should be coalesced

**Deliverable:** Tmux mode is safe enough for long-running workflow use across
Codex, Claude, and OpenCode, with other providers gated by explicit support.

## Cutover Rules

1. `terminal_mode=tmux` defaults off.
2. Portal hooks are allowed to fail with warnings unless the workflow author
   opts into halt.
3. Pane capture never becomes node output.
4. Provider transcript adapters remain the only source for automated gates and
   task summaries.
5. `link-window` is required for focus/projection unless move fallback is
   explicitly enabled.
6. Headless workflows must keep working without tmux installed.

## Verification Ladder

1. `rtk cargo test --bin blackboxd workflow`
2. `rtk cargo test --bin blackboxd transcripts`
3. `rtk cargo test --bin blackboxd portal` once portal tests exist
4. `rtk cargo test`
5. Manual smoke:
   - start Codex TUI in hidden actor session
   - link into `admin`
   - submit prompt
   - verify node output came from transcript events
   - unlink portal projection
   - verify actor window still exists in the container session

## Initial Implementation Order

The least risky first slice is:

1. Phase 0 schema fields and JSON schema.
2. Phase 1 portal handle metadata in task/status surfaces.
3. Phase 2 fake backend plus command-builder tests.
4. Phase 3 hook ops against fake backend.
5. Manual live smoke for `link-window` through the backend.
6. Phase 4 Codex-only terminal actor MVP.

That slice proves the integration without committing every provider to tmux
mode at once.
