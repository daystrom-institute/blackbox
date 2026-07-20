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
brief: "Retire write_redirects, the host-local map that makes kb.json required to interpret the repo's own committed .bbox/ files. Two moves in one push: (1) a durable published-scope identity key (repo_id, bbox_root_relpath) replacing the absolute-path-string scope key, and (2) a self-describing provisional knowledge lane for in-flight worktree entries that rebuilds from the worktree files instead of from an opaque host-only redirect map. Hybrid visibility (own checkout always, peers behind a flag), passive merge-observation promotion, and a render --check merge gate ride along. This is slices 1 + 3 of locality-first-decomposition.md, and it finishes the identity model repo-owned-project-state.md left unbuilt."
---

# Checkout identity and the provisional knowledge lane

> **Status: proposed.** Nothing here is landed. Anchors verified against
> `beta/blackbox-v2` after the fleetd extraction; line cites rot, so grep the
> named symbols before building. This is the concrete build plan for slices 1
> and 3 of
> [locality-first-decomposition.md](../../daemon-runtime/locality-first-decomposition.md),
> and the finish of the identity model
> [repo-owned-project-state.md](repo-owned-project-state.md) specified but did
> not ship.

## 0. Decision and scope

Attack the knowledge/code seam as one push, foundation first:

1. **Identity contract.** Give project-scoped durable knowledge a key that
   travels: `(repo_id, bbox_root_relpath)` instead of the absolute path
   string it keys on today. Mint a `checkout_id` for the provisional lane
   from the `CheckoutContext` the resolver already produces. Stamp knowledge
   and indexed-lane responses with a generation.
2. **The provisional lane.** Replace `write_redirects` (the host-local map
   that today decides which worktree a repo-owned entry file lands in) with a
   provisional knowledge lane whose source of truth is the worktree's own
   `.bbox/knowledge/` files, so it rebuilds from disk instead of from
   `kb.json`. Hybrid visibility, passive merge-observation promotion, and a
   `bbox_lint` / `render --check` merge gate ride with it.

Out of scope, deliberately, and left to their own triggers: the harness-ward
moves (provenance, blame, render relocation), the code-corpus collector, and
the corpus off-host move. None has a live forcing function; this seam does.

## 1. Present state (verified)

Two identity functions, opposite behavior
(`crates/bbox-corpus-core/src/entity_ref.rs`):

- `project_id_for_path` (:499) is a host realpath hash. A worktree hashes
  differently from its base, and the id does not survive a different `$HOME`
  or machine.
- `repo_id_for_root` (:511) is a hash of the first commit. It is identical
  across every worktree of a repo and travels across hosts; it also conflates
  a fork with its upstream (shared history).

The resolver already carries most of the shape this design needs
(`crates/bbox-corpus-core/src/project_record.rs`):

- `ProjectContext` (:41) holds `project_id`, `repo_id: Option<String>`,
  `aliases`, `host_root`, and `checkout: Option<CheckoutContext>`.
- `CheckoutContext` (:63) holds `checkout_dir` and a `managed` flag, and its
  doc comment states the exact seam: `checkout_dir` "doubles as the checkout
  identity until a consumer needs a minted `checkout_id`." The provisional
  lane is that consumer.
- The read/write asymmetry is load-bearing and must be preserved:
  `resolve_base_project_for_scope` (:98) is the broad READ gate (any worktree
  of a registered repo aliases to base, harmless for "which corpus do I
  query"); `resolve_managed_fleet_worktree` + `ResolveIntent::{Read,Write}`
  (`crates/bbox-indexing/src/projects.rs:466`, :588) is the conservative
  WRITE gate (only managed worktrees alias; an arbitrary user worktree never
  receives write-side aliasing).

Durable scope keys on an absolute path string, everywhere
(`crates/bbox-knowledge/src/knowledge.rs`): every entry type carries
`project: Option<String>` (:43 and siblings), matched by path equality.

The bridge between base-scope and branch-travel is `write_redirects`
(`knowledge.rs:952`): a `HashMap<entry_id, worktree_checkout_dir>` held in the
CENTRAL store. `repo_owned_carrier` (:1055) and `set_write_redirect` (:1066)
use it to land a repo-owned entry FILE in the writer's worktree while the
entry's durable `project` scope stays base. It is dropped per id when a
base-root committed file is observed at load (:1167, "the merge landed").

## 2. The two defects this closes

**A. `kb.json` is required to interpret the repo's own committed files.**
`write_redirects` lives only in the host central store and has no on-disk
source. Lose `kb.json` and you lose the map that says which worktree each
in-flight repo-owned entry belongs to. That directly contradicts the
repo-owned inversion's core promise: "losing the daemon store stops being
catastrophic; the project layer rebuilds from the repos"
([repo-owned-project-state.md](repo-owned-project-state.md)). It is a
host-local authority the design said should not exist.

**B. In-flight worktree knowledge is invisible and unlabeled.** Only
registered base roots are loaded and watched, so a peer agent in another
worktree cannot see an entry a sibling wrote on its branch, and the writing
agent sees its own write only via the central in-memory copy, never labeled
as unmerged. Measured live during the seam survey: 17 worktrees, up to 10
divergent gap files and 5 divergent knowledge files against base. That
divergence is correct branch state; the machinery's silence about it is not.

Both are the **same identity-by-absolute-path failure mode that sank the
satellite arc** (gap-cbcc314d: sixteen dead registry entries kept alive as a
path-selector bridge, "load-bearing, not corpses"). A milder manifestation on
one host, but the identical root, and the reason "identity before motion" is
the load-bearing rule.

## 3. The identity contract (slice 1)

### 3.1 Durable published-scope key

Published project-scope knowledge keys on `(repo_id, bbox_root_relpath)`:

- `repo_id` is the existing first-commit hash. `bbox_root_relpath` is the
  `.bbox/` directory's path relative to the repo root, which makes
  **monorepos first-class** (each sub-project's `.bbox/` is a distinct scope)
  and decouples identity from `$HOME` and checkout location.
- `repo_id` is a repo-FAMILY key, not a complete one, so it carries the three
  handlers repo-owned-project-state.md already specified: an `aka_repo_ids`
  remap in `.bbox/config.toml` for history rewrites, a `project_key_override`
  for a fork that genuinely wants knowledge separate from upstream, and a
  recorded `repo_id` fallback for shallow clones with no first commit to hash.
- The host-local layer (the provisional lane, the cache, activity stores) may
  keep using `project_id`/path: it never travels, so its identity choice is
  unconstrained.

### 3.2 Checkout identity

Mint `checkout_id` from `CheckoutContext`, as its comment anticipates. The
provisional lane needs to answer two questions about an in-flight entry:
which checkout produced it (so it vanishes when that checkout is removed) and
whether it has merged to base (so it promotes). A host-local `checkout_id`
derived from the worktree's gitdir identity is sufficient, because provisional
entries are host-local ephemera by nature; the durable thing that must travel
is the published scope key, not the lane. `checkout_dir` remains the fallback
until a consumer needs the minted id; the provisional lane is that consumer.

### 3.3 Generation stamps

Knowledge and indexed-lane responses carry the commit/generation they were
built from, so a consumer can tell a published-truth hit from a provisional
one and (later, when the corpus moves) which snapshot an `indexed_hints`
answer hints about. Additive; existing consumers ignore the field.

### 3.4 Two known consumers, not a speculative contract

This is not architecture in the abstract. Two consumers define the shape
today: knowledge scope keying (§3.1) and the index's doc stamping, which
already routes through `resolve_base_project_for_scope`. Scope the contract to
those two; do not design for remote workers that have no trigger yet.

### 3.5 Migration: dual-read, then cut over

Add the identity key alongside the path string; resolve by identity first and
fall back to path (dual-read) so existing central entries keep resolving
through the transition. `bbox_project_eject` and the load path stamp the
identity key onto entries as they are read. Cut the path fallback only after
a generation stamp confirms every live entry carries the key. No flag day.

## 4. The provisional knowledge lane (slice 3)

Replace `write_redirects` with a lane whose source of truth is on disk:

1. **Published knowledge truth** = base-branch `.bbox/` (committed) plus the
   central store's global lane. This is what the corpus indexes as
   authoritative and what `verified` can mean.
2. **Provisional truth** = a managed checkout's own `.bbox/knowledge/` files,
   at its branch, indexed as provisional and stamped with `checkout_id` and
   generation. The recognition set is exactly today's write-aliasing set
   (in-tree linked worktrees, cockpit-managed roots, marker+repo_id lanes) so
   no new gate is introduced. **This is the self-healing win:** the lane
   rebuilds by rescanning live managed checkouts' `.bbox/knowledge/`, so
   losing `kb.json` costs a rescan, not the map. Defect A is closed by
   construction.
3. **Visibility (decided): hybrid.** A caller always sees published entries
   plus its own checkout's provisional entries (read-your-writes preserved;
   the resolver already knows the caller's checkout); other checkouts'
   provisional entries appear only behind an explicit query flag. Closes
   defect B without letting ten branches of unmerged claims read as settled.
4. **Promotion (decided): passive merge-observation first.** When the entry
   file appears at a base root, the provisional entry promotes to published
   and the checkout-stamped copy is dropped: today's redirect-drop trigger
   (`knowledge.rs:1167`) generalized, and the correctness backstop that
   catches a merge however it happened. An explicit closeout hook that
   announces "branch merged, promote now" and hosts the merge gates arrives
   with §4.5; until then a merged-but-unpulled base leaves entries
   provisional and self-heals on pull.
5. **Merge gate.** One-file-per-entry makes textual conflicts rare and
   semantic conflicts silent, so `bbox_lint` at the merge/closeout boundary
   is required once branches carry knowledge deltas. `render --check` rides
   the same gate: today renders race across worktrees with nothing detecting
   a stale committed `CLAUDE.md`/`AGENTS.md`, and the check closes that for
   near-zero extra cost.

The branch-private alternative (no provisional lane; in-flight entries simply
invisible outside their worktree until merge) stays the fallback: it is the
purest reading of "the corpus indexes published truth" and strictly less
machinery, but it regresses today's cross-fleet read-your-writes. If the lane
proves noisy, demoting to branch-private is a deletion, not a redesign.

## 5. What retires

- `write_redirects`, its purge-exclusion special case, and `repo_owned_carrier`'s
  redirect branch.
- The "host store required to interpret repo files" property.
- The absolute-path-string scope key as the durable authority (kept only as a
  dual-read fallback through migration, then dropped).

## 6. Sequencing

1. **Identity contract, additive.** `(repo_id, bbox_root_relpath)` key and
   `checkout_id` in `bro-core`/`bbox-corpus-core`; generation stamps on
   knowledge + indexed responses; dual-read resolution. Behavior-preserving.
2. **Provisional lane.** Index managed-checkout `.bbox/knowledge/` as
   provisional (self-healing rescan); hybrid visibility; passive promotion;
   retire `write_redirects`.
3. **Merge gate.** `bbox_lint` + `render --check` at the closeout/merge
   boundary; the explicit promotion hook.
4. **Cut the path fallback** once generation stamps confirm full key coverage.

Each slice is independently valuable and lands on the monolith; each gets the
full lane gate (workspace nextest full profile, clippy, concurrency lint).

## 7. Migration hazards

- **Dual-authority window.** During dual-read, an entry could resolve by path
  and by key to different scopes if a repo moved. Resolve key-first and treat
  a path-only match as provisional until stamped, so a stale path never wins
  over a durable key.
- **`repo_id` remap.** A history rewrite changes `repo_id`; the `aka_repo_ids`
  list in `.bbox/config.toml` must be consulted at resolution or a rewrite
  orphans a repo's knowledge. Fork/upstream conflation is handled by
  `project_key_override`, opt-in.
- **Preserve the read/write asymmetry.** The broad read gate and conservative
  write gate must stay distinct (the CLAUDE.md invariant in
  `bbox-corpus-core`); the provisional lane keys on the WRITE recognition set,
  never the broad read set, or an arbitrary user worktree's scratch entries
  leak into the lane.
- **Provisional GC.** A checkout removed without merging must drop its
  provisional entries (no base fallback, since the daemon never writes the base on
  a branch's behalf, matching `repo_owned_carrier`'s current `None`).

## 8. Open questions

- **`checkout_id` derivation.** Worktree gitdir-path hash vs branch +
  common-dir vs a minted-and-recorded id. Gitdir identity is stable across a
  branch rename; a recorded id survives a gitdir move. Pick by which failure
  is likelier in the fleet.
- **Provisional query UX.** Default-include-own-checkout is decided; the flag
  name and whether an orchestrator reviewing a campaign gets a "show all
  provisional across the fleet" mode is open.
- **Lint at merge vs at closeout.** `bbox_lint` as a git merge driver, a CI
  gate, or a `/closeout` step. The closeout hook (§4.4) is the natural host,
  but a repo without closeout wants the CI path.
- **Generation stamp granularity.** Per-entry vs per-store-snapshot; the
  cheaper per-snapshot stamp is likely enough for the two current consumers.

## 9. Relationship

- **Implements** slices 1 and 3 of
  [locality-first-decomposition.md](../../daemon-runtime/locality-first-decomposition.md);
  its §4 visibility and promotion decisions are adopted here verbatim.
- **Finishes** [repo-owned-project-state.md](repo-owned-project-state.md): the
  `(repo_id, bbox_root_relpath)` identity model it specified as a requirement
  and left unbuilt, and the provisional-vs-published split its "uncommitted
  entries are provisional" rule anticipated.
- **Carries forward** the fleetd-era lesson: identity-by-path is what fractured
  the satellite arc across hosts (gap-cbcc314d) and what makes today's
  `write_redirects` fragile; landing durable identity now, on one machine,
  keeps the remote-worker rung
  ([remote-worker-boundary.md](../../bro-harness/remote-worker-boundary.md))
  operational work instead of architecture.
- **Preserves** the read/write gate asymmetry owned by
  `bbox-corpus-core`/`bbox-indexing`; this design consumes it, never
  re-derives it.
