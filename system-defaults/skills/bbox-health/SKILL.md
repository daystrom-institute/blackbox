---
name: bbox-health
description: Diagnose Blackbox health, starting read-only, and gate mutating maintenance on the operator. Use when search returns nothing, stale or deleted results, when recent sessions or a repository are missing, when hybrid search looks keyword-only, when storage is growing, when Blackbox tools error or are missing from the session, or when asked for a health check or routine maintenance.
---

# Blackbox health triage

Diagnose with read-only checks first. Reindexing, re-embedding, garbage collection, compaction, config changes and restarts alter shared state other agents are using, so each needs operator authorization. Your job is to find the fault, bound it, and name the smallest next action.

Before interpreting anything, identify which daemon you are talking to and which project or source is affected, and use that same daemon for every probe. Paths a daemon reports refer to its own storage or to a source host, not necessarily to your filesystem.

Most health tools are on the `ops` surface: `bro mcp call <tool> '<json>' --surface ops`. A health tool absent from your session is usually on `ops`, not missing.

## 1. Run the aggregate check

```bash
bro mcp call bbox_doctor '{"format":"summary"}' --surface ops
```

`bbox_doctor` classifies findings as ok, info, warn, action or blocked and suggests next commands. Start from the first `action` or `blocked` finding. A suggested command that mutates is still subject to the authorization rule above: the suggestion is not permission. Narrow follow-up reads with `{"section":"<name>"}` using a section name the report returned, since the set varies by deployment.

## 2. Follow the symptom

Run only the checks that explain the symptom.

| Symptom | Read-only follow-up | What usually fixes it |
|---|---|---|
| Empty or stale search | `bbox_stats`, the project's registration and publication status, then a targeted `bbox_hybrid_search` | An incremental reindex, once the source itself is confirmed current |
| Recent sessions missing | The hit's `source_freshness`, and the source host's collector observations | Collection or backfill on the source host. Reindexing cannot recover history no producer published. |
| A repository missing from results | `bbox_project_catalog_list`, `bbox_project_publisher_status`, the relevant doctor section | Registration or the identity commit (the `bbox-onboard-project` skill) |
| Results name deleted or moved files | Publication status for that project, then doctor's index findings | Republishing a stale source; a full reindex only when the published source is current and the index still disagrees |
| Hybrid search behaves like keyword search | `bbox_embed_status` with default arguments | The provider or its key, then a re-embed of the affected route |
| Disk growth | `bbox_storage_health`, adding `include_files=true` only for needed detail | A storage GC, previewed first |
| Blackbox tools missing or failing in the session | Your client's MCP connection status and configured surface | Client config or credentials; whether a running session picks up a change depends on the client |

A quick end-to-end smoke that needs no `ops` access:

```text
bbox_project_list()
bbox_hybrid_search(query="<a phrase you know is in the corpus>", limit=5)
```

## 3. Read the evidence within its bounds

- Counts from `bbox_stats` are cached for up to a minute and do not establish coverage or freshness. A healthy total is not evidence that one repository or session is indexed.
- In `bbox_embed_status`, `session_indexed_count` counts embedding successes since daemon start, not corpus size. `capped_count` is retryable enqueue residue. `dropped_count` identifies items that can never be embedded. A non-zero `queue_depth` during a large reindex is normal; one that never drains is not.
- A publication time alone does not prove a collector is alive or that its scan completed.

Escalate observation deliberately. `bbox_embed_status(debug=true)` adds configuration diagnostics cheaply. `include_coverage=true` walks the source corpus and can take minutes, and `include_diagnostics=true` runs connectivity probes (scope them with `diagnostic_routes`). Do not launch these to fill out a checklist. Storage health rescans storage on each call. Follow returned pagination and exact-read hints instead of repeating scans.

## 4. Report, then act only within authorization

Report the observed fault, its scope, the evidence, and the smallest next action that would fix it or distinguish the remaining explanations, with its cost. Before proposing a long rebuild, restore or re-provisioning, check whether a cheap operator-side action exists, such as a credential fix or a managed reset, and surface that choice first.

Run a mutating operation only when the operator has authorized that action; an authorization that already covers a repair sequence covers its steps, and anything outside it needs asking. Read [maintenance.md](maintenance.md) before running any mutating operation, and when asked for routine maintenance instead of a diagnosis. Before restarting or replacing a shared service, find out who depends on it and get approval for that named service.

After an authorized remedy, rerun the specific check that showed the fault, and report what recovered and what did not.

If the root cause is something Blackbox should have detected, prevented or explained, file it (the `bbox-gap` skill).
