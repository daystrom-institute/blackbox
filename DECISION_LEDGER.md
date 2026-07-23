# Monolith Decomposition Decision Ledger

This ledger records autonomous decisions made while executing the durable
project-catalog decomposition. It is not a release log. Entries exist only for
material forks where repository evidence made one choice clearly better for
correctness and completeness. A decision that survives review remains binding
until a later entry explicitly supersedes it.

## D-001: Keep offline administration out of `blackboxd`

- Date: 2026-07-22
- Phase: durable project catalog, Phase 1
- Status: accepted after independent plan review
- Decision: Add a separate `blackbox` executable in the root package for
  `project-catalog migrate` and later offline catalog administration. Keep
  `blackboxd` as the zero-argument foreground daemon.
- Evidence:
  - `src/main.rs` deliberately accepts only `--help` and `--version`; unknown
    arguments fail instead of accidentally starting another daemon.
  - The root package already owns `clap`, the `blackbox` library target, and the
    daemon wiring needed by a thin administration binary.
  - No `blackbox` executable currently exists, while the governing design names
    `blackbox project-catalog ...` as the offline surface.
- Rationale: Adding migration modes to `blackboxd` would mix offline exclusive
  state mutation with daemon startup and weaken a side-effect-free command-line
  contract. A separate executable matches the designed command, preserves the
  daemon safety boundary, and avoids introducing another package solely for a
  thin adapter.
- Revisit only if: the root package is split before Phase 1 implementation, or
  a reviewed repository-wide administration CLI replaces the designed
  `blackbox` surface.

## D-002: Do not activate v2 state before the complete v2 runtime can preserve parity

- Date: 2026-07-22
- Phase: durable project catalog, Phase 1 to Phase 6 boundary
- Status: accepted after independent plan review
- Decision: Phase 1 implements and fault-tests the complete v1-to-v2 migration
  engine, but its CLI may install post-images only into an explicitly selected
  isolated rehearsal root. Applying to the configured live project store stays
  fail-closed until the Phase 6 cut, after the catalog resolver, path-free
  index, collector state machine, accepted-publication store, publisher
  migration, remaining adapters, and v2 daemon startup path have all landed.
- Evidence:
  - The current daemon opens `projects.json` through the version-1
    `ProjectRegistry` and persists it asynchronously through
    `StorePersister`.
  - Phase 1 creates the pure model, strict stores, transaction owner, migration
    engine, and rehearsal participants. Phases 2 through 5 create the runtime
    consumers required to preserve existing behavior.
  - The governing Phase 6 is already the overlap-proof and cut phase.
  - Installing v2 bytes at the configured path before those consumers exist
    would either strand the bridge daemon or activate a partial runtime.
- Rationale: A migration command that can strand the only runnable daemon is
  incomplete. A dual-format daemon would weaken the explicit migration cut and
  make rollback authority ambiguous. Isolated apply rehearsal proves the
  Phase 1 engine without creating an operator-state trap. Phase 6 removes the
  guard only after exact-state rehearsal and live runtime parity are
  demonstrated. The configured service remains on the last deployed
  bridge-compatible binary through Phases 2 to 5.
- Revisit only if: an earlier phase is deliberately expanded to include every
  remaining runtime consumer and the complete reviewed cut.

## D-003: Preserve local Git history without inventing repository authority

- Date: 2026-07-22
- Phase: durable project catalog, identity and Git overlay
- Status: accepted after independent plan review
- Decision: A newly created `LegacyLocal` project receives a server-minted,
  project-bound local history record and commit namespace. It may ingest and
  query attachment-backed local Git history, but that namespace cannot
  authorize publishing, producer grants, cross-host repository identity, or
  another catalog project. Promotion records proved repository authority while
  preserving the materialized local namespace as compatibility history.
- Evidence:
  - The current runtime indexes Git history for registered Git projects even
    when their repository id is only a computed bootstrap hint.
  - Treating that computed hint as v2 authority would preserve behavior by
    violating the new authority model.
  - Creating no history record would remove an existing user capability and
    violate the final parity requirement.
- Rationale: A random, catalog-owned local namespace preserves history without
  laundering a path hash or computed repository hint into cross-host
  authority. Explicit promotion remains the only way to acquire published
  repository authority.
- Revisit only if: local Git history is removed as an explicitly approved
  product change after the decomposition parity gate.

## D-004: Split catalog administration by proof, not by a claimed MCP identity

- Date: 2026-07-22
- Phase: durable project catalog, administration
- Status: accepted after independent plan review
- Decision: Attachment-backed operations with live repository proof may remain
  model-facing MCP tools when they require the expected catalog epoch, explicit
  acknowledgement for authority changes, and an audit reason. Agents may pass
  operator-supplied acknowledgement values but may not default or infer them.
  Proofless authority operations, including unattached import or scope
  migration, conflict-resolution apply, and whole-store migration apply, are
  local `blackbox project-catalog` CLI operations. Read-only inspection stays
  available on both surfaces.
- Evidence:
  - The current MCP transport has tool-surface filtering but no authenticated
    human-operator identity distinct from a model-facing client.
  - The governing security model already excludes unattached scope migration
    from model-facing routes.
  - Existing project mutations are model-facing MCP tools and the repository's
    operator-authority convention permits agents to pass through, but never
    invent, explicit operator acknowledgements.
  - The governing design already places proofless unattached scope migration
    outside model-facing routes.
  - The new local CLI provides the required boundary for operations that
    cannot derive authority from a validated attachment.
- Rationale: The transport does not prove a human identity, so the design must
  not claim that it does. Repository proof plus compare-and-swap and explicit
  authority acknowledgement preserves the established delegated MCP workflow.
  Operations without that proof remain local rather than trusting a request
  boolean as identity.
- Revisit only if: a reviewed authenticated operator capability is added to
  the MCP transport and its audit model distinguishes operator delegation from
  agent discretion.

## D-005: Repository aliases nominate; the catalog accepts

- Date: 2026-07-22
- Phase: durable project catalog, alias migration
- Status: accepted after independent plan review
- Decision: Existing materialized aliases migrate as accepted catalog aliases
  so selectors do not regress. Later committed `.bbox/config.toml` alias
  changes create bounded nominations only. Acceptance or rejection is an
  explicit local catalog-authority action.
- Evidence:
  - The bridge daemon currently rewrites central alias state from committed
    config during startup.
  - Repository content is portable producer input, while alias uniqueness and
    selector ownership span the host catalog.
  - Allowing startup sync to accept aliases would let one checkout rewrite
    host-wide selector authority without a catalog transaction.
- Rationale: Migration preserves every active alias. New alias capability
  remains available through an explicit action, but repository content cannot
  self-elect into host-wide selector authority.
- Revisit only if: alias authority moves into a separately authenticated,
  distributed control plane.

## D-006: Migration rehearsal changes destination, not transaction semantics

- Date: 2026-07-22
- Phase: durable project catalog, Phase 1 migration
- Status: accepted after independent plan review
- Decision: Phase 1 uses one generalized migration transaction owner for the
  catalog, attachments, effective source-manifest quarantine, accepted
  publication pointers, migration marker, and their immutable assets. Rehearsal
  redirects every participant to isolated copies, but runs the same prepare,
  install, verify, recovery, and rollback protocol used by the Phase 6 cut.
- Evidence:
  - A duplicate-scope loser must leave effective collected selection before v2
    binds.
  - Existing publisher pins must have verified accepted publication generation
    G1 before the catalog epoch becomes visible.
  - Deferring either transition would make Phase 6 use a path the rehearsal and
    fault matrix never exercised.
- Rationale: A catalog-only rehearsal would prove the least risky files while
  leaving the actual cross-store cut untested. One role-bounded participant
  plan gives every mutable post-image one commit decision and lets immutable
  assets remain unreachable on rollback.
- Revisit only if: the migration is redesigned around an equally strict
  transactional substrate with complete end-to-end rehearsal and recovery.

## D-007: Accepted publication does not fabricate Git ancestry

- Date: 2026-07-22
- Phase: durable project catalog, provisional overlays
- Status: accepted after independent plan review
- Decision: A verified accepted publication generation remains published truth
  after publisher detach and supplies commit P plus canonical knowledge and gap
  file manifests. It does not supply a merge base or Git objects. Each
  attachment may compute its overlay only when its own object database proves P
  and the merge base. `own` fails explicitly when that proof is unavailable;
  `all` omits only unavailable peers with diagnostics; `published` remains
  available.
- Evidence:
  - The overlay algorithm needs both commit ancestry and the committed file map
    at the merge base.
  - Accepted publication bytes can preserve published content but cannot prove
    Git ancestry.
  - Requiring a live publisher for all reads would discard the remote-only
    capability the catalog is intended to provide.
- Rationale: This separates durable published truth from checkout-specific Git
  evidence without silently borrowing another attachment or treating content
  hashes as ancestry.
- Revisit only if: accepted publication generations later carry a separately
  verified immutable Git object bundle with explicit ancestry semantics.

## D-008: Normalize markerless legacy checkouts during explicit migration

- Date: 2026-07-22
- Phase: durable project catalog, attachment migration
- Status: accepted after independent plan review
- Decision: Read-only preflight inventories checkout marker state and persists
  one planned strong-random checkout id per eligible canonical checkout root.
  Apply journals and installs each planned missing marker idempotently before
  admitting attachments. A matching marker resumes; a different, malformed,
  unreadable, or symlinked marker refuses. Rollback never deletes a successfully
  installed marker.
- Evidence:
  - The bridge currently synthesizes a path-derived id for markerless reads and
    mints the durable random marker on first write.
  - A synthetic id cannot become authoritative v2 attachment identity without
    reintroducing path-reuse bugs.
  - Excluding markerless roots would remove current local capabilities.
- Rationale: Explicit migration is the safe normalization point. Persisting the
  planned random value makes crashes and retries deterministic, while leaving a
  successfully installed host-local marker after rollback is benign and
  compatible with the bridge.
- Revisit only if: checkout identity moves to a stronger host-local authority
  that preserves the same reuse and recovery properties.

## D-009: Split scope-migration audit from path-bearing compatibility state

- Date: 2026-07-22
- Phase: durable project catalog, administration and compatibility
- Status: accepted after independent plan review
- Decision: Path-free `ScopeMigrationRecord` values live inside
  `CatalogSnapshotV2`. Path-bearing `LegacyPathLedgerEntry` values live inside
  the strict host-local `AttachmentSnapshotV1`, along with any
  attachment-specific migration proof. Catalog records never contain a
  host-local attachment id. A regular pair transaction changes both snapshots
  atomically.
- Evidence:
  - Scope migration requires a durable logical record to authorize temporary
    activation and publication bridges.
  - Historical path bindings contain absolute host paths and therefore cannot
    enter the catalog.
  - A sidecar written after the pair would leave crash windows where the new
    scope and its compatibility proof disagree.
- Rationale: The split follows the catalog/attachment trust boundary while
  retaining one atomic commit decision.
- Revisit only if: both record families move to another path-free/path-bearing
  paired substrate with equivalent crash recovery.

## D-010: Rewrite collected scope metadata during migration

- Date: 2026-07-22
- Phase: durable project catalog, collected-source migration
- Status: accepted after independent plan review
- Decision: Migration writes strict scope-bearing v2 activation and retained
  generation metadata for every surviving collected generation. Scope comes
  only from exact agreement among the immutable descriptor, manifest, legacy
  activation/selector, and migrated published catalog project. An ambiguous
  join refuses.
- Evidence:
  - Current `ActivationRecord` has no scope field.
  - The v2 startup contract requires exact scope agreement before selecting a
    generation.
  - Deferring the rewrite would make the first remote-only v2 boot reject its
    active generation or guess from project id.
- Rationale: The immutable descriptor already records producer-authorized
  scope. Transactional rewrite makes that evidence explicit before the strict
  runtime opens it.
- Revisit only if: a stronger immutable source-authority record replaces the
  descriptor and participates in the same migration proof.

## D-011: Catalog origin makes migration-marker loss detectable

- Date: 2026-07-22
- Phase: durable project catalog, recovery
- Status: accepted after independent plan review
- Decision: Every v2 catalog records whether it was initialized fresh or
  migrated from v1. A migrated origin carries its transaction id and requires
  a committed marker with the same transaction id at strict open. The
  complete plan hash remains in the marker and journal to avoid a hash cycle
  through the catalog post-image. A fresh-v2 origin does not require a marker.
- Evidence:
  - Valid v2 bytes alone cannot distinguish a fresh store from a migrated store
    whose marker was deleted.
  - Migration backups, G1 assets, and quarantine pins depend on the marker
    through final parity and rollback closeout.
- Rationale: Self-identifying origin turns marker retention from a convention
  into an enforceable invariant without burdening genuine fresh stores.
- Revisit only if: migration provenance becomes intrinsic to a replacement
  snapshot schema with the same strict-open property.

## D-012: Promotion is a typed scope transition, not an orphan audit

- Date: 2026-07-22
- Phase: durable project catalog, administration
- Status: accepted after independent plan review
- Decision: `ScopeMigrationRecord` covers `LegacyLocal -> Published`
  promotion as well as published relpath and recorded-authority changes. It
  carries a typed old/new `ProjectScope`, transition kind, catalog epoch, and
  bounded operator invocation. Attachment-proved transitions require exactly
  one matching host-local proof; operator-attested transitions require none.
- Evidence:
  - Promotion already promises a durable audit containing old kind, new scope,
    invocation, and catalog epoch.
  - A journal alone is recovery evidence, not the queryable catalog audit
    surface.
  - A separate promotion ledger would duplicate the same nonbranching
    project-scope transition chain.
- Rationale: One typed transition chain gives promotion and migration the same
  atomicity and validation while preserving the catalog/attachment boundary.
- Revisit only if: catalog authority transitions move to a single replacement
  audit substrate with equivalent bidirectional proof validation.

## D-013: Full legacy paths stay out of default migration reports

- Date: 2026-07-22
- Phase: durable project catalog, compatibility inventory
- Status: accepted after independent plan review
- Decision: Immutable inventory observations contain the bounded literal
  selector required to classify legacy rows, but default reports expose only a
  domain-separated path digest. Full paths live in the strict host-local
  attachment snapshot. An operator may explicitly display an ambiguous row or
  request an owner-only, sensitive local-path review artifact.
- Evidence:
  - Deepest-root classification cannot be reproduced from row ids and counts.
  - Full historical paths may contain private repository or user identifiers.
  - Apply already reruns and hash-checks the immutable inventory, so the
    persisted default report need not carry literals to build exact post-images.
- Rationale: The engine receives complete deterministic inputs without turning
  a routinely archived report into a path disclosure surface.
- Revisit only if: the report is replaced by an equally deterministic local
  review format with explicit sensitive-data handling.

## D-014: Bound accepted-publication generations and hash exact source bytes

- Date: 2026-07-22
- Phase: durable project catalog, accepted publication migration
- Status: accepted during implementation
- Decision: Migration retains at most 2 MiB per accepted source file, 100,000
  knowledge entries, 100,000 gap entries, 128 MiB of source bytes per lane,
  and 256 MiB for the encoded generation. Deployment configuration may lower
  but never raise these limits. The generation records the exact committed
  source JSON bytes and their hashes, and its id is the lowercase SHA-256 of
  `bbox-accepted-publication-generation-v1`, a NUL separator, and the exact
  persisted generation bytes.
- Evidence:
  - Exact source bytes make commit provenance independently verifiable without
    trusting a reserialization of decoded values.
  - Explicit file, entry, lane, and generation limits prevent migration from
    turning malformed or unexpectedly large history into unbounded memory use.
  - A domain-separated content id is deterministic across rehearsal, apply,
    recovery, and remote-only verification.
- Rationale: Accepted publication is a durable recovery asset, so its identity
  must bind the exact evidence while every allocation remains bounded.
- Revisit only if: a replacement publication substrate provides equivalent
  byte-exact provenance, deterministic identity, and stricter resource bounds.

## D-015: Preflight persists the migration transaction identity

- Date: 2026-07-22
- Phase: durable project catalog, migration reproducibility
- Status: accepted after independent plan re-review
- Decision: Preflight mints one strong-random migration transaction id and
  persists it in the migration report and deterministic post-image input. The
  catalog origin, transaction draft, journal, and marker must all use that
  exact id. Apply never remints or substitutes it.
- Evidence:
  - A migrated catalog embeds the transaction id in its canonical post-image.
  - The report promises an exact predicted catalog hash that a later,
    separately invoked apply must reproduce.
  - Persisting only the predicted hash cannot recover a randomly minted id and
    would make the advertised post-image non-reconstructible.
- Rationale: Planning the id before hashing removes the reproducibility gap
  without deriving an authority-bearing transaction identity from mutable
  source data or introducing a hash cycle.
- Revisit only if: the migration report is replaced by another durable,
  operator-reviewed artifact that binds the same exact transaction identity.

## D-016: Preflight persists every random post-image identity

- Date: 2026-07-22
- Phase: durable project catalog, migration reproducibility
- Status: accepted after independent plan re-review
- Decision: Preflight plans and persists every strong-random value embedded in
  a predicted migration post-image. This includes repository-history ids,
  legacy-path binding ids, local commit namespaces required by inventoried
  local history, checkout ids, and the migration transaction id. Apply reuses
  the exact planned values and never remints them. A `LegacyLocal` project
  without inventoried history evidence receives no repository-history record.
- Evidence:
  - Repository-history and legacy-path maps use opaque random ids as canonical
    keys, and a local-authority history may require a random namespace.
  - A later apply cannot reconstruct a predicted hash if it remints any value
    contained in the hashed snapshot.
  - Deriving authority-bearing ids from paths, aliases, or weak namespaces
    would reintroduce the identity assumptions the catalog removes.
- Rationale: Persisting all planned random values makes separate preflight and
  apply invocations byte-reproducible while keeping logical identity opaque and
  independent of private or mutable source labels.
- Revisit only if: a replacement planning artifact durably binds every random
  post-image value or a reviewed opaque deterministic id scheme provides the
  same authority and privacy properties.

## D-017: Predicted migration timestamps come from inventoried state

- Date: 2026-07-22
- Phase: durable project catalog, migration reproducibility
- Status: accepted after independent plan re-review
- Decision: A migrated project's `created_at` and each corresponding
  attachment's `attached_at` preserve the legacy project's exact
  `registered_at` value. No wall-clock value enters a predicted migration
  post-image. Any later timestamp-bearing participant must use a persisted
  planned value or exact inventoried source value.
- Evidence:
  - Both timestamps are canonical snapshot bytes covered by predicted hashes.
  - Separately invoked preflight and apply cannot reproduce a wall-clock value.
  - The validated legacy registration timestamp already expresses the closest
    durable event available for both imported records.
- Rationale: Preserving existing time evidence gives byte-reproducible
  post-images without inventing precision or another planned timestamp.
- Revisit only if: a stronger exact source timestamp is inventoried and bound
  into the same plan before its predicted post-image is hashed.

## D-018: Stream all generation history and migrate only protected survivors

- Date: 2026-07-22
- Phase: durable project catalog, migration inventory
- Status: accepted after independent plan re-review
- Decision: Inventory streams the complete legacy generation namespace into a
  canonical ordered SHA-256 commitment and row count. Only effective roots and
  generations retained by the code-source store's actual owner policy for
  catalog, activation, or collision-lifecycle scopes become v2 participants.
  Other historical and orphan rows are inert, non-selectable GC candidates
  covered by the complete-set proof. Current validation and GC use the same
  mixed-store classifier.
- Evidence:
  - A valid long-lived store can contain more historical generations than a
    bounded migration report may materialize.
  - Aggregate commutative digests are weaker than a canonical ordered
    commitment and do not preserve sequence structure.
  - Treating every immutable directory as authority would revive orphan state;
    ignoring it entirely would make the migration inventory incomplete.
- Rationale: Streaming separates complete source-set evidence from the small
  protected subset that actually carries authority, bounding memory without
  imposing an arbitrary lifetime cardinality limit.
- Revisit only if: the owner store adopts a stronger bounded index that commits
  the complete namespace and applies the same survivor classification.

## D-019: Collision retirement ends in a durable terminal receipt

- Date: 2026-07-22
- Phase: durable project catalog, collision retirement
- Status: accepted after independent plan re-review
- Decision: One project-scoped lifecycle record advances through typed
  `Pending`, `Queued`, and `Completed` states while preserving the original
  losing project, former scope, generation, selector, and migration evidence.
  The completed record is retained as the terminal receipt. The physical queue
  is subordinate execution state; a matching lagging row is tolerated and
  removed idempotently.
- Evidence:
  - Deleting the only collision record after retirement erases the durable
    explanation for unavailable migrated state.
  - Requiring a queue row forever confuses execution work with terminal proof
    and prevents normal queue completion.
  - Crash windows exist both before queue publication and between physical
    completion, receipt installation, and queue cleanup.
- Rationale: A monotonic durable lifecycle gives startup, recovery, and GC one
  auditable authority surface while allowing the execution queue to drain.
- Revisit only if: a different durable store retains equivalent immutable
  migration evidence and monotonic crash-safe terminal state.
