---
title: "Agent System - discovery, dispatch, distillation"
kind: design
lifecycle: archived
corpus: blackbox-design
topic:
  - orchestration
  - agents
brief: "Predecessor design for manifest-wrapped brofiles and semantic agent discovery before atoms became the public capability model."
---

# Agent System — discovery, dispatch, distillation

## 1. Thesis

The bbox substrate already runs agents — it just doesn't know it. Every
brofile is a persona; every `bro_exec` spawns one; the artifact catalog
versions them. What's missing is the **discovery and composition layer**
that lets agents be *selected* for a task instead of *named* for it.

Claude Code formalizes selection-by-cue for one provider via
`.claude/agents/*.md` files: markdown with YAML frontmatter
(`name`, `description`, `model`, `tools`). The frontmatter does double
duty — declarative install metadata and live selection cue ("when the
user prompt looks like X, prefer agent Y"). It works inside Claude. It
is invisible to every other provider on the same machine.

This doc generalizes the pattern, but **does not adopt the format**.
Bbox agents are JSON artifacts in the catalog, accessed only over MCP.
The Claude `.md` files remain Claude's private convention; bbox does
not read them and does not ship in their shape. The whole point of the
MCP abstraction is that other systems do not read provider-specific
agent files. There is no value in a Claude-shaped backflip.

What an Agent is, in one sentence: **a brofile with a manifest, where
the manifest is "Claude frontmatter on steroids" — same role
(selection cuing) but rich enough to power semantic discovery,
composition contracts, provenance, and cross-provider dispatch.**

The manifest carries:
- description + when_to_use + anti_patterns (selection cuing, like
  Claude's `description` field but split for precision)
- input/output contracts (composition substrate)
- semantic embedding (cross-agent discovery without exact-name lookup)
- provenance (hand-authored vs distilled by which badgey, with
  traceability back to the corpus that motivated the agent)
- filter overlay (per-agent allow/deny on top of the brofile's filters)
- cost hint (cheap/normal/expensive — drives escalation patterns)

The dispatch primitive is unchanged: agents run as bros under
`bro_exec`; they resume under `bro_resume`. No new lifecycle, no new
dispatch engine. Sessions live as long as someone holds the
`AgentSession` handle (§1.2) and lets go when nobody does. Pass an
`AgentSession` around in a thread — it's still resumable a week
later, subject to substrate TTL.

Badgey is an agent under this framing — a consultant-flavored one
with extra producer machinery (proposal store, action journal). Most
agents will not need that machinery. The framework here is broader;
badgey is one citizen, not the model citizen.

### 1.1 Why bother

Three concrete payoffs:

1. **Cross-provider parity.** Codex, Gemini, OpenCode have no native
   agent-selection layer. With agents as MCP-discoverable artifacts,
   every provider gets the same surface: search by query → dispatch →
   resume. Claude users can choose: native Task tool for ephemeral
   built-ins, `bro_agent_dispatch` for project-installed agents.
2. **Distillation.** Badgey already proposes workflows / packets /
   brofile-lenses. Add a fifth: `propose-agent`. When badgey detects
   a recurring task shape across the corpus, it drafts a manifest +
   brofile draft + evidence bundle. User approves; the agent registry
   is one richer. Agents become a compounding artifact class, not a
   hand-curated catalog.
3. **Composition.** Agents declare input/output contracts.
   Chain / fan-out / escalation patterns ride on top of the existing
   workflow engine without each workflow author reinventing
   composition glue.

### 1.2 Lifecycle: portable handle, substrate-bounded TTL

The substrate's `bro_exec` / `bro_resume` already provides the
lifecycle shape agents need, with two caveats the doc is honest about:

**The portable handle is not just `session_id`.** `bro_resume`
requires `session_id + provider` outside named-bro scenarios; brofile
resolution and provider routing are not recoverable from the
session_id alone. The agent system surfaces a **portable
`AgentSession` handle**:

```rust
pub struct AgentSession {
    pub session_id: String,
    pub provider: ProviderName,
    pub project_dir: Option<PathBuf>,
    pub agent: AgentRef,            // "code-reviewer@v3"
    pub task_id: TaskId,            // most-recent dispatch task; refreshed on resume
}
```

Dispatch returns this; resume takes this; status/list surface it.
Storing this handle (in a thread post, a knowledge entry, a workflow
var) is what makes a session resumable later. Storing only
`session_id` is a hazard the doc explicitly does not endorse.

**TaskStore has a load TTL.** The bbox `TaskStore` evicts task
metadata 24h after last activity by default (substrate behavior).
This means: an `AgentSession` handle stored in durable storage
remains *resolvable* (the session_id + provider can be passed to
`bro_resume`), but the task-level metadata (live status, recent
events) ages out. Resuming a 7-day-old session works; querying its
historical task status doesn't.

**Provider-side session storage outlives bbox TaskStore.** Provider
session histories (Claude / Codex / Gemini all persist their own
session content) live by the provider's policy, typically much
longer than 24h. The session_id remains valid until the provider
GCs it.

So the durability story is layered:
- **Live (≤ 24h since last activity):** task status queryable;
  full bbox-side observability via `bro_status` / dashboard.
- **Cold (> 24h since last activity):** session resumable but task
  metadata gone; resumption refreshes activity, re-populates task
  metadata for another 24h window.
- **Provider-evicted (≥ provider TTL):** unrecoverable.

Within this substrate, there is no agent-system-level idle timeout.
No "session_cached vs consultant" taxonomy. Durability is emergent:
how long does anyone hold the `AgentSession` handle, in what
storage, and choose to resume it.

The handle is portable. Post one to a thread; a future caller picks
it up, optionally a different provider caller (subject to brofile
provider compatibility — see §9.2 limitations). Workflow vars carry
handles. Knowledge entries can pin them: "for follow-up on this
investigation, resume `<handle>`".

GC is by *forgetting* at the application layer + substrate TTL at
the bbox/provider layer. Both work concurrently.

## 2. Architecture

Agents are not a new system. They are a manifest layer over existing
primitives.

| Layer | Existing | Agent-system addition |
|---|---|---|
| Persona prompt | brofile artifact | (unchanged; reused) |
| Filter rules | brofile artifact + dispatch filter chain | per-agent filter overlay (manifest field) |
| Spawn | `bro_exec` | `bro_agent_dispatch` is sugar that resolves agent → brofile + filters then calls `bro_exec` |
| Resume | `bro_resume` | (unchanged; resume by `AgentSession` handle — §1.2) |
| Discovery | none — brofiles addressed by name | `bro_agent_search` (semantic), `bro_agent_list` (registry browse) |
| Versioning | `bbox_artifact_install` / `bbox_artifact_supersede` | (unchanged; agents install through the same path with `kind="agent"`) |
| Provenance | per-artifact metadata | manifest's `provenance` field carries hand_authored vs distilled; agentic-corpus edges trace distillation back to source transcripts |

Storage:
- Agent artifacts are JSON, installed via
  `bbox_artifact_install(kind="agent", source=path)`. The catalog's
  `ArtifactKind` enum currently has variants `Workflow | Packet |
  Brofile`; it needs one more: `Agent`. This is a small additive
  change to `src/main.rs` and the artifact install dispatcher
  (validate, normalize, write to `artifacts/agent/<name>/`,
  manage supersession via `bbox_artifact_supersede`).
- Catalog stores normalized form under
  `$BLACKBOX_STATE_DIR/artifacts/agent/<name>.json` plus
  `agent/<name>/metadata.json` with version + supersession state
  (existing F4 catalog mechanics, unchanged).
- The manifest's embedding is computed at install time. The
  embedding bucket is **`agent_manifest`** — a new bucket added to
  the agentic-corpus per-bucket routing (§5.4 of that doc). Reasons:
  (a) the embedded text is composite (`description` + `when_to_use` +
  `anti_patterns` joined with structure), distinct from knowledge
  entry titles/bodies; (b) per-bucket route policy may diverge
  later (e.g., user wants a code-aware embedder for technical
  agents); (c) keeps the `knowledge` bucket clean. The bucket
  registration is additive to agentic-corpus §5.4's bucket table.
  Re-embed on every supersede (cheap).
- Re-activation / deactivation: `bbox_artifact_supersede` already
  manages "the active version of this name". An agent at version N
  superseding version N-1 leaves the embedding for N-1 in the
  vector store but marks it inactive; `bro_agent_search` filters to
  active versions only. Dispatching by bare name resolves to active.
  Callers can pin an exact version with `agent="name@v3"`.
- List behavior: `bro_agent_list` filters the artifact catalog by
  `kind="agent"` and `active=true` by default; pass
  `include_superseded=true` to surface history.

### 2.1 Layering

```
caller LLM (Claude / Codex / Gemini / OpenCode)
   │
   ▼  (MCP transport)
bbox daemon
   │
   ├─ bbox_*               ──► graph primitives
   ├─ bro_*                ──► generic dispatch
   │
   └─ bro_agent_*          ──► agent registry layer
        (src/orchestration/agents.rs)
            │
            ├─ list / search / describe ──► artifact catalog
            │   (kind=agent) + agentic-corpus vector store
            │
            └─ dispatch:
                 ├─ manifest.dispatch_adapter == null
                 │     └─ direct: resolve manifest → compose
                 │        brofile + filter overlay → bro_exec
                 │
                 └─ manifest.dispatch_adapter == <name>
                       └─ AgentAdapterRegistry::get(name)
                          → adapter.dispatch(manifest, args, ctx)
                          (adapter owns validation + spawn)
```

Three things to notice:
- The agent registry is a *projection* over the artifact catalog plus
  the vector store. No new persistent storage.
- Dispatch is sugar, not a new engine. `bro_agent_dispatch` returns
  `(task_id, session_id)` exactly like `bro_exec`. Resume is direct
  through `bro_resume`.
- Native Claude `.claude/agents/*.md` files are orthogonal. Bbox
  neither reads them nor ships in their format. If a user wants a
  Claude native agent available via bbox, they author a separate
  bbox agent JSON (or accept the divergence). v2 may add an
  optional one-way *import* path; v1 does not.

### 2.2 An agent vs. a brofile — and why a separate kind

| Property | Brofile | Agent |
|---|---|---|
| Persona prose | yes | inherited (via `brofile_ref`) or inline |
| Filter rules | yes | inherited + per-agent overlay |
| Discoverable by query | no | yes (semantic embedding) |
| Selection cue (when to use, anti-patterns) | no | yes |
| I/O contract | no | yes |
| Composition metadata | no | yes |
| Provenance traceable | basic (artifact catalog) | rich (manifest's `provenance`, agentic-corpus distillation edges) |
| Cost hint | no | yes |

Every agent has an underlying brofile (persona + filters). Not every
brofile is an agent — only those a user wants to make discoverable.
Existing brofiles continue to work as before; they don't auto-promote.

#### Why a separate `kind`, not richer brofile metadata?

Steelman of the alternative: extend brofiles with optional
`description` / `when_to_use` / `anti_patterns` / `inputs` /
`outputs` / `provenance` fields, add a `bro_brofile_search` for
semantic queries; skip the agent registry entirely.

Three reasons the doc rejects that:

1. **Opt-in discoverability.** Brofiles are persona definitions; many
   are internal scaffolding (per-workflow ensemble members,
   one-shot facilitators, project-specific tweaks of upstream
   personas). Forcing every brofile to declare manifest fields
   pollutes the brofile namespace and the search index. A separate
   `kind=agent` is the user's explicit "this should appear in the
   registry" signal.

2. **Many-manifests-to-one-brofile reuse.** A single brofile (e.g.
   `code-reviewer-persona`) can back multiple agents that differ
   only in cuing or filter overlay: `code-reviewer-strict`,
   `code-reviewer-quick`, `code-reviewer-security-only`. The
   manifest layer is where these distinct cuings live; the brofile
   underneath is shared. Collapsing into one `kind=brofile` would
   force either duplication of the persona prose or invent a new
   "brofile variant" concept — a separate `kind` is cleaner.

3. **Provenance asymmetry.** Brofile provenance is artifact-catalog-
   level (who installed it, what version). Agent provenance is
   richer (distilled vs hand-authored, evidence sessions, accept/
   reject feedback, agentic-corpus DERIVED_FROM edges). Bolting
   that onto every brofile entry is over-extension; making it a
   property of the agent manifest keeps the layering honest.

A user who has a one-off brofile and wants it semantically findable
can install both a brofile and a thin agent manifest pointing at
it. The cost is one small JSON file. The benefit: brofiles stay
focused on persona, agents stay focused on selection.

### 2.3 An agent vs. badgey

Badgey is a consultant-flavored agent with bespoke producer machinery:
- proposal store + apply state machine
- action journal for exactly-once side effects
- triage / closer specialized tools
- self-tuning learning loop

These are badgey-specific. Most agents won't need any of them. The
agent system provides:
- registry (badgey is in it)
- manifest (badgey has one)
- discovery (badgey is searchable)
- dispatch (badgey is dispatched via `bro_agent_dispatch`, which
  resolves to its brofile + filter overlay then calls `bro_exec` —
  the same path the existing badgey design uses)

Everything else in the badgey design stays. The badgey design doc
predates this one; semantically badgey IS an agent. Reading the
badgey doc now: every reference to "the badgey brofile" can be read
as "the badgey agent's brofile_ref". The badgey-impl skeleton's B1
phase (brofile artifacts) becomes "agent artifacts" once agent infra
lands; B2-B4 and downstream phases stay structurally identical.

## 3. Sources

- Claude Code's `.claude/agents/*.md` frontmatter convention — the
  *role* the manifest plays (selection metadata + cue), not the
  format.
- bbox brofile system (`src/orchestration/brofile.rs`,
  `apply_brofile_lens`) — agents wrap brofiles, do not replace them.
- `bro_exec` / `bro_resume` lifecycle (`src/orchestration/`) — agent
  dispatch is sugar over these.
- Artifact catalog (`bbox_artifact_install` / `_list` / `_supersede`,
  `src/main.rs`) — agents install through the same path with
  `kind="agent"`.
- `agentic-corpus` doc — vector store (§5.3-5.4) for manifest
  embeddings; entity refs (§6) for provenance edges.
- `badgey` doc — exemplar consultant-flavored agent; informed the
  manifest schema's `cost_class`, filter overlay, and provenance
  needs.
- Workflow `mcp_call` op (`src/workflow/ops.rs`) — agents are
  dispatch targets in workflow nodes via `bro_agent_dispatch`.

## 4. Manifest schema

The manifest is a JSON object stored in the agent artifact under the
`manifest` field. The full agent JSON has:

```json
{
  "kind": "agent",
  "name": "code-reviewer",
  "version": 3,
  "supersedes": "code-reviewer@v2",

  "manifest": {
    "description": "Reviews code for security and correctness.",
    "when_to_use": [
      "after writing or modifying code, before committing",
      "when the user explicitly asks for a review"
    ],
    "anti_patterns": [
      "do not use for one-line typo fixes",
      "do not use for greenfield design discussions"
    ],

    "brofile_ref": "code-reviewer-persona",
    "filter_overlay": {
      "allow": ["mcp__blackbox__bbox_*", "Read", "Grep", "Glob"],
      "disallow": ["Bash"]
    },

    "inputs": {
      "schema": {
        "type": "object",
        "properties": {
          "diff": { "type": "string", "description": "Unified diff to review" },
          "context_refs": { "type": "array", "items": { "type": "string" } }
        },
        "required": ["diff"]
      },
      "prompt_template": "Review the following diff:\n\n{{diff}}\n\nRelevant context:\n{{#each context_refs}}- {{this}}\n{{/each}}"
    },

    "outputs": {
      "schema": {
        "type": "object",
        "properties": {
          "verdict": { "enum": ["approve", "revise", "reject"] },
          "findings": { "type": "array" },
          "citations": { "type": "array" }
        }
      },
      "evidence_density": "high"
    },

    "composition": {
      "chainable_after": ["test-author", "diff-narrator"],
      "parallel_safe": true,
      "fan_out_aggregator": "vote-majority"
    },

    "cost_class": "normal",

    "dispatch_adapter": null,

    "provenance": {
      "kind": "hand_authored",
      "author": "user",
      "created_at": "2026-04-30T12:00:00Z"
    },

    "embedding": {
      "model": "voyage-code-3",
      "computed_at": "2026-04-30T12:00:00Z",
      "vector_ref": "agent_embed:code-reviewer:v3"
    }
  }
}
```

### 4.1 Field-by-field

**Header (artifact catalog conventions):**
- `kind` — always `"agent"`. Lets the catalog filter and route.
- `name` — stable display id. Globally unique within the catalog.
- `version` — monotonic int (per F4 catalog convention).
- `supersedes` — optional pointer to prior version.

**Selection cuing (the Claude-frontmatter analog):**
- `description` — single sentence; primary input to the embedding.
  Required.
- `when_to_use` — array of positive cues. Each item is a short phrase
  or sentence. The `bro_agent_search` ranking weights these.
- `anti_patterns` — array of negative cues. Used both for caller
  display ("do not use for X") and to *down*-weight queries that
  match anti-patterns more strongly than they match `when_to_use`.

**Persona binding:**
- `brofile_ref` — name of a separately-installed brofile artifact.
  When dispatched, the agent's filter chain is composed from this
  brofile + the manifest's `filter_overlay`.
- OR `brofile_inline` — full inline brofile body, for agents that
  don't share persona with anything else. Mutually exclusive with
  `brofile_ref`.
- `filter_overlay` — additional `allow` / `disallow` patterns merged
  on top of the brofile's filters at dispatch time. Tightening the
  surface for a specific role.

**I/O contract:**
- `inputs.schema` — JSON Schema describing the expected shape of the
  caller's prompt args. Used by `bro_agent_dispatch` to validate args
  before spawning the bro.
- `inputs.prompt_template` — Handlebars-shaped (or simpler) template
  expanded with the args to produce the actual first-turn prompt.
  Optional; if absent, args are JSON-stringified and passed verbatim.
- `outputs.schema` — JSON Schema for the agent's expected return.
  Currently advisory (no enforcement); used by composition chains to
  validate downstream consumers and by eval to compare against gold.
- `outputs.evidence_density` — `low | medium | high`. Hint to
  composition: a `high` agent returns rich citations
  (EvidenceBundle-shaped); a `low` agent returns plain text. Affects
  whether downstream nodes can rely on structured fields.

**Composition:**
- `composition.chainable_after` — array of agent names this agent
  expects to consume from. Used by `bro_agent_compose` to suggest
  chains and by workflow authors to validate.
- `composition.parallel_safe` — bool. Can N copies run concurrently
  against the same project_dir? (False, e.g., for agents that
  mutate shared state.)
- `composition.fan_out_aggregator` — when used in a fan-out, how
  should results be combined: `vote-majority`, `ensemble-merge`,
  `first-success`, or a named workflow node id.

**Cost:**
- `cost_class` — `cheap | normal | expensive`. Drives escalation
  patterns and per-scope budget reporting. Advisory.

**Dispatch adapter:**
- `dispatch_adapter` — optional string naming a daemon-registered
  dispatch interceptor. Default null. When set, `bro_agent_dispatch`
  routes through the named adapter instead of taking the standard
  direct path. v1 registers exactly one adapter (`"badgey"`) for
  the consultant-flavored badgey agent (§11.4). Most agents leave
  this null.

**Provenance:**
- `provenance.kind` — `hand_authored | distilled | imported`.
- For `distilled`: includes `distilled_by` (badgey instance id),
  `evidence_session_ids[]`, `accept_count`, `reject_count`,
  `created_from_threads[]`. Traceable back to the corpus instances
  that motivated the agent.
- For `imported`: includes `source` (`claude-native | from-repo` etc.)
  and `import_at`.

**Embedding:**
- `embedding.model` / `vector_ref` — managed by the agentic-corpus
  vector store. Re-computed on every supersede; v2 considers
  re-computing on `description` field changes only (cheap; not
  load-bearing for v1).

### 4.2 What's NOT in the manifest

Deliberately omitted to keep v1 lean:

- **Lifecycle hint** (`oneshot | session_cached | consultant`). The
  substrate's session lifecycle (§1.2) is sufficient. Adding a hint
  would either (a) be advisory and ignored or (b) imply enforcement
  the substrate doesn't do. Skip until concrete need emerges.
- **Token budget** per agent. Per-scope and per-instance budgets at
  the badgey level (badgey doc §13.2) cover the consultant case.
  Generic agents inherit `bro_*` defaults.
- **Model preferences** (Claude's `model: sonnet`). Provider-specific
  hints don't belong in a cross-provider abstraction. v1 takes
  model / provider / effort from the brofile (the existing brofile
  artifact already carries these); the manifest stays
  provider-agnostic. There is no model-override arg on
  `bro_agent_dispatch` in v1.
- **Tool list** (Claude's `tools:`). The filter overlay covers this
  more cleanly — overlays compose with brofile filters, tool lists
  fight with them.

### 4.3 Manifest validation at install

`bbox_artifact_install(kind="agent", source=path)`:

1. **Parse JSON.** Reject with `error.bad_input(code=invalid_json)`.
2. **Schema validate** against the agent JSON schema (fixed file at
   `schema/agent.schema.json`). Reject with structured errors.
3. **`brofile_ref` XOR `brofile_inline`** — exactly one. Reject if
   both or neither are present.
4. **Brofile resolution.**
   - If `brofile_ref`: confirm the named brofile artifact exists and
     is active in the catalog. Reject if not.
   - If `brofile_inline`: validate the inline body against the brofile
     schema (persona text length, filter pattern syntax, declared
     `provider` is recognized).
5. **Lint manifest fields:**
   - `description`: length >= 10, <= 500 chars.
   - `when_to_use`: array, non-empty, each item <= 200 chars.
   - `anti_patterns`: array (may be empty); each item <= 200 chars.
   - `inputs.schema`: valid JSON Schema 2020-12 (validated by a
     bundled JSON-Schema-of-JSON-Schema check).
   - `outputs.schema`: same.
   - `inputs.prompt_template`: parseable by the chosen template
     engine (Handlebars-shaped per §5.3); reject on syntax error.
   - `filter_overlay`: each pattern matches the existing MCP
     filter-pattern grammar (`mcp__<server>__<tool>`, glob suffixes
     allowed); normalize for the canonical / dotted form mismatch
     (the bbox filter chain accepts both; agent install picks one
     and stores it).
   - `cost_class`: enum membership.
   - `provenance.kind`: enum membership; field-set matches the kind
     (e.g. `distilled` requires `distilled_by` and at least one
     evidence ref).
   - `dispatch_adapter`: if non-null, must match a daemon-registered
     adapter name; reject otherwise.
6. **Filter overlay sanity check.** Compute the merged filter set
   against the resolved brofile. If the overlay's `allow` patterns
   include tools the brofile's filters explicitly deny, emit a
   warning (not error) and record in install metadata; the deny-wins
   merge will still strip those tools at dispatch.
7. **Compute embedding** via the `agent_manifest` bucket (§2 storage).
   On embedding-provider unavailability, write the artifact with
   `degraded.embedding_pending=true` per agentic-corpus §4.4 —
   install proceeds; agent is invisible to `bro_agent_search` until
   the queued embed lands.
8. **Write normalized form + metadata.** Atomic via tempfile + rename.
9. **Write provenance edges.** If `provenance.kind == "distilled"`,
   write `DERIVED_FROM` edges to the EdgeIndex per §8.1.
10. **Return install result** with the agent's stable name + version.

Steps 1-6 are pure validation; nothing on disk changes if any reject.
Steps 7-10 are commit-phase; partial commit on a crash mid-7-10 is
recovered by the artifact catalog's existing F4 metadata sweep.

## 5. MCP surface

Five new tools.

### 5.1 Tool inventory

| Tool | Purpose |
|---|---|
| `bro_agent_list` | Browse installed agents, optionally filtered by tag/cost/provenance. |
| `bro_agent_search` | Semantic search by query. Returns ranked manifests with similarity scores. |
| `bro_agent_describe` | Full manifest + resolved brofile + merged filters for one agent. |
| `bro_agent_dispatch` | Dispatch an agent. Returns a portable `AgentSession` handle. |

Resume / status / cancel reuse the existing `bro_resume` /
`bro_status` / `bro_cancel` — agent dispatch returns standard bro
session metadata wrapped in the portable handle (§5.3). No agent-
specific resume tool.

Hand-authoring agents in v1 goes through the standard
`bbox_artifact_install(kind="agent", source=path)` path. There is no
dedicated `bro_agent_propose` tool in v1 — distillation lives inside
badgey (BadgeyProposalStore handles `kind=Agent` proposals; see §11).
A user without badgey installed authors a JSON file directly and
runs `bbox_artifact_install`. v2 may introduce a generic
`AgentProposalStore` if a non-badgey distillation source emerges
(see §16 OQ).

### 5.2 `bro_agent_search` — the load-bearing primitive

```
bro_agent_search(
  query: String,
  top_k: u32 = 5,
  filter: Option<{ cost_class?: ..., provenance?: ... }>,
  exclude_anti_pattern_matches: bool = true,
)
→
{
  "results": [
    {
      "agent": "code-reviewer",
      "version": 3,
      "similarity": 0.87,
      "description": "...",
      "when_to_use": [...],
      "anti_patterns": [...],
      "cost_class": "normal",
      "matched_anti_patterns": []   // populated only if exclude=false
    },
    ...
  ],
  "vector_status": { ... }
}
```

Ranking:
- cosine similarity between query embedding and manifest embedding
- penalize results where the query embeds *more closely* with an
  anti-pattern than with `when_to_use` (configurable threshold)
- tiebreak by `cost_class` ascending (cheap first)

### 5.3 `bro_agent_dispatch`

```
bro_agent_dispatch(
  agent: String,                   // name; resolves to latest version
  args: Object,                    // validated against manifest.inputs.schema
  project_dir: Option<String>,
  bro: Option<String>,             // named bro instance
  ambient: Option<Object>,         // forwarded to scope-bind
)
→
{
  "session": AgentSession,         // §1.2 portable handle
  "task_id": "...",                // also in session.task_id
  "resolved_brofile": "code-reviewer-persona@v2",
  "merged_filters": { ... }
}
```

Validation:
- `args` must conform to `manifest.inputs.schema`; otherwise
  `error.bad_input` with field-level details.
- Filter merging: brofile filters ∪ manifest overlay; conflicts
  resolved by deny-wins. (Existing filter chain supports per-dispatch
  overlays; no substrate change required.)
- Calls `bro_exec(prompt=expand_template(args), project_dir, ...)`
  with the merged filter chain.

Provider selection is determined by the brofile's declared provider
(`brofile.provider`) — `bro_agent_dispatch` does not carry a
`provider_override`. v1 agents are bound to a single provider via
their brofile_ref; multi-provider agents are a v2 question (see
§9.2).

Session attribution: the bbox `TaskInner` carries `bro_label` for
routing attribution (`<team>::<member>` or bare brofile name) and
`agent_label` for agent attribution (`agent:<name>@v<version>`).
Agent dispatch sets both at construction time. `record_task_to_bro`
may overwrite `bro_label` for team routing, but `agent_label` is
immutable after construction. `bro_status` and `bro_dashboard`
emit both `broLabel` and `agentLabel` so callers can distinguish
agent-initiated tasks from direct dispatches.

To resume from a stored handle:

```
bro_resume(session_id=session.session_id,
           provider=session.provider,
           prompt=...)        // existing tool, takes both fields
```

Or via named bro: if `bro_agent_dispatch` was called with `bro=...`,
subsequent `bro_resume(bro=name)` works without the explicit
provider arg. The named-bro path is the most ergonomic for
in-session continuation; the explicit `(session_id, provider)` path
is what handle-portability uses.

### 5.4 Tool descriptions — cuing the protocol

Per-tool descriptions at the MCP level carry the same
behavioral-cuing pattern as `bbox_*` (agentic-corpus §4.2). Concrete
example for `bro_agent_dispatch`:

```
Dispatch a registered agent for a focused task. Returns a bro
(task_id, session_id) — resume with bro_resume, status with
bro_status. Prefer over hand-rolling a brofile + bro_exec when:
  1) the task matches an agent's `description` and `when_to_use`
  2) you want the agent's filter overlay (e.g. read-only access)
  3) the result will be consumed by another agent in a chain

Anti-pattern: do not bro_agent_dispatch when the agent's manifest
declares one of your task's properties as an anti_pattern. Use
bro_agent_search to discover candidates first.

After dispatch, treat the returned `AgentSession` handle as portable.
Posting the full handle in a thread or knowledge entry lets future
callers resume the session. Storing only `session_id` is a hazard
(§1.2) — `bro_resume` requires both `session_id + provider`.
```

## 6. Discovery

### 6.1 Embedding pipeline

Manifest embeddings ride the agentic-corpus vector store
(`design/corpus/agentic-corpus/agentic-corpus.md` §5.3-5.4). Bucket: `agent_manifest`
(new bucket; required substrate extension per §16.1). Field
embedded:

```
{description}\n\nWhen to use:\n- {when_to_use[0]}\n- ...\n\nAnti-patterns:\n- {anti_patterns[0]}\n- ...
```

Embed at install time. Re-embed at supersede. Re-embeds are cheap (low
volume; one HTTP call per install). No batching required for v1.

### 6.2 Search query shape

The query passed to `bro_agent_search` is the caller's task prompt or
a summarized form. Embedding the full prompt is fine — the cosine
geometry tolerates noise. Shorter, intent-focused queries (e.g.
`"review this diff for SQL injection"`) score better than verbose
contextual ones.

### 6.3 Cuing patterns

Two flavors of how search results reach an orchestrator:

**Passive (default).** Caller calls `bro_agent_search` directly,
inspects results, decides whether to dispatch. No daemon-side
injection.

**Active (opt-in via brofile lens).** A brofile lens that wants its
bro to consider agents for sub-tasks includes a directive like:

```
Before drafting a sub-task plan, call bro_agent_search with the
sub-task description as the query. If a result returns with
similarity > 0.7 and no anti-pattern match, prefer dispatching
that agent over doing the sub-task yourself.
```

The lens is text — no daemon-side enforcement. This is how badgey,
workflow-authoring bros, and ensemble facilitators get agent-aware
behavior without coupling.

**Workflow-injection.** Workflow nodes can include an `on_enter`
hook that calls `bro_agent_search` and stuffs results into a vars
slot the node prompt references:

```json
"on_enter": [{
  "op": "mcp_call",
  "args": {
    "server": "blackbox",
    "tool": "bro_agent_search",
    "arguments": { "query": "${vars.sub_task}", "top_k": 3 }
  },
  "into_var": "candidates"
}],
"prompt": "Sub-task: ${vars.sub_task}\n\nCandidate agents:\n${candidates.results}\n\nDispatch a candidate or do the work yourself."
```

Same data-flow pattern as `badgey_ask` integration in the badgey doc
§10.1.

## 7. Composition

Three primitive shapes, all expressible in the existing workflow
engine. The agent system contributes manifest fields that make
composition declarative rather than ad-hoc.

### 7.1 Chain

`A → B`: A's output becomes (part of) B's input.

Manifest support:
- A declares `outputs.schema` (B can validate at compose time)
- B declares `composition.chainable_after: [A]` (suggests the chain)
- `bro_agent_compose(shape="chain", agents=["A","B"], initial_args=...)`
  generates a tiny workflow spec on the fly via
  `bro_orchestrate_author`, dispatches via `bro_orchestrate_run`.
  Returns the final agent's session_id for follow-up.

### 7.2 Fan-out

Same prompt to N agents in parallel; aggregate.

Manifest support:
- All N declare `composition.parallel_safe: true`
- Each declares `outputs.evidence_density` (high-density fan-outs
  produce richer aggregate)
- The `fan_out_aggregator` field on each names how to merge:
  `vote-majority` for classification-style outputs,
  `ensemble-merge` for narrative bundles,
  `first-success` for racing patterns

`bro_agent_compose(shape="fan_out", agents=[...], aggregator="vote-majority", prompt=...)`
returns aggregated output. Underlying implementation uses workflow
ensemble nodes.

### 7.3 Escalation

Try cheap; fall back to expensive on `degraded.unsuitable_for_task`
or low-confidence output.

Manifest support:
- Both declare matching `outputs.schema`
- Cheap agent has `cost_class: cheap`; expensive has
  `cost_class: expensive`
- Escalation logic checks the cheap output's structured fields
  (e.g. `confidence < 0.6`); if degraded, dispatches expensive

`bro_agent_compose(shape="escalation", cheap=A, expensive=B, prompt=...)`.

### 7.4 Composition primitives are sugar

`bro_agent_compose` could be deferred to v2 — every shape is
expressible by hand-authoring a workflow that calls
`bro_agent_dispatch` per node. v1 ships the manifest fields that
*support* composition; sugar tools that *generate* composition
workflows are nice-to-have.

## 8. Provenance + distillation

### 8.1 Provenance traceability

Every installed agent's `manifest.provenance` answers: where did this
come from?

`hand_authored`:
- `author`, `created_at`. That's it — for agents users wrote
  themselves.

`distilled`:
- `distilled_by`: badgey instance id
- `evidence_session_ids[]`: transcript sessions backing the proposal
- `created_from_threads[]`: threads where the recurring pattern lived
- `accept_count`, `reject_count`: tracked across the agent's life

When an agent's `provenance.kind == "distilled"`, the agentic-corpus
graph carries `DERIVED_FROM` edges from `agent:<name>@<version>` to
each session/thread in the evidence list. `bbox_blame` and `badgey
explain <agent>` can walk these — narrating *why* this agent exists.

This is the high-leverage thing. Hand-curated agent registries rot
silently as the corpus changes. A distillation-traceable registry
can be re-evaluated against a fresh corpus.

#### Substrate extensions (REQUIRED, not OQ)

Provenance traceability requires three concrete extensions to
agentic-corpus that this doc commits to as substrate dependencies:

1. **`agent` entity type** — added to `agentic-corpus.md` §6.1 entity
   types table. Backing store: artifact catalog (`kind="agent"`).
2. **EntityRef variant `Agent { name: String, version: u32 }`** —
   added to `agentic-corpus.md` §6.2 entity-ref grammar with
   serialized form `agent:<name>@v<version>`. Round-trip-stable.
3. **Durable `DERIVED_FROM` edge with `agent` source target** — the
   existing `DERIVED_FROM` edge kind in `agentic-corpus.md` §9
   already supports cross-type sources. The extension is including
   `agent` in the EdgeIndex's source-type allowlist and persisting
   distillation edges in the per-project edge sidecar
   (`agentic-corpus.md` §5.5 EdgeIndex sidecar).

These are small additive substrate changes, not new edge types. They
must land before agent provenance is queryable; they are not optional.

### 8.2 Distillation loop

Badgey's distillation pipeline. Each step names its concrete substrate
primitive; v1 does not introduce new primitives, only composes
existing ones.

**Task-shape unit.** The mining unit is a **user-turn-with-context**:
a single user prompt extracted from a transcript, paired with the
preceding session metadata (project_id, brofile, prior tool calls).
Extraction source: `bbox_search(role="user", project=...)` with
filters; output is a stream of `(session_id, turn_idx, prompt_text,
context)` records.

**Embedding storage.** User-turn embeddings already exist as part of
the agentic-corpus `transcripts` bucket (§5.4 of that doc).
Distillation reads from this bucket; no new embeddings needed.

**Clustering job.** Run as a cron-installed badgey arc. Algorithm:
1. fetch user-turn embeddings for the project from the
   `transcripts` bucket via a new substrate primitive
   `bbox_embed_iterate(bucket, project_id, since)` that yields
   `(entity_ref, vector)` pairs. (This primitive does not exist in
   agentic-corpus today; flagged as substrate dependency below.)
2. cluster via online HNSW-neighborhood mining: for each turn, find
   k-nearest neighbors at cosine similarity >= 0.85; group via
   union-find. (Reuses agentic-corpus HNSW index, no new structures.)
3. filter clusters: size N >= 5, time-range >= 14 days,
   user-prompt-uniformity >= 0.7.
4. for each surviving cluster, score against installed agent
   manifests via `bro_agent_search(query=cluster.centroid_summary)`;
   discard clusters where the top result has similarity >= 0.7
   (already-served).

**Definitions:**
- *user-prompt-uniformity*: 1 - (standard deviation of pairwise
  cosine distances within cluster). High uniformity means tight
  cluster; low means scattered. Threshold tuned via badgey
  self-audit.
- *centroid_summary*: a synthesized description of the cluster's
  recurring intent. Generated by an LLM call (badgey itself, in a
  separate turn) reading 3-5 representative cluster members.
- *time-range >= 14d*: filters out short-lived patterns.

**Substrate dependency:** `bbox_embed_iterate(bucket, project_id,
since)` is not in agentic-corpus today. v1 distillation requires it.
The implementation is a thin wrapper over the existing vector store;
not a new index. Documented as a substrate dependency for the agent
infra impl skeleton (TBD).

**Drafting (per surviving cluster).**
1. badgey calls itself in a sub-turn with the cluster summary +
   representative members; produces:
   - draft `manifest` (description from cluster summary,
     `when_to_use` from observed task framings, `anti_patterns`
     left empty initially — populated only via accept/reject
     feedback)
   - draft `brofile_inline` (persona text inferred from typical
     successful responses in the cluster, plus standard filter
     overlay)
   - evidence bundle (transcript session_ids, count, time-range,
     centroid_summary, threads-spanned)
2. Emits `bg-action-emit-proposal` with `kind=Agent`. Lands in
   BadgeyProposalStore as `pending`.
3. User reviews via `badgey_resume(id, "describe P-N")`; sees the
   manifest + evidence summary. Approves, rejects, or edits.
4. On apply: `bbox_artifact_install(kind="agent", ...)` lands the
   manifest + brofile in the catalog. Embedding computed against
   `agent_manifest` bucket. Provenance edges (`agent → session`
   `DERIVED_FROM`) written to the EdgeIndex per §8.1 substrate
   extensions.

### 8.3 Distillation feedback

Per agent, badgey tracks `accept_count` (times dispatched and useful)
and `reject_count` (times dispatched and refused / re-routed).
Aggregation source: `bro_status` events plus optional structured
"agent-feedback" notes the calling user/orchestrator can post.

When `reject_count / accept_count > threshold`, badgey emits a
proposal: `propose-agent-deprecation` (a sub-kind of
`propose-artifact-promotion` with target = retire the agent).

The loop closes: agents that don't earn their keep get retired, with
evidence.

## 9. Cross-provider parity

### 9.1 Native vs registered

Claude has the Task tool: native, ephemeral, opaque. It dispatches
specialized Claude-built agents (general-purpose, statusline-setup,
etc.) plus user `.claude/agents/*.md` files. Selection is internal
to Claude.

bbox agents are: project-installed, MCP-discoverable, traceable.

Claude users can use both. The recommendation in cuing:
- Reach for native Task for ephemeral built-in agents (Explore,
  general-purpose, code-reviewer-as-shipped-by-Anthropic).
- Reach for `bro_agent_dispatch` for project-installed agents,
  agents that should be callable across providers, or anything
  distilled by badgey.

Anthropic-shipped Claude built-ins (Explore, general-purpose) are
not duplicated in bbox. They remain Claude-only.

### 9.2 Cross-provider semantics — v1 limitations

A manifest's selection cues (`description` / `when_to_use` /
`anti_patterns`) must be written in provider-agnostic English. Don't
say "use this when running Claude Code"; say "use this when reviewing
a Rust pull request". Embedding lives on this prose; provider-neutral
language ranks better across the registry.

The brofile, however, IS provider-bound. Brofile artifacts in the
existing catalog declare `provider`, `model`, `effort_tier`, and
provider-specific environment hints. These are not portable across
provider families; a Claude-tuned brofile won't run cleanly under
Codex, and vice versa.

**v1 commits to single-provider agents.** A given agent
(`agent:foo@v1`) binds to one provider via its `brofile_ref`. Any
caller (Claude, Codex, Gemini, OpenCode) can *invoke*
`bro_agent_dispatch(agent="foo")`; the dispatch spawns under the
brofile's declared provider regardless of caller provider. The
caller's MCP transport is provider-agnostic; the spawned bro is not.

**There is no `provider_override` arg in v1.** Earlier drafts of
this doc claimed one; that was hand-waving. Existing brofile
resolution does not support cleanly substituting the provider while
keeping model/effort/env settings consistent — the override would
need a parallel resolution path that v1 does not implement.

**Multi-provider agents are a v2 concern.** The natural shape is
*multiple brofile_refs per manifest*, one per provider family, with
the dispatcher selecting based on caller provider:

```json
"brofile_refs_by_provider": {
  "claude": "code-reviewer-claude",
  "codex": "code-reviewer-codex",
  "gemini": "code-reviewer-gemini"
}
```

Out of v1 scope; flagged in §16 OQ. Implication for v1 callers: an
agent authored against the Codex provider is callable from any
provider, but always runs under Codex. Cross-provider ergonomics
mean the *caller* is provider-agnostic, the *executing bro* is not.

### 9.3 `.claude/agents/*.md` interop

v1: no interop. The two registries are independent. A user who wants
both maintains both.

v2 (deferred): one-way *import* tool that reads
`.claude/agents/*.md`, generates a synthetic manifest (description
from `description` field, when_to_use synthesized from `description`
+ `name`, anti_patterns empty), drops it in the bbox registry as
`provenance.kind = imported`. Out of scope for v1.

## 10. Relationship to existing systems

| System | Relationship |
|---|---|
| Brofile | Agents wrap brofiles via `brofile_ref` (or inline). Brofiles continue to exist independently; not all brofiles are agents. |
| Badgey | Badgey is a consultant-flavored agent. Its design doc + impl skeleton are valid as-is; agent infra lands first, badgey artifacts re-shape to use the manifest layer. See §11. |
| Workflow engine | Agents are dispatch targets in workflow nodes via `bro_agent_dispatch`. Composition primitives (§7) generate workflows. |
| Artifact catalog | Agents install through `bbox_artifact_install(kind="agent")`, same path as workflow / packet / brofile. F4 supersession unchanged. |
| Agentic-corpus | Manifest embeddings live in a new `agent_manifest` bucket (§2 storage; required substrate extension §16.1). Provenance edges (DERIVED_FROM) extend the entity graph. `agent` becomes a new entity type (§6.1 of agentic-corpus.md). |
| Threads | `AgentSession` handles are portable. Posting one in a thread = handing off agent state. |
| Knowledge entries | Same — store an `AgentSession` (not bare `session_id`) in a `bbox_remember` body for later resume. |

## 11. Migration: badgey under agent infra

The badgey design doc + impl skeleton converged before this doc.
Agent infra is a load-bearing change to badgey's substrate, not a
re-framing. The badgey doc + impl skeleton both need targeted edits.
This section enumerates them.

### 11.1 Concrete deltas in `badgey.md`

| Section | Required edit |
|---|---|
| §1 thesis / §2.1 layering | Add: "badgey is a consultant-flavored agent (`agent:badgey@v1`); see `agent-system.md` for the broader registry." |
| §2.3 cosession framing | Reframe lifecycle in terms of `agent-system.md` §1.2 portable `AgentSession` handle. The wrapper's `badgey_id ↔ session_id` mapping becomes `badgey_id ↔ AgentSession`. |
| §6.5 producer-side recursion | Add fifth proposal kind: `Agent` (manifest + brofile draft). Reference `agent-system.md` §8.2 distillation pipeline. |
| §7 brofile lens | Reframe as "the badgey agent's brofile binding" — same content; the brofile is now referenced from the badgey manifest via `brofile_ref`. |
| §10 boundaries | Add: "NOT the agent system. Badgey is one consultant-flavored citizen; `bro_agent_*` is the broader surface." |
| §16 OQ #1 (lens-flavored variants) | Resolve: lens-flavored variants are *separate agents* sharing a `brofile_ref` with different manifest cuing, not multi-lens-on-one-badgey. Move from OQ to design decision. |
| §16 OQ #2 (idle eviction) | Resolve: no daemon-side eviction. Lifecycle is substrate-bounded TaskStore TTL (24h) + provider session TTL. Per `agent-system.md` §1.2. |

### 11.2 Concrete deltas in `badgey-impl.md`

| Phase | Required edit |
|---|---|
| B1 (brofile artifacts + filter wiring) | Splits into B1a (brofile artifacts: `badgey-persona.json`, `badgey-scout-persona.json`) and B1b (agent manifests: `badgey.json`, `badgey-scout.json`, each referencing the matching brofile via `brofile_ref`). |
| B2 (types) | Add `ProposalKind::Agent` variant; adjust `BadgeyProposal` to carry agent-manifest payload for that kind. |
| W6 (apply executor) | Add dispatch case for `kind=Agent` → `bbox_artifact_install(kind="agent", source=draft_path)`. |
| M1 (lifecycle MCP tools) | `badgey_exec` becomes thin sugar over `bro_agent_dispatch(agent="badgey", ...)`. The wrapper still owns the `badgey_id` friendly name → `AgentSession` mapping. |
| New phase A0 (substrate dep) | Agent infra phases (separate impl skeleton, not yet written) land before any badgey phase. B1 cannot proceed without `bbox_artifact_install` accepting `kind="agent"`. |
| §16 (OQ summary at end of impl) | Update: agent-system phases are upstream of every badgey phase. Critical path begins with agent infra. |

### 11.3 What stays badgey-specific

- **BadgeyProposalStore** stays badgey-side. Generic agent
  installation goes through `bbox_artifact_install` directly; only
  badgey's *staged* proposals (with the apply state machine, action
  journal, retry semantics) need this storage. v2 may promote to
  `AgentSideStore` if a non-badgey distillation source emerges.
- **Action journal** stays badgey-side for the same reason.
- **`badgey_triage_inbox` / `badgey_close_loops`** stay distinct
  tools; they encapsulate consultant-mode-specific orchestration
  patterns that don't generalize to all agents.
- **`badgey-scout`** is now an agent (`agent:badgey-scout@v1`) but
  its dispatch path remains internal to badgey's wrapper (see
  badgey §6.3 sub-bro pattern). External callers don't typically
  invoke `bro_agent_dispatch(agent="badgey-scout", ...)` directly;
  it's a private collaborator surface.

### 11.4 Dispatch adapter mechanism

A naive `bro_agent_dispatch(agent="badgey")` would *not* converge
with `badgey_exec`: generic dispatch resolves manifest → brofile →
`bro_exec` and bypasses badgey wrapper machinery (the `badgey_id`
registry, thread-of-record, BadgeyProposalStore, action journal).
The two paths would diverge into two separate badgey flavors.

Fix: agent manifests can declare a **dispatch adapter** — a daemon-
registered hook that intercepts `bro_agent_dispatch` for agents
that need wrapper machinery. Manifest field:

```json
"dispatch_adapter": "badgey"   // optional; default null = direct dispatch
```

#### `AgentDispatchAdapter` interface

```rust
pub trait AgentDispatchAdapter: Send + Sync {
    /// Stable name; matches the manifest's `dispatch_adapter` field.
    fn name(&self) -> &'static str;

    /// Called BEFORE generic dispatch validation. The adapter takes
    /// full ownership of input validation, prompt template
    /// expansion, brofile resolution, filter merging, and the
    /// underlying spawn call. The agent-system layer does NOT
    /// pre-validate args against `manifest.inputs.schema` — the
    /// adapter is responsible if it cares.
    async fn dispatch(
        &self,
        manifest: &AgentManifest,
        args: serde_json::Value,
        ctx: DispatchContext,        // project_dir, ambient, bro_label, etc.
    ) -> Result<AgentDispatchResult, AgentDispatchError>;
}

pub struct AgentDispatchResult {
    pub session: AgentSession,
    pub task_id: TaskId,
    pub resolved_brofile: BrofileRef,
    pub merged_filters: MergedFilters,
    pub degraded: Option<DegradedInfo>,
}

pub enum AgentDispatchError {
    BadInput { code: String, field: Option<String>, message: String },
    NotFound { ref_: String },
    AdapterFailed { code: String, message: String },
}
```

The adapter returns the same shape `bro_agent_dispatch` returns to
callers (so the wrapper layer's response shape is identical
regardless of adapter or direct path).

#### Adapter registry + availability semantics

- Registry lives in `daemon::AgentAdapterRegistry`. Adapters
  register themselves at daemon startup, BEFORE the artifact
  catalog opens for validation. Init order is enforced in `main()`.
- Install-time validation (§4.3 step 5): if `dispatch_adapter` is
  non-null, registry MUST contain an adapter with that name; reject
  install otherwise with `error.bad_input(code=adapter_unknown)`.
- Restart with an installed agent whose adapter is no longer
  registered: hard fail at dispatch time with
  `error.bad_input(code=adapter_unavailable, agent="<name>")`.
  **No fallback to direct path.** Falling back would silently lose
  the wrapper machinery the manifest declared a dependency on.
- Adapter authors are responsible for graceful degradation INSIDE
  the adapter (e.g., badgey adapter handling a wrapper-init failure
  by returning `AdapterFailed`).

#### Direct path (null adapter)

When `dispatch_adapter == null`:
1. agent-system layer validates `args` against `manifest.inputs.schema`
2. expands `manifest.inputs.prompt_template` with args
3. resolves brofile_ref (or uses brofile_inline)
4. merges filters (brofile + manifest overlay; deny-wins)
5. calls `bro_exec(prompt, project_dir, brofile, filters, ...)`
6. wraps result in `AgentSession` and returns

The direct path is the agent-system layer's reference implementation;
adapters re-implement steps 1-5 if they need different validation or
spawn semantics.

#### Caller-facing convergence

This means there's exactly one badgey instance per `badgey_id`,
reachable from either `badgey_exec` or `bro_agent_dispatch`. The
two surfaces converge at the wrapper, not at `bro_exec`.

Outside callers' choice:
- `badgey_exec` / `badgey_resume`: badgey-specific surface for
  consultant flows (triage, scout, proposals). Returns friendly
  `badgey_id` + `AgentSession`.
- `bro_agent_dispatch(agent="badgey", args={"prompt":"...",
  "badgey_id"?:"existing"})`: generic surface. Routes through the
  badgey adapter. Returns `AgentSession` carrying the same
  underlying provider session as `badgey_exec` would. Optional
  `args.badgey_id` resumes a known instance; absent, creates new.

Adapters are a v1 extension point with one shipped instance
(`"badgey"`). v2 may add adapters for other consultant-flavored
agents. Default for new agents is null adapter (direct path).

## 12. Failure modes

### 12.1 Manifest drift

Failure shape: the manifest's `description` and `when_to_use` no
longer match what the agent actually does after a brofile rewrite.
Discovery surfaces it for queries it doesn't really serve.

Mitigation:
- Eval suite per agent (see §13). Regressions show up in pass-rate.
- `bro_agent_describe` includes a `last_brofile_revision` field; if
  the brofile was superseded but the agent manifest wasn't,
  `degraded.manifest_stale=true` surfaces.
- Distilled agents have evidence sessions; if the recent dispatch
  results no longer match the evidence shape, badgey can flag.

### 12.2 Embedding poisoning

Failure shape: an agent author crafts `description` keywords to
surface for queries the agent shouldn't serve (over-broad
positioning).

Mitigation:
- Anti-pattern penalty in ranking (§5.2): if the query embeds *more
  closely* with anti-patterns than `when_to_use`, downrank.
- Eval suite includes adversarial queries (negative examples) that
  the agent should NOT match.
- `bro_agent_describe` exposes the embedding's `vector_ref` for
  external audit / re-embed.

### 12.3 Session leak

Failure shape: an `AgentSession` handle intended for one user leaks (posted in a
shared knowledge entry) and another user resumes it, picking up
sensitive context.

Mitigation:
- Single-machine, single-user assumption (substrate). Same posture
  as the existing bbox knowledge store and bro registry.
- v1: documented limitation. Don't post sensitive `AgentSession` handles in
  shared stores.
- v2 (deferred): per-session ACL. Out of scope.

### 12.4 Distillation drift

Failure shape: badgey distills an agent that pattern-matches on a
transient corpus phenomenon; the agent ages poorly and surfaces for
unrelated queries.

Mitigation:
- Eval pass-rate trend per agent. Auto-deprecation proposal when
  pass-rate falls > 5% from baseline (§8.3).
- Distilled agents carry `evidence.time_range` in provenance.
  Periodic badgey self-audit re-clusters the corpus and checks
  whether the original evidence cluster still exists. If it
  evaporated, propose deprecation.

### 12.5 Filter overlay vs brofile conflict

Failure shape: an agent's `filter_overlay.allow` includes a tool the
underlying brofile's filters deny. Caller dispatches; the merged
filter chain (deny-wins) silently strips the tool.

Mitigation:
- Install-time validation: warn (not error) if `filter_overlay`
  contains allows that brofile's filters explicitly deny.
- `bro_agent_describe` returns `merged_filters` field showing the
  effective post-merge filter chain. Inspectable.

### 12.6 Cross-provider brofile mismatch

Failure shape: agent's brofile is authored for Claude; caller is
running Codex; the call goes through MCP fine but the brofile prompt
might be tuned for Claude phrasing.

Mitigation (v1):
- Per §9.2, v1 dispatches under the brofile's declared provider
  regardless of caller provider. The Codex caller invokes
  `bro_agent_dispatch(agent="claude-tuned-agent")` and the spawned
  bro runs under Claude. The CALLER stays provider-agnostic; the
  EXECUTING bro is bound by brofile.
- This is fine for ergonomics: the caller doesn't need to care.
- It's not fine if the user wants the agent's WORK to happen on
  their preferred provider (e.g., for billing or capability
  reasons). v1 does not address this; flagged as v2 work.

v2: multi-brofile per agent (one brofile_ref per provider family),
selected by caller provider at dispatch time. Defer.

## 13. Eval surface

Per agent, lives at `eval/agents/<agent_name>/`.

### 13.1 Query suite

For each agent, hand-author or auto-generate from evidence:
- ~15 positive queries (should match this agent in `bro_agent_search`)
- ~10 adversarial queries (should NOT match — anti-pattern check)
- ~10 dispatch queries (validate output schema + content quality)

Distilled agents auto-seed positive queries from their evidence
sessions.

### 13.2 Eval arc

`examples/agent-system/workflows/agent-eval-arc.json` runs nightly:

1. for each installed agent:
   a. for each positive query: run `bro_agent_search`; pass if
      this agent in top-3 with similarity > 0.6
   b. for each adversarial query: run `bro_agent_search`; pass if
      this agent NOT in top-5
   c. for each dispatch query: dispatch + check output against
      gold structure; pass if structural fields match and citation
      density meets `evidence_density` declared
2. aggregate per-agent pass-rate; baseline tracked
3. on regression > 5%: emit `bbox_note(kind=blocked, tag=agent-eval-regression)`,
   surface in `bbox_inbox`

### 13.3 Discovery eval

Cross-agent: do queries actually return the right agent? A query
matching multiple agents' `when_to_use` should rank them sensibly.

### 13.4 Cuing eval (active mode)

For brofiles that include active-mode cuing (§6.3), validate that
the bro actually delegates to the suggested agent. The eval is
measurable via tool-call inspection on the dispatched bro's task
record:

For each gold scenario:
1. dispatch a bro with the active-cuing-enabled brofile + a prompt
   matching one of the seeded "should delegate" cases.
2. wait for the bro's task to terminate.
3. read the task's tool-call log (existing `bro_status` /
   `bbox_search(role=tool_use, session_id=...)` returns the events).
4. assert: the log contains a `bro_agent_dispatch` call AND the
   dispatched agent matches the gold-expected agent (parse the
   `agent` arg from the tool-call).
5. for "should NOT delegate" cases (where the prompt matches an
   anti-pattern), assert the log contains NO `bro_agent_dispatch`
   call.

Pass criteria per scenario: the binary delegation decision matches
gold. Aggregate pass-rate is the cuing eval's score. Run as a
weekly arc (more expensive than nightly because it dispatches real
bros end-to-end).

## 14. Boundaries — what agent system is NOT

- **NOT a new dispatch engine.** `bro_exec` is. Agent dispatch is
  sugar that resolves manifest → filters → calls `bro_exec`.
- **NOT a new persona system.** Brofiles are. Agents wrap brofiles.
- **NOT a new artifact store.** Existing catalog is. Agents are
  one more `kind`.
- **NOT a new vector store.** Agentic-corpus is. Manifest embeddings
  ride a new `agent_manifest` bucket added to agentic-corpus's
  per-bucket routing (§16.1 substrate dependency; small additive
  change to the existing routing table).
- **NOT a privileged surface.** Any caller with MCP access reaches
  `bro_agent_*`. No special permissions.
- **NOT a security boundary.** Filter overlays constrain an agent's
  tool access for *focusing* purposes. They are not a sandbox; they
  are not a permission system. Sensitive tool access is governed by
  the substrate's existing filter chain and MCP transport, not by
  the manifest.
- **NOT auto-dispatching.** Cuing is informational. The orchestrator
  decides whether to delegate. No daemon-side auto-routing.
- **NOT a Claude `.claude/agents/*.md` reader.** Bbox does not parse
  Claude's native agent files. Optional v2 import path; v1 has
  none.
- **NOT a lifecycle classifier.** No `oneshot vs consultant` taxonomy
  in the manifest. Sessions live as long as someone resumes them.

## 15. Non-goals

- Auto-installing agents from arbitrary sources.
- Per-user / per-tenant agent isolation. Single-machine, single-user.
- Real-time agent registry sync across machines.
- Replacing Claude's native Task tool (orthogonal; both coexist).
- Replacing workflow authoring. Composition primitives are sugar over
  workflows, not a parallel composition engine.
- Replacing badgey. Badgey gains a manifest; everything else stays.
- Gating dispatch on eval pass. Eval is a regression detector for
  the registry's health, not a pre-dispatch check.

## 16. Substrate dependencies (REQUIRED) and open questions

### 16.1 Substrate dependencies (must precede v1)

These are not optional. The agent infra cannot land without them:

1. **`ArtifactKind::Agent` variant** added to the artifact catalog
   enum (§2 storage). Plus install dispatcher case for `kind="agent"`.
2. **`agent_manifest` embedding bucket** added to agentic-corpus
   §5.4 routes table.
3. **`agent` entity type + EntityRef variant** added to
   agentic-corpus §6.1, §6.2. Required for `DERIVED_FROM`
   provenance edges.
4. **`bbox_embed_iterate(bucket, project_id, since)`** new substrate
   primitive yielding `(entity_ref, vector)` pairs. Required for
   distillation clustering (§8.2). Thin wrapper over the existing
   vector store.

### 16.2 Open design questions

1. **`bro_agent_compose` in v1 or v2?** Composition manifest fields
   land in v1; sugar tool that auto-generates workflow specs from
   them could ship later. Bias: defer to v2; v1 ships fields and
   lets users hand-author composition workflows.
2. **`AgentProposalStore` generalization.** v1 keeps proposal staging
   inside BadgeyProposalStore. If a non-badgey distillation source
   emerges (e.g. workflow-arc-driven agent authoring), promote to a
   generic `AgentProposalStore` or absorb into the artifact
   catalog's draft-state machinery. v2 question.
3. **Multi-provider agents.** v1 binds agents to one provider via
   their brofile_ref (§9.2). v2 question: `brofile_refs_by_provider`
   dispatch shape, OR per-provider variants in the catalog with
   shared manifest. Defer until cross-provider demand proves.
4. **`parallel_safe: false` enforcement.** Same concept exists in
   the workflow engine. Reuse if possible — runtime advisory lock
   per `(agent, project_dir)` keyed on the manifest field. Defer
   plumbing detail.
5. **Manifest schema versioning.** When manifest fields evolve,
   migrate older agents on read. Bias: include `manifest_version`
   field; bbox migrates on load. Standard pattern.
6. **Anti-pattern penalty threshold.** §5.2 ranking penalty
   threshold. Bias: penalize when `cosine(query, anti_pattern) >
   cosine(query, when_to_use)`; magnitude calibrated via eval.
7. **Distillation calibration.** §8.2 constants: cluster size N=5,
   time-range 14d, uniformity 0.7, similarity-to-existing 0.7.
   Tune via badgey self-audit feedback (badgey doc §6.5 bounded
   learning loop).
8. **Auto-deprecation thresholds.** §8.3 `reject_count /
   accept_count > threshold`. Bias: 2:1 reject ratio, minimum 5
   accepts before considering. Tune via eval.
9. **Provenance audit chain.** Distilled agents carry evidence
   refs; if those source sessions get pruned by transcript GC, the
   provenance edges go stale. v2 question: re-anchor evidence to
   thread-level summaries before transcript GC eligibility, OR
   accept staleness with a `provenance.evidence_pruned` flag.

## 17. Glossary

- **Agent** — an installed artifact (`kind="agent"`) carrying a
  manifest + brofile binding. Discoverable via `bro_agent_search`,
  dispatchable via `bro_agent_dispatch`, runs as a regular bro under
  the hood.
- **Manifest** — the JSON payload that distinguishes an agent from a
  raw brofile: description, cuing, I/O contract, composition,
  provenance, embedding ref. "Claude frontmatter on steroids" —
  same role, richer schema, format-different (JSON, not markdown).
- **Brofile binding** — `brofile_ref` to a separately-installed
  brofile artifact, OR `brofile_inline` with the persona body. An
  agent must have one.
- **Filter overlay** — per-agent `allow` / `disallow` patterns
  merged on top of the brofile's filter chain at dispatch time
  (deny-wins).
- **Selection cuing** — `description` + `when_to_use` +
  `anti_patterns` in the manifest. Drives `bro_agent_search`
  ranking and informs callers.
- **Composition contract** — `inputs.schema`, `outputs.schema`,
  `composition.chainable_after`, `composition.parallel_safe`,
  `fan_out_aggregator`. Makes chain / fan-out / escalation
  declarative.
- **Provenance** — `manifest.provenance` field: `hand_authored`,
  `distilled` (with badgey + evidence refs), or `imported`.
- **Distillation** — badgey's proposal pipeline that mines transcript
  clusters and drafts new agent manifests. See §8.2.
- **Session portability** — a `bro_agent_dispatch` returns an
  `AgentSession` handle; storing it (the full handle, not bare
  `session_id`) in a thread / knowledge / workflow var lets future
  callers resume via `bro_resume(session_id, provider)`.
- **Native agents** — Claude's `.claude/agents/*.md` files +
  Anthropic-shipped Task-tool agents. Orthogonal to bbox agents.
  Bbox neither reads nor ships in their format.
