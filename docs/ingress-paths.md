# Execution and corpus ingress

Application webhooks, pollers and crons are retired. External drivers use bro tools or the thin /control/* HTTP adapters. Native transcript and checkout-source collectors retain their authenticated transport contracts; they publish evidence rather than trigger application workflows.

Blackbox retains bro execution, resume, cancellation, status and waits. The
caller owns sequencing, gates, retries, schedules and integrations, using its
own code and harness tools. No daemon workflow is needed to compose bro calls.

See [bro runtime](bro-runtime.md) for the surviving execution primitives and
[the retirement contract](../design/orchestration/bro-execution-boundary-and-retirement.md)
for history and ownership guarantees.
