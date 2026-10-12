---
name: bbox-dev-lanes
description: Choose where to build, test and verify changes to this repository, including cold worktrees, warm cluster lanes and the local contributor fallback. Use before running cargo build, check, nextest or clippy in a checkout you have not built in, when starting work in a new worktree, when cargo fails with the cold-checkout guard, when dispatching an agent to write code here, and when asked to verify or build a ref.
---

# Where blackbox work runs

The rule that decides everything is **warmth, not task size.** A checkout with a populated build cache builds in seconds. A cold one starts with a full dependency compile that takes far longer and, on the operator's macOS control host, starves every other agent on the machine. So the first question is always whether this checkout is warm, not how small the change is.

Use the narrowest check that proves the change, then broaden only for shared behaviour or an unresolved concern. [validation.md](../../../docs/project-guides/validation.md) has the recipes per subsystem.

## 1. Establish warmth and ownership

Look, do not assume: a base checkout is not necessarily warm, and a worktree that has been built in is not cold. Check for build output under `target/`.

The repository also guards one case mechanically. `scripts/rustc-cold-guard.sh` refuses to compile in a linked worktree on macOS that has no cached `.rlib` files. That guard is narrower than the policy: a checkout the guard lets through is not thereby cheap to build. Do not set `BBOX_ALLOW_COLD_BUILD=1` to get past it. That override is the operator's decision.

Know whose checkout it is, too. Stage, commit and discard only files that belong to your assignment, by explicit path, and never reuse another worktree's `target/` as a shared cache.

## 2. In a cold or edit-only checkout: edit, do not compile

- Edit code and docs. Write tests for behaviour you change where tests are warranted, and leave running them to a warm environment.
- Format with `scripts/fmt.sh`. It formats only and does not compile.
- Commit and push only as your brief allows. Done means the change is handed over as the ref or patch your brief asks for, with tests written, not that gates ran locally.
- Report the exact ref or patch, what you could not verify, and each assumption you made in place of a compile check.

Whoever dispatched you runs the gates and sends corrections back to your session. No reply does not mean green.

## 3. Heavy work on the operator's estate

Where the operator's cluster tooling is present, heavy builds and full gates run in warm lanes and on cluster verification, not on the control host. Read [estate-lanes.md](estate-lanes.md) before claiming a lane, dispatching into one, submitting a verify or image build, or releasing one.

What stays local either way: file edits, formatting, narrow checks in a warm checkout, and macOS builds of the host binaries (collectors, `fleetd`, `bro`), because the cluster produces Linux artifacts only.

## 4. Without the estate

The repository does not depend on the cluster. Use an ordinary local checkout and the contributor setup in [developing-blackbox.md](../../../docs/developing-blackbox.md), with a build cache you provision yourself. Do not retry estate commands that are not there.

## 5. Gates

```bash
cargo check -p <crate>                          # tight loop
cargo check --workspace
cargo nextest run --workspace                   # mid-cycle gate
cargo nextest run --workspace --profile full    # fold and closeout gate, full suite
cargo clippy --workspace
scripts/lint-concurrency.sh                     # handler or concurrency changes
scripts/fmt.sh --check
```

- Use nextest, not `cargo test`, and always pass `--workspace`, including on targeted runs. The root manifest is both a workspace and a package, so a bare run covers the root package only and silently skips most of the suite.
- Use `scripts/fmt.sh`, not a moving toolchain alias. It pins the exact formatter so macOS and Linux agree.
- A compile is not enough for config, startup or service changes. Start an isolated daemon (`scripts/dev-isolated-daemon.sh`) and confirm it initializes. Never test against the deployed daemon.
- Documentation-only changes, including skill edits, need no Rust build.
- Keep test state out of the real home directory, XDG paths and deployed services: per-test temporary directories and the project's test-state constructors.

## Dispatching another agent to write code here

- For heavy work, give it a warm lane as its working directory and one line telling it to read the lane contract first (see [estate-lanes.md](estate-lanes.md)).
- For edit-only work in a cold worktree, point it at `prompts/agents/edit-only-worktree.md`. Its done-criteria is the handed-over change, never locally run gates.
- You run the gates against its ref and send corrections back to the same session, which still holds the context.

## Report

Where validation ran, what it established, and what still needs checking. A finished worker or a submitted workflow is not a passing result.
