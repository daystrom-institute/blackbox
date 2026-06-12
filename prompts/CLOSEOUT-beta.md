---
title: "Closeout Workflow (beta/blackbox-v2)"
kind: operator-prompt
corpus: blackbox-prompts
audience: interactive
topic:
  - prompts
brief: "Fold a worktree back into beta/blackbox-v2: commit, fast-forward-only merge into beta/blackbox-v2, push, then clean up the worktree. Beta-line sibling of CLOSEOUT.md (which folds into main)."
---

# Closeout Workflow (beta/blackbox-v2)

Beta-line sibling of [`CLOSEOUT.md`](CLOSEOUT.md): identical flow, but the
integration branch is **`beta/blackbox-v2`** instead of `main`.

1. **Capture durable knowledge into crate-/leaf-scoped CLAUDE.md** (below)
2. Commit your work if needed
3. Fetch and rebase on latest `beta/blackbox-v2` if needed
4. From your primary `beta/blackbox-v2` checkout: `git merge --ff-only <branch>` into `beta/blackbox-v2`
5. Push `beta/blackbox-v2`
6. Fold down and clean up your worktree

## Crate-/leaf-scoped CLAUDE.md upkeep (step 1)

Before committing, walk what the session actually changed and update the
nearest **crate-scoped** CLAUDE.md and any **sufficiently-dense-leaf-scoped**
one (a module dir that earns its own — the bar is "would future-you trip
without this?"). This is the per-session half of decision `0a8ffc5d`
(concepts/footguns, not lines; finer grain for dense leaves).

- **Capture invariants and footguns, not a changelog.** The audience is
  future-you re-entering this code cold: the rule that must hold, the
  why, and the cost of getting it wrong. A thing that bit during the
  session and is now load-bearing is exactly what belongs; a list of what
  you added does not.
- **Concept altitude, never `file:line` — that's rot-bait.** Anchor on
  stable names that survive edits and grep cleanly: module/function names,
  constants, gap IDs, decision IDs, empirical numbers (with their why).
  "`constructor_body_insert_position` anchors after a leading `super()`"
  ages well; "see line 856" is wrong by the next commit.
- **Match the house voice — sample siblings first.** Read 2–3 existing
  crate/leaf CLAUDE.md before writing so density, bolded lead-ins, and tone
  match. These are terse by design; earn every line.
- **Honor the privacy rule** (knowledge `b2261ea4`): no private client
  identifiers in any committed artifact, CLAUDE.md included — genericize.
- New durable *cross-session* rules/decisions still go through
  `bbox_learn`/`bbox_decide` (+ `bbox_render`); CLAUDE.md here is the
  code-local, hand-authored layer those don't cover.
