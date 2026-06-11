# Developing Blackbox

Contributor setup and build guidance. User-facing install/run lives in
[`README.md`](../README.md); day-2 operations live in
[`docs/operations.md`](operations.md) and
[`docs/operating-blackbox.md`](operating-blackbox.md). For a throwaway daemon to
validate changes without touching prod state, see
[`docs/operations-isolated-dev-daemon.md`](operations-isolated-dev-daemon.md).

## Build and test

```bash
cargo build --release                      # binaries into target/release
cargo check                                # fast type-check
cargo nextest run --workspace              # mid-cycle gate (~24s execution)
cargo nextest run --workspace --profile full   # fold/closeout gate (~85s, full suite)
cargo clippy                               # lints
```

The test gates run under [cargo-nextest](https://nexte.st)
(`brew install cargo-nextest`). Nextest runs each test in its own process and
parallelizes across cores; `.config/nextest.toml` defines the two profiles. The
`default` profile quarantines the two >45s tests behind the `full` profile so
the mid-cycle gate stays fast, and applies per-test slow-timeouts so a newly
slow test gets *named* in the output instead of silently stretching the suite.

**Always pass `--workspace`.** The root manifest is workspace+package, so a
bare `cargo nextest run` (or `cargo test`) covers the root `blackbox` package
only — the ~1,800 tests living in the peeled `bbox-*`/`bro-*` crates silently
drop out. There are no executable doctests in the workspace, so nextest's
no-doctest limitation costs nothing.

Measured timing story (M-series laptop, 14 threads, warm `target/`,
2026-06-10 — re-measure rather than trust these once the suite shifts):

| operation | wall |
|---|---|
| `cargo check` no-op | ~0.2s |
| `cargo check` after a root-crate edit | ~3.3s |
| `cargo check` after a leaf-crate edit (e.g. bbox-packets) | ~3.5s |
| `cargo build --bin blackboxd` after a root edit (codegen+link) | ~31s |
| `cargo nextest run --workspace` (3,700 tests) | ~24s |
| `cargo nextest run --workspace --profile full` (3,702 tests) | ~85s |
| `cargo test --lib` (legacy fallback, root package only) | ~610s |

The fold gate's wall-clock is pinned by the single 80s index/search agentic
test; the mid-cycle gate's by ~5s artifact-install tests. Cold worktree builds
are solved separately by `project_dispatch.seed_dirs` CoW target seeding (see
below), not by the test profiles.

Use the narrowest command that proves your change, then broaden when touching
shared behavior (see [`PROJECT.md`](../PROJECT.md) → Validation for targeted
recipes per subsystem).

## Build performance across git worktrees (per-worktree isolation + sccache)

`bro fleet` and ad-hoc `git worktree add` both create sibling worktrees of this
Cargo workspace. **Each worktree builds into its own `target/`** — there is no
shared `CARGO_TARGET_DIR` injected by the cockpit (it was removed: a single
shared target dir serializes every concurrent build on cargo's exclusive build
lock, and it leaked Rust assumptions onto non-Rust projects). Isolation is the
default and keeps concurrent worktree builds from blocking each other; the cost
is that a fresh worktree cold-compiles the dependency tree once.

To recover cross-worktree compile reuse **without** a shared build lock, use
[`sccache`](https://github.com/mozilla/sccache) — a compilation cache that shares
*rustc outputs* host-wide while each worktree keeps its own `target/` (so builds
still run in parallel):

```bash
brew install sccache            # or: cargo install sccache
sccache --show-stats            # default cache: ~/Library/Caches/Mozilla.sccache (macOS)
```

Enable it per developer (don't commit `RUSTC_WRAPPER` into the repo's
`.cargo/config.toml` — a contributor or CI without sccache would then fail to
build):

```bash
export RUSTC_WRAPPER=sccache     # in your shell profile
# or, repo-local and untracked, in ~/.cargo/config.toml:
#   [build]
#   rustc-wrapper = "sccache"
```

### Fleet worktrees: `project_dispatch` in `fleet.json`

For agents dispatched by `bro fleet`, opt a project into dispatch-time env via a
per-project **`project_dispatch`** block in `fleet.json` (it sits beside your
blackbox `config.toml`; keyed by **canonical repo path**). This is the
project-agnostic replacement for the old hardcoded `CARGO_TARGET_DIR` — a Rust
repo opts into sccache, a Java repo sets `GRADLE_USER_HOME`, most set nothing:

```json
{
  "project_dispatch": {
    "/Users/you/repos/transcript-search": {
      "env": { "RUSTC_WRAPPER": "sccache" },
      "seed_dirs": ["target"]
    }
  }
}
```

`env` is resolved **daemon-side at task spawn** for every dispatch path
(`bro_exec`, agent dispatch, workflows, and the fleet cockpit), keyed by the
task cwd with worktree→base-repo mapping, and delivered to harness shell
children on the dedicated non-secret `shell_env` lane (never the transport
session env). Reserved `BRO_FLEET_*` vars are never overridden.

`seed_dirs` lists repo-relative directories to copy-on-write clone from the
base repo into each freshly created worktree (fleet cockpit and workflow
`WorktreeCreate`). Seeding a warm `target/` turns the dispatched agent's
first build from a cold full-workspace compile into an incremental one
(measured: a 56G target clones in ~13s on APFS; `cargo check` drops from
10+ minutes to ~30s of first-party recompiles). Best-effort: missing dirs,
already-present dirs, and non-CoW filesystems are skipped — there is
deliberately no plain-copy fallback, and a malformed `fleet.json` never
blocks a dispatch. The closeout side of the same per-project surface
(`project_closeout` — fold target, branch prefixes, and `closeout_hooks`) is
documented in [`design/fleet-tui/closeout-command.md`](../design/fleet-tui/closeout-command.md);
it is strict-loaded so a typo fails the `/closeout` command loudly.

## Build, run, or develop with Nix

The root flake separates product outputs from contributor tooling:

```bash
nix build .#blackbox
nix run .#blackboxd
nix run .#bro
nix develop .
nix flake check
nix fmt
```

- `packages.blackbox` / `packages.default`: build the crate for consumers
- `apps.blackboxd` / `apps.bro`: run the shipped binaries without a local Rust toolchain
- `checks.default`: validates the packaged build path that consumers use
- `formatter`: `nix fmt` formats the flake with `nixpkgs-fmt`
- `devShells.default`: contributor shell with Rust/Nix tooling

## Run a fully isolated dev-agent world with Nix

The dev systemd unit isolates the daemon, but not the agent harnesses that may
still auto-read `~/.claude-shared/CLAUDE.md`, `~/.codex/AGENTS.md`, or
`~/.gemini/GEMINI.md`. For contained end-to-end testing, use the flake-backed
dev harness instead:

```bash
nix develop .#dev-agent
cp .dev-agent-links.example .dev-agent-links   # optional; keep untracked
$EDITOR .dev-agent-links                       # link only auth/session material
bbx-dev-home init
bbx-dev-blackboxd
```

Open a second shell in the same repo and launch provider CLIs through the
wrappers:

```bash
nix develop .#dev-agent
bbx-dev-claude
bbx-dev-codex
bbx-dev-gemini
```

What the harness does:

- creates an isolated home tree at `./.dev-agent/home`
- keeps config, MCP wiring, render targets, blackbox state, transcript index,
  and bro state inside that tree
- points rendered global memory at the fake home's real pickup paths:
  - `./.dev-agent/home/.claude-shared/CLAUDE.md`
  - `./.dev-agent/home/.codex/AGENTS.md`
  - `./.dev-agent/home/.gemini/GEMINI.md`
- leaves auth/session passthrough explicit via `./.dev-agent-links`

`./.dev-agent-links` is TAB-separated:
`<relative-path-under-dev-home><TAB><absolute-host-path>`. That lets you borrow
only the auth material the real CLI requires while keeping the mutable config and
memory files isolated. Example:

```text
.claude/.credentials.json	/home/you/.claude/.credentials.json
.codex/auth.json	/home/you/.codex/auth.json
```

This split is intentional: auth may need to map back to host paths, but config,
MCP, render targets, and blackbox state should not.

If a provider co-locates auth with config in a single file, do not symlink your
real config wholesale unless you accept losing isolation for that provider.
Prefer copying just the auth-bearing material into the dev home or using a
provider-specific env var when the CLI supports one.
