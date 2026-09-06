# Performance review in the caller

Daemon pathology workflows and atom wrappers are retired. Gather runtime evidence in the caller harness, dispatch focused reviewers and explicitly aggregate their findings.

Blackbox retains bro execution, resume, cancellation, status and waits. The
caller owns sequencing, gates, retries, schedules and integrations, using its
own code and harness tools. No daemon workflow is needed to compose bro calls.

See [bro runtime](bro-runtime.md) for the surviving execution primitives and
[the retirement contract](../design/orchestration/bro-execution-boundary-and-retirement.md)
for history and ownership guarantees.
