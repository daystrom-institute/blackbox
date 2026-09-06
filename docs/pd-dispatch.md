# Phase decomposition in the caller

Daemon phase-decomposition workflows are retired. Role prompts and deterministic analysis helpers can support caller-owned decomposition, but there is no installed workflow to invoke.

Blackbox retains bro execution, resume, cancellation, status and waits. The
caller owns sequencing, gates, retries, schedules and integrations, using its
own code and harness tools. No daemon workflow is needed to compose bro calls.

See [bro runtime](bro-runtime.md) for the surviving execution primitives and
[the retirement contract](../design/orchestration/bro-execution-boundary-and-retirement.md)
for history and ownership guarantees.
