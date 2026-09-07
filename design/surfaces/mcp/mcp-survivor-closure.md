---
title: "MCP survivor closure milestones"
kind: design
corpus: blackbox-design
lifecycle: partial
topic:
  - surfaces
  - mcp
brief: "Evidence-backed closure of diagnostic paging, native recovery, retirement and remaining adverse caller paths."
---

# MCP survivor closure milestones

Continuation of thread-c749d06c after the
[caller-contract fix checkpoint](mcp-survivor-fix-checkpoint.md) at b6189fbf.
Milestones record the changed contract and its acceptance evidence. A catalog
count or full-suite pass does not substitute for branch-specific evidence.

## Embedding report snapshots

`bbox_embed_status` validates contradictory and oversized selectors before
collecting status. Explicit diagnostic routes are bounded to 64 names of at
most 256 bytes. Debug expansion does not implicitly enable coverage scans or
recall probes.

Small ordinary reports retain their existing JSON shape. `body_limit` forces
exact pages; oversized complete replies enter exact mode automatically. The
first call collects once and stores serialized JSON in the MCP session. Its
clones share that cache, but other sessions cannot use its cursors. Every page
reports capture time and an immutable snapshot identity. Repeat the original
selectors with `body.next_cursor` as `cursor`; page size may change. No
continuation calls the status, coverage, graph-diagnostic or recall producers.

Reports expire after ten minutes or session/daemon loss. The session retains
at most four reports and 16 MiB, evicting oldest captures first. Each report is
limited to 8 MiB. A larger report explicitly reports that collection completed
but snapshot storage refused, with instructions to narrow opt-ins before an
explicit retry. Invalid, expired, evicted, wrong-session and changed-selector
cursors refuse without starting new work. The cache is temporary memory, not
a durable storage or publication claim.

Regression evidence: `status_snapshot` tests reconstruct escaped Unicode,
measure complete mirrored envelopes, assert producers run exactly once, reject
cross-session and changed selectors, exercise expiry and eviction, and verify
both byte and count retention limits. The isolated HTTP probe additionally
checks actual MCP session isolation and continuation through the served tool.
Later diagnostic/probe failures preserve completed observations and return
`error.embedding_observation_partial` on every exact page. Final gate and
deployment evidence is recorded after integration.

## Native transcript recovery

Native search, citation and tool-history hits can use compact indexed-record
handles instead of oversized or path-shaped source locators. Handles bind
index identity, segment, document and complete stored content. Exact
`bbox_context` pages recover stored content and metadata, while normal context
and message readers constrain native source, account and session. Deleted,
replaced, foreign-index or wrong-source handles refuse. Reindex/segment merges
can invalidate handles; repeat the original search. These readers never open a
caller-supplied path or grant retained-conversation access through a native
fallback. Source-host freshness and parser truncation remain explicit limits.

## Broadcast authority and admission

The [branch acceptance record](mcp-v01-dispatch-acceptance.json) names the
regressions and evidence limits. Broadcast now preserves explicit tool defaults
on fresh and resumed children, applies the ordinary-resume ownership guard,
and writes history only for tracked admissions. A tracked setup/provider
failure retains exact task recovery. A refused reservation reports a member
error and preserves the original team and corrupt store bytes. Reservation
refusal releases the task-store write lock before its readback.

## Roadmap elision

[Roadmap elision](roadmap-retirement.md) records the operator's 2026-09-07
removal contract. The MCP tool, historical readers, runtime store, config,
entity/edge vocabulary, indexing and templates are removed. Calls use the
ordinary unknown-tool error. There is no historical migration, per-owner
archival obligation or replacement workflow. The earlier mutation-only
retirement was superseded by this direction.

## Recall probes are reads

Self-recall selects only an already-loaded vector partition and never opens
or creates a partition. Missing names, busy inventory/partition locks, empty
HNSW state and measured recall remain distinct outcomes. Tempdir regressions
prove unknown, parent-relative and absolute routes leave storage and the
loaded-partition inventory unchanged.

## Verified rollout before roadmap elision

The measurements in this section describe the earlier retained-reader revision.
They remain historical evidence; roadmap elision requires its own verification.

Final tested source: `38eaf3738606fae164900fcb896f36b5d42ebf12`, including the concurrent Anthropic retry fix.
All 6,753 workspace tests passed in 157.814 seconds, with
19 skipped and no scheduling overrides. Clippy, binary build, pinned
formatting and concurrency lint passed. The isolated HTTP probe passed
297 checks over 29 distinct tools in a 109-tool catalog;
its largest complete result was 8,321 bytes.

The image for that exact source was converged to production and deployment
readiness was observed. Read-only production probes verified the new schemas,
cheap immutable snapshot reconstruction, session isolation, selector refusal,
historical roadmap search and explicit thread summary. No production scan,
recall probe, task dispatch or historical-data mutation was used for validation.

[Verification evidence](mcp-survivor-closure-verification.json) records the
commands, measurements, image digest and limits. Remaining acceptance work is
live remote-executor/provider failure certification. Historical roadmap
migration is not an acceptance obligation. Backend delivery/readiness repair and GLM
experiments remain separately owned. No source defect is inferred solely from
an unexecuted provider combination.
