# Typed Git-history source contract invariants

- This crate is the dependency-clean wire and validation leaf for checkout-produced Git history snapshots. It never opens a filesystem, invokes Git, serves HTTP, writes an index, or links daemon/runtime crates.
- Wire callers supply only a `PublishedScope`. Project ids, repo-history ids, commit namespaces, generation ids, and authority assignments are server-derived.
- History uploads are complete logical snapshots. Content-addressed record reuse is a transfer optimization, never cursor/delta authority.
- History facts are typed commit records, not Git packs or object databases. Object format, graph closure, HEAD reachability, paths, fragments, counts, bytes, hashes, and commitments all fail closed here.
- Canonical hashes use explicit versioned length-prefix encodings. Never replace them with JSON serialization hashes.
