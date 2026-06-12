---
title: "Daily Cleaning (beta/blackbox-v2, macOS)"
kind: operator-prompt
corpus: blackbox-prompts
audience: operator
topic:
  - prompts
  - maintenance
  - macos
brief: "macOS sibling of daily-cleaning-beta.md. Same start-of-day reset to beta/blackbox-v2, but the prod daemon is a launchd service under ~/Library/LaunchAgents/ (com.daystrom.blackbox) and is restarted with `launchctl kickstart -k`, not `systemctl --user`. Linux hosts with systemd should use daily-cleaning-beta.md as-is."
---

# Daily Cleaning (beta/blackbox-v2, macOS)

Reset the local environment to a fresh, current state at the start of a day,
tracking the **`beta/blackbox-v2`** integration branch instead of `main`. This
prompt is the **macOS** sibling of [`daily-cleaning-beta.md`](daily-cleaning-beta.md)
— everything is identical except **F4 (daemon restart)**, which uses `launchctl`
against the per-user LaunchAgent at `~/Library/LaunchAgents/com.daystrom.blackbox.plist`
instead of `systemctl --user restart blackbox.service`. Use
[`daily-cleaning-beta.md`](daily-cleaning-beta.md) on Linux hosts; use this one
on macOS.

This prompt is **operator-pointed and intentionally destructive**: it discards
the build cache, prunes worktrees, and restarts the production daemon. It is
safe *because a human invokes it interactively* and accepts those effects — do
not wire it into an unattended schedule without revisiting the gates below.

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
git worktree list                  # rtk may render compact: "<path> <sha> [branch]"
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

Full clean of the workspace build cache (a cold rebuild is acceptable). On
macOS, `du -sh target` may take a few seconds to walk a multi-GB directory —
that's expected, not a hang.

```bash
du -sh target 2>/dev/null          # record before (may be slow on large dirs)
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

On macOS the prod daemon is a per-user **launchd** LaunchAgent, not a systemd
unit. The plist lives at
`~/Library/LaunchAgents/com.daystrom.blackbox.plist`; the service label is
`com.daystrom.blackbox` and is registered into the `gui/$UID` domain
(`launchctl print` will show `gui/<uid>/com.daystrom.blackbox`).

LaunchAgents are **shared infrastructure** — other Claude accounts and
background bros in this user session all hit the same daemon on
`127.0.0.1:7264`. Restarting it mid-session briefly disconnects all of them,
and here you are cutting **beta code** into prod, so confirm the operator
actually wants beta running as prod. Do a read-only scope check and **get
explicit operator confirmation for the named service** even though the
operator invoked this prompt — the cleaning authorizes the *rebuild*, this
gate authorizes the *cutover*.

```bash
# 1. Verify the LaunchAgent is registered and the binary on disk is the one
#    the plist points at (catches a drifted ~/.local/bin/blackboxd).
launchctl list | grep com.daystrom.blackbox                        # expect: "<pid> 0  com.daystrom.blackbox"
plutil -p ~/Library/LaunchAgents/com.daystrom.blackbox.plist | grep -E 'Program|Label'   # expect: Program = ~/.local/bin/blackboxd

# 2. Verbose state read (state/pid/last exit code) before the cutover.
launchctl print "gui/$(id -u)/com.daystrom.blackbox" 2>&1 | grep -E 'state|pid|last exit code'

# --- confirm with operator, then: ---

# 3. In-process restart. `launchctl kickstart -k` SIGKILLs the running instance
#    and respawns it under the SAME service registration — much cleaner than
#    `launchctl unload && launchctl load`, which drops KeepAlive across the
#    gap and can race with the watchdog if anything else is holding a handle.
launchctl kickstart -k "gui/$(id -u)/com.daystrom.blackbox"

# 4. Verify the new pid is alive and the daemon answers on the port.
launchctl print "gui/$(id -u)/com.daystrom.blackbox" 2>&1 | grep -E 'state|pid|last exit code'   # expect: state = running, fresh pid, last exit code = 0
curl -fsS "127.0.0.1:${BBOX_PORT:-7264}/roster" >/dev/null && echo "daemon answering"
```

If `launchctl list` returns nothing for `com.daystrom.blackbox`, the LaunchAgent
isn't loaded — surface that to the operator before trying `kickstart`. The
remediation is `launchctl load ~/Library/LaunchAgents/com.daystrom.blackbox.plist`,
not a daemon-binary reinstall.

> ⛔ **Do not use `launchctl unload` then `launchctl load` to "restart".** That
> path is equivalent to a stop + start, not an in-process restart, and (a) the
> service can be relaunched in the gap by something other than launchd, (b)
> KeepAlive is torn down across the gap, (c) environment variables from the
> plist are re-evaluated at load time, which can silently change behavior if
> `~/Library/LaunchAgents/com.daystrom.blackbox.plist` drifted. `kickstart -k`
> is the cutover; `unload`/`load` is a service-registration rewrite.

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
- **Daemon health:** pre-cutover pid → post-cutover pid, `state` from
  `launchctl print`, and `/roster` probe result.

Keep the report operational; do not narrate every command.
