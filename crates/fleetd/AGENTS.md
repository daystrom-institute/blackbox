# fleetd - the per-machine fleet supervisor

Slice 5 of `design/daemon-runtime/locality-first-decomposition.md`. The daemon
composes a fully-resolved `WorkerSpawnSpec` and hands it over a narrow typed
local RPC; fleetd spawns and supervises the harness child, relays its stdio
lanes, and serves a bounded replay window so live sessions survive daemon
restarts.

Why it exists at all: the monolith decision's own escape-hatch trigger
("sessions dropped by corpus-driven restarts") fires daily on a dev machine
that rebuilds and kickstarts `blackboxd` constantly. Extracting supervision
into a binary that changes a few times a year is the fix.

## Invariants

- **fleetd never re-derives policy.** It reads no brofile, no credential
  store, no provider config. Everything it needs arrives in the spawn spec.
  The moment a "just read this one config" shortcut lands here, the daemon and
  fleetd have two sources of truth for dispatch composition and the seam is
  worthless. Final binary path resolution is the ONLY thing derived
  executor-side, and only because it depends on fleetd's own login-shell PATH.
- **Dependency ceiling, enforced by `scripts/acceptance-fleetd-deps.sh`.** No
  `blackbox`, no `bro-harness`/`bro-tools`/`bro-code-mode`/`bro-capabilities`,
  no `bbox-*`, no tantivy, no V8. The script asserts on the resolved
  `cargo tree` graph, not on `Cargo.toml`, so a transitive arrival fails too.
  Run it whenever you touch a manifest here.
- **Single owner connection, generation-fenced.** Each accepted connection
  gets a fresh, never-reused generation; authenticating installs it as owner
  and fences the previous one. Two mechanisms enforce this and BOTH are
  load-bearing: `bro_rpc` rejects frames carrying a generation other than the
  one their own handshake negotiated, and fleetd explicitly notifies the
  superseded connection's fence so it drops the socket instead of lingering.
  This is what lets a restarted daemon reclaim fleetd with no liveness
  protocol.
- **Auth is a gate, not a suggestion.** The first post-handshake envelope must
  be `Authenticate` with a valid bearer token. A well-formed `Spawn` sent
  first is refused, not executed. `verify_peer_uid` runs on accept as a second
  independent check: it proves the peer runs as our uid, never which service
  it is. Use both, as `bro-rpc`'s AGENTS.md says.
- **Disconnect is not session death.** Losing the owner connection keeps
  children running, keeps the registry, and pauses relaying. Emitted messages
  with no owner attached are DROPPED on purpose: the durable event log is the
  backlog, and the next daemon replays from its own cursor. Do not "fix" this
  by adding an unbounded pending-message queue; that reintroduces the
  in-memory replay buffer the design explicitly rejected.
- **The daemon owns the replay cursor; fleetd owns the window.** No in-memory
  replay buffer. `ReplayFrom` streams the session's event-log tail; when the
  log does not reach back to the requested cursor, `ReplayUnavailable` reports
  the exact `(earliest_available, latest_available)` window so the daemon
  chooses between a documented gap and abandoning history. A silently short
  replay would be the worst outcome of the three.
- **Replay is chunked and yields.** A session with a huge log must not starve
  live control traffic on the same connection. Caps are per-chunk events AND
  bytes, with a `yield_now` between chunks.

## Spawn parity with the daemon's `LocalExecutor`

`crates/fleetd/src/spawn.rs` is a port of `src/orchestration/executor.rs`.
The ordering rules are copied deliberately and must stay in step:

- `env_unset` removal first, then spec `env` (so it wins over anything
  inherited), then `BRO_HOME` pinned LAST, because `BRO_HOME` is itself on the
  scrub list.
- `PATH` is the augmented dispatch PATH (`BRO_EXTRA_PATH`, `~/.local/bin`,
  `~/.cargo/bin`, then inherited PATH), plus `NO_COLOR=1`, `TERM=dumb`,
  `FORCE_COLOR=0`.
- `initial_messages` are queued on the control lane BEFORE the writer task
  starts, so the first user turn is unconditionally the first NDJSON line the
  child reads.
- The waiter joins the stdout pump, THEN stderr, then publishes the outcome.
  A fast fatal exit must not race the stderr snapshot empty.
- Login-shell `resolve_bin` with a PATH-walk fallback, falling back to the
  bare name on a miss so `Command::spawn` yields the familiar "No such file or
  directory".

Deliberate deltas, all documented at their call sites:

1. **Bin fallback is `BRO_HARNESS_BIN` else `bro-harness`**, not the daemon's
   provider-keyed `Provider::bin()`. fleetd supervises harness workers only;
   there is no `Provider::Workflow` lane here.
2. **stderr is a bounded tail** (`STDERR_TAIL_MAX_BYTES`), not the child's
   entire stderr. The daemon hands stderr over an in-process channel and can
   afford the whole thing; fleetd has to fit it in a bounded RPC frame. Whole
   lines are dropped from the front, so a snapshot is never a half-line.
3. **No `open_harness_tee`.** Teeing raw stdio is a daemon-side transcript
   concern, and the harness child already writes its own durable event log
   under the spec's `BRO_HOME`.
4. **fleetd's spawn is async** (`resolve_bin` runs in `spawn_blocking`). The
   daemon's `HarnessExecutor` trait is still synchronous; making it async is
   the cutover slice's job, not this one.

## Accepted v1 limits (do not build these here)

- A fleetd restart kills its children. Process-adoption tricks are not worth
  it for a binary whose whole value is that it rarely changes.
- No priority lanes, no capability brokering, no policy re-derivation, no
  persistence beyond the children and their event logs.
- Terminal-session GC is ack-driven only: a session is forgotten when it has
  exited AND the daemon has acknowledged through its last seq.

## Wire note

The design doc's slice 5 contract paragraph says "newline-delimited JSON".
The transport that actually landed is `bro-rpc`'s length-prefixed bounded
framing, which bans newline framing outright (see `crates/bro-rpc/AGENTS.md`).
The framing is an implementation detail beneath the message contract; the
contract's substance (versioned handshake, file-sourced bearer token, message
types in `bro-protocol`, socket path derived from the state dir) is unchanged.
