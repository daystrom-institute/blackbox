# Ops Artifact Bundles And Doctor

Date: 2026-05-13
Status: design proposal v1

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

That list is narrower than the shipped default surface. `system-defaults/` also
contains crons and MCP surface routing, and the runtime already has install/list
tools for crons, pollers, and webhooks. Those objects are operational artifacts:
they are versioned JSON specs, daemon-owned, installed into runtime registries,
and need upgrade/uninstall semantics. Their current invisibility is historical,
not principled.

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
  Atom, Team}`.
- `ArtifactCatalog::install_value[_scoped]` stores active JSON payloads,
  `metadata.json`, `.versions/v<version>.json`, and
  `.versions/v<version>.metadata.json`.
- Active installs are content-hash idempotent via canonical JSON SHA-256.
- `ArtifactMetadata` records source, version, install time, active flag,
  content hash, optional project id, supersession fields, and install warnings.
- `bbox_artifact_install`, `bbox_artifact_list`, `bbox_artifact_supersede`, and
  `bbox_artifact_remove` are the public MCP tools in `src/tools/artifacts.rs`.
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
- `deactivate_artifact` removes workflow files, packet domains, and brofiles;
  agents, atoms, and teams currently have no separate runtime registry to tear
  down.

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
- Those tools expose list operations, but there is no catalog metadata,
  supersession chain, bundle membership, hard uninstall, or source tracking for
  these specs.
- `CronRegistry` and `PollerRegistry` keep `JoinHandle`s and abort the previous
  handle on reinstall, but they do not currently expose an uninstall/remove API.
  `WebhookRegistry` has install/get/list and no remove API. Managed removal
  therefore requires new registry methods, not just wiring existing functions
  into `deactivate_artifact`.
- Daemon startup restores webhooks, pollers, and crons directly from that
  runtime store and respawns poller/cron loops. That restore path bypasses the
  artifact catalog today, so doctor must treat these runtime files as
  potentially unmanaged until the artifact path owns them.

System defaults:

- `system-defaults/README.md` says the daemon does not auto-install the tree.
- `docs/artifact-catalog.md` calls `system-defaults/` the shipped catalog
  source, but explicitly treats crons/webhooks as install-order dependencies
  rather than catalog kinds.
- `system-defaults/badgey/crons/*.json`,
  `system-defaults/agentic-corpus/crons/*.json`, and
  `system-defaults/agents/crons/*.json` are therefore shipped defaults that
  must currently be installed directly through `bro_cron_install`.
- `system-defaults/mcp-surfaces/routing.json` is also shipped default
  machinery, but it is installed through `bbox_compile`, not the artifact
  catalog.
- Current cron, poller, and webhook runtime structs do not define `version` or
  `supersedes` fields. Runtime serde will ignore extra top-level fields in JSON
  specs, but the artifact catalog requires a version to install. Managed inlet
  sources therefore need either a top-level `version` field in shipped JSON, or
  an install-time version override/synthesis rule.

Operations baseline:

- `docs/operating-blackbox.md` defines the manual health smoke:
  `bbox_stats`, `bbox_embed_status`, `bbox_project_list`,
  `bbox_describe_schema`, and `bbox_hybrid_search`.
- `docs/operations.md` defines protected vs rebuildable stores and the
  post-upgrade/manual maintenance checklist.
- Startup already runs one schema-like maintenance action:
  `ArtifactCatalog::backfill_content_hashes`.

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
| `team` | team artifact catalog | `name` | catalog only until team activation is formalized |
| `cron` | cron registry | `name` | validate schedule, persist, spawn loop |
| `poller` | poller registry | `name` | validate fetch/selector shape, persist, spawn loop |
| `webhook` | webhook registry | `name` | validate signature policy, persist endpoint |

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

## Proposed MCP Surface

### `bbox_artifact_install`

Extend `kind` to include:

- `cron`
- `poller`
- `webhook`
- `bundle`

Keep the existing artifact install shape: `source`, optional `name`, optional
`version`, and optional `supersedes`. New kinds use that same shape so operators
learn one lifecycle command instead of one command family per runtime object.

Version rule for new kinds:

- Managed cron/poller/webhook sources under `system-defaults/` should gain a
  top-level `version`. The runtime structs will ignore it, while
  `ArtifactCatalog` can record it.
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

This keeps runtime-specific behavior in one place per kind and lets bundles run
the same path a single install uses.

The implementation is not a trivial wrapper. `install_artifact_value` currently
conflates validation, runtime activation, catalog write, agent embedding, and
provenance edge persistence. The first implementation should extract one
activator at a time while preserving exact existing behavior for the six current
kinds before adding inlet kinds.

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

New runtime removal APIs needed before inlet artifacts can be safely managed:

```rust
impl CronRegistry {
    fn uninstall(&self, name: &str) -> Option<CronSpec>; // abort handle, drop run state
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
| `bro_cron_upcoming` | `bbox_cron_upcoming` |
| direct `bbox_compile` for shipped MCP surfaces | `bbox_artifact_install(kind="packet", role="mcp_surface", source=...)` |

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

- cron/poller/webhook routing packets must exist before activation.
- workflow references to packets, atoms, brofiles, teams, subworkflows, and MCP
  hook targets must validate before activation.
- workflow-backed atoms need their workflow active.
- profile-backed atoms need their brofile active.
- agents with `brofile_ref` need the brofile active.
- MCP surfaces compile as packets and should be available before provider sync
  steps that reference the surface.

The first bundle implementation can use explicit bundle order plus validation.
A later pass can add static
dependency extraction to reorder automatically and report cycles.

Current direct inlet installers do not verify that `routing_packet` exists at
install time. Managed artifacts should be stricter: cron, poller, and webhook
activators must resolve their `routing_packet` before activation so a bad bundle
fails before any endpoint or tick loop is installed.

## System Defaults Bundle Layout

Add:

```text
system-defaults/bundles/
  blackbox-system-defaults.json
  badgey.json
  agentic-corpus-maintenance.json
  refactor-atoms.json
  mcp-surfaces.json
```

Suggested ownership:

- `blackbox-system-defaults`: top-level meta-bundle. Depends on the others.
- `mcp-surfaces`: default MCP surface routing.
- `agentic-corpus-maintenance`: schema migration, project bootstrap,
  auto-digest, auto-edge, eval, embed compaction workflows/packets/brofiles and
  related crons.
- `badgey`: Badgey agents, brofiles, workflows, packets, and crons.
- `refactor-atoms`: refactor brofiles, atoms, and workflow wrappers.

Meta-bundles should be shallow references to child bundles rather than giant
duplicated member lists. The generation record expands transitive membership so
uninstall remains precise.

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
  missing payloads, unmanaged runtime specs, bundle generation drift
- `inlets`: installed webhooks/pollers/crons, routing packet existence,
  running tick loops for poller/cron specs
- `workflows`: installed workflow count, missing referenced packets/atoms/
  brofiles where statically detectable
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
    Team,
    Cron,
    Poller,
    Webhook,
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
