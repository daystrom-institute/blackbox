---
title: "Ops Artifact Bundles And Doctor - Implementation Plan"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - operations
  - config-artifacts
---

# Ops Artifact Bundles And Doctor - Implementation Plan

Date: 2026-05-14
Status: implementation proposal
Companion to: [Ops Artifact Bundles And Doctor](ops-artifact-bundles-and-doctor.md)

This plan extracts the implementation work from the design proposal and orders
it into testable cuts. The priority is to delete redundant lifecycle surfaces by
first making the artifact path capable of doing the whole job: validate, plan,
activate, deactivate, record provenance, and report drift.

```text
Phase 0 -> Phase 1 -> Phase 2 -> Phase 3 -> Phase 4 -> Phase 5 -> Phase 6 -> Phase 7
baseline   paths       artifacts   bundles     surfaces    doctor      upgrade    deps
           teardown    activators  watcher     deletion    health      helper     harden
```

Every mutating Rust phase should finish with:

```text
rtk cargo fmt
rtk cargo test --bin blackboxd
```

Use narrower tests while developing, but do not call a phase complete until the
binary test suite passes or every pre-existing failure is documented.

## Implementation Invariants

- New runtime writes go to daemon-owned paths, not the old `bro` runtime dirs:
  `runtime/workflows/` and `inlets/{crons,pollers,webhooks}/`.
- Old `store_dir/{workflows,crons,pollers,webhooks}` files may be read only to
  adopt or relocate them. This is a replacement path, not an indefinite dual
  store.
- Artifact tools own install/remove lifecycle. Runtime modules keep validation,
  persistence, restoration, and loop/endpoint activation internals.
- Successful user-directed installs/upgrades write tool responses and operation
  records, not inbox or note noise.
- Normal `bbox_artifact_install` rejects missing versions. Missing-version
  synthesis happens only in explicit adoption/backfill paths.
- Bundle apply validates the full plan before activating any member.
- Bundle generations are the uninstall boundary. Never infer removal from a
  shared source prefix such as `system-defaults/`.
- The surface is ops-only. Use a daemon-local mutation guard plus last-moment
  generation/content-hash rechecks; do not introduce a distributed lock system
  for hypothetical parallel artifact mutators.

## Phase 0: Baseline And Anchors

**Goal:** pin the current behavior before extraction starts.

### 0.1 Source Anchors

Record the current code anchors before editing:

- `src/artifacts.rs`
  - `ArtifactKind`
  - `ArtifactMetadata`
  - `ArtifactCatalog::install_value[_scoped]`
  - `ArtifactCatalog::list`
  - `artifact_kind_from_dir_pub`
- `src/server/routes.rs`
  - `install_artifact_value`
  - `deactivate_artifact`
  - workflow, poller, cron, webhook install route helpers
- `src/tools/artifacts.rs`
  - `bbox_artifact_install/list/supersede/remove`
- `src/tools/orchestrate.rs`
  - `bro_workflow_install/list`
  - `bro_webhook_install/list`
  - `bro_poller_install/list`
  - `bro_cron_install/list/upcoming`
- `src/main.rs`
  - startup restore for webhooks, pollers, crons, workflows
- `src/crons.rs`, `src/pollers.rs`, `src/webhooks.rs`
  - registry install/list/handle state
- `src/watcher.rs`
  - `.bbox/` path-to-kind handling and scoped install behavior
- `src/tool_docs.rs`
  - registered tool documentation coverage

### 0.2 Baseline Tests

Run:

```text
rtk cargo test --bin blackboxd
```

Record any pre-existing failures. New failures after Phase 1 are owned by this
work.

**Deliverable:** no product behavior change.

## Phase 1: Runtime Paths, Restore, Teardown, And Status

**Goal:** give daemon runtime objects neutral storage paths and safe removal
internals before the artifact catalog takes ownership.

### 1.1 Runtime Path Helpers

Add central helpers instead of scattering `state.store_dir.join(...)`:

```rust
workflow_runtime_dir(state) -> PathBuf // .../runtime/workflows
cron_inlet_dir(state) -> PathBuf      // .../inlets/crons
poller_inlet_dir(state) -> PathBuf    // .../inlets/pollers
webhook_inlet_dir(state) -> PathBuf   // .../inlets/webhooks
```

Reasonable locations:

- `src/runtime_paths.rs`, or
- methods on `SharedState` if that type already owns path helpers.

Do not repurpose `store_dir`; it still backs many orchestration stores. This
work only moves daemon runtime objects.

Update all current write sites:

- `src/tools/orchestrate.rs`
  - webhook install
  - poller install
  - cron install
  - workflow install
- `src/server/routes.rs`
  - workflow artifact activation/deactivation
  - `admin_poller_install`
  - `admin_cron_install`
  - `admin_webhook_install`
  - project-rename persistence for pollers/crons, including the re-spawn after
    rewritten specs are persisted
- future artifact activators added in Phase 2.

### 1.2 Startup Restore And Old-Path Relocation

Update `src/main.rs` startup restore:

- read new paths first;
- read old `store_dir/{webhooks,pollers,crons,workflows}` only for adoption or
  relocation;
- when relocation succeeds, write the same spec to the new path and delete the
  old file;
- if relocation fails, leave the old file and report a warning.

Duplicate name policy:

- if the new path already has a spec for the same name, do not overwrite it from
  the old path;
- doctor should report the old-path duplicate as stale cleanup.

### 1.3 Registry Uninstall And Status

Add internal APIs:

```rust
impl CronRegistry {
    fn uninstall(&self, name: &str) -> Option<CronSpec>; // can wrap/evolve remove()
    fn status(&self, name: &str) -> Option<InletRuntimeStatus>;
}

impl PollerRegistry {
    fn uninstall(&self, name: &str) -> Option<PollerSpec>;
    fn status(&self, name: &str) -> Option<InletRuntimeStatus>;
}

impl WebhookRegistry {
    fn uninstall(&self, name: &str) -> Option<WebhookSpec>;
    fn status(&self, name: &str) -> Option<InletRuntimeStatus>;
}
```

`InletRuntimeStatus` should be a read-only projection for doctor/planner output:
registered spec present, handle present, handle finished, in-flight count when
applicable, persisted runtime path, and persisted runtime content hash when
available. It is not a second registry.

Uninstall rules:

- cron: abort handle and drop run/concurrency state for that name; the existing
  `remove` method already does the core teardown, but status/return-value shape
  should be normalized for activators and doctor;
- poller: abort handle and drop the per-name dedup ring;
- webhook: drop endpoint registration and per-name delivery ring.

No public uninstall MCP tools are added in this phase.

### 1.4 Tests

Add focused tests for:

- path helpers produce the settled `runtime/` and `inlets/` paths;
- a test helper can build `SharedState` with distinct artifact, runtime, and
  inlet directories matching production layout; do not rely only on a unified
  temp store that hides path-divergence bugs;
- startup restore prefers new-path specs over old duplicates;
- old-path relocation writes new path and removes old path on success;
- project rename re-persists poller/cron specs to the new inlet paths and
  re-spawns tick loops;
- cron/poller uninstall aborts or removes tracked handles;
- poller/webhook uninstall drops dedup/delivery side state.

**Acceptance gate:** daemon startup restores runtime objects from the new paths,
old objects are adopted/relocated once, and registry removal can actually stop
runtime behavior.

## Phase 2: Artifact Core Expansion And Activators

**Goal:** make artifact catalog semantics broad enough for inlets, bundles, and
surface roles while preserving behavior for existing kinds.

### 2.1 Artifact Kinds And Metadata

Update `src/artifacts.rs`:

- keep the existing `Cron` variant and add `Poller`, `Webhook`, and `Bundle` to
  `ArtifactKind`;
- update string parsing/rendering for new kinds;
- update/verify `artifact_kind_from_dir_pub` for:
  - `crons`
  - `pollers`
  - `webhooks`
  - `bundles`
- update/verify `artifact_name()` so `Cron`, `Poller`, `Webhook`, and `Bundle`
  all read `value["name"]`;
- extend `ArtifactInstallParams` with optional `role` so MCP-surface packets can
  stay `kind="packet"` while still carrying role metadata into the catalog;
- extend `ArtifactMetadata`:

```rust
pub bundle: Option<String>,
pub bundle_generation: Option<String>,
pub managed_by: Option<String>,
pub runtime_ref: Option<String>,
pub role: Option<String>,
```

Use serde defaults so existing metadata files continue to load.

### 2.2 Scoped Listing

Fix `ArtifactCatalog::list` so project-scoped artifacts are visible. Add a tool
parameter in `src/tools/artifacts.rs`:

```text
bbox_artifact_list(include_scoped=true)
```

Default can stay current/global-only if that is less disruptive, but doctor and
bundle planning must be able to request scoped artifacts explicitly.

Add scoped removal too. `ArtifactCatalog::remove_hard` is global-only today; add
`remove_hard_scoped` or equivalent and update `bbox_artifact_remove` so
project-scoped artifacts can be removed deliberately instead of becoming
uninstallable catalog entries.

### 2.3 Activator Extraction

Extract the behavior currently embedded in `install_artifact_value` into an
internal activator dispatch:

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

Helper structs:

- `PreparedArtifact`: parsed artifact payload plus resolved dependency refs;
  validation produces it without touching runtime registries or files.
- `ActivationResult`: runtime ref, activated content hash, and warnings for
  metadata/generation records.
- `DeactivationResult`: booleans/details for registry removal, persisted runtime
  file removal, and per-name side-state cleanup.

Extract current kinds first:

- workflow
- packet
- brofile
- agent
- atom
- team

Then add inlet activators:

- cron: schedule validation, routing packet validation, persist, spawn loop;
- poller: spec validation, routing packet validation, persist, spawn loop;
- webhook: signature policy validation, routing packet validation, persist
  endpoint.

Do not call `bro_*_install` tools internally. Activators call runtime helpers
directly.

Update deactivation dispatch as part of this extraction. Today
`deactivate_artifact` is a free `match` over the original artifact kinds, and
`bbox_artifact_remove` / `bbox_artifact_supersede` call that path. Either
replace `deactivate_artifact` with activator dispatch or add the new
`Cron`/`Poller`/`Webhook` arms there while extraction is in progress. Removal
and supersession must both reach the same deactivation adapter.

### 2.4 Version Adoption Path

Keep normal catalog install strict: no version means install fails.

Add an explicit adoption/backfill helper that:

- reads an active runtime spec;
- if `version` is absent, adds `version="unmanaged"` to the catalog payload;
- records an install warning that the version was inferred from existing
  runtime state;
- does not pretend the spec came from `system-defaults/`.

This helper is for doctor/upgrade-check adoption, not ordinary install.

### 2.5 Tests

Tests:

- new artifact kinds parse and round-trip;
- old metadata files load with new optional fields absent;
- `role="mcp_surface"` persists in metadata;
- scoped artifact list includes `.bbox/` artifacts when requested;
- existing six kind installs still behave the same through activators;
- direct cron/poller/webhook install rejects missing `version`;
- adoption path records `version="unmanaged"` and an install warning.

**Acceptance gate:** all existing artifact installs pass through activators, and
cron/poller/webhook artifacts can be installed and removed without public
kind-specific lifecycle tools.

## Phase 3: Bundle Plan, Apply, Generations, And Watcher

**Goal:** add bundle lifecycle without making single-artifact install recurse.

### 3.1 Bundle Manifest Type

Add typed bundle structs, likely near artifact code or in `src/artifact_bundle.rs`:

```rust
BundleManifest {
    name,
    version,
    scope,
    auto_apply,
    members,
}

BundleMember {
    kind,
    source,
    name?,
    version?,
    role?,
}
```

`bbox_artifact_install(kind="bundle", source=...)` records the manifest only.
It does not apply members.

`bbox_artifact_remove(kind="bundle", name=...)` removes the bundle manifest and
metadata only. It does not deactivate member artifacts; use
`bbox_artifact_bundle_apply(operation="uninstall", ...)` for that. Generation
records are preserved for audit unless a future explicit purge tool is added.

### 3.2 Planning

Add `bbox_artifact_plan`:

```text
bbox_artifact_plan(operation, source?, bundle?, include_unmanaged?)
```

The planner returns:

- dependency-ordered member operations;
- same-hash no-ops;
- members that will be deactivated/replaced;
- unmanaged runtime objects;
- drifted runtime objects;
- destructive actions that require confirmation;
- warnings for missing dependencies and unknown kinds.

Plan validation must complete before mutation.

Initial dependency checks:

- inlets resolve `routing_packet`;
- workflows resolve statically detectable packets, atoms, brofiles, teams,
  subworkflows, and MCP hook targets;
- workflow-backed atoms resolve workflow;
- profile-backed atoms resolve brofile;
- agents with `brofile_ref` resolve brofile;
- MCP surface packets exist before any provider sync step that references them.

### 3.3 Apply And Generation Records

Add `bbox_artifact_bundle_apply`:

```text
bbox_artifact_bundle_apply(operation, source?, bundle?, generation?, confirm=false)
```

Operations:

- `install`: install missing or changed members;
- `reinstall`: uninstall the previous generation, then install the supplied
  bundle;
- `uninstall`: deactivate and remove members from the selected generation;
- `verify`: validate catalog and runtime presence without mutation.

Apply guard:

- wrap mutating artifact/bundle/upgrade operations in one daemon-local mutex or
  equivalent operation guard;
- plan output records expected active generation ids and content hashes;
- immediately before any destructive action, re-check those expected values;
- if reality changed, stop with a drift result instead of trying to merge
  operator intent automatically.

This is not a general distributed lock. It only prevents accidental in-process
overlap and stale-plan mutation on an ops surface.

Persist generation records:

```text
artifacts/bundles/<bundle-name>.json
artifacts/bundles/<bundle-name>/metadata.json
artifacts/bundles/<bundle-name>/.generations/<generation>.json
```

Retention rule:

- keep active generation plus the previous generation for recovery;
- prune older generation records unless explicitly pinned.

### 3.4 Runtime Drift Decisions

If a managed artifact's active runtime spec differs from the catalog payload
that last activated it, plan must require one explicit choice:

- `overwrite_runtime`
- `adopt_runtime`
- `skip`

Do not silently pick one during reinstall.

### 3.5 Project Watcher

Update `src/watcher.rs`:

- recognize `.bbox/bundles/`;
- refactor watcher handling to accept `SharedState` or a narrower plan/apply
  context. The current event path has roots plus `ArtifactCatalog`, which is not
  enough to validate packets, touch registries, or activate bundle members;
- `auto_apply=false` or absent: catalog and validate the bundle manifest only;
- `auto_apply=true`: call the same plan/apply path as
  `bbox_artifact_bundle_apply`;
- write operation records for auto-apply, but do not emit inbox/noise for
  successful user-directed or manifest-directed installs.

### 3.6 System Defaults Manifests

Add:

```text
system-defaults/bundles/
  blackbox-system-defaults.json
  badgey.json
  agentic-corpus-maintenance.json
  refactor-atoms.json
  mcp-surfaces.json
```

Use shallow meta-bundle references. The generation record expands transitive
membership so uninstall remains precise.

Verify shipped cron specs keep top-level `version` fields. Require the same for
future shipped poller/webhook defaults.

### 3.7 Tests

Tests:

- installing a bundle manifest does not apply members;
- bundle apply validates all members before activating any member;
- reinstall removes only members from the selected previous generation;
- uninstall does not remove custom artifacts that share names or
  `system-defaults/` source prefixes unless owned by that generation;
- drift requires an explicit decision;
- watcher catalogs `auto_apply=false`;
- watcher applies `auto_apply=true` through the shared apply path.

**Acceptance gate:** `blackbox-system-defaults` can be planned, installed,
reinstalled, verified, and uninstalled through artifact tools with precise
generation boundaries.

## Phase 4: Tool Surface Deletion And Naming Cleanup

**Goal:** delete redundant lifecycle MCP surfaces once artifact tools own the
behavior.

### 4.1 Remove Lifecycle Tools

Remove from MCP registration and `src/tool_docs.rs`:

- `bro_cron_install`
- `bro_cron_list`
- `bro_poller_install`
- `bro_poller_list`
- `bro_webhook_install`
- `bro_webhook_list`
- `bro_workflow_install`
- `bro_workflow_list`
- `bro_cron_upcoming`

Add:

- `bbox_cron_upcoming`

Keep:

- `bro_webhook_replay`
- `bro_webhook_deliveries`

These are diagnostic/action tools, not install/list lifecycle.

### 4.2 Docs And Examples

Update:

- `docs/artifact-catalog.md`
- `docs/operating-blackbox.md`
- `docs/operations.md`
- `system-defaults/README.md`
- any examples that mention direct `bro_cron_install` or `bbox_compile` for
  shipped MCP surfaces.

Default instructions should use:

```text
bbox_artifact_plan(...)
bbox_artifact_bundle_apply(...)
bbox_artifact_install(kind="...", source="...")
bbox_artifact_remove(...)
```

### 4.3 Tool Docs Coverage

Update `src/tool_docs.rs` for:

- `bbox_artifact_plan`
- `bbox_artifact_bundle_apply`
- `bbox_artifact_bundle_list`
- new `bbox_artifact_install` kind examples
- `bbox_cron_upcoming`
- later doctor/upgrade tools as they land

Run the tool-doc coverage test after registration changes.

**Acceptance gate:** operators have one lifecycle surface, and the tool catalog
does not expose stale kind-specific install/list commands.

## Phase 5: Doctor

**Goal:** provide one read-only "what do I need to know right now?" surface.

### 5.1 Tool Shape

Add:

```text
bbox_doctor(scope="all", project?, format="summary|json")
```

Doctor is read-only. It may inspect operation records, bundle generations,
runtime directories, registries, index/vector status, project registry, lint
summary, and inbox counts. It must not enqueue notes or inbox items.

### 5.2 Sections

Implement sections incrementally:

- `daemon`: version, bind URL, store paths, runtime/inlet paths;
- `index`: document count, index size, schema marker if available;
- `vectors`: per-route availability, queue depth, retry count, last error,
  partition delete ratio;
- `graph`: entity counts, EdgeIndex rebuild/compaction status if available;
- `projects`: registered roots, missing paths, project-file coverage;
- `artifacts`: active counts by kind, missing payloads, unmanaged runtime
  specs, stale old-path files, bundle generation drift;
- `inlets`: installed webhooks/pollers/crons, routing packet existence,
  poller/cron loop status;
- `workflows`: installed workflows and statically detectable missing refs;
- `knowledge`: `bbox_lint` severity summary and rendered-file freshness when
  available;
- `attention`: unresolved inbox counts by kind.

### 5.3 Finding Classification

Use:

- `ok`
- `info`
- `warn`
- `action`
- `blocked`

Findings should include suggested next commands when a command exists. Do not
make normal install history attention-worthy.

### 5.4 Tests

Tests:

- operation-record tests in this phase should write synthetic operation records
  from the test harness. The upgrade helper in Phase 6 is the production writer;
- stale `applying` operation is reported as `blocked`;
- unmanaged runtime spec is reported but not mutated;
- old-path duplicate is reported as cleanup, not auto-overwritten;
- missing routing packet for an inlet is reported;
- summary output remains compact and JSON output is stable enough for callers.

**Acceptance gate:** doctor can replace the scattered manual smoke checklist
for artifact/inlet/runtime health.

## Phase 6: Upgrade Helper

**Goal:** mechanize safe post-upgrade checks and gated repairs.

### 6.1 Read-Only Check

Add:

```text
bbox_upgrade_check(apply=false, bundle="blackbox-system-defaults")
```

Checks:

- binary version/build metadata if available;
- artifact catalog schema/backfill status;
- installed system-default bundle generation vs shipped bundle source hash;
- changed shipped defaults by member content hash;
- missing managed runtime objects;
- unmanaged runtime objects under runtime/inlet stores;
- shipped default packet domains without catalog metadata;
- index schema/chunker marker and whether full reindex is required;
- embedding route identity vs existing vector partition identity;
- embedding queue health and route errors;
- project registry stale paths;
- EdgeIndex sidecar compaction pressure;
- knowledge/render lint summary;
- active/pending tasks that may conflict with upgrade operations.

### 6.2 Apply Mode

With `apply=true`, allow only conservative actions:

- artifact metadata backfills;
- bundle reinstall when explicitly confirmed;
- full reindex only when a schema/chunker marker requires it;
- incremental reindex for ordinary freshness;
- targeted re-embed only when provider/model/dim changed or partition is
  missing;
- compile managed MCP surface packets when the surface bundle changed.

Never:

- delete user-managed artifacts outside the selected bundle generation;
- remove custom runtime specs with matching names unless the plan proves
  generation ownership;
- blanket re-embed every route on every upgrade;
- restart systemd services.

### 6.3 Operation Log And Recovery

Every mutating run writes an operation record before the first mutation:

```json
{
  "operation": "upgrade-check",
  "status": "applying",
  "started_at": "...",
  "steps": []
}
```

Each step appends its result. Success marks the record `applied`. On restart,
doctor and upgrade-check report stale `applying` records as `blocked` with a
resume or retry plan.

This is not rollback. It is auditable, resumable operation tracking.

### 6.4 Tests

Tests:

- read-only mode never mutates files or registries;
- apply mode refuses bundle reinstall without confirmation;
- targeted re-embed selects only changed/missing routes;
- stale applying operation is visible to both doctor and upgrade-check;
- upgrade helper does not emit inbox or note entries for normal operation
  history.

**Acceptance gate:** after an upgrade, an operator can run one tool to see the
required actions and optionally apply the safe ones without losing custom
runtime state.

## Phase 7: Dependency Extraction Hardening

**Goal:** improve ordering and preflight quality after the basic bundle system
ships.

Work:

- extract static dependency refs from workflows;
- extract refs from atoms and agents;
- extract `routing_packet` refs from inlets;
- extract MCP surface/provider-sync refs;
- auto-order bundle operations;
- report cycles before mutation.

This is deliberately later. Phase 3 can ship with explicit bundle order plus
strict validation.

**Acceptance gate:** bundle authors can omit most manual ordering and still get
deterministic preflight failures for cycles or missing dependencies.
