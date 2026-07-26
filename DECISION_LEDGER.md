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
  attachment ids, legacy-path binding ids, local commit namespaces required by
  inventoried local history, checkout ids, and the migration transaction id.
  Apply reuses the exact planned values and never remints them. A
  `LegacyLocal` project without inventoried history evidence receives no
  repository-history record.
- Evidence:
  - Repository-history, attachment, and legacy-path maps use opaque random ids
    as canonical keys, and a local-authority history may require a random
    namespace.
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
  per-generation `Pending`, `Queued`, and `Completed` states while preserving
  the original losing project, former scope, generation, typed selector
  evidence, and migration evidence. Active losers carry their exact
  materialized selector; retained-only losers carry `NoDurableSelector`.
  Completed entries are retained as terminal receipts. Physical work rows are
  subordinate execution state; a matching lagging row is tolerated and removed
  idempotently.
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

## D-020: The offline catalog CLI has one versioned result envelope

- Date: 2026-07-22
- Phase: durable project catalog, offline CLI
- Status: accepted after independent plan re-review
- Decision: Every parsed migration or verification command writes one tagged
  v1 JSON envelope containing `version`, `command`, and exactly one of `result`
  or `error { code, message }`. The envelope goes to stdout, human diagnostics
  go to stderr, and failures exit nonzero without success-shaped output. Help,
  version, and parser diagnostics retain conventional side-effect-free streams.
- Evidence:
  - "Versioned and stable" alone does not define a wire contract callers can
    parse or distinguish from a partial success.
  - A single tagged envelope permits compatible field growth while keeping
    success and failure structurally exclusive.
  - Clap must be able to render help and version before configuration or stores
    are available.
- Rationale: One explicit response algebra gives automation deterministic
  output without weakening normal command-line ergonomics.
- Revisit only if: a versioned protocol with equivalent exclusive
  success/failure semantics replaces the JSON command surface.

## D-021: CLI roots and resolution precedence are explicit

- Date: 2026-07-22
- Phase: durable project catalog, offline CLI
- Status: accepted after independent plan re-review
- Decision: `verify --root` and apply's `--rehearsal-root` name a rehearsal
  state root, from which the migration facade derives participant paths.
  Explicit `--state-dir` re-roots the complete conventional source bundle;
  explicit `--projects-path` then overrides its projects member and wins when
  both are present; otherwise shared configuration supplies every path.
  Preflight and apply both require an explicit resolution artifact path;
  first preflight may create the canonical empty artifact, and apply consumes
  the exact clean report/resolution pair.
- Evidence:
  - Treating `--root` as a file on one command and a state root on another
    would make rehearsal verification ambiguous.
  - Independent projects and state path overrides can otherwise select
    different source sets depending on argument order.
  - Resolution affects preflight classification and must be hash-bound to the
    report that apply validates.
- Rationale: A single destination model and fixed override order prevent the
  CLI adapter from inventing authority or bypassing the migration facade.
- Revisit only if: the facade adopts one typed location object that makes
  conflicting overrides unrepresentable while preserving these semantics.

## D-022: Ordinary retained generations carry no fabricated selector

- Date: 2026-07-23
- Phase: durable project catalog, migration inventory
- Status: accepted after independent plan re-review
- Decision: Active generations and active collision losers must carry their
  exact durable materialized selectors. An ordinary retained generation or
  retained-only collision loser without an activation has no selector
  authority and carries typed `NoDurableSelector`; migration joins it through
  the owner-locked retention set, immutable descriptor and manifest, generation
  id, project, and scope. Retained-only collision retirement is keyed by exact
  project/generation identity. Ambiguous retained scope ownership produces a
  bounded resolution-required candidate set instead of an invented owner or
  pre-report abort.
- Evidence:
  - Stored-generation evidence does not durably retain the `:m<16hex>`
    materialization suffix after activation/effective selection is gone.
  - The project-plus-generation selector is only a prefix and cannot authorize
    retirement, quarantine, or an exact selector join.
  - Descriptor, manifest, owner retention, project, and scope remain exact
    evidence for rewriting ordinary retained metadata.
- Rationale: Typed absence preserves every authority distinction while still
  migrating retained bytes whose ownership is provable.
- Revisit only if: the owner store begins durably binding the exact
  materialized selector to every retained generation.

## D-023: Canonical inventory is path-redacted

- Date: 2026-07-23
- Phase: durable project catalog, migration inventory privacy
- Status: accepted after independent plan re-review
- Decision: Canonical inventory JSON contains domain-separated path digests,
  typed relationships, and stable row ids, never absolute paths or literal
  legacy selectors. Bounded literals live only in a non-serializable
  host-local runtime binding set paired one-to-one with those digests. Apply
  recaptures inventory and bindings under the same owner locks and verifies the
  pairing. The explicit owner-only local-path review artifact and strict
  attachment post-image remain separate sensitive host-local surfaces.
- Evidence:
  - Canonical inventory bytes are hashed and may otherwise leak through debug,
    serialization, or report plumbing.
  - Deepest-root classification still requires literals during capture and
    apply, but the durable binding needs only their domain-separated digest.
  - D-013 already requires default reports and public fixtures to remain
    path-redacted.
- Rationale: Separating deterministic evidence from ephemeral path authority
  preserves reproducibility without making canonical inventory a disclosure
  surface.
- Revisit only if: a different non-serializable capability model provides the
  same digest-bound classification and apply-time recapture guarantees.

## D-024: Collision lifecycle is a bounded per-generation map

- Date: 2026-07-23
- Phase: durable project catalog, collision retirement
- Status: accepted after independent plan re-review
- Decision: One project-scoped collision lifecycle document contains a bounded
  canonical map with one immutable-evidence entry for every active and
  owner-policy-retained generation of the losing project. Entries transition
  independently through `Pending`, `Queued`, and `Completed`. Subordinate work
  is keyed by a code-derived id over project and generation, and workers
  complete by that identity; selector evidence is an optional deletion target,
  never the work identity.
- Evidence:
  - One losing project can have one active generation plus multiple retained
    generations, while a scalar lifecycle can represent only one.
  - Retained-only generations have no durable materialized selector, so a
    selector-keyed queue cannot execute or complete their retirement.
  - A project-scoped document preserves the durable terminal-receipt model while
    allowing atomic installation of the complete collision set.
- Rationale: Per-generation entries make the quarantine complete and
  crash-recoverable without fabricating selectors or multiplying project-level
  authority records.
- Revisit only if: the owner store adopts an equally complete bounded
  generation-indexed lifecycle with the same immutable evidence and monotonic
  transition guarantees.

## D-025: One facade owns migration authority end to end

- Date: 2026-07-23
- Phase: durable project catalog, migration facade repair
- Status: accepted after independent plan re-review
- Decision: `bbox-indexing` exposes exactly one executable migration facade
  with typed preflight, rehearsal-apply, and fresh-verify operations. The
  facade opens every inventory owner, constructs every deterministic
  post-image and participant, owns the complete registry, invokes the
  transaction owner, and returns a redacted receipt plus a separate
  non-serializable compatibility projection. Inventory captures, runtime path
  bindings, owner snapshots, participant drafts, registries, and
  migration-aware store open remain crate-private.
- Evidence:
  - The implemented inventory-only facade deliberately fails while auxiliary
    owner lanes are unconnected.
  - The validated transaction seams are crate-private, but without a higher
    owner a CLI would have to reconstruct their authority joins and path
    registry.
  - Compatibility paths are required for parity tests but are forbidden in
    default report and CLI JSON.
- Rationale: One closed authority owner prevents the offline CLI and later
  runtime adapters from assembling subtly different migrations, while the
  split receipt/projection keeps private paths out of serializable output.
- Revisit only if: a replacement crate provides one equally closed,
  independently verified migration authority surface without exposing
  participant assembly to callers.

## D-026: Rehearsal is preflighted in place and binds exact artifacts

- Date: 2026-07-23
- Phase: durable project catalog, rehearsal and artifact identity
- Status: accepted after independent plan re-review
- Decision: The Phase-1 facade never copies configured live state. An operator
  or hermetic test prepares a complete isolated v1 bundle and reruns preflight
  against it before rehearsal apply. First preflight may create a canonical
  empty resolution at the explicit resolution path; apply always requires the
  existing report and resolution. Their exact byte hashes are recorded in the
  journal, marker, apply receipt, and verification receipt.
- Evidence:
  - Canonical inventory includes path digests and host-local bindings, so a
    report captured against live roots cannot safely authorize silently
    rebased checkout and participant paths.
  - Having preflight copy state would violate its read-only owner-store
    contract and introduce an unbounded filesystem copier into the authority
    path.
  - Semantic plan hashing alone cannot prove that apply consumed the exact
    operator-reviewed files when insignificant JSON byte differences exist.
- Rationale: In-place isolated preflight makes destination identity explicit,
  preserves no-copy safety, and gives recovery an auditable binding to the
  exact reviewed artifacts.
- Revisit only if: a reviewed snapshot/export format can rebase every
  path-bearing authority while preserving the same inventory and post-image
  proofs, or persisted artifacts adopt a stricter canonical-byte format with
  equivalent exact identity.

## D-027: Full rebuild preserves history from immutable generations

- Date: 2026-07-23
- Phase: path-free index and Git history
- Status: accepted after independent plan re-review
- Decision: A full index replacement rematerializes stale, compatibility, and
  active commit documents from referenced immutable
  `RepoHistoryGeneration`s. Ambiguous or unclaimed legacy commit namespaces
  live in immutable `RepoHistoryQuarantineGeneration`s with complete ordered
  document commitments and remain rebuild/GC roots until explicit resolution
  or acknowledged retirement. A checkout is never the only rebuild source for
  retained history.
- Evidence:
  - Full replacement deletes the old index before local Git reingestion would
    run.
  - An attachment-less project cannot rewalk Git, while ambiguous namespaces
    are intentionally forbidden from selecting an arbitrary repository.
  - The governing design promises that imported commit refs remain queryable
    and that ordinary project GC does not delete quarantined history.
- Rationale: Immutable history generations make the durability promise
  executable and let rebuild fail before replacement when its complete source
  set cannot be proved.
- Revisit only if: a different durable history store supplies the same
  complete-count/hash proof, namespace quarantine, reference tracking, and
  checkout-independent rematerialization.

## D-028: Migration reports distinguish executable plans from assessments

- Date: 2026-07-22
- Phase: durable project catalog, migration facade
- Status: accepted during facade integration repair
- Decision: Every migration report carries `plan_kind` as either `executable`
  or `assessment_only`. A clean report must be executable. Resolution-required
  and refused reports must be assessment-only and carry a domain-separated
  assessment hash that is never accepted as a transaction plan identity.
- Evidence:
  - An unsafe checkout marker correctly refuses migration, but the strict
    deterministic post-image cannot represent an attachment through that
    checkout.
  - An unresolved attachment exclusion cannot both preserve every candidate
    and satisfy the exact post-image attachment set.
  - Existing report fields require a hash even when no valid executable
    post-image exists.
- Rationale: Explicit plan kind preserves the stable report field shape while
  preventing diagnostic review state from masquerading as installable state.
  Apply rejects assessment-only identity before participant or transaction
  assembly.
- Revisit only if: a later report version makes executable plan and prediction
  fields explicitly optional and preserves the same fail-closed apply check.

## D-029: A terminal committed migration journal admits the registry-free runtime open

- Date: 2026-07-24
- Phase: durable project catalog, Phase 2 (P2-B runtime path)
- Status: accepted pending implementation review
- Decision: `recover_locked` on a regular-registry owner verifies a
  migration-kind journal in the terminal committed state
  (`Committed`/`Committed`) with a registry-free pair subset: the installed
  catalog and attachment images must match the journal's new hashes, and the
  strict open's existing origin/marker/journal binding verification covers
  the marker chain. Every non-terminal migration journal still refuses
  without the complete code-owned participant registry, and full participant
  plus code-source verification remains the offline facade's verify
  operation.
- Evidence:
  - Phase 1 deliberately retains the committed migration journal as a GC
    root, so every successfully migrated root carries one forever.
  - The pre-existing gate refused ANY migration journal without the
    registry, which made a bare `open_existing` on a migrated root
    impossible; Phase 2's catalog-mode daemon open is exactly that call.
  - `verify_origin_marker_locked` already performs the registry-free
    marker/journal binding on every strict read, and
    `commit_regular_pair_locked` already preserves the committed migration
    journal before regular transactions, so the regular-owner lifecycle
    over migrated state was otherwise designed for.
- Rationale: A committed terminal migration requires no forward or rollback
  action, so recovery has nothing to decide; refusing to open was Phase 1
  fail-closed posture with no runtime consumer, not a durable invariant.
  The registry-free subset verifies exactly the state a regular owner can
  honestly prove and defers the rest to the offline facade.
- Revisit only if: the migration journal retention policy changes, or a
  later phase gives the daemon a code-owned participant registry at boot.

## D-030: The catalog-mode smoke root is produced by the facade-driving test, verified by the CLI

- Date: 2026-07-25
- Phase: durable project catalog, Phase 2 (P2-B..P2-E bootsmokes)
- Status: accepted pending implementation review
- Decision: catalog-mode live bootsmokes materialize the isolated migrated
  root by running the ignored producer test
  (`produce_migrated_smoke_fixture_from_env_root` in
  `crates/bbox-indexing/tests/project_catalog_migration_facade.rs`), which
  drives the byte-identical facade ceremony production uses
  (assessment, scope-owner resolution, quarantine, preflight, apply into a
  rehearsal root). The stable-signed `blackbox` CLI then runs
  `verify --root` on the produced root, and the stable-signed daemon boots
  on it. The plan's original section 12 wording had the CLI produce the
  root via preflight/apply.
- Evidence:
  - The CLI preflight source layout is config-shaped by design; a synthetic
    config-shaped fixture has no real publisher git history and fails
    preflight with `publisher_git_evidence_missing`, so a CLI-produced
    smoke root would require a live prior host, exactly what an isolated
    smoke must not depend on.
  - The CLI preflight/apply envelopes were live-smoked against real state
    in P1-D, so the smoke's CLI coverage duty here is verification, not
    production.
- Rationale: the smoke's purpose is proving the daemon boots and serves
  catalog mode on faithfully migrated bytes; the facade test produces those
  bytes through the same owner code paths, and shifting the CLI's smoke
  role to `verify --root` keeps every artifact exercised without a
  synthetic-evidence back door.
- Revisit only if: preflight gains a fixture-evidence mode, or the smoke
  gains access to a real pre-migration host state.

## D-031: Bespoke exact-only filter resolvers adopt the engine's broad Read gate

- Date: 2026-07-25
- Phase: durable project catalog, Phase 2 (P2-E caller conversion)
- Status: accepted pending implementation review
- Decision: converting B1 (corpus-search filters), B4 (slack channel
  binding), and B6 (storage tools) to the shared resolver engine gives
  their selectors the canonical-spine Read semantics: worktree and
  descendant paths of a registered project now resolve to the base project
  id instead of missing. The plan's section 4.3 parity list gains this as
  deliberate change 6. Literal substring/pass-through lanes are unchanged,
  and no unresolved selector manufactures identity outside the tagged
  eight-hex and path-hash compatibility lanes.
- Evidence:
  - The exact-only misses were the documented defect class: a worktree path
    reaching the hash lane derives a foreign id and silently returns empty
    results (gap-72fd5932; crates/bbox-corpus-index/AGENTS.md warns the
    ordering is load-bearing).
  - The 2026-06 taxonomy consolidation already declared
    `resolve_project_context` the single entry point for project-like
    input; the bespoke matchers predate it.
- Rationale: preserving an exact-only miss as "parity" would freeze a trap
  the plan retires B1-B8 to eliminate; the engine's Read gate is the
  documented resolution semantic for every other read surface.
- Revisit only if: a surface is found whose consumers depend on worktree
  paths NOT aliasing to the base (none known; dispatch execution targets
  are out of scope by section 3).

## D-032: The version-1 any-read grant is a sanctioned bridge lane; v2 enforces recorded capabilities

- Date: 2026-07-25
- Phase: durable project catalog, Phase 2 (closing review, finding M9)
- Status: accepted pending implementation review
- Decision: the capability asymmetry between the two authorities is
  deliberate for the bridge window. The version-1 authority grants every
  Read-intent kind on any resolvable path (it records no capability bits
  and has nothing to enforce); the catalog authority enforces the
  capability bits recorded at attach or migration time for every kind,
  with `PublisherConfigTreeRead` and `KnowledgeGapOverlayRead` both riding
  `repo_knowledge`, and additionally verifies the live checkout (marker
  match plus canonical-dir identity) at resolve and revalidation time.
  Surfaces that must behave identically across modes do so by degrading
  per capability (post-register enrichment, scoped views), never by
  weakening the catalog gate to the v1 grant.
- Evidence:
  - The v1 lane cannot deny what it never recorded; back-deriving bits for
    version-1 records would manufacture authority from path shape, exactly
    what the catalog design forbids.
  - The Phase 6 cut criteria consume compatibility observations;
    capability-grant asymmetry is visible there through the catalog-mode
    degradations (skipped enrichment steps, per-checkout diagnostics), not
    through v1-side counters.
- Rationale: attach-time capability derivation is the catalog's authority
  model; the bridge window tolerates the broader v1 grant because every
  v1 read still routes through path-resolution authority, and the strict
  gate arrives with the mode, not with a flag.
- Revisit only if: the observation window shows catalog-mode capability
  denials for surfaces operators expect to work (a derivation gap at
  attach time), or Phase 6 needs per-kind grant telemetry on the bridge.

## D-033: Closing-review residual dispositions (publisher-bind window, v1 synthetic ids, Selected ladder)

- Date: 2026-07-25
- Phase: durable project catalog, Phase 2 (closing review, round 2)
- Status: accepted pending implementation review
- Decision: three review residuals are dispositioned rather than coded
  away this phase.
  1. Publisher-bind freshness: the bind operation performs its epoch CAS
     and Attached revalidation inside the publication-lock critical
     section, with a second freshness recheck immediately before the
     pointer swap. Catalog detach transactions deliberately do NOT take
     the publication lock (the §11 lock order keeps the pointer store's
     lock independent, and inverting that order from the detach side
     would couple every catalog transaction to publication I/O), so a
     detach landing inside the final swap window can still leave the
     pointer naming a freshly detached attachment. That state is a
     misleading binding, not corruption: publisher freshness reporting
     degrades it, and rebinding repairs it. Accepted for the bridge
     window.
  2. The version-1 authority keeps fabricating the shared deterministic
     `v1-root` checkout id for markerless checkouts. Changing v1 lease
     identity semantics mid-bridge is exactly what the §4.3 parity
     contract forbids; the residual is bounded to observation identity on
     read leases of markerless checkouts, and every catalog-mode
     attachment mints a real marker. Retired with the v1 lane in Phase 6.
  3. The catalog `Selected` lease ladder resolves the operator default,
     then a single active attachment, then the unique active `Base`
     attachment. The base rung extends exit-gate item 3's enumeration and
     is the §5.3 key-to-base rule applied to lease selection: index and
     overlay lanes act on the durable base checkout, and refusing the
     normal base-plus-worktree topology would stall exactly the lanes the
     catalog serves. Ladder rungs deliberately ignore the requested
     capability: a selected default lacking the needed bit is a typed,
     visible capability refusal, not a silent fall-through to a different
     attachment than the operator selected.
- Revisit only if: publisher freshness reporting shows bind-then-detach
  interleavings occurring in practice, or Phase 6 retires the v1 lane and
  the synthetic-id residual with it.

## D-034: The bridge identity marker is identity provenance, not a scope variant

- Date: 2026-07-25
- Phase: durable project catalog, Phase 3 (P3-A substrate)
- Status: accepted after milestone review (two-round adjudication)
- Decision: `CodeProjectIdentity` carries a typed
  `IdentityOrigin { Catalog, Bridge }` provenance field, and the Phase 3
  plan's "typed BridgeLegacy marker" is realized as that field rather than
  as a third `ProjectScope` variant. The bridge constructor keeps
  `scope = LegacyLocal` as a placeholder. The collected-staging refusal
  keys on `origin == Catalog && scope == LegacyLocal`; bridge identities
  always proceed.
- Evidence:
  - `ProjectScope` serializes into catalog bytes; a bridge-only variant
    could leak into `CatalogSnapshotV2` unless `validate_catalog` also
    rejected it at every boundary.
  - `ProjectRecord` carries no `PublishedScope`, so a scope-shaped marker
    would collapse every bridge identity into the catalog LegacyLocal
    refusal, wholesale breaking bridge collected staging, which runs on
    lease/grant-table authority through Phase 3.
  - The milestone reviewer confirmed the origin-keyed predicate preserves
    the plan section 6 item 1 refusal exactly (a catalog LegacyLocal
    project can never hold a producer grant) while bridge staging
    proceeds.
- Rationale: identity provenance is bridge-lane metadata, not durable
  catalog vocabulary; keying the refusal on provenance plus scope yields
  the intended refusal set without contaminating catalog serialization.
- Revisit only if: Phase 6 retires the bridge lane, at which point
  `IdentityOrigin::Bridge` and the placeholder scope can be deleted with
  it.

## D-035: A version bump migrates collected materializations in place

- Date: 2026-07-26
- Phase: durable project catalog, Phase 3 (P3-E schema cut)
- Status: accepted after milestone review
- Decision: The full-rebuild collected path classifies a persisted
  collected selector whose materialization suffix differs from the running
  binary's mint, for the same project and generation and with an agreeing
  activation record, as outgoing rather than corrupt, and migrates it in
  place: re-stage from store blobs under the current version with zero
  leases, save the new activation record preserving cutback state
  verbatim, flip the manifest entry under the coordinator, and enqueue the
  outgoing selector's retirement. Every other mismatch shape keeps the
  fail-closed bail; incremental passes preserve rather than migrate.
- Evidence:
  - Persisted collected selectors and snapshot ids fold the
    materialization version; the P3-E INDEXER_VERSION bump is the first
    since the collected lane shipped, and the pre-existing bail made the
    first post-deploy boot fail for every project with an active collected
    generation (the synchronous post-reset rebuild errored before the
    activation-driven convergence path could ever run).
  - The grounding sweep's footgun list predicted the class; four
    plan-review rounds missed it because the plan never enumerated a
    collected-selector materialization migration.
  - The milestone reviewer verified the classification matrix fail-closed
    for every non-outgoing shape, and the re-recorded document count and
    entity inventory keep the next rebuild's preservation check satisfied.
- Rationale: outgoing state is not corrupt; it is the deterministic
  consequence of the version bump the release itself ships. Refusing it
  converts every upgrade into an outage; migrating from immutable store
  blobs preserves the zero-lease remote-only contract.
- Revisit only if: materialization versioning moves into a
  selector-independent read-view layer, or Phase 6 retires the persisted
  selector shapes entirely.

## D-036: The materializer proof is two-mode, gated by source fingerprint

- Date: 2026-07-26
- Phase: durable project catalog, Phase 3 (P3-E integration of the P3-D
  materializer)
- Status: accepted after milestone review and live forced-replacement smoke
- Decision: prove_against_inventory selects its mode by recomputing the
  Phase 1 capture-recipe source fingerprint over the current index through
  the same shared fold the asset was written with. Equality mode (recorded
  fingerprint matches recomputed) keeps exact per-namespace count and
  commitment equality. Every other case is Drift mode: namespaces absent
  from the asset classify normally with no asset constraint, recorded
  namespaces must survive with observed counts at least the recorded ones,
  and commitments are not compared because an ordered fold cannot prove
  subset containment. A cross-namespace survival check runs in both modes.
  The proof mode and both fingerprints are recorded in the outcome and the
  rebuild manifest; a Phase 6 offline rebuild must require Equality mode.
- Evidence:
  - The live forced-replacement smoke refused on a real migrated root that
    had been live-indexed since migration: a post-migration local
    namespace was absent from the point-in-time asset, and growth within
    recorded namespaces is equally legitimate for append-only history.
  - Two pre-existing tests asserted refusal on exactly the growth a live
    root produces; they encoded the defect and were converted.
  - The reviewer verified the shared fold leaves one recipe with two
    callers, the loss guards hold in both modes, and equality-mode
    reachability is proven first in the facade-rehearsal row.
- Rationale: the asset is a point-in-time migration record, not a
  description of a living index. Equality is the right contract exactly
  when the index is unchanged since migration; under drift the honest
  provable property is that no recorded history was lost.
- Revisit only if: the asset gains per-row content (not just fold hashes)
  sufficient to prove subset containment, or Phase 6 changes the offline
  rebuild's capture story.

## D-037: Ready binds the primary namespace; compatibility generations are manifest-owned

- Date: 2026-07-26
- Phase: durable project catalog, Phase 3 (P3-E integration, materializer
  addendum)
- Status: accepted after milestone review
- Decision: RepoHistoryRecord.materialization Ready names the generation of
  the record's PRIMARY namespace only. Generations materialized for a
  record's compatibility namespaces mint owned (rhg_) ids but are owned
  durably by the rebuild manifest through a dedicated
  compatibility_generation_ids bucket, exactly as unclaimed generations
  are; no catalog field names them. The double-advancement refusal narrows
  to the primary map: one advancement per record per pass, keyed to the
  primary namespace.
- Evidence:
  - One record legally owns a primary plus compatibility namespaces:
    validate_catalog enforces global uniqueness across records, not one
    namespace per record; the runtime admin path produces the state (the
    v1 importer cannot, because conflicting published authorities refuse
    at preflight - the fixture documents the exact refusal chain).
  - The prior guard comment claimed unreachability from a misreading of
    the uniqueness invariant; the pin test made the wrongness visible.
  - Pin continuity holds because every rebuild re-materializes the same
    content-addressed ids into its own manifest while the documents
    persist, and the Phase 6 strict startup check rides the committed
    manifest it already requires together with Equality proof mode.
- Rationale: the governing model routes all new materialization through
  the primary namespace; compatibility namespaces are legacy-lookup
  surfaces. A dedicated manifest bucket keeps the audit honest: a
  generation the catalog cannot reach must not be reported as
  catalog-owned.
- Revisit only if: materialization becomes per-namespace catalog state, or
  the namespace-resolution operation changes compatibility-namespace
  ownership semantics.
