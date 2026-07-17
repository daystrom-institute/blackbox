# bbox-connectors - remote source connectors (stage 1: library only)

Pluggable adapters onto remote file/document stores, materialized into a
local root and then indexed by the existing (unmodified) pipeline. Design
authority: `design/connectors/remote-source-connectors.md`. This crate is
stage 1 of that design: the trait, the sync driver, the manifest/policy/
materialization machinery, the `MountRecord` store, and the `git`
connector. No daemon wiring, MCP tools (`bbox_mount_*`), or poller
integration yet - that is stage 2.

## Core invariant: read-only

`RemoteSourceConnector` has no write/delete methods, by construction - a
caller cannot even request mutation through the trait. This is
defense-in-depth, not proof an adapter's internals never issue a write:
adapters must still use read-only credentials/scopes wherever the provider
offers them. The `git` connector never pushes and never writes to the
remote; every write it performs targets the local materialization root
only.

## The manifest is the source of truth, not the cursor

A bare `ChangeCursor` cannot express deletion tombstones or rename
identity. The per-mount `Manifest` (`manifest.rs`) is what makes those
possible:

- **State invariant**: `content_hash` is `Some` exactly when
  `state == Materialized`. `Skipped`/`Pending` carry `None`. Checked by
  `ManifestEntry::is_state_consistent`, not enforced at the type level (the
  wire shape stays a flat JSON object) - don't construct a `ManifestEntry`
  literal without keeping this in mind.
- **Renames reconcile as moves.** The driver (`driver::reconcile_rename`)
  looks up the OLD manifest entry by `from_path` and carries its
  `remote_id` forward to the new `logical_path`/`physical_path`, physically
  renaming the file rather than deleting and re-fetching. A rename reported
  with no matching prior entry is a **degradation** (reported in
  `SyncSummary::degradations`), not a silent fallback - the entry still
  gets materialized as new, but the operator can see something was off.
- **Resume-without-refetch** works by comparing `remote_version`, not by
  trusting the cursor alone: an entry already `Materialized` with a
  matching `remote_version` is skipped with zero network calls, even on a
  freshly opened `Manifest` after a crash.

## Cursor-advance rule

The driver only advances a mount's cursor to the connector-reported
`next_cursor` when the batch processed with **zero per-entry errors**. A
batch with errors returns the cursor UNCHANGED, so the next pass re-lists
from the same point. This is cheap, not a full re-fetch, because
resume-without-refetch skips everything already materialized - only the
entries that actually failed get retried. Do not "helpfully" advance the
cursor on partial success; that would permanently strand the failed
entries (the next `list_changes` call would never see them again).

## Path safety: encoding is generic-path-loop only

`manifest::encode_physical_path` sanitizes components, rejects traversal,
and disambiguates case-fold/NFC collisions against the manifest - this is
what makes two case- or Unicode-normalization-colliding remote names
materialize as distinct files. It is exercised by the generic per-entry
driver loop (`driver::process_content_change_inner`) and future
non-bulk connectors.

The `git` connector does **not** run this path. It sets `physical_path`
equal to the raw git-tracked path, because `git reset --hard` already
wrote the file there verbatim and git's own path model forbids `..`/
absolute components. A case-insensitive/normalizing local filesystem
(APFS) cannot even hold two git-tracked paths that collide that way
simultaneously - `git reset --hard` would already have collapsed them
before the connector ever computed a diff. If a future connector needs
this path, follow the generic loop's shape, not git's.

## Bulk materialization seam

`RemoteSourceConnector::bulk_materializes()` / `materialize_bulk` is a
stage-1 addition beyond the design's literal four-method contract, for
connectors (git) whose sync mechanics already produce a full local tree
rather than a stream of discrete blobs. Two consequences to keep in mind
when adding a second bulk connector or reviewing `git_connector.rs`:

- **Policy is enforced after checkout, not before fetch**, for bulk
  connectors. The driver still removes excluded files and marks them
  skipped so the materialization root and index never carry them, but the
  git-protocol transfer itself was not prevented - git doesn't support
  selective per-blob transfer without partial-clone filters. Byte caps
  (`max_file_bytes`, `max_total_bytes`) are charged against the ALREADY
  materialized on-disk size for the same reason. This is an honest,
  documented asymmetry with the generic per-entry loop, not a bug.
- **`ChangeBatch::is_full_walk`** (distinct from `degraded_to_full_walk`,
  which specifically means "the connector couldn't resume") tells
  `materialize_bulk` whether `batch.changes` is exhaustive. Only then does
  it diff the manifest's existing entries against the batch's live paths
  to find orphans - files present from a PRIOR sync that the current
  enumeration doesn't mention at all (the concrete case: a mount's target
  `#<ref>` switches to a different branch that doesn't carry some files).
  An ordinary incremental batch is not exhaustive, so this reconciliation
  only runs when `is_full_walk` is set.

## Concurrency discipline

This crate is a plain library - it must never spawn its own tokio tasks or
threads. Every filesystem write (`materialize.rs`, `manifest.rs` open/save,
`mount_record.rs` open/save) is a synchronous function carrying a reasoned
`#[allow(clippy::disallowed_methods)]`, matching the
`bbox-corpus-core::json_store` / `bbox-indexing::projects` convention
elsewhere in the workspace: stage 1 callers (tests, direct library use) run
these inline; the daemon (stage 2) is expected to pool them onto its
blocking lane rather than call them from a tokio worker. `git_connector.rs`
is the exception that proves the rule: it shells out via
`tokio::process::Command` (never `std::process::Command`), which is
legitimately async and needs no such wrapping.

## Credentials never land here

`MountConfig`/`MountRecord`/`Manifest`/`SyncSummary` carry no credential
fields, by design. The git connector relies entirely on ambient auth (ssh
agent, credential helper, or a token the operator already embedded in the
scope URL) and redacts userinfo (`user:pass@`/`token@`) out of any URL
before it can reach `RemoteInfo`, logs, or an error message
(`git_connector::redact_scope`). `mount_record.rs` has a test asserting no
field name on `MountRecord` looks credential-shaped - a tripwire, not a
substitute for review, when a future field gets added.

## Testing

No env mutation in this crate's tests (unlike `bbox-util`/`bbox-indexing`,
which hold `crate::util::test_env_lock()` for that reason) - keep it that
way; if a future addition needs env-dependent path resolution, resolve it
at the call site instead of inside a test that mutates process env.
`git_connector.rs`'s integration tests build real local fixture repos with
the git CLI under per-test tempdirs (`file://` URLs) - they need `git` on
`PATH` but touch no shared state.
