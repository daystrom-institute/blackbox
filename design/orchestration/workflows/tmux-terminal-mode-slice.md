---
title: "Tmux terminal mode: provider-eligibility slice"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - orchestration
  - workflows
date: 2026-05-31
status: "design proposal v1"
brief: "First extractable slice of the tmux portal design: let TUI-capable providers (Claude, vanilla Codex) run their real TUI inside a tmux pane and complete from the transcript read plane, with none of the portal projection/focus/hook apparatus."
---

# Tmux terminal mode: provider-eligibility slice

Related: [Tmux Portal Workflows](tmux-portal-workflows.md),
[Tmux Portal Workflows Impl](tmux-portal-workflows-impl.md),
[Provider Transcript Read Plane](../../surfaces/provider-transcripts/provider-transcript-read-plane.md),
[Workflow Engine](../../../docs/workflows.md)

## 1. Scope and thesis

The tmux portal design (`tmux-portal-workflows.md`) bundles two separable
concerns:

1. **Terminal-mode dispatch** — launch a provider's real interactive TUI inside
   a tmux pane instead of as a headless child process, submit the rendered
   prompt by typing into it, and complete the workflow node from the **provider
   transcript read plane** (not pane text).
2. **The portal apparatus** — a human-facing projection layer: overview/focus,
   the attention queue, `link-window` projection into an `admin` session, the
   six `portal_*` hook ops, TUI processors, and startup reconciliation.

This document specifies **concern #1 only**, and narrower than the parent doc's
Phase 4: the unit of opt-in is the **provider**, not a portal policy. The
deliverable is "a TUI-capable provider can run its node inside tmux instead of
headless," with no projection, focus, hooks, or reconciliation.

The portal apparatus (concern #2) remains future work and is explicitly out of
scope here. This slice is designed so that nothing it ships has to be unwound to
build the portal on top of it later.

### 1.1 Why this slice is safe to ship alone

- The **transcript read plane has landed** (`src/transcripts/`:
  `TranscriptLocation`, `TranscriptCursor`, `NormalizedTranscriptEvent`,
  adapters, cursor persistence). `TaskInner` already carries
  `transcript_location` / `transcript_cursor`. The *substrate* this slice needs
  already exists. Two pieces do **not** yet exist and are explicitly in scope
  here: (a) a transcript-only **turn-complete resolver** — the landed
  provider-event wait is event-level and resolves on the first matching event
  (§3.3); and (b) **post-launch session-id binding** for TUI dispatch, since the
  headless stdout path that learns the session id is gone in tmux mode (§3.3).
- **Claude and Codex transcript adapters are present** (`src/transcripts/adapters.rs`,
  `Provider::Claude` and `Provider::Codex` JSONL). These are exactly the two
  providers in scope, so transcript-driven completion works without inventing
  pane scraping.
- Both providers have real, steerable TUIs validated in the parent doc's
  experiment ledger (Claude Code TUI; `codex --no-alt-screen`).

### 1.2 Provider eligibility (the load-bearing decision)

Terminal mode is only meaningful for providers whose dispatch path is an
**interactive TUI process** that can be typed into and that writes a transcript
store. The current catalog (`src/orchestration/providers.rs`) splits cleanly:

| Provider | Backend | TUI? | In this slice? |
|---|---|---|---|
| `claude` | Claude Code CLI | yes | **yes** |
| `codex` | `codex` CLI (`--no-alt-screen`) | yes | **yes** |
| `brodex` | bro-harness, OpenAI Responses transport | no (emits stream-json) | no |
| `glm`, `deepseek` | bro-harness, Anthropic transport | no (stream-json) | no |
| `inception` | OpenCode | yes, but noisy chrome + unverified interrupt | deferred |
| `copilot`, `vibe`, `gemini` | provider-specific arg builders | unverified | deferred |

The brodex exclusion is the key correctness point and is structural, not a
policy choice: `codex` (the `codex` CLI) and `brodex` (bro-harness on the
Responses transport) are **distinct `Provider` enum variants** precisely
because the former is an interactive CLI and the latter is a headless harness
that emits the Claude stream-json envelope. A headless harness has no TUI to
launch in a pane and no place to type a prompt, so terminal mode is undefined
for it. The same reasoning excludes GLM and DeepSeek (also bro-harness).

This is expressed as a predicate on `Provider`, mechanically parallel to the
existing `Provider::capabilities()` matcher:

```rust
impl Provider {
    /// True iff this provider dispatches through an interactive TUI process
    /// that can be steered with typed input and persists a transcript store.
    /// Terminal mode (`terminal_mode=tmux`) is only valid for TUI-capable
    /// providers. Harness-backed providers (Brodex/GLM/DeepSeek via
    /// bro-harness) emit a stream-json envelope with no interactive surface
    /// and are intentionally excluded.
    pub fn tui_capable(&self) -> bool {
        matches!(self, Provider::Claude | Provider::Codex)
    }
}
```

`tui_capable` is intentionally separate from `capabilities()` — it is not an LLM
capability, it is a dispatch-surface fact. OpenCode/Copilot/Vibe/Gemini are not
added until each has a manual smoke record (parent doc Phase 8 matrix); adding a
variant here is a one-line change gated on that evidence.

## 2. What this slice ships

| Area | In scope | Out of scope (portal apparatus) |
|---|---|---|
| Actor schema | `terminal_mode: Native\|Tmux` | `portal_policy`, `portal_target`, `portal_cleanup` |
| Tmux backend | `ensure_session`, `create_window`, `send_text`, `send_enter`, `capture_pane`, `kill_window` | `link_window`, `unlink_window`, `select_window`, `select_layout`, `zoom_pane`, `move_window` |
| Dispatch | launch interactive TUI in pane, post-launch session-id binding, submit prompt, complete via a transcript-only turn resolver | overview/focus/attention queue |
| Hook ops | none | `portal_focus`/`portal_release`/`portal_status_snapshot`/`portal_cleanup`/… |
| TUI processors | none (capture used only for liveness) | advisory `TuiSnapshot` parsing |
| Status | bare tmux ids on the task record | `bro_arc_status` portal fields |
| Recovery | kill the pane on task end / cancel | startup reconciliation, orphan linking |

### 2.1 Invariants carried over from the parent design

These remain hard rules even in the minimal slice (they are the reason the slice
is safe):

1. **Pane capture never becomes node output.** Completion is driven entirely by
   the transcript read plane. `capture_pane` is used only as a process-liveness
   check ("pane still exists / process still attached"), never parsed for
   content. (Parent cutover rule #3.)
2. **Transcript adapters are the only source for automated gates and task
   summaries.** (Parent cutover rule #4.)
3. **`terminal_mode=tmux` defaults off.** Existing dispatch is unchanged unless
   an actor opts in. (Parent cutover rule #1.)
4. **Headless workflows must keep working without tmux installed.** If
   `tmux` is not on `PATH`, declaring `terminal_mode=tmux` is a clear dispatch
   error, and every existing (Native) workflow is unaffected. (Parent cutover
   rule #6.)

## 3. Implementation

### 3.1 Phase A — schema field + eligibility validation

**Prerequisites:** none.

- Add to `ActorSpec` (`src/workflow/schema.rs`):

  ```rust
  #[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
  #[serde(rename_all = "snake_case")]
  pub enum TerminalMode {
      #[default]
      Native,
      Tmux,
  }

  // on ActorSpec:
  #[serde(default)]
  pub terminal_mode: TerminalMode,
  ```

  The portal-related fields (`portal_policy`, `portal_target`,
  `portal_cleanup`) from the parent impl doc are deliberately **not** added
  here; adding them later is additive and does not change `TerminalMode`.

- Add `Provider::tui_capable()` (§1.2).

- **Stateful eligibility validation (not `workflow::compile`).** This check
  needs brofile/team → provider resolution, which is **not** available in the
  state-free `workflow::compile` (`src/workflow/mod.rs`). It belongs in the same
  place capability validation already lives:
  `server::workflow_capabilities::validate_workflow_capabilities`
  (`src/server/workflow_capabilities.rs`), which takes `&Arc<SharedState>` and
  resolves actor providers via `resolve_actor_providers`. Extend that function:
  if an actor sets `terminal_mode=tmux`, **every** provider its brofile/team can
  resolve to must be `tui_capable()`. A non-TUI provider (e.g. a brodex brofile)
  under `terminal_mode=tmux` is a **hard validation error**, not a silent
  downgrade to Native — matching the project's fail-closed capability
  convention. Note this is orthogonal to the existing `requires`-gated path,
  which short-circuits when `requires` is empty; the terminal-mode check must
  run regardless of `requires`.
- **Close the ingress gap.** `validate_workflow_capabilities` is invoked on the
  MCP `bro_orchestrate_run` path before dry-run (`src/tools/orchestrate.rs`),
  but the plain HTTP `/orchestrate` dry-run currently returns after `compile`
  only (`src/server/routes.rs`). Every dry-run **and** run ingress that can
  accept `terminal_mode=tmux` must call the stateful validator first, or a
  brodex+tmux workflow could slip through the HTTP path and only fail at
  dispatch. Wire the HTTP ingress through the same validator.

- Update `schema/workflow.schema.json`; add a minimal example under
  `examples/workflows/` that passes `bro_orchestrate_run(dry_run=true)`.

**Deliverable:** workflows can declare `terminal_mode=tmux` on a Claude/Codex
actor and dry-run validate; declaring it on a harness-backed actor is rejected by
the stateful validator.

**Tests:**
- schema parse for `native`/`tmux`; unknown value rejected
- `validate_workflow_capabilities` accepts tmux on a Claude/Codex actor, rejects
  it on a brodex/glm/deepseek actor (with `requires` both empty and non-empty)
- HTTP `/orchestrate` dry-run rejects a brodex+tmux workflow (regression guard
  for the ingress gap)
- dry-run validation green for the example workflow

### 3.2 Phase B — minimal tmux backend

**Prerequisites:** Phase A.

A narrow trait over direct `tmux` CLI invocation (fixed argv, no shell
concatenation). Blackbox owns these ops directly; it does **not** depend on the
experimental tmux MCP, and it does **not** need the `link-window` patch — that
primitive belongs to the portal apparatus, which this slice omits.

```rust
#[async_trait]
pub trait TmuxBackend {
    async fn tmux_available(&self) -> bool;
    async fn ensure_session(&self, name: &str) -> Result<TmuxSession>;
    async fn create_window(&self, session: &str, name: &str, command: &[String]) -> Result<TmuxHandle>;
    async fn send_text(&self, pane_id: &str, text: &str) -> Result<()>;
    async fn send_enter(&self, pane_id: &str) -> Result<()>;
    async fn capture_pane(&self, pane_id: &str, limit: usize) -> Result<String>; // liveness only
    async fn kill_window(&self, handle: &TmuxHandle) -> Result<()>;
}
```

Notes:
- `TmuxHandle` stores `(container_session, window_id, pane_id)` — the
  `(session, window)` pair, never `window_id` alone, so it stays compatible
  with the parent doc's linked-window caveat if the portal is built later.
- Deterministic container session name: `bb-actors:<arc_id>` (parent §2.3). No
  portal session is created in this slice.
- `tmux_available()` backs the cutover-rule-#6 dispatch error.
- A fake backend implements the trait for unit tests; no live tmux required in
  CI.

**Deliverable:** unit-tested backend, no workflow integration yet.

**Tests:**
- command-builder argv test per op
- fake-backend lifecycle (ensure → create → send → kill)
- `send_text` never shell-interpolates the prompt body

### 3.3 Phase C — terminal-mode dispatch for Claude + Codex

**Prerequisites:** Phases A–B and the landed transcript read plane.

- **Provider TUI launch profiles** (separate from transcript adapters).
  **Terminal mode must NOT reuse `Provider::build_exec_args` as-is** — that
  builder is headless: Claude-family uses `-p <prompt> --output-format
  stream-json` and Codex uses `exec --json` with the prompt as argv
  (`src/orchestration/providers/exec_args.rs`), and the spawn path pipes stdout
  and forces `TERM=dumb` (`src/orchestration/mod.rs`). None of that is an
  interactive TUI. Instead:
  - Factor the shared, mode-neutral pieces out of `build_exec_args` (model
    selection, effort, env/MCP injection, default suppression) into a helper both
    the headless and TUI paths call.
  - The TUI profile adds the **interactive** invocation and omits the headless
    flags: Codex launches `codex --no-alt-screen` (no `exec --json`, no
    `-p`); Claude launches the normal interactive TUI (no `-p`,
    no `--output-format stream-json`). `TERM` is left as a real terminal type,
    not `dumb`.
  - The prompt is **never** passed as a launch arg in TUI mode. It is submitted
    only by typing into the pane (`send_text` + `send_enter`) after launch.
  These are the only two profiles; OpenCode et al. are deferred.

- **Launch path.** When `terminal_mode=tmux` and the resolved provider is
  `tui_capable()`:
  1. Create the task record first (it still owns provider/session metadata).
  2. `ensure_session("bb-actors:<arc_id>")`, then `create_window` with the
     provider's interactive launch argv. The child TUI lives in the pane; stdout
     is no longer the dispatch channel.
  3. Stash `(session, window, pane)` ids on the task record (a small struct;
     **not** the full `PortalHandle` — that is portal scope).

- **Session-id binding (cannot be hand-waved).** In headless dispatch the task's
  `session_id` is learned from the provider's stdout stream-json events — the
  dispatch loop flips `inner.session_id` from `"pending"` to the emitted id
  (`src/orchestration/mod.rs`). **In TUI mode stdout is not piped, so that path
  never fires.** Yet transcript reads fail closed on an empty/`pending` session
  id: the provider-event reader rejects it
  (`src/workflow/engine/provider_events.rs`), and both the Claude and Codex
  adapters refuse `pending` in `locate` (`src/transcripts/adapters.rs`). So
  Phase C must explicitly bind the provider session id **after launch and before
  any transcript polling**:
  1. After the TUI is up, discover the freshly-created provider session by
     scanning the provider's session store for the new rollout/session file
     under the launch cwd (Codex writes the id into its rollout filename, e.g.
     `rollout-<ts>-<session_id>.jsonl`; Claude writes a per-session JSONL under
     its project dir). Reuse the existing adapter `locate`/discovery rather than
     inferring identity from pane text.
  2. Populate `transcript_location` + `session_id` on the task record from that
     discovery.
  3. If the session id cannot be bound within a bounded window, **fail closed**
     (dispatch error) — do not fall back to pane scraping. This matches the
     fail-closed conventions and cutover rule #3.

- **Completion rule (transcript-only — needs a turn resolver, NOT first event).**
  The existing provider-event wait machinery is **event-level**:
  `run_provider_event_wait_node` resolves on the *first* matching event and
  records that single payload, matching only on `kind`/`tool`/`contains`
  (`src/workflow/engine/provider_events.rs`), and the normalized event model has
  message/tool/thinking kinds but **no turn-final / turn-complete boundary**
  (`src/transcripts/types.rs`). Reusing it verbatim would complete the node on
  the first assistant or tool event mid-turn — wrong for an actor node that must
  return the *completed* turn. This slice therefore must add a **transcript-only
  turn resolver** for terminal-mode actor nodes:
  1. Capture the actor's current `TranscriptCursor` **before** prompt
     submission.
  2. Send the rendered prompt via `send_text` + `send_enter`.
  3. Poll the transcript adapter, advancing the durable cursor, until a
     **turn-complete predicate** is satisfied — a provider-specific
     turn-final/done marker or final-assistant boundary, not merely the first
     new assistant/tool event. (Defining that predicate per provider is in
     scope for this slice; it is the one genuinely new piece of machinery the
     slice must build on top of the landed read plane.)
  4. Return node output from the normalized transcript events of that completed
     turn.
  Transcript-adapter failure follows the **existing** provider-event
  retry/block rule — no new *failure* path is introduced (only the new
  turn-complete predicate).

- **Liveness only.** A bounded "pane still exists" check via `capture_pane` is
  acceptable as a process-health signal; it is never a completion gate and its
  text is never returned.

- **Cleanup.** On node/task completion or `bro_arc_cancel`, `kill_window` the
  actor pane. There is no portal projection to unlink and no reconciliation
  pass in this slice; a killed daemon leaves at most an orphaned
  `bb-actors:<arc_id>` session, which a future portal-reconciliation phase will
  reap. Document this known gap rather than silently leaking.

**Deliverable:** a Claude or Codex actor node runs its turn inside a tmux pane,
returns assistant output sourced from transcript events, and tears the pane down
on completion/cancel.

**Tests:**
- fake-backend + fake-adapter: full terminal dispatch lifecycle for Claude and
  Codex profiles
- completion returns transcript content, asserts pane text is **not** used
- **turn resolver does not complete on the first mid-turn assistant/tool event**;
  it waits for the turn-complete predicate (regression guard against reusing the
  event-level wait verbatim)
- session-id binding: discovery populates `session_id`/`transcript_location`
  after launch; failure to bind within the window → fail-closed dispatch error
  (no pane-scrape fallback)
- timeout when no completed turn arrives after the prompt
- `terminal_mode=tmux` with `tmux_available()==false` → clear dispatch error
- cancel path kills the actor window exactly once

## 4. Explicitly deferred (portal apparatus)

Out of scope here; tracked by `tmux-portal-workflows-impl.md`:

- `PortalHandle` / `bro_arc_status` portal fields (parent Phase 1, 6.3)
- `link-window`/`unlink-window` and the tmux-mcp-rs patch (parent §8, Phase 2.4)
- portal session, overview, focus, attention queue (parent Phases 3, 6)
- the six `portal_*` hook ops (parent Phase 3)
- TUI processors / `TuiSnapshot` advisory state (parent Phase 5)
- `portal_policy` automation and Keystone tmux reference variant (parent Phase 6)
- startup reconciliation of orphaned sessions/links (parent Phase 7)

None of these require reworking the slice's surface: `TerminalMode` is a strict
subset of the parent's actor fields, the backend trait is a subset of the
parent's, and the task-record tmux ids are the seed of a later `PortalHandle`.

## 5. Open questions

1. Should the per-task tmux-id stash be a tiny dedicated struct now, or should we
   land a minimal `PortalHandle` shell up front so the portal phase doesn't
   migrate task records later? (Leaning: tiny struct now; migration is cheap and
   keeps this slice honest about scope.)
2. For the Claude profile, is the standard TUI's queued-message behavior a
   problem when we submit exactly one prompt per node, or is single-prompt
   submission always clean? (Parent ledger suggests clean; confirm in manual
   smoke.)
3. Do we want a daemon-config kill-switch (`terminal_mode` globally disabled)
   independent of per-actor opt-in, for headless deployments that have tmux
   installed but don't want actors spawning panes?

## 6. Verification ladder

1. `cargo test --lib workflow` (schema parse + stateful eligibility validation)
2. `cargo test --lib transcripts` (completion path unchanged)
3. `cargo test --lib` (slice tests: backend, dispatch)
4. Manual smoke (no portal):
   - declare a Codex actor `terminal_mode=tmux`
   - run the node; confirm a `bb-actors:<arc_id>` pane spawns the codex TUI
   - confirm node output came from transcript events, not pane text
   - confirm the pane is killed on completion
   - repeat for a Claude actor
   - confirm a brodex actor with `terminal_mode=tmux` is rejected by the
     stateful validator (and by the HTTP `/orchestrate` dry-run)
