---
title: "Roadmap elision"
kind: design
corpus: blackbox-design
lifecycle: implemented
topic:
  - surfaces
  - mcp
brief: "Remove the roadmap subsystem completely, including historical readers and configuration, without migration or replacement workflows."
---

# Roadmap elision

The operator's 2026-09-07 direction removes the roadmap subsystem completely.
Its history is stale and irrelevant to the supported product. The earlier
mutation-only retirement and its historical-reader, archival and migration
obligations are superseded.

## Caller contract

`bbox_roadmap` is absent from MCP discovery, surface replay, tool documentation
and dispatch. Calling that name produces the ordinary unknown-tool error.
No action remains, including list, get, search, ranking, rendering or export.
There is no retired-action adapter or replacement workflow.

`roadmap_item` and the `ROADMAP_*` relationship family are absent from the
supported entity/schema vocabulary. Providers, indexing, graph projection,
embedding and ownership adapters do not recover or expose roadmap records.

## Runtime removal

Runtime state, persistence, configuration and path utilities no longer own a
roadmap store. Daemon and project configuration have no roadmap template or
write-path fields. `BLACKBOX_ROADMAP_PATH` has no consumer. The rendered
snapshot, template and reader documentation are removed.

Startup and ordinary operations do not read, rewrite or migrate leftover
roadmap JSON. Malformed files at former default paths or the retired environment
override cannot prevent startup. This independence is an acceptance condition,
not a historical-data preservation service.

No historical read/export surface, per-record disposition, archival workflow,
converter or owner-mapping prerequisite belongs to this removal. Earlier audit
measurements remain as evidence of their tested revision, not current runtime
or migration requirements.

## Acceptance

- The complete served catalog and replay omit `bbox_roadmap`; its invocation
  returns the same unknown-tool response as an unregistered name.
- Schema discovery and graph readers expose no roadmap entity or edge family.
- An isolated daemon starts with malformed files at former store paths and a
  malformed file named by `BLACKBOX_ROADMAP_PATH`. Those bytes remain untouched
  throughout startup and the caller-contract probe.
- Source checks find no roadmap runtime configuration, path, persistence,
  index/provider or template dependency. Active documentation has no roadmap
  reader, planning or migration guidance.

Verification is recorded in [the elision acceptance record](roadmap-elision-verification.json).
Runtime source `eeb138e7a566` passed 6,731 full-workspace tests (19 skipped),
clippy, pinned formatting, concurrency checks and daemon/admin builds. The
isolated HTTP probe passed 285 checks with 108 catalog tools, including ignored
malformed legacy storage. The same runtime image is deployed and production
read checks confirm roadmap absence and surviving snapshot/thread readers.
Historical counts in earlier audit evidence remain tied to their tested source.
