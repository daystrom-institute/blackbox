# Execution-host adapters

`bro-worker` owns native provider sessions and durable event logging for both
fleetd and the local executor. It depends on wire types and provider identity,
never daemon, corpus, harness, or V8 crates.

Codex settings and controls are typed at the dispatch boundary. Translate them
into app-server RPC here. Do not introduce a CLI compatibility executable.
Validate restrictions before spawn, fail closed on effective config errors,
acknowledge controls only after native acceptance, and scope completion to the
active thread and turn. Bound RPC and shutdown waits.

Agy settings and controls are typed the same way and translated into Agent
Client Protocol requests. The server holds a prompt request open for its whole
turn, so send it without a reply deadline. A prompt sent during a turn joins
it, and the turn ends only when every prompt sent for it has been answered.
Drain session updates before reading a prompt answer, so a turn's last updates
precede its result. Resolve worker-variable references in MCP definitions in
the adapter, never into argv or a log.

A session's outcome is settled when its adapter returns, which is after the
last turn's result is durable. A worker process that then outlives the exit
bound is killed and noted on stderr with the child processes it still had,
and the session keeps its successful exit code. Only an adapter failure or
the worker's own exit code makes a session fail.

Open and validate durable logs before spawning. Refuse damaged tails and propagate
persistence errors to supervision so the worker stops. Persist durable events
with sequence numbers before relaying them. Preserve the
pinned recovery path when adopting a provider session id. Tests use isolated
scripted server pipes and temporary logs, never real provider homes.
