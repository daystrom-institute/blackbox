# Execution-host adapters

`bro-worker` owns native provider sessions and durable event logging for both
fleetd and the local executor. It depends on wire types and provider identity,
never daemon, corpus, harness, or V8 crates.

Codex settings and controls are typed at the dispatch boundary. Translate them
into app-server RPC here. Do not introduce a CLI compatibility executable.
Validate restrictions before spawn, fail closed on effective config errors,
acknowledge controls only after native acceptance, and scope completion to the
active thread and turn. Bound RPC and shutdown waits.

Open and validate durable logs before spawning. Refuse damaged tails and propagate
persistence errors to supervision so the worker stops. Persist durable events
with sequence numbers before relaying them. Preserve the
pinned recovery path when adopting a provider session id. Tests use isolated
scripted app-server pipes and temporary logs, never real provider homes.
