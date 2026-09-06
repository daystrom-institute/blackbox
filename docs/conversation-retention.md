# Retaining conversation history after collection stops

Conversation read authorization and producer ingest authorization are separate.
Existing conversation grants continue to authorize reads for compatibility.
To remove a collector's write access while retaining its already landed history,
configure an explicit retained read enrollment:

```toml
[source_connectors]
enabled = false

[[source_connectors.retained_conversations]]
connector_source_id = "csrc_0000000000000001"
connector_kind = "slack"
workspace_id = "WORKSPACE_EXAMPLE"
remote_authority = "workspace.example"
```

Use the source's existing catalog id and kind, its exact stored workspace id,
and the authority used to derive its permalinks. Retention cannot onboard a
new source. Missing catalog authority, a missing landing store or workspace
binding, or an identity mismatch refuses daemon startup with a specific error.
Duplicate retained ids and overlap with any producer scope grant also refuse.

Remove that source's scope from its producer in the same config change. Remove
the producer row if it has no remaining grants. Keep `source_connectors.enabled`
true when other producers still need ingest; false disables the ingest family.
Retained rows accept no token, producer id, or ingest profile and never enter
the producer authentication table. Removing a grant denies writes even while
its retained history remains readable. These config changes apply at startup.

Retained enrollment reads the latest stored evidence, including stored edits,
redactions and membership exclusions. It is not an immutable snapshot and does
not imply recent collection, current remote membership, or collector liveness.
Original message and observation timestamps keep their existing meaning.
Search and transcript tools continue to use corpus coordinates and permalinks.

To revoke reads, remove both the live conversation grant and any retained row
for the source. At the next startup it leaves the adapter's enrollment set;
the next reindex purges its indexed documents unless another enrolled observer
still covers the same channel. Direct transcript reads require current
enrollment. Stopping a collector alone is not a revocation signal.

The catalog and conversation landing store remain the durable history. This
configuration does not delete them, discover additional sources from disk, or
provide a filesystem fallback for transcript callers.
