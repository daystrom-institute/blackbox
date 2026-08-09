# fleetd - the per-machine fleet supervisor

Slice 5 of `design/daemon-runtime/locality-first-decomposition.md`. The daemon
composes a fully-resolved `WorkerSpawnSpec` and hands it over a narrow typed
Unix or explicitly enabled TCP RPC; fleetd spawns and supervises the harness child, relays its stdio
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
  An off-host executor also pins `BRO_HOME` and the event-log path to the
  explicitly configured worker roots. That is path localization, not policy
  derivation, and prevents a container-local path from crossing machines.
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
  first is refused, not executed. Unix accepts also run `verify_peer_uid` as a
  second independent check: it proves the peer runs as our uid, never which
  service it is. TCP has no Unix uid claim, is disabled by default, refuses a
  non-loopback bind without a second explicit grant, and is valid only behind
  an encrypted, ACL-restricted network identity boundary such as a tailnet.
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
4. **fleetd's spawn is async** (`resolve_bin` runs in `spawn_blocking`), as is
   the daemon's `HarnessExecutor` seam.

## Accepted v1 limits (do not build these here)

- A fleetd restart kills its children. Process-adoption tricks are not worth
  it for a binary whose whole value is that it rarely changes.
- No priority lanes, no capability brokering, no policy re-derivation, no
  persistence beyond the children and their event logs.
- Terminal-session GC is ack-driven only: a session is forgotten when it has
  exited AND the daemon has acknowledged through its last seq.
- One daemon selects one fleetd endpoint. Routing among multiple fleetd
  instances remains outside this v1 supervisor contract.

## Split, not hand-rolled

The connection is split with `bro_rpc::NegotiatedIo::split`, which carries the
negotiated `ConnectionBinding` onto both halves so generation fencing still
runs on every frame. This replaced a hand-rolled re-framing here (splitting the
`UnixStream` and rebuilding two `FramedIo`s with `validate_envelope` called
manually). The daemon-side client needs the identical shape, and two hand-rolled
copies of a fencing-critical primitive is one too many. Do not reintroduce a
local version.

## Wire note

The transport is `bro-rpc`'s length-prefixed bounded framing, which bans
newline framing outright (see `crates/bro-rpc/AGENTS.md`). The same framed
protocol runs over the state-local Unix socket and the explicitly configured
TCP endpoint. The Unix path derives its token and socket from the state dir;
the remote client requires a pre-existing explicit token file and never
creates or auto-starts anything.

## The daemon side of this contract

`src/orchestration/fleetd_client.rs` is the client. Three things there are
paired with invariants above and must not drift:

- **One endpoint and one connection, sessions multiplexed over it.** Because
  fleetd serves a single owner connection, the daemon must not open a second:
  a second dial
  fences the first out and silently strands whatever the first was relaying.
  A connection actor owns the socket and fans messages to per-session handles.
- **The daemon advances its cursor AFTER ingest, not on receipt.** That is what
  makes `ReplayFrom` exact. The seq the client tracks per session is only for
  `EventAck`, which is advisory and gates nothing but fleetd's GC.
- **The spec's `session_id` is the supervision key**, and the daemon
  substitutes the task id when a dispatch has no provider session yet (several
  paths still pass the literal `"pending"`). Two concurrent pending dispatches
  would otherwise collide on this registry, on the daemon's slot map, and on
  the event-log filename.
- **Remote worker roots are mandatory and absolute.** The daemon state root,
  worker HOME, and worker BRO_HOME are three different localities. Provider
  credential paths and harness replay logs use the worker roots; task stores,
  transcript mirrors, and corpus indexing use the daemon roots.

Deliberate delta from the "accepted v1 limits" above: for the state-local Unix
endpoint only, the daemon starts fleetd itself, detached, when the socket is
absent at first need. A remote endpoint never autostarts and never falls back
to local execution. fleetd still never adopts processes and still kills its
children on its own restart.
