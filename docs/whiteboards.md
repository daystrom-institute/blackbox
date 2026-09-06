# Historical whiteboards

Whiteboard mutation, voting and phase transitions are retired. Discover retained whiteboard refs through graph retrieval, then request bbox_inspect_entity(entity_ref="whiteboard:<id>", property="body"). Follow property pagination. Both root and archive records load without being rewritten. Inspection grants no participant role: blind posts remain hidden, and annotations and votes retain their stored phase visibility. Project rename preserves unknown historical fields.

Blackbox retains bro execution, resume, cancellation, status and waits. The
caller owns sequencing, gates, retries, schedules and integrations, using its
own code and harness tools. No daemon workflow is needed to compose bro calls.

See [bro runtime](bro-runtime.md) for the surviving execution primitives and
[the retirement contract](../design/orchestration/bro-execution-boundary-and-retirement.md)
for history and ownership guarantees.
