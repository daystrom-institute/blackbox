---
title: "Leaf sandbox isolation (proposed)"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - bro-harness
  - orchestration
brief: "Forward-looking OS-level *scope* sandboxing for the shell child processes the in-process harness spawns. Re-pointed (2026-06-02) after the V8/shell simplification: V8 runs in-process (isolate-contained), and the v1 shell isolation is supervision (timeout + ulimit/cgroup), not a sandbox. This doc is the threat-model-change escape hatch — pulled only when agents are genuinely untrusted/third-party or run unattended-autonomous. Key framing: a sandbox bounds *scope* (worktree, network, PIDs), it does not deny *capability* — a trusted agent with file-write+execute can do anything within its scope regardless of bash-vs-python, and name-based allowlists are speed bumps (cargo runs build.rs). So on a single-user box this is accident-containment between concurrent agents, not security. Per-OS, best-effort, graceful fallback first-class. Mines codex-rs/linux-sandbox + sandboxing (namespaces/landlock/seccomp on Linux, Seatbelt on macOS) and Daystrom's NamespaceIsolation. Bonus on Linux: mount/PID namespaces make two repo conventions (only-touch-your-own-worktree, don't-kill-across-agents) mechanical."
---

# Leaf sandbox isolation (proposed)

> **Status: proposed, forward-looking — not v1, re-pointed 2026-06-02.** After the
> V8/shell simplification, **V8 runs in-process** (isolate-contained) and the v1
> shell isolation is **supervision** (timeout + ulimit/cgroup) — see
> [`harness-daemon-boundary.md`](./harness-daemon-boundary.md) §5. This doc is the
> *scope* sandbox for the shell child processes (what they can *touch*), and it is
> a **threat-model-change escape hatch**, not on the v1 path. Pull it only when the
> triggers in §8 fire.

## 1. Scope

The boundary doc runs harness sessions in-process; **V8 cells run in-process**
(the isolate is the containment unit) and **shell tool ops are child processes**
the harness spawns and supervises (timeout + resource cap). This doc covers the
orthogonal, *optional* question: a shell child is its own process — should we also
confine **what it can touch** (filesystem, network, other processes), and how,
per-OS?

The honest answer up front: a sandbox bounds **scope**, not **capability**. An
agent with file-write + execute has arbitrary code execution — bash vs python vs a
`build.rs` is irrelevant. A sandbox cannot stop the agent from doing destructive
things *within its allowed scope* (it can `rm -rf` its own worktree). What it
bounds is the **blast radius**: which files (peer worktrees), which network, which
processes are reachable. On a **single-trusted-user** box that is
**accident-containment between concurrent agents**, not security — which is why
this is defense-in-depth and not v1.

One mechanical note decides *which* layer is even real: a process-tree sandbox is
**not** bypassed by "just write a python script" — namespaces/Seatbelt are
inherited by child processes, so the kernel confines the python that bash spawns
too. The sandbox **holds** against indirection; **name-based allowlists do not**
(`cargo check` runs `build.rs` = arbitrary code). So if you want a real boundary,
it is the sandbox, not the allowlist.

## 2. The environment reality: no Linux CI

There is **no Linux prod/CI tier** to design against. The dev environment is
**either Linux or macOS depending on the machine** — a single tier whose OS
varies, not a dev-vs-prod split. So:

- OS sandbox hardening is **forward-looking**, not a current requirement.
- It must be **per-OS and best-effort**, with "no isolation available" a
  first-class, expected state — never a hard failure (Daystrom's probe → cache →
  graceful-fallback pattern).
- Isolation here is **defense-in-depth, not load-bearing**. If a future use case
  (e.g. unattended autonomous runs) makes it load-bearing, *that* path runs on
  Linux, where the strong guarantees exist.

## 3. Two orthogonal layers

Keep these separate; they answer different questions and compose:

- **`execpolicy` — what may run.** A command allowlist/policy. The repo's RX-V2
  cargo-only allowlist for atom-dispatched refactor runs is an instance of this
  layer. Codex ships a general `execpolicy` crate. **Caveat:** name-based
  allowlists are speed bumps, not walls — `cargo check` runs `build.rs` = arbitrary
  code, and indirection defeats them. Their value is accident-prevention and
  keeping atoms predictable, **not** containing a determined agent.
- **sandbox — what it can touch.** Filesystem, network, process visibility,
  restricted regardless of *which* command ran.

A shell leaf wants both: `execpolicy` says "only `cargo check`," the sandbox says
"and even then, only your worktree, no network, no killing across the fence."

## 4. Per-OS strategy behind one interface

One manager interface, per-OS backends, no-op fallback (the shape of codex's
`sandboxing::manager` / `SandboxType`):

| OS | Mechanism | Gives |
|---|---|---|
| **Linux** | `unshare --user --pid --net` + landlock (FS ACL) + seccomp + `no_new_privs` | PID-ns (no cross-agent kill), mount-ns (worktree hiding), net-ns (network denial), syscall filtering |
| **macOS** | Seatbelt — `sandbox-exec` + an SBPL profile | path-based file deny/allow, network deny; **profile-based MAC, not namespaces** |
| **other** | no-op | nothing — fall back to a bare (unsandboxed) child process |

macOS caveats (see the session that produced this doc): Seatbelt's `sandbox_init`
is **deprecated since OS X 10.7** but never removed and universally used (Chrome,
codex, Bazel) for lack of a CLI-level replacement; App Sandbox/entitlements only
applies to signed `.app` bundles and is useless for wrapping a subprocess. It is a
*different model* — it restricts operations by policy, not visibility — so it
**cannot** match the namespace-invisibility guarantees (no PID-ns cross-kill
protection; worktree confinement is path-policy, not "the directory isn't there").

## 5. Mine, don't hand-roll

Rust-native primitives already exist; mine them rather than reimplement:

- **codex-rs `linux-sandbox`** (`../../../codex/codex-rs/linux-sandbox/`) —
  `bwrap.rs` (bubblewrap), `landlock.rs`, seccomp + `no_new_privs`,
  `--unshare-{user,pid,net}`. The production Rust version of the whole stack.
- **codex-rs `sandboxing`** — per-OS manager (`manager.rs`), bwrap + landlock
  wrappers, the `SandboxType` dispatch (incl. `MacosSeatbelt`).
- **codex-rs `execpolicy`** — the command-allowlist layer; **`process-hardening`**;
  **`windows-sandbox-rs`** for the third OS.
- **Daystrom `NamespaceIsolation`**
  (`../../../daystrom-mk2/src/Daystrom.AgentSdk/Transport/NamespaceIsolation.cs`)
  — the *reference pattern*: probe candidate `unshare` args most→least capable,
  cache (`Lazy`), fall back to bare spawn when unsupported; **transparent wrap**
  (prepend `unshare … --` to the existing command); a mount-namespace
  worktree-hiding script (tmpfs over the worktrees root, bind-restore only the
  target); PID-ns with `--mount-proc`.

## 6. The payoff: two conventions become mechanical (Linux)

On Linux, namespace isolation upgrades two of this repo's **trust-based
conventions** into structural guarantees:

- **Mount-ns worktree hiding** → CLAUDE.md's *"multi-tenant working tree, only
  touch files you changed"* stops being prompt-discipline: a peer's worktree
  literally is not visible inside the leaf's mount namespace.
- **PID-ns** → *"don't kill processes across agents"* becomes structural: a leaf
  cannot see, let alone signal, another agent's PIDs.

These are Linux-only; macOS Seatbelt approximates the first via path policy and
does not get the second.

## 7. Graceful degradation is first-class

Non-negotiable, because unprivileged user namespaces are off on many hosts and
absent entirely on macOS:

1. Probe the best available config at first use; cache the result.
2. If nothing works → report not-supported; the leaf runs as a bare worker.
3. Never hard-fail a dispatch because isolation is unavailable. The security model
   does not *assume* the strong case — it states guarantees per-platform and treats
   isolation as additive.

## 8. Open questions

- Adopt codex's sandbox crates as **dependencies**, or mine the patterns into our
  own `bro-*` crate? (Dependency is faster; mining avoids coupling to codex's
  release cadence and policy shapes.)
- **When does isolation become load-bearing?** (Unattended autonomous runs,
  untrusted-tool execution.) That answer also forces "runs on Linux."
- Does the **V8 cell** get sandboxed too, or only the shell leaf? (V8 already has
  no ambient fs/network from JS — globals deleted — so the FS/net sandbox matters
  mostly for the shell ops the cell *dispatches*.)
- macOS SBPL profile authoring: a single conservative profile, or per-tool
  profiles?

## 9. Relationship

- **The scope-sandbox escape hatch referenced by**
  [`harness-daemon-boundary.md`](./harness-daemon-boundary.md) §5. V8 runs
  in-process there and the v1 shell isolation is supervision (timeout/cap); this
  doc is pulled only on a threat-model change (§8).
- Reference implementations: codex-rs `linux-sandbox` / `sandboxing` /
  `execpolicy`; Daystrom `NamespaceIsolation`.
- Composes with **RX-V2** (the cargo-only `execpolicy` instance for atom-dispatched
  runs).
