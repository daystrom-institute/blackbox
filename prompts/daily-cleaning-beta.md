---
title: "Daily Cleaning (beta/blackbox-v2)"
kind: operator-prompt
corpus: blackbox-prompts
audience: operator
topic:
  - prompts
  - maintenance
brief: "Start-of-day environment reset for the beta/blackbox-v2 line: sync to latest beta/blackbox-v2, prune landed manual worktrees, full cargo clean, cold rebuild + reinstall the prod daemon/bro/bro-harness, restart the prod service. Operator-invoked and intentionally destructive — the operator running it interactively IS the authorization. Beta-line sibling of daily-cleaning.md (which tracks main)."
---

# Daily Cleaning (beta/blackbox-v2)

Reset the local environment to a fresh, current state at the start of a day,
tracking the **`beta/blackbox-v2`** integration branch instead of `main`. This
prompt is **operator-pointed and intentionally destructive**: it discards the
build cache, prunes worktrees, and restarts the production daemon. It is safe
*because a human invokes it interactively* and accepts those effects — do not
wire it into an unattended schedule without revisiting the gates below.

This is the beta-line sibling of [`daily-cleaning.md`](daily-cleaning.md). The
only difference is the integration branch: everywhere the main variant syncs to
and measures landing against `main`, this one uses `beta/blackbox-v2`. F3/F4
still rebuild and restart the **production** daemon from the beta code (the
operator's running prod tracks beta).

You are operating in the **main worktree** (`~/repos/transcript-search`, branch
`beta/blackbox-v2`). Run phases in order. Stop and surface anything that trips a
safety gate rather than forcing past it.

> **Multi-tenant invariant.** This host runs multiple concurrent Claude accounts
> and background bros against shared working state and the shared prod daemon.
> Never discard, stash, or rebase over files this session did not create. Never
> auto-remove a worktree that has uncommitted changes — that is a peer's
> in-flight work. The only file mutations you author are your own.

## F0 — Sync to latest beta/blackbox-v2

1. Confirm a clean tree. If `git status --porcelain` is non-empty, **stop**:
   list the dirty paths and ask the operator. Those may be a peer agent's
   uncommitted work; do not stash or discard them to clear the rebase.
2. Confirm you are on `beta/blackbox-v2` (`git rev-parse --abbrev-ref HEAD`). If
   not, **stop** and surface — this prompt is for the beta line only.
3. Fetch and rebase onto the remote head:

   ```bash
   git status --porcelain                  # must be empty to proceed
   git fetch origin --prune
   git rebase origin/beta/blackbox-v2      # beta → latest; abort with `git rebase --abort` on conflict and surface
   ```

   `origin` is `git@github.com:daystrom-institute/blackbox.git`. If the rebase
   conflicts, abort and report — do not hand-resolve during a cleaning pass.

## F1 — Worktree survey + prune

Survey every worktree, classify it, and prune only the safe ones.

```bash
git worktree list
```

For each worktree **other than the main checkout** (`~/repos/transcript-search`):

- **Scope filter.** Worktrees under
  `~/.local/state/blackbox/bro/fleet/worktrees/…` are **fleet-managed and out of
  scope** — they may belong to a live `bro fleet` session. *List them in the
  report, never prune them here.* Only manual worktrees under `~/repos/*` are
  eligible for auto-prune.
- **Landed?** Closeout is fast-forward-only, so a landed branch's HEAD is an
  ancestor of `beta/blackbox-v2`:

  ```bash
  git merge-base --is-ancestor <wt-head-sha> beta/blackbox-v2   # exit 0 = landed
  git cherry beta/blackbox-v2 <branch> | grep -q '^+' || echo "all commits equivalent in beta"  # rebase/squash fallback
  ```

- **Clean?** Run `git -C <wt-path> status --porcelain`; empty = clean.

Classification → action:

| Class | Condition | Action |
|-------|-----------|--------|
| **Reclaim** | manual worktree, landed **and** clean | `git worktree remove <path>` then delete the merged branch `git branch -d <branch>`. Reclaims its `target/` too. |
| **Landed-but-dirty** | landed but `status` non-empty | **Report only.** Uncommitted peer work — do not remove. |
| **True orphan** | not landed | **Report only.** Has unlanded commits (`git rev-list --count beta/blackbox-v2..<branch>`). The operator decides. |
| **Fleet** | path under `…/fleet/worktrees/…` | **Report only**, regardless of class. |

After pruning, `git worktree prune` to clear stale admin entries.

## F2 — Full cargo clean

Full clean of the workspace build cache (a cold rebuild is acceptable). Main's
`target/` is the large reclaim (tens of GB).

```bash
du -sh target 2>/dev/null          # record before
cargo clean
```

## F3 — Cold rebuild + reinstall (prod)

Rebuild release binaries and reinstall the **production** surfaces:
`blackboxd`, `bro`, and `bro-harness`. (Do **not** install to the `-dev`
binary; this resets prod from the beta code.)

```bash
cargo build --release                 # blackboxd, bro-slack (root package)
cargo build --release -p bro-cli      # bro (fleet/orchestration CLI — split out of the root package 2026-06-03)
cargo build --release -p bro-harness  # bro-harness (workspace member crate)

ls -l target/release/blackboxd target/release/bro target/release/bro-harness   # ALL THREE must exist, freshly built

install -m 755 target/release/blackboxd   ~/.local/bin/blackboxd
install -m 755 target/release/bro         ~/.local/bin/bro
install -m 755 target/release/bro-harness ~/.local/bin/bro-harness
```

> ⛔ **If any expected binary is missing from `target/release/` after the
> builds, STOP and surface it.** Do not narrow the install list to make the
> command succeed, and do not infer an explanation (e.g. "it must be a
> subcommand now") — a missing binary almost always means the bin target moved
> to a different workspace crate (exactly what happened when `bro` moved to
> `bro-cli`, dfa907a). Find the crate that declares the bin
> (`grep -rl 'name = "bro' crates/*/Cargo.toml Cargo.toml`), build it with
> `-p <crate>`, and install from there. A skipped reinstall leaves a stale
> binary that silently passes `bro --version` while running week-old code.

Refresh runtime-loaded system memories to the current checkout (part of a full
reset). `cp` is interactive-aliased on this host — bypass it:

```bash
install -d ~/.local/share/blackbox/memories
command cp -af system-defaults/memories/. ~/.local/share/blackbox/memories/
```

## F4 — Restart prod daemon  ⛔ GATED

`blackbox.service` is a **shared service** other accounts and background bros
depend on. Restarting it mid-session disrupts them — and here you are cutting
**beta code** into it, so confirm the operator actually wants beta running as
prod. Before restarting, do a read-only scope check and **get explicit operator
confirmation for the named service** even though the operator invoked this
prompt — the cleaning authorizes the *rebuild*, this gate authorizes the
*cutover*.

```bash
systemctl --user status blackbox.service          # read-only scope check first
# --- confirm with operator, then: ---
systemctl --user restart blackbox.service
systemctl --user is-active blackbox.service        # expect: active
curl -fsS "127.0.0.1:${BBOX_PORT:-7264}/roster" >/dev/null && echo "daemon answering"
```

## F5 — Report

Return a tight summary:

- **Synced:** beta rebased onto `origin/beta/blackbox-v2` (or "already
  current"); any conflict surfaced.
- **Worktrees:** reclaimed (path + branch), landed-but-dirty (reported, paths),
  true orphans (path + branch + `beta/blackbox-v2..branch` commit count), fleet
  worktrees (listed, untouched).
- **Disk:** `target/` size before → after clean; total reclaimed.
- **Installed:** `blackboxd --version` / `bro --version` / `bro-harness`
  version, PLUS the install mtimes
  (`ls -l ~/.local/bin/blackboxd ~/.local/bin/bro ~/.local/bin/bro-harness`) —
  all three must postdate this cleaning run; `--version` alone cannot detect a
  stale binary. Note whether the prod service was restarted (or deferred at the
  gate).
- **Daemon health:** `is-active` + `/roster` probe result.

Keep the report operational; do not narrate every command.
