---
title: "bro-harness tool surface — the ideal built-in subset"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - orchestration
  - surfaces
brief: "Defines the target built-in tool subset for bro-harness, derived from the superset of capabilities across the Claude Code and Codex CLIs. Inventories what bro-harness has today (verified arg shapes), what CC and Codex offer, and gives a per-capability verdict: adopt / already-have / skip — with shape notes."
---

# bro-harness tool surface — the ideal built-in subset

> **Status.** Proposed. bro-harness column verified against
> `crates/bro-tools/src/{workspace.rs,web.rs,lib.rs}` and
> `crates/bro-harness/src/registry.rs`. Codex column verified live via a Codex
> dispatch reporting its own schema (2026-05-29). CC column from the Claude Code
> harness tool schemas.

## Goal

bro-harness should carry the **smallest built-in set that covers the real work**,
choosing the best *shape* (args, modes) for each tool from across CC and Codex
rather than copying either wholesale. This doc is the guardrail for what belongs
in `builtin_tools()` and how each tool's arg surface should look.

## What bro-harness has today (verified)

`crates/bro-tools/src/lib.rs::builtin_tools()` returns 14 tools; the registry
also pins the daemon's `bbox_slice_*` (MCP) and adds the `tool_search` meta-tool.

| Tool | Args (verified) | Notes / limits |
|---|---|---|
| `file_read` | `file_path, start_line?, end_line?` | 1-based inclusive range. **No output cap; reads whole file then line-filters.** No multimodal. |
| `smart_read` | `file_path, max_full_lines?=400` | Small files whole; large files → definition outline + 40-line head. **No CC/Codex equivalent — bro is ahead.** |
| `file_write` | `file_path, content` | Creates parent dirs; overwrite. |
| `file_edit` | `file_path, old_string, new_string, replace_all?=false` | Unique-match-or-fail; rejects identical old/new. **Matches CC `Edit`.** |
| `list_dir` | `path?` | Immediate entries. |
| `content_search` | `pattern (regex), path?, glob?, max_results?=200 (cap 5000)` | gitignore-aware; returns `relpath:line:text`. **No output_mode / context-lines / count mode.** |
| `glob` | `pattern, path?` | gitignore-aware; **alphabetical sort** (not mtime); cap 2000. |
| `shell_run` | `command, cwd?` | `bash -lc`; `SafetyPolicy::deny_command` gate. **No timeout / background / output cap / stdin / session.** |
| `git_status/log/diff` | — | `log` = `--oneline -20`. |
| `git_show` | `rev?=HEAD` | |
| `git_commit` | `message, paths[]` | Explicit paths only (no `git add .`); sensitive-file refusal. |
| `web_fetch` | (see `web.rs`) | provider-executed web-search is intentionally absent (`lib.rs:22`). |
| `bbox_slice_*` | (daemon, MCP, pinned) | structural source→target edits with sha-guard + dry-run. |
| `tool_search` | `query` | three-tier deferral; loads deferred tools on demand (`registry.rs`). |

Safety baseline: `resolve_in_root` confines every path to `cx.root` (rejects
`..`, absolute-outside, with lexical normalization); shell via
`SafetyPolicy::deny_command`; commit-time sensitive-path refusal.

## Live probe feedback (2026-05-29, deepseek-v4-pro)

A deepseek-v4-pro bro was dispatched through the daemon against the freshly
installed harness to exercise the surface and self-report as a tool user. It saw
the full surface (built-ins + pinned `bbox_slice_*` + `tool_search` +
server-side `web_search`), and `todo_write` round-tripped across turns. Verdict:
"coherent, descriptions honest about behavior." Findings folded in:

**Applied this pass (cheap fixes):**
- `file_read` now takes `line_numbers` (cat -n style prefixes) — the probe called
  out that a line-addressed tool returning no line numbers is friction.
- `content_search` now takes `case_insensitive` (ripgrep `-i`) — "I'd reach for
  this constantly."
- `glob` description now states the mtime default loudly — the probe found the
  default-mtime order a "footgun" it only caught by re-reading the schema. The
  default is kept (deliberate), but no longer silent.
- `TodoItem.task` replaces `content` (with `content`/`description` serde aliases
  for Claude Code parity) — "content for a task description feels off."

**Deferred (tracked, not done this pass):**
- `shell_run` `timeout`/`env` — already in the Tier-B adopt list below.
- `file_delete`, `file_rename`/`file_move`, `mkdir` — file management ops the
  probe flagged as "table stakes." Candidate Tier-C; weigh against `shell_run`
  covering them and the SafetyPolicy surface.
- `git_branch`/`git_checkout`/`git_stash` (and documenting the deliberate absence
  of `git_reset`/`git_restore`).
- Arbitrary-file `diff` tool (only `git_diff` exists).
- `smart_read` rename (probe found the name vague; behavior rated good).

## Resolved decisions (2026-05-29)

Operator-confirmed for v1:

- **`shell_run` async = Codex yield-poll, plus gap-fills.** **DONE**
  (`crates/bro-tools/src/shell.rs`). Three tools:
  - `shell_run{command,cwd,timeout_ms,yield_time_ms,max_output_tokens,stdin,close_stdin}`
  - `shell_poll{session_id,stdin,close_stdin,yield_time_ms,max_output_tokens}`
  - `shell_kill{session_id,signal(term|int|kill),grace_ms,max_output_tokens}`

  Plus `shell_list{}` → live sessions (`session_id`, command, elapsed) to
  recover a lost id or find orphaned processes. `shell_run` also takes an `env`
  map (merged onto inherited env). `shell_kill` reports `signal_sent` +
  `escalated_to_sigkill` (the latter means the requested signal was ignored and
  we force-killed after grace — NOT merely "SIGKILL was requested").

  Cooperative, synchronous from the loop; true-background + wake stays deferred
  to Stage 3 of the chaining design. Sessions are in-memory, single-`run()`
  lifetime (live children can't serialize into `side`), `kill_on_drop`, capped at
  32 concurrent. Output capped tail-first (errors trail); `timed_out` always
  present; `exit_code` null until termination.

  **Correctness fixes found during the hunt (beyond verbatim Codex parity):**
  - *Reader-hang bug:* a command that backgrounds a pipe-inheriting process
    (`cmd &`) keeps stdout open after the direct child exits; awaiting readers to
    EOF would hang the agent loop forever. `drain_final` now bounds the wait
    (`READER_DRAIN_GRACE`) and aborts stragglers.
  - *`close_stdin`:* read-until-EOF commands (`cat`, `sort`) could never finish
    without an EOF; added on both run and poll.
  - *`shell_kill`:* the only prior way to stop a non-timeout session was
    `run()`-end drop; now there's an in-dispatch signal+reap with SIGKILL
    escalation after `grace_ms`.
  - *Session cap:* prevents unbounded live-child accumulation.

  `stdout_to`/`stdin_from` clipboard chaining args intentionally deferred to the
  (separately deferred) clipboard work.
- **Per-call escalation = deferred.** Keep the static `SafetyPolicy`
  (`resolve_in_root` + `deny_command` + sensitive-path refusal) for now. Spec
  escalation; do not build it this pass — it overlaps the brofile allow/deny
  layer and would create two competing privilege systems.
- **`todo_write` = adopt** as a harness built-in.
- **Multimodal read = skip v1.**

## The superset (CC ∪ Codex) and the verdict

| Capability | Claude Code | Codex | bro today | **Verdict** |
|---|---|---|---|---|
| Read line-range | `offset`+`limit` | shell `sed` | `start_line/end_line` | **have** |
| Read output cap | implicit 2000-line | `max_output_tokens` | **none** | **adopt** — token/line cap on `file_read` |
| Read multimodal | images, PDF `pages`, notebooks | `view_image` | none | **skip v1** — niche for headless coding |
| Semantic read | — | — | `smart_read` | **have (lead)** |
| Edit replace_all + unique-or-fail | yes | apply_patch (no replace_all) | `file_edit` | **have** |
| Multi-file atomic patch | — | apply_patch `Move to:` | `bbox_slice_*` + refactor::apply | **have (lead)** — do **not** adopt apply_patch |
| Shell timeout | `timeout` (ms) | `yield_time_ms` | **none** | **adopt** |
| Shell background / long-running | `run_in_background` + wake; `Monitor` | `session_id` + `write_stdin` poll | **none** | **adopt** — start with Codex's yield-poll shape |
| Shell output cap | implicit | `max_output_tokens` | **none** | **adopt** |
| Shell stdin | — | `write_stdin` | none | **adopt** (with poll) |
| Per-call privilege escalation | — | `sandbox_permissions` + `justification` + `prefix_rule` | static `SafetyPolicy` | **defer** — spec only; keep static policy (overlaps brofile allow/deny) |
| Grep output modes | `output_mode` (content/files/count), `-A/-B/-C`, `-n`, `type`, `multiline`, `head_limit` | shell `rg` | content-only | **adopt subset** — `mode` + context-lines |
| Glob sort | mtime | shell | alphabetical | **adopt** — mtime option |
| Todo / plan | `TodoWrite` | `update_plan` | **none** | **adopt** — see below |
| Deferred tool discovery | Tool Search | `tool_search` | `tool_search` | **have** |
| Parallel tool calls | (native) | `multi_tool_use.parallel` | (native batching) | **have** |
| Token-budget primitive | — | `create_goal{token_budget}` | none | **skip v1** — orchestrator owns budget |
| Web verticals (finance/weather/sports) | — | `web.run` verticals | none | **skip** — irrelevant |
| Image generation | — | `image_gen` | none | **skip** |
| Scheduled wakeup / cron | `ScheduleWakeup`, `Cron*` | — | none (daemon owns crons) | **skip in harness** — daemon's job |

## Adopt list — target shapes

### `shell_run` (highest-value change)

Add the Codex-style cooperative long-running model plus caps and escalation:

```
shell_run {
  command, cwd?,
  timeout_ms?,            // hard kill
  yield_time_ms?,        // return partial output + session_id after N ms
  max_output_tokens?,    // cap returned stdout/stderr (default ~4k)
  stdin?,                // initial stdin
  stdout_to?,            // Stage-2 chaining: stdout → clip register (see tool-chaining)
  stdin_from?,           // Stage-2 chaining: register → stdin
}
  -> { exit_code?, stdout, stderr, session_id?, running: bool }
+ shell_poll { session_id, stdin?, max_output_tokens?, yield_time_ms? }
```

Rationale: `yield_time_ms` + `session_id` polling is the pragmatic middle path
between block-forever and a full background Task registry — it covers builds,
servers-that-need-a-poke, and REPLs without the async/wake machinery. The
true-background + wake form (CC `run_in_background`) is deferred to Stage 3 of
the chaining design, when the harness has a Task abstraction.

`escalate`/`justification` are intentionally **not** in this shape — per-call
escalation is deferred (see Resolved decisions). The static `SafetyPolicy` gate
remains the privilege model for v1.

### `file_read`

Add `max_output_tokens?` (or `max_lines?`) and stream-slice rather than reading
the whole file into memory before filtering. Add `into?` (Stage-2 chaining).

### `content_search`

Add `mode? = content | files | count` and `context_lines?` (symmetric `-C`).
Keep the regex + gitignore-aware walk. This closes most of the CC `Grep` gap
without the full flag surface.

### `glob`

Add `sort? = name | mtime` (default mtime, matching CC's "most recently edited
first" affordance).

### `todo_write` (new tool)

Both peers expose a plan surface and bro has none. Adopt the Codex/CC shape:

```
todo_write { items: [{ content, status: pending | in_progress | completed }] }
```

At most one `in_progress`. Cheap, improves multi-step coherence, and the daemon
can surface it via `bro_report`/dashboards.

**Durable, same mechanism as the clipboard** (operator-confirmed 2026-05-29): the
todo list is a loop-level side-cell persisted in the `SessionStore` file
(sibling key, not inside the transport `snapshot`), so it survives
`exec → resume`. See the persistence integration in
[`bro-harness-clipboard.md`](./bro-harness-clipboard.md) — the same
`Restored` field + `save(&SaveState)` widening carries both cells.

### Per-call escalation (deferred — spec only)

The Codex model: run `shell_run` (and any future networked tool) least-privilege
by default; require `escalate:true` + `justification` for mutating or networked
commands, recorded for audit. **Not built in v1** — it overlaps the brofile
allow/deny layer and the static `SafetyPolicy`, and shipping it now would create
two competing privilege systems. Revisit once the privilege model is unified;
the `shell_run` shape can gain the two fields without breaking callers.

## Explicit skips (and why)

- **`apply_patch` as primary editor** — weaker than what bro already has: no
  `replace_all`, unspecified ambiguous-match behavior, no sha-guard, no parse
  validation. The `bbox_slice_*` + `refactor::apply` plane is strictly better;
  its `bbox_slice_move` already covers apply_patch's `Move to:`.
- **Web verticals, image-gen, multimodal read** — out of scope for a headless
  coding harness.
- **Token-budget tools** — budget is an orchestrator concern (the daemon's
  allocation/supervision layer), not a per-agent built-in.
- **Scheduled wakeup / cron** — the blackbox daemon owns scheduling
  (`crons.rs`, `pollers.rs`); the harness should not duplicate it.

## Relationship to the other harness docs

- The chaining args (`stdout_to`, `into`, `from`, `stdin_from`) are specified in
  [`bro-harness-tool-chaining.md`](./bro-harness-tool-chaining.md) and gated on
  the `RefKind` tag.
- The clipboard (`clip_*`) tools in
  [`bro-harness-clipboard.md`](./bro-harness-clipboard.md) are the settled-ref
  backing store those args write to.
- Transport / loop / tiering context: [`anthropic-harness.md`](./anthropic-harness.md).
