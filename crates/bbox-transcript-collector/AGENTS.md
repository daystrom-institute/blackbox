# Native transcript host collector

- Explicit configured roots only. Never infer roots from HOME or daemon paths,
  follow discovered symlinks, or mix host paths into published locator identity.
- The source/account pair names one configured root; duplicate identities are
  refused. Raw JSONL stays source-owned and is parsed into corpus projections
  only on the daemon.
- Capture and upload use bounded 1MiB buffers. Metadata is streamed with unknown
  fields ignored, not loaded as arbitrary JSON message bodies. Check source
  length and modification stamp across capture and upload; content digests
  remain the final authority.
- Only the complete final JSONL prefix is published. Rewrites and shrinks are
  normal new snapshots. Local mtime or cursor state never proves publication:
  reconcile the current server generation and verify the durable receipt.
- Producer secrets come from token files, travel only in bearer headers, and
  are never Debug fields. Require HTTPS except loopback fixtures and disable
  redirects so a credential cannot cross authorities.
- Capture failures, deferred incomplete files, and unchanged/published streams
  have separate counts. One failed stream must not prevent backfill of others.

- Begin and complete each scan with authenticated contact evidence. Recording
  scan completion can fail after snapshots were admitted; report that partial
  effect. Transport errors expose only bounded machine codes, never proxy bodies.
