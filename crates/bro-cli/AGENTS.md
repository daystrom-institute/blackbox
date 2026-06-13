# bro-cli — fleet cockpit (`bro fleet`) + single-agent view (`bro agent`)

Notes for working on the fleet TUI slice. Invariants and traps, not inventory —
verify shapes against code, but do not violate these without explicit design.

## The transcript is a file, not a stream

- The zoom transcript is the harness session event log
  (`$BRO_HOME/harness-sessions/<session_id>.events.jsonl`), file-tailed
  directly. The daemon resolves the path per roster row (`transcript_path`);
  the cockpit attaches and incrementally reads it. There is no transcript
  RPC/SSE plane — one existed (snapshot + live cursors + history paging) and
  collapsed under three mutually misaligned cursor spaces; it was deleted
  deliberately. Do not reintroduce a transcript transport.
- If something is missing from the transcript, the fix is an **emit lever in
  bro-harness** (log the event), never a cockpit-side reconstruction or a new
  endpoint.
- Event-log lines are wrapped `{ts, event}`; events are **complete,
  append-only steps** (one assistant message per model step, tool-result
  batches). Nothing revises retroactively, so everything parsed from the file
  is stable and commits straight into native terminal scrollback. The
  conservative turn-boundary watermark exists only for the in-process
  fallback transcript.

## Resume identity model

- A resume is a NEW task id over the SAME session id → same transcript file.
  Continuity must always be keyed by session/transcript-path, never task id:
  the file tail and the inline scrollback-commit cursors are path-keyed so
  they carry across the swap. Anything else holding the old task id
  (focus, roster anchor, pending handshakes) must be repointed when the
  resumed handle is installed.
- `bro agent --resume <session_id|name>` with no prompt is attach-only:
  create a local, daemon-less row pointed at the existing session event log,
  render it, and wait for a real composer turn before calling
  `/control/resume`. Its session path must be resolved via the same
  `bro_home` precedence as the daemon/fleet client (`BRO_HOME` →
  `BLACKBOX_CONFIG [paths].bro_home` → state dir), or the TUI silently points
  at an empty file while the daemon wrote the real transcript elsewhere.
- Each resumed task runs exactly ONE turn and completes. **Task status IS the
  turn boundary.** `snapshot().turn_active` is meaningless for daemon-backed
  rows (derived from an always-empty local event buffer; reads true even for
  terminal tasks) — never gate logic on it for roster-fed tasks.
- A failed `/control/*` launch returns a daemon-less stub handle
  (`launch_error()`); never install it into a row or the task store. Keep the
  existing row, give the composer text back, surface the error. The daemon
  404s DELETE for ids it doesn't know, so a registered ghost is undeletable.

## Two render paths — wire both or it doesn't exist

- The zoom view is the INLINE renderer (custom terminal viewport at the
  bottom + `insert_history` into native scrollback). The roster/config views
  are the alt-screen `draw()` path. Overlays, menus, and queued cockpit lines
  must be wired into the path where the user will be, or they silently never
  render (slash menu and /help were invisible in zoom for exactly this
  reason; overlay cores are buffer-level and shared — keep them that way).
- `pending_cockpit_lines` drain into scrollback ONLY in the inline view.
  Never push an outcome line and then zone-flip away from zoom in the same
  breath — the operator must be left looking at the verdict.
- Native scrollback is the scroll surface. Committed lines belong to the
  terminal; the live area holds only the in-flight remainder. If tmux can
  only scroll a couple of lines, content is being trapped in the live area.

## Slash command routing

- `run_local_slash` runs BEFORE zone routing. Zoom-local commands must be
  matched there. Unmatched slash input: in zoom it steers the session
  (provider-native commands like `/compact` are the agent's to handle);
  everywhere else it is consumed with an unknown-command error, because the
  fall-through would dispatch the text as a new task.
- Zone command tables (`zone_slash_commands`) are the contract for what the
  menu advertises — keep handler gating and tables in sync.

## Closeout: mechanical vs judgment (the handshake)

- The fold is mechanical and driver-owned (bro-tools phased driver). The
  judgment halves are requested, never assumed:
  - Commit message: composed by the worktree's AGENT (resume with a
    compose-message turn; read the reply from the session file — roster
    snippets are truncated). `--message` is an operator override only.
  - Worktree rebase conflict: the agent reconciles ITS OWN work, then the
    cockpit auto-reruns the fold as adopt.
  - Base-repo state (diverged from origin, terminal push failures): the
    OPERATOR's history, the operator's call. The agent is resumed
    assess-only (inspect read-only, summarize, recommend, flag needs-input)
    and nothing mutates or auto-retries.
- Pending handshakes are polled on task-status turn boundaries; a failed
  resume must abort the pending handshake loudly (otherwise the poll fires
  against the old task's stale last message).

## UX defaults

- One Enter submits. No arm/confirm double-keypress patterns in the chat
  flow — typing a message is already deliberate. (Reserve arm-confirm for
  destructive bulk actions like prune.)
- Surface daemon error bodies (`{"error": …}`), never bare reqwest status
  lines ("HTTP status client error (400 …)" is a bug, not an error message).
- The agent name defaults to the first-turn excerpt until renamed — never
  render both side by side.
- The first user turn in the session file is the ambient-wrapped dispatch
  prompt; when this cockpit launched the dispatch, display the operator's own
  text instead (the wrapped form stays in the file for forensics).

## Validation

- Unit tests lie about TUI behavior; validate user-visible changes with tmux
  MCP against a real session. `bro agent` is instance-lock-free (probe-safe);
  `bro fleet` takes an advisory flock on the fleet store — never duel the
  operator's live cockpit. Use an isolated tmux socket, capture panes with
  scrollback (`start: -N`), kill your probe sessions, and forget your probe
  tasks from the roster (only the ones you created).
- Cheap live probe: `--provider glm --model glm-4.7-flash` with a
  reply-exactly prompt. Multi-tool prompts ("run these N echo commands as
  separate tool calls") exercise mid-turn scrollback commits.
- Always `cargo nextest run --workspace` — a bare run silently skips this
  crate's tests.
