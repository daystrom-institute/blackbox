---
title: "Worker and leaf sandbox isolation"
kind: design
lifecycle: partial
corpus: blackbox-design
topic:
  - bro-harness
  - orchestration
brief: "As-built narrow authority firewall for every fleetd-launched bro-harness process, plus the proposed broader worktree/PID scope sandbox. macOS authority launch is fail-closed through Seatbelt. Linux authority launch requires a root-owned external sandbox launcher that satisfies the v1 probe and launch contract."
---

# Worker and leaf sandbox isolation

> **Status: partially shipped.** The service split changed the threat model: a
> same-UID worker must not recover the shared daemon bearer or bypass fleetd by
> calling authority services directly. That narrow process-tree boundary is now
> load-bearing and fail-closed. Broader worktree hiding, PID namespaces, and
> arbitrary network policy remain proposed.

## 0. Shipped authority boundary

`fleetd` never launches `bro-harness` directly in authority mode. The launcher
first establishes an inherited OS policy around the whole harness process, so
V8-dispatched tools, shells, compilers, and their descendants cannot escape by
changing executable or language.

The required guarantees are deliberately narrow:

- deny read, write, hard-link, replacement, and unlink access to the canonical
  shared service-token path;
- deny replacement of the canonical bro-harness executable trusted for later
  dispatches;
- deny reads and writes to every provider credential source file/directory
  fleetd used to construct the spawn environment; Brodex receives the parsed
  auth document inline through scrubbed task-local session state and refreshes
  only that in-memory copy;
- deny reads and writes to fleet authority snapshots/locks and every sibling
  worker directory, while allowing the bound worker's private journal tree;
- deny reads and writes to configured blackopsd state/catalog and corpus
  private-state/index roots; when the corpus state root is a shared parent, the
  narrower fleet state subtree remains available under its own rules;
- scope `BRO_HOME` to the durable SessionId, deny every sibling Bro session
  directory, and retain that same private session directory when fleetd starts
  a replacement worker for the session;
- deny unlink, rename, and replacement of the fleet Unix socket path while
  retaining Unix-domain connection access to it;
- deny direct loopback TCP connections to blackboxd, fleetd, and blackopsd,
  including configured non-default service ports;
- deny cross-sandbox process inspection, task-port acquisition, and signals on
  macOS so another same-UID daemon is not a token side channel;
- retain ordinary provider network access, repository/worktree access, the
  private fleet Unix socket, and worker journals; and
- fail the authority startup or worker launch rather than fall back to a bare
  process when the required policy cannot be established.

On macOS, fleetd uses only the root-owned `/usr/bin/sandbox-exec`, compiles and
applies a Seatbelt policy during startup as a probe, canonicalizes every
protected path before inserting it as a profile parameter, and wraps the
complete bro-harness process tree. Fleet state, worker, socket-parent, Bro
session-root, bound worker, and bound Bro-session directories are real
canonical directories with mode `0700`. An external override is rejected.
Daemon-owned persistence of a Brodex refresh-token rotation remains the
explicit `AR-004` removal gate in `ARCH_RELAYER_LOG.md`; direct worker
write-back to the source is not an acceptable bridge.

On Linux, authority mode requires
`FLEETD_WORKER_SANDBOX_LAUNCHER=/absolute/root-owned/path`. fleetd rejects a
launcher that is not root-owned, executable, or is writable by group/other. It
also rejects a launcher beneath any non-root-owned or group/other-writable path
component, then requires this exact protocol:

```text
launcher --self-test --protocol blackbox-worker-sandbox-v1 \
  --service-token-file PATH --worker-binary PATH --worker-socket PATH \
  --fleet-state-dir PATH --worker-root PATH --worker-dir PATH \
  --bro-root PATH --bro-session-dir PATH \
  --protected-service-root PATH ... \
  --protected-path PATH ... \
  --deny-loopback-port PORT ...

stdout: blackbox-worker-sandbox-v1\n

launcher --launch --protocol blackbox-worker-sandbox-v1 \
  --service-token-file PATH --worker-binary PATH --worker-socket PATH \
  --fleet-state-dir PATH --worker-root PATH --worker-dir PATH \
  --bro-root PATH --bro-session-dir PATH \
  --protected-service-root PATH ... \
  --protected-path PATH ... \
  --deny-loopback-port PORT ... -- /absolute/bro-harness ARGS...
```

The self-test must validate active kernel enforcement, not merely argument
syntax. The launch path must establish token and protected-source read/write
denial, worker-binary write denial, peer worker/Bro-session denial, fleet and
peer-service authority-state denial, Unix-socket mutation denial, service-port
connection denial, child inheritance, provider egress, bound worker/session
writes, and access to the named Unix socket before it execs bro-harness. It
must consume
the control arguments rather than forwarding authority paths to the worker.
The exact one-line marker is the only accepted self-test output. Other
platforms reject authority mode.

## 1. Scope

The boundary doc runs harness sessions in-process; **V8 cells run in-process**
(the isolate is the containment unit) and **shell tool ops are child processes**
the harness spawns and supervises (timeout + resource cap). The shipped authority
firewall above covers the minimum service boundary. The rest of this doc covers
the orthogonal, broader question: should a shell child also be confined to one
worktree, one process set, and a smaller network scope?

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

- The narrow service-authority firewall is current and load-bearing on every
  authority launch. It never degrades gracefully.
- Broader worktree/PID hardening remains per-OS and forward-looking.
- Linux has no in-repo test tier, so its load-bearing integration is an explicit
  root-owned launcher with a runtime self-test rather than an unverified
  best-effort backend.

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

One manager interface and per-OS backends (the shape of codex's
`sandboxing::manager` / `SandboxType`):

| OS | Mechanism | Gives |
|---|---|---|
| **Linux** | `unshare --user --pid --net` + landlock (FS ACL) + seccomp + `no_new_privs` | PID-ns (no cross-agent kill), mount-ns (worktree hiding), net-ns (network denial), syscall filtering |
| **macOS** | Seatbelt — `sandbox-exec` + an SBPL profile | path-based file deny/allow, network deny; **profile-based MAC, not namespaces** |
| **other** | none | authority mode is rejected |

macOS caveats (see the session that produced this doc): Seatbelt's `sandbox_init`
is **deprecated since OS X 10.7** but never removed and universally used (Chrome,
codex, Bazel) for lack of a CLI-level replacement; App Sandbox/entitlements only
applies to signed `.app` bundles and is useless for wrapping a subprocess. It is a
*different model* — it restricts operations by policy, not visibility — so it
**cannot** match the namespace-invisibility guarantees (no PID-ns cross-kill
or hiding; the shipped authority policy denies cross-sandbox inspection and
signals, while worktree confinement would still be path-policy, not "the
directory isn't there").

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

Namespace invisibility is Linux-only. macOS Seatbelt can approximate the first
with path policy and the second with cross-sandbox signal denial, but peer paths
and PIDs still exist in the host namespace.

## 7. Fail closed for authority; optional only for broader scope

For the shipped service boundary:

1. Probe the required backend during authority startup.
2. Reject missing, mutable, malformed, or non-enforcing launchers.
3. Never retry a worker as a bare process.

Future worktree/PID confinement may still use feature probes and optional
degradation, but it must compose inside the already-required authority boundary.

## 8. Open questions

- Adopt codex's sandbox crates as **dependencies**, or mine the patterns into our
  own `bro-*` crate? (Dependency is faster; mining avoids coupling to codex's
  release cadence and policy shapes.) This applies to the broader scope layer;
  the narrow macOS authority policy is already code-owned by fleetd.
- Does the **V8 cell** get sandboxed too, or only the shell leaf? (V8 already has
  no ambient fs/network from JS — globals deleted — so the FS/net sandbox matters
  mostly for the shell ops the cell *dispatches*.)
- Which Linux root-owned launcher implementation should become the recommended
  packaged default once a Linux test tier exists?

## 9. Relationship

- **The scope-sandbox boundary referenced by**
  [`harness-daemon-boundary.md`](./harness-daemon-boundary.md) §5. V8 runs
  in-process there and shell supervision still owns timeout/resource caps; this
  doc owns the inherited OS authority boundary and broader scope follow-ons.
- Reference implementations: codex-rs `linux-sandbox` / `sandboxing` /
  `execpolicy`; Daystrom `NamespaceIsolation`.
- Composes with **RX-V2** (the cargo-only `execpolicy` instance for atom-dispatched
  runs).
