# bro-rpc - transport above the pure contract bottom

- This crate owns framing and typed connection mechanics. Wire DTOs belong in
  `bro-protocol`; capability behavior belongs in `bro-capabilities`.
- A frame is a bounded big-endian 32-bit length followed by UTF-8 JSON. Never
  add newline framing or an unbounded allocation path.
- Handshake rejection is sent as a typed peer-visible message before the local
  error is returned. Version negotiation never silently downgrades outside the
  advertised intersection.
- The crate must not depend on any daemon, harness, fleet, blackops, corpus, or
  store implementation.
- `ServiceToken` is the shared same-host HTTP bearer primitive. Token files are
  regular, non-symlink, owner-only files under a private directory; values are
  sensitive headers and must stay redacted in diagnostics.
