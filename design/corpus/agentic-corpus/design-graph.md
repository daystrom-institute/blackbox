---
title: "Design Graph: Corpus State as a Reflective Project Graph"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - corpus
  - agentic-corpus
  - graph
  - design-corpus
tags: [design-corpus, graph, retrieval, hints, runbooks]
brief: "Subsume the design corpus's STATE (lifecycle, supersession, dependencies, decisions, constraints) into a reflective project graph under .bbox/graphs/design/, keep docs as STORY, and make agent-facing retrieval a cue-stack runbook over bbox primitives plus a few computed views, gated by an eval suite."
date: 2026-08-17
---

# Design Graph

Status: proposed

Related:

- [Reflective Project Graph](reflective-project-graph.md) - the substrate this
  builds on; schema-as-data, structural validation, hints, visibility planes.
- [Reflective graph state transport and visibility](reflective-graph-state-transport.md) -
  published/provisional planes and workspace binding capture.
- [Retrieval Eval Harness](retrieval-eval-harness.md) - the measurement
  instrument the design-graph suite reuses; also the donor-spike provenance
  pointer (`../daystrom-mk2/spikes/Daystrom.Spike.McpPoc/`).
- [Knowledge And Memory](knowledge/knowledge-and-memory.md) - the store-side
  boundary partner (what does NOT move into this graph).
- `docs/corpus-frontmatter-schema.md` - the frontmatter chassis the miner
  projects from.
- A client repo's campaigns-layer graph pilot (private) - the operating
  discipline donor: verb-script authorship, commitment gate, computed views,
  consumer preamble. Genericized here; do not cite client identifiers.

## Thesis

The design corpus's **state** should live in a reflective project graph
committed under `.bbox/graphs/design/`, and its documents should keep only the
**story**. State is what agents need to be right about today: which designs are
live, which are superseded and by what, what depends on what, which decisions
and constraints bind a given surface, and when any of that was last verified
against the code. Story is why: rationale, rejected alternatives, the narrative
arc. Today both are fused in prose, and agents reading for state routinely
anchor on obviated narrative.

The graph is authored by a verb script (the only sanctioned writer), projected
alongside prose, and consumed through the existing blackbox read surface plus a
small set of deterministic computed views. Retrieval quality comes from the
cue stack (hints, breadcrumbs, answer protocols), not from pre-baked query
recipes: that lesson is measured, not aesthetic, and it shapes this design's
center of gravity.

## The friction this exists for

Four concrete failure mechanics, all observed:

1. **State is stored as story.** A design doc is a point-in-time narrative
   write with no decay signal. The `lifecycle:` field lags reality (the
   reflective graph design above still reads `proposed` though its floor
   shipped); "Status: proposed" lines sit atop descriptions of shipped
   machinery; supersession hides in optional backmatter.
2. **Headline anchoring.** Agents skim titles and section headings and never
   reach the "Resolved Choices" or supersession notes buried below. The corpus
   map's own guidance ("verify against the code before relying on it") is a
   manual discipline no dispatched agent executes.
3. **Status-blind retrieval.** Hybrid search happily surfaces a superseded doc
   beside or above its successor; ranking carries no liveness signal.
4. **Unanswerable cross-doc questions.** "Which live designs depend on a
   retired one" is a traversal in disguise, and with 130 design leaves it is
   near-certain to be happening silently today: 14 leaves carry `superseded_by`
   free-text pointers the sweep tooling does not even parse, and dependency
   between designs is untyped prose.

## Grounding

- **The substrate is proven.** The reflective floor landed on this branch and
  is exercised end to end (schema-as-data above the `meta:` floor, structural
  validation, published/provisional planes, hints with authored and derived
  tiers, declared search participation). This repo has no graph yet; this
  design would be its first.
- **The operating discipline is proven.** A client repo's campaigns layer
  (private) runs its plan state as a 15-kind reflective graph with a
  verb-script-only write path, check/lint, a file-vs-bind commitment gate,
  computed planning views, and a maintained consumer preamble. Hand-authored
  content round-tripped through the daemon with zero structural errors on the
  first pass.
- **The retrieval lesson is measured.** The donor retrieval spike
  (`../daystrom-mk2/spikes/Daystrom.Spike.McpPoc/`, distilled in
  `../daystrom-mk2/design/agentic-discovery-tools.md`) graded a 30-question
  suite across strategies:

  | Approach | Pass rate |
  |---|---|
  | Search-only (hybrid + RRF) | 83% |
  | Static conditioned scoring, incl. pre-baked relation plans | 23% |
  | Answer packaging polish | 13-17% |
  | Agentic: primitive tools + breadcrumbs + prompt cues + answer protocol | 97% |

  Search was never the bottleneck; everything after the seed was. The winning
  run did NOT use the pre-baked recipe tools it had access to, and did not
  need evidence bundling. What moved the number: response-shaped affordances
  (recommended next hops computed from the full neighborhood, edge-family
  coverage with explicit zeros as negative evidence, direction-preserving
  arrows, stable path IDs), description-level cues, an always-on navigation
  fragment, and an intent-keyed answer protocol with MUST-include criteria.

## The state/story split

The core cut:

- **Graph owns state.** Identity, lifecycle (as a filterable, minable
  projection), supersession, dependency, open questions, decisions,
  constraints, verification stamps.
- **Docs own story.** Rationale, alternatives, narrative, phase history. A doc
  remains the canonical artifact for WHY; the graph is canonical for WHAT IS
  CURRENTLY TRUE about the concept space.
- **A minted state block** (see below) renders graph state at the top of each
  doc, exactly where the headline anchor lands, so even the skimmer who reads
  only the opening lines reads current state instead of obviated narrative.

This converts doc currency from a memory discipline into a lint condition.
Prose currency requires an agent to remember, mid-task, that an old doc exists
and deserves an edit; nothing fails when it does not. Graph currency rides
events that already happen: design edits flow through the miner,
implementation flows through git (`enacted_by` anchors, module-change-driven
staleness), verification flows through the checking pass, and divergence
between graph and frontmatter is a `check` failure rather than silent drift.

## The graph

Graph id `design`, namespace `dsg`, files under `.bbox/graphs/design/`
(`schema.json`, `vertices.jsonl`, `edges.jsonl`). Refs resolve as
`project_graph_vertex:<project>:design:<vertex-id>`. Vertex ids are typed
slugs; Design and Hub ids are the repo-relative doc path (stable, and the
miner can derive them mechanically).

### Vertex kinds

| Kind | Id shape | State it owns | Phase |
|---|---|---|---|
| `dsg:Design` | `doc/<path>.md` | one per design leaf; `lifecycle` and `topic` mined from frontmatter; `brief`; `verified_against` commit + `verified_at` | A |
| `dsg:Hub` | `doc/<path>.md` | one per design-hub / topic home | A |
| `dsg:Module` | `module/<crate-or-plane>` | the touch-surface anchor: major code boundaries, path-keyed, mechanically seedable | A |
| `dsg:OpenQuestion` | `question/<slug>` | statement, depth, outcome + synthesis when concluded; input to campaign inquiries and active threads | A |
| `dsg:Decision` | `decision/<slug>@N` | rationale, alternatives, `status: proposed \| active \| superseded \| reversed`, `enacted_by` commit ref | B |
| `dsg:Constraint` | `constraint/<slug>@N` | kind `invariant \| preference` (the senior-engineer test), scope, `check_rule` (mechanical check), licensing | B |
| `dsg:Concept` | `concept/<slug>` | the durable idea behind one or more docs; lifecycle `proposed → active → superseded \| absorbed \| deferred \| dismissed`. Mint lazily: only when a second artifact shows up for the same idea | B |
| `dsg:Choice` | `choice/<slug>` | a Resolved Choices section entry: decision, rationale, rejected alternatives | B (harvest on new/edited docs only) |

An `@N` suffix versions identity-changing replacement: superseding a decision
mints `decision/<slug>@2` and a SUPERSEDES edge, leaving the prior vertex
readable as history.

### Edge families

- Structure: `dsg:UNDER` (Design -> Hub), `dsg:RELATES_TO`
- Supersession: `dsg:SUPERSEDES` (absorbs the free-text `superseded_by`)
- Dependency: `dsg:DEPENDS_ON` with `kind: hard | crosslink` (the campaigns
  pilot's HORIZON shape)
- Questions: `dsg:RAISES` (Design -> OpenQuestion), `dsg:RESOLVES` (Design or
  Choice -> OpenQuestion)
- Governance (phase B): `dsg:CONSTRAINS` (Decision/Constraint -> Module or
  Design), `dsg:RELAXED_BY` (justified deviation; accumulation on one
  constraint is a standing signal it needs revision; never targets
  `kind: invariant`), `dsg:SCOPED_BY` ("A except B": source holds except for
  the target's narrow case)
- Articulation (phase B): `dsg:ARTICULATES` (Design -> Concept)
- Later: `dsg:CANONIZED_IN` (Design -> a spec mirror vertex), wiring the
  intent corpus to the canon corpus (`specs/` keeps clause-level normative
  decomposition; this graph links to it, never absorbs it)

V1 rules from the substrate bind: edges connect graph vertices only; refs to
commits, files, and specs ride as canonical ref strings in properties
(`enacted_by`, `verified_against`, `spec:`).

### Lifecycle and verification

- `lifecycle` on a Design vertex is a **mined projection** of frontmatter:
  the miner owns it, the verb script refuses hand-flips, and `lint` asserts
  graph == frontmatter. Frontmatter stays authoritative in phase A; authority
  flips per-field only where a later phase earns it (supersession flips
  first, in phase B).
- **Anchored verification only.** Any claim that a design matches current
  runtime carries `verified_against` (a commit) plus `verified_at` and the
  verifying pass. Never a bare boolean. Staleness is then a query: a design
  whose `verified_at` predates subsequent changes to modules it constrains.
- Implementation state is never stored, only computed or anchored (the
  dated-inventory hazard, applied).

### Hints and search participation

Authored hints per kind from day one; they are the substrate's Cue layer and
this graph would be the second authored-hints consumer after the campaigns
pilot. Examples: Design hints out DEPENDS_ON ("depends on"), in DEPENDS_ON
("unblocks"), in SUPERSEDES ("superseded by"), out RAISES ("open questions");
Decision hints out CONSTRAINS, in SUPERSEDES, and out ENACTED_BY-adjacent
anchors; Constraint hints in RELAXED_BY ("relaxation history"). Zero-count
authored hints render `(none)` so absence stays visible.

Search participation is declared, not implied (the 2026-08-13 ruling): labels
index by default; `brief`, `question.text`, and `decision.rationale` are
annotated in `schema.json` or the hybrid-search graph lane will not see them.

## Retrieval: primitives and cues, not recipes

The donor's refutations set the center of gravity: recipe tools lost to
cue-rich primitives; pre-baked plans were the 23% lane. So the agent-facing
retrieval story is the existing primitive sequence under a cue stack, and the
verb script's computed views are planning conveniences, never the load-bearing
path for agent recall. Three manifestations:

### 1. The thought runbook (cue stack)

An ops doc plus a deferred system memory entry extending
`sm-agentic-opening-sequence` (which owns the generic question shapes) with
design-space overlays. Contents:

- **The consumer preamble**, maintained verbatim in one place and pasted into
  dispatch briefs: canonical ref format; `property_mode: full` for
  statement-bearing kinds; follow "recommended next hops"; trust topical seeds
  over exact wording; never restate multi-hop chains from memory, cite
  `path_ids`; budget discipline; report the missing hop instead of filling it
  in. The preamble is **generated from `schema.json`** (kinds, hints, edge
  families) by a `render-preamble` verb so prompt text cannot drift from, or
  hallucinate, tool affordances.
- **Answer-protocol overlays** keyed by question type, each with MUST-include
  criteria:
  - STATE/CURRENT: resolve the supersession chain to its live endpoint and
    cite its verification stamp; superseded forms appear only under explicit
    historical questions.
  - BINDING: each constraint with kind (invariant vs preference), scope, and
    licensing; for in-transition ones, the licensing target.
  - WHY: rationale plus rejected alternatives from the Decision vertex,
    traced to the motivating question or finding.
  - IMPACT: follow through modules to affected designs and constraints, with
    staleness stamps.
  - REPLACEMENT/HISTORY: both old and new entities, the bridge, direction as
    shown by validated paths.
- Base shape rules from the donor, verbatim in spirit: direct answer first;
  name the missing hop rather than plausible prose; prefer the current
  canonical entity.

### 2. Computed views (the verb script's read verbs)

Deterministic, checkout-local, offline-capable, and shaped like the donor's
tool outputs (arrows, breadcrumbs, direction labels, "checked vs not-queried",
truncation tiers):

- `current <concept-or-doc>` - resolve the supersession chain; print live
  state, verification stamp, open questions, binding constraints.
- `binding <module-or-path>` - constraints and decisions binding a touch
  surface (the smallest, highest-frequency read).
- `stale` - designs whose `verified_at` predates subsequent changes to
  modules they constrain.
- `blast <commit-range>` - commits -> modules -> affected constraints,
  decisions, designs -> what needs re-verification.
- Planning views (the `frontier`/`blockers` class) as needed later; they are
  conveniences over the same state, not retrieval mechanisms.

### 3. The minted state block

A managed render region (`<!-- dsg:state -->` markers) at the top of each
design doc, generated from graph state in the same commit as the graph
mutation: current status, superseded-by pointer, verification stamp, live open
questions. This is the direct counter to headline anchoring: the thing the
skimmer reads first becomes current state. Edit the graph, never the block;
the block is also the offline fallback.

## Write path and authority

- `scripts/design-graph` is the only sanctioned author of vertices and edges
  (stamps provenance, refuses colliding ids, pre-flights with
  `bbox_project_graph_validate`). Hand-editing the jsonl is banned; the verb
  surface is the integrity contract, and the script doubles as the copyable
  reference implementation the product hands to other repos. The substrate
  permits file-first v1 mutation; this design chooses script-first anyway.
- Verbs: `list`, `show`, `create`, `update`, `edge`, `edge-rm`, `check`
  (mirrors the daemon validator), `lint` (instance invariants), `apply`
  (transactional batch plans from miners), `render-state`, `render-preamble`,
  plus the computed views above.
- **Commitment gate**: any pass (agents included) may FILE proposed state:
  OpenQuestion vertices, edge proposals, Design/Module seeds, miner plans.
  Only an operator-ratified sync pass BINDS: lifecycle flips, supersession
  ratification, Decision/Constraint status changes. The substrate enforces
  none of this; the verb script, the ops doc, and review carry it. Say so
  honestly where it matters.
- **Miners never touch the graph directly.** `mine-design-frontmatter` parses
  frontmatter into plan files (Design/Hub/UNDER seeds, `superseded_by` ->
  SUPERSEDES edges, lifecycle projection) and lands through `apply`
  (idempotent, conflict-refusing, `--check` first). The rule binds: no surface
  is mined without its divergence check wired the same day.
- Graph-vs-prose divergence is a first-class measure: `check` reports it, and
  it is fixed in the same commit, in the direction the authority points.

## Boundaries with existing stores

The duplication boundary, stated once and held:

- Structure and relations of the design corpus itself -> `dsg:` vertices.
- Agent-behavior rulings and cross-project operating rules -> knowledge
  (`bbox_learn` / `bbox_decide`) stays.
- Substrate gaps -> `bbox_gap` stays (a design-graph Finding analog, if ever
  needed, files drift between docs and tree and cites evidence; it does not
  replace gap notes).
- Prospective concepts and inquiries -> the campaign layer; active execution
  -> threads. Roadmap records are retained as read-only history; see
  [the retirement contract](../../surfaces/mcp/roadmap-retirement.md).
- Normative clause-level content -> `specs/` stays; this graph links.

## What this design refuses

- The factory. No resident sweep services, no decay daemons, no staleness
  daemons, no event-sourcing machinery (point-in-time historical projection
  is `git log` on the committed jsonl; that is sufficient). Every standing
  check is actuated by a pass that already runs.
- Recipe-tool gravity. The computed views exist for planning convenience and
  offline work; agent recall rides primitives + cues. If a view starts being
  the only correct path to an answer, that is a retrieval failure to fix in
  the cue stack, not a verb to add.
- Claim-level normative decomposition (specs owns that), re-homing decisions
  that belong to the knowledge lanes, and wholesale backfill of 130 leaves by
  hand. The seed is mechanical; everything else accretes on contact.

## Evaluation gate

Reuse [Retrieval Eval Harness](retrieval-eval-harness.md) rather than forking
it. Its modes map cleanly onto this graph:

- SearchOnly/Conditioned against the graph lane: expected graph vertices as
  the `expected_entity_refs`; the fixture corpus is a fixture checkout of
  `.bbox/graphs/design/` built through `SharedState::for_test` (the graph is
  committed state, so full corpus control is a copy away).
- EndToEnd with real agent turns is where the cue stack is actually measured:
  a ~30-question suite (benchmark + held-out, authored against the seed
  cluster) run through dispatched agents with the generated preamble,
  scored on MUST-include criteria per question type (required typed refs
  present in the final answer) plus the budget envelope (donor-validated ~20
  tool calls). This is the design-graph analogue of the donor's gate and the
  only way "we adapted the cues" is falsifiable rather than asserted.

Gate criteria: pass rate must not regress on tool-surface or schema changes;
failures classified (infra vs substrate vs retrieval); held-out discipline
per the harness doc's open questions.

## Substrate follow-ons (not blockers)

- **Schema-generated preamble** (already adopted above as `render-preamble`);
  generalize if a second graph wants it.
- **Status-aware ranking**: let the hybrid-search graph lane prefer live
  endpoints of supersession chains over their ancestors. Daemon-side retrieval
  work, separate from this campaign.
- Edge-family coverage on inspect (expected-but-absent zeros beyond authored
  hints) if the eval shows hallucinated-absence failures.

## Phasing

- **Phase 0:** ratify this design and the schema; write the ops doc skeleton
  and the verb script core (list/show/create/update/edge/check/lint).
- **Phase A (projection):** miner seeds Design/Hub/Module vertices, UNDER
  edges, and the SUPERSEDES backfill from the 14 `superseded_by` pointers;
  hand-wire DEPENDS_ON plus OpenQuestion extraction for one cluster
  (`design/corpus/agentic-corpus/` is the candidate: dense, interdependent,
  and the machinery's own neighborhood, with enough content to author 30
  honest eval questions); minted state blocks; generated preamble; eval suite
  skeleton and baseline run. Prose stays canonical throughout.
- **Phase B (new-value flips):** new designs RAISES their open questions as
  vertices; Decision/Constraint/Concept minting begins (harvest on
  new/edited docs; lazy Concepts); supersession authority flips to the graph
  with frontmatter minted by render.
- **Phase C (only if earned):** lifecycle authority flips; hub "related
  designs" blocks become renders; `binding` wired into dispatch briefs as the
  standard pre-edit read.

Each phase is operator-gated; a phase that does not earn its keep is rolled
back to prose with the graph deleted, losing nothing the prose did not already
hold.

## Open questions

- Concept as a distinct kind vs collapsing into Design-plus-supersession
  clusters (current lean: distinct-but-lazy, mint on second articulation).
- Module seeding grain: crate- and plane-level only, or finer? Coarse is
  useful and maintainable; finer awaits a `binding` query that actually hurts.
- Decision home confirmation: the `dsg:Decision` vs `bbox_decide` boundary
  above is this design's proposal, not an established rule.
- Whether the minted state block renders for hub docs too, or leaves only.
- Held-out suite discipline: who guarantees the held-out questions stay
  unconsulted while the cue stack is tuned.

## Privacy

This repo is public. The sibling-substrate references above follow existing
committed precedent (`retrieval-eval-harness.md`); the client campaigns pilot
stays genericized (never its repo name, paths, or artifact names); no client
identifiers may enter vertices, edges, renders, or commits. Scrub before every
commit; a leaked blob in pushed history is a real exposure even after the tip
is fixed.
