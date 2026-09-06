# Caller-owned orchestration

Workflow execution, authoring, scheduling and webhook/poller ingress are retired. Existing task records, threads and artifact receipts remain historical evidence. Startup does not resume checkpoints or choose subsequent work.

Blackbox retains bro execution, resume, cancellation, status and waits. The
caller owns sequencing, gates, retries, schedules and integrations, using its
own code and harness tools. No daemon workflow is needed to compose bro calls.

See [bro runtime](bro-runtime.md) for the surviving execution primitives and
[the retirement contract](../design/orchestration/bro-execution-boundary-and-retirement.md)
for history and ownership guarantees.
