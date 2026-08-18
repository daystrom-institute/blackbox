# Design Graph Operations

Agent-facing operations for the `design` project graph: what lives in it, how
to mutate it, who may flip what, and how to read it. The substrate design,
schema rationale, and campaign phasing live in
`design/corpus/agentic-corpus/design-graph.md`; this doc is the hands-on
layer. Status: phase 0 (schema ratified, verb script core, seed vertices).
The graph holds STATE; the design docs hold STORY.

## Surfaces

- **State**: `.bbox/graphs/design/` (`graph.json`, `schema.json`,
  `vertices.jsonl`, `edges.jsonl`). Committed repo content.
- **Writer**: `scripts/design-graph` is the ONLY sanctioned author.
  Hand-editing the jsonl is banned; the verb surface is the integrity
  contract. Mutations stage, run `check` against the staged graph, and refuse
  to land on any error; `graph.json`'s generation bumps on every landing.
- **Read**: the blackbox daemon via MCP (`bbox_hybrid_search` graph lane,
  `bbox_inspect_entity`, `bbox_find_paths`, `bbox_project_graph_*`). Logical
  refs look like `project_graph_vertex:<project>:design:<vertex-id>`.
- **Renders**: `render-state <doc-id> [--write]` mints the state block into a
  design doc between `<!-- dsg:state -->` markers (or prints it); the block is
  a projection and the offline fallback. Edit the graph, never the block.
  `render-preamble` emits the consumer preamble generated from `schema.json`.

## The authoring loop

1. Once per checkout: `bro workspace-binding mint --daemon-url
   https://blackbox.daystrom.app` (the estate daemon is remote; the localhost
   default is wrong here). Capture after edits with
   `bro workspace-binding capture` to push the working state.
2. Mutate through the verb script (`create` / `update` / `edge` / `edge-rm`);
   the staged `check` is the gate.
3. Run `check` (structural mirror of the daemon validator; the daemon's
   `bbox_project_graph_validate` stays authoritative) and `lint` (instance
   invariants: graph lifecycle must equal doc frontmatter lifecycle).
4. Commit to publish. Provenance is git: every mutation lands as a commit
   that names what moved; the committed generation is what other checkouts
   and dispatched agents read.
5. Re-render affected state blocks (`render-state <doc-id> --write`) in the
   same commit.

## Entity selection

| You have | Kind | Id shape |
|---|---|---|
| A design doc (intent record) | `dsg:Design` | `doc/<repo-relative-path>.md` |
| A topic-hub doc | `dsg:Hub` | `doc/<repo-relative-path>.md` |
| A code boundary / touch surface | `dsg:Module` | `module/<crate-or-plane>` |
| An open question | `dsg:OpenQuestion` | `question/<slug>` |
| A design-space decision (rationale, alternatives) | `dsg:Decision` | `decision/<slug>@N` |
| An invariant or preference the tree must satisfy | `dsg:Constraint` | `constraint/<slug>@N` |
| The durable idea behind one or more docs (mint lazily) | `dsg:Concept` | `concept/<slug>` |
| A Resolved Choices entry | `dsg:Choice` | `choice/<slug>` |

If it is about agent behavior or cross-project operating rules, it belongs in
knowledge (`bbox_learn` / `bbox_decide`), not here. If it is prospective work,
it belongs in roadmap/threads. If it is normative clause content, it belongs
in `specs/`. The graph links to those stores by ref, never absorbs them.

Phase gating: phase A mints Design/Hub/Module/OpenQuestion. Decision,
Constraint, Concept, and Choice minting begins in phase B (harvest on new and
edited docs; Concepts only when a second articulation shows up).

## Write authority (the commitment gate)

- **Anyone (agents included) may FILE**: OpenQuestion vertices, edge
  proposals (DEPENDS_ON, RELATES_TO, RAISES, RESOLVES), Module seeds,
  Decision/Constraint vertices at `status: proposed`.
- **Operator-ratified sync only may BIND**: lifecycle flips, SUPERSEDES
  ratification, Decision/Constraint status changes, schema edits.
- The substrate enforces none of this; the verb script, this doc, and review
  carry it. Say so honestly when it matters.

## Reading

- `scripts/design-graph show <id>` prints the vertex, its directed edges, and
  the authored hint breadcrumbs with counts; a zero-count hop prints `(0)`
  because absence is an answer.
- Statement-bearing properties (`brief`, `statement`, `rationale`,
  `decision`) truncate by default under the daemon's smart property mode:
  pass `property_mode: full` when the payload matters.
- STATE questions ("what is the current X"): resolve `dsg:SUPERSEDES` chains
  to the live endpoint before answering in the present tense; superseded
  designs answer historical questions only.
- BINDING questions ("what constrains this module"): inbound `dsg:CONSTRAINS`
  on the Module; include each constraint's kind (invariant vs preference),
  scope, and status.
- The full question-type answer protocol (STATE / BINDING / WHY / IMPACT /
  REPLACEMENT) is specified in the design doc's retrieval section; the
  pasteable consumer preamble is `scripts/design-graph render-preamble`.

## Computed views (phase A backlog)

`current`, `binding`, `stale`, and `blast` are planned deterministic views
over the same state (see the design doc). They are planning conveniences and
the offline mirror; agent recall rides the bbox primitives plus the preamble,
never these views.

## Anti-patterns

| Do not | Instead |
|---|---|
| Hand-edit the jsonl | The verb script, always |
| Answer STATE questions from doc prose | Resolve the supersession chain in the graph |
| Store implementation state on vertices | `verified_against` + `verified_at` anchors only |
| Edit a rendered state block | Edit the graph, re-render in the same commit |
| Ratify your own proposed decision | File proposed; sync ratifies |
| Mint a Concept for a single-doc idea | Concepts mint on second articulation |

## Known deltas (phase 0)

- `check` mirrors the daemon validator's load-bearing rules; nested object
  property terms are only key-presence checked, and enum member sanity is
  daemon-side.
- `lint` checks lifecycle agreement only; topic-list agreement with frontmatter
  is TODO.
- Search-lane convergence semantics (learned the hard way, gap-7bc434bf,
  resolved as misdiagnosed): graph word lanes activate on published-view
  INSTALL (accept-advance of the pushed commit, boot reconcile, or capture
  when no published view is installed). Incremental `bbox_reindex` is NOT the
  trigger; it only preserves existing lanes. Expect minutes of lag between
  push and searchability (collector + accept refresh + writer queue). Diagnose
  with `bbox_project_graph_describe`'s retrieval block:
  `indexed_generation` vs `accepted_generation`, `indexed_vertex_count`.
- Miners (`mine-design-frontmatter`) and transactional `apply` are phase A.
- The eval suite (30 questions over the seed cluster) is phase A; see the
  design doc's evaluation gate and `retrieval-eval-harness.md`.
