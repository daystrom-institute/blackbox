---
title: "MCP survivor fix dispatch"
kind: agent-lens
corpus: blackbox-prompts
audience: dispatched-bro
topic: [prompts, prompts-agents, mcp]
brief: "Bounded implementation briefs for the source-grounded MCP survivor audit."
---

# MCP survivor fix dispatch

Implement the assigned brief against the committed survivor audit, not the earlier backend delivery work. Evidence: [action audit](../../../design/surfaces/mcp/mcp-survivor-action-audit.md), [measured probes](../../../design/surfaces/mcp/mcp-survivor-audit-evidence.json), gap-7a2513c9 and thread-d7cd3385. The audit distinguishes live reproduction, source findings, and recommendations; preserve those distinctions.

## Shared execution contract

Read PROJECT.md, applicable AGENTS.md files, [edit-only worktree rules](../edit-only-worktree.md), and your assigned brief before editing. Each worker gets an isolated EDIT-ONLY worktree and branch based on the same pushed brief commit. Implement the caller contract, write regression tests, format with scripts/fmt.sh, commit by explicit owned paths, and PUSH your assigned branch to origin. Commit plus push is the deliverable; do not merge into beta.

Do not run cargo check/build/test/nextest/clippy or compile-shaped gates in these cold checkouts. Do not override the cold-build guard or set shared build environment. The orchestrator verifies pushed refs on the cluster with lane-run.sh --ref, workspace nextest, clippy, pinned formatting and concurrency checks, then resumes the original author for corrections. Read the estate BBOX_LANE_WORK.md before any subsequently authorized heavy work.

Keep changes inside your assignment. Matching tool parameter types, tests, and only your named tool's stanza in crates/bbox-tool-docs/src/tool_docs.rs may be changed as needed. That file is shared across isolated branches: make small stanza-local edits, never wholesale regeneration. Existing body/collection-page helpers are read-only shared dependencies unless the orchestrator assigns a helper change. Report cross-owner needs through bro_report rather than changing a sibling's surface.

Bounds apply to the complete serialized tool result, including escaping/structured duplication and a single oversized item. Use existing producer-owned projections and exact body readers. Preserve exact recoverability, actionable failures, scope/authority identity and truthful continuation. Do not silently trim answers or add spillfiles, persistent receipt stores, index redesigns, generic transaction/replay engines, or owner transport.

Write meaningful tests using isolated synthetic fixtures. Canonicalize tempdir roots, use SharedState::for_test, isolate real HOME/XDG, and hold test_env_lock for process-env mutation. Never inject real credentials or probe production mutations. Read-only inspection is fine; fixture tests are not live validation.

Retirement recommendations do not authorize data deletion or broad callable-name removal. Correct misleading no-op outcomes/docs and report consumer-backed disposition; roadmap retirement and delivery/replay gaps remain separate. Preserve bbox_corpus_search's real harness consumer. Never expand work_* beyond workflow-internal tools.

Do not restart/deploy shared services, send external messages, create sibling dispatches, prune tasks, or delete worktrees. Do not touch peer changes. Public artifacts must contain no private client identifiers, secrets, em dashes, or AI attribution.

Report milestones with bro_report: grounded plan, implementation complete, pushed deliverable, and concrete blockers. Final report: branch, commit SHA, changed paths, acceptance cases/tests written, checks actually run, unresolved assumptions, and any contract compatibility change. Do not claim tests passed when you only wrote them.

## Ownership and dispatch units

1. [Aggregate task selection and outcome truth](wait-contracts.md), A01, A06, A12.
2. [Pin and note identity plus bounded thread recovery](host-owned-reads.md), A02, A03, A04, A06, A10, A13.
3. [Entity-aware search follow-up calls](search-recovery.md), A07.
4. [Bounded graph discovery and exact schema detail](graph-projections.md), A04, A06, A10, A13.
5. [Useful knowledge summaries and exact diagnostic recovery](knowledge-projections.md), A04, A05, A06, A08, A13.
6. [Honest brofile and MCP configuration actions](configuration-contracts.md), A03, A04, A06, A08, A09, A10, A11.
7. [Packet and MCP policy discovery bounds and validation](packet-surface-contracts.md), A03, A05, A06, A10, A13.
8. [Maintenance selector parity and bounded diagnostic output](maintenance-contracts.md), A03, A06, A09, A10, A13, A14.
9. [Compact publisher status with retained authority evidence](publisher-projections.md), A05, A06, A09, A10, A13.
10. [Allocator persistence truth and safe specialist detail](specialist-outcomes.md), A04, A06, A10, A11, A12.

The units can run concurrently. Each owns different implementation surfaces; the orchestrator resolves small shared documentation hunks during integration. No unit owns the audit record, gap status, or thread lifecycle. Runtime task/session handles are recorded in the existing host-local thread, not this durable prompt.

## Review and completion

For each pushed ref, review the complete diff against its acceptance checks, run lane gates, and return failures to the same provider session. A pushed branch alone does not close the audit gap. After integration and approved deployment, replay the bounded synthetic/read-only MCP cases and distinguish deployed evidence from branch tests. Further source-only audit rows and consumer-dependent retirement decisions remain visible in the audit until individually resolved.
