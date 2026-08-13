# Connector satellite invariants

- This binary is a **dumb producer** running on a producer host. It observes
  one remote scope through that store's native change signals, applies policy
  on enumeration metadata, exports provider-native documents, and uploads
  bounded bytes. Chunking, Tantivy, embeddings, vectors, edges, activation,
  and every other corpus behavior stay on the corpus host.
- **Export is not chunking.** The satellite asks a vendor to render a document
  and receives ordinary bytes in a format the registry already claims. Its
  entire knowledge of the corpus is a static extension map. Exactly one
  chunker version exists in the system, on the corpus host, so a satellite
  deploy can never skew against the index. The dependency ceiling is enforced
  by `scripts/acceptance-file-collector-deps.sh`; vendor SDKs are welcome
  there, corpus crates never are.
- **Read-only is structural, not a default.** The connector trait has no
  write, delete, or permission method. Adapters additionally request
  read-only vendor scopes and assert HTTP-method conformance in their own
  tests: the absent trait method stops callers from requesting mutation but
  proves nothing about an adapter's internals.
- **Two credential planes, no overlap.** Vendor credentials are producer-side
  and resolve through secret references, never values. Wire credentials are a
  file-sourced `ServiceToken` producer grant, sent only as a bearer header:
  never in env vars, query strings, JSON bodies, logs, metrics, or a
  `remote_url`. Non-loopback plaintext corpus URLs are refused and redirects
  stay disabled, so a credential cannot be forwarded to another authority.
- **A checkpoint set is NAMED and per-stream.** One expired Drive token or
  Graph delta link must invalidate one stream, not force a full
  re-enumeration of every other drive in the scope. Invalidation is a value
  (`Observation::CheckpointInvalidated`), not an error, so no catch-all
  handler can swallow it; it increments a DURABLE `cursor_epoch` and is
  reported with its cause and cost.
- **Freshness is content hashes, never the vendor's version string.**
  `remote_version` gates re-acquisition producer-side and rides the wire as
  metadata; it is never index freshness authority. `remote_watermark` is
  display and diagnostic only and is deliberately excluded from the
  generation id.
- **The journal is working state, not authority.** Losing it costs one full
  re-enumeration and re-export pass, never data: the remote store is the
  durable backlog and the satellite has no spool. Its published-implies-hash
  invariant is structural (an enum), not a checked convention.
- **Orphan detection requires a COMPLETE enumeration.** `ChangeBatch::complete`
  is the license. Treating a delta's seen-set as complete would delete every
  unchanged document from the corpus on the first incremental cycle.
- **Logical-path assignment runs over the union of journal and batch, and
  suffixes every member of a collision group.** First-claimant-wins would make
  the bare name migrate between documents across scans; assigning from the
  batch alone would un-suffix a path whose collision partner is only in the
  journal, silently shadowing a document. Collision keys are case-folded, and
  the faithful remote name is retained producer-side for status output.
- **Policy runs before any fetch, and every exclusion is counted.** A policy
  quietly dropping half a drive is indistinguishable from a broken connector.
  A per-file cap can only LOWER the corpus ceiling; the per-source total
  aborts the publication loudly naming the largest offenders rather than
  truncating.
- **The streamed export cap is the only cap that fires mid-transfer**, because
  export size cannot be bounded on enumeration metadata. Ordinary files are
  screened on their reported size and never fetched at all when oversized.
- Symlinks are never followed and special files are never read, in the fixture
  connector as much as anywhere else: a connector must not be a path by which
  the producer host's filesystem escapes its configured scope.
