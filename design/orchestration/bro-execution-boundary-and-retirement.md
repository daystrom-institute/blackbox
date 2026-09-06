---
title: "Bro execution boundary and orchestration retirement"
kind: design
corpus: blackbox-design
lifecycle: partial
topic:
  - orchestration
brief: "Accepted responsibility boundary, complete MCP disposition map, extraction prerequisites, and staged removal of daemon application orchestration."
---

# Bro execution boundary and orchestration retirement

The operator accepted this direction on 2026-09-06: Blackbox owns reliable bro
execution and corpus capabilities. Callers own higher-order orchestration in
ordinary code. Implementation is underway. The milestone record below distinguishes pushed
source changes from deployed behavior. Tracking thread: `thread-d7cd3385`.

The map is grounded in source `8031e3d5`, the matching 190-name live ops catalog,
and read-only deployed consumer checks. The deployed daemon image was built from
`b049caa8572c`; subsequent commits record verification and native collection
activation. Counts describe this snapshot, not a permanent product budget.

## Implementation record

- S1 deployed from `a3139da9` after full cluster verification. Neutral control
  routes, ordinary bro status/wait and advisor refusal passed live smoke checks.
  Mixed legacy task recovery and ownership guards remain enforced.
- S2 deployed from `973ef861` after full verification `bbox-verify-xcb6q` and
  image build `build-bbox-image-6fjpv` passed. The live ops catalog has 168 tools:
  all 22 Slack, Badgey and consultant tools are absent. Their runtime hooks,
  collector binaries, installed defaults and deployment credentials are removed.
  Previously collected search/context/messages retain the same evidence after
  cutover; the removed producer credential returns HTTP 401. The old collector
  journal volume is retained, and ordinary bro admission is open.
- E1 request-key admission is pushed through `a3d40cc7`. Four focused cluster
  checks cover concurrent/reopened claims, corrupt claims, repeated invocation,
  and hidden-origin rejection. It is not deployed yet. An unresolved durable
  claim never authorizes another launch; this is not exactly-once execution.
- E4 maintenance moved into `bbox-vectors`; all 51 focused vector checks passed.
  Connectivity repair snapshots under a short read lock, builds outside the
  partition lock and defers stale publication. Storage GC, embedding residue
  and observation-journal retention run as independent mechanical loops.
- S3/S4 source is pushed through `cb2702f8`: workflow/atom execution, trigger
  routes, reaction delivery and whiteboard mutation are removed. The map gate
  passes 109 surviving declarations and all 82 planned retirements absent.
  `bbox_tool_calls` preserves indexed tool history. Native bro and bro-harness
  builds passed, were stablesigned and installed; Fleet remains and the retired
  orchestrate command is absent. The cage is still running S2 pending full gates.
- S5 removed executable defaults and updated caller-composition guidance.
  Focused checks passed retained history, project ownership and visibility.
  Full verification is in progress; stale permission-test expectations and the
  empty custom-agent adapter registry are being removed before convergence.
  The final deployed surface and response audit remain open.

## Responsibility boundary

Blackbox keeps bro launch/resume/steer/interrupt/cancel, provider and account
routing, task/session identity, worker transport, permission enforcement,
bounded observation, results, transcripts, and execution recovery. Fleet and
fleetd remain. The daemon can own these responsibilities; the process boundary
does not force every bro management function into the harness child.

Callers own plans, branching, application retries, fan-out policy, approvals,
workflow checkpoints, schedules, event-driven business actions, and composition
of agent roles. A bro wait reads/waits for task state; it does not decide to run
an advisor. Blackbox does not replace the retiring workflow graph with another
daemon-hosted programmable orchestration framework.

The corpus remains a separate core responsibility: native transcripts, retained
conversation history, search, graph/provenance, project publication, knowledge,
gaps, notes, and threads survive. Mechanical maintenance required for those
services also survives without requiring workflow or atom execution.

## Exact MCP map

[bro-execution-retirement-map.json](bro-execution-retirement-map.json) gives one
disposition for every current tool, with source owner and implementation wave.
It reconciles exactly against all 190 source declarations and live ops names.
`keep` means outside this retirement, not that the response audit has passed.
`slim` means the tool survives but its actions, DTOs, or internal consumers need
review. `retire` removes the whole tool from callable surfaces.

| Group | Tools | Disposition | Wave |
| --- | ---: | --- | --- |
| Bro control, routing, roster, brofiles and teams | 20 | Slim to execution responsibilities | S1 |
| Named single-bro agent templates | 5 | Slim; keep ordinary role dispatch, remove special runtimes | S1 |
| Badgey | 15 | Retire | S2 |
| Consultant proposals | 4 | Retire | S2 |
| Slack bindings and proposal links | 3 | Retire | S2 |
| Atoms | 8 | Retire after preserving useful local capabilities | S3 |
| Workflows, arcs, signals, crons, pollers and webhooks | 22 | Retire after maintenance extraction | S3 |
| Restricted workflow workspace wrappers | 8 | Retire; retain harness tools and indexed recall | S3 |
| General event/reaction/identity automation | 12 | Retire; extract necessary execution observation | S4 |
| Whiteboard phase/vote machinery | 10 | Retire; preserve historical evidence | S4 |
| Artifact installation/lifecycle | 4 | Slim to surviving kinds | S4 |
| Deterministic policy packets | 6 | Slim to retained permission/policy consumers | S4 |
| Cross-cutting corpus/admin tools | 47 | Remove obsolete hooks, catalog rows and guidance | S4 |
| Other corpus and operational tools | 26 | Keep; remaining response audit continues | S5 |

This proposes retiring **82 of 190 names**, leaving 108 existing names before any
further consolidation or required primitive additions. It is a reduction target,
not a claim that 82 tools are already gone or that all 108 survivors are approved
unchanged. Broad families with action parameters still need action-level review.

Validate the source inventory with:

```sh
python3 scripts/check-orchestration-retirement-map.py
```

During surgery, `--mode progress` allows mapped retired tools to disappear but
rejects unmapped tools or missing survivors. At final closeout, `--mode target`
also requires every retired tool to be absent. Deliberate scope/name changes
must update the map and its rationale in the same commit.
The checker recognizes explicit Rust tool attributes, including reordered
arguments, and ignores comments/literal examples. It does not expand macros or
replace compiled-router and deployed-catalog verification.

## Keep, remove, extract

| Owner / entry points | Target | Dependency work before removal |
| --- | --- | --- |
| `src/tools/dispatch.rs`, `roster.rs`; `/control/*`, `/roster`, `/tail` in `src/server/mcp.rs` | Keep direct bro control, wait aggregation, explicit broadcast, membership and routing | Remove advisor launches from waits and automatic team continuation. Preserve worker cwd, pin precedence, admission/resume leases, result paging and closeout. |
| `src/orchestration/executor.rs`, `fleetd_client.rs`; `crates/fleetd`, `bro-rpc`, `bro-protocol`, `bro-fleet-client`, `bro-cli` | Keep worker execution and Fleet | Preserve authentication, generation fencing, acknowledgement after ingest, child reconnect/replay, control routes and closeout contracts. Remove CLI workflow commands and workflow-only UI actions. |
| `crates/bro-harness`, `bro-code-mode`, `bro-capabilities`, `bro-tools`, `bbox-refactor` | Keep local execution, code mode, filtered capabilities, refactor bindings, local hooks | Remove only daemon `atom_invoke` projection and retired tool guidance. Native refactor operations do not become casualties of atom removal. |
| `src/orchestration/agents`, `src/tools/agents.rs` | Keep simple installed role templates while they add value beyond brofiles | Remove Badgey adapters and application evaluation/promotion/composition loops. Generic single-bro dispatch has independent consumers. Do not keep a workflow/atom execution adapter under an agent name. |
| `src/tools/badgey*`, `consultant/`; `src/orchestration/badgey`, `consultant` | Remove runtime and public surface | `consultant/consumers.rs` registers only Badgey. Remove queue, proposal, action journal and recovery wiring after records are archived. Preserve user knowledge produced by those processes. |
| `crates/bro-slack`, `bbox-slack`; Slack handlers/stores in server/config tools | Remove interactive Slack integration | Detach channel/thread/proposal mappings from project-catalog migration owners and preserve old receipt decoding. Remove credentials only from the retired consumer's configuration, not shared credentials used elsewhere. |
| `crates/bbox-slack-collector`, runtime image and cage deployment | Retire Slack-specific collection from the Blackbox distribution | Stop new Slack collection only after separating ingest authorization from retained read enrollment. Future Slack freshness ends unless an external producer takes ownership. Generic conversation ingestion and historical search stay. |
| `src/workflow`, `src/tools/orchestrate*`, `src/server/{dispatch,restore,workflow_runtime,workflow_capabilities}.rs`, `src/routing.rs` | Remove workflow compiler/engine, arcs, waits, operation DSL and execution routes | Extract essential vector/storage operations; distinguish `src/routing.rs` event DSL from `src/server/surface.rs` permission evaluation. Keep shared `/control/*` handlers from mixed files. |
| `src/orchestration/atoms`, `src/tools/atoms*` | Remove atom registry, composition, invocation, delegation and automatic supervision | Inventory actual runner operations. Preserve meaningful refactor/file/process operations in existing harness modules. External orchestration can reuse prompts as files without keeping the atom runtime. |
| `src/tools/workspace.rs` and workflow workspace dispatch | Remove `work_*` adapters | Host-owned file/git execution remains in harness tools. Indexed tool-call recall must remain reachable through corpus retrieval with precise provenance. Do not restore daemon-local checkout execution. |
| `src/crons.rs`, `pollers.rs`, `webhooks.rs` and adapters | Remove configurable application triggers | Move schedule ownership outside Blackbox; preserve required service maintenance as narrow operations. Remove inbound webhook routes, replay stores and autonomous restore. |
| `crates/bbox-system-events`, `src/system_events_runtime`, `src/tools/system_events.rs` | Remove programmable reaction engine and its MCP management surface | Separate journal/observation from matching, outbox creation and execution; preserve needed bro status/tail/transcript evidence. Archive general events and identity mappings. Forgejo integration recipes move to external owners. |
| `src/orchestration/supervision.rs` | Keep useful execution measurements | Distinguish from `src/tools/atoms/supervision.rs`, which launches higher-order work. Preserve honest last-request context measurements without compaction alarm semantics. |
| Whiteboard runtime/tools and `crates/bbox-whiteboards` | Remove board lifecycle/voting orchestration | Detach inbox/attention, corpus providers and schema consumers. Preserve existing posts, votes and decisions as historical evidence with their visibility constraints. |
| `crates/bbox-packets`, `src/server/surface.rs`, artifact support | Keep minimal deterministic permission/policy machinery initially | Remove workflow routing, phase and auto-advisor packet consumers. Ordinary bro tool filtering currently depends on packet evaluation; missing policy must never silently widen access. Reassess generic packet authoring after consumers are reduced. |
| `src/embed_runtime.rs` | Keep embeddings and contradiction observations | Replace `contradiction-review-arc` launch with the existing note/evidence fallback. An indexing event must not start a new planning process. |
| `src/server/{open,state,background,restore}.rs` | Keep corpus and bro service initialization | Remove automation hooks individually. Shared startup also owns indexing, publication, provenance, vector maintenance and artifact restoration for retained kinds. |

Paths above identify source ownership, not paths a remote MCP caller should read
to recover a tool result. The source layout may change during extraction; update
this map at each milestone.

## Required extraction and migration contracts

### E1: bro operations do not choose subsequent work

`bro_wait` and `bro_when_all` currently call `maybe_resume_team_advisor` in
`src/tools/dispatch.rs`; team routing and `src/tools/roster.rs` can launch advisor
work separately from Badgey. Remove both automatic paths. Keep explicit caller
broadcast and bounded wait-all/wait-any over existing tasks. Keep fixed resource
limits and execution safety, but application retry/continuation policy belongs
to the caller.

Pin down current behavior before promising stronger semantics: stable IDs,
dispatch admission receipt, timeout versus task failure, interrupt versus cancel,
resume serialization, retained results, restart/reconnect, replay limits, and
deduplication of an uncertain dispatch response. An external orchestrator must
not have to guess whether retrying launch creates another bro. If admission
deduplication is missing, add a narrow request-key contract or document a
recoverable lookup protocol before declaring external orchestration ready.

### E2: preserve historical task and catalog readability

`TaskStore::load` in `src/orchestration/mod.rs` currently decodes `tasks.json` as
one `Vec<PersistedTask>` and can return an empty store on a decode error. Deleting
`Origin::{Workflow,Atom,Cron,Webhook}` or `Provider::Workflow` serde support can
hide valid current bro tasks mixed with historical records. Keep read-only
legacy decoding or migrate records individually before dropping variants.
Legacy values must not remain valid new-dispatch choices.

`workflow_owned` reaches protocol DTOs and Fleet closeout. Retain ownership
protections for old tasks/worktrees until explicit migration makes their owner
terminal. Runtime retirement does not grant permission to delete those checkouts.

Slack stores also participate in project-catalog inventory/genesis and stamping
(`crates/bbox-indexing/src/project_catalog_inventory_adapters.rs`,
`project_catalog_genesis.rs`, `src/project_catalog_stamper.rs`). Retire live
owners while preserving migration journal and ownership-digest interpretation.
Apply the same rule to artifact kinds in durable install receipts. Startup with
old data must keep surviving records and must not reanimate retired work.

### E3: retained Slack and whiteboard evidence stays readable

Slack collector shutdown is separate from history deletion. Current conversation
read enrollment is derived from producer grants in `src/server/open.rs`;
`crates/bbox-corpus-index/src/transcripts/conversation.rs` removes unenrolled
channels from search. Merely retaining source bytes does not preserve visibility.
Split accepted historical read enrollment from write authorization, or supply an
explicit frozen-source read enrollment. Verify search/context/messages before
and after disabling writes and restarting the daemon.

Archive inactive automation records under an explicit legacy read boundary, not
by keeping executable registries loaded. Whiteboard evidence keeps its access
and blind-phase constraints; converting it to unrestricted generic notes would
change visibility. A read-only export/reader is sufficient if old exact entity
refs remain meaningful or explicitly describe their migration.

Preserving history is not permission to bypass a real access revocation. A
collector retirement should preserve the same authorized read scope; an
explicit authorization withdrawal must still hide the affected records.

### E4: maintenance does not require workflow execution

Two deployed schedules run corpus/storage maintenance: `daily-compaction` and
`embed-compaction-nightly`. Their checked-in workflows call storage GC,
embedding backfill, vector status and partition compaction. Preserve those real
operations and their serialization, reader safety, bounds and outcomes in
domain code. Expose one bounded maintenance operation where necessary; an
external timer can invoke it without a daemon workflow engine.

Inspect implementation rather than copying the graph's labels: in
`src/workflow/ops.rs`, `QuiesceSearch` and `SwapAtomic` currently return
`OpEffect::None`. The new maintenance contract must describe the actual vector
store behavior and cannot claim quiescence/atomicity merely because an old node
had that name. The useful implementation is in `src/workflow/ops/vector.rs`
and the vector runtime below it.

Do not run production deletion or vector rebuilding as a planning probe. During
implementation, use isolated stores and failure injection, then narrowly smoke
the retained read-only health and admission surfaces. Schedule retirement is
blocked until the maintenance replacement has a documented owner and cadence.

### E5: observation survives without the reaction executor

Direct bro tasks already have roster/tail projection separate from optional
system-event emission. `EventHub` combines journal append with reaction matching
and outbox creation; split those before deleting the outbox worker. Preserve
only necessary task/session evidence and external observation. Do not carry the
whole programmable journal-management surface forward solely to retain a status
stream. No task completion, embedding change, or observation read should trigger
an atom, workflow, advisor, or Slack action after retirement.

### E6: permissions survive packet and artifact reduction

Keep shared MCP filtering, scoped instructions, code-mode filtering and worker
policy composition. Ordinary bro dispatch uses packet-backed surface policy;
missing packet behavior currently permits passthrough. Keep the required policy
subset or replace it with explicit equivalent enforcement before deleting any
policy artifact. Probe flat, qualified MCP and nested code-mode aliases so a
removed restriction cannot survive as a bypass.

## Deployed consumer snapshot

Read-only checks on 2026-09-06 found:

| Surface | Observation | Cutover consequence |
| --- | --- | --- |
| Ops catalog | 190 names, exact source match | Capture all configured surfaces again at each removal milestone. |
| Default catalog | 123 names; 28 mapped retirements, 95 existing survivors | Actual prompt-surface reduction differs from total registered-tool reduction. |
| Atom catalog | 135 unique installed name/version rows, two pages | Retire installed and checked-in definitions; do not equate installed with actively used. |
| Workflow catalog | 39 installed rows | Source deletion alone leaves durable installations and startup loaders. |
| Agent catalog | 5 rows, including two Badgey roles | Preserve useful single-bro role content; remove Badgey adapters and dependent artifacts. |
| Crons | 5 installed rows | Two Badgey jobs and agent evaluation retire; two maintenance jobs need E4. |
| Reaction/poller/webhook registries | Zero installed rows, no reaction load warnings | Lower observed migration burden, not proof that external clients never use them. |
| Badgey instances | Zero | Still remove scheduled admission and durable restoration paths. |
| Arc status | One errored Badgey triage snapshot, zero pending waits | Scheduled automation remains enabled despite idle instance counts. Recheck live execution before cutover. |
| Cluster | `blackboxd` and `bbox-slack-collector`, one ready replica each | Scope deployment changes separately; preserve native/code/file collection. |

The errored arc was triggered by the daily Badgey schedule and failed its MCP
handshake before useful work. Its existence is evidence that zero live Badgey
instances does not mean Badgey admission is disabled. No live workflow, atom,
reaction, Slack write, or GC deletion was invoked for this planning exercise.

The full live payloads remain host-local evidence; this public plan includes only
source-owned names and aggregate observations. Fleet consumers and any other
deployments must be inventoried again at implementation time. A quiet snapshot
does not constitute a drain lease.

## Implementation waves

Each wave ends with scoped commits/pushes, appropriate lane-side verification,
and deployed smoke evidence when runtime behavior changes. Existing operator
deployment authority applies; gates below are technical prerequisites.

### S0: baseline and retirement preparation (this change)

- Commit the accepted boundary, all-name map, consumer snapshot, extraction
  contracts, and validation procedure. Link the ongoing audit to this plan.
- Record which residual response fixes are replaced by retirement. Keep mutation
  consistency and provenance readiness as independent work.
- Freeze feature expansion in retiring families for this campaign. Do not spend
  another milestone polishing reaction DTOs that will disappear.

### S1: make bro control independent

- Isolate retained control/roster routes from orchestration handlers; preserve
  Fleet and fleetd contracts, closeout and worker-local execution.
- Remove automatic advisor/continuation paths from bro waits and team behavior.
  Separate ordinary single-bro agent dispatch from special adapters and preserve
  useful brofiles. Keep the Badgey adapter until S2 stops all of its admission;
  removing the adapter first would break scheduled callers mid-transition.
- Establish E1, E2 and E6 fixtures: legacy/current tasks, permission policy,
  global brofile ownership, model/account pins, explicit cancellation and results.
- Verify direct bro operation with all retiring registries absent. Keep runtime
  telemetry and harness hooks independent from atom supervision.

### S2: retire Slack and Badgey application machinery

- Capture exact installed dependencies and inactive records; stop their admission
  before deleting handlers. Remove Badgey scheduled jobs and application agents,
  workflows, packets, brofiles and auto-install defaults coherently.
- Remove the 22 Badgey/consultant/Slack tools, bot bridge, consultant runtime,
  linkage stores and startup recovery. Apply E2 to catalog/receipt migration.
- Implement E3 before removing Slack collector enrollment/configuration. Stop
  collection and retire its distribution/deployment; preserve historical reads.
- Remove only dedicated service/token/config ownership. Preserve data volumes
  through rollback; source deletion is not a data-retention policy.

### S3: remove workflow and atom execution

- Implement E4. Assign the retained maintenance invocation to an external timer
  or service-owned fixed maintenance loop; no graph interpreter dependency.
- Drain or deliberately terminalize old arcs/invocations with retained receipts.
  Reject new admission on every MCP, HTTP, CLI, artifact and startup path before
  code removal. Prevent cron, watcher and default installation from resurrecting
  retired work. Keep essential bro execution available throughout.
- Remove workflow/atom/workspace routers, engine/DSL, registries, waits, stores
  that exist only to execute them, trigger routes and automatic supervision.
  Remove the harness atom alias and daemon workflow CLI commands together.
- Before deleting either engine, remove or explicitly reject the `AtomInvoke`
  and `StartWorkflow` branches in `src/system_events_runtime/executors.rs` and
  retire their pending deliveries. The executor directly calls both engines;
  S4 is too late to break those dependencies. Historical variants can remain
  readable without a callable executor.
- Confirm useful refactor/file/git/corpus operations remain directly available.
  Replace contradiction workflow launch with a note/evidence observation.

### S4: remove heavy events and dependent coordination; slim shared owners

- Complete E5, remove general reaction execution, outbox/retry machinery, Forgejo
  automation adapters and their 12 MCP tools. Keep only demonstrated execution
  observations and historical identity/event interpretation.
- Remove whiteboard runtime and its ten tools after preserving E3 evidence.
- Restrict artifact kinds and packet consumers; update attention, graph/schema,
  doctor, storage inventory, project migration and knowledge signposts.
- Remove retired jobs from shared startup individually. Indexing, publication,
  embeddings, native transcripts and source freshness continue operating.

### S5: deployment, instruction and surface closure

- Remove retired runtime packaging, cage resources and secret/config demands;
  update converge so absence of Slack inputs is normal. Do not remove the
  cluster's Argo build/verify workflows: those are external orchestration.
- Update source-owned tool docs, generated-schema tests, system memories,
  prompts, examples, manifests and installer/default migrations. Historical
  design/research/spec evidence stays, marked superseded where appropriate.
  Update operative generated instructions through their owning memory/render
  path; do not hand-edit generated AGENTS content or create release ledgers in
  system memory. The accepted boundary is recorded here; no extra blanket memory
  persistence was performed by this preparation change.
- Run final map validation and reconcile default, ops, restricted, project-scoped
  MCP catalogs plus HTTP/CLI/harness entry points. Removed tools must be absent
  or return an explicit retirement error during a bounded migration window;
  no executable compatibility adapters remain at final closeout.
- Finish the brevity/action/DTO audit on surviving tools with explicit evidence.
  Tool-count reduction is not proof of intuitive contracts or complete coverage.

## Packaging and deployment footprint

Inside this repository, inspect `Cargo.toml`/`Cargo.lock`,
`deploy/docker/Dockerfile.runtime`, install/default migrations, and scripts/tests
that build or bundle Slack binaries. Remove obsolete crates only after shared
owner and legacy read migrations above. Keep `bro-capabilities`, `bro-code-mode`,
native collectors, generic conversation/source stores and refactor bindings.

The sibling cage repository owns `index.ts`, `Pulumi.cage.yaml`,
`scripts/converge.sh`, and image-build packaging under `build/`. Current converge
requires Slack collector token/config inputs even when focusing on daemon work;
remove those requirements with the Slack resources. Inventory the dedicated
deployment, runtime secret, bot-token ExternalSecret and journal PVC. Stop the
producer, preserve historical enrollment, and retain the journal/PVC through the
rollback interval before considering separate disposal. Do not delete a shared
vault token or unrelated bot installation merely because one consumer retires.

Its `build/workflows/` Argo definitions and lane pool are retained infrastructure.
Their use of the word workflow is not a dependency on Blackbox's workflow engine.

## Artifact and instruction cleanup ledger

This ledger names active sources requiring a scoped change, not a blanket text
replacement. Useful prompts can be retained as ordinary files for an external
caller without retaining their old executable manifest.

| Source-owned footprint | Required disposition |
| --- | --- |
| `system-defaults/{atoms,workflows,badgey}/`, `system-defaults/agents/badgey.json` | Stop installation and remove executable definitions for retiring runtimes. |
| `system-defaults/agents/{workflows,crons,packets}/` | Remove nightly evaluation and application composition; preserve independently useful simple roles and their prompt/output contracts. |
| `system-defaults/{maintenance,agentic-corpus}/` | Extract E4 operations; remove graph-based schedules, auto-edge/digest and review automation. Preserve direct corpus APIs and required health behavior. |
| `.bbox/workflows/{gap-processing,blackbox-review}.json`, `.bbox/atoms/gap-cluster-validator.json` | Retire repo-owned execution manifests. Inspect associated brofiles independently. |
| `schema/{atom,workflow}.schema.json` | Remove new-write/execution schemas after preserving legacy record readers separately. |
| `examples/{workflows,slack,keystone,sastquatch}/`, workflow portions of `examples/whiteboard/`, reactions in `examples/{forgejo,system-events}/` | Remove runnable retired examples from installation indexes; archive useful historical explanation. |
| `crates/bbox-tool-docs/src/tool_docs.rs`, `system-defaults/mcp-surfaces/routing.json`, `src/server/mod.rs` | Change docs, surface policy and routers together. Do not expose retired operations through a specialist surface. |
| `system-defaults/memories/{atoms,workflow-orchestration}.md` | Retire operative instructions and their catalog entries. Update the `sm-atoms` ordering fixture in `crates/bbox-system-memory/src/catalog.rs`. |
| `system-defaults/memories/{bro-dispatch-patterns,create-etiquette,side-channel-notes,system-memory-catalog}.md` and refactor memories | Preserve useful invariants; remove retired discovery/execution prescriptions and point to direct harness capabilities. |
| `prompts/gap-processing.md`, `prompts/agents/gap-processing-orchestrator.md`, prompt indexes, closeout/daily-cleaning prompts | Remove instructions to start daemon workflows; retain applicable inspection/cleanup guidance. |
| `PROJECT.md`, `docs/{atoms,workflows,badgey,slack-bridge,system-events,ingress-paths}.md`, documentation indexes | State the shipped boundary when removal lands; avoid claiming planned removal already happened. |
| `.bbox/knowledge/{5fa26d26,8b2ff028,5cd8a294,7276b5c6,c46b67e8,998a7834}.json` | Review operative generated-rule sources. Preserve operator-authority and harness-boundary rules while revising obsolete workflow/atom references through the owning memory path. |
| `scripts/converge-gate`, `scripts/lint-concurrency.sh`, `scripts/catalog-ownership-baseline.txt` | Remove retired checks/owners while preserving live-task protection and retained-owner coverage. |
| `src/mcp_client.rs`, `src/orchestration/http_fetch.rs` | Verify remaining callers after retirement; delete if only workflow/reaction/poller consumers remain. Generic naming does not justify retention. |
| `design/`, `research/`, `specs/`, stored artifact versions and `source_arc` provenance | Retain historical evidence with truthful lifecycle/compatibility handling. Do not increase the Tantivy schema version merely for orchestration retirement. |

Do not delete similarly named Rust `atomic` primitives, harness hooks, refactor
`atom_plans` implementations, or external build workflows by textual match.

## Validation and rollback

| Gate | Required evidence |
| --- | --- |
| Bro independence | MCP and Fleet launch/resume/steer/interrupt/cancel/wait/result work with no workflow/atom/Badgey/reaction installation. Waiting launches no additional tasks. |
| Routing and safety | Explicit and implicit provider/account behavior, Astra selection, worker cwd, global templates, scoped tool defaults and deny filters remain correct. |
| Transport | fleetd reconnect, child survival, fenced owners, replay acknowledgement and bounded retention remain correct; keep `scripts/acceptance-fleetd-deps.sh`. |
| Legacy data | Mixed current/legacy task records retain current tasks and ownership; old artifact/catalog receipts load without executing retired kinds; interrupted migration resumes safely. |
| Historical corpus | Slack and native search/context/messages and retained board evidence survive removal/restart with unchanged visibility constraints. |
| Maintenance | Isolated vector/storage maintenance succeeds, failures retain readers/leases/state, and the replacement schedule has an owner. No test depends on production deletion. |
| No hidden admission | Startup, task completion, embedding changes, waits, artifact watches and tool aliases cannot start a retired runtime. |
| Surface closure | Exact retirement map passes; HTTP/CLI routes and default installers agree; retained action schemas/docs are reviewed and response budgets remain bounded. |
| Deployment | Exact image build/verify ref passes, converge completes, daemon/retained collectors are healthy, and live read/control smoke confirms the intended release. |

Use pinned `scripts/fmt.sh`, `cargo nextest run --workspace` for intermediate
gates and `--profile full` for closeout, workspace Clippy and concurrency lint on
the cluster lane. No full Rust rebuild is needed for this documentation/map-only
preparation. Fleet changes require a real isolated TUI smoke. Source tests are
necessary but do not substitute for deployed catalog and control probes.

Before each runtime cutover, snapshot the old image/config identity and the
specific mutable stores being migrated, with counts and checksums. Stop new
admission and ensure no active owner is lost. Use non-destructive, repeatable
migrations with explicit terminal/inactive states. Rollback must not restore a
stale entire corpus or replay already-completed automation. If a store format
changes, provide backward readability or a documented reversible migration
before deployment; an old image alone is not sufficient rollback.

Do not delete historical data, private installations or worktrees as collateral
cleanup. Retained formats can have small read-only compatibility readers without
retaining executable workflow/atom/reaction engines.

## Residual audit disposition

- Reaction delivery caps, event-open shaping, and reaction-install schemas move
  to retirement acceptance; stop treating them as independent feature fixes.
- GC remains and still needs bounded candidate/diagnostic responses and honest
  preview/apply semantics, now also part of E4 maintenance extraction.
- Queued gap mutation lost updates remain a correctness priority for the core
  knowledge/publication path. Retirement does not address them.
- Sustained `edge_index_warming` for provenance export remains an availability
  investigation. No orchestration dependency has been established as its cause.
- Native collection is restored, verified with stablesign and a successful
  background cycle. It remains in the retained execution/corpus baseline.

The plan is ready for S1 extraction work. The full audit and runtime retirement
remain open until the waves and evidence above are complete.
