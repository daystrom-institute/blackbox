---
title: "Kimi Durable Project Catalog Plan Review"
kind: agent-lens
corpus: blackbox-prompts
audience: dispatched-bro
topic:
  - prompts
  - prompts-agents
  - design-review
brief: "Read-only fixed-scope review of the durable corpus project catalog and checkout-attachment implementation plan against the complete current identity, indexing, knowledge, tool, and decomposition architecture."
---

# Kimi durable corpus project catalog plan review

You are the independent implementation-plan reviewer. Review the complete
governing document at
`design/daemon-runtime/durable-project-catalog-impl.md`, every tracked phase
implementation document matching
`design/daemon-runtime/durable-project-catalog-phase*-impl.md`, and
`DECISION_LEDGER.md` against the actual current repository, the annotated
baseline `monolith-decomposition-pre-attempt-2`, and every relevant `AGENTS.md`
instruction. A phase document is an executable refinement, not permission to
narrow review away from the complete governing design. Treat these as
governing companion designs and read them completely where they constrain the
plan:

- `design/daemon-runtime/locality-first-decomposition.md`;
- `design/daemon-runtime/distributed-code-source-collector-impl.md`;
- `design/corpus/agentic-corpus/project-taxonomy-standardization.md`;
- `design/corpus/knowledge/checkout-identity-and-provisional-knowledge.md`; and
- `design/daemon-runtime/checkout-provenance-export-impl.md`.

Current code contains the distributed code-source collector and may contain
completed earlier decomposition phases, but it is expected not to contain the
active phase before implementation begins. Judge whether the governing plan and
all phase refinements together identify and resolve the actual identity,
authority, persistence, migration, project-selection, response-provenance,
path-rendering, indexing, search, graph, embedding, source-activation, cutback,
Git-history, publisher, knowledge, tool, concurrency, recovery, overlap,
parity, live-bootsmoke, and dependency constraints. Do not reject a plan merely
because its proposed active-phase changes are not implemented.

Do not narrow review to a summary, selected phase, named concern, or claimed
fix. Read every phase, boundary decision, migration rule, acceptance criterion,
and validation gate. Inspect code and callers as needed to decide whether the
plan is implementable and complete.

## Review method

1. Verify the baseline tag, current commit, staged and unstaged changes, the
   complete governing plan, every phase implementation document, the decision
   ledger, and already-implemented decomposition state. Ignore unrelated
   untracked Java-worker build output as review scope, but report any other
   unexplained source or plan mutation.
2. Trace the proposed id and authority model through `ProjectRecord`,
   `PublishedScope`, aliases, config authority, entity refs, legacy commit
   namespaces, catalog/attachment persistence, migration, registration,
   rename/detach/delete, publisher selection, and every project selector. Look
   specifically for any remaining eight-hex, path-hash, string-shape, or
   computed-repo-id assumption.
3. Trace remote-only projects through collector grants, manifests, activation,
   restart, full and incremental indexing, purge, Tantivy and vector selection,
   edge manifests and read views, Git overlays, cutback, doctor/health, GC, and
   server reload. Prove the plan does not require a local `ProjectRecord` or
   fabricate an absolute path anywhere in that lane.
4. Trace catalog identity and attachment requirements through published and
   provisional knowledge/gaps, `built_from` responses, publisher authority,
   blame, render, file providers, provenance, refactor/mutation, artifacts,
   tool/transcript edges, coordination stores, and path-keyed compatibility
   migration. Distinguish durable logical state from execution-path state.
5. Challenge the two-file transaction and v1 import at every crash boundary,
   including duplicate old scopes, project-id collision, missing paths,
   non-Git and unrecorded projects, corrupt files, active collected generations,
   monorepos, multiple attachments, and rollback after partial index/backfill.
6. Challenge the plan for forged or ambiguous scope authority, self-election by
   repository config, unsafe id/path/URI encoding, symlink and attachment races,
   stale or mixed read views, silent source fallback, auth/cutback coupling,
   Git edges targeting the wrong code generation, accidental data deletion,
   compatibility breaks, dependency inversion, and tests that cannot prove the
   stated property.
7. Reproduce every prior plan finding after revisions. Search the complete plan
   again for new defects instead of checking only the edited paragraphs.
8. Use read-only source and Git inspection. Do not edit files, create commits,
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
