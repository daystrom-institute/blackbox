# Knowledge source contract invariants

- This crate is the dependency-clean wire, validation, and canonical-hashing leaf for committed knowledge publication candidates and provisional workspace source snapshots. It never opens a filesystem, invokes Git, serves HTTP, writes a store, accepts publication, computes a corpus view, or links daemon/runtime crates.
- A workspace id is the existing reuse-safe checkout marker carried as `bro_core::WorkspaceId`. Paths, cwd values, task ids, and producer ids never substitute for it.
- Publication candidates contain committed source facts only. A producer can submit a candidate but cannot move the accepted pointer.
- Provisional descriptors contain source facts and a complete bounded ancestry witness. They never carry caller-computed overlay values, tombstones, project ids, accepted records, or corpus entities.
- Knowledge and gap manifests use exact repository-relative paths below the published scope's `.bbox/knowledge/` and `.bbox/gaps/` directories. Filenames, counts, bytes, ordering, digests, and limits fail closed.
- Canonical ids use explicit versioned length-prefix encodings. Never replace them with JSON serialization hashes.
