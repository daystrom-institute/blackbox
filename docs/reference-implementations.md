# Historical application examples

Keystone, Sastquatch, Slack and whiteboard applications depended on the retired workflow and reaction engines. Their runnable manifests and installers have been removed. Prior revisions preserve their design history; they are not current installation recipes.

Blackbox retains bro execution, resume, cancellation, status and waits. The
caller owns sequencing, gates, retries, schedules and integrations, using its
own code and harness tools. No daemon workflow is needed to compose bro calls.

See [bro runtime](bro-runtime.md) for the surviving execution primitives and
[the retirement contract](../design/orchestration/bro-execution-boundary-and-retirement.md)
for history and ownership guarantees.
