---
title: "Checkout identity and the provisional knowledge lane"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - corpus
  - knowledge
  - daemon-runtime
tags: [identity, worktrees, knowledge-seam, provisional-lane, write-redirects, repo-id, render-check]
brief: "Retire write_redirects, the host-local map that makes kb.json required to interpret the repo's own committed .bbox/ files. Two moves in one push: (1) a durable published-scope identity key anchored on a strongly-minted repo_id recorded in .bbox/config.toml (the computed first-commit hash is only a bootstrap hint) plus bbox_root_relpath, and (2) a versioned provisional knowledge lane. The unifying model: EVERY checkout (base included) has a committed layer (published truth, read from the committed tree) and a provisional overlay (its dirty/branch .bbox changes, computed as a merge-base-relative diff with tombstones for deletes). The overlay keys (published_scope, checkout_id, entry_id); checkout_id is persisted in a checkout-local marker; promotion is observed when a change reaches the committed base tree; own-checkout visibility binds to the server-authoritative session checkout. Monolithic-rung only. Slices 1 + 3 of locality-first-decomposition.md, hardened over two adversarial review rounds (2026-07-20)."
---

# Checkout identity and the provisional knowledge lane

> **Status: proposed.** Nothing here is landed. Anchors verified against
> `beta/blackbox-v2` after the fleetd extraction and hardened over two
> adversarial design-review rounds (2026-07-20) that found, and this doc now
> closes, seventeen correctness gaps in the first draft. Line cites rot, so
> grep the named symbols before building. This is the concrete build plan for
> slices 1 and 3 of
> [locality-first-decomposition.md](../../daemon-runtime/locality-first-decomposition.md),
> and the finish of the identity model
> [repo-owned-project-state.md](repo-owned-project-state.md) specified but did
> not ship.

## 0. Decision and scope

Attack the knowledge/code seam as one push, foundation first:

1. **Identity contract.** Give project-scoped durable knowledge a key that
   travels: `(repo_id, bbox_root_relpath)` where `repo_id` is a strongly
   minted value RECORDED in committed `.bbox/config.toml`, not the weak
   computed hash. Mint a durable, checkout-local `checkout_id`. Stamp
   knowledge and indexed-lane responses with a generation.
2. **The provisional lane.** Replace `write_redirects` (the host-local map
   that today decides which worktree a repo-owned entry file lands in) with a
   versioned provisional overlay computed from each checkout's own `.bbox/`
   against its merge base with the published tree.

**The unifying model.** Do not split "base = published, worktrees =
provisional." EVERY checkout, the registered base included, has two layers:

- a **committed layer** = the entries in that checkout's COMMITTED tree
  (read via git, not the working tree), which for the base checkout is
  published truth;
- a **provisional overlay** = that checkout's uncommitted-or-branch `.bbox/`
  changes, expressed as a merge-base-relative diff (adds, modifies, and
  delete-tombstones) against the published tree.

This one model absorbs the base checkout's own dirty state, worktree
branches, and deletes uniformly (review round 2, findings 4 and 5).

**Rung limit, stated up front.** This is a MONOLITHIC-RUNG design: it holds
while every checkout lives on the daemon's host. The parent's locality
boundary says checkout `.bbox/` reads execute where the bytes live and the
daemon does not reach into a remote worker's files; a host-local overlay that
serves cross-checkout visibility cannot span machines without a
transport/aggregation contract explicitly out of scope here. Nothing here may
be extended to remote workers without that contract.

Out of scope, deliberately, on their own triggers: the harness-ward moves
(provenance, blame, render relocation), the code-corpus collector, and the
corpus off-host move.

## 1. Present state (verified)

Two identity functions, opposite behavior
(`crates/bbox-corpus-core/src/entity_ref.rs`):

- `project_id_for_path` (:499) is a host realpath hash; a worktree hashes
  differently from its base, and it does not survive a different `$HOME` or
  machine.
- `repo_id_for_root` (:511) is an **8-hex, 32-bit hash with a fallback
  chain**: first commit, else origin remote URL, else a realpath hash
  (:511-543). So it is NOT always first-commit-based, NOT guaranteed
  cross-host (the realpath fallback is host-local), and 32 bits is weak for a
  durable authority key. The identity contract treats the computed value as a
  bootstrap hint only.

Resolver shape already present
(`crates/bbox-corpus-core/src/project_record.rs`): `ProjectContext` (:41)
holds `project_id`, `repo_id: Option<String>`, `aliases`, `host_root`,
`checkout: Option<CheckoutContext>`; `CheckoutContext` (:63) holds
`checkout_dir` + a `managed` flag, its comment noting `checkout_dir` "doubles
as the checkout identity until a consumer needs a minted `checkout_id`." The
read/write asymmetry is load-bearing: `resolve_base_project_for_scope` (:98)
is the broad READ gate; `resolve_managed_fleet_worktree` +
`ResolveIntent::{Read,Write}` (`crates/bbox-indexing/src/projects.rs:466`,
:588) is the conservative WRITE gate, recognizing ONE caller-supplied path at
a time, private, with no enumeration capability. The write gate is NOT a
marker check: in-tree linked and cockpit worktrees are recognized structurally
or by managed-root location, only independent clones use
`.git/blackbox-managed-checkout` (`crates/bbox-corpus-core/src/git.rs:155-166`;
`projects.rs:438-503`), so any verification must re-run the gate, not test the
marker (review round 2, finding 3).

Loading reads the base WORKING tree directly, not the committed tree
(`crates/bbox-knowledge/src/knowledge.rs:773-827`, `fs::read_dir` /
`fs::read_to_string`), which is why "published = committed" needs an explicit
committed-tree read (finding 4). Durable scope keys on `project:
Option<String>` (path string) everywhere (:43+). The bridge is
`write_redirects` (:952), central-store only; its load-time drop (:1163-1169)
fires on mere ID existence at base (the promotion bug §4.4 fixes). Only
`learn`/`remember`/`decide` receive a worktree `write_dir`; `forget`,
`review`, `knowledge_link`, and the entry mutated by a superseding `decide` do
not (:1296-1299, :1742-1752, :1770-1782, :2482-2496) and write into base
regardless. The gap store carries the identical host-only carrier
(`crates/bbox-gaps/src/gaps.rs:175-187`) on the shared watcher/reload path.

## 2. The two defects this closes

**A. `kb.json` is required to interpret the repo's own committed files.**
`write_redirects` lives only in the host store and has no on-disk source; lose
`kb.json` and you lose the map of which worktree each in-flight repo-owned
entry belongs to. Directly contradicts the repo-owned promise ("losing the
daemon store stops being catastrophic; the project layer rebuilds from the
repos").

**B. In-flight worktree knowledge is invisible and unlabeled.** Only base
roots are loaded/watched; a peer cannot see a sibling's branch entry, and the
writer sees its own write only via the central in-memory copy, never labeled
unmerged. Measured: 17 worktrees, up to 10 divergent gap files and 5
divergent knowledge files against base.

Both are the identity-by-absolute-path failure mode that sank the satellite
arc (gap-cbcc314d). Milder on one host, identical root.

## 3. The identity contract (slice 1)

### 3.1 Durable published-scope key and repo_id minting

Published scope keys on `(repo_id, bbox_root_relpath)`.

- **`repo_id` is a strongly minted repo-FAMILY id, recorded** in committed
  `.bbox/config.toml`. Padding or copying the 32-bit computed hash adds no
  entropy and random minting lets concurrent clones diverge (review round 2,
  finding 1), so the recorded id is minted ONCE, at first eject/init, as the
  **full first-commit SHA** (not its 32-bit hash). It is repo-family identity:
  every subproject of one repo shares it, and `bbox_root_relpath` is the sole
  monorepo discriminator, carried only as the tuple's second component, not
  folded into `repo_id` (review round 3, nit 4: folding relpath in and then
  pairing it again double-counts the identity dimension). A repo with no first
  commit records a strong random id; because it is a committed file, concurrent
  clones converge on whichever commit lands, and a genuine divergence is a
  merge conflict in `.bbox/config.toml`, surfaced not silent.
- **Shallow clones fail closed, never mint (review round 5, finding 1).**
  First-commit discovery is `git rev-list --max-parents=0 HEAD`
  (`crates/bbox-corpus-core/src/git.rs:52-61`), which in a shallow clone
  returns the shallow BOUNDARY commit, not the true root, so minting a durable
  id there would fabricate a wrong identity that then travels. Minting must
  first detect a shallow repo (`git rev-parse --is-shallow-repository` /
  `.git/shallow`) and refuse: a shallow clone requires a committed recorded id
  already present, an operator-supplied id, or a history unshallow/fetch before
  any durable id is minted. Never derive durable identity from an apparent
  shallow root.
- **Resolution precedence, exact:** `project_key_override` (operator intent,
  wins) > recorded `repo_id` (committed authority) > `aka_repo_ids` remap
  (history-rewrite reconciliation) > computed `ProjectRecord.repo_id`
  (bootstrap only, for a checkout whose `.bbox/config.toml` has no recorded id
  yet).
- `bbox_root_relpath` makes monorepos first-class and decouples identity from
  `$HOME`/checkout location.
- The host-local layer (overlay, checkout registry, cache, activity stores)
  keeps `project_id`/path; it never travels.

### 3.2 Checkout identity (durable, reuse-safe)

`checkout_id` is a primary key of the overlay and the GC identity, so it may
not be a gitdir-path hash (reused when a new checkout takes a removed one's
path, wrongly inheriting retained state) nor registry-only (unreconstructable
after registry loss) (review round 2, finding 7). Mint it ONCE and persist it
in a checkout-local durable marker (`.bbox/local/checkout-id`, gitignored,
per-worktree), so it survives registry loss and a new checkout at the same
path mints a fresh one. `checkout_dir` remains the transitional fallback until
this lands. `checkout_id` is host-local and need not travel.

**The base checkout is normalized to a concrete checkout too.** Exact base
resolution returns `checkout: None` today
(`crates/bbox-indexing/src/projects.rs:628-685`), so a session scoped
directly to the base (no dispatch cwd default) would have no own overlay to
select its own dirty base-side changes (review round 3, finding 2). Every
resolved project therefore normalizes into a concrete checkout descriptor: the
base checkout gets a `checkout_id` recorded in its own
`.bbox/local/checkout-id` exactly like any worktree. The value is strong-random
and atomically create-if-absent, the SAME reuse-safe minting as every other
checkout (review round 4, finding 3: "derived from `host_root`" would
reintroduce the path-derived reuse the invariant above rejects, where a
replacement base checkout at the same path inherits stale state). `host_root`
identifies only WHERE to store and read the marker, never its value. A
base-scoped session then has an own-checkout overlay by the same rule as a
worktree session, and `checkout: None` never reaches the visibility path.

### 3.3 Checkout discovery (closes round 1, finding 2)

Enumeration does not exist today. Add a **host-local checkout registry**:
first provisional write from a checkout records
`(checkout_id, checkout_dir, repo_id, bbox_root_relpath, branch_ref)`. It is a
discovery INDEX, not authority (entries live on disk). On startup: reload,
**re-run the conservative WRITE gate per candidate dir** (not a marker check,
finding 3), re-read surviving checkouts' `.bbox/`, drop rows whose dir is gone
or no longer passes the gate.

Degradation, stated honestly:

- DISCOVERABLE set (cockpit roots `$BRO_HOME/{fleet,agent}/worktrees` + `git
  worktree list` of every registered repo) re-enumerates even if the registry
  is lost, so it self-heals unconditionally.
- ARBITRARY-location marker clones are re-discoverable only via the registry;
  if it is also lost, they re-register on next write. State the limit rather
  than claim universe-scan.

Lifecycle: dynamic registration on write, periodic reconciliation, explicit
teardown deregistration. Provisional-checkout watching is new wiring; the
existing watcher initializes from base roots only
(`src/server/background.rs:173-180`).

### 3.4 Generation stamps

Knowledge and indexed responses carry the commit/generation they were built
from, so a consumer distinguishes published from provisional. Additive.
Per-snapshot granularity likely suffices; per-entry is the fallback.

### 3.5 Migration: inventory and quarantine (closes round 1, finding 7)

Lazy "stamp on read, fall back to path" mis-keys (repo A at `/old/path`
stamped repo B after A moves and B occupies it) and a generation cannot prove
coverage across offline hosts and dormant stores. Migration is an explicit
**schema epoch**: a one-time inventory resolves every entry to
`(recorded repo_id, relpath)` by §3.1 precedence, QUARANTINES the
unresolvable (moved, ambiguous, no reachable recorded id) for operator
resolution rather than re-keying by current path, retains the path key only as
an inventory-bounded read fallback with deterministic conflict handling
(recorded-id match wins; a path match disagreeing with a recorded id is
quarantined), and asserts coverage by the epoch marker plus an empty
quarantine, never a generation number.

### 3.6 Two known consumers

Knowledge scope keying (§3.1) and the index's doc stamping (already through
`resolve_base_project_for_scope`). Scope to those two.

## 4. The provisional overlay (slice 3)

### 4.1 The overlay: merge-base diff with tombstones

**One elected publisher per scope (closes round 3, finding 1).** The registry
permits multiple registered paths with the same repo identity
(`crates/bbox-indexing/src/projects.rs:144-184`); if two clones of the same
`(repo_id, bbox_root_relpath)` have divergent HEADs, today's ID-based load
overwrites by root iteration order (`knowledge.rs:1160-1172`), a silent
data-dependent-on-scan-order bug. So each published scope elects exactly ONE
base publisher, and the election is HOST-LOCAL state, not committed config: the
elected publisher's host PATH lives in the host registry, because
`.bbox/config.toml` is byte-identical in every clone and cannot portably name
one host path (review round 4, finding 2). Committed config may carry only a
portable published-ref policy, never a host path. Election defaults to the
first-registered publisher for a scope; an operator override selects a
different registered path via the registry. A second registered publisher for
an already-owned scope does not contribute published truth; it registers as a
(managed) checkout whose divergence surfaces as its own overlay, or, if it
claims to publish, the daemon fails closed and surfaces the duplicate-publisher
ambiguity rather than picking by iteration order. If the elected publisher's
path disappears, the scope has no publisher until re-election and reads fail
closed (surfaced), rather than silently falling to another path.

Published truth is the COMMITTED tree of the elected publisher, read via git
(`git show`/`cat-file`, or a `git worktree list` committed-tree read), NOT the
working tree the loader reads today (finding 4). A checkout's provisional
overlay is the **merge-base-relative diff** of its `.bbox/knowledge/` against
its merge base with the published tree, so change detection is git-shaped, not
content-hash-vs-current-base (which cannot tell an untouched-behind-base copy
from an intentional revert) (finding 5):

- **Add / modify:** a tuple `(published_scope, checkout_id, entry_id)` with
  the branch content and a content hash.
- **Delete:** a **tombstone** tuple (not an absent tuple), so own-checkout
  results do not silently fall back to the published entry and a deletion can
  promote (finding 5).
- **Exclusion:** an entry identical to, or strictly behind, the merge base
  contributes nothing to the overlay (avoids every branch echoing base as
  noise). "Behind vs revert" is decided by the merge-base diff, not a hash
  comparison to current base.
- The base checkout's OWN uncommitted `.bbox/` changes form its overlay by the
  same rule, so a dirty base-side `bbox_learn` is provisional, never falsely
  published or invisible (finding 4).

### 4.2 Discovery and self-healing

Overlays are recomputed from the checkouts the registry (§3.3) knows, verified
live on load. Losing `kb.json` costs a recompute, not the map (defect A
closed), with §3.3's discoverable-vs-arbitrary degradation.

### 4.3 Visibility: hybrid, session-authoritative (closes round 2, finding 6)

Decided: a caller sees published entries plus its OWN checkout's overlay;
peers' overlays only behind an explicit flag. The own-checkout identity MUST
be the server-authoritative session checkout, because tool-arg defaults fill
only omitted values and an explicit model-supplied checkout would otherwise
win (`src/orchestration/mod.rs:2139-2148`, :2238-2249), letting a caller
present a peer checkout as its own and get shadowing without the flag. So:

- The MCP session records the resolved `CheckoutContext` at init (extend
  `src/server/handler.rs:112-146`, which stores only `host_root` today).
- Own-checkout shadowing binds to that session value ONLY. A model-supplied
  checkout argument is validated against the session checkout and ignored (or
  rejected) for shadowing purposes; it may not name a peer checkout to gain
  its overlay.
- The request-level cwd default (via tool-arg-defaulting, the
  `RETRIEVAL_PROJECT_DEFAULT_TOOLS` pattern) applies only when no session
  checkout exists, and still resolves server-side, never trusting a supplied
  value.

### 4.4 Promotion: committed-base observation, content equality

A branch's overlay entry promotes to published when the change reaches the
COMMITTED base tree: the base committed file's content hash matches the
branch's overlay content (finding 3, promotion by equality not ID existence),
observed by re-reading the committed base tree (not the working tree). A
delete-tombstone promotes when the entry is absent from the committed base
tree. Until then the overlay entry stands.

### 4.5 The merge gate: candidate tree + scoped contradiction lint

Two buildable halves and one scoped dependency (finding 6, round 1; finding 8,
round 2):

- **Candidate tree, buildable now:** construct the would-be-merge tree without
  touching any working checkout via `git merge-tree` (or a temporary index),
  and run the gate over THAT tree, pre-integration. The closeout `pre_push`
  hook runs after the local fast-forward (`fleet_worktree.rs:607-645`), so the
  gate is a new pre-integration point, not that hook.
- **`render --check`, buildable now:** render the candidate tree's `.bbox/`
  and diff against its committed provider files; a mismatch fails. Closes the
  render-race staleness.
- **Contradiction lint, scoped dependency:** `bbox_lint` today checks
  status/expiry/recall/same-title dupes, not semantic contradiction
  (`src/tools/render.rs:80-86`; `knowledge.rs:2348-2434`). A concrete first
  cut: within one `published_scope`, flag two entries whose directives target
  the same subject with opposing polarity (a "use X" vs "never X" pair keyed
  on a normalized subject). This is a named sub-project with a defined
  starting rule, not an assumed capability; the gate ships `render --check`
  first and adds contradiction lint as it lands.

### 4.6 Mutation coverage with crash-consistent multi-file writes

Every worktree-reachable durable mutation routes through the checkout-scoped
write: `learn`/`remember`/`decide`, plus `forget`, `review`, `knowledge_link`,
and the prior entry mutated by a superseding `decide` (finding 10, round 1). A
supersession touches at least two files, which independent per-file atomic
writes cannot make atomic together (finding 9, round 2).

**The traveling transaction is `git commit`, not a daemon marker (closes round
3, finding 3).** The entry files are repo-committed and travel with the branch,
so a repo-owned singleton commit marker would conflict across branches and a
host-local marker would not travel. The resolution is that the durable,
traveling boundary is the agent's `git commit` of the changed `.bbox/` files;
git history is the transaction that travels. The daemon's ONLY atomicity job is
its own uncommitted multi-file write window, which is purely host-local. It is
a write-ahead redo/undo log, not a one-way discard (review round 4, finding 1:
a pending pointer hides a partial apply from readers but does not by itself
make the canonical replacement atomic, and discard cannot restore files
already promoted):

- **Exclusive transaction claim (review round 5, finding 2).** The per-checkout
  pending pointer is also the mutual-exclusion lock: a write acquires it by
  atomic create-if-absent, and daemon writes, crash recovery, and `/closeout`
  all contend for the SAME claim, so two writes cannot race and closeout cannot
  observe "no pointer" in the instant before a new transaction creates one.
  Closeout acquires (or revalidates) the claim and holds it through candidate
  commit construction, so no write applies canonical files underneath a
  running integration.
- **Recoverable manifest, both directions.** Under the claim, the write stages
  every new file version to a host-local scratch path and records a manifest
  holding, per affected path, BOTH the old bytes (or a content-addressed copy)
  and the new staged ref. It then flips the pending pointer, and only after the
  pointer is set does it apply, copying each staged version into its canonical
  path.
- **Power-loss durability (review round 5, finding 3).** Atomic rename orders
  visibility but not durability, so the protocol pins an fsync order: staged
  data and the manifest are fsync'd before the pointer is created, the pointer's
  directory entry is fsync'd, and canonical replacements plus the final pointer
  removal are fsync'd before the transaction is reported complete. A power loss
  at any step leaves the manifest and pointer recoverable to a terminal state.
- **Idempotent recovery to a terminal state.** The loader and watcher skip a
  checkout whose pending pointer is set, so a scan never observes a partial
  apply. On restart, recovery reads the manifest and drives it to a TERMINAL
  state before clearing the pointer: either roll forward (re-copy every staged
  new version, idempotent and safe to repeat) or roll back (restore every old
  version from the manifest). Because the manifest retains old and new for
  every path, recovery is possible even after some canonical files were already
  replaced; a crash never leaves some-old-some-new visible.
- **Same-commit proof at closeout (commit-completeness).** git commit is the
  traveling boundary, but the agent controls it, so the daemon cannot force all
  N files into one commit; instead the completed transaction manifest names
  every affected path with its expected new blob ref (or deletion), and
  `/closeout` proves that the candidate commit carries exactly those blob IDs
  and deletions for those paths, not merely that the path NAMES occur in one
  commit (review round 5, finding 3: name membership does not prove the content
  is the transaction's). It fails closed on a partial or content-mismatched
  commit (one file of a two-file supersession, or a path present with stale
  bytes). A checkout with an unresolved pending pointer is likewise ineligible
  for integration.
- **Generation-zero migration.** Existing committed or on-disk entry files are
  generation 0 and always covered, so the loader never starts ignoring legacy
  files that predate any transaction; the pending pointer guards only the
  daemon's active write window, not file existence.

Because git commit is the traveling boundary, none of this host-local
machinery travels, which is why the earlier "generation commit marker" framing
(a marker that both provides crash atomicity AND travels with repo-owned
entries) was overloaded and is replaced.

The branch-private alternative (no overlay; in-flight entries invisible
outside their worktree until merge) stays the fallback: strictly less
machinery, regresses cross-fleet read-your-writes. Demoting to it is a
deletion, not a redesign.

## 5. What retires, and the gap-store twin (closes round 1, finding 11)

Retires: `write_redirects`, its purge-exclusion, `repo_owned_carrier`'s
redirect branch, the "host store required to interpret repo files" property,
and the path-string scope key as durable authority (kept as an
inventory-bounded read fallback through migration, then dropped).

The **gap store is a parallel twin** (same host-only carrier,
`gaps.rs:175-187`, shared watcher/reload). This design either **generalizes
the overlay to cover gaps** (preferred: one overlay model, one registry, one
promotion rule, one host-local staged-write path) or **sequences a named gap
migration** (§6 slice 7). Not left implicit.

## 6. Sequencing

1. **Identity contract, additive.** Strongly-minted recorded `repo_id` +
   `bbox_root_relpath` + precedence; durable checkout-local `checkout_id`;
   generation stamps; schema-epoch inventory + quarantine.
2. **Checkout registry + discovery lifecycle.** Register on write, startup
   reload + write-gate re-verify, periodic reconciliation, teardown
   deregistration.
3. **Committed-tree published read + overlay.** Committed-tree source for
   published truth; merge-base-diff overlay with tombstones; content-equality
   committed-base promotion; retire `write_redirects`.
4. **Session checkout propagation.** Session-recorded `CheckoutContext`;
   server-authoritative own-checkout shadowing; validated request default.
5. **Mutation coverage + crash-consistent writes.** Route all mutation verbs;
   host-local staging + recoverable manifest + pending pointer for multi-file
   atomicity, with closeout proving same-commit membership.
6. **Merge gate.** `git merge-tree` candidate tree; `render --check`;
   contradiction lint as the scoped sub-project; the explicit promotion hook.
7. **Gap-store convergence.** Generalize the overlay to gaps, or the sequenced
   gap migration.
8. **Cut the path fallback** once the epoch marker plus empty quarantine
   confirm coverage.

Each slice lands on the monolith and gets the full lane gate.

## 7. Migration and GC hazards

- **GC and branch durability (resolves the round-2 finding-2 contradiction).**
  A provisional overlay has no on-disk source once its checkout is torn down,
  so the daemon does NOT retain vanished content. Durability is the branch
  REF, not a daemon copy: a torn-down checkout's overlay drops from the lane,
  and its knowledge re-enters either when the branch is checked out again
  (registry re-registers, overlay recomputes) or when it merges to the
  committed base (promotion). The consequence is explicit and accepted:
  cross-fleet visibility of a pushed-but-not-checked-out branch's provisional
  knowledge is lost until re-checkout or merge; the design does not fetch
  remote refs to reconstruct it (that is the out-of-scope multi-host
  transport). This supersedes the first revision's "retain on removal," which
  had no recoverable source.
- **Read/write asymmetry.** The overlay keys on the conservative WRITE
  recognition set, never the broad read set, or a user worktree's scratch
  leaks in. Two gates stay distinct (`bbox-corpus-core` CLAUDE.md invariant);
  this design consumes them.
- **`repo_id` remap.** `aka_repo_ids` consulted at resolution per §3.1
  precedence or a history rewrite orphans knowledge; fork/upstream conflation
  handled by opt-in `project_key_override`.
- **Multi-host.** Monolithic-rung; cross-machine provisional visibility needs
  the out-of-scope transport, and visibility is limited to same-host checkouts
  until it exists.

## 8. Open questions

- **Contradiction-lint semantics.** How far past the §4.5 opposing-polarity
  first cut to push (scope overlap, entailment) before diminishing returns.
- **Provisional query UX.** The peer-visibility flag name and whether an
  orchestrator gets a fleet-wide provisional view.
- **Generation stamp granularity.** Per-snapshot vs per-entry.
- **Gap convergence shape.** One generalized overlay vs a gap-specific
  migration reusing the registry, promotion, and staged-write path.
- **Remote-rung transport.** How provisional visibility would ever cross
  machines, if that rung is taken.

## 9. Relationship

- **Implements** slices 1 and 3 of
  [locality-first-decomposition.md](../../daemon-runtime/locality-first-decomposition.md);
  its visibility and promotion decisions are adopted and hardened here.
- **Finishes** [repo-owned-project-state.md](repo-owned-project-state.md): the
  `(repo_id, bbox_root_relpath)` model it specified and left unbuilt, with the
  recorded-id authority its shallow-clone note implied made explicit, and the
  provisional-vs-published split its "uncommitted entries are provisional"
  rule anticipated, now unified as the committed-layer-plus-overlay model.
- **Bounded by** [remote-worker-boundary.md](../../bro-harness/remote-worker-boundary.md):
  the overlay is single-host; "reads execute where the bytes live" is why it
  cannot span machines without new transport.
- **Carries forward** the fleetd-era lesson: identity-by-path fractured the
  satellite arc across hosts (gap-cbcc314d) and makes `write_redirects`
  fragile; durable identity now, on one machine, keeps the remote rung
  operational work instead of architecture.
- **Preserves** the read/write gate asymmetry owned by
  `bbox-corpus-core`/`bbox-indexing`; this design consumes it, never
  re-derives it.
