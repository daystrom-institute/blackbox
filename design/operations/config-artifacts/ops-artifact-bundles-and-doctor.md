---
title: "Ops Artifact Bundles And Doctor"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - operations
  - config-artifacts
---

# Ops Artifact Bundles And Doctor

Date: 2026-05-13
Status: design proposal v1
Regrounded: 2026-05-30

## Regrounding Note (2026-05-30)

This proposal has not been implemented. As of this regrounding, `ArtifactKind`
is still exactly `Workflow, Packet, Brofile, Agent, Atom, Team, Cron`
(`src/artifacts.rs:16-24`); there is no `Poller`/`Webhook`/`Bundle` variant, no
`ArtifactActivator` trait, no `BundleManifest`, no `bbox_doctor`/
`bbox_upgrade_check`, no `runtime_paths` helpers, and no `InletRuntimeStatus`.
`ArtifactMetadata` (`src/artifacts.rs:83-107`) carries none of the proposed
`bundle`/`bundle_generation`/`managed_by`/`runtime_ref`/`role` fields. The
Phase 0-7 plan in the companion doc is still all forward work.

Since v1 the shipped `system-defaults/` surface has grown well past the handful
of members the original examples imply. The full default surface is 275 JSON
files (264 excluding sidecars/templates) plus markdown and script assets:

| Category | Count | Install path today |
|---|---|---|
| atoms | 140 | `bbox_artifact_install kind=atom` |
| workflows | 41 | `bbox_artifact_install kind=workflow` or `bro_workflow_install` |
| brofiles | 35 | `bbox_artifact_install kind=brofile` |
| packets | 30 (+10 `.audit_examples.json` sidecars) | `bbox_artifact_install kind=packet` |
| agents | 6 | `bbox_artifact_install kind=agent` |
| crons | 6 | partial artifact path or `bro_cron_install` |
| macros | 4 | **compiled into the binary** (`include_str!`) |
| teamplates | 2 (+1 inline team) | **shell script** curling `/admin/team/upsert` |
| mcp-surfaces | 1 | `bbox_compile` |
| system memories | 28 `.md` | **loaded at daemon init**, never installed |
| scripts/fixtures | 7 `.py`/`.sh` + 2 `.md` | workflow-referenced assets, never staged |

Two categories are **already auto-loaded** and are NOT manual-install gaps:

- **System memories** load at startup via
  `system_memory::init(&cfg.paths.defaults_memories_dir, ...)`
  (`src/server/open.rs:236`). They are markdown runbooks resolved from a
  defaults dir, not versioned JSON specs, and editing one needs no rebuild.
  They stay out of the artifact/bundle model; doctor only verifies the catalog
  loaded. See [Auto-Loaded Default Surfaces](#auto-loaded-default-surfaces).
- **Macros** are currently `include_str!`-baked into the binary via
  `MacroRegistry::builtin_definitions()` (`src/macros/registry.rs:195-215`).
  This is a wart: tweaking a macro requires a daemon rebuild. This design
  **migrates macros off `include_str!`** and onto the managed-artifact path so
  they are installed from disk like every other default. See
  [Macro Management](#macro-management).

New tool families exist that v1 did not account for: `macro_*` (registry-backed
macro lifecycle), `reaction_*` (event-reaction inlets, `src/system_events/`),
and `identity_*` (read-only actor identities). Reactions are an inlet-class
managed kind; identities are read-only and out of scope.

The sections below have been updated against this reality. Where a section still
reflects v1 framing, the regrounded detail is called out inline.

## Problem

Blackbox now ships enough daemon-owned machinery under `system-defaults/` that
manual install order is becoming an operations liability.

Today the artifact catalog manages only:

- workflows
- packets
- brofiles
- agents
- atoms
- teams
- crons

That list is still narrower than the shipped default surface. Cron support has
started moving into the catalog, but pollers, webhooks, bundles, and MCP surface
routing are not yet managed through the same lifecycle. The runtime already has
install/list tools for crons, pollers, and webhooks. Those objects are
operational artifacts: they are versioned JSON specs, daemon-owned, installed
into runtime registries, and need upgrade/uninstall semantics. Any remaining
invisibility is historical, not principled.

The current day-2 runbooks also scatter health and upgrade checks across
`bbox_stats`, `bbox_embed_status`, `bbox_project_list`, `bbox_describe_schema`,
`bbox_inbox`, `bbox_lint`, systemd journal inspection, and manual reindex or
re-embed decisions. There is no single "what do I need to know about Blackbox
right now?" tool, and no upgrade helper that mechanizes required post-upgrade
actions.

There is also a tool-surface problem. Direct runtime install/list tools for
workflows, crons, pollers, and webhooks duplicate the lifecycle semantics the
artifact catalog should own. Leaving both surfaces around indefinitely is code
hoarding: operators have to remember which install path records provenance,
which path is uninstallable, which one survives upgrades cleanly, and which list
tool shows the truth.

## Current Code Baseline

Artifact catalog:

- `src/artifacts.rs` defines `ArtifactKind::{Workflow, Packet, Brofile, Agent,
  Atom, Team, Cron}`.
- `ArtifactCatalog::install_value[_scoped]` stores active JSON payloads,
  `metadata.json`, `.versions/v<version>.json`, and
  `.versions/v<version>.metadata.json`.
- Active installs are content-hash idempotent via canonical JSON SHA-256.
- `ArtifactMetadata` records source, version, install time, active flag,
  content hash, optional project id, supersession fields, and install warnings.
- `bbox_artifact_install`, `bbox_artifact_list`, `bbox_artifact_supersede`, and
  `bbox_artifact_remove` are the public MCP tools in `src/tools/artifacts.rs`.
  Cron is already accepted as an artifact kind, but its activation still writes
  the old runtime path and has not gone through the planned activator/path
  extraction.
- Hard remove exists for one global artifact and calls `deactivate_artifact`
  before deleting catalog files.
- Project-scoped artifacts exist in the storage layer and `.bbox/` watcher, but
  `ArtifactCatalog::list` currently walks only the global `<root>/<kind>/`
  directories.

Activation path:

- `install_artifact_value` in `src/server/routes.rs` validates each known kind
  through its native runtime path before recording metadata.
- Workflow install compiles and writes the orchestration runtime store's
  `workflows/<id>.json`.
- Packet install compiles into the packet store.
- Brofile install writes the global brofile registry and verifies resolution.
- Agent install validates dependencies, computes manifest embeddings, records
  provenance edges, and may enqueue agent-manifest embeddings.
- Atom install validates dependency references.
- Team install is catalog-only today.
- `deactivate_artifact` removes workflow files, packet domains, brofiles, and
  cron runtime files; agents, atoms, and teams currently have no separate
  runtime registry to tear down.

Inlet runtime:

- `bro_webhook_install`, `bro_poller_install`, and `bro_cron_install` live in
  `src/tools/orchestrate.rs`.
- Webhooks persist under the orchestration runtime store's
  `webhooks/<name>.json`, install into
  `WebhookRegistry`, and expose `/webhook/<name>`.
- Pollers persist under the orchestration runtime store's
  `pollers/<name>.json`, install into
  `PollerRegistry`, and spawn a tick loop.
- Crons persist under the orchestration runtime store's `crons/<name>.json`,
  install into
  `CronRegistry`, validate schedule syntax, and spawn a tick loop.
- Those tools expose list operations, but pollers and webhooks have no catalog
  metadata, supersession chain, bundle membership, hard uninstall, or source
  tracking. Cron has partial artifact support but no bundle metadata or new
  daemon-owned runtime path yet.
- `CronRegistry` already exposes `remove`, which aborts the handle and drops
  run state. `PollerRegistry` keeps `JoinHandle`s and aborts the previous handle
  on reinstall but does not expose uninstall/remove. `WebhookRegistry` has
  install/get/list and no remove API. Managed removal therefore still requires
  new registry methods for pollers/webhooks and a status-oriented cleanup pass
  for cron, not just wiring existing functions into `deactivate_artifact`.
- Daemon startup restores webhooks, pollers, and crons directly from that
  runtime store and respawns poller/cron loops. That restore path bypasses the
  artifact catalog today, so doctor must treat these runtime files as
  potentially unmanaged until the artifact path owns them.

System defaults:

- `system-defaults/system-defaults.md` says the daemon does not auto-install the tree.
- `docs/artifact-catalog.md` calls `system-defaults/` the shipped catalog
  source, but explicitly treats crons/webhooks as install-order dependencies
  rather than catalog kinds.
- `system-defaults/badgey/crons/*.json`,
  `system-defaults/agentic-corpus/crons/*.json`, and
  `system-defaults/agents/crons/*.json` are therefore shipped defaults that
  can be installed through the partial cron artifact path but still lack bundle
  membership and neutral runtime-store semantics.
- `system-defaults/mcp-surfaces/routing.json` is also shipped default
  machinery, but it is installed through `bbox_compile`, not the artifact
  catalog.
- Runtime serde ignores extra top-level fields in inlet JSON specs, while the
  artifact catalog requires a version to install. Shipped cron specs already
  carry top-level `version` fields. Future shipped poller/webhook defaults must
  do the same; adoption/backfill of already-active user specs can synthesize
  `version="unmanaged"` with an explicit warning. Do not add `version` or
  `supersedes` fields to runtime structs just for cataloging; serde's existing
  tolerance for extra fields is the compatibility boundary.

Operations baseline:

- `docs/operating-blackbox.md` defines the manual health smoke:
  `bbox_stats`, `bbox_embed_status`, `bbox_project_list`,
  `bbox_describe_schema`, and `bbox_hybrid_search`.
- `docs/operations.md` defines protected vs rebuildable stores and the
  post-upgrade/manual maintenance checklist.
- Startup already runs one schema-like maintenance action:
  `ArtifactCatalog::backfill_content_hashes`.

Newly surfaced subsystems and gaps (2026-05-30 regrounding):

- **Team activation is a no-op.** `install_artifact_value`'s `Team` arm is a
  bare comment, "Teams are stored as artifacts but have no additional validation
  at install time" (`src/server/routes.rs:1065-1067`); the deactivate arm is the
  same (`routes.rs:1414-1416`). The actual team/teamplate registry lives in
  `src/orchestration/team.rs` (`save_teamplate`, `save_team`, `load_all_teams`),
  and shipped teams are installed today by a **shell script**,
  `system-defaults/agentic-corpus/scripts/install-teams.sh`, which curls
  `/admin/team/upsert` (`admin_team_upsert`, `routes.rs:2220`). Teamplates ship
  as JSON (`system-defaults/phase-decompose/teamplates/*.json`, identity `name`,
  members declared by brofile ref), but the `contradiction-specialists` team
  exists only inline in the shell script, not as a file.
- **Macros are compiled into the binary.** The four shipped
  `system-defaults/macros/*.json` are pulled in by `include_str!` in
  `MacroRegistry::builtin_definitions()` (`src/macros/registry.rs:195-215`) and
  surface at `builtin` scope. The registry also resolves `user`
  (`~/.config/blackbox/macros/` or `BLACKBOX_MACROS_DIR`) and `project`
  (`<project>/.bbox/macros/<id>.json`) scopes. Lifecycle runs through
  `macro_register`/`macro_unregister` (`src/tools/macros.rs:341-405`), not the
  artifact catalog. Macro identity is `id`; specs carry `version` (currently
  "not yet consumed — the registry resolves by id only").
- **Reactions are an inlet-class kind with a parallel lifecycle tool.**
  `ReactionSpec` (`src/system_events/types.rs:324-341`) carries `name`,
  `version: u32`, `event_kinds`, `when`, `action`, `retry`, `on_failure`.
  `reaction_install` "Validates and persists to disk"
  (`src/tools/system_events.rs:236`), persisting under a reactions dir, exactly
  paralleling `bro_cron_install`/`bro_poller_install`/`bro_webhook_install`.
  There are no shipped reaction defaults yet. `identity_get`/`identity_list` are
  read-only (no install path) and out of scope.
- **Workflow activation does not stage assets.** The `Workflow` arm only
  compiles, capability-validates, and writes the spec to `store_dir/workflows`
  (`routes.rs:1027-1041`). But shipped workflows reference sibling assets by
  repo-relative path — e.g.
  `system-defaults/workflows/phase-decompose/main.json` references
  `system-defaults/phase-decompose/scripts/epoch-check.py`. A bundle that
  installs such a workflow must guarantee those `.py`/`.md` assets resolve; the
  catalog has no concept of workflow assets today. See
  [Workflow Assets](#workflow-assets).
- **`.audit_examples.json` are companion datasets, not auto-loaded.** They pair
  with packets (`entry-quality.json` ↔ `entry-quality.audit_examples.json`) but
  have **no runtime consumer in `src/`** — they are validated only by a shipped
  test (`shipped_packet_audit_examples_pass`, `src/tools/artifacts.rs:1054`) and
  are the `{entity, expected}` datasets an operator feeds to `bbox_audit`. They
  are not separate artifacts and are not read by packet install/compile. A
  bundle treats the sidecar as travelling with its packet member for hashing and
  copy, not as an installable unit.

## Thesis

**All daemon-owned installable JSON specs should be managed artifacts.**

Runtime registries are activation targets. The artifact catalog is the
source-tracked lifecycle ledger. Bundles operate over the ledger and dispatch
kind-specific activation/deactivation adapters.

The runtime store remains the restart source for active daemon behavior. Current
code calls this `state.store_dir` and physically places it under the `bro` home,
but crons, pollers, webhooks, and workflows are not conceptually "bro-owned".
They are daemon runtime objects. Managed artifact activation therefore writes
both:

- catalog payload/metadata under `artifacts/`
- runtime payload under the daemon runtime object store

The catalog answers "who installed this, from which source, in which bundle
generation?" The runtime store answers "what should this daemon activate after
restart?" Doctor's drift checks exist because those stores can diverge.

Naming cleanup: do not preserve `blackbox/bro/crons` as the design language.
The implementation should introduce neutral daemon-owned paths:

```text
~/.local/state/blackbox/runtime/workflows/
~/.local/state/blackbox/inlets/crons/
~/.local/state/blackbox/inlets/pollers/
~/.local/state/blackbox/inlets/webhooks/
```

Crons, pollers, and webhooks are event inlets. Workflows are executable runtime
specs. Existing files under the old `bro` runtime home can be adopted by
doctor/upgrade-check, but new design text should call this the daemon runtime
store.

This changes the mental model from:

```text
some defaults use bbox_artifact_install
some defaults use bro_cron_install
some defaults use bbox_compile
operator remembers order
```

to:

```text
system-default bundle manifest
  -> plan dependency-ordered operations
  -> activate each member through its native adapter
  -> record each member in artifact metadata
  -> record bundle membership and install generation
```

And it changes the public ops surface from many kind-specific lifecycle tools to
one artifact lifecycle surface:

```text
bbox_artifact_install
bbox_artifact_plan
bbox_artifact_bundle_apply
bbox_artifact_bundle_list
bbox_artifact_remove
bbox_doctor
bbox_upgrade_check
```

Runtime-specific helpers can remain as internal functions. They should not stay
as parallel MCP tools once the managed artifact path covers the kind.

## Vocabulary

### Managed Artifact

A managed artifact is a versioned JSON spec with:

- a stable kind
- a stable name
- a version
- a source path or URL
- a content hash
- a native validator
- an activation adapter
- a deactivation adapter

Initial managed kinds:

| Kind | Native owner | Name field | Activation |
|---|---|---|---|
| `workflow` | workflow registry | `name` | compile, capability validate, write runtime workflow spec |
| `packet` | packet store | `domain` | compile packet |
| `brofile` | brofile store | `name` | save and resolve brofile |
| `agent` | agent catalog/registry | `name` | validate, embed manifest, provenance edges |
| `atom` | atom catalog/registry | `name` | validate atom install |
| `macro` | macro registry | `id` | validate `inputs_schema`, register into `MacroRegistry` (replaces `include_str!` builtins) |
| `teamplate` | team store (`teamplates/`) | `name` | validate member brofile refs exist, `save_teamplate` |
| `team` | team store (`teams/`) | `name` | resolve members, `save_team` / `admin_team_upsert` (replaces `install-teams.sh`) |
| `cron` | cron registry | `name` | validate schedule + routing packet, persist, spawn loop |
| `poller` | poller registry | `name` | validate fetch/selector shape + routing packet, persist, spawn loop |
| `webhook` | webhook registry | `name` | validate signature policy + routing packet, persist endpoint |
| `reaction` | `EventHub` | `name` | validate `event_kinds`/`action`, persist spec, register in `EventHub` (event-reaction inlet; **not** an HTTP endpoint) |

Notes on the kind table:

- `macro` identity is `id`, not `name`; the activator's `name()` reads
  `value["id"]`. Macros already carry `version`. This kind exists to take the
  shipped macros off `include_str!` so editing one is a reinstall, not a
  rebuild — see [Macro Management](#macro-management).
- `teamplate` and `team` are split rather than collapsed: a teamplate is a
  reusable template (members declared by brofile + alias + count); a team is an
  instantiated roster. Both are owned by `src/orchestration/team.rs`, and both
  retire the `install-teams.sh` shell path — see
  [Team And Teamplate Activation](#team-and-teamplate-activation).
- `reaction` is added now for surface consistency even though no defaults ship
  yet: `reaction_install`/`reaction_list` already duplicate the inlet lifecycle
  the catalog should own. Reactions persist a JSON spec under the reactions dir
  and register in `EventHub` (`src/system_events/hub.rs`); there is no HTTP
  endpoint and, critically, **no remove API today** — `EventHub` exposes install
  + restore but no `remove_reaction`. That teardown method is a prerequisite
  before `reaction` deactivation/removal can be wired (see impl Phase 1).
- `identity` (`identity_get`/`identity_list`) is read-only and is **not** a
  managed kind.

MCP surfaces should be managed as `packet` artifacts with an artifact role, not
as a separate `ArtifactKind`. `system-defaults/mcp-surfaces/routing.json` is
already a packet-shaped compile spec (`domain`, `version`, `scope`,
`classification_lattice`, `rules`). A separate `mcp_surface` enum variant would
add future churn without changing activation semantics. Use:

```jsonc
{
  "kind": "packet",
  "role": "mcp_surface",
  "source": "system-defaults/mcp-surfaces/routing.json",
  "name": "mcp-surface/routing"
}
```

Doctor can still report missing MCP surface routing by querying packet metadata
for `role == "mcp_surface"` or by checking the known domain.

### Bundle

A bundle is itself a managed artifact whose payload lists member artifacts and
their desired lifecycle state.

```jsonc
{
  "name": "blackbox-system-defaults",
  "version": 1,
  "description": "Default Blackbox agents, atoms, inlets, packets, workflows, and MCP surfaces.",
  "members": [
    {
      "kind": "packet",
      "source": "system-defaults/agentic-corpus/packets/cron-routing/embed-compaction.json",
      "name": "cron-routing/embed-compaction"
    },
    {
      "kind": "workflow",
      "source": "system-defaults/agentic-corpus/workflows/embed-compaction-arc.json"
    },
    {
      "kind": "cron",
      "source": "system-defaults/agentic-corpus/crons/embed-compaction-nightly.json"
    }
  ]
}
```

The bundle records intent. The installer computes actual names, versions, hashes,
dependency order, and operation status.

Installing a bundle manifest and applying a bundle are separate operations:

- `bbox_artifact_install(kind="bundle", source=...)` records and versions the
  bundle manifest itself. It does not recurse into members.
- `bbox_artifact_bundle_apply(source=..., operation=...)` records the bundle
  manifest if needed, expands members, plans, validates, activates/deactivates,
  and writes a generation record.

That split avoids surprising recursion in the single-artifact install path and
keeps "catalog this manifest" distinct from "mutate the daemon to match this
manifest."

### Bundle Generation

Every apply of a bundle creates a generation record:

```jsonc
{
  "bundle": "blackbox-system-defaults",
  "bundle_version": "1",
  "generation": "gen-20260513-...",
  "operation": "install|reinstall|uninstall|verify",
  "started_at": "...",
  "finished_at": "...",
  "members": [
    {
      "kind": "cron",
      "name": "embed-compaction-nightly",
      "version": "1",
      "source": "system-defaults/agentic-corpus/crons/embed-compaction-nightly.json",
      "content_sha256": "...",
      "status": "installed",
      "runtime_status": "active",
      "warnings": []
    }
  ]
}
```

The generation is the uninstall boundary. "Remove system defaults" means remove
the members installed by the selected bundle generation, not every artifact whose
source happens to live under `system-defaults/`.

## Auto-Loaded Default Surfaces

Not everything under `system-defaults/` is a manual-install gap. Two categories
are loaded by the daemon directly and are deliberately **out of the
artifact/bundle model**:

- **System memories** (`system-defaults/memories/*.md`, 28 files) load at
  startup through `system_memory::init(&cfg.paths.defaults_memories_dir, ...)`
  (`src/server/open.rs:236`). They are markdown runbooks resolved from a
  configurable defaults dir, not versioned JSON specs with activators. Editing a
  memory already needs no rebuild. Forcing them into the JSON-spec activator
  path would add lifecycle ceremony without removing any real friction. They
  stay auto-loaded; doctor's `knowledge`/`daemon` sections only verify the
  catalog loaded and the memories dir resolved.

- **Builtin macros** are currently in this category by accident:
  `include_str!` bakes the four shipped macro JSONs into the binary
  (`MacroRegistry::builtin_definitions()`). Unlike memories, this *does* impose
  a rebuild to edit. This design moves macros **out** of the compiled-in
  category and onto the managed-artifact path (next section). After that
  migration, only system memories remain genuinely auto-loaded.

The distinguishing test: a default belongs out of the bundle only if it is
loaded by a dedicated runtime loader AND editing it needs no rebuild. Memories
pass; compiled-in macros fail the second clause, which is why they move.

## Macro Management

Macros become a managed `macro` artifact kind. The driving requirement: tweaking
a macro must not require a daemon rebuild, and macros should live inside the same
version/source/generation/drift lifecycle as every other default.

Current state (`src/macros/registry.rs`):

- `builtin_definitions()` uses `include_str!` to embed the four shipped macros at
  compile time, surfaced at `builtin` scope.
- The registry also resolves `user` (`~/.config/blackbox/macros/` or
  `BLACKBOX_MACROS_DIR`) and `project` (`<project>/.bbox/macros/<id>.json`)
  scopes from disk.
- `macro_register`/`macro_unregister` (`src/tools/macros.rs`) write the
  **project** scope only (they require `project_dir` and write
  `<project>/.bbox/macros/`); the catalog is not involved. `MacroScope`
  (`src/macros/model.rs:39`) is `Builtin | User | Project` — there is no managed
  variant yet.

Constraint to respect: today there is **no managed/global macro scope**. The
registry resolves project → user → `builtin_definitions()` only
(`src/macros/registry.rs:618-629`), and `macro_register` writes the **project**
scope exclusively (`registry.rs:649`). So an artifact macro activator cannot just
"register into the registry" — there is nowhere managed for it to write. The
migration must add a scope before it can remove the builtins.

Target state, in order (the sequencing is load-bearing — see impl Phase 2/4):

1. **Add a managed macro scope + loader.** Add a `MacroScope::Managed` variant
   (`src/macros/model.rs:39` is `Builtin | User | Project` today) and a
   daemon-owned macros dir (e.g. `inlets/`-sibling `macros/` runtime store, or a
   global macro dir) that the registry loads with defined precedence
   (project → user → **managed** → builtin, so the managed scope shadows the
   compiled-in fallback). The loader assigns scope **by source directory**, not
   by the payload's `scope` field — shipped macro JSONs carry `"scope":
   "builtin"` (e.g. `builtin.java.lombok.json`), and the activator normalizes
   that to `managed` when installing from the bundle. The macro activator parses
   `MacroDefinition`, validates `inputs_schema`, writes the managed dir
   (deactivate = remove from the managed dir). Identity is `id`; version from the
   spec.
2. **Ship the four macros as members of the refactor bundle** (they are Java
   refactor macros). `bbox_artifact_bundle_apply` installs them into the managed
   scope.
3. **Only after** the managed scope exists and the refactor bundle can be applied
   and verified, **remove the `include_str!` builtin block**. Until then the
   builtins remain as a fallback so a daemon that has not applied the bundle does
   not lose the macros.
- **Migration policy:** prefer requiring `bbox_artifact_bundle_apply` for the
  refactor bundle as a documented post-upgrade step surfaced by
  `bbox_upgrade_check`, over a silent startup auto-install (consistent with the
  no-startup-auto-install non-goal). While the builtins still exist as fallback,
  doctor reports the shipped macros as `info` (managed copy not yet installed);
  after the builtins are removed, the same gap becomes `action`.
- `macro_register`/`macro_unregister` remain for interactive **project-scope**
  macro authoring (they require `project_dir` and write `.bbox/macros/`, the
  macro analog of editing a `.bbox/` file), but shipped defaults flow through the
  artifact path. See the tool-surface table for the
  disposition.

## Team And Teamplate Activation

v1 listed `team` as "catalog only until team activation is formalized." That
formalization is now required work, because the real install path today is a
shell script.

Current state:

- `install_artifact_value` `Team` arm and `deactivate_artifact` `Team` arm are
  both no-ops (`src/server/routes.rs:1065-1067`, `1414-1416`).
- The team/teamplate store is `src/orchestration/team.rs`: `save_teamplate`,
  `list_teamplates`, `save_team`, `load_all_teams`, `remove_team`, all under
  `store_dir/teamplates/` and `store_dir/teams/`.
- Shipped teamplates: `system-defaults/phase-decompose/teamplates/*.json`
  (`phase-decomposer-panel`, `phase-recompose-council`), members declared by
  `{ brofile, alias, count }`.
- The `contradiction-specialists` team is **not a file** — it exists only inline
  in `system-defaults/agentic-corpus/scripts/install-teams.sh`, which curls
  `/admin/team/upsert`.

Target state:

- `teamplate` activator: validate every member brofile ref resolves
  (dependency edge: brofiles activate before teamplates), then `save_teamplate`.
  Deactivate removes the teamplate. The shipped teamplate JSON is already the
  `Teamplate` shape (`name`, `version`, `members[{brofile,alias,count}]`), so the
  teamplate spec needs no new type.
- `team` activator: **needs a dedicated install spec, not the runtime `Team`.**
  The runtime `Team` (`src/orchestration/team.rs:109`) carries activation/runtime
  fields (`created_at`, session/history, expanded members) that must not be
  hand-authored in a shipped default. The existing admin path
  (`admin_team_upsert`, `routes.rs:2214-2225`) only accepts
  `members: Vec<String>` and synthesizes aliases/teamplate/team. Define a
  `TeamArtifactSpec` (name + member refs, optionally a teamplate ref) whose
  activator deterministically materializes a runtime `Team` via the same path
  `admin_team_upsert` uses, then `save_team`. Deactivate `remove_team`. Do not
  ship a raw runtime `Team` blob.
- Promote `contradiction-specialists` to a shipped `TeamArtifactSpec` JSON
  (`system-defaults/agentic-corpus/teams/contradiction-specialists.json`) and
  retire `install-teams.sh`. The agentic-corpus maintenance bundle owns it.
- Teamplate members reference brofiles; the bundle planner must order brofiles
  before teamplates before any teamplate-backed team.

## Workflow Assets

Workflow activation today writes only the spec (`routes.rs:1027-1041`), but
shipped workflows reference sibling assets by repo-relative path. Confirmed
example: `system-defaults/workflows/phase-decompose/main.json` references
`system-defaults/phase-decompose/scripts/epoch-check.py`. The phase-decompose
tree alone ships 6 `.py` scripts and 2 `.md` fixtures that workflows and hooks
invoke by path.

**Install-time validation alone is insufficient — the problem is runtime
resolution.** Shell ops resolve `cwd` from the op args, then the worktree, then
`meta.project_dir` (`src/workflow/ops/external.rs:112`), and the shipped argv is
a repo-relative path with `cwd: "${vars.project_dir}"`
(`system-defaults/workflows/phase-decompose/main.json:220`). So the asset only
resolves when the **target project happens to be this repo**. Validating that
`system-defaults/phase-decompose/scripts/epoch-check.py` exists in the defaults
tree at plan time does nothing for a workflow run against some other project's
`project_dir`. A managed default that ships an asset reference must make that
reference resolve regardless of the run's `project_dir`.

Options:

- **Asset-root interpolation (preferred).** Add a `${defaults_dir}` /
  `${asset_root}` interpolation that the workflow executor resolves to the
  managed asset location (the configured defaults dir, same source memories
  already resolve from). Rewrite shipped workflow argv to
  `${asset_root}/phase-decompose/scripts/epoch-check.py` so the script path is
  independent of the run's `project_dir`. The bundle also validates the asset
  exists at plan time, but resolution no longer depends on cwd.
- **Staged absolute paths (heavier).** Copy referenced assets into a daemon
  runtime asset dir and rewrite argv to absolute managed paths. Needed only for
  deployments without the source tree present at runtime.

Decision: introduce `${asset_root}` interpolation and migrate shipped workflows
that reference assets to use it; keep plan-time existence validation as a
backstop. Pure existence-validation against the defaults tree is explicitly
**rejected** as the sole mechanism because it does not survive a foreign
`project_dir`.

### Out-of-tree assets

Not every shipped workflow's asset lives under `system-defaults/`.
`system-defaults/agentic-corpus/workflows/nightly-eval-arc.json:32` runs
`bash eval/run-agentic-eval.sh` with `cwd: "${meta.project_dir}"`, and
`eval/run-agentic-eval.sh` lives at the **repo root**, not under
`system-defaults/`. This workflow is effectively a *repo-self-eval* — it only
runs correctly when `project_dir` is this repository. Two consequences:

- The "automate everything in `system-defaults/` in one bundle" goal cannot
  bundle this asset, because the asset is not in the tree.
- Such repo-internal workflows are classified separately from portable defaults.

Decision: `nightly-eval-arc` is **repo-internal and excluded** from the portable
system-defaults bundle — it evaluates this repository's own corpus and only runs
correctly when `project_dir` is this repo. The agentic-corpus bundle's workflow
`source_glob` excludes `nightly-eval-arc.json` (see ownership note in the bundle
layout). If a future need makes it portable, relocate
`eval/run-agentic-eval.sh` under the defaults tree and switch the workflow to
`${asset_root}`. More generally, the planner must reject any bundle member whose
referenced asset it can neither place nor resolve, rather than ship a workflow
that silently fails at run time.

## Proposed MCP Surface

### `bbox_artifact_install`

Extend `kind` to include:

- `cron` (already partially present; included here for path/activator/bundle
  completion)
- `poller`
- `webhook`
- `reaction`
- `macro` (identity field is `id`; replaces the `include_str!` builtins)
- `teamplate`
- `team` (gains a real activator; no longer catalog-only)
- `bundle`

Keep the existing artifact install shape: `source`, optional `name`, optional
`version`, and optional `supersedes`, and add optional `role` for artifacts such
as MCP-surface packets whose activation kind stays `packet` but whose ops role
matters to doctor and bundle planning. New kinds use that same shape so
operators learn one lifecycle command instead of one command family per runtime
object.

Version rule for inlet kinds:

- Managed cron/poller/webhook sources under `system-defaults/` must carry a
  top-level `version`. Shipped cron specs already do; future poller/webhook
  defaults must follow the same rule. Runtime structs ignore the extra field
  while `ArtifactCatalog` records it.
- For direct adoption of already-installed runtime specs that lack `version`,
  backfill should synthesize `version="unmanaged"` or require an explicit
  `version` override. Do not silently store `version=1` for unknown user specs;
  that erases whether the version was declared or inferred.

Version synthesis belongs in the adoption/backfill path, not in the base catalog
writer. Normal `bbox_artifact_install` should continue rejecting missing
versions. A separate adoption operation can read an existing runtime spec, add
`version="unmanaged"` to the catalog payload, and record an install warning that
the version was inferred from an already-active object.

Implementation detail: `install_artifact_value` should become a dispatch over an
`ArtifactActivator` trait instead of a growing `match` block.

```rust
trait ArtifactActivator {
    fn kind(&self) -> ArtifactKind;
    fn name(&self, value: &Value) -> anyhow::Result<String>;
    fn version(&self, value: &Value) -> anyhow::Result<String>;
    fn validate(&self, state: &SharedState, value: &Value) -> anyhow::Result<PreparedArtifact>;
    fn activate(&self, state: &SharedState, prepared: PreparedArtifact) -> anyhow::Result<ActivationResult>;
    fn deactivate(&self, state: &SharedState, name: &str) -> anyhow::Result<DeactivationResult>;
}
```

Helper shapes:

- `PreparedArtifact` is the parsed/validated runtime payload plus any resolved
  dependency refs needed for activation; validation must not mutate registries or
  runtime files.
- `ActivationResult` records the runtime ref, activated content hash, and
  warnings to persist into artifact metadata/generation records.
- `DeactivationResult` records whether runtime state, persisted runtime files,
  and per-name side state were actually removed.

This keeps runtime-specific behavior in one place per kind and lets bundles run
the same path a single install uses.

The implementation is not a trivial wrapper. `install_artifact_value` currently
conflates validation, runtime activation, catalog write, agent embedding, and
provenance edge persistence. The first implementation should extract one
activator at a time while preserving exact existing behavior for the current
kinds before adding poller, webhook, and bundle support.

### `bbox_artifact_plan`

Dry-run an install, reinstall, uninstall, or verify operation.

```text
bbox_artifact_plan(
  operation="reinstall",
  bundle="blackbox-system-defaults",
  source="system-defaults/bundles/blackbox-system-defaults.json"
)
```

Returns:

- dependency-ordered member operations
- no-op members with same active hash
- members that will be deactivated/replaced
- runtime objects that exist but are not catalog-managed
- destructive actions requiring `confirm=true`
- warnings for missing dependencies, inactive dependencies, and unknown kinds

Planning must validate the whole bundle before activating any member. Inlets
currently install and activate in one call (`bro_cron_install` and
`bro_poller_install` persist then spawn loops immediately), so bundle apply
should not delegate to those MCP tools internally. It should call kind-specific
preflight validators first, then activate only after dependency validation has
passed.

### `bbox_artifact_bundle_apply`

Apply a bundle plan.

```text
bbox_artifact_bundle_apply(
  operation="reinstall",
  source="system-defaults/bundles/blackbox-system-defaults.json",
  confirm=true
)
```

Operations:

- `install`: install missing or changed members, leave extra unmanaged runtime
  objects alone.
- `reinstall`: uninstall the previous generation for this bundle, then install
  the bundle from the supplied source.
- `uninstall`: deactivate and remove members from the selected bundle
  generation.
- `verify`: validate catalog metadata and runtime registry presence without
  mutating.

Default `confirm=false` returns the plan and refuses mutating operations that
would deactivate runtime objects, abort tick loops, remove endpoints, or delete
catalog payloads.

### `bbox_artifact_bundle_list`

List installed bundles and generations.

```text
bbox_artifact_bundle_list(include_members=true)
```

This is separate from `bbox_artifact_list` because operators need to reason
about generations and bundle ownership, not only individual artifacts.

### `bbox_artifact_remove`

Keep the single-artifact tool, but make removal adapter-backed for every managed
kind. For `cron` and `poller`, removal must abort the running handle. For
`webhook`, removal must remove the endpoint from the registry and delete the
persisted spec. MCP surface removal is packet removal for the
`mcp-surface/routing` domain or another packet marked `role="mcp_surface"`.

The current hard-remove sequence has the right safety shape:

1. dry-run paths first
2. require `confirm=true` for mutation
3. deactivate runtime before deleting catalog files

It needs to stop being global-only and kind-limited.

Runtime removal/status APIs needed before inlet artifacts can be safely managed:

```rust
impl CronRegistry {
    fn uninstall(&self, name: &str) -> Option<CronSpec>; // may evolve existing remove()
    fn status(&self, name: &str) -> Option<InletRuntimeStatus>; // handle exists/is_finished/in_flight
}

impl PollerRegistry {
    fn uninstall(&self, name: &str) -> Option<PollerSpec>; // abort handle, drop dedup ring
    fn status(&self, name: &str) -> Option<InletRuntimeStatus>; // handle exists/is_finished
}

impl WebhookRegistry {
    fn uninstall(&self, name: &str) -> Option<WebhookSpec>; // drop endpoint + delivery ring
}
```

`InletRuntimeStatus` is a small doctor/planner projection, not a new source of
truth. It should expose only what operators need to decide whether runtime state
matches catalog intent: whether a spec is registered, whether a tick-loop handle
exists and is finished, current in-flight count when applicable, and the
persisted runtime path/hash when available.

Each activator's `deactivate` must call the registry uninstall method and then
remove the persisted runtime spec for that inlet. Uninstall must also remove
per-name runtime side state such as poller dedup rings and webhook delivery
rings; otherwise repeated install/remove cycles retain dead per-inlet memory.
Reinstall may keep the current "new install aborts previous handle" behavior,
but uninstall must be explicit so bundle removal does not leave background loops
running after catalog files are deleted.

Do not add public `bro_cron_uninstall`, `bro_poller_uninstall`, or
`bro_webhook_uninstall` MCP tools. That would create more redundant surface area.
Add internal registry/runtime functions and expose removal through
`bbox_artifact_remove` only.

## Tool Surface Deletion

The desired end state removes kind-specific ops lifecycle tools from MCP:

| Remove from MCP | Replacement |
|---|---|
| `bro_cron_install` | `bbox_artifact_install(kind="cron", source=...)` |
| `bro_cron_list` | `bbox_artifact_list(kind="cron")` plus doctor inlet section |
| `bro_poller_install` | `bbox_artifact_install(kind="poller", source=...)` |
| `bro_poller_list` | `bbox_artifact_list(kind="poller")` plus doctor inlet section |
| `bro_webhook_install` | `bbox_artifact_install(kind="webhook", source=...)` |
| `bro_webhook_list` | `bbox_artifact_list(kind="webhook")` plus doctor inlet section |
| `bro_workflow_install` | `bbox_artifact_install(kind="workflow", source=...)` |
| `bro_workflow_list` | `bbox_artifact_list(kind="workflow")` |
| `reaction_install` | `bbox_artifact_install(kind="reaction", source=...)` |
| `reaction_list` | `bbox_artifact_list(kind="reaction")` plus doctor inlet section |
| `install-teams.sh` shell script | `bbox_artifact_install(kind="team"/"teamplate", source=...)` via the agentic-corpus bundle |
| `bro_cron_upcoming` | `bbox_cron_upcoming` |
| direct `bbox_compile` for shipped MCP surfaces | `bbox_artifact_install(kind="packet", role="mcp_surface", source=...)` |
| `include_str!` builtin macros | `bbox_artifact_install(kind="macro", source=...)` via the refactor bundle |

`reaction_install`/`reaction_list` are removed for the same reason as the other
inlet lifecycle tools. Keep `reaction_execute`, `reaction_replay`,
`reaction_retry`, and `reaction_deliveries` — they are diagnostics/actions, the
reaction analog of `bro_webhook_replay`/`bro_webhook_deliveries`.

`macro_register`/`macro_unregister` are **not** removed: they remain the
interactive surface for ad-hoc project-scope macros (they require `project_dir`
and write `.bbox/macros/`, the macro analog of editing a `.bbox/` file by hand).
Only the shipped builtin macros move to the artifact path. `macro_list`/`macro_describe`/`macro_plan`/`macro_apply`/`macro_run`/
`macro_validate` are execution/inspection tools and are untouched.

Keep non-lifecycle diagnostic/action tools:

- `bro_webhook_replay` remains a routing debugger.
- `bro_webhook_deliveries` remains a delivery log reader.

`bro_cron_upcoming` is not a lifecycle tool, but keeping it in the `bro_cron_*`
namespace after deleting the other cron lifecycle tools preserves the wrong
mental model. Rename it to `bbox_cron_upcoming` as an ops helper.

Removal does not mean deleting runtime modules. `src/crons.rs`,
`src/pollers.rs`, `src/webhooks.rs`, and workflow install helpers still own
validation, persistence, restoration, and runtime activation. The public MCP
surface should route lifecycle operations through artifact tools so there is one
place to reason about install/uninstall/provenance/upgrade behavior.

## Dependency Ordering

Bundle order may be explicit, but the installer should still validate obvious
dependency edges:

- cron/poller/webhook/reaction routing packets must exist before activation.
- workflow references to packets, atoms, brofiles, teams, subworkflows, and MCP
  hook targets must validate before activation.
- workflow **assets** (scripts/fixtures referenced by repo-relative path, e.g.
  `system-defaults/phase-decompose/scripts/epoch-check.py`) must resolve at plan
  time — see [Workflow Assets](#workflow-assets).
- workflow-backed atoms need their workflow active.
- profile-backed atoms need their brofile active.
- agents with `brofile_ref` need the brofile active.
- teamplate members reference brofiles; brofiles activate before teamplates.
- teamplate-backed teams need their teamplate (and its member brofiles) active.
- MCP surfaces compile as packets and should be available before provider sync
  steps that reference the surface.

Activation order for the system-default surface therefore settles to roughly:
packets/mcp-surface → brofiles → macros/agents/atoms → teamplates → teams →
workflows → inlets (cron/poller/webhook/reaction). The bundle planner enforces
the edges it can detect; explicit bundle order covers the rest until Phase 7
auto-ordering lands.

The first bundle implementation can use explicit bundle order plus validation.
A later pass can add static
dependency extraction to reorder automatically and report cycles.

Current direct inlet installers do not verify that `routing_packet` exists at
install time. Managed artifacts should be stricter: cron, poller, and webhook
activators must resolve their `routing_packet` before activation so a bad bundle
fails before any endpoint or tick loop is installed.

## System Defaults Bundle Layout

The v1 layout (a flat `refactor-atoms`, one `agentic-corpus-maintenance`,
`badgey`, `mcp-surfaces`) under-counts the real tree. The shipped surface splits
into cohesive directory groups, several of which v1 omitted entirely
(`phase-decompose/`, `supervision/`, a `maintenance/` tree separate from
`agentic-corpus/`). The bundle manifests map onto these directory groups:

```text
system-defaults/bundles/
  blackbox-system-defaults.json   # top-level meta-bundle → child bundles
  mcp-surfaces.json               # routing.json as packet role=mcp_surface
  agentic-corpus.json             # auto-digest/auto-edge/contradiction/eval/embed packets+workflows+brofiles+crons, contradiction-specialists team
  maintenance.json                # daily-compaction cron+packet+workflow (own tree, NOT agentic-corpus)
  agents.json                     # default agents + agent-eval cron/packets/workflows
  phase-decompose.json            # phase-decompose workflows + brofiles + packets + teamplates + script/fixture assets
  supervision.json                # supervision atoms + brofiles + workflows + packets
  refactor.json                   # refactor atoms (140) + brofiles + workflow wrappers + macros
  badgey.json                     # badgey agents + brofiles + workflows + packets + crons
```

Suggested ownership, grounded in the directory groups:

- `blackbox-system-defaults`: shallow meta-bundle that references the children.
- `mcp-surfaces`: `system-defaults/mcp-surfaces/routing.json` installed as a
  `packet` with `role="mcp_surface"`.
- `agentic-corpus`: `system-defaults/agentic-corpus/**` packets, workflows,
  brofiles, and crons, plus the promoted `contradiction-specialists` team
  (retiring `install-teams.sh`). **Excludes `nightly-eval-arc`** — it runs the
  repo-root `eval/run-agentic-eval.sh` against `${meta.project_dir}` and only
  works against this repository, so it is classified repo-internal and is not a
  portable bundle member (see Workflow Assets → Out-of-tree assets). The
  `source_glob` for this bundle's workflows must exclude `nightly-eval-arc.json`.
- `maintenance`: `system-defaults/maintenance/**` — daily-compaction cron, its
  cron-routing packet, and the arc workflow. This is a separate shipped tree and
  deserves its own bundle, not folding into agentic-corpus.
- `agents`: `system-defaults/agents/**` — default agents plus their co-located
  `crons/`, `packets/`, and eval `workflows/`.
- `phase-decompose`: `system-defaults/phase-decompose/**` teamplates and
  script/fixture assets, plus `system-defaults/workflows/phase-decompose/**`,
  `system-defaults/brofiles/phase-decompose/**`, and the phase-decompose packets
  under `agentic-corpus/packets/phase-decompose/`. This is the strongest case
  for asset-aware activation.
- `supervision`: `system-defaults/atoms/supervision/**`,
  `brofiles/supervision-*`, `workflows/supervision/**`, and supervision packets.
- `refactor`: the 140 `system-defaults/atoms/refactor/**` atoms, the refactor
  brofiles/personas, `workflows/refactor/**`, and the four macros.
- `badgey`: `system-defaults/badgey/**` agents, brofiles, workflows, packets,
  and crons.

Meta-bundles are shallow references to child bundles, never duplicated member
lists. The generation record expands transitive membership so uninstall stays
precise.

### Glob membership

Enumerating 140 refactor atoms (or 41 workflows) by hand in a manifest does not
scale and rots on every add. Bundle members must support directory-glob sources
so a manifest can declare a whole group:

```jsonc
{
  "kind": "atom",
  "source_glob": "system-defaults/atoms/refactor/*.json",
  "exclude": ["**/_*.json"]   // skip _base.outputs.schema.json templates
}
```

The planner expands a `source_glob` member into concrete members at plan time,
records each resolved source + hash in the generation, and applies the same
dependency/drift checks per file. Exclusions are required because some "json"
files are sidecars/templates, not artifacts: `.audit_examples.json` (packet
companion datasets), `_base.outputs.schema.json` and `_template.prompt.md`
(refactor atom templates). The planner must not treat those as installable
members; it carries `.audit_examples.json` alongside its packet for hashing/copy
only.

## Upgrade Helper

Add an ops tool:

```text
bbox_upgrade_check(apply=false, bundle="blackbox-system-defaults")
```

With `apply=false`, it reports the plan. With `apply=true`, it performs
mechanical safe actions and leaves risky actions as warnings.

Checks:

- binary version and build metadata if available
- artifact catalog schema/backfill status
- installed system-default bundle generation vs shipped bundle source hash
- changed shipped defaults by member content hash
- missing managed runtime objects
- unmanaged runtime objects under the workflow/inlet runtime stores, and packet
  domains that match shipped default names but lack catalog metadata
- index schema version and whether a full reindex is required
- embedding route configuration vs existing vector partition identity
- embedding queue health and route errors
- project registry presence and stale project paths
- EdgeIndex sidecar compaction pressure
- knowledge/render hygiene via `bbox_lint` summary
- inbox unresolved blocked/dispute/surprise count
- active/pending tasks that may conflict with upgrade operations

Apply-mode actions:

- run artifact metadata backfills
- run bundle reinstall when `confirm_system_defaults=true`
- trigger `bbox_reindex(full=true)` only when an explicit schema/chunker marker
  requires it
- trigger `bbox_reindex(full=false)` for ordinary freshness catch-up
- enqueue `bbox_reembed(route=...)` only for routes whose provider/model/dim
  changed or whose partition is missing
- compile shipped MCP surface packets if the managed surface bundle changed

Apply-mode should not:

- delete user-managed artifacts outside the selected bundle generation
- remove custom runtime specs with matching names unless the dry-run shows they
  were installed by the target generation
- blanket re-embed every route on every upgrade
- restart systemd services; that remains an operator shell action

Recovery model:

- Every mutating upgrade run writes an operation record before the first
  mutation with `status="applying"`.
- Each step appends its result as it completes.
- On success, the operation record becomes `status="applied"`.
- On daemon restart, doctor/upgrade-check report any stale `applying` operation
  as `blocked` with a resume/retry plan.

This is not full transaction rollback. It is an auditable, resumable operation
log that prevents a partial bundle reinstall or targeted re-embed from looking
complete.

Concurrency model:

- This is an ops-only surface; do not build a distributed lock subsystem for a
  hypothetical fleet of mutating agents.
- Mutating artifact/bundle/upgrade tools should use one daemon-local operation
  guard so accidental overlapping apply/reinstall/remove calls in the same
  process cannot interleave.
- Destructive apply steps should re-check the generation id and content hashes
  recorded by the plan immediately before mutation. If the runtime or catalog
  changed since planning, refuse with a drift finding and ask the operator to
  re-plan or choose `overwrite_runtime`, `adopt_runtime`, or `skip`.
- On daemon restart, stale `applying` operation records are reported by
  doctor/upgrade-check. They are recovery evidence, not lock files that block
  forever.

## Doctor

Add:

```text
bbox_doctor(scope="all", project="/optional/repo", format="summary|json")
```

Doctor is read-only. It aggregates existing health signals into one ranked
surface.

Sections:

- `daemon`: version, uptime if available, bind URL, store paths
- `index`: doc count, index size, schema version, last reindex signal if known
- `vectors`: per-route availability, queue depth, retry count, last error,
  partition delete ratio
- `graph`: entity populations from `bbox_describe_schema`, EdgeIndex rebuild
  status if available, sidecar compaction candidates
- `projects`: registered roots, missing paths, project-file index coverage
- `artifacts`: active catalog counts by kind, inactive/superseded counts,
  missing payloads, unmanaged runtime specs, bundle generation drift; shipped
  macros that are not yet catalog-managed (i.e. still relying on a stale
  compiled-in builtin); teamplates/teams present in the store but uncataloged
  (e.g. left over from `install-teams.sh`)
- `inlets`: installed webhooks/pollers/crons/reactions, routing packet
  existence, running tick loops for poller/cron specs
- `workflows`: installed workflow count, missing referenced packets/atoms/
  brofiles where statically detectable, and **missing workflow assets**
  (referenced script/fixture paths that do not resolve)
- `memories`: system-memory catalog loaded and `defaults_memories_dir` resolved
  (auto-loaded surface, not catalog-managed)
- `knowledge`: `bbox_lint` severity summary and rendered-file freshness if
  detectable
- `attention`: unresolved inbox counts by kind

Doctor output must classify findings:

- `ok`: no operator action
- `info`: notable but not actionable
- `warn`: should inspect
- `action`: suggested command exists
- `blocked`: required prerequisite missing

Example summary:

```text
status: warn

action:
- system-defaults bundle drift: 7 shipped members changed since generation gen-...
  next: bbox_artifact_plan(operation="reinstall", bundle="blackbox-system-defaults")
- route knowledge has last_error=...
  next: fix provider env, then bbox_reembed(route="knowledge")

warn:
- cron embed-compaction-nightly exists in runtime but is not catalog-managed
- project /repo/old missing on disk
```

## Data Model Additions

Extend `ArtifactKind`:

```rust
pub enum ArtifactKind {
    Workflow,
    Packet,
    Brofile,
    Agent,
    Atom,
    Macro,      // identity = `id`; replaces include_str! builtins
    Teamplate,  // team template (members by brofile)
    Team,       // instantiated roster; gains a real activator
    Cron,
    Poller,
    Webhook,
    Reaction,   // event-reaction inlet
    Bundle,
}
```

Extend metadata:

```rust
pub struct ArtifactMetadata {
    ...
    pub bundle: Option<String>,
    pub bundle_generation: Option<String>,
    pub managed_by: Option<String>, // e.g. "bundle:blackbox-system-defaults"
    pub runtime_ref: Option<String>, // e.g. "cron:embed-compaction-nightly"
    pub role: Option<String>, // e.g. "mcp_surface"
}
```

Add bundle generation store:

```text
artifacts/bundles/<bundle-name>.json
artifacts/bundles/<bundle-name>/metadata.json
artifacts/bundles/<bundle-name>/.generations/<generation>.json
```

Do not overload per-member `.versions/` with generation state. Versions answer
"what payload versions existed?" Generations answer "what did this bundle apply
as an operation?"

## Implementation Plan

The phased implementation details live in
[Ops Artifact Bundles And Doctor - Implementation Plan](ops-artifact-bundles-and-doctor-impl.md).

High-level order:

1. establish daemon runtime/inlet paths and teardown/status internals
2. expand artifact kinds, metadata, scoped listing, and activators
3. add bundle plan/apply/generation records and watcher `auto_apply`
4. delete redundant public lifecycle tools and rename cron preview to
   `bbox_cron_upcoming`
5. add doctor
6. add upgrade helper
7. harden dependency extraction and auto-ordering

## Non-Goals

- No automatic install of all shipped defaults on daemon startup.
- No deletion of user artifacts just because they live under
  `system-defaults/` paths or share a name.
- No systemd restart orchestration inside MCP tools.
- No indefinite compatibility layer for redundant ops lifecycle tools.
- No new public kind-specific uninstall tools.
- No inbox or note spam for successful user-directed installs/upgrades. The
  user asked for the operation; the result belongs in the tool response and
  operation log, not in the attention queue.

## Decisions

Bundle generation records live under the artifact catalog. Doctor and
`bbox_upgrade_check` can read those records and surface failed/stale operations
on demand. They must not emit inbox items for normal install history.

Project-scoped bundles belong under `.bbox/bundles/` so they follow git with
the rest of a repo's local machinery. Auto-apply is controlled by the bundle
manifest, not by the watcher globally:

```jsonc
{
  "name": "project-defaults",
  "version": 1,
  "scope": "project",
  "auto_apply": false,
  "members": []
}
```

Watcher behavior:

- `auto_apply=false` or absent: catalog/validate the bundle manifest only; do
  not mutate runtime state.
- `auto_apply=true`: run the same plan/apply path as
  `bbox_artifact_bundle_apply`, with the same dependency checks and operation
  log.

Runtime drift means: a managed artifact's active runtime spec differs from the
cataloged payload that last activated it. Example: a cron was installed from a
bundle, then someone manually edited the active cron spec in the daemon runtime
store. On reinstall, Blackbox should not guess. The plan must classify it as
drift and require an explicit operator choice:

- `overwrite_runtime`: replace the runtime copy with the bundle member.
- `adopt_runtime`: install the current runtime copy into the catalog as the new
  managed payload.
- `skip`: leave it alone and keep reporting drift.

### Regrounding Decisions (2026-05-30)

- **Macros are managed artifacts, not `include_str!` builtins.** Editing a macro
  must not require a daemon rebuild; macros join the version/source/generation/
  drift lifecycle like every other default. The four shipped macros become
  refactor-bundle members. The `include_str!` `builtin_definitions()` block is
  removed **only after** a `MacroScope::Managed` scope + loader exists and the
  refactor bundle has been applied and verified — never mid-migration, or the
  macros vanish (see Macro Management for the ordered sequence). Migration
  prefers documented `bbox_artifact_bundle_apply` (surfaced by
  `bbox_upgrade_check`) over a silent startup auto-install, to honor the
  no-startup-auto-install non-goal.
- **System memories stay auto-loaded and out of the bundle.** They already load
  from `defaults_memories_dir` at init with no rebuild cost and are markdown, not
  JSON specs. Doctor verifies the catalog loaded; it does not install them. The
  distinguishing test for "out of bundle" is: dedicated runtime loader AND no
  rebuild to edit. Memories pass; compiled-in macros failed and therefore move.
- **Team activation is real; `install-teams.sh` is retired.** `teamplate` and
  `team` are separate kinds with activators that call `save_teamplate`/
  `save_team`. `contradiction-specialists` is promoted to a shipped JSON owned by
  the agentic-corpus bundle.
- **Reactions are added as an inlet kind now** for surface consistency, with
  `reaction_install`/`reaction_list` removed in favor of the artifact path and
  `reaction_execute`/`replay`/`retry`/`deliveries` kept as diagnostics, even
  though no reaction defaults ship yet. Identities remain read-only and excluded.
- **Workflow assets are first-class.** The real fix is an `${asset_root}`
  executor interpolation that resolves asset paths independent of the run's
  `project_dir` (shell `cwd` resolves to the run project/worktree, so
  repo-relative argv breaks outside this repo); plan-time existence validation is
  only a backstop. Shipped workflows that reference assets migrate to
  `${asset_root}`. Copy/staging to absolute paths is deferred until a
  no-source-tree deployment requires it. Repo-internal workflows whose assets
  live outside `system-defaults/` (e.g. `nightly-eval-arc`) are excluded from the
  portable bundle.
- **`.audit_examples.json` are packet companion datasets, not artifacts.** They
  travel with their packet member for hashing/copy and feed `bbox_audit`; they
  are never installed as standalone units. Glob membership must exclude them
  along with `_*.json` / `_*.md` refactor templates.
- **Bundle membership supports `source_glob`.** Hand-enumerating 140 atoms is a
  rot hazard; manifests declare directory groups and the planner expands them at
  plan time.
