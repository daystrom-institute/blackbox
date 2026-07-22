---
title: "Checkout-plane provenance export implementation plan"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - daemon-runtime
tags: [decomposition, provenance, git-notes, bro-harness, bro-cli, strangler]
brief: "Move provenance Git-note writes out of blackboxd without introducing an untrusted inbound edge-ingest path; retain the legacy adapter during overlap and defer import extraction until its authority channel is designed."
---

# Checkout-plane provenance export implementation plan

Date: 2026-07-21

Companion design:
[`locality-first-decomposition.md`](locality-first-decomposition.md), sequencing
slice 2.

## 1. Outcome and scope

This is the first bounded harness-ward move in the checkout-plane strangler.
It moves **provenance export**, specifically the Git-note write, out of
`blackboxd` and into the process that owns the checkout.

At the end of this slice:

- the defects found by the first full-scope Kimi pass on 2026-07-21 are
  repaired before the provenance boundary moves;
- the corpus side can prepare a deterministic, paginated provenance export
  plan without reading a checkout or invoking Git;
- `bro provenance export` can request that plan and write the notes in its
  local checkout;
- the Git-note document schema is owned by a dependency-clean shared crate and
  includes the target entity needed by a future checkout-independent import;
- the existing `bbox_provenance_export` and `bbox_provenance_import` adapters
  remain available during the strangler overlap.

This slice deliberately does **not** accept note documents from a model or an
arbitrary MCP client and append them to the central edge sidecar. The current
import derives its evidence by reading a registered repository itself. Replacing
that with caller-supplied JSON without an authenticated checkout-source channel
would expand write authority and permit forged or cross-project graph edges.
Import extraction therefore remains a separate gate, expected to use either a
server-authoritative harness checkout identity or the authenticated producer
channel introduced by the code-corpus collector.

## 2. Current coupling

`bbox_provenance_export` currently performs two different jobs inside one
daemon tool call:

1. It reads `EdgeIndex`, groups `EDITED_FILE` and `READ_FILE` anchors by
   project and commit, joins session, brofile, and thread provenance, and
   serializes note documents.
2. It shells out to Git in each registered checkout to configure the note merge
   strategy and write `refs/notes/<namespace>/provenance`.

The first job belongs to the corpus plane. The second belongs to the checkout
plane. The existing private `GitProvenanceNote` schema hides that boundary, and
it omits the original target entity. Import compensates by reading and chunking
the current checkout file to reconstruct the target, which is another
checkout-local dependency.

## 3. Boundary decisions

### 3.1 Split planning from application

Add a corpus-only MCP tool named `bbox_provenance_export_plan`. It performs no
Git or filesystem I/O. It returns a page of fully serialized note documents
plus the exact durable checkout scope they belong to.

Add `bro provenance export` as the operator-facing composite. It initializes an
MCP session with the canonical local project path as transport context, pages
through `bbox_provenance_export_plan`, and applies each page through the same
dependency-clean local library used by the legacy adapter's schema path.

There is no harness end-to-end composition in this slice. Dispatch selfbox MCP
URLs currently carry a surface but no project transport context, so dispatched
sessions correctly have no authoritative checkout. Appending a project to every
dispatch URL would also grant every harness session default-own provisional
knowledge and gap visibility. That system-wide authority posture is not hidden
inside a provenance change. A later reviewed authority decision may add a local
binding and trusted transport plumbing; the governing design explicitly allows
provenance to move through a harness binding and/or a `bro` CLI verb.

### 3.2 Authority comes from MCP session context

`bbox_provenance_export_plan` has no `project`, `project_id`, path, or scope
argument. It requires `BlackboxServer::authoritative_session_checkout()` and
uses the checkout's server-materialized `project_id` to select the exact
registered project. Tool arguments cannot redirect the plan to another
project.

Add `project_id` to `ResolvedCheckoutScope`. The conservative write resolver
sets it from the `ProjectRecord` at the same time it constructs the checkout's
durable `PublishedScope`; MCP initialization stores that resolved value. The
planner reads only this session value plus the in-memory project registry and
edge index. It does not re-elect a publisher or re-read config on each call.

The tool fails closed when:

- the MCP session has no authoritative checkout;
- the session checkout has no recorded durable `PublishedScope`;
- its authoritative `project_id` has no matching central project record.

The export plan carries both the durable `PublishedScope` and the host-local
`project_id`. The local writer validates the durable scope only. The
host-local id is retained inside entity refs because it identifies the current
central corpus generation and is not treated as cross-host authority.

### 3.3 Shared schema, dependency-clean local crate

Create a leaf crate named `bbox-provenance`. It owns:

- the versioned Git-note document structs;
- the export plan and page structs;
- deterministic serialization and hashing;
- note-reference validation;
- local published-scope resolution for an explicitly confined project root;
- local Git-note page application.

Allowed internal dependencies are `bbox-corpus-core`, `bbox-config`, and small
serialization, hashing, and error crates. The crate must not depend on
`blackbox`, `bbox-edge-index`, `bbox-indexing`, `bbox-chunker`, Tantivy, V8,
`bro-harness`, or `bro-cli`. A dependency acceptance test checks this forbidden
set through a new `scripts/acceptance-provenance-deps.sh` using the resolved
`cargo tree`, matching `scripts/acceptance-fleetd-deps.sh`. Both dependency
ceilings run explicitly alongside the focused and closeout verification gates.

`bbox-mcp-tools` and `bro-cli` may depend on this leaf. No runtime implementer
depends on another runtime implementer. `bro-harness` does not gain this
dependency in this slice.

### 3.4 Version the document additively

Move the current private schema into `bbox-provenance` and add these fields:

```text
GitProvenanceNote {
  schema_version: 2,
  commit,
  part: GitProvenanceNotePart,
  produced_by,
  tool_calls,
  knowledge_writes
}

NoteToolCall {
  tool,
  edge_kind,
  source_ref,
  target_ref,        # new, always emitted by v2 export
  file,
  byte_range,
  turn
}

GitProvenanceNotePart {
  document_id,       # sha256 of the unsplit logical note document
  part_index,
  part_count
}
```

Deserialization remains compatible with the existing unversioned documents:
missing `schema_version` is v1 and missing `target_ref` remains valid. New
exports always emit v2 and always copy `Edge::target` into `target_ref`.
The legacy import adapter continues using its current local re-chunk fallback
for v1 notes. For v2 notes it may use the validated target directly only when
the target is a `project_file` or `project_file_v2` for the selected central
`project_id`; otherwise it falls back to the current resolver. This keeps the
schema migration additive and makes later import extraction possible without
claiming that it is implemented here.

### 3.5 Fragmentation and pagination are generation-bound

One export can exceed the MCP response cap. The plan tool therefore accepts
only pagination controls:

```text
bbox_provenance_export_plan(cursor?, generation?)
```

One commit can itself exceed a safe MCP page. Before paging, the planner splits
that commit's ordered tool calls into multiple independently valid v2 note
documents at tool-call boundaries. Every part repeats `commit` and
`produced_by`, repeats the complete `knowledge_writes` list for v1-import
compatibility, carries `GitProvenanceNotePart`, and stays below 24 KiB. The
existing Git-note separator and import document splitter already support
multiple documents on one commit. The current importer ignores
`knowledge_writes`; a future consumer must deduplicate the repeated list by
`(id, kind)` before publication. A single tool-call record that cannot fit is
rejected explicitly; a large commit is not permanently foreclosed merely
because its complete logical note exceeds one response.

The implementation then:

1. Builds the sorted `(commit, serialized_document)` inventory for the
   authoritative project from one `EdgeIndex` read generation.
2. Computes `generation` from the project id, notes ref, and ordered
   `(commit, part_index, document_sha256)` tuples.
3. Uses an opaque cursor encoding `(commit, part_index)`, caps each page at 64
   documents and 64 KiB of total serialized document payload, and leaves room
   under the daemon's 80 KiB MCP cap for the response envelope.
4. Returns `next_cursor` only when more documents remain.
5. Requires every page after the first to provide the first page's generation.
   A mismatch returns `error.stale_generation`; callers restart from page one.
6. Returns `error.tool_call_too_large` only when one indivisible tool-call
   record cannot fit in a 24 KiB document part. It never relies on the MCP
   lossless-spill envelope because a remote checkout cannot consume a
   daemon-local spill path.

The new local apply function makes writes idempotent, so a generation restart
after several pages is safe. The existing Git helper append-writes every call;
it is not itself idempotent.

## 4. Local write contract

`bbox-provenance::apply_export_page(root, page)` performs all checks before the
first write in that page:

1. Canonicalize `root` and require it to equal the CLI's explicit or
   cwd-defaulted project root. The library never falls back to process cwd on
   its own.
2. Require `root` to own a committed `.bbox/config.toml` at local `HEAD` and
   resolve a recorded or operator-overridden repo id through the committed-ref
   reader added in Phase 0.2. Working-tree, computed, and `aka_repo_ids` inputs
   do not establish write authority.
3. Compute `(repo_id, bbox_root_relpath)` from that project root and require an
   exact match with `page.scope`.
4. Require one consistent `project_id`, notes ref, and generation throughout
   the page.
5. Treat the server plan as namespace authority, but structurally restrict its
   note ref to `refs/notes/<one-safe-component>/provenance`. Reject ref
   traversal, whitespace, control bytes, option-like components, and arbitrary
   Git refs. The writer does not consult its own process environment, so a CLI
   and daemon with different `BBOX_GIT_NOTES_NAMESPACE` values cannot disagree
   after the plan is issued.
6. Require each entry's key commit to match the document's commit, require the
   commit to exist locally as a commit object, verify `document_sha256`, and
   parse the document before writing.
7. Require every v2 tool-call target to be a project-file entity for the page's
   central `project_id`.
8. Read the existing note for each commit, split its documents, and skip an
   incoming document whose exact sha256 is already present. Append only a
   distinct document. Re-reading is part of the new local apply function, not a
   claimed behavior of `bbox_corpus_core::git::write_note`.

Only after the page validates does it take an advisory lock in the repository's
shared Git common directory, re-read existing notes under that lock, set
`notes.mergeStrategy=union`, and write the notes. This serializes leaf users
across linked worktrees. The result reports written, unchanged, and rejected
counts without including full document bodies.

The page is not globally atomic across multiple Git commits. That is acceptable
because writes are deterministic and idempotent, generation restart is safe,
and the preexisting operation already writes one note at a time. A process
failure can leave a valid prefix, never a malformed or cross-scope note.

## 5. Implementation phases

### Phase 0: repair the landed decomposition baseline

The first fresh Kimi pass reviewed the complete tagged attempt rather than only
this plan. It found five tracked-code defects. They are prerequisites, not
deferred cleanup, because building the next boundary on known authority and
retrieval regressions would invalidate the later implementation review.

#### 0.1 Restore exact-entity embedding convergence

- Change the logical-ref and scope sync helpers to enqueue every
  `KnowledgeIndexDocument` they publish through
  `embed_queue::enqueue_knowledge(document.entry, document.entity_id,
  knowledge_chunk_hash(document.entry))`.
- Include provisional entity ids exactly as materialized in Tantivy. Do not
  enqueue the logical published id for a provisional document.
- Keep visibility filtering as the query-side authority, so a vector is usable
  only when its exact entity id is present in the selected knowledge view.
- Add focused tests proving a worktree knowledge write enqueues its
  `provisional_knowledge` id and a published write enqueues its published id.
  Tests install an isolated queue and never touch the operator's vector state.
- Verify scope replacement enqueues every surviving document. Removed variants
  remain query-invisible immediately through exact-view filtering; vector
  tombstone/GC policy is not widened in this repair.

#### 0.2 Resolve live repo authority from committed bytes

- Add a `bbox-config` reader for `.bbox/config.toml` at a named Git commit or
  full ref using `git show`, sharing the same TOML field parser as the
  working-tree reader.
- Live `PublishedScope` admission reads config from committed `HEAD`, never the
  working tree. It derives a candidate scope first, then looks up the
  `PublisherRefStore` by that scope. When a pin exists, the loader resolves the
  pin to a commit, re-reads config at that commit, and requires the pinned
  document to derive the same scope before using the view. When no pin exists,
  the committed-HEAD candidate is used only to seed the first pin. This
  two-step ordering avoids using a scope before it has been derived.
- Never use uncommitted working-tree config for publisher election, write
  admission, `recorded_scope`, or schema-epoch authority.
- Preserve `project_key_override > recorded repo_id` precedence inside the
  committed document. Computed and `aka_repo_ids` inputs remain insufficient
  for live admission.
- Add tests where an uncommitted repo-id edit cannot move scope, committed
  bytes are observed only at the selected ref, and a pinned config mismatch
  fails closed rather than silently re-keying a view.

#### 0.3 Remove listener-critical blocking recovery

- Startup lifecycle recovery attempts only the nonblocking abandoned-claim
  path. A transaction whose lane is still locked by a live closeout is
  diagnosed and deferred.
- The listener can bind while the periodic lifecycle pass later retries full
  recovery. Recovery correctness and roll-forward semantics do not change.
- Add a test holding the lane lock while startup recovery runs and assert that
  startup returns without waiting or clearing the live transaction.

#### 0.4 Reject symlinks in the materialized merge candidate

- Before parsing candidate `.bbox/knowledge`, `.bbox/gaps`, or managed provider
  files, inspect each component with `symlink_metadata` and reject symlinks.
- Reuse the strict inventory's confinement vocabulary where possible, but do
  not require the candidate tree to mutate or quarantine files.
- The gate fails closed with relative candidate paths only. It never reads or
  reports the external symlink target or content.
- Add adversarial tests for a knowledge symlink, a gap symlink, and each
  managed provider-file symlink.

#### 0.5 Route knowledge readers through one visibility view

- Route `bbox_lint`, `bbox_review` list mode, compatibility absorb/bootstrap
  reads, and workspace enrichment through `session_knowledge_view` or a small
  shared read-only facade built directly on it.
- Keep review approve/reject mutation authority on the existing prepared
  mutation path. The detached view must never become a second writer.
- Preserve published-only behavior for sessions without checkout authority and
  default-own behavior for authoritative checkout sessions.
- Add cross-check tests proving these readers see own provisional data, never a
  peer checkout by default, and surface invalid-own diagnostics consistently.

The unrelated untracked `deploy/blackbox-java-worker/` directory remains
outside this session's ownership and is neither staged nor modified.

### Phase A: schema and local leaf

- Add `crates/bbox-provenance` to the workspace.
- Move the note structs and split-document parser from
  `bbox-mcp-tools::provenance` into the leaf.
- Add v1 compatibility and v2 target-ref serialization tests.
- Add plan hashing, note-ref validation, scope checks, commit checks, and
  idempotent local-write tests using canonicalized temporary repositories.
- Add the forbidden-dependency acceptance test.
- Refactor the legacy export and import helpers to use the shared schema
  without changing their public response shapes.

### Phase B: corpus planning tool

- Extract pure note-inventory construction from the legacy exporter.
- Add generation-bound paging and response structs.
- Add `project_id` to `ResolvedCheckoutScope` and populate it in the
  conservative resolver and all protocol/test constructors.
- Add the `bbox_provenance_export_plan` MCP adapter. Resolve its project only
  from the authoritative session's materialized `project_id` and the in-memory
  registry.
- Add `tool_docs.rs` coverage and preserve server surface filtering.
- Test absent authority, missing or mismatched project id, stable generation,
  stale generation, deterministic part cursoring, page byte caps, large-commit
  fragmentation, and oversized single-tool-call refusal.
- Add a tripwire test proving the planner module contains no Git-note write or
  checkout-read, config-read, or publisher-election call across the entire
  adapter path.

### Phase C: operator CLI

- Refactor `bro-cli::mcp_call` into a small reusable MCP client that can return
  a typed tool-result JSON value while preserving the existing `bro mcp call`
  behavior.
- Add `bro provenance export [--project-root DIR] [--daemon-url URL]`.
  `--project-root` defaults to the process cwd; explicit and default roots are
  both canonicalized and pass the same scope and confinement checks.
- Canonicalize the project root before MCP initialization and pass it as the
  session's transport project context.
- Loop over plan pages with one generation, restart on
  `error.stale_generation`, and cap restarts to prevent livelock.
- Apply each page through `bbox-provenance`, printing one final summary.
- Test argument routing, structured MCP result extraction, paging, bounded
  stale-generation restart, and propagation of local validation failures.

### Phase D: overlap documentation and closeout

- Keep `bbox_provenance_export` and `bbox_provenance_import` registered and
  behavior-compatible.
- Mark the legacy export description as an overlap adapter and point operator
  docs to `bro provenance export`. Document that the new command exports
  exactly its authoritative checkout project, while the legacy tool can export
  all registered projects in one call; multi-repo operators run the CLI once
  per checkout.
- Update the locality-first design status to record export as landed while
  leaving provenance import, blame, and render pending.
- Do not claim that blackboxd is checkout-free while the legacy adapters and
  daemon-side project walker remain.

## 6. Explicit non-goals

- No `bbox_provenance_import_apply` accepting caller-supplied note JSON.
- No retirement of the legacy import or export adapter in this slice.
- No harness binding and no dispatch-wide `?project=` authority expansion.
- No blame, render, project-file collector, or Git-history collector work.
- No Git push or fetch policy for the notes ref.
- No global-scope rendering or service restart.
- No off-host authentication design. The new export direction is
  corpus-to-checkout and does not require one.

## 7. Validation and review gates

Focused validation:

- project-pinned `scripts/fmt.sh --check`;
- `cargo check --workspace`;
- `cargo nextest run --workspace`;
- `scripts/acceptance-provenance-deps.sh` and
  `scripts/acceptance-fleetd-deps.sh`;
- `scripts/lint-concurrency.sh` because an MCP handler changes;
- a local temporary-repository smoke test for `bro provenance export` against
  an isolated daemon state, without touching production state or restarting a
  shared service.
- focused regression tests for Phase 0 embedding, committed authority,
  nonblocking startup, merge-gate symlink refusal, and unified visibility.

Closeout validation runs on the cluster against the pushed ref:

- `cargo nextest run --workspace --profile full`;
- `cargo clippy --workspace --all-targets` through the project verify wrapper;
- concurrency lint.

The implementation does not proceed until a fresh Kimi session reviews this
plan and returns `PASS`. After implementation and verification, a second fresh
Kimi session reviews the complete mandatory baseline-to-HEAD scope. Every
finding is fixed and rechecked by resuming that same second session until it
returns `PASS`.

## 8. Acceptance criteria

- AC-0: every tracked-code finding from the first full-scope Kimi pass is
  repaired and covered by a focused regression test before provenance export
  extraction begins.
- AC-1: `bbox_provenance_export_plan` cannot select a project from tool
  arguments and performs no checkout or Git I/O.
- AC-2: all local writes validate exact durable published scope, path
  confinement, notes-ref confinement, commit existence, document hash, and v2
  target project before mutating Git notes.
- AC-3: paging is deterministic, response-bounded, generation-checked, and
  safely restartable.
- AC-4: the legacy export and import tools retain their existing names,
  argument shapes, response shapes, and v1 note compatibility.
- AC-5: new exports contain `target_ref`, and the shared schema has one owner
  used by daemon and CLI paths.
- AC-6: `bro provenance export` uses the leaf crate's one local apply function;
  no new second CLI or daemon implementation duplicates its Git-note
  semantics. The preexisting legacy daemon writer remains only for the defined
  overlap.
- AC-7: no caller-supplied note document can enter the central edge sidecar
  through a new surface in this slice.
- AC-8: dependency acceptance, focused tests, full workspace verification, and
  the fresh-session Kimi implementation review all pass.

## 9. Deferred gate for provenance import

Import extraction begins only after a separate reviewed plan answers all of
these questions:

- Which authenticated process attests that documents were read from the notes
  ref of an admitted checkout?
- How is that checkout identity bound to the MCP or collector request without
  trusting model-supplied fields?
- Does import preserve historical v1 documents by local target resolution, or
  require a one-time migration to v2 target refs?
- What transaction and replay contract prevents partial or forged edge-sidecar
  publication?

Until then, the legacy daemon import remains the authority-preserving path.
