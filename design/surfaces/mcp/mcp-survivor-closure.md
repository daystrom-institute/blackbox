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
Final gate and deployment evidence is recorded after integration.
