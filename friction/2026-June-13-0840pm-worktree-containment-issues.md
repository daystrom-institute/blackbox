---
title: "Worktree containment issues — file tools can't reach a freshly-created git worktree"
kind: friction-log
date: 2026-06-13
time: 08:40 PM America/Edmonton
session: 231cdfca-6687-4280-9801-c821bf261683
task: f2f66c28-6df5-4b56-980c-b03659940efa
gaps:
  - gap-e0ae3e7d
---

# Worktree containment issues

Harness retro for a short session that tried to use `git worktree add` to
isolate a small fix on a new branch, and discovered the harness's file-tool
scope is permanently anchored to the launch worktree. Operator pivoted to
"document the friction, then we decide the fix shape" — this log captures
the substrate issues, not the product fix.

## 1. Overall feel

The harness surfaces a clean, well-orchestrated tool belt for *reading and
editing the launch worktree*, but a sharp cliff at "I want to do work in a
different worktree." The cliff is invisible until you try to step off: every
file-mutation tool returns the same terse `path escapes worktree root`, with
no hint about *why* the scope is bounded, *which* tools are bounded, or
*what* the intended escape hatch is. The intended escape hatch turns out to
be "spawn a fresh sub-agent" — heavy for a 4-test-body fix.

## 2. Helpful

- **AGENTS.md riders are transcript events, not sidecars.** The harness
  delivered `crates/bro-harness/AGENTS.md` and `crates/bro-cli/AGENTS.md`
  as in-band rider blocks the moment I first touched each path via
  `list_dir` / `file_read`. The content was exactly what I needed (boundary
  invariants, session-event-log product surface, multi-tenant rules); not
  having to re-fetch the file was a real win.
- **rtk proxy for shell.** Every shell command I ran went through
  `rtk …` with no friction; the rewrite to `cargo test: 1 passed, 205
  filtered out` style saved several thousand tokens over the session.
- **`exec` / `wait` for composed reads.** Loading 3-4 file chunks per
  turn via one V8 isolate (with `text()` emitting only the lines I needed)
  was materially faster than four sequential `file_read` round-trips.
- **`sandbox_status(root=…)`** for inspecting a different worktree's git
  state without leaving the main scope. Confirmed the new worktree was
  created, on the right branch, at the right commit, working tree clean.
- **Test isolation primitive is canonical and well-documented.**
  `bbox_util::util::TestEnvGuard` / `test_env_lock` is the right answer
  for the env-isolation test fix; the doc-comment even calls out that
  binary crates should reach it. The only friction is that `bro-cli`
  doesn't have `bbox-util` as a dev-dep, so I have to add it.
- **No `Claude Co-Authored-By` trailer in commits.** The repo's
  `AGENTS.md` rule is enforced by the human-side review habit, not the
  tool — but I noticed and followed it on the prior `feat(fleet-tui)`
  commit without prompting.

## 3. Noisy / awkward

- **`path escapes worktree root` is a one-line denial that does not name
  the offender or the intended work-around.** First time I hit it (on
  `file_edit` for the new worktree's `Cargo.toml`), I had to probe four
  more tools to confirm the scope was the harness, not a per-tool
  restriction. A denial message like
  `"file_edit is scoped to launch_root=/home/invidious/repos/transcript-search; to operate elsewhere, dispatch a fresh session in that worktree"`
  would have collapsed the investigation.
- **`bbox-util`'s `TestEnvGuard` lives in `bro_fleet_client::config`
  re-export and `bbox_util::util` directly, with a `pub(crate)` shadow
  in `bro-fleet-client`.** Three call sites, two of them `pub`-visible,
  one `pub(crate)`. The `pub(crate)` in `bro-fleet-client/src/lib.rs:39`
  is exactly the wrong visibility for a primitive that binary crates
  *outside* the workspace are encouraged to use — and it bit me: the
  *intended* consumer (the `bro-cli` test module) is structurally barred
  from importing it, forcing a `dev-dependencies` addition just to call
  one symbol.
- **The "namespace globals" pattern (`await code.items(...)` vs.
  `await tools.code_items(...)`) is documented but the error message
  is unhelpful.** When I first tried `tools.code_items`, I got a
  bare `TypeError: tools.code_items is not a function` with no pointer
  to the global. A targeted error ("namespace `code` is global, not a
  member of `tools`; use `await code.items(...)`") would be a one-line
  fix.
- **The "preview" tool list (the system prompt's "122 more hidden")
  and `tool_search` activation are not surfaced as a routine affordance.**
  I had to read `crates/bro-harness/AGENTS.md` to learn that `tool_search`
  is activation (not a schema dump) and that the next turn's wire list
  is the authority. A pointer in the system prompt's "Additional tools"
  section ("Use `tool_search` to load by name; the result activates them
  for the next turn") would have been useful.

## 4. Missing / wishlist

- **A session-scoped `sandbox_worktree_enter` primitive** that rebinds
  the file-mutation and read scopes to a different worktree path. The
  gap filed below is the same request, more formally.
- **A "list my worktrees" / "what's the effective worktree root" tool
  in the always-available set.** Today I have to call `sandbox_status`
  with no argument to see the launch root, and with a `root=` argument
  to inspect a different one. A first-class `sandbox_worktree_list`
  would be cleaner and would be the natural companion to a future
  `sandbox_worktree_enter`.
- **A "scope check" affordance for tools that have a hidden scope.**
  `sandbox_status` already exposes `launch_root`; a `sandbox_tool_scope`
  (or a `sandbox_can_reach(path)` predicate) would let an agent ask
  "can I edit this file from where I am?" before hitting a denial.
- **A first-class "branch on the same disk" alternative.** When the
  operator says "new worktree," the natural mental model is a separate
  directory. The harness's actual model is "the session is bound to one
  worktree forever; a new branch in the same worktree is the cheap
  alternative." A glossary or a closeout-helper doc that maps the
  operator's "worktree" intent to the harness's "branch" reality would
  reduce this round-trip.

## 5. Evidence from the session

- **`file_edit` denial.** Tried to edit
  `~/.local/state/.../env-isolation-test-fleet_tui/crates/bro-cli/Cargo.toml`
  in a freshly-created worktree; got
  `path escapes worktree root: <abs path>`. Same denial for `file_read`,
  `file_write`, and `code.items`/`code.read`.
- **`sandbox_status(root=…)` showed the new worktree was created and
  clean.** Output: `inspected_root=/home/invidious/.../env-isolation-test-fleet_tui`,
  `git.head=04b6f7a95cd7`, `git.toplevel=<new worktree>`, `dirty_count=0`.
  But the same `sandbox_status` with no argument showed
  `launch_root=/home/invidious/repos/transcript-search` — the scope is
  the launch root, not the inspected root.
- **Test pass in isolation, fail in suite.** The pre-existing failure
  `resume_attach_uses_configured_bro_home_when_env_is_absent` passed
  when run alone (`1 passed, 205 filtered out`) and failed in the full
  suite (`195 passed; 1 failed`). The pattern: 4 env-mutating tests in
  `crates/bro-cli/src/fleet_tui/tests.rs` race because none of them take
  the `test_env_lock`. The fix is `TestEnvGuard`; the friction is that
  the lock was added to `bbox-util` after these tests were written, and
  the conversion wasn't done.
- **Namespace error message.** First call to `tools.code_items` returned
  `TypeError: tools.code_items is not a function`. The global binding
  pattern (`await code.items(...)`) is documented in the system prompt
  under "Namespace globals" but the runtime error doesn't redirect.

## 6. Gaps

- **Filed: `gap-e0ae3e7d`** —
  *Harness file-tool scope is permanently bound to launch worktree; in-session
  `git worktree add` produces an unreachable dir.* (dedupe_key
  `tooling/harness/worktree-session-scope`, gap_kind `tooling`, impact
  `high`, blocking `workaround_available`.) The fallback used was to
  stop and ask the operator to choose between (1) branch in main worktree
  and (2) spawn a fresh sub-agent in the new worktree; the operator
  chose (1).
- **Not filed:**
  - The `pub(crate)` on `bro-fleet-client::test_env_lock` — that's a
    one-line visibility fix; not a reusable substrate gap, just code
    debt.
  - The `path escapes worktree root` denial message clarity — same.
  - The `tools.code_items` TypeError clarity — same.
  - The "preview tool list" not advertising `tool_search` activation —
    this is more of a documentation gap than a tooling gap, and the
    info *is* in `crates/bro-harness/AGENTS.md` once you've read it.
  - The pre-existing test failure in `bro-cli/src/fleet_tui/tests.rs` —
    that's a product test bug, not a harness substrate issue.

## 7. One next harness improvement

**Make the file-tool scope visible from the inside.** Two changes that
would have collapsed the entire investigation:

1. Surface the launch-root scope in the system prompt's environment
   context (alongside `<cwd>`), not just in `sandbox_status`'s response.
2. When a file-mutation tool denies a path with `path escapes worktree
   root`, return a message that names the launch root, names the file
   that was rejected, and names the operator's two real options
   (branch in main / spawn a fresh dispatch).

The underlying primitive (`sandbox_worktree_enter`) is the right long-term
fix, but the short-term message-clarity change is mechanical, low-risk,
and would have saved this session roughly three turns of probe-and-deny.
