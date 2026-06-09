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

1. Commit your work if needed
2. Fetch and rebase on latest `beta/blackbox-v2` if needed
3. From your primary `beta/blackbox-v2` checkout: `git merge --ff-only <branch>` into `beta/blackbox-v2`
4. Push `beta/blackbox-v2`
5. Fold down and clean up your worktree
