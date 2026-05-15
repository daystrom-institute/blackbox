---
title: "Commit to Work Provenance Design"
kind: design
lifecycle: archived
corpus: blackbox-design
topic:
  - corpus
---

# Commit to Work Provenance Design

Date: 2026-05-07

## Problem

Blackbox indexes commits as graph entities and indexes threads/notes/tasks as
work entities, but the graph has no first-class durable edge for "this commit
resolved or implemented this work item."

Current state:

- Git indexing emits `COMMIT_PARENT` and `COMMIT_TOUCHED_FILE`.
- Notes emit `TASK_PRODUCED_NOTE`, `NOTE_FROM_TASK`, `NOTE_FROM_SESSION`, and
  `NOTE_IN_THREAD` when the corresponding fields exist.
- Threads emit `THREAD_HAS_SESSION` plus manual thread-to-thread/session edges:
  `THREAD_SPAWNED_FROM`, `THREAD_BLOCKED_BY`, `THREAD_RELATES_TO`, and
  `THREAD_SUBSUMES`.
- Provider schemas advertise `ARC_PRODUCED_COMMIT` /
  `COMMIT_PRODUCED_BY_ARC`, but no code currently emits those edges.
- `bbox_thread(action="link")` only accepts `target_type=thread|session`, so
  it cannot link a thread to `commit:<repo_id>:<sha>`, `note:<id>`,
  `project_file:<...>`, or other entity refs.

This document is a forward design, not a description of implemented behavior.
Except for the existing git/thread/note edges listed above, every new edge kind,
store, and MCP surface below is proposed work. Until Phase 1 lands, commit to
thread/note provenance remains prose-only or inference-only.

The result is provenance by prose and inference. A commit may mention a thread
id in its message, or a thread note may mention a SHA, but graph traversal
cannot reliably answer:

- Which commit closed this thread?
- Which thread did this commit implement?
- Which `done` note verified this commit?
- Which session/task produced the commit?
- Which design doc or project file became the durable spine for this work?

## Existing Edges Worth Keeping

Do not replace these:

- `COMMIT_PARENT`: commit ancestry.
- `COMMIT_TOUCHED_FILE`: commit to changed project-file chunks.
- `NOTE_IN_THREAD`: side-channel note membership.
- `NOTE_FROM_SESSION`: note authoring session.
- `TASK_PRODUCED_NOTE` / `NOTE_FROM_TASK`: task-to-note provenance.
- `THREAD_HAS_SESSION`: explicit thread/session association.

The missing layer is not file diff provenance. It is work-object provenance:
commit, thread, note, task, session, and durable artifact links.

## Design Goals

1. A commit can be explicitly linked to the work item it implements.
2. A note can be explicitly linked to a commit it reviews, verifies, blocks, or
   supersedes.
3. A thread can be explicitly linked to any graph entity, not only another
   thread or session.
4. Automatic edges are added when the system has high-confidence scope data.
5. Prose remains useful context but is not the authoritative link.
6. Git notes export/import can carry the same provenance across clones.
7. Destructive cleanup of bad links is explicit and auditable.

## Edge Vocabulary

Use direction names that read naturally from the entity that owns the action.
Every edge has a reverse traversal automatically through the graph index; avoid
defining duplicate inverse edge kinds unless the storage layer requires it.

### Commit Edges

- `COMMIT_PRODUCED_BY_TASK`
  - Source: `commit`
  - Target: `task`
  - Meaning: this dispatch task authored or landed the commit.

- `COMMIT_PRODUCED_BY_SESSION`
  - Source: `commit`
  - Target: `session`
  - Meaning: this provider session authored or landed the commit.

- `COMMIT_RESOLVES_THREAD`
  - Source: `commit`
  - Target: `thread`
  - Meaning: the commit closes or materially resolves the thread's work.

- `COMMIT_ADVANCES_THREAD`
  - Source: `commit`
  - Target: `thread`
  - Meaning: the commit moves the thread forward but does not close it.

- `COMMIT_VERIFIED_BY_NOTE`
  - Source: `commit`
  - Target: `note`
  - Meaning: a done/review note records acceptance or verification for the
    commit.

### Note Edges

- `NOTE_REVIEWS_COMMIT`
  - Source: `note`
  - Target: `commit`
  - Meaning: the note is a review, verdict, blocker, or residual-risk record
    about the commit.

- `NOTE_REFERENCES_ENTITY`
  - Source: `note`
  - Target: any `EntityRef`
  - Meaning: the note intentionally points at an artifact but no stronger
    relationship applies.

### Thread Edges

- `THREAD_REFERENCES_ENTITY`
  - Source: `thread`
  - Target: any `EntityRef`
  - Meaning: generic durable pointer from a work thread to a design doc, commit,
    note, whiteboard, project file, or other graph entity.

- `THREAD_RESOLVED_BY_COMMIT`
  - Source: `thread`
  - Target: `commit`
  - Meaning: explicit user/operator claim that the commit resolves the thread.
    This is the direct inverse of `COMMIT_RESOLVES_THREAD` but useful at the
    thread API boundary. Projection may emit both directions from one stored
    record if bidirectional query ergonomics require it.

### Arc Edges

Keep the advertised names but define them concretely:

- `ARC_PRODUCED_COMMIT`
  - Source: `thread` where `kind=work_item` and opened by workflow arc.
  - Target: `commit`
  - Meaning: the workflow arc produced the commit.

- `COMMIT_PRODUCED_BY_ARC`
  - Source: `commit`
  - Target: `thread`
  - Meaning: inverse of `ARC_PRODUCED_COMMIT`.

These should be emitted together from the same underlying link record when the
source thread represents an arc.

## Storage Model

Do not overload `ThreadEdge` beyond recognition. Add a generic graph-link store:

```rust
struct WorkGraphLink {
    id: String,                  // link-<8hex>
    source: String,              // canonical EntityRef
    kind: String,                // edge kind
    target: String,              // canonical EntityRef
    provenance: LinkProvenance,  // manual | derived | imported
    confidence: EdgeConfidence,  // exact | heuristic | unknown
    project: Option<String>,
    task_id: Option<String>,
    session_id: Option<String>,
    provider: Option<String>,
    note: Option<String>,
    created_at: String,
    created_by: LinkAuthor,
}

enum LinkAuthor {
    User,
    Agent { provider: String, session_id: Option<String> },
    System,
    Import { source: String },
}
```

Persist at `~/.local/state/blackbox/work-graph-links.json` or the existing
daemon state root equivalent. EdgeIndex projection reads this store and emits
exactly the stored `source -> kind -> target` edge, plus configured inverse
edges for ergonomic pairs.

This store deliberately sidesteps the current `ThreadEdge` limitation:

- `threads::EdgeTarget` has only `Thread` and `Session` variants.
- `ThreadEdge.target` is a raw thread/session id, not a canonical `EntityRef`.
- Widening `ThreadEdge` would mix thread lifecycle relations with arbitrary
  graph links and would require compatibility handling for old records.

`WorkGraphLink.source` and `WorkGraphLink.target` are canonical `EntityRef`
strings from the start. `bbox_thread(action="link_entity")` is only a
convenience wrapper that writes a `WorkGraphLink`; it must not append a
`ThreadEdge`.

`WorkGraphLink.provenance` is also intentionally separate from
`chunker::EdgeProvenance`. Current projected edges only support
`Explicit|Derived|Implicit`. For v1 projection:

- `manual` and `imported` links project as `EdgeProvenance::Explicit`
- `derived` and `system` links project as `EdgeProvenance::Derived`
- the original link provenance is preserved in edge metadata as
  `link.provenance`

If future graph consumers need imported/system as first-class provenance values,
that should be a separate enum/schema migration. The generic link store should
not require that migration to ship.

Why a separate store:

- It can link any `EntityRef`, not just thread/session targets.
- It avoids schema churn in thread records for every new edge kind.
- It gives deletion/review a stable link id.
- It can import git notes without mutating thread/note source records.

## Tool Surface

Add one generic tool rather than widening every existing tool:

```text
bbox_link(action="add|get|list|remove",
          source="<EntityRef>",
          edge="<EDGE_KIND>",
          target="<EntityRef>",
          project="/repo",
          note="optional rationale")
```

Validation:

- Parse `source` and `target` through `EntityRef::parse`.
- Reject unknown edge kinds unless `edge` is registered in the graph schema.
- Enforce source/target type constraints for known edge kinds.
- Dedupe exact `(source, edge, target)`.
- `remove` requires link id, not just tuple, so deletion is auditable.

Convenience wrappers:

- `bbox_thread(action="link_entity", id, target="<EntityRef>", edge=...)`
  delegates to `bbox_link`.
- `bbox_note` accepts optional `links=[{edge,target,note}]` and stores them via
  `bbox_link` after note creation.
- `bro_commit_link` is not needed; commit links are graph links.

List-before-create applies: callers should `bbox_link(action="list", source=,
target=)` before adding a link if the tool does not already perform dedupe.

## Automatic Capture

### Commit Hook Capture

When an agent runs a git commit command inside a scoped task/session:

1. Detect new HEAD after a successful commit command.
2. Resolve repo id from the project registry.
3. Create `commit:<repo_id>:<sha>`.
4. If ambient task is known, add `COMMIT_PRODUCED_BY_TASK`.
5. If ambient session is known, add `COMMIT_PRODUCED_BY_SESSION`.
6. If ambient thread/work_item is known:
   - add `COMMIT_ADVANCES_THREAD` by default
   - add `ARC_PRODUCED_COMMIT` / `COMMIT_PRODUCED_BY_ARC` if the thread is a
     workflow arc
7. If the task emits a `done` note after the commit, link
   `COMMIT_VERIFIED_BY_NOTE` and `NOTE_REVIEWS_COMMIT`.

Only exact post-command HEAD changes should produce exact links. If a commit SHA
is parsed from prose or a command string without confirming HEAD, mark the link
heuristic or do not create it.

### Commit Message Parsing

Support optional explicit trailers:

```text
BBox-Thread: thread-3cfbf9e0
BBox-Task: dccd4d03-6007-4bae-b5de-fef8e4487814
BBox-Note: note-4a948e1e
```

Do not require trailers for agent-authored commits. Ambient capture should be
the normal path. Trailers are for human commits, imported commits, or recovery.

### Git Notes Import/Export

Extend existing `bbox_provenance_export` / `bbox_provenance_import`:

- Export all `WorkGraphLink` records touching a commit into
  `refs/notes/bbox/provenance`.
- Import merges links by stable `(source, kind, target)` plus provenance
  metadata.
- Imported links use `provenance=imported` unless the note includes an exact
  anchor that can be validated locally.

This lets "commit resolves thread" survive clone/fetch even when local thread
ids are host-local. If a target thread id is missing locally, preserve the edge
as a dangling entity ref and report it as unresolved rather than dropping it.

## EdgeIndex Projection

Projection sources after this design:

- thread store: current thread/session edges
- note store: note/task/session/thread edges
- git index: commit ancestry and touched files
- work graph link store: generic explicit/derived/imported links
- tool-call provenance: existing transcript/bash/edit edges

`bbox_describe_schema` must advertise only edge kinds with an emitter or a
stored-link path. If `ARC_PRODUCED_COMMIT` remains in schema, the work-link
projector is the emitter.

Provider updates:

- Commit provider expected edges:
  - `COMMIT_PARENT`
  - `COMMIT_TOUCHED_FILE`
  - `COMMIT_PRODUCED_BY_TASK`
  - `COMMIT_PRODUCED_BY_SESSION`
  - `COMMIT_RESOLVES_THREAD`
  - `COMMIT_ADVANCES_THREAD`
  - `COMMIT_VERIFIED_BY_NOTE`
  - `COMMIT_PRODUCED_BY_ARC`
- Thread provider expected edges:
  - existing thread edges
  - `THREAD_REFERENCES_ENTITY`
  - `THREAD_RESOLVED_BY_COMMIT`
  - `ARC_PRODUCED_COMMIT`
  - incoming `COMMIT_ADVANCES_THREAD` and `COMMIT_RESOLVES_THREAD`
- Note provider expected edges:
  - existing note edges
  - `NOTE_REVIEWS_COMMIT`
  - `NOTE_REFERENCES_ENTITY`
  - incoming `COMMIT_VERIFIED_BY_NOTE`

## Migration Plan

Phase 0: Schema honesty

- Either remove `ARC_PRODUCED_COMMIT` / `COMMIT_PRODUCED_BY_ARC` from provider
  schemas and `bbox_describe_schema`, or land Phase 1 projection in the same
  change that advertises them.
- Add a unit test that every advertised edge kind has at least one emitter:
  native projector, generic link projector, or explicit documented virtual
  source. Ghost schema edges should fail the build.

Phase 1: Generic link store

- Add `WorkGraphLink` store.
- Add parser/validator for source/edge/target triples.
- Project links into EdgeIndex.
- Add `bbox_link(action=list|add|get|remove)`.
- Update schema docs and provider expected edges.
- Preserve `link.provenance`, `link.id`, and `link.created_at` in projected edge
  metadata.

Phase 2: Thread/note convenience

- Add `bbox_thread(action="link_entity")`.
- Add optional note links on `bbox_note`.
- Migrate future "point this thread at this doc/commit" workflows to
  structured links.
- Do not widen `ThreadEdge` for arbitrary entity refs; keep thread lifecycle
  edges and generic graph links as separate stores.

Phase 3: Commit capture

- Detect successful agent git commits and add task/session/thread links.
- Add tests with a temporary git repo and a fake ambient scope.
- Add commit trailer parsing for human/recovery paths.
- Scope of "detect successful commit" for v1: commands that leave `HEAD`
  changed in the task's project repo. If a task creates multiple commits, link
  every new commit in `old_head..new_head`.
- Do not infer commit production from arbitrary SHA text in prose.

Phase 4: Git notes portability

- Export/import work links touching commits.
- Preserve dangling refs on import.
- Add provenance import/export tests.
- Imported links are stored as `WorkGraphLink { provenance=imported }` and then
  projected through the normal link projector. Do not write imported links
  directly to project edge JSONL.

Phase 5: Cleanup and lint

- Add `bbox_lint` checks for:
  - prose-only SHA mentions in threads/notes without structured links
  - schema edge kinds with no emitter
  - dangling imported thread/note refs
- Add an optional repair assistant that proposes `bbox_link` calls, but never
  auto-adds links without an explicit operator action.

## Acceptance Criteria

- Before implementation, the doc is clear that `WorkGraphLink`, `bbox_link`,
  commit capture, and work-link git notes do not exist yet.
- A thread can link to `commit:<repo_id>:<sha>` and
  `project_file:<project_id>:<rel_path_hash>:<chunk_hash>:<occurrence_idx>`
  without prose-only notes.
- A commit can be traversed to the task, session, thread, and verification note
  that produced it when those scopes are available.
- `ARC_PRODUCED_COMMIT` and `COMMIT_PRODUCED_BY_ARC` are either emitted from the
  work-link store or removed from advertised schema. No advertised ghost edges.
- Generic thread/entity links are stored in `WorkGraphLink`, not `ThreadEdge`.
- Link provenance is preserved even if projected `EdgeProvenance` remains
  `Explicit|Derived|Implicit`.
- `bbox_bundle_evidence` can include a commit and thread and show the
  intra-bundle link without relying on text search.
- Duplicate links are rejected or treated as idempotent.
- Removing a bad link requires a link id and records deletion metadata.
- Git notes export/import preserves commit-linked work provenance across clones.

## Open Questions

- Should `THREAD_RESOLVED_BY_COMMIT` be stored directly, or always derived as
  the inverse of `COMMIT_RESOLVES_THREAD`?
- Should human commits use trailers by convention, or should `bbox_link` after
  commit be the preferred manual workflow?
- How should host-local thread ids be reconciled when imported git notes point
  at threads absent on the receiving machine?
- Should file/design-doc links use the current chunk-level `project_file` refs
  or wait for the planned virtual `file:<project_id>:<rel_path_hash>` entity?
