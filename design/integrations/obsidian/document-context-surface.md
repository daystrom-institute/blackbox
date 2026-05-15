---
title: "Obsidian Document Context Surface"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - integrations
  - obsidian
  - document-context
date: 2026-05-15
status: "design proposal v1; reviewed by Opus and DeepSeek"
brief: "Designs an Obsidian plugin and Blackbox document-context surface for enriching notes with graph, provenance, git, knowledge, and related-document context."
---

# Obsidian Document Context Surface

## Problem

Obsidian is a strong local reading and writing environment, but its native graph
model is intentionally simple: files, links, backlinks, tags, aliases, and
frontmatter. Blackbox has a richer graph around the same documents: provenance,
commit history, git notes, transcript sessions, work threads, notes, knowledge
entries, semantic edges, symbol references, and vector similarity.

When an operator is reading a design document in Obsidian, that context is
currently invisible unless they switch back to an agent CLI and manually run
graph/search tools. The result is a split-brain workflow:

- Obsidian owns the reading surface.
- Blackbox owns the provenance and graph context.
- The operator has to manually ask for the context that should be ambient.

The desired product is not "Obsidian can call every blackbox MCP tool." The
desired product is: while viewing a document, Obsidian can show the useful
blackbox context around that document in a way that is typed, explainable, and
safe.

## Thesis

Add a read-only **document context surface** in blackbox and render it from an
Obsidian plugin.

The surface is not an MCP surface. It is a curated response shape:

```text
given project + file path -> return the enriched context for this document
```

Blackbox owns the enrichment recipe, aggregation, ranking, and provenance
semantics. Obsidian owns the UI. The plugin should not implement MCP session
mechanics, graph traversal policy, chunk aggregation, or corpus ranking.

## Goals

1. Show blackbox provenance, git, knowledge, thread, note, link, and related-doc
   context while reading a document in Obsidian.
2. Keep the first version read-only and low-risk.
3. Return an opinionated document-level model instead of raw graph dumps.
4. Distinguish asserted graph facts from suggested vector-similar candidates.
5. Aggregate chunk-level `project_file` data into a whole-document view.
6. Make the response usable by non-Obsidian clients later: editor plugins, web
   dashboards, Slack unfurls, and CLI inspectors.
7. Preserve blackbox namespace conventions: workspace-shaped MCP handlers use
   `work_*`, while plain HTTP endpoints may use route names.
8. Keep operator-authored documents unchanged unless a later explicit write
   workflow is designed and approved.

## Non-Goals

- Do not expose the whole MCP tool catalog to Obsidian.
- Do not require Obsidian to implement streamable-HTTP MCP session handling.
- Do not write to the vault automatically.
- Do not create or accept Obsidian links without explicit operator action.
- Do not mutate the existing markdown document in v1, even on explicit link
  acceptance. Accepted relations live in project-scoped `.bbox/` state.
- Do not treat vector similarity as a durable edge.
- Do not replace `bbox_hybrid_search`, `bbox_inspect_entity`, or evidence
  bundling. The document context surface composes those capabilities for one
  user-facing shape.
- Do not build a general graph visualization product in v1.
- Do not implement live git-note fallback reads in this surface. Git-note
  provenance appears only after normal bbox import/indexing has made it graph
  state.

## Design Anti-Conventions

Things this design deliberately does NOT do, even though a reader familiar with
blackbox, Obsidian, or MCP tooling might expect them:

- **No MCP session in the plugin.** The Obsidian plugin speaks plain HTTP. It does
  not initialize, handshake, heartbeat, or teardown an MCP session. The server
  still exposes an optional `work_document_context` MCP tool for agent clients,
  but the Obsidian path avoids MCP transport entirely.
- **No markdown mutation in v1.** Accepted document relations are written to
  `.bbox/` sidecar state, never to the source markdown. A later design may
  deliberately opt into markdown mutation; v1 does not.
- **No per-chunk MCP calls.** The enrichment recipe reads edges through internal
  batched EdgeIndex/store lookups. It does not call `bbox_inspect_entity` or any
  other MCP tool per chunk. This is a server-side composition, not an agent
  scripting exercise.
- **No LLM-generated text in the response.** Every string the server returns is
  deterministic — templates, classifiers, and lookup tables. The `why` field in
  `potential_related` items uses a fixed template, not an LLM summary. The plugin
  never prompts an LLM.
- **No live git fallback.** Git-note provenance appears only after normal
  blackbox import/indexing has promoted it to graph state. The endpoint does not
  shell out to `git notes` or read `.git/` directly.
- **No vector similarity as a durable edge.** `potential_related` items carry
  `confidence="suggested"` and are never persisted as graph edges without
  explicit operator acceptance through the write-capable phase.
- **No whole-document embeddings in v1.** Related-document candidates use a
  chunk-composition fallback (title, headings, representative chunks → hybrid
  search). The gap is tracked as `note-ff15b657`.
- **No general graph visualization.** The response is a typed, sectioned document
  context model, not a node-link diagram or raw graph dump.

## V1 Scope And Architectural Decisions

Phases 0-5 are the read-only v1. Phase 6 is a later write-capable extension
that depends on a relation importer. The architecture decisions below define the
target boundary, but relation acceptance does not ship in the read-only v1.

Keep the first pass intentionally narrow:

1. **Registered projects only.** The endpoint requires a registered blackbox
   project. There is no local-only fallback for arbitrary vault files.
2. **Caller chooses the project.** The Obsidian plugin maps a vault path to a
   project root, but the endpoint accepts any registered project. This matters
   when Codex or another tool creates adjacent worktree directories and the
   active note should be resolved against that project instead of the vault's
   default root.
3. **BBox graph state is the source of truth.** Git notes are not read live as a
   fallback. If provenance matters, import/export keeps it in bbox first; the
   document context route renders what bbox knows.
4. **No markdown mutation.** The plugin does not insert links, frontmatter,
   evidence blocks, or provenance stanzas into the existing document in v1.
5. **Accepted relations go to `.bbox/`.** If the operator accepts a suggested
   relation in the later write-capable phase, write a project-scoped sidecar
   under `.bbox/` and ingest it into bbox through a focused relation importer.
   That keeps relations git-controlled without rewriting the source document.
6. **Whole-document embeddings are a known gap.** Until bbox has a
   document-level embedding lane, related-doc candidates are composed from
   title, headings, and representative chunks, then collapsed by document path.
   Gap note: `note-ff15b657`. The gap is a document-level embedding/index lane
   for aggregates of existing markdown/project-file chunks. Without it, long
   documents can overrepresent headings and early sections, while deeply nested
   sections may be underrepresented in related-doc suggestions.
7. **Plugin location.** The Obsidian integration lives in this repository under
   `integrations/obsidian/`.

## Current Ground Truth

Relevant existing capabilities:

| Concern | Current capability |
|---|---|
| Indexed file chunks | `project_file` entities, chunked by document section/code block, with `file_path`, `project_id`, `chunk_kind`, `language`, and content preview. |
| Graph search | `bbox_hybrid_search` ranks typed entities with BM25/vector fusion and project filtering. |
| Entity inspection | `bbox_inspect_entity` returns properties and targeted edges for one entity ref. |
| Path finding | `bbox_find_paths` returns direction-preserving graph paths for multi-hop explanations. |
| Evidence bundles | `bbox_bundle_evidence` packages entity refs and path ids into a bounded evidence object. |
| Provenance | Tool-call, transcript, thread, note, commit, and git-note provenance are already modeled in the graph and docs. |
| Git notes | `bbox_provenance_export` / `bbox_provenance_import` round-trip provenance through `refs/notes/bbox/provenance`. |
| Edge vocabulary | `SUPERSEDES` and `DERIVED_FROM` are existing knowledge/agent edge kinds; roadmap supersession is separately projected as `ROADMAP_SUPERSEDES`. |
| HTTP daemon | `blackboxd` already serves non-MCP routes beside `/mcp`. |

The main gap is not raw data. The gap is a product-shaped, document-scoped
aggregation layer.

## Product Shape

### Obsidian Plugin

The plugin adds a "Blackbox Context" side pane that follows the active note.

Expected behaviors:

- Resolve the active note to a project-relative path.
- Ask blackbox for document context.
- Render typed, collapsible sections.
- Let the operator open linked vault files from results.
- Let the operator copy a compact evidence bundle through an explicit command.
- Cache the last response briefly to avoid re-querying while moving the cursor
  around the same note.

The initial plugin does not need to know blackbox graph internals. It renders
the server's section model.

### UI Sections

For a `design/*.md` document, useful sections are:

| Section | Meaning |
|---|---|
| `provenance` | Sessions, tasks, brofiles, agents, notes, or threads that created, edited, reviewed, or discussed the document. |
| `git` | Commits touching the document, commit subjects, git-note provenance, and linked work objects when available. |
| `lifecycle` | `DERIVED_FROM`, `SUPERSEDES`, superseded-by, archived-by, replacement, and design lineage relationships. |
| `knowledge` | Decisions, conventions, memories, and runbooks derived from, cited by, or topically tied to the document. |
| `threads` | Active or historical work threads related to the document. |
| `attention` | Unresolved disputes, assumptions, followups, blocked notes, and stale work items connected to the document. |
| `implementation` | Source files, symbols, commits, and roadmap items that implement or reference the document. |
| `explicit_links` | Markdown links, `LINKS_TO_FILE`, `LINKS_TO_SECTION`, and other asserted document links. |
| `potential_related` | Vector/BM25-related documents and knowledge entries. Suggested only, not graph truth. |

Other document classes can use different recipes. A Rust source file might
emphasize symbols, callers, recent edits, and open notes; a knowledge markdown
export might emphasize supersession, contradictions, and source sessions.

## Server Surface

### Preferred HTTP Endpoint

Add a plain HTTP endpoint for editor integrations:

```text
GET /context/document?project=/abs/project&path=design/foo.md
```

Optional query params:

```text
include=provenance,git,lifecycle,knowledge,threads,attention,implementation,explicit_links,potential_related
max_items=8
related_limit=10
similarity_threshold=0.65
```

Reasons to prefer HTTP for Obsidian:

- The plugin avoids MCP initialization/session mechanics.
- Browser-like request code is simpler in an Obsidian plugin.
- The endpoint can enforce read-only behavior by construction.
- The response is stable product JSON, not a transport-level tool result.

### Evidence Bundle Action

`copy_evidence_bundle` can use the same document-context service to produce a
compact bundle from the currently rendered refs:

```text
POST /context/document/evidence-bundle
```

Inputs are `project`, `path`, selected section/item ids, and `format`.
Supported formats in v1: `markdown` and `text`. Both include stable machine
refs plus short human labels. This endpoint is read-only even though it uses
`POST`; read-only here means no server or vault state mutation.

### Optional MCP Wrapper

Expose the same capability as a workspace-shaped MCP tool for agents and
editor clients that already speak MCP:

```text
work_document_context(project, path, include?, max_items?, related_limit?)
```

The implementation should be shared with the HTTP route. The MCP wrapper is a
compatibility surface, not the primary Obsidian integration.

## Response Model

Top-level shape:

```json
{
  "schema_version": "document_context.v1",
  "project": {
    "path": "/home/invidious/repos/transcript-search",
    "project_id": "d723917f"
  },
  "document": {
    "path": "design/foo.md",
    "absolute_path": "/home/invidious/repos/transcript-search/design/foo.md",
    "title": "Foo",
    "kind": "design_doc",
    "entity_refs": ["project_file:d723917f:<rel_path_hash>:<chunk_hash>:0"],
    "chunk_count": 7
  },
  "summary": {
    "headline": "Design doc with 3 provenance links, 4 commits, and 6 related candidates.",
    "badges": ["provenance", "open-followups", "suggested-links"]
  },
  "sections": [],
  "actions": [
    { "id": "refresh", "label": "Refresh" },
    { "id": "copy_evidence_bundle", "label": "Copy evidence bundle" }
  ],
  "degraded": []
}
```

Section shape:

```json
{
  "kind": "provenance",
  "title": "Provenance",
  "confidence": "asserted",
  "items": [
    {
      "id": "session:codex:019e...",
      "label": "Originating Codex session",
      "summary": "Created during workspace tools review.",
      "target_ref": "session:codex:019e...",
      "target_path": null,
      "edges": ["EDITED_BY_SESSION"],
      "evidence": [
        {
          "kind": "edge",
          "ref": "project_file:d723917f:<rel_path_hash>:<chunk_hash>:0",
          "edge": "EDITED_BY_SESSION"
        }
      ],
      "actions": [
        { "id": "open_entity", "label": "Open entity" }
      ]
    }
  ]
}
```

Confidence vocabulary:

| Confidence | Meaning |
|---|---|
| `asserted` | Backed by explicit graph edge, git data, stored knowledge, or imported provenance. |
| `derived` | Computed from deterministic local data, for example markdown links or commit touch history. |
| `heuristic` | Based on syntax or text matching where false positives are possible. |
| `suggested` | Vector/BM25 similarity candidate; not a durable claim. |

The UI should visually separate `suggested` items from asserted context.

Top-level actions are document-level actions. Item actions are scoped to a
specific section item. The read-only v1 top-level actions are only `refresh`
and `copy_evidence_bundle`.

### Degraded Markers

Every section can degrade independently. The top-level `degraded` array uses
this shape:

```json
{
  "section": "potential_related",
  "code": "vector_route_unavailable",
  "message": "Related-document suggestions skipped because no vector route was available.",
  "retryable": true
}
```

Use stable `code` values for tests and UI branching. Human text belongs in
`message`. If a section cannot be computed, return either a partial section with
its own degraded marker or omit the section and add a top-level degraded marker.

Cross-section duplicates are allowed when the same entity carries different
meaning in different sections. For example, a session may appear under
`provenance` because it edited the document and under `threads` because it
belongs to a related work thread. Section kind disambiguates the rendering.

## Document Resolution

Input accepts either absolute or project-relative paths. Resolution steps:

1. Resolve `project` through the registered project registry.
2. Canonicalize `path` under the project root.
3. Reject paths outside the project root.
4. Look up all current `project_file` chunks for the relative path.
5. If no chunks exist, return a degraded response with `document.indexed=false`
   and optional fallback git/link data where available.
6. Roll chunk refs up into one document object.

The server should not require the plugin to know chunk hashes or entity refs.

Phase 1 should introduce a shared helper:

```text
resolve_project_relative_path(project, path) -> Result<ResolvedProjectPath>
```

The helper canonicalizes under the registered project root and rejects traversal
or symlink escapes. This avoids copying path-resolution logic into future HTTP
or MCP endpoints.

## Document Classification

The v1 classifier is deterministic:

1. Frontmatter `kind`, `type`, or `tags` wins when it matches a known value.
2. Path prefixes classify common project docs:
   - `design/` -> `design_doc`
   - `docs/` -> `doc_page`
   - `src/` with Rust extension -> `rust_source`
   - `src/` with Java extension -> `java_source`
3. Extension fallback:
   - `*.md` -> `markdown`
   - `*.rs` -> `rust_source`
   - `*.java` -> `java_source`
4. Unknown files use `document`.

Document kind selects the default section recipe. The initial implementation
only needs the `design_doc` recipe, but the classifier contract should be stable
from the start.

## Enrichment Recipe

The default recipe for markdown design documents:

1. **Document anchors**
   - all `project_file` chunks for the path;
   - title from first H1 or filename;
   - document kind from path and frontmatter.
2. **Direct graph edges**
   - collect graph edges for document chunk refs through an internal batched
     EdgeIndex lookup, not one MCP call per chunk;
   - collect `LINKS_TO_FILE`, `LINKS_TO_SECTION`, `DESCRIBES`,
     `EDITED_BY_SESSION`, `EDITED_IN_COMMIT`, `COMMIT_TOUCHED_FILE`,
     `NOTE_IN_THREAD`, `NOTE_FROM_SESSION`, `KNOWLEDGE_FROM_SESSION`,
     `DERIVED_FROM`, `SUPERSEDES`, and contradictions when present.
   - lifecycle items for design documents usually arrive through linked
     knowledge or agent entities; direct document-to-document lifecycle edges are
     not expected by default.
3. **Git context**
   - recent commits touching the file;
   - commit subjects, authors, dates, and linked work provenance;
   - imported/exported git-note provenance when present.
4. **Threads and notes**
   - notes whose entity refs, body, or thread links mention the document;
   - unresolved `dispute`, `assumption`, `followup`, and `blocked` first;
   - done notes only when they explain provenance or completion.
5. **Knowledge context**
   - knowledge entries directly linked to the document;
   - knowledge entries whose text cites the path/title;
   - supersession and contradiction chains for those entries.
6. **Implementation context**
   - project files and symbols linked from the design;
   - commits that mention both the design and source files;
   - roadmap items or work threads spawned from the design.
7. **Potential related**
   - hybrid search seeded by document title, headings, and representative chunks;
   - candidates collapsed by document path;
   - explanation labels: vector similarity, lexical overlap, shared neighbor,
     same thread, same commit.

The recipe should be deterministic and bounded. Results are ranked within each
section, not globally.

Bounds:

- Do not call `bbox_inspect_entity` per chunk through MCP. The server-side
  service reads EdgeIndex/store internals directly.
- Edge collection may consider all chunk refs but must cap rendered items per
  section with `max_items`.
- Full edge aggregation may consider at most `max_chunks_aggregated` document
  chunks, default 50. Larger documents return partial context plus degraded
  markers.
- Representative chunks for related-doc query seeds are capped to 5 by default:
  first heading chunk, largest prose chunk, most-linked chunk, and up to two
  additional high-information chunks.
- If a section exceeds its time or item budget, return partial results plus a
  degraded marker.

Whole-document embeddings would improve this recipe, but are not required for
v1. The current fallback is explicit: compose a small query set from the
document title, headings, and top representative chunks; retrieve candidates
with existing hybrid search internals; collapse results by document path; then
mark the section `confidence="suggested"`.

## Potential Related Documents

Potential links are high value but dangerous if they look authoritative.

The `potential_related` section must:

- use `confidence="suggested"`;
- include score and evidence class;
- avoid writing links automatically;
- collapse section/chunk matches to document-level candidates;
- prefer same-project docs, then knowledge entries, then transcript/session
  hits;
- exclude the current document and near-duplicate chunks from itself;
- show the strongest matching section preview;
- include "why shown" text that can be inspected without trusting the model.

Retrieval strategy:

1. Run project-scoped `project_file` searches for document candidates.
2. Run knowledge searches separately.
3. Optionally run session/transcript searches only when `include` asks for
   non-document candidates.
4. Collapse the `project_file` hit list from step 1 by canonical document path.
5. Interleave lanes with document candidates first, then knowledge, then
   transcript/session hits. Preserve the raw score and lane in each item.

The `why` string is deterministic. Template:

```text
<evidence-class summary> on <top terms>; <structural boost if present>
```

Examples:

- `vector + lexical overlap on dispatch, refactor, provenance`
- `vector + lexical overlap on dispatch, refactor, provenance; shared commit abc123`
- `shared thread thread-1234abcd; lexical overlap on rollout, plugin`

The `<evidence-class summary>` is derived from `evidence_classes`, for example
`vector_similarity` -> `vector` and `bm25_overlap` -> `lexical`.

Self-match exclusion is exact in v1: drop every candidate whose canonical
project-relative path equals the source document path, and drop every chunk ref
belonging to that path. Transcribed duplicates, copied design drafts, and
forked-doc similarity are deferred.

Example:

```json
{
  "kind": "potential_related",
  "title": "Potentially Related",
  "confidence": "suggested",
  "items": [
    {
      "id": "related:design/refactor-agents.md",
      "label": "design/refactor-agents.md",
      "summary": "Similar design language around atoms, dispatch, and refactor plan provenance.",
      "target_ref": "project_file:d723917f:<rel_path_hash>:<chunk_hash>:0",
      "target_path": "design/refactor-agents.md",
      "score": 0.82,
      "evidence_classes": ["vector_similarity", "bm25_overlap"],
      "why": "vector + lexical overlap on dispatch, refactor, provenance; shared commit abc123",
      "actions": [
        { "id": "open_note", "label": "Open note" },
        { "id": "accept_link_candidate", "label": "Accept relation" }
      ]
    }
  ]
}
```

Accepting a link candidate is a future explicit write action. It should not
rewrite the source markdown. The preferred target is a project-scoped relation
sidecar under `.bbox/`, which can be committed with the project and ingested by
a focused document-context relation importer.

Candidate sidecar shape:

```json
{
  "schema_version": "document_relation.v1",
  "source_path": "design/foo.md",
  "target_path": "design/refactor-agents.md",
  "relation_kind": "potential_related",
  "confidence": "suggested",
  "acceptance_state": "accepted",
  "acceptance_provenance": "operator_confirmed",
  "accepted": {
    "kind": "obsidian_document_context",
    "suggestion_id": "related:design/refactor-agents.md",
    "accepted_at": "2026-05-15T00:00:00Z"
  },
  "evidence": {
    "evidence_classes": ["vector_similarity", "bm25_overlap"],
    "score": 0.82,
    "why": "vector + lexical overlap on dispatch, refactor, provenance"
  }
}
```

The sidecar path should be stable and git-friendly, for example:

```text
.bbox/document-context/relations.jsonl
```

Use one JSONL file for the later write-capable phase. It is git-friendly,
append/merge friendly enough, easy for a focused importer to ingest, and avoids
catalog machinery. If relation volume becomes large, shard by source path hash
later. Do not start with per-edge files unless merge behavior demands it.

Accepted relations should ingest as first-class relation records, then project
queryable graph edges from those records. Do not collapse the JSONL entry into a
bare edge and lose why/who/when/evidence. The source relation record is the
durable provenance object; graph edges are an indexed projection for traversal.
Projected edges must carry metadata that preserves `confidence`,
`acceptance_state`, and `acceptance_provenance`, or use a distinct edge kind
that cannot be confused with an asserted content-authored link.

## Actions

Initial read-only actions:

| Action | Behavior |
|---|---|
| `open_entity` | Open a read-only entity detail view in the plugin pane. |
| `open_note` | Open a target vault file if `target_path` maps into the vault. |
| `copy_ref` | Copy entity ref or commit SHA. |
| `copy_evidence_bundle` | Ask server for a compact evidence bundle and copy it to clipboard. |
| `refresh` | Re-query the endpoint for the active file. |

Later explicit-write actions:

| Action | Behavior |
|---|---|
| `accept_link_candidate` | Write an accepted relation to project `.bbox/` sidecar state, not to the markdown document. |
| `create_followup_note` | Create a bbox followup note against the document. |
| `link_to_thread` | Link the document to an existing work thread. |

Write actions need a separate design pass because they cross from read-only
context into blackbox store mutation. Vault markdown mutation remains out of
scope unless a later design deliberately opts into it.

Copied evidence bundles should include both machine-stable refs and short human
labels: entity refs, path refs, commit SHAs, and session IDs for machines;
paths, commit subjects, thread titles, and session prompt/summary snippets for
readability outside blackbox-aware tools.

## Obsidian Plugin Design

### Settings

```json
{
  "blackboxUrl": "http://127.0.0.1:7264",
  "projectRoot": "/home/invidious/repos/transcript-search",
  "vaultRoot": "/home/invidious/repos/transcript-search",
  "enabledSections": [
    "provenance",
    "git",
    "lifecycle",
    "knowledge",
    "threads",
    "attention",
    "implementation",
    "explicit_links",
    "potential_related"
  ],
  "maxItems": 8,
  "refreshMode": "on-active-leaf-change"
}
```

The plugin can infer `vaultRoot` from Obsidian. `projectRoot` may differ when a
vault is a subdirectory or when multiple repos are mounted into one vault.

For multi-repo vaults, the plugin chooses a default project by longest-prefix
match among configured or registered project roots. If more than one match is
ambiguous, the plugin stores an explicit per-vault or per-folder mapping in
settings. The server endpoint does not guess; it receives `project` explicitly.

### Rendering

UI rules:

- Use one side pane, not generated notes, for default rendering.
- Group by section kind.
- Show confidence badges for `heuristic` and `suggested` items.
- Keep item summaries short and expandable.
- Prefer paths and commit subjects over raw entity refs in primary text.
- Keep entity refs visible in a details affordance for copy/paste.
- Do not block editor interaction while loading.

### Cache

The plugin should cache responses by:

```text
blackboxUrl + projectRoot + relativePath + activeFileMtime + include-set + generation
```

The server should return a `generation` or `ETag` derived from document index
state plus relevant store revisions when cheap. The plugin also uses a short
wall-clock TTL, default 30 seconds, because notes, threads, and knowledge can
change without touching the source file.

## Security And Safety

- The route is read-only in v1.
- Path resolution must reject traversal outside the registered project.
- The server should not expose arbitrary file reads through this endpoint.
- The plugin should connect only to loopback by default.
- The endpoint inherits the daemon's existing loopback trust boundary. If the
  daemon is configured to bind beyond loopback, this route is exposed with the
  rest of the operator HTTP surface and should be protected by the same
  deployment controls.
- Write actions must be disabled until explicitly configured.
- Suggested related links must not be represented as asserted edges.
- Response items should include enough evidence metadata for the UI to explain
  why they are present.
- If a requested section is unavailable, return a degraded marker instead of an
  empty success that hides missing infrastructure.
- Operator acceptance of a suggested relation should not rewrite the original
  confidence. Store separate fields: `confidence="suggested"`,
  `acceptance_state="accepted"`, and
  `acceptance_provenance="operator_confirmed"`. The vector rationale remains a
  suggestion; the operator confirms that the relation is useful.

## Implementation Plan

### Phase 0 - Shape And Fixtures

- Add fixture JSON for `document_context.v1`.
- Define Rust structs for the response model.
- Add snapshot tests for serialization.
- Include fixture coverage for `degraded` markers, top-level actions, and
  deterministic `why` strings.
- Use hand-built fixtures before wiring graph data.

### Phase 1 - Read-Only Server Endpoint

- Add `GET /context/document`.
- Add `POST /context/document/evidence-bundle`.
- Add shared `resolve_project_relative_path` helper.
- Resolve project/path and aggregate `project_file` chunks.
- Return basic document metadata plus degraded markers.
- Add route tests for path traversal, unregistered project, missing index, and
  same-project resolution.

### Phase 2 - Asserted Context Sections

- Implement `provenance`, `git`, `lifecycle`, `knowledge`, `threads`,
  `attention`, `implementation`, and `explicit_links` sections from existing
  stores and graph edges.
- Deduplicate by target entity ref and by target path.
- Rank section items with explicit edges before text matches.
- Use internal batched EdgeIndex/store lookups and section-level item caps.

### Phase 3 - Potential Related

- Build document query seeds from title, headings, and representative chunks.
- Call the existing hybrid search internals, not an HTTP/MCP loopback.
- Collapse candidates by document path.
- Return score, strongest preview, and evidence classes.
- Add tests that prevent self-matches and require `confidence="suggested"`.
- Track the whole-document embedding gap through `note-ff15b657`; do not block
  v1 on it.

### Phase 4 - MCP Wrapper

- Add `work_document_context` using the same service function.
- Document it as a workspace/document-context helper, not a raw graph primitive.
- Add the `work_document_context` stanza to `tool_docs.rs`.
- Keep `bbox_*` graph tools as the lower-level expert surface.

### Phase 5 - Obsidian Plugin Prototype

- Build a minimal TypeScript plugin under `integrations/obsidian/`:
  - separate `package.json`;
  - `node_modules` ignored;
  - plugin build excluded from Rust-only CI unless explicitly requested;
  - plugin versioning documented relative to daemon API schema version;
  - settings tab;
  - side pane;
  - active-file watcher;
  - HTTP fetch;
  - section renderer;
  - open-note and copy-ref actions.
- Keep write actions out of the prototype.

### Phase 6 - Explicit Writes

- Add separate confirmations and capability checks for:
  - a focused importer for `.bbox/document-context/relations.jsonl` with
    validation, replay diagnostics, and projected graph-edge emission;
  - accepting related-doc link candidates into `.bbox/` sidecar state;
  - creating bbox followup notes;
  - linking documents to work threads.
- Consider a separate endpoint namespace for writes, for example
  `POST /context/document/actions`.
- Keep existing document markdown immutable unless a future proposal explicitly
  changes that boundary.

## Acceptance Criteria

1. Opening a `design/*.md` file in Obsidian shows a Blackbox Context pane without
   the plugin speaking MCP.
2. The pane shows at least document metadata, git touches, explicit links, and
   potential related documents.
3. Potential related documents are visibly marked as suggested and are never
   written to the vault automatically.
4. A server test proves path traversal is rejected.
5. A server test proves chunk-level entities are aggregated to one document.
6. Section tests cover every asserted v1 section: `provenance`, `git`,
   `lifecycle`, `knowledge`, `threads`, `attention`, `implementation`, and
   `explicit_links`.
7. A server test proves self-matches are excluded from related candidates.
8. A server test proves section degradation returns a stable degraded marker.
9. A document with more than 50 chunks returns partial results plus degraded
   markers instead of blocking the editor-facing route indefinitely.
10. The same service function backs HTTP and `work_document_context`.

## Open Questions

1. What exact projected edge kind should accepted document relations use:
   `DOCUMENT_RELATED_TO`, `OPERATOR_RELATED_TO`, `RELATES_TO`, or an existing
   edge family with typed metadata?
2. Should accepted relation projection use a new document relation entity type,
   a generic artifact/sidecar entity type, or a knowledge-like entry with
   document-specific fields?
