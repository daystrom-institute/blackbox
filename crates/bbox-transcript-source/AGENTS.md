# Native transcript wire

- This is a pure transport leaf shared by the daemon and host collector. No
  store, indexing, provider credentials, or host discovery belongs here.
- Identity is connector scope plus hashed source/account/relative stream id.
  Locators include the published generation, so callers cannot accidentally
  read a replacement source generation using stale coordinates.
- A snapshot is a complete JSONL prefix, represented by bounded content-addressed
  chunks. Its digest covers source identity and every chunk reference. Rewrites
  and shrinks are new generations, not exceptional append-cursor resets.
- Publication uses expected-generation CAS. Only a durable receipt confirms
  publication; it does not claim the index has processed that generation.

- Scan contact is producer evidence, separate from publication receipts and index
  completeness. A completed scan includes failures and deferred files; scan ids
  fence stale completions from overwriting a newer walk.
