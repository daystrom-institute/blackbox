---
title: "Daily Cleaning (beta/blackbox-v2)"
kind: operator-prompt
corpus: blackbox-prompts
audience: operator
topic:
  - prompts
  - maintenance
brief: "Start-of-day environment reset for the beta/blackbox-v2 line: sync to latest beta/blackbox-v2, prune landed manual worktrees, full cargo clean, cold rebuild + reinstall the checkout-host satellites and CLIs (bro, bro-harness, fleetd, collectors, blackbox), restart the collectors, check the deployed daemon with bbox_doctor. Operator-invoked and intentionally destructive: the operator running it interactively IS the authorization. Beta-line sibling of daily-cleaning.md (which tracks main)."
---

# Daily Cleaning (beta/blackbox-v2)

Reset the local environment to a fresh, current state at the start of a day,
tracking the **`beta/blackbox-v2`** integration branch instead of `main`. This
prompt is **operator-pointed and intentionally destructive**: it discards the
build cache, prunes worktrees, and restarts the checkout-host collectors. It is safe
*because a human invokes it interactively* and accepts those effects — do not
wire it into an unattended schedule without revisiting the gates below.

This is the beta-line sibling of [`daily-cleaning.md`](daily-cleaning.md). The
only difference is the integration branch: everywhere the main variant syncs to
and measures landing against `main`, this one uses `beta/blackbox-v2`. F3/F4
install and restart the **production** checkout-host binaries from the beta
code (the operator's running prod tracks beta).

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

## F3 - Cold rebuild + reinstall checkout-host binaries

The corpus daemon does not run on this host: it is a containerized workload
that the cluster build/converge path deploys (see "Where Heavy Work Runs" in
[`docs/project-guides/validation.md`](../docs/project-guides/validation.md)).
This phase never builds, installs or restarts `blackboxd`, and it copies no
system memories (they ship in the daemon's runtime image). It rebuilds the
checkout-host satellites and CLIs from the beta code and reinstalls them:
`bro`, `bro-harness`, `fleetd`, `bbox-code-collector`,
`bbox-transcript-collector`, and the offline `blackbox` CLI.

First record the build id of the installed `fleetd`; F4 uses it to decide
whether `fleetd` changed:

```bash
~/.local/bin/fleetd --version   # "fleetd <version> (<build id>)"; the build id is the commit it was built from
```

```bash
cargo build --release -p bro-cli -p bro-harness -p fleetd \
  -p bbox-code-collector -p bbox-transcript-collector
cargo build --release --bin blackbox   # offline blackbox CLI (root package)

ls -l target/release/{bro,bro-harness,fleetd,bbox-code-collector,bbox-transcript-collector,blackbox}   # ALL SIX must exist, freshly built

install -m 755 target/release/{bro,bro-harness,fleetd} ~/.local/bin/
install -m 755 target/release/{bbox-code-collector,bbox-transcript-collector} ~/.local/bin/
install -m 755 target/release/blackbox ~/.local/bin/
```

> ⛔ **If any expected binary is missing from `target/release/` after the
> builds, STOP and surface it.** Do not narrow the install list to make the
> command succeed, and do not infer an explanation (e.g. "it must be a
> subcommand now"). A missing binary almost always means the bin target moved
> to a different workspace crate. Find the crate that declares the bin
> (`grep -rl 'name = "<bin>"' crates/*/Cargo.toml Cargo.toml`), build it with
> `-p <crate>`, and install from there. A skipped reinstall leaves a stale
> binary that silently passes `--version` while running old code.

## F4 - Restart collectors (fleetd only if changed)  ⛔ GATED

The collectors and `fleetd` are **shared host infrastructure** other accounts
and background bros depend on. Here you are cutting **beta code** into them,
so confirm the operator actually wants beta running on this host. Before
restarting anything, do a read-only scope check and **get explicit operator
confirmation for each named service** even though the operator invoked this
prompt: the cleaning authorizes the *rebuild*, this gate authorizes the
*cutover*.

1. **Collectors.** Restart `bbox-code-collector` and
   `bbox-transcript-collector` through the host's service manager (whatever
   unit or supervisor this host runs them under), then confirm each is running
   and completes a new collection cycle. A running job is not proof of a
   successful scan; check its cycle report.
2. **fleetd.** Restart it ONLY if it changed, and only after its own explicit
   operator confirmation: restarting `fleetd` kills every harness worker it
   supervises (see "The fleet supervisor" in
   [`docs/operating-blackbox.md`](../docs/operating-blackbox.md)). Compare the
   build id recorded in F3 against the synced checkout:

   ```bash
   git diff --quiet <recorded build id> HEAD -- \
     crates/fleetd crates/bro-core crates/bro-protocol crates/bro-rpc Cargo.lock \
     && echo "fleetd unchanged: leave it running"
   ```

   If it changed, name the live sessions it will kill when asking, then restart
   it through the host's service manager. If the operator declines, leave it
   running and report the deferral.
3. **bro-harness.** No restart: new sessions pick up the new binary; running
   sessions keep theirs.

Finally, check the deployed daemon (not a localhost port):

```bash
bro mcp call bbox_doctor '{"format":"summary"}' --surface ops
```

## F5 — Report

Return a tight summary:

- **Synced:** beta rebased onto `origin/beta/blackbox-v2` (or "already
  current"); any conflict surfaced.
- **Worktrees:** reclaimed (path + branch), landed-but-dirty (reported, paths),
  true orphans (path + branch + `beta/blackbox-v2..branch` commit count), fleet
  worktrees (listed, untouched).
- **Disk:** `target/` size before → after clean; total reclaimed.
- **Installed:** `bro --version` / `fleetd --version` / `bro-harness`
  version, PLUS the install mtimes
  (`ls -l ~/.local/bin/{bro,bro-harness,fleetd,bbox-code-collector,bbox-transcript-collector,blackbox}`):
  all six must postdate this cleaning run; `--version` alone cannot detect a
  stale binary.
- **Services:** collectors restarted or deferred at the gate; `fleetd`
  restarted, left running because unchanged, or deferred at the gate.
- **Daemon health:** the `bbox_doctor` summary findings from the deployed
  daemon.

Keep the report operational; do not narrate every command.
