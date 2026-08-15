# bro-rpc - transport substrate for bounded authenticated RPC channels

Built for design/daemon-runtime/locality-first-decomposition.md slice 5: the
daemon<->fleetd channel, state-local over Unix and explicitly remote over TCP.
Mined from
`salvage/satellite-arc-20260718`'s `crates/bro-rpc`, which built the same
mechanics for a worker<->fleet capability-RPC system; this crate keeps the
framing/auth/handshake substrate and drops everything that was specific to
that system (capability request/response correlation, lease grants,
reconnect-proof rotation, the priority-queued `RpcPeer` connection actor).

## Invariants

- **Bounded frames, never newline framing.** A frame is a big-endian u32
  length prefix followed by UTF-8 JSON
  (`DEFAULT_MAX_FRAME_BYTES = 8 MiB`). Oversize reads are rejected from the
  4-byte header alone, before the payload buffer is allocated. Oversize
  writes are caught by `LimitedWriter` aborting mid-serialize, so a
  streaming `Serialize` impl cannot force an unbounded intermediate buffer
  before the limit check runs. Do not add a newline-delimited or
  length-unbounded framing path to this crate, ever.
- **No daemon/harness/fleet/corpus/store dependency.** This crate must not
  depend on `blackbox`, `bro-harness`, or any `bbox-*` crate. It is pure
  transport: framing, auth primitives, and version/build negotiation. Wire
  DTOs for what daemon and fleetd actually say to each other (spawn specs,
  event/command enums) belong to fleetd's own crate when it lands, riding
  the payload-generic `Envelope<T>` this crate defines.
- **Dependency ceiling.** `bro-core`, `serde`, `serde_json`, `tokio`,
  `libc`, `thiserror`, plus whatever is already a small, already-in-use
  workspace dependency. Check other crates' `Cargo.toml` before adding
  anything new. `verify_peer_uid` delegates to
  `tokio::net::UnixStream::peer_cred` (which already implements the
  `SO_PEERCRED`/`LOCAL_PEERCRED`+`getpeereid` platform split for
  Linux/macOS) rather than hand-rolling duplicate unsafe FFI.
  `ServiceToken`'s random secret comes from reading `/dev/urandom` directly
  rather than pulling in a random-number crate.
- **Handshake rejection is always wire-visible before the local error
  returns.** `handshake::accept` writes a `Reject` frame (code +
  public-facing `message` + `supported_protocol_versions`) to the peer
  *before* returning `RpcError::HandshakeAuthorityRejected`. The two
  messages are deliberately different: `Reject.message` is safe to show the
  peer, `HandshakeAuthorityRejected.local_message` is for local logs only
  and may contain detail (parsed version lists, build strings) that
  shouldn't leak across the wire. Never collapse these into one string.
- **Version negotiation never silently downgrades outside the advertised
  intersection.** The server picks the max of `offered ∩ supported`; the
  client independently verifies the server's selection was one it actually
  offered (`SelectedProtocolNotOffered`) rather than trusting the peer's
  claim.
- **Generation fencing is not optional.** `validate_envelope` runs on both
  read and write of every `Envelope<T>`; a message from a stale
  `connection_generation` is `StaleGeneration`, not silently accepted. This
  is what lets a reconnecting daemon fence out a superseded connection
  without a separate liveness protocol.
- **`ServiceToken` hardening is load-bearing, not decorative.** Exactly 64
  lowercase hex characters; the backing file must be a regular,
  non-symlink, single-hardlink file owned by this process's uid with no
  group/other permission bits; comparison against a wire-received
  candidate is constant-time; `Debug` never prints the secret. Any change
  that weakens one of these checks needs an explicit reason, not just a
  passing test suite (the symlink/hardlink/broad-permission tests exist
  because those are real attack shapes against a shared secret file, not
  hypothetical ones).
- **`verify_peer_uid` is a second, independent check**, not a substitute
  for `ServiceToken`. It confirms the connected process is running as this
  process's own effective uid; it says nothing about which *service* is on
  the other end of that uid's sockets. Use both.
- **`ServiceTokenSet` is an ordered list of `ServiceToken`s for one
  producer**, used by the daemon's HTTP producer-grant lanes (code
  collection, source connectors) to stage an overlap-tolerant token
  rotation: add the new token as a later slot, redeploy the producer,
  remove the old slot once nothing verifies against it anymore. `verify`
  checks every slot without short-circuiting and returns the matching
  slot's index (never the token itself); callers use that index for
  matched-token observability (logs, rotation-status surfaces), never for
  branching on which credential value matched. `Debug` renders only the
  slot count, same redaction discipline as `ServiceToken` itself.

## Where things live

- `framing.rs` - `FramedIo<T>`, the length-prefix/JSON wire format,
  `LimitedWriter`.
- `auth.rs` - `ServiceToken`, `verify_peer_uid`.
- `handshake.rs` - `BuildIdentity`, `builds_compatible`, `Hello`/`Welcome`/
  `Reject`/`HandshakeMessage`, `connect`/`accept`.
- `envelope.rs` - `Envelope<T>`, `ConnectionBinding`, `NegotiatedIo<T>`,
  `validate_envelope`.
- `error.rs` - the `RpcError` taxonomy shared by all of the above.
