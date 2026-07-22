---
title: "Kimi Checkout-Provenance Plan Review"
kind: agent-lens
corpus: blackbox-prompts
audience: dispatched-bro
topic:
  - prompts
  - prompts-agents
  - design-review
brief: "Read-only fixed-scope review of the checkout-provenance export implementation plan, including the prerequisite repairs discovered by the full-scope code review."
---

# Kimi checkout-provenance plan review

You are the independent implementation-plan reviewer. Review the complete
document at
`design/daemon-runtime/checkout-provenance-export-impl.md` against the actual
current repository, the annotated baseline
`monolith-decomposition-pre-attempt-2`, the governing design
`design/daemon-runtime/locality-first-decomposition.md`, and every relevant
`AGENTS.md` instruction.

The plan includes prerequisite repairs for defects found by a separate
full-scope code review. Current code is expected not to contain those fixes
yet. Judge whether the plan identifies and resolves them correctly. Do not
reject the plan merely because its proposed changes are not implemented.

Do not narrow review to a summary, selected phase, named concern, or claimed
fix. Read every phase, boundary decision, migration rule, acceptance criterion,
and validation gate. Inspect code and callers as needed to decide whether the
plan is implementable and complete.

## Review method

1. Verify the baseline tag, current commit, staged and unstaged changes, and
   exact plan document.
2. Trace every planned seam through current callers, crate dependencies,
   persistence, indexing, embedding, Git-note behavior, MCP session authority,
   harness binding registration, and CLI transport.
3. Challenge the plan for missing authority checks, hidden checkout I/O,
   unbounded payloads, stale generations, partial writes, migration gaps,
   compatibility breaks, dependency inversion, and tests that cannot prove the
   stated property.
4. Reproduce every prior plan finding after revisions. Search the complete plan
   again for new defects instead of checking only the edited paragraphs.
5. Use read-only source and Git inspection. Do not edit files, create commits,
   push, restart services, dispatch other reviewers, or run local cargo, rustc,
   nextest, build scripts, or compile-shaped commands.

## Required response

Return plan findings first, ordered by severity. Every finding must include:

- severity;
- concrete plan section plus code or design evidence;
- the counterexample or failure sequence;
- impact on implementation or rollout;
- the exact condition required to fix the plan.

Then list:

- open design or policy questions that still require operator judgment;
- verification performed and important limits;
- a final plan verdict of exactly `PASS` or `REVISE`.

`PASS` means the document is a complete, feasible, dependency-ordered
implementation plan with no unresolved material correctness, security,
durability, concurrency, authority, migration, compatibility, or validation
gap. The code need not already implement it. An unresolved material plan or
policy question requires `REVISE`.
