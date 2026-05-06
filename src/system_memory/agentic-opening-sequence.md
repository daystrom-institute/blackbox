# Agentic opening sequence — orient, search, inspect, traverse, answer

This is the **default first-loop pattern** for any task that touches the
codebase, prior decisions, or conversational history. Run this sequence
before falling back to filesystem `grep`/`find` or to your training prior.

The donor McpPoc spike measured a 97% vs 23% recall improvement when
agents followed a structured open-loop instead of single-keyword recall.
The sequence below is bbox's adaptation: same shape, our entity types
and edge families.

## The five primitives

```
1. bbox_describe_schema           # orient — entity types + edge families
2. bbox_hybrid_search(query, k=5) # seeds — mixed-modal results with notable_edges
3. bbox_inspect_entity(ref)       # confirm — properties + edges in one call
4. bbox_find_paths(from, to_*)    # traverse — direction-preserving BFS chains
5. bbox_bundle_evidence(...)      # answer — package refs + path_ids
```

`bbox_blame(file, line)` is the line-level provenance escape hatch — use
when the question is "who/why does this line exist?" rather than a
graph walk.

`bbox_discover_seed_entities` is `bbox_hybrid_search` plus emphasis on
notable_edges for orientation; either tool returns seeds you can hand
to step 3.

## Domain orientation (memorize once per session)

**12 entity types** the graph contains:

| Type | Population | Use it for |
|---|---|---|
| `knowledge` | rules, decisions, conventions | "what's the policy on X?" |
| `project_file` | source/doc chunks | "where does X live?" / "what does Y do?" |
| `transcript` | one block of one Claude/Codex/Gemini session | "what did this turn say?" |
| `session` | a full agent conversation | "what was that session about?" |
| `thread` | persistent investigation across sessions | "what's the deferred-items work?" |
| `note` | structured side-channel records (dispute/done/etc) | "what's pending review?" |
| `symbol` | named code symbols (functions, types, modules) | "what calls X?" |
| `brofile` | persona+model+lens triple | "what brofile dispatched this?" |
| `whiteboard` | multi-agent deliberation surface | "what did the contradiction-review board decide?" |
| `commit` | git commits with parent + touched-file edges | "what changed in commit X?" |
| `task` (virtual) | bro_exec dispatch unit | "what produced this artifact?" |
| `bash_call` (virtual) | one shell invocation in a transcript | "what did this command emit?" |

**7 edge families**:

- **Structural** (`IN_FILE`, `IN_SESSION`, `THREAD_HAS_SESSION`, `NEXT_SECTION`, `NEXT_CHUNK`, `PREV_CHUNK`) — containment + sequence
- **AST** (`DEFINED_IN`, `CONTAINS_SYMBOL`, `HAS_FIELD`, `IMPLEMENTS_TRAIT`, `CALLS`, `USES_TYPE`) — code navigation
- **Knowledge** (`SUPERSEDES`, `DERIVED_FROM`, `Contradicts`, `KNOWLEDGE_FROM_SESSION`, `KNOWLEDGE_FROM_BOARD`) — rule lifecycle
- **Provenance** (`SESSION_USED_BROFILE`, `ARC_USED_BROFILE`, `ARC_OPENED_BOARD`, `NOTE_FROM_SESSION`, `NOTE_IN_THREAD`, `NOTE_FROM_TASK`, `TASK_PRODUCED_NOTE`) — origin trails
- **Git** (`COMMIT_PARENT`, `COMMIT_TOUCHED_FILE`, `COMMIT_PRODUCED_BY_ARC`) — version control history
- **Format-specific** (`LINKS_TO_FILE`, `LINKS_TO_SECTION`, `DESCRIBES`, `ON_PAGE`, `FIGURE_OF`, `TABLE_OF`) — cross-reference within docs
- **Tool-call** (`EDITED_FILE`, `EDITED_BY_SESSION`, `READ_FILE`, `RAN_BASH`) — agent activity provenance

`bbox_describe_schema` returns the live counts and the schema-aware tip
for each family.

## Hard rules (reads like a contract — break these and quality collapses)

1. **Entity refs are canonical.** Every API takes `<type>:<segments>`.
   `project_file:<project_id>:<rel_path_hash>:<chunk_hash>:<occurrence_idx>`.
   `commit:<repo_id>:<sha>`. `knowledge:<id>`. `transcript:<provider>:<session_id>:<line_offset>:<event_idx>`.
   When `bbox_inspect_entity` returns an `error.bad_input` with a
   `suggested_fix`, use the suggestion verbatim — don't guess.

2. **Don't restate paths from memory.** `bbox_find_paths` returns stable
   `path_ids` (`P1`, `P2`, ...) cached server-side. Pass those IDs to
   `bbox_bundle_evidence`. Reconstructing path text from your turn buffer
   loses fidelity (the server holds the validated graph; you hold a
   summary).

3. **Targeted inspection beats broad inspection.** When you call
   `bbox_inspect_entity`, pass `edge_types="CONTAINS_SYMBOL,DEFINED_IN"`
   and `direction="out"` (or `"in"`) when you know the relationship you
   care about. The default `direction="both"` with all edge types is for
   first-time orientation only — after that, narrow.

4. **Follow recommended_next_hops.** `bbox_inspect_entity` returns a
   `recommended_next_hops` list ordered by edge family priority (semantic
   first, structural last). The first non-zero entry is almost always
   what you want next. Don't traverse `IN_FILE` self-relationships
   blindly.

5. **Trust topical hits.** `bbox_hybrid_search` blends BM25 + vector
   + path-token boost. If the top seed is a topical match without
   exact wording overlap, treat it as the canonical entity for the
   query — don't insist on a literal-string match. The vector lane
   exists precisely to catch paraphrases.

6. **Per-file collapse is on by default.** Search and find_paths return
   ONE entity per file by default (the highest-scoring or shortest-
   path chunk). If you need multiple chunks of the same file, the
   chunk's notable_edges already point you to siblings via
   `NEXT_SECTION`.

## Final-answer protocol — verify by question type

Before sending your answer, walk this checklist for the question shape:

**WHERE** ("where is X defined?"): your answer MUST cite a
`project_file` entity_ref AND its file_path + (optional) line. If a
`symbol` entity exists for X, also cite the `DEFINED_IN` target.

**WHAT** ("what does X do?"): cite the `project_file` chunk's
`content_preview` from `bbox_bundle_evidence`. For decisions/conventions,
cite the `knowledge` entity directly.

**WHO/WHEN** ("who wrote X?", "when did this change?"): cite the
`commit` entity (`COMMIT_TOUCHED_FILE` traversal) AND the
`session` (`EDITED_BY_SESSION`) when one exists. `bbox_blame` gives
both in one call for a known file/line.

**WHY** ("why does X exist?", "what was the rationale?"): trace
`KNOWLEDGE_FROM_SESSION` from a knowledge entry to the originating
session, OR `DERIVED_FROM`/`SUPERSEDES` to the lineage. A bare
"this is the current rule" answer without the originating trail is
incomplete.

**REPLACEMENT** ("what replaced X?", "what's the current version?"):
cite BOTH the old (`SUPERSEDES` source) AND the new (`SUPERSEDES`
target) entities. State the supersession direction explicitly.

**HOW** ("how does X work?"): assemble a
`bbox_bundle_evidence(question, [code_chunks, design_docs])` answer
kit. The bundle's `intra_bundle_edges` field shows relationships
between cited entities (when implemented).

**HISTORICAL** ("trace the chain"): every step in your narrative MUST
be grounded in a validated `path_id` from `bbox_find_paths`. State
edge directions as the path returned them — do not invert from
memory ("X reads Y" stays as `READ_FILE` from X to Y; do not flip it
to "Y was read by X" without re-querying).

**IMPACT** ("what gets affected if X changes?"): traverse outward
from X via `CALLS`, `IMPLEMENTS_TRAIT`, `EDITED_IN_COMMIT` →
downstream commits → other touched files. Stop at a depth where
edge-confidence drops to `Heuristic` and surface that as a caveat.

## Common patterns

### "Where is the implementation of X?"

```
1. bbox_hybrid_search(query="X implementation", k=5)
2. Pick the top result whose chunk_kind=code_block; if none in top 5,
   the modal-diversification slot at the bottom will have one.
3. bbox_inspect_entity(ref, edge_types="CONTAINS_SYMBOL,DEFINED_IN")
4. Answer with the file_path + symbol name from properties.
```

### "Who/when last edited file X line N?"

```
1. bbox_blame(file="absolute/path", line=N)
2. The response includes git_blame.{commit_sha, author, author_time}
   AND, if a bbox-tracked tool call matches the commit, the full
   anchor chain (session, brofile, threads).
```

### "What's our policy on X?"

```
1. bbox_knowledge(query="X policy")           # rendered rules first
2. If empty: bbox_hybrid_search(query="X")    # broader recall
3. For decisions: bbox_inspect_entity(ref) and follow SUPERSEDES out.
```

### "What did session S do?"

```
1. bbox_session(session_id="S")               # metadata + first prompt
2. bbox_inspect_entity("session:provider:S",
       edge_types="EDITED_BY_SESSION,READ_FILE",
       direction="in")
3. bbox_find_paths(from="session:provider:S", to_type="commit",
       max_depth=2)                            # commits this session produced
```

### "Why does the codebase have Y convention?"

```
1. bbox_knowledge(query="Y")
2. For each entry: bbox_inspect_entity(ref, edge_types="KNOWLEDGE_FROM_SESSION,DERIVED_FROM,SUPERSEDES")
3. Walk DERIVED_FROM until you reach the original session or commit.
```

## Anti-patterns

- **Single bbox_knowledge call as the entire grounding step.** Knowledge
  is rendered RULES, not corpus. Most questions need search-or-graph too.
- **Iterating bbox_search 5 different ways.** If 2-3 reformulations
  don't surface the answer, switch to `bbox_hybrid_search` (vector lane
  catches paraphrases) or `bbox_describe_schema` (you may be looking at
  the wrong entity type).
- **Inventing entity refs.** If you didn't read it from a tool response
  this turn, query for it. The bad_input error returns a `suggested_fix`.
- **Truncating paths in the answer.** When the user asks
  HISTORICAL/REPLACEMENT/WHY, every link in your narrative needs a
  validated path_id from `bbox_find_paths`. Don't paraphrase the chain.
- **Skipping bundle_evidence.** Without it, you're packaging the answer
  by hand from your scratch-buffer. The bundle gives the user
  inspectable refs + path_ids they can re-query if they doubt your
  answer.

## Keep hot vs cold

Always rendered into provider markdown (BLACKBOX.md / CLAUDE.md /
AGENTS.md / GEMINI.md):

- The five primitives + their order
- The hard rules
- Pointer to this runbook by ID

Cold here (only fetched when the agent escalates):

- The full domain orientation table
- The final-answer protocol checklist
- The common-pattern recipes
- The anti-patterns list
