# Architecture Relayer Log

This file records architecture issues that are too large for a safe in-place
bug fix during the differentiated-runtime campaign. Entries describe current
structural gaps, not release history.

## AR-001: complete the corpus MCP extraction from legacy SharedState

- Status: open
- Design anchors: `design/daemon-runtime/agent-runtime-program.md` Milestone 7;
  `design/daemon-runtime/fleet-extraction.md` Stages 7 and 8
- Implemented boundary: `blackbox-corpus-service` is independently buildable
  and owns typed corpus search plus durable, idempotent record ingestion. Its
  transitive dependency test rejects blackbox, blackops, fleet, harness, tools,
  provider, code-mode, and V8 implementation dependencies.
- Remaining coupling: the public RMCP `BlackboxServer` and corpus tool handlers
  are assembled over the root `SharedState`. Writer/reindex construction and
  corpus stores are still mixed with compatibility operational fields across
  `src/server/state.rs`, `src/server/open.rs`, `src/server/mcp.rs`, and
  `src/tools/`.
- Impact: the typed worker and record path has an independent restart boundary,
  but replacing the legacy `blackboxd` binary with the slim service would drop
  the existing public corpus MCP surface. The legacy process also retains
  operational compatibility code during the migration window.
- Required next increment: peel a corpus-only state/writer assembly, classify
  and move corpus RMCP handlers onto it, run both routers against the same
  corpus fixtures, then rename the dependency-clean binary to `blackboxd` and
  remove the legacy operational implementation after the compatibility sunset.
- Removal gate: `cargo tree` for the installed blackboxd contains none of the
  forbidden implementation crates, all corpus MCP conformance tests run against
  that binary, and a corpus restart during a live local-only worker turn loses
  neither the worker nor idempotent record catch-up.

## AR-002: restore strict Clippy conformance across legacy synchronous crates

- Status: open
- Scope: a workspace `cargo clippy --workspace --all-targets -- -D warnings`
  reaches more than 1,800 diagnostics outside the differentiated-runtime
  implementation. Most are `clippy::disallowed_methods` findings in older
  synchronous stores, refactor adapters, corpus fixtures, and TUI tests after
  the blocking-I/O lint policy is applied to every target.
- Impact: the release can enforce strict Clippy on the new contract, service,
  harness, and client packages, but the same flag set is not yet a useful
  repository-wide regression gate. Broad crate-level suppressions would hide
  real async-boundary mistakes and are not an acceptable closeout shortcut.
- Required next increment: classify each legacy call site as an async runtime
  violation, an intentional synchronous actor/persistence boundary, or a test
  fixture. Migrate runtime violations to sanctioned blocking lanes, place only
  narrow rationale-backed allowances on intentional boundaries, and convert
  test fixtures in bounded crate batches.
- Removal gate: the exact workspace command above passes without crate-wide
  lint suppressions, and CI runs it as a required gate.

## AR-003: add an explicit legacy authority-state cutover and migration path

- Status: open
- Design anchors: `design/daemon-runtime/fleet-extraction.md` Stages 3, 5, and
  6; `design/daemon-runtime/process-topology.md` Sections 7 and 8
- Implemented boundary: fleetd and blackopsd own new, independent state roots;
  blackopsd imports the exact shipped definition catalog and existing installed
  artifact definitions, while blackboxd defaults to corpus-only authority.
- Remaining gap: there is no supported reader or conversion tool for legacy
  live `tasks.json`, worker or resume leases, logical-agent/mailbox state,
  workflow runs, waits, approvals, schedules, crons, webhook/poller cursors, or
  system-event runtime state. Starting the differentiated authorities creates
  fresh fleet and operational state even when the old monolith stores remain.
- Impact: an operator must drain or abandon legacy work before cutover and keep
  the old state only for audit or rollback. Copying old files into the new
  state roots is unsupported and can violate the one-writer invariant. Legacy
  maintenance installers and system-event tools are compatibility-only until
  their state and scheduling semantics are ported to blackopsd.
- Required next increment: inventory every legacy operational store, define
  versioned readers and stable identity mappings, add a dry-run conversion
  report, require a drained-authority precondition, write blackopsd/fleetd
  snapshots transactionally, and provide a rollback manifest. Include explicit
  conversions or retirement decisions for maintenance schedules and
  system-event queues.
- Removal gate: a fixture containing live and terminal legacy tasks, logical
  agents, mailboxes, workflow and schedule state can be dry-run, converted once
  under stable IDs, started by the new services without duplicate effects, and
  rolled back before either new authority accepts additional mutations.

## AR-004: persist daemon-owned Brodex refresh-token rotation safely

- Status: open
- Design anchor: `design/bro-harness/leaf-sandbox-isolation.md` credential
  isolation contract
- Implemented boundary: fleetd reads the host Codex auth source before worker
  launch, passes its contents through the scrubbed task-local session
  environment, and denies the worker all reads and writes to that source. The
  harness keeps refreshed access, ID, and refresh tokens only in that
  session-local in-memory value.
- Remaining gap: there is no authenticated worker-to-fleetd write-back channel
  by which fleetd can persist a rotated refresh token to the host credential
  source. A worker must not be granted a sandbox exception or direct source
  path merely to close this gap.
- Impact: if the OAuth server rotates and invalidates the previous refresh
  token, a worker crash or fleetd restart before daemon-owned persistence can
  leave the source token stale. A later Brodex worker can then fail to refresh
  and require the operator to authenticate again.
- Required next increment: define a fleetd-owned credential-rotation message
  carrying the provider lane and expected source generation, authenticate it
  to the bound session, update the source atomically under an owner-only lock,
  and acknowledge durability before the harness discards the prior task-local
  credential value.
- Removal gate: a forced-refresh integration test rotates the refresh token,
  crashes the worker immediately after the durability acknowledgement, starts
  a replacement worker for the same session, and proves that it refreshes from
  the persisted token; stale-generation and cross-session write-back attempts
  fail closed. The worker sandbox still denies the credential source.
