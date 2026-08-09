# Typed Git-source store invariants

- The store owns resumable history upload sessions, immutable manifests, content-addressed canonical records, ready source generations, and generation lookup. It does not own HTTP, Git execution, catalog authority, index publication, or transport cutover.
- The same store root owns the last authenticated provenance export receipt per
  catalog project plus resumable provenance-import uploads, document CAS,
  immutable generations, and checksummed per-project publication journals.
  Receipt replacement is atomic, exact retries are no-ops, and server-side
  catalog authorization happens before this store is called.
- Every upload and generation lookup is producer- and repo-authority-bound by server-derived ids. Caller-supplied project ids never enter this store.
- Manifest completion is immutable. Replayed pages and generations succeed only when their exact bytes match; conflicts fail closed.
- Record installation verifies manifest membership, exact canonical byte length, SHA-256, and decode validity before reuse. Finalize streams records through the complete-graph verifier before publishing `ready`.
- All directory paths are opened component-by-component without following symlinks. Durable files use fsynced atomic replacement; every mutation holds the in-process mutex and the canonical store lock in that order.
- Background maintenance is never part of daemon startup or an upload request. It expires idle upload sessions, retains the current ready generation plus the configured number of prior generations, treats caller-supplied materializer generation ids as additional GC roots, and reclaims CAS records only after every surviving upload/generation manifest drops the hash and the grace interval passes.
- Provenance document CAS follows the same evidence rule: open imports and
  immutable generation manifests root hashes; expired uploads are removed
  before grace-delayed orphan reclamation. A partial upload can never become a
  durable storage leak merely because finalization did not run.
- A `ready` generation is retained until maintenance has complete root evidence. Intake never guesses that an older complete snapshot is disposable, and GC refuses malformed pointers, metadata, symlinks, or unexpected directory members instead of widening deletion.
- Repo activation journals are checksummed monotonic lower bounds. Their
  immutable plan cannot drift after `Prepared`; external publication is
  re-probed rather than inferred from the stage label. Journal source ids are
  automatic GC roots, and per-project file commitments are durable snapshot
  receipt SHA-256 values, never disposable transaction tokens.
- `current-ready.json` arbitrates competing accepted sources per repository.
  Activation checks it before starting and again at every bounded post-build
  recheck, so an older queued or long-running source cannot commit after a
  newer accepted source becomes authoritative. The previously selected Active
  source remains last-good until the newer activation itself commits.
- Provenance publication journals are monotonic
  `Prepared -> EdgesPublished -> Committed`, with `Superseded` and
  `Quarantined` terminal for autonomous background redrive. Re-finalizing the
  exact immutable upload is an authenticated explicit retry: it reopens only
  that source and activation replaces the terminal journal with a plan pinned
  to the current catalog and code selector.
  They bind the immutable source evidence and exact pinned code selector;
  `EdgesPublished` also binds the ordered edge-key commitment. Retries re-prove
  source bytes, rebuild only through the bounded background lane, and verify
  those keys in the published read view rather than treating a stage label or
  rebuild nudge as proof of graph visibility.
- A per-project provenance ready pointer advances by a durable acceptance
  sequence, never generation-id ordering. Publication settles against that
  pointer atomically, so older queued work may finish an idempotent append but
  cannot become Active after a newer note snapshot is accepted. Maintenance
  retains the pointer target, configured prior generations, unfinished
  journals, last-good Active state, and quarantined evidence before sweeping
  document CAS.
