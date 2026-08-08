# Typed Git-source store invariants

- The store owns resumable history upload sessions, immutable manifests, content-addressed canonical records, ready source generations, and generation lookup. It does not own HTTP, Git execution, catalog authority, index publication, or transport cutover.
- Every upload and generation lookup is producer- and repo-authority-bound by server-derived ids. Caller-supplied project ids never enter this store.
- Manifest completion is immutable. Replayed pages and generations succeed only when their exact bytes match; conflicts fail closed.
- Record installation verifies manifest membership, exact canonical byte length, SHA-256, and decode validity before reuse. Finalize streams records through the complete-graph verifier before publishing `ready`.
- All directory paths are opened component-by-component without following symlinks. Durable files use fsynced atomic replacement; every mutation holds the in-process mutex and the canonical store lock in that order.
- A `ready` generation is retained until later activation/GC authority explicitly proves it unreferenced. Intake never guesses that an older complete snapshot is disposable.
