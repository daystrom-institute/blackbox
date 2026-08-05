# bbox-edge-sidecar — the JSONL edge lanes on disk

Store-agnostic persistence floor for the edge corpus: the workspace and
materialization manifest, snapshot dir layout, and the append/replace/merge/
purge/compact primitives over the observed / explicit / managed-derived lanes.
The store to edge emitters live in the root crate; this crate never sees store
types.

## Scale is the design constraint

Edge lanes are the only durable owner whose size is unbounded by design. A
working host carries several GiB of lanes, individual lanes above 1 GiB, and
millions of rows carrying a literal `cwd`. Every path that reads or writes a
lane must therefore be O(chunk + line) in memory, and anything that holds a
per-row structure will fall over on a real host while passing every fixture.

- Lanes are STREAMED, never read whole: capture, stamp, and verify all read
  through one descriptor with an incremental digest. See the two-lane note in
  `bbox-corpus-core/AGENTS.md` for why the buffered owner-snapshot lane is
  wrong here and must not be "fixed" by raising its budget.
- The lane row walk carries O(1) state. Row position discriminates duplicates,
  NOT a counter over same-content rows: a same-content counter needs a live map
  of every identity in the lane, which is millions of entries on a real host.
- One open descriptor is one coherent version of a lane. A concurrent atomic
  replacement cannot tear a read that is already in progress, which is why the
  read halves do not need the double-scan the buffered tree capture uses.

## The project-catalog backfill obligation is SELECTOR-keyed

A handful of hundred distinct `cwd` values cover those millions of rows, and
deepest-root classification, planning, and stamping all key on the selector.
So this owner contributes ONE observation per (lane, selector) to the
legacy-path-observations lane, carrying a member count and an ordered
commitment rather than the member rows. A per-row ledger cannot fit the
canonical inventory at all.

Consequences a future change must preserve:

- An obligation never spans lanes, so applying one is exactly ONE atomic
  whole-file replacement. It cannot be half applied, which is what makes a
  crashed backfill safe to repeat.
- Stamping resolves an obligation by RE-WALKING the named lane and stamping
  every row whose selector digest matches, never by looking up a row id. Only
  the lane whose subsource prefixes the id is opened.
- Verify answers per group: stamped only when EVERY member carries the same
  project id. A partially stamped group must not read as stamped, or a torn
  apply would verify as complete.
- The stamp temporary is deliberately not a `.jsonl` file. A crash between its
  creation and its rename leaves a half-written copy of a lane on disk, and
  capture must not read it as a lane.
