---
title: "Fleet TUI — daemon-authoritative roster stream + fleet/tail unification"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - fleet-tui
brief: "The code-grounded realization of the daemon-owned fleet model that backlog-multi-instance-coherence.md pointed at: retire the cockpit's client-side persistent TaskStore mirror, project the daemon's already-unified task_store + tail_tx into a roster snapshot + summary-delta SSE, kill the per-agent poller zoo, and fold `bro tail` into `bro fleet` as an `origin`-faceted tab on one roster. Splits the one conflated client store into three correctly-owned tiers: daemon-authoritative fleet data, ephemeral per-instance view state, and a shared flock'd composer histfile."
---

# Fleet TUI — daemon-authoritative roster stream + fleet/tail unification

> **Scope.** This doc is the concrete, code-grounded plan to move fleet **data**
> ownership out of the cockpit client and into the daemon, and to unify
> `bro fleet` and `bro tail` onto one roster. It **inherits** the UX and
> command-ordering requirements of
> [`backlog-multi-instance-coherence.md`](./backlog-multi-instance-coherence.md)
> and **answers its open owning-process question**: the owner is the existing
> singleton daemon, via the `task_store` + `tail_tx` it *already* holds — not a
> new per-store coordinator. It does **not** re-litigate the §2 UX contract or
> the §5–§6 seq-ordered command protocol from that doc; those are adopted by
> reference.

## 1. Problem — the cockpit owns a durable, shared, uncoordinated mirror

The daemon is already the single source of truth for **every** agent. There is
one task registry and one event broadcast, and both fleet-launched and
framework-dispatched agents land in them:

- `/control/exec` — the cockpit's dispatch path (`control_exec_handler`,
  `src/server/routes.rs:872`; mounted at `src/server/mcp.rs:68`, aliased
  `/irc/exec` at `:103`) creates a task in `state.task_store`.
- `bro_exec` / `bro_agent_dispatch` / workflow nodes / atoms create tasks in the
  **same** `state.task_store` and emit to the **same** `state.tail_tx`
  (`src/tools/orchestrate.rs:175-176` threads `state.task_store.clone()` +
  `state.tail_tx.clone()`; atoms/supervision read `state.task_store`).

Despite that, the cockpit keeps a **second, client-side** copy of fleet data:

- `FleetOrchestrator` holds a `TaskStore` **mirror** (the doc-comment's own word,
  `crates/bro-fleet-client/src/fleet.rs:1366`), persisted to an on-disk
  `store_dir`.
- The store path is **shared and deterministic, not per-instance**:
  `config::bro_home().join(store_name)` with `store_name = "fleet"`
  (`fleet.rs:1426`, via `from_config` `:1411`). Every `bro fleet` on the host
  with the same `BRO_HOME` resolves the **same** `$BRO_HOME/fleet/tasks.json`.
- Each cockpit independently **loads** it on launch
  (`TaskStore::load`, `fleet.rs:1429`), runs **one status poller per task**
  (`spawn_daemon_status_poller`, `fleet.rs:982`) that pulls
  `/control/status/{task_id}` and overwrites its in-memory window
  (`update_daemon_task` `fleet.rs:1266`; `inner.events = events.clone()` from
  `recentEvents` `:1293-1294`), and **persists the whole roster** back via
  `persist_all_events` → atomic `tmp`+`rename`
  (`crates/bro-fleet-client/src/task.rs:170-204`).

Consequences, each observed during the v2 dogfood campaign:

1. **Multi-instance clobber.** Two cockpits each rename a full-roster snapshot
   over the same `tasks.json` with no cross-process coordination — last writer
   wins. The campaign hit exactly this: two `bro fleet` processes "dueling over
   the same fleet store," producing unreliable roster display. This **defeats the
   headline capability** the in-daemon move was meant to unlock (multiple panes
   over one fleet).
2. **Poller zoo.** N agents × M instances each run a continuous per-task event
   poller, including for agents nobody is looking at. The roster only needs
   summaries; only the focused agent needs the event stream.
3. **The 80KB band-aid's root.** A verbose *unfocused* agent's `recentEvents`
   overflowed the daemon's MCP response cap and byte-truncated mid-JSON, silently
   freezing roster rows. The size-budget fix (`cf87a52`) hardened that path
   instead of removing the reason a roster poll carries event payloads at all.

The campaign's concurrency fixes — a single-writer `TaskPersister` actor,
`reconcile_reloaded`, the poller supervisor, panic isolation — all hardened the
mirror **within a single process**. None coordinate **across** cockpit
processes, because each silently assumed one cockpit. They are symptomatic
treatment of a store that should not be client-owned at all.

## 2. State taxonomy — one store conflates three owners

The defect is not "the cockpit holds state." A TUI instance is a **view**, and a
view legitimately owns view-state. The defect is that one store
(`$BRO_HOME/fleet/tasks.json`) conflates three things with three different
correct owners.

### Tier 1 — Fleet data (user-scoped, daemon-authoritative)

Roster membership; per-agent status / provider / model / cost / turns / cwd /
label / session; and the transcript. *Today:* persisted into the shared client
mirror and re-derived by the poller zoo. **Move to the daemon** (it already holds
the authoritative live copy). The client keeps an **in-memory** projection only.

**Event ownership is two distinct things — do not conflate them.** The daemon
owns the **live** event stream (broadcast on `tail_tx`), but its *persisted* task
snapshot caps events at `MAX_PERSISTED_EVENTS = 50`
(`src/orchestration/mod.rs:513`, applied in `serialize_snapshot` `:584`). So the
daemon task store is **not** a durable full event log — full deep transcript
history lives in the provider transcript files, locatable via
`transcript_location` (the same files `bbox_search`/index read). The cockpit's
soon-to-be-removed client mirror *did* persist all its mirrored events
(`crates/bro-fleet-client/src/task.rs:170`); nothing inherits that role, and it
should not — it was redundant with the provider transcripts.

Consequence for the design: a focused agent's transcript is hydrated from **two**
sources, not one — the daemon's *live* events for the tail, plus a **transcript
replay** (provider transcript file / a transcript adapter) for deep history
beyond the 50-event live window. The roster summary (Tier-1) never needs events at
all (§3).

### Tier 2 — Per-instance view state (ephemeral, in-memory — already correct)

Zone (roster vs. single-agent), focused agent, roster sort / cursor / anchor /
scroll, composer buffer + cursor, recall cursor, overlays — **and the instance's
inherited cwd / project context** (which biases dispatch defaults and `@project`
resolution; the project *registry* itself is user/daemon-scoped). *Today:* lives
in `App`, in-memory — keep it. View state is **non-sticky**: it dies with the
instance (no durable per-instance prefs file in scope).

This matches `backlog-multi-instance-coherence.md` §2.2's local-only list.

### Tier 3 — Composer input history (shared, user-scoped histfile)

*Today:* `input_history: Vec<String>` is **per-agent and in-memory**
(`crates/bro-cli/src/fleet_tui.rs:157`, pushed on send at `:2770/:2910/:2952`,
recalled ↑/↓ in single-agent view §5.3). It is lost on cockpit exit and invisible
to other instances — so instanceB zooming an agent instanceA has been driving
sees an **empty** recall list. Wrong shape for a user-scoped fleet.

**Decision: one shared, user-scoped histfile** spanning all `bro fleet` *and*
`bro agent` sessions (your prompts are your prompts), at
`$BRO_HOME/composer_history.jsonl` (above the per-surface `fleet/` / `agent/`
store dirs), mode `0600`.

Two correctness requirements, both forced by the now-multi-line composer
(bracketed-paste splices multi-line prompts into one buffer):

1. **JSONL records, one JSON object per physical line** (`{"ts":…,"text":"a\nb"}`,
   newlines escaped inside the string). Raw newline-delimited append would shred
   one pasted prompt into N bogus entries — the same failure shape as the original
   paste→mass-dispatch bug.
2. **`flock(LOCK_EX)` around append.** `O_APPEND` alone is only atomic for small
   writes; it will not protect a multi-KB pasted prompt from interleaving with a
   concurrent instance's append. Use `fs2`'s `lock_exclusive()` (already a repo
   dependency — flock under the hood; released on fd close/crash → no stale
   locks). Avoid `fcntl`/`F_SETLK` (the "close any fd drops all locks" footgun)
   and lockfiles (stale-lock risk). flock is **advisory and host-local**: it only
   serializes *cooperating* writers (all `bro` processes); non-cooperating writers
   to the histfile are unsupported.

**Trim must not truncate-in-place.** Capping to the last ~5000 entries by
truncate-and-rewrite risks corrupting the histfile on a crash mid-rewrite, even
under the lock. Instead: hold the *same* exclusive lock for the whole append, and
when a trim is due, write the compacted tail to a temp file (fsync where
practical) and `rename` it over the histfile **before releasing the lock** — the
same tmp+rename atomicity the task store already uses.

Behavior change opted into: ↑/↓ recall becomes **cross-agent** (all your prompts,
like a shell), not scoped to the focused agent's sends. Reads parse line-by-line
and skip a torn trailing record; dedup consecutive on read. This realizes the
input-history-persistence
follow-on noted in [`backlog-follow-ons.md`](./backlog-follow-ons.md) and the
"history entries are shared, cursor is local" split from
`backlog-multi-instance-coherence.md` §2.2.

## 3. Daemon-authoritative roster stream — kill the poller zoo

`/tail` **is** the roster stream, under-projected. `tail_handler`
(`src/server/tail.rs:109`) already subscribes to `state.tail_tx`, enriches each
event with bro/team/session attribution (`find_bro_ref_for_task`, `bro_label`,
`session_id`, `jsonl_path`), and filters by `teams` / `bros` / `sessions` /
`providers` (no selectors ⇒ all). The roster summary stream is the **same**
broadcast with a different fold: events → per-task **summary deltas**, not raw
event passthrough. The SSE machinery already exists
(`axum::response::sse::{Event, Sse}`, `tail.rs`).

New daemon surface:

1. **Roster summary DTO (phased by field provenance; no event payloads).** The
   DTO must not pretend the daemon already exposes everything:
   - **`RosterSummaryV1`** — fields the daemon already holds or derives directly:
     `task_id`, `status`, `provider`, `cost`, `turns`, `cwd`, `label`,
     `session_id`, `last_message_snippet`. `model` is **best-effort derived** from
     event payloads (the client does this today at
     `crates/bro-fleet-client/src/fleet.rs:1355`) and may be absent until a stored
     task field lands. `last_event_at` is **not** a daemon field today — the
     client stamps it from poller `eventCount` growth (`fleet.rs:1301`); V1 must
     derive it from event timestamps or add an explicit daemon stamp, not assume
     it exists.
   - **`RosterSummaryV2`** — additive nullable fields introduced by later slices:
     `origin` (Slice 1b), and `managed_worktree` + `owner` (Slice 2b, option (a)
     of §4.3).
   No event payloads in any version — this alone dissolves the 80KB poller-stall
   class (roster traffic stops carrying transcripts).
2. **`GET /control/roster`** — a one-shot snapshot of summary DTOs for all
   fleet tasks (initial render / reconcile-on-reconnect).
3. **Roster SSE — a snapshot-plus-versioned-delta protocol, not stream-only
   derivation.** `tail_tx` is a broadcast channel with lag/drop semantics and no
   replay cursor (`/tail` today just logs lag and continues,
   `src/server/tail.rs:239`), and today's `TailEvent` has only
   `start`/`progress`/`completed`/`failed`/`cancelled` variants
   (`src/orchestration/tail.rs:16`) — there is **no membership remove/prune/forget
   event**. So the roster cannot be derived from the live stream alone. The
   contract is: client fetches `/control/roster` (a monotonically versioned
   snapshot), then streams **explicit roster deltas** — new variants
   `RosterAdded` / `RosterUpdated` / `RosterRemoved` carrying the summary fields
   (the fold may consult `task_store` at emit time to fill cost/turns). On
   detecting a sequence gap or an SSE-lag signal, the client **re-fetches the
   `/control/roster` snapshot** and resumes from its version. The other tail
   summary fields (status change, last-message snippet, cost/turn tick,
   completion) ride `RosterUpdated`.
4. **Focused-transcript subscription — atomic snapshot-then-stream with a
   cursor.** Opened on zoom-in, dropped on zoom-out. It must close the head-race
   (events arriving between snapshot and stream start): the contract is "snapshot
   through event cursor `N`, then stream events strictly after `N`" (or
   equivalently, open the stream first and deliver the initial snapshot as its
   first message). It is **not** the existing `/control/status/{task_id}`, which
   delegates to `bro_status` (`src/server/routes.rs:1105`) and returns a *bounded*
   `recentEvents` blob (`src/orchestration/mod.rs:2985`), not a cursor-based
   replay API. Deep history beyond the live window is hydrated from the
   **transcript replay** source (§2 Tier 1), then committed to terminal-native
   scrollback via `insert_history` (today the inline scrollback reflows from
   `AgentHandle::transcript()`, `crates/bro-cli/src/fleet_tui.rs:1477`, which
   parses the client's local event buffer — removing that buffer is exactly why
   the focused subscription needs the stronger snapshot+cursor+replay contract).

Client change: the cockpit swaps **N `spawn_daemon_status_poller` instances → one
roster subscription per instance**, plus one focused-transcript stream for the
zoomed agent. Reconcile-on-reconnect becomes "fetch `/control/roster` snapshot,"
not "load a file and guess which `Running` rows are orphaned." This retires
`TaskStore::persist_all_events` / `load`, the `TaskPersister` actor, the
poller supervisor, and `reconcile_reloaded`-from-file. The shared-`tasks.json`
clobber disappears because instances **stop owning Tier-1 at all**.

## 4. Fleet/tail unification — `origin` facet + tabs

Because both surfaces already read one daemon registry, the fleet/tail split is
purely two **client consumers** of one stream. Collapse them into one tabbed
roster.

### 4.1 The one new schema field: task `origin`

There is **no** first-class origin/source enum on the daemon task today (greps
empty; PROJECT.md's `dispatch_origin` is an unrelated refactor-run flag). Add an
`origin` set at each creation site:

| Creation site | `origin` |
|---|---|
| `control_exec_handler` (`/control/exec`, `/irc/exec`) | `Cockpit` |
| `bro_exec` / `bro_agent_dispatch` | `AgentDispatch` |
| `orchestrate.rs` workflow nodes | `Workflow` |
| atoms | `Atom` |
| cron / webhook ingress | `Cron` / `Webhook` |

It rides the Tier-1 summary DTO and drives the tabs:

- **Tab "Fleet Agents"** — `origin == Cockpit` (default tab).
- **Tab "Dispatched Agents"** — everything else (framework-launched).

This is **not** a one-line change despite being additive: `origin` must be set at
~7 disjoint creation sites, added to `TaskInner` and to the wire
`bro_protocol::TaskSnapshot` (which today carries only
task/session/status/last_message/error, `crates/bro-protocol/src/lib.rs:46`), and
**persisted** so the tab survives a daemon restart and a reload-reconcile. The
creation-site audit is the real risk of the slice that introduces it (§5).

`tail`'s existing `team` / `bro` / `session` / `provider` selectors become the
roster's **filter facets**, orthogonal to the origin tabs.

### 4.2 Control verbs are already origin-agnostic

Steer / resume / interrupt / closeout / retro all run through `/control/*` (and
`bro_*`) against a daemon `task_id` — the daemon does not care who launched it.
So "spy on dispatched agents, steer/resume them, run retros" is already
*mechanically* possible; the cockpit simply has not been **showing** those tasks.
The fold-in surfaces addressable state; it does not build new control paths.

### 4.3 Disposition guards key on task properties, not on tab

The interaction set is *mostly* shared across origins, but it is **not "basically
identical"** — three dispositions need property-keyed gating, and **the gate
properties do not exist as daemon task state today**. That gap is itself part of
the work, not an assumption.

1. **Closeout / worktree-fold** — only valid for a task with a managed worktree
   and not actively owned by a live workflow/atom (folding a mid-workflow bro
   fights its orchestrator). The desired gate is
   `has_managed_worktree && !workflow_owned` — but **neither property is on
   `TaskInner` today** (`src/orchestration/mod.rs:320` has cwd/labels/report/
   transcript/supervision, none of these). The current closeout derives the
   worktree from the *client's* focused-agent `snapshot.cwd`
   (`crates/bro-cli/src/fleet_tui/closeout.rs:284`) and the daemon validates
   managed roots at **request time** (`src/server/routes.rs:976`) — a request-time
   refusal, not a roster-summary predicate. **Two options, pick one in the slice:**
   (a) add explicit `managed_worktree` + `owner`/`workflow_owned` task metadata
   (define creation-site population + persistence), so the roster row can disable
   the action up-front; or (b) interim: the row always *offers* preflight/attempt
   and surfaces the daemon's request-time refusal, with no proactive disable. (a)
   is the correct end-state; (b) ships without new task fields.
2. **Steer / interrupt / closeout of an orchestrator-owned bro** — mechanically
   possible, but **not safe as a soft warning**: a workflow/atom node may own the
   task's lifecycle and fire follow-on actions off its terminal state, so an
   operator interrupt can desync the orchestrator. Require an **explicit
   confirm/ack** on orchestrator-owned tasks (naming the owning workflow/atom) and
   **emit an ownership/audit event** the orchestrator can observe — not a passive
   toast. This is distinct from the agent-facing "don't stomp another session's
   bro" convention (which restrains *background automation*); the human operator
   *may* override, but only through the acked, audited path. Note this leans on
   the ownership metadata from (1) and is adjacent to the seq-ordered command
   protocol that §7 scopes out — until that lands, treat operator mutation of
   orchestrator-owned tasks as confirm-gated, not free.
3. **Prune / cleanup races** — a framework bro may be pruned by its orchestrator;
   terminal-only prune + ownership awareness covers it.

### 4.4 `bro tail` CLI disposition

Fold the interactive/spy TUI into the `bro fleet` tabs. Keep a thin **headless
`bro tail` stream-printer** (the existing SSE consumer, no TUI) for
piping/scripting/logging. Retire only the redundant interactive surface.

## 5. Implementation slices

Ordered so each is independently shippable and dogfoodable.

**Slice 0 — Shared composer histfile (warmup; client-only).** Replace per-agent
in-memory `input_history` with the `flock`'d JSONL histfile (§2 Tier 3). No
daemon change. Independent of the roster work; immediately useful.

**Slice 1a — `RosterSummaryV1` DTO + `/control/roster` snapshot.** Define V1 over
fields the daemon already holds (status / provider / cost / turns / cwd / label /
session / last-message snippet), with `model` best-effort-derived from events and
`last_event_at` derived-or-daemon-stamped (§3 item 1), and add the snapshot
endpoint. No new task fields, no client behavior change yet.

**Slice 1b — `origin` plumbing (cross-cutting; the audit IS the work).** Add
`origin` to `TaskInner` and `bro_protocol::TaskSnapshot`, set it at every creation
site (`/control/exec`, `bro_exec`, `bro_agent_dispatch`, workflow, atoms, cron,
webhook), persist it, and test restart-survival. This is the risk-bearing slice
(§4.1) — call it out as a creation-site audit, not a one-liner. Tabs depend on it.

**Slice 2 — Roster SSE + client subscription.** Add the `RosterAdded/Updated/
Removed` deltas + snapshot-version/lag resync (§3 item 3); switch the cockpit from
N pollers to one roster subscription + one focused-transcript stream with the
snapshot-then-cursor contract + transcript replay (§3 item 4). Retire client
persist/load/supervisor/reconcile-from-file. Reconcile = snapshot fetch.

**Slice 2b — Worktree/owner task metadata (option (a) of §4.3; optional).** Add
`managed_worktree` + `owner`/`workflow_owned` to `TaskInner` and `TaskSnapshot`
(populated at creation, persisted) → the `RosterSummaryV2` fields, so a roster row
can proactively disable an invalid closeout. If shipping the interim instead, skip
this slice and rely on option (b) (preflight/attempt + daemon request-time
refusal), in which case the V2 worktree/owner DTO fields are omitted.

**Slice 3 — Tabs + facets + guards.** Render Fleet / Dispatched tabs from `origin`
(requires Slice 1b); wire `team`/`bro`/`session`/`provider` filter facets; apply
§4.3 disposition guards — proactive disable when Slice 2b landed, else
preflight+refusal, with confirm/ack + audit on orchestrator-owned tasks either
way.

**Slice 4 — Fold `bro tail`.** Reduce the CLI verb to a headless stream-printer;
remove the redundant interactive tail surface; doc the new tabbed cockpit.

## 6. Acceptance criteria

- Two `bro fleet` instances against the same `BRO_HOME/fleet` show the same
  roster, and **neither persists a shared `tasks.json`** — there is no
  client-owned Tier-1 store to clobber. (Realizes
  `backlog-multi-instance-coherence.md` §11 acceptance #1 without a file race.)
- A verbose unfocused agent never freezes a roster row: roster traffic carries no
  event payloads, so the 80KB truncation class cannot occur (regression-test the
  DTO has no events field).
- A framework-dispatched bro (origin ≠ `Cockpit`) appears under "Dispatched
  Agents," is zoomable, and is steerable/resumable from the cockpit; when it is
  workflow/atom-owned, the mutation is confirm/ack-gated and audited (§4.3), not a
  silent action.
- Closeout refuses on a task without a managed worktree regardless of tab.
- ↑/↓ recall in any instance returns prompts typed in any other instance
  (shared histfile), and a multi-line pasted prompt round-trips as one entry.
- Zoom-in opens exactly one focused-transcript stream; zoom-out drops it; the
  roster runs one subscription per instance (no per-agent poller fan-out).
- **Daemon restart** with running/recoverable tasks: a reconnecting cockpit
  rebuilds its roster from `/control/roster` (incl. correct `origin`/tab and
  persisted summary), with no client-side orphan-guessing.
- **SSE lag / sequence gap** forces a `/control/roster` resync rather than a
  silently stale roster (regression-test the lag path).
- **Focused transcript hydration** loses no events at the snapshot→stream
  boundary (the cursor contract), and deep history beyond the 50-event live
  window reflows from the transcript-replay source, not the daemon snapshot.
- **`origin` survives** persistence + reload (a `Workflow` bro does not reappear
  as a `Cockpit` agent after a daemon restart).
- **Orchestrator-owned override** is confirm/ack-gated and emits an audit event;
  it is not performable as a silent passive action.

## 7. Non-goals

- The §5–§6 seq-ordered **command** protocol of
  `backlog-multi-instance-coherence.md` (ordered concurrent steer/interrupt
  resolution). That is adopted by reference and remains its own work; this doc
  covers the **read/observe** plane (roster data ownership) and the surface
  unification, not the write-ordering plane.
- Cross-machine fleet sharing; collaborative composer-draft editing.
- Sticky per-instance view-state prefs (explicitly out: view state dies with the
  instance).
- Provider quota/headroom routing.

## 8. Relationship

- Predecessor whose UX (§2) and command protocol (§5–§6) this inherits, and whose
  owning-process question this answers (the singleton daemon, via existing
  `task_store`/`tail_tx`):
  [`backlog-multi-instance-coherence.md`](./backlog-multi-instance-coherence.md).
  That doc's "local fleet coordinator, not blackboxd" decision (its §3) is
  **superseded** here: the coordinator is the daemon the cockpit already drives
  over `/control/*`; the harness/daemon boundary relaxation
  ([`../bro-harness/harness-daemon-boundary.md`](../bro-harness/harness-daemon-boundary.md))
  is what makes that collapse legal.
- Hub: [`fleet-tui.md`](./fleet-tui.md). **This doc supersedes the hub's stated
  "only hard line is daemon RPC — no HTTP to a running `blackboxd`" invariant**
  (`fleet-tui.md:17`): the cockpit already drives the daemon over `/control/*`
  (`/control/exec` etc.), and this design makes that the *owning* path for Tier-1
  data. The relaxation is licensed by
  [`../bro-harness/harness-daemon-boundary.md`](../bro-harness/harness-daemon-boundary.md);
  the hub line is now stale and a hub update is a planned follow-up of this work
  (do not let a reader take the hub's invariant as current). As-built cockpit:
  [`fleet-tui-cockpit.md`](./fleet-tui-cockpit.md) (§3 in-process model, §5 state
  model, §7 `FleetOrchestrator`).
- Input-history persistence follow-on realized by §2 Tier 3:
  [`backlog-follow-ons.md`](./backlog-follow-ons.md).
- Snapshot testing for the new tabbed/streamed states:
  [`ratatui-snapshot-preview.md`](./ratatui-snapshot-preview.md).
