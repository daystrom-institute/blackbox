---
title: "NARF execution effects and safety — parked apparatus"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - bro-harness
  - narf
  - execution
  - safety
  - effects
brief: "Captures the (correct but premature) reasoning about transactions, sagas, and mutation guards for NARF cells, and records the v1 decision: a NARF cell is arbitrary code at the same trust level as the shell the agent already has, so it carries NO new safety apparatus. The machinery here is shelved until the threat model changes (untrusted or unattended agents), the same disposition as leaf-sandbox-isolation.md."
---

# NARF execution effects and safety — parked apparatus

> **Status: parked, not v1.** This doc preserves a chain of reasoning that is
> *more correct in the abstract* but solves a threat model that does not exist in
> current practice. It is recorded so it is not re-derived from scratch when the
> threat model actually changes. Until then, the v1 stance in §0 governs.

## 0. The v1 decision (this is what holds today)

Every agent we run today runs in **YOLO mode** — full tool access, no permission
gating. A NARF cell is **arbitrary code execution at the same trust level as the
shell the agent already holds**. It therefore adds **no new trust boundary**, and
any safety apparatus wrapped around it (transactions, sagas, mutation guards,
effect-class dispatch) is **theater**: it guards a door that is already wide open
through the authorial surface.

The refutation is mechanical: an agent poisoned enough to call `rm -rf ~`
directly is equally able to write that line into a Python script and run it
through `shell.run` or a NARF cell. Guarding the "direct" path while the front
door grants arbitrary execution buys nothing. This is the boundary doc's own
"bash or python — same thing" point
([`harness-daemon-boundary.md`](./harness-daemon-boundary.md) §5), taken to its
conclusion.

**So v1 NARF is: capability bindings + refs. No `Tx`, no saga, no
mutation-guard-as-safety.** Mutations go through capability applies that may keep
cheap *hygiene* defaults (don't clobber a dirty file, surface a public-API
change) — but those are predictability/UX conveniences, **not** safety, and must
not be dressed as guarantees. The real safety layers are already in place and
are the only ones that are real:

- **Local edits → git.** A half-applied cell is recoverable exactly like a
  half-finished manual edit. Git + the operator reading the diff is the net.
- **External / out-of-worktree effects → operator attention.** `git push
  --force`, a network POST, spending money, dispatching a bro that does those —
  git nets *none* of it, and there is no cheap guard. Under **trusted +
  attended** operation this is accepted risk, not machinery to build.

## 1. The parked apparatus (only if the threat model changes)

The reasoning below is sound; it is simply not yet *needed*. Pulled in order of
how load-bearing it would become.

- **`Tx` overpromises.** A "transaction" implies all-or-nothing over everything
  in scope. The only rollback we can actually perform is `bbox_refactor_run`'s
  snapshot/restore of touched worktree files — i.e. **reversible local state
  only**. The name should signal its true domain (a worktree edit scope), not
  ACID-over-the-world. Reversibility (undo) is also distinct from idempotency
  (safe re-execution / replay); both break on external effects, for different
  reasons (the journal dodges idempotency by caching settled results, not by
  re-running them).

- **Saga / compensation is the other half.** Irreversible / external effects are
  **commit-points**: you cannot unwind them, only sequence and compensate
  forward. If this layer is ever built, each such step either declares a
  compensating action (run LIFO on failure) or surfaces its residue — never
  pretends to roll back.

- **Effect-class dispatch is the switch.** `AtomEffects` (`types.rs:102`) already
  declares effects and `EffectsObserved.violations` already observes them.
  Reversible (`writes_files` in-worktree) → transact; irreversible
  (`runs_shell` arbitrary / `dispatches_runs` / external) → saga or refuse. The
  declaration would have to be **enforced, not trusted**, or the model is theater
  again.

- **Sequencing shrinks the saga surface to near-zero.** Order a cell
  reversible-first, push commit-points last (`prepare` validates the whole
  reversible plan; the irreversible commit is the final statement). Most failures
  then fall in the reversible phase and roll back cleanly. This is the front half
  of NARF's prepare→run split
  ([`narf-capability-library.md`](./narf-capability-library.md) §6).

## 2. Triggers that un-park this

Same disposition as [`leaf-sandbox-isolation.md`](./leaf-sandbox-isolation.md):
shelved until one of the boundary doc's §5 triggers is real —

1. genuinely **untrusted / third-party** agents, or
2. **unattended autonomous** runs where no human is watching the blast radius.

Until then, building any of §1 is fabricating a need. Don't.

## 3. Relationship

- **Resolves by deferral** the [`harness-daemon-boundary.md`](./harness-daemon-boundary.md)
  §12 "tx vs saga" open fork: neither is built in v1; the `bro-capabilities`
  trait signatures stay neutral so §1 remains buildable later.
- **De-escalates** the boundary doc §5 "cell-bounded `Tx` → rollback on abort"
  line, which over-claimed a v1 guarantee.
- **Sibling to** [`leaf-sandbox-isolation.md`](./leaf-sandbox-isolation.md) — both
  are threat-model-change escape hatches, not v1 work.
