---
title: "Locality-first decomposition: the checkout plane and the corpus plane"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - daemon-runtime
  - bro-harness
  - corpus
  - knowledge
tags: [decomposition, satellite, harness, worktrees, knowledge-seam, collector, render, provenance, indexing]
brief: "Split the system on LOCALITY (checkout-coupled vs shared/append-only), not authority. Two moves, strictly ordered: (1) the daemon's remaining checkout-coupled surfaces (provenance notes, blame, project-scope render, project-file walking, .bbox read path) become harness/CLI-native, executing where the bytes live; (2) only then does the corpus + shared stores move off-host, with nothing left to neuter. The knowledge seam gets the same treatment as code: a worktree's .bbox/ is working-set truth, merge is the integration boundary, the corpus indexes published truth plus an explicitly-labeled provisional lane, and the host-local write_redirects bridge retires. Grounded in the satellite-arc post-mortem (salvage/satellite-arc-20260718): that split cleaved by authority while the code coupled by locality, and every disagreement between the two became a deleted tool family or a bespoke recovery protocol."
---

# Locality-first decomposition: the checkout plane and the corpus plane

> **Status: proposed, with the knowledge seam and checkout-local provenance
> export implemented.** The identity foundation, prerequisite repairs, and
> dark provisional overlay are landed. Provenance planning now stays in the
> corpus while `bro provenance export` validates and writes Git notes in its
> own checkout. The legacy daemon export remains during overlap; provenance
> import, blame, render, checkout collectors, and all off-host behavior remain
> proposed. The evidence base is the
> satellite-arc post-mortem (section 1), the shipped harness process boundary
> ([harness-process-boundary.md](../bro-harness/harness-process-boundary.md)),
> and a code-verified inventory of the daemon's remaining checkout-coupled
> surfaces (section 3) taken on `beta/blackbox-v2` at `15c8d3cc`. Line cites
> rot; verify against code before building on any of them.

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

Inventory of the daemon's remaining checkout-coupled surfaces, split by
**write authority**, not by whether they touch disk:

| Concern | Today | Move |
|---|---|---|
| Provenance export | `bbox_provenance_export_plan` builds generation-bound documents from corpus edges; `bro provenance export` applies them through the dependency-clean `bbox-provenance` leaf. The legacy daemon writer remains for overlap. | Landed for operator checkouts. Retire the legacy adapter only after overlap use is measured. |
| Provenance import | `bbox_provenance_import` still reads checkout notes and publishes central edges inside blackboxd. | Pending a separately reviewed authenticated checkout-source channel. Do not accept caller-supplied note JSON as central graph authority. |
| `bbox_blame` | `crates/bbox-mcp-tools/src/mcp_tools/blame.rs`; shells `git blame`, reads files, then joins against the transcript index | Hybrid: blame executes where the checkout is (harness binding); the turn-join is a corpus query the harness makes over MCP. |
| Project-scope render | `crates/bbox-knowledge/src/render.rs` + `src/tools/render.rs` rescope; daemon writes managed-marker files into the caller's checkout | Checkout-local by construction: render is a pure function of the checkout's `.bbox/` plus global-scope entries fetched over MCP. The satellite arc's byte-for-byte fork is unnecessary now because render lives in `bbox-knowledge`, which is daemon-independent; a harness binding or `bro render` links the same crate (peel a `bbox-render` leaf if the dep tree needs slimming). |
| Project source indexing | `crates/bbox-corpus-index/src/index/project_files.rs` walks registered roots, reads bytes, chunks, writes tantivy | Split at write authority: walk+read+hash is checkout-local; chunking and the tantivy write stay with the corpus (decided 2026-07-19, see section 5). |
| Repo-owned `.bbox/` read path | `crates/bbox-knowledge/src/knowledge.rs` loads registered base roots only | The harness reads its own checkout's `.bbox/` directly; it is just files in its working set. Write-authority coordination (dedupe, review lanes, provisional promotion) stays central. |
| Git history ingest | `crates/bbox-corpus-index/src/index/git_history.rs` | Rides the collector split: history increments are produced checkout-side, ingested corpus-side. |

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

### 4.1 Mechanics today (verified)

- **Reads alias broadly** to the base project:
  `resolve_base_project_for_scope`
  (`crates/bbox-corpus-core/src/project_record.rs`) accepts
  path-descendants, git-common-dir matches (out-of-tree worktrees), and
  managed-checkout markers with a uniquely-matching durable `repo_id` (pool
  lanes). **Writes alias conservatively**: only managed worktrees
  (`resolve_managed_fleet_worktree`,
  `crates/bbox-indexing/src/projects.rs`).
- **Files follow the branch, scope follows the base.** A knowledge write
  from a recognized worktree keeps base scope but its repo-owned entry file
  lands in the worktree checkout, so it travels with the branch
  (`src/tools/scope.rs` is the canonical statement).
- **The bridge is `write_redirects`**: a `HashMap<entry_id,
  checkout_dir>` persisted only in the central host store
  (`crates/bbox-knowledge/src/knowledge.rs`), with a hand-patched purge
  exclusion, dropped per-id when the merged file is observed at base.
- **Worktree `.bbox/` is invisible on disk but globally shadowed in memory**:
  `load_project_entries` iterates registered base roots only, and the
  `.bbox/` watcher watches registered roots only. The write path nevertheless
  rescopes the entry to the base project, replaces the central in-memory copy,
  and immediately syncs that same logical entry to the index. Other callers
  can therefore see an unlabeled provisional version, while restart recovery
  still depends on `write_redirects` and `kb.json`.
- **Partially implemented from the repo-owned design**
  ([repo-owned-project-state.md](../corpus/knowledge/repo-owned-project-state.md)):
  additive `repo_id`, checkout-registry, `built_from`, and inventory
  primitives have landed, but production does not yet record durable repo
  authority, key checkout records by published scope, build overlays, or run
  `render --check` and `bbox_lint` as a merge gate.
- Live measurement at time of writing: 17 nested worktrees under
  `.claude/worktrees/`, with up to 10 gap files and 5 knowledge files per
  worktree diverging from base. That divergence is branch state and is
  correct; the hazards are in how the machinery interprets it.

Structural defect: `write_redirects` makes host-local `kb.json` required
state for correctly interpreting the repo's own committed `.bbox/` files.
That contradicts the repo-owned inversion's core promise ("losing the
daemon store stops being catastrophic; the project layer rebuilds from the
repos"). It is a patch compensating for a missing principle, and the
principle is already written down for code.

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
   Checkout removal tears down its overlay. `write_redirects` then retires,
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

This slice is what makes the corpus movable without mounts: after it, the
corpus host needs network reachability from checkout-owning machines, not
filesystem access to them.

## 6. Sequencing

Strictly ordered; each slice is independently valuable and lands on the
monolith:

1. **Contract slice.** `(repo_id, bbox_root_relpath)` identity for
   project-scoped stores; checkout/workspace identity in `bro-core`;
   per-view `built_from` stamps on knowledge and indexed-lane responses.
   Additive.
2. **Harness-ward moves.** Provenance bindings; blame binding (hybrid);
   checkout-local render via the shared `bbox-knowledge` render crate,
   plus `render --check`. Daemon adapters stay live during overlap
   (strangler, as with refactor v2), then retire.
3. **Knowledge seam.** Provisional lane for managed-checkout `.bbox/`;
   retire `write_redirects`; `bbox_lint` + `render --check` at the merge
   gate.
4. **Code-corpus collector.** Producer peeled and acceptance-tested like
   `bbox-collector`; corpus-side ingest endpoint; retire daemon-side
   walking of checkouts.
5. **Fleet supervisor extraction (`fleetd`).** Pull worker
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
6. **Corpus off-host.** Now a relocation, not a split: the surviving
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
