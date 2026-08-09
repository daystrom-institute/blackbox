# bbox-knowledge-source-client - bound workspace producer

- This crate owns stable, bounded provisional capture and resumable upload for
  one daemon-bound workspace. It never accepts a caller-selected project id and
  never sends paths, operations, overlay conclusions, or corpus records.
- Capture is local-first and nofollow. It pins accepted context from the
  authenticated daemon, captures exact Git ancestry and baseline bytes, reads
  both working lanes twice around the transaction/head checks, and refuses any
  movement.
- The binding token stays in the redacted protocol newtype and is used only as
  the private request header. Never log it, serialize it into artifacts, place
  it in URLs, or pass it to child processes.
- HTTP redirects are disabled. HTTPS is required except for loopback HTTP.
- This is a producer/client layer. It may depend on the pure source contract
  and corpus Git/identity leaves, but never on blackbox, stores, indexes,
  embeddings, vectors, or MCP server code.
