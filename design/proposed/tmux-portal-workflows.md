# Tmux portal for workflow-native live bro handoff

Status: design proposal v1
Date: 2026-05-12
Related: `WORKFLOWS.md`, `examples/keystone/`, `design/proposed/supervision.md`

## 1. Thesis

`tmux` should be treated as a workflow portal, not as a notification
channel and not as the canonical workflow state store.

The normal operator flow has two terminals:

1. **Orchestrator chat** — the human talks to the supervising agent, starts
   workflows, checks status, and makes high-level decisions.
2. **Portal tmux session** — the human can drop into live agent TUIs when a
   workflow reaches a human-attention state.

The workflow engine still owns arc state, waits, signals, node outputs,
notes, and cleanup. `tmux` is an ephemeral live projection of selected
actors.

Keystone is the reference shape: webhook or poller starts an arc, hook-only
nodes do mechanical work, LLM actors run in subworkflows, wait nodes park
until external signals arrive, and gates route the arc. Portal operations
should fit that model as deterministic hook side effects around actor
dispatch and waits.

## 2. Experiment ledger

Validated locally with `tmux-mcp-rs` and live TUIs:

| Provider | Result |
|---|---|
| Codex | `codex --no-alt-screen` can be launched in a tmux pane, steered with `send_keys` + `send_enter`, soft-queues mid-turn input at tool-call boundaries, and can be interrupted with `send_escape` followed by `/stop` cleanup for background terminals. |
| Claude Code | Normal TUI captures cleanly, accepts mid-turn typed input, shows queued-message state, and submits the queued message after the active shell tool completes. |
| OpenCode | Normal TUI accepts mid-turn input and marks it `QUEUED`. Output is noisier because the TUI exposes thinking/status chrome. Interrupt semantics need more provider-specific testing. |
| Window handoff | `move_window(@20, targetSessionId=$19)` moved a live Codex TUI window into the attached `admin` session. Moving it back required recreating the source session because tmux deletes an empty source session after its only window moves. |

The useful discovery is that TUI-driven steering is not merely blind text
paste. The major CLIs expose visible queue or interrupt states in the pane.

## 3. Workflow integration

Add portal intent to actors, not to ad-hoc bro calls:

```jsonc
"actors": {
  "implementer": {
    "kind": "executor",
    "brofile": "keystone-impl",
    "durable": true,
    "compaction_anchor": true,
    "terminal_mode": "tmux",
    "portal_policy": "on_attention"
  }
}
```

Suggested `portal_policy` values:

| Policy | Meaning |
|---|---|
| `never` | Existing behavior. No tmux projection. |
| `record_only` | Run in tmux and store pane/window ids, but do not project into the portal. Useful for later forensic attach. |
| `overview` | Include the actor in the portal overview while it is active. |
| `on_attention` | Keep hidden by default; project/focus when a workflow event requires human attention. |
| `always_focus` | Make the actor visible and focused for the full node. Useful for debugging a new workflow. |

The workflow engine should persist a `PortalHandle` per dispatched actor
turn:

```rust
struct PortalHandle {
    arc_id: String,
    node: String,
    actor: String,
    task_id: String,
    provider: String,
    session_id: Option<String>,
    tmux_session_id: String,
    tmux_window_id: String,
    tmux_pane_id: String,
    portal_state: PortalState,
}
```

The handle is task metadata, not output. It should appear in
`bro_arc_status`, `bro tail`, and notes only by reference.

## 4. Portal hook ops

Expose portal transitions as hook ops, so Keystone-style workflows can use
them exactly like `http_json`, `shell`, `worktree_create`, or `mcp_call`.

Initial ops:

| Op | Purpose |
|---|---|
| `portal_register` | Ensure actor terminal exists and record its tmux handle. Usually implicit in actor dispatch when `terminal_mode="tmux"`. |
| `portal_overview_add` | Add or link the actor to the portal overview. |
| `portal_focus` | Bring the actor's live TUI to the portal foreground, optionally zooming the pane/window. |
| `portal_release` | Return the actor to hidden/overview state after the attention state resolves. |
| `portal_cleanup` | Remove portal projections on arc exit/cancel. Does not kill the actor unless the actor task is being cancelled. |
| `portal_status_snapshot` | Capture a bounded pane snapshot and store it as task/debug evidence. |

Example around a wait timeout:

```jsonc
"AwaitReviewTrigger": {
  "actor": "",
  "wait": {
    "any_of": [
      { "signal": "pr-ready", "correlate": { "pr": { "kind": "json_path", "path": "vars.pr_number" } } },
      { "signal": "pr-merged", "correlate": { "pr": { "kind": "json_path", "path": "vars.pr_number" } } }
    ],
    "timeout": "24h"
  },
  "gate": "domain:workflow-gate/merge-or-review",
  "on_exit": [
    {
      "op": "portal_focus",
      "args": {
        "actor": "implementer",
        "target": "admin",
        "reason": "review trigger wait resolved as ${last_signal.name}"
      },
      "when": "domain:portal-attention/review-wait-needs-human",
      "on_failure": "warn"
    }
  ],
  "next": { "type": "branch", "cases": { "ready": "Review", "merged": "Done" } }
}
```

This is deliberately workflow-native: a packet decides whether the resolved
wait needs human attention; the hook performs the tmux projection; the arc
continues or parks according to existing `wait`/`gate` semantics.

## 5. Keystone mapping

In `examples/keystone/`, useful portal points are:

| Workflow point | Portal behavior |
|---|---|
| `implementer-arc.FetchIssue` | Run durable implementer in tmux. Usually hidden or overview. |
| `implementer-arc.PushAndOpenPr` failure | Focus implementer and capture pane snapshot. Mechanical push/PR failed; human may need live context. |
| `issue-to-merged-pr.AwaitReviewTrigger` timeout | Focus implementer or arc overview. Waiting for PR events stalled. |
| `reviewer-arc.Review` | For debugging, overview both reviewers in tiled portal panes. For normal operation, hidden. |
| `reviewer-arc.Aggregate` parse failure in `PostReview` | Focus aggregator. Strict JSON failed; human can inspect the live TUI and steer. |
| `issue-to-merged-pr.AwaitFeedbackOrMerge` feedback | Optional focus before `AddressFeedback`, depending on policy. |
| `on_arc_cancel` | Focus overview briefly, then cleanup/release. |
| `on_arc_exit` | Cleanup portal projections; keep hidden actor session only if cleanup policy says keep-on-fail. |

This avoids inventing a `user` actor. Human attention is another workflow
state reached by packets, waits, and hooks.

## 6. Portal layouts

The portal can be more than one attention window.

### 6.1 Tiled overview

Default portal view:

```text
admin / portal session
  window: active-bros
    pane: implementer
    pane: reviewer-a
    pane: reviewer-b
    pane: aggregator or arc tail
```

The engine updates the overview as actors start and finish:

```text
actor dispatch -> add to overview
node terminal  -> mark idle or remove
arc exit       -> cleanup
```

Use `select_layout(tiled)` or `select_layout(even-horizontal)` to normalize
geometry. Use pane titles with `arc/node/actor` labels.

### 6.2 Focus mode

On attention events:

```text
portal_focus(actor)
  -> select actor window/pane
  -> zoom pane or move/link window to foreground
  -> record reason in status
```

Multiple attention events should be ordered, not stacked invisibly:

```text
portal queue:
  1. issue-42 Implement parse failure
  2. issue-43 Review disagreement
  3. nightly eval stuck in test loop
```

The human cycles through hot windows/panes using tmux itself.

## 7. Window vs pane ownership

There are three viable models.

### 7.1 One actor = one tmux window

Each actor TUI owns a window in a hidden container session. The portal
shows those windows by moving or linking them.

Pros:
- Clean mental model: one live bro TUI per window.
- Natural fullscreen takeover.
- Easy human navigation in tmux.
- Window id is a stable handle for actor status.

Cons:
- `move-window` transfers ownership. If the source session becomes empty,
  tmux deletes it.
- Without `link-window`, moving into the portal mutates the engine's
  bookkeeping.

This is the best model if `link-window` is available.

### 7.2 One portal window, subpanes per actor

All active actors are joined into one tiled portal window.

Pros:
- Excellent live dashboard: all active bros visible together.
- `select_layout(tiled)` and `zoom_pane` make overview/focus simple.
- No need for many admin windows.

Cons:
- tmux has pane move/join/break, not true pane linking. Joining a pane into
  the portal moves it out of its original window.
- Layout churn changes the actor's environment from the user's point of
  view and can make debugging harder.
- Some TUIs dislike small panes; output wrapping/noise gets worse.
- Returning a pane to its original session/window requires additional
  bookkeeping.

This is good as a portal projection, but risky as the actor's canonical
home unless the daemon fully owns the tmux layout.

### 7.3 Hybrid

Keep canonical actor windows in a hidden container session. Maintain a
portal overview that either:

- links actor windows when `link-window` exists, or
- creates lightweight observer panes that tail/capture/summarize actor
  windows when linking is not available.

On attention, focus the real actor window. For normal overview, show a
lossy projection.

This is likely the most robust long-term shape.

## 8. Required tmux primitives

Already available in `tmux-mcp-rs`:

- `create_session`
- `create_window`
- `move_window`
- `join_pane`
- `break_pane`
- `select_window`
- `select_pane`
- `select_layout`
- `zoom_pane`
- `rename_window`
- `rename_pane`
- `capture_pane`
- `send_keys`
- `send_enter`
- `send_escape`
- `send_cancel`

Strongly desired:

- `link_window`
- `unlink_window`

`link-window` is the cleaner primitive for portal takeover because it lets
the same live window appear in both the hidden actor container and the
human portal. `move-window` works, but it changes ownership and may delete
the source session.

## 9. Provider processors

Pane capture is display text, not transcript. Provider-specific processors
should turn noisy TUI text into operational hints:

| Provider | Useful markers |
|---|---|
| Codex | `Messages to be submitted after next tool call`, `background terminal running`, `/stop`, `esc to interrupt`, MCP startup warnings. |
| Claude | queued-message footer, shell running state, token/status footer, background shell manager hints. |
| OpenCode | `QUEUED`, `esc again to interrupt`, thinking/status chrome, command block boundaries. |

Processors should produce advisory state:

```json
{
  "queue_state": "idle|queued|running",
  "background_processes": 1,
  "interrupt_hint": "send_escape_then_stop",
  "last_visible_assistant_line": "..."
}
```

They should not be canonical workflow state. The arc context and task
registry remain authoritative.

## 10. Implementation sketch

1. Add `terminal_mode` and `portal_policy` to actor schema.
2. Extend task metadata with `PortalHandle`.
3. Add a tmux control module with provider profiles:
   - start command
   - submit action
   - soft-queue markers
   - interrupt action
   - cleanup action
4. Add hook ops: `portal_focus`, `portal_release`,
   `portal_status_snapshot`, `portal_cleanup`.
5. Add `bro_arc_status` portal fields:
   - active portal handles
   - portal target session
   - attention queue
6. Add `link-window` support to `tmux-mcp-rs` or use `move-window` as the
   first implementation with explicit bookkeeping updates.
7. Adapt Keystone as the first reference workflow:
   - implementer actor `terminal_mode="tmux"`
   - focus on mechanical failure / parse failure / wait timeout
   - cleanup on arc exit.

## 11. Open questions

1. Should every durable actor default to `record_only` tmux mode?
2. Should the portal target be global (`admin`) or per-workflow
   (`portal:${workflow}`)?
3. Should attention focus pause the workflow on a wait, or can the workflow
   continue while the human watches?
4. Should link-window be mandatory for v1, or is move-window acceptable with
   bookkeeping updates?
5. Should tiled overview show real panes, linked windows, or processed
   summaries?
6. Should provider processors live in blackbox, in a sidecar, or as
   tmux-portal packet rules?

