+++
title = "Agentic opening sequence: recall, search, inspect, answer"
tags = ["opening", "opening-sequence", "grounding", "first-step", "first-loop", "agentic", "agentic-tools", "discover", "inspect", "inspect-entity", "where", "what", "why", "who", "how", "when", "history", "provenance", "search-quality", "answer-protocol", "verification", "self-check", "answer"]
order = 0
template = false
+++
# Agentic opening sequence: recall, search, inspect, answer

This is the **default first-loop pattern** for any task that touches the
codebase, prior decisions, or conversational history. Run it before
falling back to filesystem `grep`/`find` or to your training prior.

## The three primitives

```
1. bbox_knowledge(query)          # recall: rendered rules, decisions, runbooks
2. bbox_hybrid_search(query, k=5) # seeds: mixed-modal ranked entity refs
3. bbox_inspect_entity(ref)       # confirm: properties and provenance of one ref
```

Conversation hits feed `bbox_context`, `bbox_messages` and `bbox_session`
unchanged; use them to read the surrounding turns of a transcript hit.

## Entity types

| Type | Population | Use it for |
|---|---|---|
| `knowledge` | rules, decisions, conventions | "what's the policy on X?" |
| `project_file` | source/doc chunks | "where does X live?" / "what does Y do?" |
| `transcript` | one block of one agent session | "what did this turn say?" |
| `session` | a full agent conversation | "what was that session about?" |
| `thread` | persistent investigation across sessions | "what's the deferred-items work?" |
| `commit` | indexed git commits | "what changed in commit X?" |
| `project_graph_vertex` | a vertex of an authored project graph | "what does this record correspond to?" |
| `system_memory` | code-owned runbooks like this one | "how do I do X with blackbox?" |

`bbox_inspect_entity` reads the stored properties of any of these. Project
graph vertices also carry their graph edges and evidence bindings, and a
project file or knowledge entry named by an evidence binding shows that
binding with its freshness.

## Hard rules

1. **Entity refs are canonical.** Every API takes `<type>:<segments>`.
   `project_file:<project_id>:<rel_path_hash>:<chunk_hash>:<occurrence_idx>`.
   `commit:<repo_id>:<sha>`. `knowledge:<id>`.
   `transcript:<provider>:<session_id>:<line_offset>:<event_idx>`.
   When a tool returns `error.bad_input` with a `suggested_fix`, use the
   suggestion verbatim; don't guess.

2. **Read exact values instead of paraphrasing previews.** Inspection
   shortens long text by default. Use `property=<key>` to page the stored
   value, and cite what the page says.

3. **Trust topical hits.** `bbox_hybrid_search` blends BM25, vector and
   path-token boost. If the top seed is a topical match without exact
   wording overlap, treat it as the canonical entity for the query. The
   vector lane exists to catch paraphrases.

4. **Per-file collapse is on by default.** Search returns one entity per
   file (the highest-scoring chunk). Inspect the chunk or read the file
   when you need its neighbors.

5. **Report evidence freshness.** When an evidence binding reports a
   `stale` or `missing` endpoint, say so; a binding records what was
   asserted, not that it is still true.

## Final-answer protocol: verify by question type

**WHERE** ("where is X defined?"): cite a `project_file` entity_ref and
its file path.

**WHAT** ("what does X do?"): cite the `project_file` chunk you read. For
decisions and conventions, cite the `knowledge` entity directly.

**WHO/WHEN** ("who wrote X?", "when did this change?"): cite the `commit`
hit from `bbox_hybrid_search(doc_type="commit")` or the git history.
Search transcripts for the file path to find the sessions that touched it.

**WHY** ("what was the rationale?"): cite the knowledge entry and, for a
project entry, the git history of `.bbox/knowledge/<id>.json`, which
carries prior versions and the reason for each change. Search transcripts
for the originating conversation. A bare "this is the current rule"
answer without the originating trail is incomplete.

**REPLACEMENT** ("what replaced X?"): cite both the old and the new
entity with the evidence that links them, and state the replacement
direction explicitly.

**HISTORICAL** ("trace the chain"): ground every step in a ref you read
this turn. Do not reconstruct a chain from memory.

## Common patterns

### "Where is the implementation of X?"

```
1. bbox_hybrid_search(query="X implementation", k=5)
2. Pick the top result whose chunk_kind=code_block; if none in top 5,
   the modal-diversification slot at the bottom will have one.
3. bbox_inspect_entity(ref, property_mode="full")
4. Answer with the file path and symbol name from properties.
```

### "What's our policy on X?"

```
1. bbox_knowledge(query="X policy")           # rendered rules first
2. If empty: bbox_hybrid_search(query="X")    # broader recall
3. For a project entry's history: git log .bbox/knowledge/<id>.json
```

### "What did session S do?"

```
1. bbox_session(session_id="S")               # metadata + first prompt
2. bbox_messages(...) for the exact turns
```

## Anti-patterns

- **A single bbox_knowledge call as the entire grounding step.**
  Knowledge is rendered rules, not corpus. Most questions need search too.
- **Iterating keyword queries five different ways.** If two or three
  reformulations don't surface the answer, drop the narrowing filters on
  `bbox_hybrid_search` (the vector lane catches paraphrases) or change
  `doc_type`.
- **Inventing entity refs.** If you didn't read it from a tool response
  this turn, query for it.
