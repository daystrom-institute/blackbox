---
title: "Closeout Workflow"
kind: operator-prompt
corpus: blackbox-prompts
audience: interactive
topic:
  - prompts
brief: "Fold a worktree back into main: commit, fast-forward-only merge into main, push, then clean up the worktree."
---

# Closeout Workflow

1. Commit your work if needed
2. Fetch and rebase on latest main if needed
3. From your primary main checkout: `git merge --ff-only <branch>` into main
4. Push main
5. Fold down and clean up your worktree
