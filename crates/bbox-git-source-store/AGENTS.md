# Typed Git-source store invariants

- The store owns resumable history upload sessions, immutable manifests, content-addressed canonical records, ready source generations, and generation lookup. It does not own HTTP, Git execution, catalog authority, index publication, or history ownership.
- Top-level trees outside that layout are not store members; `open` removes them.
- Every upload and generation lookup is producer- and repo-authority-bound by server-derived ids. Caller-supplied project ids never enter this store.
- Manifest completion is immutable. Replayed pages and generations succeed only when their exact bytes match; conflicts fail closed.
- Record installation verifies manifest membership, exact canonical byte length, SHA-256, and decode validity before reuse. Finalize streams records through the complete-graph verifier before publishing `ready`.
- All directory paths are opened component-by-component without following symlinks. Durable files use fsynced atomic replacement; every mutation holds the in-process mutex and the canonical store lock in that order.
- Background maintenance is never part of daemon startup or an upload request. It expires idle upload sessions, retains the current ready generation plus the configured number of prior generations, treats caller-supplied materializer generation ids as additional GC roots, and reclaims CAS records only after every surviving upload/generation manifest drops the hash and the grace interval passes.
- A `ready` generation is retained until maintenance has complete root evidence. Intake never guesses that an older complete snapshot is disposable, and GC refuses malformed pointers, metadata, symlinks, or unexpected directory members instead of widening deletion.
- Repo activation journals are checksummed monotonic lower bounds. Their
  immutable plan cannot drift after `Prepared`; external publication is
  re-probed rather than inferred from the stage label. Journal source ids are
  automatic GC roots, and per-project file commitments are durable snapshot
  receipt SHA-256 values, never disposable transaction tokens.
- History finalize is idempotent per generation id. Existing immutable
  evidence with the same identity, descriptor, and exact manifest is reused
  with its original creation time and lifecycle; mismatches fail closed.
  Each accepted upload attempt takes a durable per-repository acceptance
  sequence, checkpointed in its upload record before `current-ready.json`
  moves. While a pointer exists, only a newer sequence repoints the
  repository and reopens a `Superseded`/`Failed` source, a completed upload
  replay is a no-op, and an equal sequence must name the same upload and
  source. The reopen is durable before the pointer names it, so a probe
  never reports a terminal source as current.
- Operator retirement removes the pointer and with it the baseline
  acceptances are ordered against. A completed upload replay stays a no-op.
  The next finalize that reaches the pointer step publishes it again and
  reopens its source, whatever its sequence: a fresh upload, or an
  interrupted upload resumed with a checkpoint older than the retired
  pointer's. Retirement does not touch the counter, so sequences stay
  unique. Every read of `current-ready.json`, including the pointer listing
  and retirement, goes through the one validating loader. A malformed
  pointer is never treated as current and never removed: acceptance and
  retirement refuse it, retirement naming the repository, and the listing
  reports it as malformed for its repository while still listing the
  others. No tool clears one: that is a manual edit of the store with the
  daemon stopped.
- Upload records and history pointers without acceptance fields are legacy
  state, never reinterpreted: a legacy pointer is a baseline below every new
  acceptance and a completed legacy upload never gains one. Allocation
  recovers its high-water mark from the counter, the pointer, and retained
  upload checkpoints; it never moves backwards.
- `current-ready.json` arbitrates competing accepted sources per repository.
  Activation checks it before starting and again at every bounded post-build
  recheck, so an older queued or long-running source cannot commit after a
  newer accepted source becomes authoritative. The previously selected Active
  source remains last-good until the newer activation itself commits.
