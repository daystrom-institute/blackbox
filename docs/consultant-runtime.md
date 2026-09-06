# Retired consultant runtime

Persistent consultant instances, action journals and automatic proposals no longer execute. The caller can keep its own application state and use ordinary bro sessions for model turns.

Blackbox retains bro execution, resume, cancellation, status and waits. The
caller owns sequencing, gates, retries, schedules and integrations, using its
own code and harness tools. No daemon workflow is needed to compose bro calls.

See [bro runtime](bro-runtime.md) for the surviving execution primitives and
[the retirement contract](../design/orchestration/bro-execution-boundary-and-retirement.md)
for history and ownership guarantees.
