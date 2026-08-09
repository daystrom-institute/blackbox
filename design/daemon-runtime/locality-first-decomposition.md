---
title: "Locality-first decomposition: the checkout plane and the corpus plane"
kind: design
lifecycle: partial
corpus: blackbox-design
topic:
  - daemon-runtime
  - bro-harness
  - corpus
  - knowledge
tags: [decomposition, satellite, harness, worktrees, knowledge-seam, collector, render, provenance, indexing]
brief: "Split the system on LOCALITY (checkout-coupled vs shared/append-only), not authority. Two moves, strictly ordered: (1) empty the daemon of checkout-coupled acquisition and mutation; (2) only then move the corpus + shared stores off-host. All covered Published-project transports, including render and collected code source, now have measured strict cutovers. Operational coverage and compatibility retirement precede the corpus move."
---

# Locality-first decomposition: the checkout plane and the corpus plane

> **Status: partial; current-HEAD inventory reverified 2026-08-09.** Durable
> project identity, the single-host knowledge seam, fleetd, authenticated
> Git-history transport,
> bidirectional authenticated provenance transport, GH-F overlap proof, and
> GH-G strict cutover are implemented. Covered published repositories no
> longer fall back to daemon-side Git/provenance leases; bridge, uncovered,
> and `LegacyLocal` adapters remain intentionally scoped. KT-A through KT-F are
> implemented: authenticated committed and provisional knowledge sources,
> harness-local mutation, durable remote `own`/`all`, measured overlap, and an
> offline strict-cutover marker. For an operator-cutover covered Published row,
> blackboxd no longer watches, reads, writes, or falls back to the checkout for
> project `.bbox/` state. The shared local adapter bodies now have no covered
> route and remain only for bridge, uncovered, and `LegacyLocal` lanes.
> Managed workspace blame now executes Git in the harness, and the
> authenticated `bro blame` route does the same for operator checkouts. BL-C's
> measured overlap, mandatory quiet window, checksummed marker, and strict
> covered-project refusal are implemented. A production marker is not claimed;
> bridge and raw unscoped-path blame remain explicit compatibility lanes.
> Project render and collected code-source strict cutovers are also implemented.
> For a marked Published project, render writes stay with the checkout owner,
> collected transport remains authoritative, `LocalProjectWalk` refuses before
> the broker observes or resolves a path, and transcript tool edges resolve from
> verified immutable blobs. No production render or code-source marker is
> claimed. Section 3 is the current
> code-verified inventory and section 6 is the dependency and retirement map.
> Line cites rot; reverify
> symbols and contracts against code before building on this snapshot. No
> production knowledge marker or deployment is claimed by this inventory.

## 0. Decision

Decompose on **locality**, not authority:

- The **checkout plane** is everything whose truth is a mutable working copy
  on some machine: file bytes, git history and notes, blame, worktrees,
  `.bbox/` entry files, rendered provider memory, build/validation runs, and
  every process spawned against a checkout with the operator's credentials.
  This plane is owned by the machine that owns the checkout, and its
  execution locus is the harness (for dispatched agents) or the `bro` CLI
  (for the operator's own checkouts). The daemon never reaches into it.
- The **corpus plane** is everything append-only, derived, or shared:
  transcripts, the tantivy index, vectors/embeddings, the graph, the
  knowledge/threads/notes/pins/gaps stores' central lanes, orchestration
  state, ingress, and credential brokering. This plane can move off-host.

The ordering constraint is the whole design: **empty the daemon of
checkout-coupled surfaces first; move the corpus second.** The satellite arc
did it in the other order and had to amputate sixteen tool families to make
the move typecheck against reality (section 1). Executed in this order, the
eventual off-host move relocates only things that pass the
[remote-worker-boundary.md](../bro-harness/remote-worker-boundary.md)
residency test (shared mutable state, or the coordination point itself), so
there is nothing left to neuter.

The knowledge plane is not an exception to the split; it is the load-bearing
instance of it (section 4). A worktree's `.bbox/` is working-set truth
exactly like the code beside it. Merge is the integration boundary for
knowledge, the same gate code goes through. The corpus indexes published
truth, plus an explicitly-labeled provisional lane for managed-checkout
entries that have not merged yet.

## 1. Evidence: the satellite-arc post-mortem

`salvage/satellite-arc-20260718` (53 commits, ~95k insertions over
merge-base `2ff9a911`) built the four-plane authority topology of its
`process-topology.md`: blackboxd as corpus authority, blackopsd as
operational singleton, fleetd per agent machine, bro-harness workers, plus a
`bbox-collector` transcript satellite, a git-mount/connector substrate, OTLP
telemetry, and a k3s deployment (`bbox-cage`). Two of its 53 commits were
ever salvaged onto `beta/blackbox-v2` (the heavy-work-offload docs). The
autopsy, with the branch's own gap records as citations:

1. **Filesystem locality was the unabstracted primitive.** Project identity
   was `hash(canonical local path)`, freshness was mtime+size, and there was
   no file-source trait (its `remote-source-connectors.md` states this
   verbatim). Identity, indexing, freshness, render targeting, blame, and
   `.bbox/` placement all bottomed out in "this daemon can stat that path";
   moving the daemon broke all six at once.
2. **The neutering was a role mask, not a decomposition.** `RuntimeRole::
   Corpus` dropped sixteen tool families (refactor, code_nav, dispatch,
   orchestrate, atoms, agents, workspace, macros, ...) from the remote
   daemon's surface. The clean `blackbox-corpus-service` crate could not
   serve the public MCP surface (its AR-001); the worker RPC listener never
   left blackboxd (its own header says "temporary ... P5 moves this into
   fleetd"); there was no legacy state migration (AR-003). Strictly more
   moving parts than the monolith, with none of the promised independence.
3. **Knowledge is bidirectional and only the read direction survived.**
   `bbox_render`, `bbox_learn`, and `bbox_gap` are filesystem writes into
   checkouts the remote daemon could not see. Render on the corpus host
   would have clobbered 71 repo-owned entries down to 1 (gap-4e2db371).
   Recovering that one tool cost a versioned wire protocol (RenderPlanV1,
   two HTTP endpoints), a `bro render` subcommand duplicating daemon render
   semantics byte for byte (the client could not link the daemon's crates),
   and a heuristic shrink guard to stop it destroying data. One tool, one
   protocol: the seam was cut in the wrong place.
4. **Identity fractured across hosts.** Sixteen dead `local_fs` registry
   entries carrying another machine's absolute paths had to be kept alive as
   a path-selector bridge, documented as "load-bearing, not corpses"
   (gap-cbcc314d, unresolved).
5. **Mirroring is a semantic downgrade dressed as a transport.** The mount
   substrate restored reading via clone mirrors but delivered no uncommitted
   state, no worktrees, no dirty-buffer blame, plus git-version, submodule,
   path-encoding, and ssh-credential archaeology in the image.
6. **Container state resolution must fail loud.** Store roots scattered
   across `dirs::*` calls silently redirected durable state onto pod
   overlay; every rollout discarded the voyage vector partitions and
   re-bought the embed backfill (gap-47f167de). Any future off-host move
   derives every store path from one `BLACKBOX_STATE_DIR` and refuses to
   start otherwise.
7. **The curated-projection defect, at a second scale.** The branch
   projected a curated subset of bbox tools over its worker RPC
   (`dad26a52`), the same defect the first harness extraction made and that
   [harness-process-boundary.md](../bro-harness/harness-process-boundary.md)
   section 3 now bans outright: a hand-curated projection silently removes
   most of the surface. Complete catalogs, filtered server-side, or nothing.

Worth lifting on their own merits, independent of the topology that carried
them: the `bbox-collector` invariants (dependency acceptance test excluding
tantivy/V8 from the satellite tree; strict-prefix shipping that never
advances past a torn line; server as cursor authority with a local resume
cache; deterministic record ids over (producer, stream, byte range); no
spool, the provider's own file is the durable backlog), the tantivy-free
`bbox-transcript-read` peel, the OTLP layer, and the `.bbox-mount-owner`
marker pattern for destructive ops.

One line: **the corpus can move; the operator's working set cannot.** The
transcript half (append-only, host-agnostic) shipped cleanly. Everything
checkout-coupled cost a bespoke protocol and lost semantics.

## 2. Ground truth this design starts from

Materially different from what the satellite arc started with:

- **The refactor kill-list already executed.** The daemon's refactor /
  slice / code-nav / macro MCP surface is retired (docs/refactor.md;
  decisions af3c4783, b8dc263d). The harness owns `code.*`, `analysis.*`,
  `edits.*`, `java.*`, `rust.*`, `lsp.*`, `build.*` (71 tools at last
  count), linking `bbox-refactor`/`bbox-lsp` directly, validated via the
  `isolate` binary. Most of what the corpus role had to amputate no longer
  exists daemon-side at all.
- **The process boundary is real.** One `bro-harness` child per dispatch;
  the daemon links no V8 and no provider transports; the child receives the
  complete server-filtered MCP catalog
  ([harness-process-boundary.md](../bro-harness/harness-process-boundary.md)).
- **The placement function is written.**
  [remote-worker-boundary.md](../bro-harness/remote-worker-boundary.md)
  defines the two truth domains (working-set vs corpus), the residency test,
  and the two surviving governance boundaries (dispatch and integration),
  including the decision this doc extends to knowledge: the corpus indexes
  published truth; working-set changes feed it only at publish/merge.
- **The crate DAG is most of the decomposition already.** ~33 crates, two
  largely disjoint stacks meeting at exactly one edge
  (`bro-harness -> bbox-refactor`). The corpus/store crates
  (`bbox-corpus-index`, `bbox-vectors`, `bbox-knowledge`, `bbox-stores`,
  ...) carry no daemon entanglement.
- **Correction to the boundary ledger:** `AtomCapability` and
  `CorpusCapability` in `bro-capabilities` have zero implementers anywhere
  in the workspace; there is no `RefactorCapability` trait. The in-process
  trait wiring described in harness-daemon-boundary.md section 15 was
  retired along with in-process execution. The only live daemon-to-harness
  seam is the MCP endpoint; `ToolCapability` is a harness-internal seam
  projecting the already-filtered tool set into code-mode cells
  (`crates/bro-harness/src/capabilities.rs`). **Decided 2026-07-19: delete
  both dead traits** (keep `ToolCapability`), correcting the
  harness-daemon-boundary.md section 15 ledger in the same change. The
  standing **seam rule** this encodes: a harness-side dependency on
  daemon-side function is either the plain server-filtered MCP catalog or
  a deliberately-designed typed RPC contract (the fleetd supervision
  channel of slice 5 is the only sanctioned instance); never a dormant
  trait slot waiting for an architecture that was decided against.

## 3. The checkout plane: what still lives daemon-side, and where it goes

Inventory of the daemon's remaining checkout-coupled surfaces. "Current
HEAD" describes the executable path, not the intended architecture; a
path-free durable identity does not by itself make the operation local.

| Concern | Current HEAD | Locality end-state | Adapter retirement gate |
|---|---|---|---|
| Provenance export | Authenticated collector page/apply/receipt and GH-G strict cutover are implemented. `bbox_provenance_export_plan` and `bro provenance export` remain interactive checkout-local paths. The legacy mutation refuses before lease for transport-governed published projects; bridge and `LegacyLocal` compatibility stay scoped. | Keep corpus planning and checkout-local apply. The scope-authorized producer pulls the plan and returns a typed receipt; blackboxd never writes the notes ref for covered projects. | Complete for GH-G-covered published projects. Retain the interactive plan and verify later bridge retirement independently. |
| Provenance import | Authenticated stable snapshot upload, corpus validation, pinned V1 resolution, strict V2 membership, durable replay, quarantine, and GH-G strict cutover are implemented. The legacy import refuses before lease for transport-governed published projects. | Keep typed producer capture and central edge publication. Arbitrary caller-supplied note JSON is not graph authority. | Complete for GH-G-covered published projects. Bridge and `LegacyLocal` retirement remain separate. |
| `bbox_blame` | Managed harnesses and the scope-authenticated operator CLI execute current or exact-snapshot Git locally and return the same bounded fact for central enrichment. BL-C persists path/entity equality evidence and installs a checksummed project marker only after a minimum five-minute window with unchanged `Blame` checkout counters. Covered entity and stable-session-project path requests refuse before the daemon broker. | A checkout-side binding returns a typed blame fact at an explicit commit or working-tree state. The corpus-side query joins that fact to anchors, sessions, brofiles, and threads. A checkout path never becomes corpus authority. | Complete in code for explicitly marked Published projects. Production apply remains operator-authorized. Bridge and raw unscoped-path lanes remain separately scoped compatibility. |
| Project-scope render | Managed harnesses keep the public `bbox_render` tool but transport a bounded, path-free authorized knowledge snapshot and invoke the shared `bbox-knowledge` renderer inside their own bound checkout. The daemon independently validates the exact projection receipt. Published/own/all completions are durable; a checksummed per-project marker requires all three positive controls plus an unchanged five-minute `RenderFileProvider` checkout baseline. Covered unbound calls refuse before the broker. Global render stays daemon/operator-host local; bridge, uncovered, and `LegacyLocal` project renders retain the compatibility adapter. | A harness binding links the same `bbox-knowledge` renderer and writes only inside its own checkout. It obtains the pinned published/global inputs and explicit provisional view from the corpus. Global render remains operator-host local. | Complete in code for explicitly marked Published projects. Production apply remains operator-authorized. Bridge, uncovered, and `LegacyLocal` retirement remain separate. |
| Project source indexing | Authenticated collection, immutable generations, activation, recovery evidence, measured offline cutover, and runtime refusal are implemented. For a marked Published project the configured collected source is mandatory, assignment removal fails before reload swap, the broker refuses `LocalProjectWalk` before observation, and transcript project/tool-edge attribution uses verified generation blobs. | Checkout owners walk, hash, and ship raw capped bytes; the corpus chunks and indexes them. Every intended project uses an active collected source. No daemon source rung opens a checkout. | Complete in code for explicitly marked Published projects. Production apply, intended-project coverage, and explicit migration or retirement of bridge, uncovered, and `LegacyLocal` projects remain operator work. |
| Repo-owned `.bbox/` read and mutation path | KT-A through KT-F are implemented. Authenticated committed candidates and leased provisional workspaces feed the existing accepted and overlay models; managed project mutation runs in the harness; remote `own`/`all` survives restart. For an operator-cutover covered Published row, the marker closes daemon watcher refresh plus local read/write/schema-marker acquisition with no fallback. Non-vacuous publisher and watcher probes prove the covered route performs no checkout-broker operation. Shared local bodies remain only for bridge, uncovered, and `LegacyLocal` lanes. | A harness reads and mutates its own branch state directly. Published and deliberately shared provisional inputs reach the corpus through the authenticated checkout-source contract; corpus coordination, validation, promotion, and indexing remain central. | Complete for covered Published rows. Bridge, uncovered, and `LegacyLocal` retirement remains separately authorized. |
| Git history ingest | Authenticated complete reachable-history capture, resumable intake, certified P3 materialization, producer overlays, health, recovery, GC, rebuild, overlap proof, and GH-G strict cutover are implemented. Covered published repositories use producer state only and record no post-boundary `GitHistory` lease. The local refresh adapter remains only for named uncovered, bridge, and `LegacyLocal` categories. | The scope-authorized producer owns Git acquisition for covered published projects; corpus-side generation publication, selectors, indexing, and graph construction stay central. | Complete for GH-G-covered published projects. Later retirement must preserve the named surviving categories until their own gates. |

The rebaseline result is therefore:

- **Complete:** durable project/scope/checkout identity, slice 5's fleetd
  extraction, and KT-A through KT-F remote knowledge transport. For an
  operator-cutover covered Published row, accepted and selected provisional
  source is remote and restart-durable, and local watcher/read/write fallback
  is closed.
- **Complete for covered published repositories:** typed Git-history and
  provenance acquisition/publication through GH-G strict cutover. Runtime
  classification closes local fallback and retains only the named bridge,
  uncovered, and `LegacyLocal` categories.
- **Complete for covered blame:** managed and scope-authenticated operator
  path/entity planning, checkout-local current and exact-snapshot Git
  execution, bounded facts, measured response parity, and strict marked-project
  refusal are live with no daemon checkout acquisition or fallback.
- **Complete for covered project render:** managed plan/receipt transport,
  checkout-owned shared rendering, fixed-target confinement, explicit
  published/own/all views, candidate-gate parity, measured checkout baselines,
  and strict marked-project refusal are live. Applying a production marker is
  still a separate operator ceremony.
- **Complete in code for covered project source:** current v2 activation,
  startup and full-rebuild evidence, measured quiet-window cutover, mandatory
  producer assignment, no local cutback, no governed local-walk request, and
  collected transcript/tool-edge parity. Applying production coverage remains
  a separate operator ceremony.
- **Not relocated or retired:** bridge and raw unscoped-path blame
  compatibility, uncovered/bridge/`LegacyLocal` project render, and the
  local project-file source rung for uncovered, bridge, and `LegacyLocal`
  projects.
  Bridge, uncovered, and `LegacyLocal` `.bbox/` access remains intentionally
  local.

Non-normative operator snapshot, taken during this rebaseline: the current
catalog contains nineteen projects, one in the Published lane and eighteen
in LegacyLocal; one has an accepted publication pointer. Names and paths are
deliberately omitted. `bbox_lint` consequently reports published visibility
unavailable for those eighteen projects and legacy-compatibility knowledge
rows without provable `built_from` stamps. This is operational evidence that
bridge retirement is not ready, not a durable architecture invariant or a
target count.

Stays daemon-side, passing the residency test:

- `project_dispatch.env` resolution (`src/orchestration/mod.rs`,
  `project_dispatch_shell_env`): it configures the harness child's
  environment before that child exists. Structurally cannot move.
- Worktree lifecycle, seed_dirs, admission (fleet plane), and the project
  registry.
- Everything on the corpus plane: tantivy/vectors/graph/embeddings, the
  shared stores' central lanes, orchestration, crons/pollers/webhooks,
  credential brokering.
- Edge sidecars: they live under the state dir
  (`crates/bbox-edge-sidecar`), not in repos. Common misconception; they
  are corpus-plane.

Global-scope render (the `~/.claude/CLAUDE.md` family) stays with whatever
process runs on the operator's machine; it is host-local by nature and out
of scope here.

## 4. The knowledge seam under many worktree harnesses

### 4.1 Mechanics today (reverified 2026-08-09)

- **Identity and authority are durable.** Catalog projects, accepted
  publication pointers, checkout attachments, published scopes, and
  checkout-registry records replace path-derived authority. Absolute paths
  remain attachment observations, not project identity.
- **Published reads are pinned.** The knowledge view is built from the
  accepted publisher at its pinned commit, not from whichever moving base
  checkout happens to be visible.
- **Provisional reads are checkout-scoped.** Each admitted checkout can
  contribute a complete overlay with additions, changes, tombstones, and a
  `built_from` stamp. `session_knowledge_view` exposes
  `published|own|all`; only an authoritative session checkout grants `own`,
  and orchestrators do not silently receive `all`.
- **Promotion is equality, not observation.** A provisional variant retires
  only when its content equals the pinned published source. Checkout removal
  tears down its overlay. Gap views follow the same identity and visibility
  contract.
- **The old host-local redirect authority is gone.** `write_redirects` and
  its purge exception are retired. The project layer rebuilds from the
  pinned publisher plus admitted live checkouts.
- **Integration is enforced against candidate bytes.** The knowledge merge
  gate materializes an immutable candidate tree, invokes the shared renderer
  in check mode, and rejects stale projections or semantic contradictions
  before publication.
- **Covered Published rows have a strict remote rung.** Authenticated
  candidates supply committed knowledge/gaps, leased workspace generations
  supply provisional state, and a managed binding establishes remote `own`.
  An applied KT-E marker closes blackboxd watcher refresh and local
  read/write/schema-marker acquisition for that covered row, including after
  restart or later remote drift. Harness read-your-writes remains direct
  filesystem access inside the owning checkout. Bridge, uncovered, and
  `LegacyLocal` rows retain their local adapters.

The knowledge seam no longer needs another identity or visibility redesign.
KT-F retired the covered acquisition route and closed the parent record; it did
not widen the marker to bridge, uncovered, or `LegacyLocal` rows.

### 4.2 The principle, applied

`.bbox/` in a worktree is branch state. Treat it exactly like the code next
to it:

1. **Published knowledge truth** = the unique registered publisher's
   `.bbox/` at a host-locally pinned full branch ref, plus the central store's
   global lane. A moving checkout `HEAD` does not redefine published truth.
   This is what the corpus indexes as authoritative, what renders derive from
   on other machines, and what `verified` can mean.
2. **Working-set knowledge truth** = the worktree's `.bbox/` at its
   branch. The writing harness reads its own files; read-your-writes is
   local and free. **Merge is the integration boundary for knowledge**,
   the same gate code goes through.
3. **A provisional lane replaces the redirect map.** Every admitted checkout
   contributes a merge-base working-tree overlay, including untracked files
   and tombstones, under a compound `(published_scope, checkout_id, entry_id)`
   identity. Each materialized view carries a `built_from` snapshot; an
   invalid checkout overlay fails as a whole and never reuses a stale prior
   snapshot. Visibility is explicit: `provisional=published|own|all`.
   `own` is the default only when the server has an authoritative session
   checkout; otherwise the default is `published`. Model-supplied arguments
   and an unproven request cwd cannot establish own-checkout authority, and
   orchestrators do not receive implicit `all` visibility. Promotion is
   content equality against the pinned published commit, not mere observation
   at a moving base checkout. When equality is observed, only that matching
   provisional variant is dropped and the published document is rebuilt.
   Checkout removal tears down its overlay. `write_redirects` is retired,
   and knowledge state rebuilds from the pinned publisher plus admitted live
   checkouts. The detailed identity, failure, and lifecycle contract lives in
   [checkout-identity-and-provisional-knowledge.md](../corpus/knowledge/checkout-identity-and-provisional-knowledge.md).
4. **The alternative, named:** branch-private pre-merge knowledge (no
   provisional lane; entries are simply invisible outside their worktree
   until merge). It is the purest reading of "the corpus indexes published
   truth" and strictly less machinery, but it regresses today's
   daemon-wide in-flight visibility. The provisional lane keeps explicit,
   authorized access to that visibility while making its epistemic status
   and checkout identity queryable. If the lane proves noisy in practice,
   demoting to branch-private is a deletion, not a redesign.
5. **Semantic merge defense.** One-file-per-entry makes textual conflicts
   rare and semantic conflicts silent; `bbox_lint` at the merge gate (CI
   or closeout) is required, not hygiene, once many branches carry
   knowledge deltas. `render --check` rides the same gate so a stale
   committed render is caught where it is created.
6. **Identity before motion.** Persisted `repo_id` authority,
   `(repo_id, bbox_root_relpath)` keying, checkout identity, and per-view
   `built_from` stamps land before anything moves off-host, so no plane ever
   again keys knowledge or index state by an absolute path that does not
   travel (post-mortem items 1 and 4). This also makes pool lanes first-class
   rather than marker-special-cased, and it is the same workspace-identity
   contract remote-worker-boundary.md wants in `bro-core` for artifact
   envelopes.

Pins, live threads, and notes are unaffected: they are host-local activity
by design (`.bbox/local/`, central stores) and never had a code-plane twin.

## 5. The code-corpus collector

Generalize the transcript collector to project files: the machine that owns
a checkout ships file content; the corpus host chunks and ingests it. The
satellite arc's mount substrate inverted: push from where the bytes are,
not pull-and-mirror to where the index is.

**Implementation status (reverified 2026-08-09): strict covered-project
cutover implemented.** The
dependency-clean `bbox-code-collector`, authenticated manifest/blob upload,
content-addressed cache, immutable stored generations, activation reducer,
health model, startup reconciliation, and explicit cutback state are live.
An active collected selector prevents ordinary local indexing. The separate
[code-source locality cutover](code-source-locality-cutover-impl.md) now makes
that state mandatory for explicitly marked Published projects: startup and
full-rebuild controls bind the exact v2 generation, assignment drift and local
cutback fail closed, and `LocalProjectWalk` is rejected before broker
observation. Transcript stamps and tool edges are rebuilt from verified
generation blobs, so the removal does not discard graph/search behavior. The
old source remains intentionally available only to unmarked, bridge, and
`LegacyLocal` projects. Git history is governed by its independent GH-G
transport cutover rather than this raw-file contract.

**Wire shape (decided 2026-07-19): dumb producer, dedicated endpoint.**

- The satellite ships **raw capped file bytes**, not pre-chunked
  documents. It walks, hashes, and sends; it carries no chunker at all.
  All chunking (including the heavy PDF/xlsx/notebook converters) stays
  corpus-side, so there is exactly one chunker version in the system and
  satellite deploys never skew against the index. The corpus keeps a
  **content-addressed blob cache** of shipped bytes so a chunker or
  indexer upgrade re-processes locally without asking any satellite to
  re-send; the cache is a cache (the checkout remains the durable
  backlog), so losing it costs a re-ship, not data.
- Ingest uses a **dedicated endpoint** with manifest-negotiation
  semantics, not a new record kind on `/internal/records`. Transcript
  shipping is append-only with byte-offset cursors; code shipping is
  current-state with replace and delete. The conversation is rsync-shaped:
  the satellite sends a manifest of (path, content hash, size) at a
  generation (HEAD + dirty fingerprint), the server replies with the
  hashes it lacks, the satellite uploads only those. Deletion falls out of
  the manifest diff instead of being wedged into an append stream.

The collector invariants carry over, strengthened by the dumb-producer
choice: dependency-clean satellite tree (no tantivy AND no chunker in the
producer, enforceable by the same acceptance-test pattern), server as the
authority on what it still needs, deterministic identity (producer, root,
path, content hash) for dedupe, no spool (the checkout itself is the
durable backlog; a full rescan is always safe). The existing freshness
fingerprint (HEAD + indexer version + dirty state) becomes the manifest
generation. Only registered base roots ship; worktrees remain unindexed by
the corpus (today's behavior, kept deliberately), with the provisional
knowledge lane of section 4 as the sole worktree-sourced corpus input.

Completing this slice makes the corpus movable without mounts: after every
intended source is collected and the separate Git/provenance source contract
is live, the corpus host needs network reachability from checkout-owning
machines, not filesystem access to them.

## 6. Sequencing

Strictly ordered; each slice is independently valuable and lands on the
monolith:

| Slice | Current state | What remains |
|---|---|---|
| 1. Identity contract | Complete | The existing reuse-safe checkout marker is typed as `WorkspaceId` and transported through worker/fleet protocol state. Later session capability binding consumes this identity without redefining it. |
| 2. Harness-ward moves | Complete in code for covered Published rows | Provenance, knowledge, blame, and project render have independent measured strict cutovers. Apply production coverage and retain named compatibility lanes until separately retired. |
| 3. Knowledge seam | Complete for covered Published rows | KT-A through KT-F are closed. Bridge, uncovered, and `LegacyLocal` behavior remains. |
| 4. Code-corpus collector | Complete in code for covered Published rows | Apply intended-project coverage. Migrate or separately retire uncovered, bridge, and `LegacyLocal` local-walk lanes. |
| 5. Fleetd | Complete | No locality-program work remains. |
| 6. Corpus off-host | Pending | Blocked on deployed cutover coverage, compatibility disposition, and an exact all-store and all-MCP topology validation. |

The executable dependency map from this rebaseline is:

1. **Typed Git/provenance checkout-source contract: complete through GH-G.**
   GH-F overlap/parity and the separately gated GH-G strict cutover are
   implemented and closed out. The historical authority remains
   [git-history-provenance-transport-impl.md](git-history-provenance-transport-impl.md),
   whose current owner/caller inventory records the landed contract. It supplies
   complete and incremental Git history, provenance import, and any required
   unattended provenance-export receipt without granting callers arbitrary
   graph-write authority. It removes the `GitHistory` and
   `ProvenanceNoteIo` dependencies that raw-file collection cannot solve.
2. **Remote knowledge transport: complete through KT-F.** See
   [knowledge-source-transport-impl.md](knowledge-source-transport-impl.md).
   Operator-accepted committed publication candidates, leased provisional
   workspaces, harness-native project knowledge/gap mutations, durable
   `WorkspaceId` binding, strict watcher/read/write cutover, and the
   covered-route retirement proof are live. Shared local implementations remain
   only for bridge, uncovered, or `LegacyLocal` lanes.
3. **Interactive checkout bindings: complete for covered projects.** BL-C's
   blame boundary and the project-render harness plan/receipt boundary are
   implemented with independent measured cutovers. Production marker apply,
   bridge retirement, and compatibility lanes remain explicit operator work;
   they are not implied by the code landing.
4. **Deploy collector cutover coverage and observe every adapter.** The strict
   covered-project implementation, no-local-cutback decision, and
   feature-parity path are complete. Establish marked coverage for every
   intended Published project and run the per-surface retirement gates from
   section 3. An adapter retires because its own gate passes, not because a
   phase label says the migration is done.
5. **Plan and authorize bridge retirement separately.** Require accepted
   publication coverage, zero bridge-lane observations over a declared
   window, rebuild/restart evidence, and explicit operator approval. Neither
   the catalog implementation nor this inventory authorizes that mutation.
6. **Move the corpus off-host.** At this point it is a relocation of the
   corpus plane; blackboxd has no checkout reach-in left to preserve or
   emulate.

1. **Identity contract — complete.** `(repo_id, bbox_root_relpath)` identity,
   reuse-safe checkout markers, per-view `built_from` stamps, validated
   `WorkspaceId`, and additive worker/fleet transport are complete. KT-D binds
   that existing identity to short-lived session capabilities without
   reopening the identity contract.
2. **Harness-ward moves: complete for covered projects.** Checkout-local provenance export,
   authenticated notes import, and candidate-tree render checking are
   implemented. Git/provenance strict cutover is complete for covered published
   repositories. Remote knowledge strict cutover is complete for
   operator-cutover covered Published rows. Managed and scope-authenticated
   operator blame execution is checkout-local and centrally enriched; BL-C can
   close the covered Published-project adapter after measured overlap. Managed
   project render is also checkout-owned and has its own published/own/all
   receipt plus quiet checkout-baseline cutover. Bridge, raw unscoped-path
   blame, and uncovered render compatibility remain under their own gates.
3. **Knowledge seam — complete for covered Published rows.** The
   provisional lane, explicit visibility, promotion, lifecycle, gap
   convergence, authenticated transport, harness-local mutation, and no-local-
   fallback marker and covered-route retirement proof are live. Bridge,
   uncovered, and `LegacyLocal` retirement remains separate.
4. **Code-corpus collector: complete in code for covered Published rows.** The
   producer, ingest endpoint, generations, activation, recovery evidence,
   quiet-window cutover, assignment fence, no-local-cutback runtime, and
   collected tool-edge path are live. Production coverage and explicit
   disposition of bridge, uncovered, and `LegacyLocal` walking remain.
5. **Fleet supervisor extraction (`fleetd`) — complete.** Pull worker
   spawn/supervision out of the daemon into a small per-machine binary
   that rarely changes. The contract is a **fully-resolved spawn spec**:
   the daemon composes everything it already materializes at spawn today
   (brofile resolution, provider credentials, MCP injection env, the
   `shell_env` lane, surface-filter argv, dispatch context) and hands it
   over a narrow typed local RPC; fleetd executes and supervises the
   child, relays the stdio event/control channels, and holds leases plus
   a bounded replay window so live sessions survive daemon restarts.
   fleetd never re-derives policy and never reads brofiles or credential
   stores: policy decided centrally, enforced by construction, applied to
   process supervision. This is the one sanctioned non-MCP contract in
   the system (the seam rule, section 2). Motivations, in order: the
   monolith decision's own escape-hatch trigger ("sessions dropped by
   corpus-driven restarts") fires daily on a dev machine that rebuilds
   and kickstarts blackboxd constantly; and slice 6 quietly assumes a
   machine-side residual exists, so extracting it first keeps the corpus
   move a relocation. Lift the salvage branch's `worker-protocol.md`
   lease/replay/handshake design (its best-engineered piece after the
   collector) and give fleetd a collector-style dependency acceptance
   test (no tantivy, no stores, no V8). Cut over completely: no
   dual-path role mask (post-mortem item 2).

   **Contract (decided 2026-07-19):**
   - *Wire:* Unix domain socket + bounded length-prefixed JSON frames
     (`bro-rpc`: big-endian u32 length + UTF-8 JSON, mid-serialize abort
     at the frame cap; never newline framing, since model events and tool
     results carry arbitrary newlines and an oversize line cannot be
     rejected from a header) with a versioned handshake and a
     file-sourced bearer token; message types live in `bro-protocol`.
     (Corrected 2026-07-19 from "newline-delimited JSON": the framing
     amendment adopted with the salvage mining superseded this sentence
     before fleetd was built, and the built system follows `bro-rpc`.)
     The harness needs zero changes: it stays a plain stdio child of
     fleetd speaking NDJSON on stdin/stdout as today, and never dials
     anyone. The socket path derives from the daemon's state dir, so the
     prod and dev daemons on one machine each get their OWN fleetd; a
     shared supervisor would let one daemon adopt the other's sessions.
   - *Replay:* no in-memory replay buffer. The session's existing
     event-log JSONL in the task store is the replay source; the daemon
     is the authority on its own cursor (last-ingested seq per session)
     and fleetd streams the file tail from there on reconnect. The
     collector shape again: durable file as backlog, consumer-owned
     cursor. The only tunable left is GC of terminal-session state
     awaiting ack, which is generous (days) and boring.
   - *Re-adoption:* fully automatic on daemon start (dial socket, list
     live sessions, resume ingesting from cursors, repopulate roster).
     Adoption is non-destructive, so automation is safe; the versioned
     handshake fails closed and loudly on protocol/build mismatch
     (salvage's build-identity rejection, kept).
   - *Accepted v1 limitation:* fleetd's own restart kills its children.
     Process-adoption tricks are not worth it for a tiny binary that
     should change a few times a year; its stability is the point of
     extracting it.

   **Implementation status (2026-07-19, `beta/blackbox-v2`): shipped.** This
   slice is no longer proposed-tense; Phase A + B landed and merged. What's
   built, so the Contract paragraph above reads as record rather than
   pending work:
   - The `crates/fleetd` binary exists and owns worker spawn/supervision over
     the `bro-rpc` Unix-domain-socket wire described above.
   - Every harness dispatch composes a `WorkerSpawnSpec` and runs it through
     `HarnessExecutor`; `LocalExecutor` and `FleetdExecutor` (over
     `crates/fleetd`) are the two live implementations, selected once at
     daemon startup by `install_harness_executor`.
   - The default is fleetd (`daemon.executor` config, default
     `ExecutorKind::Fleetd`); `ExecutorKind::Local` is the explicit escape
     hatch back to in-daemon children, still what unit tests and library
     consumers get with no daemon startup.
   - The durable-cursor replay design is built as specified: no in-memory
     replay buffer, the daemon tracks a per-session durable ingest cursor
     against the event-log JSONL, and fleetd streams the file tail from
     that cursor on reconnect. Re-adoption on daemon start is automatic.
   - Badgey one-shot dispatches route through the same
     `WorkerSpawnSpec`/`HarnessExecutor` seam as interactive/reserved
     dispatch; there is no separate one-shot spawn path left to keep in
     sync.
   - The inline pipeline this slice replaces (`spawn_task_reserved`,
     `spawn_task_interactive`, `SpawnedTask`, `move_large_prompt_arg_to_stdin`,
     `ProviderEvents::parse_bulk_output`, `SupervisionState::observe_bulk_sink`)
     is deleted, not merely superseded. See
     [`harness-daemon-boundary.md`](../bro-harness/harness-daemon-boundary.md)
     section 15 for the ledger of what that replaced.
6. **Corpus off-host — pending.** Once the preceding checkout moves and the
   separate bridge-retirement gate are complete, this is a relocation, not a
   split: the surviving
   daemon surface passes the residency test wholesale. Deployment rides
   `bbox-cage`; state-root resolution derives from one `BLACKBOX_STATE_DIR`
   and fails loud (post-mortem item 6); transcript + code collectors ship
   from satellites; dispatch/fleet stays on agent machines. The
   multi-machine routing question (which machine runs a dispatch) remains
   deferred exactly as the satellite arc left it, but it no longer blocks
   the corpus move because dispatch never leaves the agent machines.

## 7. What this deletes or avoids

- No `RuntimeRole` compatibility mask threaded through the server; the
  decomposition is by construction, not by gating.
- No mount/connector substrate, no clone mirrors, no dead registry entries
  as identity bridges.
- No curated tool projections over any wire, at any scale (post-mortem
  item 7; harness-process-boundary.md section 3).
- No byte-for-byte forked implementations: checkout-side render links the
  same crate the daemon uses.
- No daemon reach-in to worker filesystems (reaffirms
  remote-worker-boundary.md section 7).
- `write_redirects`, its purge exclusion, and the "host store required to
  interpret repo files" property.

## 8. Open questions

- **Multi-machine dispatch routing.** Which machine runs a dispatch when
  there is more than one fleetd. Still deferred; nothing in this design
  needs it, and the corpus move (slice 6) does not touch it.
- **Further residual splits.** Whether the machine-side daemon ever
  splits again (operational singleton vs fleet, the satellite arc's
  blackopsd). Deliberately NOT reopened here: the post-mortem's stranded
  central scheduler is the cautionary case, and no trigger currently
  justifies it.

## 9. Relationship

- **Extends** [remote-worker-boundary.md](../bro-harness/remote-worker-boundary.md):
  adopts its truth domains, residency test, and integration boundary;
  applies them to the daemon's own residual checkout surfaces and to the
  knowledge plane, and adds the ordering constraint (empty the checkout
  plane before moving the corpus).
- **Companion of** [harness-process-boundary.md](../bro-harness/harness-process-boundary.md)
  (the shipped process seam this design assumes) and
  [refactor-tools-v2.md](../bro-harness/refactor-tools-v2.md) (the
  precedent strangler migration whose shape slices 2-4 reuse).
- **Continues** [repo-owned-project-state.md](../corpus/knowledge/repo-owned-project-state.md):
  section 4 here finishes its identity model and extends its
  committed-vs-local split to worktrees; the provisional lane is the
  worktree-generalization of its "uncommitted entries are provisional"
  rule.
- **Post-mortem source:** the satellite-arc design corpus lives on
  `salvage/satellite-arc-20260718` (`design/daemon-runtime/
  process-topology.md`, `remote-corpus-host.md`, `blackops-service-
  boundary.md`, `fleet-extraction.md`, `design/connectors/
  remote-source-connectors.md`); this doc supersedes their split axis
  while lifting the collector invariants, the telemetry layer, the
  dependency acceptance-test pattern, and (for slice 5) the
  `worker-protocol.md` lease/replay/handshake design. The deploy substrate survives in
  the operator-local `bbox-cage` overlay repo.
- **Touches** [concurrency-model.md](concurrency-model.md): slice 4 sheds
  the indexing plane's checkout I/O from the daemon and slice 5 sheds
  worker supervision, together shrinking the blocking-work surface its
  actors exist to contain.
