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

The original audit swept the MCP catalog against the zero-checkout daemon.
The project-administration rows below were rechecked against current handlers
in September 2026. Catalog mode is active remotely; absence of checkout
authority is distinct from `error.project_catalog_inactive`.

Catalog attach, promote, attached scope migration, rename, and eject apply
now refuse transport-owned projects before checkout probes or writes with
`error.project_admin_locality_required`. Attach, promote, and scope migration
also require broker-verified existing attachment authority before their local
probes. Existing path spelling is never sufficient proof of attachment
ownership. Attach requires a pre-existing checkout identity marker and never
mints one during a request that may subsequently fail catalog validation.

Detach and default-attachment selection are path-free catalog/attachment
transactions. They remain available without a mounted checkout; they are not
members of the checkout-probe failure family.

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
`bbox_project_list`, `bbox_project_unregister`, `bbox_project_detach`,
`bbox_project_default_attachment`; storage maintenance
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
| `bbox_project_eject` (apply) | transport-owned projects refuse early; other projects need an exact base mutation lease | No remote ejection lane exists. Preserve central entries; source onboarding does not migrate them. A future lane needs collector acknowledgment before central removal and a final schema marker. |
| `bbox_project_rename` | catalog relocation refuses transport-owned targets before probing the new path; local relocation requires the existing checkout marker | No remote relocation lane exists. In bridge mode canonicalization already precedes registry mutation, correcting the earlier audit claim. Later multi-store migration can still fail; `error.project_rename_partial` reports registry durability, migration completion, and old/new recovery coordinates. It is not an atomic transaction. |
| `bbox_bootstrap` | hard error (reads checkout instruction files) | Read lane over the code-source transport, or treat as checkout-host-local convenience (agent reads the files itself) |
| `bbox_project_attach`/`promote`/`scope_migrate` | explicit early locality refusal on transport-owned projects; other projects require verified existing local attachment authority before probes | Collector onboarding enrolls sources, not arbitrary attachment transitions. Offline `blackbox project-catalog promote` exists for an administrator holding the authoritative catalog and every attachment checkout. Offline `scope-migrate` is a distinct operator-attested, unattached workflow, not a drop-in substitute for attached MCP migration. No attach CLI or remote attached-migration lane is implemented. |
| `bbox_project_publisher_bind` (non-covered projects) | needs the checkout object DB reachable | same bespoke family |
| **`bbox_render` scope=global** | RESOLVED: refuses on the cage (`error.global_render_authority`); operator hosts pull instead via `bro render global` -> `bbox_render(scope=global, global_plan)` -> host-local managed-region apply | Pull model: the host that runs the apply is the target policy; no push lane |
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
   target must refuse, not succeed into the void (global render was the open
   violation; it now refuses, and `bro render global` is the host-side lane).

## Project-administration operating limits

These tools belong on the operator administration surface, not ordinary agent
retrieval discovery. Catalog list/get and publisher status remain general reads.
This audit changes runtime locality checks and tool descriptions; catalog
visibility policy is owned by the surface chooser.

For remote enrollment, use the configured checkout-host collector and its
existing onboarding flow. Do not use `bbox_project_attach` to reinterpret a
caller path as a daemon path. Local attach can add an initialized checkout only
when the daemon already has valid attachment authority for the target project.
A project without such authority requires an administrator workflow; the tool
does not invent a checkout identity or silently create a replacement project.

Promotion requires an administrator with both the authoritative catalog and
all recorded checkout roots. The implemented offline promotion command is
documented in [operating-blackbox](../../docs/operating-blackbox.md).
The offline scope-migration command uses operator attestation and different
preconditions; callers must not infer or supply that authority themselves.
An old directory removed during a relpath move can no longer grant a normal
checkout lease, so the attached MCP path refuses rather than bypassing it.

Eject still has no multi-step remote delivery protocol. Its local apply writes
repo entries, confirms central-store persistence, then writes the schema
marker. Failures after writes carry `error.project_eject_partial` and identify
which stage remains uncertain. This ordering does not make the multi-store
operation transactional.

Bridge rename persists the registry before migrating its many owner stores.
A partial failure must be repaired with the reported old and new coordinates;
a plain retry using the new registry record cannot recover references still
keyed to the former path. Missing target canonicalization itself occurs before
registry mutation. Catalog rename uses a single pair-store relocation and does
not rewrite those owner stores.
