---
title: "MCP-surface locality audit: what the zero-checkout-authority cutover regressed"
kind: design
lifecycle: partial
corpus: blackbox-design
topic:
  - daemon-runtime
  - surfaces
tags: [locality, mcp-surface, audit, checkout-mutation-backchannel]
brief: "Complete inventory of every MCP tool's behavior on the zero-checkout-authority cage daemon, classified working / has-lane / no-lane / retired, with the design direction for each unlaned regression. Audit performed 2026-08-12 against beta/blackbox-v2; method was a full sweep of tool_docs.rs mapped to handlers, reading every handler that touches a filesystem path, the checkout-access broker, or a cutover marker."
---

# MCP-surface locality audit

Method: swept all 183 tool names in `crates/bbox-tool-docs/src/tool_docs.rs`,
mapped each to its handler, and read every handler that touches a filesystem
path, the checkout-access broker, or a transport-cutover marker. One
correction to the original audit notes: the cage daemon runs in **catalog
mode** (the locality program cut over; catalog epoch 231+ at audit time), so
the catalog-mutation tools (`project_attach`/`detach`/`promote`/
`scope_migrate`/`default_attachment`) do NOT return
`error.project_catalog_inactive` there — they proceed to daemon-side
filesystem probes and fail structurally **today**, not after some future
activation.

## Class A — works on the zero-authority daemon

Transcript/index reads (`bbox_corpus_search`, `bbox_search`, `bbox_cite`,
`bbox_context`, `bbox_session`, `bbox_messages`, `bbox_topics`,
`bbox_sessions_list`, `bbox_stats`); graph reads (`bbox_hybrid_search`,
`bbox_discover_seed_entities`, `bbox_inspect_entity`, `bbox_find_paths`,
`bbox_bundle_evidence`, `bbox_describe_schema`); vector ops (`bbox_reembed`,
`bbox_embed_partitions`, `bbox_embed_status`); `bbox_edge_compact`;
knowledge/gap reads and GLOBAL-scope writes; coordination stores (`bbox_note*`,
`bbox_thread*`, `bbox_pin`, `bbox_roadmap` except render-with-write_path,
`bbox_inbox`); whiteboards, system events, reactions; the entire
dispatch/orchestration plane (`bro_*`, `atom_*`, `badgey_*`, `consultant_*`,
`identity_*`); artifact catalog reads plus http(s)-source installs;
`bbox_project_list`, `bbox_project_unregister`; storage maintenance
(`bbox_storage_*`); `bbox_mcp_surface`, `bbox_doctor`.

## Class B — regressed, first-class lane exists

| Tool | Agent-visible behavior | Lane |
|---|---|---|
| `bbox_blame` | `error.blame_locality_required` | checkout-local `bro blame`; locality Plan/Resolve protocol |
| `bbox_provenance_export` / `import` | `error.provenance_transport_authoritative` | `bro provenance export` on the checkout host |
| `bbox_render` scope=project | `error.render_locality_required` | managed bound-checkout render (Plan/Complete chunks) |
| `bbox_gap` family, project scope | was `error.knowledge_transport_authoritative` | **checkout-mutation backchannel (this program, 58cb98c3)** |
| `bbox_learn`/`remember`/`decide`/`forget`, project scope | same | **same backchannel** |
| `bbox_project_register` / `bbox_project_init` | `error.project_onboarding_remote` | `bbox-code-collector init` + onboard endpoint |
| `bbox_reindex` | succeeds but covered-project file walks are no-ops (purge fails closed as `empty_root_refused`) | collector code-source transport owns project files; harmless but the stanza should say so |
| `bbox_ref_size` with `file:` refs | refs come back Rejected | use `project_file:` refs (indexed content) |
| `bbox_artifact_install` with local checkout path | fs error | http(s) source |
| `work_*` workspace tools with checkout-host cwd | path errors | bro-harness native tools in the dispatch child |
| `bbox_project_publisher_advance` (covered) | `error.knowledge_transport_authoritative` unless consuming a Ready remote candidate | producer candidate lane (collector publishes, advance consumes) |

## Class C — regressed, NO working tool-shaped lane

| Tool | Behavior on the cage daemon | Design direction |
|---|---|---|
| `bbox_project_eject` (apply) | refuses without active attachment | Two-phase: enqueue .bbox/knowledge writes via backchannel, drop central rows only after collector ack; schema-epoch marker last. PARTIAL backchannel fit |
| `bbox_project_rename` (bridge arm) | canonicalize of new path fails; **Phase 1 registry mutation persists before the failing fs phase — partial-rename risk** | Id-keyed rename without daemon path probe, or collector-reported relocation evidence. Also: make Phase 1/2 atomic or reorder |
| `bbox_bootstrap` | hard error (reads checkout instruction files) | Read lane over the code-source transport, or treat as checkout-host-local convenience (agent reads the files itself) |
| `bbox_project_attach`/`detach`/`promote`/`scope_migrate`/`default_attachment` | catalog is ACTIVE on the cage: these proceed to fs probes/identity proofs the pod cannot perform and fail structurally | Collector-side attach proposal carrying probe evidence (extend the onboard backchannel), or operator-run admin CLI on a host with both catalog and checkout access |
| `bbox_project_publisher_bind` (non-covered projects) | needs the checkout object DB reachable | same bespoke family |
| **`bbox_render` scope=global** | **silent wrong-target write**: renders into pod `$HOME` provider files no interactive host reads | Target-policy decision first (which hosts get global renders), then the backchannel could carry the managed-region writes |
| `bbox_roadmap action=render` with checkout write_path | fs error or silent default-config render | Backchannel write of the single ROADMAP.md; project config from the published snapshot |

## Class D — deliberately retired / non-goal

`bbox_absorb` (compat no-op); catalog read tools
(`project_catalog_list`/`get`, `publisher_status`) are path-free by design;
`publisher_bind`'s covered-project refusal is a deliberate invariant; the
refactor/slice/code-nav surface is harness-native by design (docs/refactor.md).

## Tooling-docs debt

The `tool_docs.rs` stanzas (and everything rendered from them, including
BLACKBOX.md) still teach pre-cutover calls with no mention of refusals or
lanes: `bbox_gap` (:611), `bbox_learn` (:475), `bbox_render` (:532),
`bbox_bootstrap` (:560), `bbox_provenance_import` (:343), `bbox_blame`,
`bbox_project_register`/`init`, `bbox_project_eject` (:374), `bbox_reindex`.
Each stanza needs a lane pointer. `sm-gap-notes` still teaches the bridge-era
inbox spool, which nothing ingests on the estate.

## Standing design rules from the audit

1. Refusals must name the working lane; a refusal that sends an agent to
   hand-author schema JSON is a non-lane (operator ruling 2026-08-12).
2. The generic backchannel shape (daemon validates + computes exact bytes,
   enqueues a durable pending mutation, collector applies and acks, human
   commit is the publish gate) covers any single-file repo-owned write.
   Anything needing multi-step sequencing (eject) or live fs proofs (catalog
   transactions) needs bespoke design.
3. No silent wrong-target writes: a tool that cannot reach its configured
   target must refuse, not succeed into the void (global render is the open
   violation).
