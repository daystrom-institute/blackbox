# Observation journal

The event hub persists and broadcasts observations. It does not evaluate reactions, create external identities, enqueue deliveries or dispatch work. Journal retention runs as independent mechanical maintenance. Legacy journals remain decodable; old reaction and delivery stores remain inert. Event and reaction MCP tools are retired.

Blackbox retains bro execution, resume, cancellation, status and waits. The
caller owns sequencing, gates, retries, schedules and integrations, using its
own code and harness tools. No daemon workflow is needed to compose bro calls.

See [bro runtime](bro-runtime.md) for the surviving execution primitives and
[the retirement contract](../design/orchestration/bro-execution-boundary-and-retirement.md)
for history and ownership guarantees.
