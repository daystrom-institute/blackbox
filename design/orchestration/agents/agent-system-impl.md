---
title: "Agent System - implementation skeleton"
kind: design
lifecycle: archived
corpus: blackbox-design
topic:
  - orchestration
  - agents
brief: "Dependency-ordered implementation skeleton for the predecessor agent artifact and dispatch surface."
---

# Agent System — implementation skeleton

Companion to `design/agent-system.md`. Each phase below names a
discrete implementation chunk: scope, components, gates (what proves
it's done), known follow-ups, and the design-doc sections it
realizes. Phases are dependency-ordered. No timelines — landing one
phase unblocks dependents, landing all phases realizes the design.

This skeleton assumes `design/corpus/agentic-corpus/agentic-corpus-impl.md` Phase F4
(artifact catalog) and the agentic-corpus vector store substrate
(F3 + E1-E3 of that doc) have landed. The agent system is upstream
of `design/corpus/badgey-impl.md` Phase A0; landing all phases here
unblocks badgey impl.

Phases are prefixed `AS-` to disambiguate from agentic-corpus and
badgey phases.

---

## Substrate extensions

These extend agentic-corpus and the bbox catalog. Required by
`design/agent-system.md` §16.1.

### Phase AS-D1 — `ArtifactKind::Agent` variant + install dispatcher

**Scope.** Add the `Agent` variant to the artifact catalog's
`ArtifactKind` enum, plus the install dispatcher case, supersession
support, and list filtering.

**Realizes.** `design/agent-system.md` §2 storage, §16.1 #1.

**Components.**
- Extend `ArtifactKind` enum in `src/main.rs` (and wherever the
  enum is currently defined: probably `src/orchestration/artifacts.rs`).
- `bbox_artifact_install` dispatcher branch for `kind="agent"`:
  validate, normalize, write to `artifacts/agent/<name>/`,
  manage supersession.
- `bbox_artifact_list` filter accepts `kind="agent"`; results
  include manifest summary (description, version, active flag).
- `bbox_artifact_supersede` works on agent kind unchanged.
- `<name>/metadata.json` carries: install source, version, supersedes,
  active flag, embedding ref (populated by AS-I2).

**Gates.**
- `bbox_artifact_install(kind="agent", source=path)` round-trips
  for a minimal valid manifest.
- Existing artifact-kind tests pass (regression).
- `bbox_artifact_list(kind="agent")` returns active-only by default;
  `include_superseded=true` surfaces history.

**Follow-ups.** AS-I1 plugs in manifest-specific validation. AS-F1
provides the manifest type for serde.

---

### Phase AS-D2 — `agent_manifest` embedding bucket

**Scope.** Add `agent_manifest` to the agentic-corpus per-bucket
embedding routing table.

**Realizes.** `design/agent-system.md` §6.1, §16.1 #2.

**Components.**
- Extend `agentic-corpus` §5.4 bucket table with `agent_manifest`.
  Default route: same as `knowledge` (configurable per-project).
- Vector store partition for the bucket follows existing
  agentic-corpus §5.3 conventions (one slab+graph per
  (provider, dims) tuple; the manifest bucket joins existing
  partitions, no new slab unless a project routes it to a different
  provider than `knowledge`).
- `bbox_embed_status` reports the new bucket alongside existing ones.

**Gates.**
- Embedding queue accepts `agent_manifest` records.
- `bbox_embed_status` shows the bucket in its per-route reporting.
- Reindex from cold state populates the bucket without affecting
  other buckets' indexed counts.

**Follow-ups.** AS-I2 enqueues manifest embeddings on install.
AS-T3 (`bro_agent_search`) queries this bucket.

---

### Phase AS-D3 — `agent` entity type + `EntityRef::Agent` variant

**Scope.** Add the `agent` entity to agentic-corpus's entity model
and the `EntityRef::Agent { name, version }` variant to its grammar.

**Realizes.** `design/agent-system.md` §16.1 #3,
`design/corpus/agentic-corpus/agentic-corpus.md` §6.1 (extension), §6.2 (extension).

**Components.**
- `EntityType` enum gains `Agent` variant.
- `EntityRef` enum gains `Agent { name: String, version: u32 }`.
- `EntityRef::parse` / `EntityRef::render` handle the
  `agent:<name>@v<version>` serialized form.
- `EntityRef::entity_type()` returns `Agent` for the new variant.
- `agent` row added to agentic-corpus's `bbox_describe_schema`
  entity-type catalog: backing store = artifact catalog
  (`kind="agent"`); inspectable fields = manifest fields.
- `InspectableEntityProvider` impl for `Agent` reading from the
  catalog; participates in `bbox_inspect_entity`.
- `EdgeIndex` adds `Agent` to source-type allowlist for
  `DERIVED_FROM` edges (no new edge kind; existing edge taxonomy
  reused).

**Gates.**
- Round-trip parse/render for `EntityRef::Agent`.
- `bbox_describe_schema` lists `agent` as an entity type.
- `bbox_inspect_entity(ref="agent:badgey@v1")` returns the
  manifest with edges (after AS-I3 writes any).

**Follow-ups.** AS-I3 writes `DERIVED_FROM` edges from agent →
evidence sessions for distilled agents.

---

### Phase AS-D4 — Distillation primitives (Rust-internal, NOT on agent dispatch critical path)

**Scope.** Substrate primitives for badgey's distillation arc.
**Rust-internal only**; never exposed via MCP. Raw vector iteration
over MCP would blow response caps (1024-dim f32 = ~4KB/record,
exceeds the 80KB cap at any usable batch size); MCP is the wrong
abstraction for this surface.

This phase blocks **badgey distillation** only. Agent dispatch +
search + install do not depend on it. Listed under substrate
extensions for affinity, but **off the agent-system critical path**.

**Realizes.** `design/agent-system.md` §8.2 substrate dependency,
§16.1 #4.

**Components.**
- `pub fn embed_iterate_internal(bucket: &str, project_id: &str,
  since: Option<DateTime<Utc>>) -> impl Iterator<Item =
  (EntityRef, Vec<f32>)>` — crate-internal Rust API on the vector
  store. Used inside badgey's distillation arc which runs as a
  workflow node calling Rust (via `Shell` op or a dedicated
  workflow op kind), NOT via MCP.
- `pub fn cluster_neighbors_within(bucket: &str, project_id: &str,
  similarity_threshold: f32) -> Vec<ClusterId>` — server-side
  primitive that does the heavy lifting and returns bounded
  result sets (cluster IDs + member entity refs, not raw vectors).
  Optional in v1; needed only if Rust-internal iteration proves
  too coupled.
- Both APIs respect bucket partitioning (agentic-corpus §5.3).

**Gates.**
- Internal iterate over the `transcripts` bucket on the dogfood
  corpus yields all turn embeddings (in-process, no MCP traffic).
- `since` filter correctly excludes older records.
- Memory-safe streaming: doesn't materialize the full vector slab
  in memory at once.

**Follow-ups.** Consumed by badgey's distillation arc (badgey-impl
phase TBD; out of this skeleton's scope). The agent-system itself
does not consume this primitive.

---

## Foundation

### Phase AS-F1 — Rust types

**Scope.** Concrete Rust types under `src/orchestration/agents/types.rs`.

**Realizes.** `design/agent-system.md` §1.2 (AgentSession), §4.1
(manifest schema), §4.2-4.5 (related types).

**Components.**
- `AgentManifest` struct with all fields from §4.1: `description`,
  `when_to_use`, `anti_patterns`, `brofile_ref` / `brofile_inline`,
  `filter_overlay`, `inputs`, `outputs`, `composition`,
  `cost_class`, `dispatch_adapter`, `provenance`, `embedding`.
- `Provenance` enum: `HandAuthored { author, created_at } |
  Distilled { distilled_by, evidence_session_ids,
  created_from_threads, accept_count, reject_count } |
  Imported { source, import_at }`.
- `AgentRef` newtype — `agent:<name>@v<version>` parse/render.
- `AgentSession { session_id, provider, project_dir, agent, task_id }`
  with serde.
- `BadgeyAgentArgs { prompt, badgey_id }` (used by adapter; defined
  here so badgey crate can consume).
- `MergedFilters { allow: Vec<String>, disallow: Vec<String> }` —
  result of merging brofile filters with manifest overlay.
- `EvidenceDensity` enum: `Low | Medium | High`.
- `CostClass` enum: `Cheap | Normal | Expensive`.
- `CompositionShape` enum: `Chain | FanOut | Escalation`
  (currently used only as composition manifest field constants;
  no `bro_agent_compose` consumer in v1).

**Gates.**
- Serde round-trip for `AgentManifest`.
- `AgentRef::parse("agent:foo@v3")` round-trips.
- Validation helpers (`validate_description_length`,
  `validate_when_to_use_nonempty`) cover the §4.3 lint cases.

**Follow-ups.** AS-F2 + AS-F3 + AS-I1 consume.

---

### Phase AS-F2 — `AgentDispatchAdapter` trait + `AgentAdapterRegistry`

**Scope.** Define the adapter contract and the daemon-side registry.

**Realizes.** `design/agent-system.md` §11.4 (AgentDispatchAdapter
interface), §4.3 step 5 install validation hook.

**Components.**
- `AgentDispatchAdapter` trait in `src/orchestration/agents/adapter.rs`:

  ```rust
  #[async_trait]
  pub trait AgentDispatchAdapter: Send + Sync {
      fn name(&self) -> &'static str;
      async fn dispatch(
          &self,
          manifest: &AgentManifest,
          args: serde_json::Value,
          ctx: DispatchContext,
      ) -> Result<AgentDispatchResult, AgentDispatchError>;
  }
  ```
- `DispatchContext`: `{ project_dir, ambient, bro_label_prefix,
  caller_provider, caller_session_id }`.
- `AgentDispatchResult { session, task_id, resolved_brofile,
  merged_filters, degraded }`.
- `AgentDispatchError` enum: `BadInput`, `NotFound`, `AdapterFailed`.
- `AgentAdapterRegistry`:
  - `register(adapter: Arc<dyn AgentDispatchAdapter>)` —
    crate-internal; called from `main()` at daemon startup.
  - `get(name: &str) -> Option<Arc<dyn AgentDispatchAdapter>>`.
  - `list_registered() -> Vec<&'static str>`.
- Init order in `main()`: registry initialized BEFORE artifact
  catalog opens for validation. Documented in `main.rs` with a
  comment marker.

**Gates.**
- Mock adapter test: register a noop adapter, dispatch through it,
  receive the noop result.
- Registry returns `None` for unknown adapter names.
- Init-order test: artifact catalog open fails if registry was not
  initialized first (defensive panic with clear message).

**Follow-ups.** AS-T4 consumes for routing. badgey-impl M1 registers
the badgey adapter.

---

### Phase AS-F3 — `AgentRegistry` projection

**Scope.** Read-only projection over the artifact catalog plus the
vector store. Provides `list`, `get`, `search` lookups consumed by
the MCP tools.

**Realizes.** `design/agent-system.md` §2 layering, §5.

**Components.**
- `AgentRegistry` in `src/orchestration/agents/registry.rs`:
  - `list(filter: ListFilter) -> Vec<AgentSummary>` — reads
    `artifacts/agent/*/metadata.json`, filters by active /
    cost_class / provenance.kind.
  - `get(name: &str) -> Option<AgentManifest>` — reads
    `artifacts/agent/<name>.json` (active version) or
    `artifacts/agent/<name>@v<n>.json` (pinned).
  - `search(query: &str, top_k: u32, filter: SearchFilter)
    -> SearchResults` — embeds query, queries the
    `agent_manifest` vector bucket, joins with manifest metadata,
    applies anti-pattern penalty (§5.2 of design doc), returns
    ranked.
- Anti-pattern penalty implementation (consumes AS-I2 component
  embeddings): for each candidate, compute `cosine(query,
  when_to_use_embedding)` and `cosine(query,
  anti_pattern_embedding)` from the agent's stored component
  vectors. If anti-pattern cosine exceeds when_to_use cosine by
  the configured threshold, downrank by the penalty factor. If
  the anti_patterns embedding is missing (agent has empty
  anti_patterns OR component not yet embedded), the penalty
  no-ops for that agent.
- Manifest cache: in-memory LRU keyed on `(name, version)`;
  invalidated on `bbox_artifact_supersede` event hook.

**Gates.**
- `list` returns only active versions by default; `include_superseded=true` surfaces history.
- `get` resolves bare-name to active version; explicit `name@v3`
  resolves to the pinned version.
- `search` ranks installed agents by query relevance; anti-pattern
  penalty observable in test query that should match `when_to_use`
  but ALSO matches `anti_patterns` (latter case downranks).

**Follow-ups.** AS-T1 / AS-T2 / AS-T3 wrap these methods.

---

## Install pipeline

### Phase AS-I1 — Install handler + manifest validation

**Scope.** Manifest-specific validation in the
`bbox_artifact_install(kind="agent")` path. The 10-step §4.3 list.

**Realizes.** `design/agent-system.md` §4.3.

**Components.**
- `validate_agent_manifest(manifest: &AgentManifest, ctx: &InstallCtx)
  -> Result<(), ValidationError>` runs all 10 steps:
  1. JSON parse (handled at the catalog layer; no-op here)
  2. Schema validate against `schema/agent.schema.json`
  3. `brofile_ref` XOR `brofile_inline` enforced
  4. Brofile resolution (catalog lookup or inline schema validate)
  5. Lint manifest fields (description length, when_to_use
     non-empty, JSON-Schema-of-JSON-Schema validation, prompt
     template syntax, filter pattern grammar, cost_class enum,
     provenance kind enum, dispatch_adapter registry membership)
  6. Filter overlay sanity check (warn if overlay's `allow`
     conflicts with brofile's `disallow`)
  7. Embedding compute (delegated to AS-I2)
  8. Atomic write
  9. Provenance edge writing (delegated to AS-I3)
  10. Return install result
- `schema/agent.schema.json` — JSON Schema 2020-12 file shipped
  with bbox.
- `JSON-Schema-of-JSON-Schema` check: bundled meta-schema; rejects
  malformed `inputs.schema` / `outputs.schema`.
- Prompt template parsing: use Handlebars (or whatever the existing
  workflow templater chose) and reject on parse error.
- Filter pattern normalization: accept both canonical
  `mcp__blackbox__bro_*` and dotted `mcp__blackbox__.bro_*` forms;
  store one canonical form.
- Conflict warning: install metadata records the warning; install
  succeeds but `bbox_artifact_describe` shows the warning.

**Gates.**
- A minimal valid manifest installs cleanly.
- Each lint case has a deliberately-bad manifest fixture that
  fails at the expected step with the correct error code.
- Conflict warning recorded for an overlay that conflicts with
  brofile filters; install succeeds.
- Atomic write semantics confirmed (kill mid-write → reopen sees
  prior state).

**Follow-ups.** AS-I2 + AS-I3 consume.

---

### Phase AS-I2 — Embedding at install time (component embeddings)

**Scope.** Compute and store **three component embeddings** per
manifest: primary (description-shaped composite), when_to_use,
and anti_patterns. The anti-pattern penalty in `bro_agent_search`
ranking (§5.2 of design) requires per-component cosine
comparisons; a single composite embedding cannot support that.

**Realizes.** `design/agent-system.md` §6.1, §5.2 anti-pattern
penalty, §4.3 step 7.

**Components.**
- `compute_manifest_embeddings(manifest: &AgentManifest) -> ManifestEmbeddings`
  produces three vectors:
  - `primary`: embeds `{description}\n\nWhen to use:\n- ...`
    (description + when_to_use joined). This is the search-target
    embedding.
  - `when_to_use`: embeds the `when_to_use` lines only (one short
    text). Used in ranking.
  - `anti_patterns`: embeds the `anti_patterns` lines only.
    Skipped if anti_patterns array is empty (penalty becomes
    no-op for that agent).
- Vector storage: three records per agent in the `agent_manifest`
  bucket, keyed `agent_embed:<name>:v<version>:<component>` where
  component ∈ {primary, when_to_use, anti_patterns}.
- `manifest.embedding` field stores all three `vector_ref`s.
- All three queued through the same embedding queue + bucket
  route. Failure is per-record; the manifest is searchable as
  soon as `primary` lands (anti-pattern penalty is best-effort
  until the other two land).
- On embedding-provider unavailability:
  - `degraded.embedding_pending=true` (records not yet computed)
  - install proceeds
  - search excludes from results until at least `primary` lands
  - anti-pattern penalty no-ops on agents missing the
    `anti_patterns` embedding (still rankable, just less precise)

**Gates.**
- Three vectors land in the bucket per install.
- Anti-pattern-penalty regression test: a query matching both
  when_to_use AND anti_patterns ranks lower than a query
  matching only when_to_use; the magnitude depends on which
  cosine is greater.
- Re-install (supersede): all three re-computed; old versions
  remain in the bucket but search filters to active only.
- Re-embeds are cheap (bulk-tested: 100 manifest installs <
  baseline + 30s).

**Follow-ups.** Consumed by AS-T3 search + AS-F3 ranking.

---

### Phase AS-I3 — Provenance edges at install time

**Scope.** Write `DERIVED_FROM` edges from `agent:<name>@v<version>`
to evidence session/thread refs for distilled agents.

**Realizes.** `design/agent-system.md` §8.1, §4.3 step 9.

**Components.**
- For `manifest.provenance.kind == Distilled`:
  - For each `evidence_session_ids[]`: write
    `EdgeIndex` entry `(agent_ref, DERIVED_FROM, session_ref,
    confidence=Exact, provenance=Explicit)`.
  - For each `created_from_threads[]`: same pattern, target =
    thread_ref.
  - Edges land in the per-project edge sidecar
    (agentic-corpus §5.5).
- For `HandAuthored`: no edges written.
- For `Imported`: no edges written.
- On edge-write failure (sidecar I/O error): install rolls back
  via the existing artifact catalog's atomic-write semantics;
  return `error.server`.

**Gates.**
- A distilled-agent install lands with N edges where N =
  evidence_session_ids.len() + created_from_threads.len().
- `bbox_inspect_entity(ref="agent:foo@v1", edge_types="DERIVED_FROM")`
  returns the evidence refs.
- Hand-authored install writes no edges.

**Follow-ups.** Consumed by `bbox_blame` walks (existing tool;
inherited support for the new entity type via AS-D3).

---

## MCP surface

### Phase AS-T1 — `bro_agent_list`

**Scope.** MCP tool that lists installed agents, optionally filtered.

**Realizes.** `design/agent-system.md` §4.1, §5.1.

**Components.**
- `#[tool] async fn bro_agent_list(&self, params: BroAgentListParams)`
  in `src/main.rs` (or new `src/agent_tools.rs`).
- `BroAgentListParams { include_superseded?, cost_class?,
  provenance_kind?, limit? }`.
- Wraps `AgentRegistry::list`. Returns
  `Vec<AgentSummary>` (name, version, description, cost_class,
  active, embedding_pending).
- `tool_docs.rs` stanza per existing convention.

**Gates.**
- `tool_docs.rs` compile-time check passes.
- Filtering on each parameter works.
- Default response excludes superseded versions.

**Follow-ups.** AS-T2 / AS-T3 follow.

---

### Phase AS-T2 — `bro_agent_describe`

**Scope.** Full manifest + resolved brofile + merged filters for one
agent.

**Realizes.** `design/agent-system.md` §5.1, §4.5.

**Components.**
- `#[tool] async fn bro_agent_describe(&self, params: BroAgentDescribeParams)`.
- `BroAgentDescribeParams { agent: String, version?: u32 }`.
- Returns: full manifest + resolved brofile content (or ref) +
  computed `MergedFilters` against the resolved brofile +
  any install-time warnings + embedding status.
- `tool_docs.rs` stanza.

**Gates.**
- Describe returns the full manifest with resolved brofile body.
- Merged filters reflect the brofile + overlay deny-wins merge.
- Install-time warnings (filter overlay conflicts) surfaced.

---

### Phase AS-T3 — `bro_agent_search`

**Scope.** Semantic search MCP tool.

**Realizes.** `design/agent-system.md` §5.2.

**Components.**
- `#[tool] async fn bro_agent_search(&self, params: BroAgentSearchParams)`.
- `BroAgentSearchParams { query: String, top_k?: u32, filter?,
  exclude_anti_pattern_matches?: bool }`.
- Wraps `AgentRegistry::search`.
- Returns: `Vec<SearchResult>` with similarity score, plus
  `vector_status` per agentic-corpus convention.
- `tool_docs.rs` stanza naming the anti-patterns and example
  invocation.

**Gates.**
- Query embedding computed; HNSW search returns ranked candidates.
- Anti-pattern penalty observable: a query matching both
  `when_to_use` and `anti_patterns` ranks lower than a query
  matching only `when_to_use`.
- `vector_status.coverage_ratio` < 1.0 when some agents are
  embedding-pending; results from those agents excluded.

---

### Phase AS-T4 — `bro_agent_dispatch`

**Scope.** The dispatch tool. Routes through adapter or direct path.

**Realizes.** `design/agent-system.md` §5.3, §11.4.

**Components.**
- `#[tool] async fn bro_agent_dispatch(&self, params: BroAgentDispatchParams)`.
- `BroAgentDispatchParams { agent: String, args: serde_json::Value,
  project_dir?, bro?, ambient? }`.
- Routing:
  ```rust
  let manifest = registry.get(&params.agent)?;
  match &manifest.dispatch_adapter {
      Some(name) => {
          let adapter = adapter_registry.get(name)
              .ok_or(error_adapter_unavailable(name))?;
          adapter.dispatch(&manifest, params.args, ctx).await?
      }
      None => {
          // Direct path:
          validate_args_against_schema(&params.args, &manifest.inputs.schema)?;
          let prompt = expand_template(&manifest.inputs.prompt_template, &params.args)?;
          let brofile = resolve_brofile(&manifest)?;
          let merged = merge_filters(&brofile.filters, &manifest.filter_overlay);
          let task_id = TaskId::new();
          let bro_label = format!("agent:{}@v{}", manifest.name, manifest.version);
          spawn_with_pre_minted_id(task_id, ExecParams {
              prompt, project_dir, brofile, merged_filters: merged,
              bro_label, ambient, ...
          }).await?;
          AgentDispatchResult {
              session: AgentSession { session_id, provider, project_dir,
                                       agent: manifest.ref(), task_id },
              ...
          }
      }
  }
  ```
- Adapter failures are not silently swallowed; surface as
  `AgentDispatchError::AdapterFailed`.
- `tool_docs.rs` stanza per §5.3 / §5.5 of design doc.

**Gates.**
- Direct-path dispatch on a null-adapter agent: returns valid
  `AgentSession`; `bro_status(task_id)` confirms running bro under
  the resolved brofile.
- Adapter dispatch on the badgey agent (after badgey-impl B1b
  installs it): adapter receives the call and returns
  `AgentDispatchResult` with the same shape as direct path.
- Adapter unavailable (de-register adapter, attempt dispatch):
  hard fail with `error.bad_input(code=adapter_unavailable)`. No
  fallback.
- `bro_label` encoding `agent:<name>@v<version>` visible in
  `bro_status` output.

**Follow-ups.** Resume / status / cancel reuse existing
`bro_resume` / `bro_status` / `bro_cancel` (they accept the
`session_id + provider` from the returned `AgentSession`).

---

## Schema discovery

### Phase AS-S1 — `bbox_describe_schema` consultants section

**Scope.** Add an `agents` section to the schema response listing
installed agents with summary + use cases + anti-patterns.

**Realizes.** `design/agent-system.md` §10.2.

**Components.**
- Extend `bbox_describe_schema` response with `agents: Vec<AgentSummary>`.
- Per agent: `name`, `version`, `description`, `when_to_use`,
  `anti_patterns`, `cost_class`, example invocation.
- Faceting: `agents_by_dispatch_adapter` group surfaces wrapper-
  flavored agents distinctly (badgey appears under the
  `dispatch_adapter:"badgey"` group).

**Gates.**
- Response includes the `agents` section.
- Cold Codex / Gemini sees the section through MCP discovery.
- Snapshot test of schema response validates structure.

**Follow-ups.** None blocked.

---

## Composition

### Phase AS-C1 — Composition fields validation + reference workflows

**Scope.** Validate the manifest's composition fields at install
time. Ship reference workflows demonstrating chain / fan-out /
escalation patterns hand-authored against `bro_agent_dispatch`.

**Realizes.** `design/agent-system.md` §7.1-7.3, §7.4 deferral.

**Components.**
- Install-time validation (folded into AS-I1 §4.3 step 5):
  - `composition.chainable_after`: each entry resolves to an
    installed agent OR is recorded as a forward reference (warn
    but allow).
  - `composition.parallel_safe`: bool, no validation.
  - `composition.fan_out_aggregator`: enum membership
    (`vote-majority | ensemble-merge | first-success`) OR matches
    a workflow node id (free-form).
- Reference workflows under `examples/agents/workflows/`:
  - `chain.json`: A → B chain via `mcp_call` per
    agent-system §10.1 pattern
  - `fan-out.json`: same prompt to N agents + aggregator
  - `escalation.json`: try cheap, fall back to expensive on
    confidence-low
- No `bro_agent_compose` tool in v1.

**Gates.**
- Install-time validation rejects malformed composition fields.
- Reference workflows install via `bbox_artifact_install(kind="workflow")`
  and dispatch via `bro_orchestrate_run` against test agents.

**Follow-ups.** v2 adds `bro_agent_compose` sugar tool.

---

## Eval

### Phase AS-E1 — Per-agent eval suite skeleton

**Scope.** Per-agent gold-standard eval queries. Lives at
`eval/agents/<name>/`. The framework is shared; individual agents
populate their own queries.

**Realizes.** `design/agent-system.md` §13.1, §13.2.

**Components.**
- `eval/agents/<name>/queries.toml` schema:
  - `[[positive]]` queries (should match this agent in search)
  - `[[adversarial]]` queries (should NOT match)
  - `[[dispatch]]` queries (validate dispatch output structure)
- `eval/agents/check.rs`: per-query `check_pass(query, results,
  gold) -> (bool, Vec<String>)` for each category. Structural
  checks first (§13.2 ordering).
- `eval/agents/run.rs`: harness that iterates registered agents,
  runs queries, aggregates pass-rate.

**Gates.**
- Skeleton compiles.
- A sample agent's eval suite parses + runs end-to-end (even if
  pass-rate is initially low).

**Follow-ups.** AS-E2 / AS-E3 / AS-E4 build on this.

---

### Phase AS-E2 — Discovery eval

**Scope.** Cross-agent discovery quality.

**Realizes.** `design/agent-system.md` §13.3.

**Components.**
- `eval/agents/discovery_queries.toml` — queries spanning multiple
  agents.
- Per-query gold: ranked agent list (top-3 expected).
- Pass criteria: returned ranking matches gold within tolerance
  (e.g., gold-top-1 in returned-top-3).

**Gates.**
- Discovery eval runs against the dogfood corpus + installed agents.
- Pass-rate baseline established; tracked in
  `agent-discovery-eval-baseline` thread.

---

### Phase AS-E3 — Active cuing eval

**Scope.** Behavioral eval validating that brofiles with active-mode
cuing actually delegate.

**Realizes.** `design/agent-system.md` §13.4.

**Components.**
- `eval/agents/cuing/scenarios.toml`:
  - `should_delegate`: scenarios where the cued brofile should
    dispatch to the gold agent
  - `should_not_delegate`: scenarios matching anti-patterns
- Test harness: dispatches the cuing-enabled brofile with each
  scenario's prompt; reads the resulting bro's tool-call log via
  `bbox_search(role=tool_use, session_id=...)`; asserts presence
  / absence of `bro_agent_dispatch` calls per scenario.

**Gates.**
- Each scenario produces the expected delegation behavior on the
  reference brofile.
- Mis-delegation is surfaced (the brofile dispatches the wrong
  agent).

**Follow-ups.** AS-E4 wraps in a workflow.

---

### Phase AS-E4 — Eval arc workflow

**Scope.** Nightly + weekly arcs that run the eval suites and
track baselines.

**Realizes.** `design/agent-system.md` §13.2 (eval arc).

**Components.**
- `examples/agents/workflows/agent-eval-arc.json` — nightly:
  - per-agent search + dispatch eval
  - aggregator node posts pass-rates to
    `agent-eval-baseline` thread
  - regression detector (>5% drop) emits `bbox_note(kind=blocked,
    tag=agent-eval-regression)`
- `examples/agents/workflows/agent-cuing-eval-arc.json` — weekly
  (more expensive; runs end-to-end dispatches).
- `examples/agents/crons/agent-eval-nightly.json` — schedules the
  nightly arc.

**Gates.**
- Manual run completes; baseline thread populated.
- Forced regression triggers the alert note.

---

## IaC examples

### Phase AS-IaC1 — `examples/agents/` reference manifests

**Scope.** A handful of reference agents demonstrating manifest
shape + use cases. Depends on AS-T4 (dispatch must work for the
gate to exercise reference workflows) and AS-C1 (the reference
workflows themselves).

**Realizes.** `design/agent-system.md` IaC pattern (analog to
agentic-corpus §2.1).

**Components.**
- `examples/agents/code-reviewer.json` — full manifest for the
  example in §4.1 of the design doc; brofile_ref to a separately-
  installed `code-reviewer-persona` brofile.
- `examples/agents/code-reviewer-persona.json` — the brofile
  artifact.
- `examples/agents/diff-narrator.json` — chainable-before
  code-reviewer (composition demo).
- `examples/agents/badgey.json` — placeholder pointer to badgey-impl
  B1b (badgey docs ship the canonical badgey manifest in
  `examples/badgey/agents/`).

**Gates.**
- Each ships installable cleanly via `bbox_artifact_install`.
- Each is dispatchable end-to-end via `bro_agent_dispatch` (AS-T4)
  with valid args.
- AS-C1 reference workflows (chain / fan-out / escalation)
  successfully dispatch the bundled reference agents.

---

### Phase AS-IaC2 — Eval workflow installs

**Scope.** Ship the eval workflows + crons as installable artifacts.

**Components.**
- `examples/agents/workflows/agent-eval-arc.json` (from AS-E4).
- `examples/agents/workflows/agent-cuing-eval-arc.json` (from AS-E4).
- `examples/agents/crons/agent-eval-nightly.json` (from AS-E4).
- `examples/agents/packets/agent-eval-policy.json` — eval gates
  for accepted proposals (forward link to badgey distillation).

**Gates.**
- Each artifact installs via `bbox_artifact_install`.
- None fire by default (opt-in).

---

## Hardening

### Phase AS-H1 — Failure-mode validation

**Scope.** Each failure mode in `design/agent-system.md` §12 has a
test exercising the documented mitigation.

**Realizes.** §12.

**Components.**
- §12.1 manifest drift: install a brofile, supersede it without
  bumping the agent manifest version → `bro_agent_describe`
  surfaces `degraded.manifest_stale=true`.
- §12.2 embedding poisoning: deliberately-poisoned manifest fixture
  (description over-broad); confirm anti-pattern penalty downranks.
- §12.3 session leak: documented limitation; test ensures
  `bbox_remember` surface stores `AgentSession` not bare
  `session_id` (lint).
- §12.4 distillation drift: this lives in badgey-impl; agent-system
  side ensures auto-deprecation proposals route through the badgey
  proposal store.
- §12.5 filter overlay conflict: install with conflicting overlay;
  warning recorded; merged filters at dispatch confirm deny-wins.
- §12.6 cross-provider mismatch: dispatch a Codex-brofile agent from
  a Claude session; bro spawns under Codex provider regardless of
  caller.

**Gates.**
- Each scenario has an integration test that passes.

---

### Phase AS-H2 — Observability + per-agent metrics

**Scope.** Per-agent metrics surfaced through standard observability.

**Realizes.** `design/agent-system.md` §13 (observability blends
into eval; agent-system has lighter observability than badgey).

**Components.**
- Per-agent counters in dashboard:
  - dispatch count
  - success / failure ratio
  - average elapsed time
  - cost-class-aggregated token spend (advisory only;
    per-scope monthly budget surfaced not enforced)
- `bro_dashboard` integration showing agent attribution from the
  `agent:<name>@v<version>` `bro_label` prefix.
- Per-agent eval pass-rate trend (from AS-E4) surfaced in
  `bbox_inbox` summary.

**Gates.**
- Dispatch counters increment per `bro_agent_dispatch` call.
- Dashboard shows agent attribution alongside generic bro
  attribution.
- Eval regression alerts surface in `bbox_inbox`.

---

## Phase summary

```
Substrate extensions for agent dispatch (must precede core)
  AS-D1 (ArtifactKind::Agent variant + install dispatcher)
  AS-D2 (agent_manifest embedding bucket)
  AS-D3 (agent entity type + EntityRef variant)

Substrate extensions for distillation (off critical path; blocks badgey only)
  AS-D4 (Rust-internal distillation primitives)

Foundation
  AS-F1 (Rust types)              ◄── AS-D1, AS-D3
  AS-F2 (adapter trait + registry) ◄── AS-F1
  AS-F3 (registry projection)     ◄── AS-F1, AS-D2, AS-I2

Install pipeline
  AS-I1 (install handler + validation) ◄── AS-D1, AS-F1, AS-F2
  AS-I2 (component embeddings at install) ◄── AS-D2, AS-I1
  AS-I3 (provenance edges at install)  ◄── AS-D3, AS-I1

MCP surface
  AS-T1 (bro_agent_list)         ◄── AS-F3, AS-I1
  AS-T2 (bro_agent_describe)     ◄── AS-F3
  AS-T3 (bro_agent_search)       ◄── AS-F3, AS-I2, AS-D2
  AS-T4 (bro_agent_dispatch)     ◄── AS-F3, AS-F2, AS-I1

Schema discovery
  AS-S1 (bbox_describe_schema agents) ◄── AS-T1, AS-T2

Composition (manifest fields only; sugar v2)
  AS-C1 (validation + reference workflows) ◄── AS-T4, AS-I1

Eval
  AS-E1 (per-agent eval skeleton)
  AS-E2 (discovery eval)         ◄── AS-T3, AS-E1
  AS-E3 (active cuing eval)      ◄── AS-T4, AS-E1
  AS-E4 (eval arc workflow)      ◄── AS-E2, AS-E3

IaC
  AS-IaC1 (reference manifests)  ◄── AS-I1, AS-T4, AS-C1
  AS-IaC2 (eval workflow installs) ◄── AS-E4

Hardening
  AS-H1 (failure-mode validation)  ◄── all install + dispatch phases
  AS-H2 (observability)            ◄── AS-T4, AS-E4
```

Critical path (agent dispatch core): **AS-D1 + AS-D2 + AS-D3 →
AS-F1 → AS-F2 + AS-I1 → AS-I2 → AS-F3 → AS-T4 → AS-IaC1 → AS-H1**.

AS-D4 (distillation primitives) is **NOT on the agent-system
critical path**. It blocks only badgey distillation. Land
asynchronously when badgey impl needs it.

AS-S1 + AS-T1-3 + AS-C1 + AS-E* + AS-H2 parallelize once their
direct upstream lands.

This skeleton splits across the two **A0** phases referenced in
`design/corpus/badgey-impl.md`:

- **A0-core** — landing the core dispatch phases (AS-D1, AS-D2,
  AS-D3, AS-F*, AS-I*, AS-T*) unblocks badgey B1b (which installs
  `agent:badgey@v1` with `dispatch_adapter="badgey"`) and M1.
- **A0-distill** — landing AS-D4 unblocks ONLY badgey's
  distillation arc (the `propose-agent` proposal kind mining loop).
  Not required for badgey core. Land asynchronously when badgey
  distillation is being implemented.
