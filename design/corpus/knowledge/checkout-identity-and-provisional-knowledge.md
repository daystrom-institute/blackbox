---
title: "Checkout identity and the provisional knowledge lane"
kind: design
lifecycle: partial
corpus: blackbox-design
topic:
  - corpus
  - knowledge
  - daemon-runtime
tags: [identity, worktrees, knowledge-seam, provisional-lane, write-redirects, repo-id, render-check]
brief: "Retire write_redirects, the host-local map that makes kb.json required to interpret the repo's own committed .bbox/ files. Two moves in one push: (1) a durable published-scope identity key anchored on a strongly-minted repo_id recorded in .bbox/config.toml (the computed first-commit hash is only a bootstrap hint) plus bbox_root_relpath, and (2) a versioned provisional knowledge lane. Each scope has one published layer read from a pinned committed ref; every checkout, including the publisher checkout, contributes at most one provisional overlay computed as a merge-base-relative working-tree diff with tombstones. Overlay keys are (published_scope, checkout_id, entry_id); promotion is content equality at the pinned published commit; own-checkout visibility binds only to server-authoritative session context. Monolithic-rung only. Slices 1 + 3 of locality-first-decomposition.md, hardened through seven adversarial review rounds and a live-code repair pass (2026-07-20)."
---

# Checkout identity and the provisional knowledge lane

> **Status: partial.** The additive foundation, prerequisite repair, dark
> overlay, session-authoritative committed view, promotion, and registry
> lifecycle through slice 3.7 are landed on `beta/blackbox-v2`. The merge gate,
> gap convergence, and the final path-fallback cut remain.
> Anchors were re-verified against that branch after the
> slice-3.2 checkpoint and this design was repaired against the live loader,
> resolver, index, and entity-ref paths on 2026-07-20. Line cites rot, so grep
> the named symbols before building. This is the concrete build plan for
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
   knowledge and indexed-lane responses with a `built_from` commit.
2. **The provisional lane.** Replace `write_redirects` (the host-local map
   that today decides which worktree a repo-owned entry file lands in) with a
   versioned provisional overlay computed from each checkout's own `.bbox/`
   against its merge base with the published tree.

**The unifying model.** Do not split "base = published, worktrees =
provisional." Each published scope has ONE **published layer**: entries at the
unique publisher's pinned committed ref, read via git rather than its working
tree. EVERY checkout, the publisher checkout included, may then contribute one
**provisional overlay**: its branch commits plus dirty `.bbox/` changes,
expressed as a merge-base-relative working-tree diff (adds, modifies, and
delete-tombstones) against that published layer.

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

### 0.1 Implementation checkpoint and correction rule

Landed, additive, and full-profile lane-verified: identity primitives (1a),
knowledge `built_from` storage (1b), the pure schema inventory pass (1c), the
checkout-registry store and discovery primitive (2a), committed-tree git
primitives (3.0), the duplicate-publisher guard (3.1), and
published-vs-provisional labeling (3.2). These slices deliberately changed no
live query behavior.

Also landed and full-profile verified: init/eject merge-preserve and record
`repo_id`; the checkout registry uses the composite
`(checkout_id, published_scope)` key; base and worktree writes resolve a
concrete checkout id and monorepo project directory; publisher refs are pinned;
and `learn`/`remember`/`decide` register before mutation and recompute an exact
P/H/B overlay afterward. Session-authoritative `published|own|all` visibility
now drives list, hybrid search, inspection, render, graph, inbox, discover
seed, and logical-ref-scoped index replacement.

Mutation coverage and crash consistency are also landed. `forget`, `review`,
`knowledge_link`, and both sides of a superseding `decide` resolve through the
authoritative checkout. Repo-owned writes use an exclusive pending claim,
staged old/new bytes, a recoverable manifest, loader/watcher exclusion, and
startup roll-forward. Fleet closeout takes the same claim and proves every
completed manifest's terminal blobs against the locally folded candidate tree
before push.

Still not implemented: response-level `built_from`, the merge-gate,
gap-convergence, and path-fallback-cut slices. Section 6 is the authoritative
remaining sequence.

When this document and an additive primitive disagree, the contract here wins
and the primitive is repaired before live wiring.

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
`checkout_dir`, a `managed` flag, and the additive `checkout_id: Option<String>`
field. Live resolution still leaves `checkout_id` empty and returns
`checkout: None` for an exact base project. The
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

The resolver is not yet monorepo-complete for checkout writes. A managed
worktree resolution returns the checkout top, while a registered subproject's
repo-owned carrier is `checkout_top/bbox_root_relpath`; matching by git common
dir can also select the wrong registered subproject when one repository has
several registered `.bbox` roots. Slice 3.3 must resolve the published scope
first, then derive the corresponding checkout project root from that relpath.

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

A worktree write is NOT peer-invisible today. The adapter rewrites its
`project` to the registered base, inserts or updates the daemon-wide
`KnowledgeStore.entries`, and immediately upserts the same entry id into the
knowledge index. Every caller can therefore see a new provisional entry, and
an edit of an existing id globally shadows the published version, with no
checkout label. The worktree file itself remains invisible to reload, which is
why the central retention copy and `write_redirects` are required across a
daemon restart.

## 2. The two defects this closes

**A. `kb.json` is required to interpret the repo's own committed files.**
`write_redirects` lives only in the host store and has no on-disk source; lose
`kb.json` and you lose the map of which worktree each in-flight repo-owned
entry belongs to. Directly contradicts the repo-owned promise ("losing the
daemon store stops being catastrophic; the project layer rebuilds from the
repos").

**B. In-flight worktree knowledge is globally shadowing and unlabeled.** Only
base roots are loaded/watched, so the worktree file is not a reconstructable
source. Instead the central in-memory copy is visible to every caller under the
base scope and is indexed under `knowledge:<entry_id>`, the same identity as
published truth. A worktree edit can therefore replace what peers retrieve
without any provisional label; after loss of the central retention copy it
disappears until merge. Measured: 17 worktrees, up to 10 divergent gap files
and 5 divergent knowledge files against base.

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
  Production init/eject must call one `ensure_recorded_repo_id` path before
  making the project repo-owned. It parses and merge-updates the existing TOML,
  preserves aliases and every unrelated table, atomically records the minted
  id only when `project.repo_id` is absent, and is idempotent thereafter.
  `bbox_project_init(force=true)` may refresh skeleton material but MUST NOT
  erase or remint identity-bearing project config. The landed
  `mint_repo_id`/config fields are primitives only; no production caller wires
  this yet.
  Existing registered scopes without a recorded id enter `NeedsRepoId` and do
  not receive an overlay. The upgrade path is an explicit idempotent project
  init/eject operation that records the id for review and commit; startup does
  not silently dirty a repository to cross this authority boundary.
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
- **Overlay admission requires recorded authority.** The computed bootstrap
  hint may inventory old path-keyed entries and explain a migration candidate,
  but slice 3.3 does not register or publish an overlay scope until init/eject
  has recorded `repo_id` (or an explicit `project_key_override` supplies
  operator authority). This prevents a new host from making the weak fallback
  a live durable key merely because the wiring arrived before migration.
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

Normalization is daemon orchestration layered on the shared resolver, not a
reason to weaken or duplicate its conservative write gate. It produces a
scope-aware descriptor:

```text
ResolvedCheckoutScope {
  published_scope: (repo_id, bbox_root_relpath),
  checkout_id,
  checkout_dir,          # git checkout top; owns .bbox/local/checkout-id
  checkout_project_dir,  # checkout_dir joined with bbox_root_relpath
  branch_ref,
}
```

For the registered base, `checkout_dir` is its git top and
`checkout_project_dir` is the registered project root. For a worktree, the
same relpath is projected into that checkout. Existing store adapters may keep
their legacy `write_dir=None` base convention; no `checkout: None` may cross
into overlay computation or session visibility.

### 3.3 Checkout discovery (closes round 1, finding 2)

Enumeration did not exist before slice 2a. The landed **host-local checkout
registry** is the starting primitive, but its key must be repaired before use.
One checkout can carry several registered `.bbox` roots in a monorepo, so a
registry row is keyed by `(checkout_id, published_scope)`, not `checkout_id`
alone. First provisional write for a scope records the full
`ResolvedCheckoutScope`. Re-registering the same composite key updates its path
or advisory branch ref without overwriting sibling relpaths in the same
checkout.

The registry is a discovery INDEX, not authority; entries live in
`checkout_project_dir/.bbox/` on disk. On startup: reload, **re-run the
conservative WRITE gate per candidate dir** (not a marker check, finding 3),
re-resolve every row's recorded published scope, re-read surviving checkouts,
and drop a row if the directory is gone, the gate rejects it, the checkout-id
marker disagrees, or the relpath no longer resolves to that scope. A mismatch
never reassigns retained overlay state to a replacement checkout.

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

**Register-before-write ordering.** Resolve and register the composite row
before the repo-owned mutation, then recompute that scope's overlay after the
file write succeeds. A row left by a failed write is harmless and reconciles;
writing first creates an unreconstructable window for an arbitrary-location
managed clone if the daemon dies before registration. Registration itself does
not make content visible.

### 3.4 Built-from stamps

Knowledge and indexed responses carry the commit they were built from (a
`built_from` stamp), so a consumer distinguishes published from provisional.
The name is deliberate: "generation" already means the committed-file
generation-purge in `knowledge.rs`, and "epoch" is the one-time schema-migration
boundary in §3.5, so the per-response provenance stamp takes a third, unambiguous
name. The granularity is decided: **per-view snapshot, referenced by entries**,
not a repeated per-entry commit field.

- A published view stamp is
  `(published_scope, published_ref, publisher_commit)`.
- A checkout overlay stamp is
  `(published_scope, checkout_id, publisher_commit, checkout_head,
  merge_base, working_fingerprint)`. `working_fingerprint` covers uncommitted
  bytes that no commit can name.
- Each provisional search document carries the overlay snapshot id plus its own
  content hash. Text/list responses emit the distinct stamps used by their
  returned rows once, then point rows at them.

The landed `KnowledgeStore.built_from` map is internal load-time storage only;
it is not yet the response contract above. The index side substantially exists
already: project-file docs are
stamped from `(repo_id, project_id, head_sha)` (`project_files.rs`
`clean_snapshot_id` + `head_fingerprint`), so this slice mainly adds the
knowledge view stamps and index fields.

### 3.5 Migration: inventory and quarantine (closes round 1, finding 7)

Lazy "stamp on read, fall back to path" mis-keys (repo A at `/old/path`
stamped repo B after A moves and B occupies it) and a `built_from` stamp cannot
prove coverage across offline hosts and dormant stores. Migration is an explicit
**schema epoch**: a one-time inventory resolves every entry to
`(recorded repo_id, relpath)` by §3.1 precedence, QUARANTINES the
unresolvable (moved, ambiguous, no reachable recorded id) for operator
resolution rather than re-keying by current path, retains the path key only as
an inventory-bounded read fallback with deterministic conflict handling
(recorded-id match wins; a path match disagreeing with a recorded id is
quarantined), and asserts coverage by the epoch marker plus an empty
quarantine, never a per-response `built_from` stamp.

The landed inventory function is pure and writes no gate. The live migration
adds two persisted products:

- each repo-owned `.bbox/knowledge/` scope carries a committed
  `.schema-epoch` marker naming epoch 1 and its
  `(repo_id, bbox_root_relpath)`; the non-JSON name keeps it outside the
  one-JSON-file-per-entry loader;
- the host state carries the central-store inventory ledger and any
  unresolvable legacy entries in a quarantine file, because an entry that
  cannot be mapped to a repository has no honest repo-owned destination.

Writing the repo marker requires a recorded/overridden repo authority and a
clean inventory for that scope. A daemon on another host re-runs inventory for
its own legacy central store even when the committed repo marker is already
present. The path fallback is cut locally only when every registered scope has
the epoch marker and the local quarantine is empty; no host claims coverage for
an offline host's private central store.

### 3.6 First consumers

Knowledge scope keying (§3.1) and the index's doc stamping (already through
`resolve_base_project_for_scope`) are the first identity consumers. Slice 3.4
then carries the same resolved view through list, search, inspection, render,
graph, and inbox surfaces; none may reconstruct scope or visibility
independently.

## 4. The provisional overlay (slice 3)

### 4.1 The overlay: merge-base diff with tombstones

**One publisher per scope (closes round 3, finding 1).** The registry
permits multiple registered paths with the same repo identity
(`crates/bbox-indexing/src/projects.rs:144-184`); if two clones of the same
`(repo_id, bbox_root_relpath)` have divergent HEADs, today's ID-based load
overwrites by root iteration order (`knowledge.rs:1160-1172`), a silent
data-dependent-on-scan-order bug. The operator decision after the review was to
collapse election into a **duplicate-publisher guard**: zero registered paths
for a scope is `NoPublisher`, exactly one is the publisher, and two or more is
`DuplicatePublisher`. There is no first-registered winner, path override, or
automatic demotion of a second clone into a checkout. The operator resolves a
duplicate by unregistering/re-keying a clone; until then that scope fails
closed and surfaces all claiming paths.

**Published ref is pinned, not moving `HEAD`.** When a scope first has one
publisher, a host-local publisher-ref pin records the publisher's full symbolic
branch ref (`refs/heads/<branch>`). Detached HEAD cannot seed a pin and fails
until an operator supplies one. Reads resolve that pinned ref in the unique
publisher repository; switching the publisher checkout to another branch does
not redefine truth, and advancing the pinned branch by pull/merge does. A
missing or non-commit ref fails the scope. Repinning is an explicit operator
action and an auditable state change. A future committed config field may
provide a portable default ref, but committed config never names a host path.

Published truth is the COMMITTED tree at that pinned ref, read via git
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

The buildable algorithm is exact:

1. resolve the pinned published ref to commit `P`;
2. resolve the checkout's `HEAD` to `H` and compute `B = merge_base(H, P)`;
3. read the scope's knowledge file map from committed tree `B`;
4. read the same map from `checkout_project_dir`'s working tree, including
   untracked JSON files and excluding `.bbox/local`;
5. over the union of paths, emit an upsert when working bytes differ from `B`,
   a tombstone when a `B` path is absent, and nothing when bytes are equal;
6. parse and validate every emitted upsert, including filename stem equal to
   entry id and no duplicate ids, then publish one immutable overlay snapshot.

For the publisher checkout on the pinned branch, `H == P` and `B == P`, so the
same algorithm yields only its uncommitted changes. For a checkout strictly
behind, `B == H` and its unchanged files emit nothing, so current published
truth wins. Branch commits and dirty bytes are handled together without
special cases.

Dark overlay state is separate from `KnowledgeStore.entries` and never
serialized into `kb.json`:

```text
OverlayKey = (PublishedScope, checkout_id, entry_id)
OverlayValue = Upsert { entry, content_hash } | Tombstone
OverlaySnapshot = { stamp, values, status }
```

Slice 3.3 computes and inspects this state but does not merge it into list,
render, graph, inbox, or index consumers. That guarantees the dark slice has
zero retrieval behavior change and prevents the current global-by-id store
from collapsing peer variants before slice 3.4 supplies the visibility model.

### 4.2 Discovery and self-healing

Overlays are recomputed from the checkouts the registry (§3.3) knows, verified
live on load. Losing `kb.json` costs a recompute, not the map (defect A
closed), with §3.3's discoverable-vs-arbitrary degradation.

Slice 3.3 recomputes after a successful daemon-owned knowledge write and offers
an internal refresh used by tests and diagnostics. Slice 3.6 adds startup
reconciliation, periodic refresh, teardown deregistration, and dynamic watcher
coverage while keeping provisional roots outside artifact-install authority.

Overlay publication is all-or-nothing per `(published_scope, checkout_id)`.
One unreadable, malformed, filename/id-mismatched, or duplicate-id file marks
that snapshot `Invalid` with diagnostics; the daemon never publishes a partial
overlay that would silently fall back to published values for the rejected
paths. The previous valid snapshot is not retained as if current. Published
truth remains usable for other sessions, but the affected checkout cannot
claim read-your-writes until its overlay validates.

Selecting `own` for an invalid own overlay hard-errors that scope rather than
silently returning published values. Selecting `all` omits an invalid peer
overlay, keeps published and other valid overlays, and reports that checkout in
structured degradation diagnostics. An unscoped aggregate follows the same
scope-local omission rule.

Publisher failure is scope-local. An explicit query/render for a
`NoPublisher`, `DuplicatePublisher`, or unresolvable-pinned-ref scope returns a
hard error. An unscoped aggregate omits that scope and returns structured
`degraded.scopes` diagnostics while continuing to serve global and healthy
scopes. No failure path falls back to working-tree-as-published or chooses a
publisher by scan order.

### 4.3 Visibility: hybrid, session-authoritative (closes round 2, finding 6)

Decided: a caller sees published entries plus its OWN checkout's overlay;
peers' overlays only behind an explicit policy. The public query parameter is
`provisional = published | own | all`. Default is `own` only when the server
has an authoritative managed checkout for the MCP session; otherwise it is
`published`. `all` is always explicit, including for orchestrators; role names
do not silently widen visibility.

The own-checkout identity MUST
be the server-authoritative session checkout, because tool-arg defaults fill
only omitted values and an explicit model-supplied checkout would otherwise
win (`src/orchestration/mod.rs:2139-2148`, :2238-2249), letting a caller
present a peer checkout as its own and get shadowing without the flag. So:

- The MCP session records the normalized `ResolvedCheckoutScope` at init
  (extend `src/server/handler.rs:112-146`, which stores only `host_root`
  today). Worktree authority must pass the conservative WRITE gate; broad
  read-side aliasing is insufficient. An exact registered base is normalized
  as §3.2 specifies.
- Own-checkout shadowing binds to that session value ONLY. A model-supplied
  project/checkout argument may scope published results but cannot create,
  replace, or widen own-checkout authority.
- There is no request-level cwd fallback for own shadowing until the transport
  carries provenance that distinguishes a trusted injected cwd from an
  explicit model argument. Direct clients that want `own` visibility connect
  with authoritative project context; otherwise they receive `published`.

**Provisional variants need first-class identity.** The current
`knowledge:<entry_id>` ref and index upsert key can represent only one version,
so it remains the durable PUBLISHED entity. A provisional upsert is addressed
as
`provisional_knowledge:<scope_hash>:<checkout_id>:<entry_id>`, where
`scope_hash` is the full SHA-256 of `(repo_id, bbox_root_relpath)`. Its
properties include the unhashed published scope, `logical_ref =
knowledge:<entry_id>`, checkout label, content hash, and overlay snapshot
stamp. Tombstones are overlay state used by visibility filtering, not
searchable content documents.

Visibility is applied before ranking, inspection, rendering, or edge
projection:

- `published`: published documents only;
- `own`: published documents except ids shadowed or tombstoned by the session
  overlay, plus that overlay's provisional upsert documents;
- `all`: published documents plus every valid checkout's provisional upsert,
  explicitly labeled; tombstones are returned only in structured provisional
  diagnostics.

This filter must be part of both direct `bbox_knowledge` listing and hybrid
search/index retrieval. Post-ranking decoration is insufficient: it can leak a
peer document or drop a shadowing own result below the candidate cutoff.
Inspection of a compound provisional ref is stable and unambiguous even when
several checkouts modify the same logical entry.

### 4.4 Promotion: pinned-ref observation, content equality

A branch's overlay entry promotes to published when the change reaches the
published tree: the committed file at `P` has the same content hash as the
branch overlay (finding 3, promotion by equality not ID existence). Observation
re-reads commit `P` at the pinned published ref, never a working tree. A
delete-tombstone promotes when the entry is absent from `P`. The observer then
removes the matching provisional variant and its index document; the published
document is rebuilt from `P`. It never copies checkout bytes into the
publisher.

An id present at `P` with different bytes does not promote, and one checkout's
promotion does not remove a different checkout's variant of the same logical
id. The comparison runs before publishing each recomputed snapshot, so a
restart or missed event converges idempotently. Until content equality or
published absence is observed, the overlay entry stands.

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
machinery, preserves local file-level read-your-writes but removes the current
daemon-wide in-flight visibility used by multi-agent campaigns. Demoting to it
is a deletion, not a redesign.

**Landed implementation anchors (slice 3.7).** The transaction protocol lives
in `bbox_knowledge::transaction`; checkout mutation seeding and restoration
live beside `persist_repo_owned_mutation_at`; lifecycle startup owns stale
pending recovery; and `bro_tools::fleet_worktree` owns the closeout claim plus
candidate-blob proof. These are named anchors rather than line cites so this
design survives mechanical movement during later decomposition.

## 5. What retires, and the gap-store twin (closes round 1, finding 11)

Retires: `write_redirects`, its purge-exclusion, `repo_owned_carrier`'s
redirect branch, the "host store required to interpret repo files" property,
and the path-string scope key as durable authority (kept as an
inventory-bounded read fallback through migration, then dropped).

The **gap store is a parallel twin** (same host-only carrier,
`gaps.rs:175-187`, shared watcher/reload). This design either **generalizes
the overlay to cover gaps** (preferred: one overlay model, one registry, one
promotion rule, one host-local staged-write path) or **sequences a named gap
migration** (§6 slice 8). Not left implicit.

## 6. Sequencing

The eight additive slices in §0.1 remain valid foundation, but the live
sequence begins with a repair gate:

1. **Prerequisite repair (landed).** Wire merge-preserving `repo_id` recording into
   init/eject; make the checkout registry composite-keyed by checkout and
   published scope; normalize base checkouts; project `bbox_root_relpath` into
   the corresponding worktree project directory; add the host-local
   publisher-ref pin and scope failure states. Update tests that currently
   assert checkout-id-only upsert.
2. **3.3 dark overlay + register-on-write (landed).** Add `ResolvedCheckoutScope`,
   immutable overlay snapshots, exact merge-base working-tree diff with
   tombstones, validation, view stamps, and register-before-write ordering for
   `learn`/`remember`/`decide`. Compute published maps from the pinned committed
   ref, but keep both maps out of every live query/index/render consumer.
   Diagnostics prove recomputation without changing visible behavior.
3. **3.4 committed view + session-authoritative visibility (landed).** Replace the
   working-base loader with the committed published map plus base overlay;
   carry `ResolvedCheckoutScope` on the MCP session; add
   `provisional=published|own|all`; add compound provisional entity refs and
   index fields; apply visibility before ranking/inspection/rendering. Exercise
   a live throwaway dev daemon with base dirty state, a worktree modification,
   a tombstone, an attempted checkout spoof, and two peer variants of one id.
4. **3.5 promotion + retire redirects (landed).** Observe content equality at the
   pinned published commit, remove only matching provisional variants, delete
   `write_redirects` and its purge exclusion, and stop retaining worktree
   copies in central `kb.json`. Validate write, restart, merge, publisher pull,
   and index convergence on the throwaway daemon.
5. **3.6 registry lifecycle (landed).** Startup discovery and write-gate revalidation,
   periodic reconciliation, teardown deregistration, recovery of discoverable
   rows after registry loss, and dynamic watcher coverage rooted at
   `checkout_project_dir`. Complete the pure inventory runner, committed
   `.schema-epoch` markers, and local quarantine ledger.
6. **Mutation coverage + crash-consistent writes (landed).** Route all mutation verbs;
   in particular, a superseding `decide` applies the same checkout target to
   both the new and old entry. Add host-local staging + recoverable manifest +
   pending pointer for multi-file atomicity, with closeout proving same-commit
   membership.
7. **Merge gate.** `git merge-tree` candidate tree; `render --check`;
   contradiction lint as the scoped sub-project; the explicit promotion hook.
8. **Gap-store convergence.** Generalize the overlay to gaps, or the sequenced
   gap migration.
9. **Cut the path fallback** once the epoch marker plus empty local quarantine
   confirm coverage.

Each slice lands on the monolith and gets the full lane gate.

## 7. Migration and GC hazards

- **GC and branch durability (resolves the round-2 finding-2 contradiction).**
  A provisional overlay has no on-disk source once its checkout is torn down,
  so the daemon does NOT retain vanished content. Durability is the branch ref
  for committed changes and the live checkout for dirty changes, never a daemon
  copy. A torn-down checkout's overlay drops from the lane, and its committed
  knowledge re-enters either when the branch is checked out again (registry
  re-registers, overlay recomputes) or when it merges to the published ref
  (promotion). Forced removal of a dirty checkout can lose its uncommitted
  knowledge just as it can lose uncommitted code; teardown tooling should warn
  or refuse, but the daemon does not invent a second durability store. The
  consequence is explicit and accepted:
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

No open question blocks knowledge slices 3.3 through 3.7. Publisher ref,
provisional identity, visibility policy, failure behavior, and `built_from`
granularity are decided above. Remaining questions belong to later slices or a
later locality rung:

- **Contradiction-lint semantics.** How far past the §4.5 opposing-polarity
  first cut to push (scope overlap, entailment) before diminishing returns.
- **Gap convergence shape.** One generalized overlay vs a gap-specific
  migration reusing the registry, promotion, and staged-write path.
- **Portable publisher-ref policy.** Whether the remote rung requires a
  committed default-ref field so hosts converge without an operator-created
  host-local pin. The monolithic rung is fully specified by the pin above.
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
  rule anticipated, now unified as the pinned-published-layer-plus-overlay
  model.
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
