# Retired Slack integration

Blackbox no longer ships a Slack bridge, Slack collector or Slack writing tools. Already indexed conversation evidence remains available through bbox_search, bbox_context and bbox_messages. Retirement preserves historical source ownership; it does not enroll new conversation producers.

Blackbox retains bro execution, resume, cancellation, status and waits. The
caller owns sequencing, gates, retries, schedules and integrations, using its
own code and harness tools. No daemon workflow is needed to compose bro calls.

See [bro runtime](bro-runtime.md) for the surviving execution primitives and
[the retirement contract](../design/orchestration/bro-execution-boundary-and-retirement.md)
for history and ownership guarantees.
