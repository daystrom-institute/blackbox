---
title: "Daily Cleaning"
kind: operator-prompt
corpus: blackbox-prompts
audience: operator
topic:
  - prompts
  - maintenance
brief: "Start-of-day environment reset: sync to latest main, prune landed manual worktrees, full cargo clean, cold rebuild + reinstall the prod daemon/bro/bro-harness, restart the prod service. Operator-invoked and intentionally destructive — the operator running it interactively IS the authorization."
---

# Daily Cleaning

Reset the local environment to a fresh, current state at the start of a day.
This prompt is **operator-pointed and intentionally destructive**: it discards
the build cache, prunes worktrees, and restarts the production daemon. It is
safe *because a human invokes it interactively* and accepts those effects — do
not wire it into an unattended schedule without revisiting the gates below.

You are operating in the **main worktree** (`~/repos/transcript-search`, branch
`main`). Run phases in order. Stop and surface anything that trips a safety gate
rather than forcing past it.

> **Multi-tenant invariant.** This host runs multiple concurrent Claude accounts
> and background bros against shared working state and the shared prod daemon.
> Never discard, stash, or rebase over files this session did not create. Never
> auto-remove a worktree that has uncommitted changes — that is a peer's
> in-flight work. The only file mutations you author are your own.

## F0 — Sync to latest main

1. Confirm a clean tree. If `git status --porcelain` is non-empty, **stop**:
   list the dirty paths and ask the operator. Those may be a peer agent's
   uncommitted work; do not stash or discard them to clear the rebase.
2. Fetch and rebase onto the remote head:

   ```bash
   git status --porcelain          # must be empty to proceed
   git fetch origin --prune
   git rebase origin/main          # main → latest; abort with `git rebase --abort` on conflict and surface
   ```

   `origin` is `git@github.com:daystrom-institute/blackbox.git`. If the rebase
   conflicts, abort and report — do not hand-resolve during a cleaning pass.

## F1 — Worktree survey + prune

Survey every worktree, classify it, and prune only the safe ones.

```bash
git worktree list                  # rtk may render compact: "<path> <sha> [branch]"
```

For each worktree **other than the main checkout** (`~/repos/transcript-search`):

- **Scope filter.** Worktrees under
  `~/.local/state/blackbox/bro/fleet/worktrees/…` are **fleet-managed and out of
  scope** — they may belong to a live `bro fleet` session. *List them in the
  report, never prune them here.* Only manual worktrees under `~/repos/*` are
  eligible for auto-prune.
- **Landed?** Closeout is fast-forward-only, so a landed branch's HEAD is an
  ancestor of `main`:

  ```bash
  git merge-base --is-ancestor <wt-head-sha> main   # exit 0 = landed
  git cherry main <branch> | grep -q '^+' || echo "all commits equivalent in main"  # rebase/squash fallback
  ```

- **Clean?** Run `git -C <wt-path> status --porcelain`; empty = clean.

Classification → action:

| Class | Condition | Action |
|-------|-----------|--------|
| **Reclaim** | manual worktree, landed **and** clean | `git worktree remove <path>` then delete the merged branch `git branch -d <branch>`. Reclaims its `target/` too. |
| **Landed-but-dirty** | landed but `status` non-empty | **Report only.** Uncommitted peer work — do not remove. |
| **True orphan** | not landed | **Report only.** Has unlanded commits (`git rev-list --count main..<branch>`). The operator decides. |
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
binary; this resets prod.)

```bash
cargo build --release                 # blackboxd, bro, bro-irc, bro-slack (root package)
cargo build --release -p bro-harness  # bro-harness (workspace member crate)

install -m 755 target/release/blackboxd   ~/.local/bin/blackboxd
install -m 755 target/release/bro         ~/.local/bin/bro
install -m 755 target/release/bro-harness ~/.local/bin/bro-harness
```

Refresh runtime-loaded system memories to the current checkout (part of a full
reset). `cp` is interactive-aliased on this host — bypass it:

```bash
install -d ~/.local/share/blackbox/memories
command cp -af system-defaults/memories/. ~/.local/share/blackbox/memories/
```

## F4 — Restart prod daemon  ⛔ GATED

`blackbox.service` is a **shared service** other accounts and background bros
depend on. Restarting it mid-session disrupts them. Before restarting, do a
read-only scope check and **get explicit operator confirmation for the named
service** even though the operator invoked this prompt — the cleaning authorizes
the *rebuild*, this gate authorizes the *cutover*.

```bash
systemctl --user status blackbox.service          # read-only scope check first
# --- confirm with operator, then: ---
systemctl --user restart blackbox.service
systemctl --user is-active blackbox.service        # expect: active
curl -fsS "127.0.0.1:${BBOX_PORT:-7264}/roster" >/dev/null && echo "daemon answering"
```

## F5 — Report

Return a tight summary:

- **Synced:** main rebased onto `origin/main` (or "already current"); any
  conflict surfaced.
- **Worktrees:** reclaimed (path + branch), landed-but-dirty (reported, paths),
  true orphans (path + branch + `main..branch` commit count), fleet worktrees
  (listed, untouched).
- **Disk:** `target/` size before → after clean; total reclaimed.
- **Installed:** `blackboxd --version` / `bro --version` / `bro-harness`
  version, and whether the prod service was restarted (or deferred at the gate).
- **Daemon health:** `is-active` + `/roster` probe result.

Keep the report operational; do not narrate every command.
