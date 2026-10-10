# bro-codex - the codex lane's worker

`bro-codex` is the child fleetd (or the daemon's local executor) spawns for a
`codex` dispatch. It owns one `codex app-server` child for the life of the
dispatch and translates between the claude CLI's headless stream-json
contract on its own stdio and the app-server's JSON-RPC on the child's stdio.
Design: `design/orchestration/provider-lanes.md` sections 1 and 2.

## Invariants

- **No model loop, no tools, no provider config.** The app-server owns the
  model loop, tool execution, sandbox, compaction, config and transcript.
  The shim never reads `CODEX_HOME` or a config file; what it knows of the
  app-server's config it asks the app-server (`config/read`, only to turn
  off the config's own MCP servers under `--strict-mcp-config`).
- **One app-server per dispatch.** Spawned at startup from `CODEX_BIN`
  (default `codex`) with the inherited environment, so `CODEX_HOME` selects
  the account. Its stdin closes when the session ends; it is killed only if
  it does not exit within the grace period.
- **The stdio contract is the claude lane's.** Input is stream-json `user`
  envelopes and claude-shaped `control_request`s (`interrupt`, `set_model`;
  anything else gets an error `control_response`). Output is one complete
  JSON object per line in the stream-json envelope, every line carrying the
  thread id as `session_id`: `system`/`init` first, replayed user envelopes
  under `--replay-user-messages`, `stream_event` text deltas under
  `--include-partial-messages`, `assistant` text, thinking and `tool_use`
  blocks, `user` `tool_result` blocks, and one `result` per turn with usage.
- **The thread id is the session id.** The app-server mints it, so
  `--session-id` is accepted and ignored; `--resume <thread>` continues a
  thread. The daemon starts a fresh codex task `pending` and adopts the id
  from `init`.
- **Exit at end of input.** On stdin EOF the shim finishes the running turn
  and any input it already read, closes the app-server and exits 0. It exits
  non-zero only when the app-server cannot be started or the `initialize`
  handshake fails, with the reason on stderr. An app-server that dies
  mid-session ends the running turn with an error `result`.
- **No session log.** The executor that owns the shim's stdout writes the
  `BRO_HOME` session log (`supervisor_writes_event_log`); the shim writes
  nothing under `BRO_HOME`.
- **Unknown argv is a warning, never a failure.** The daemon composes one
  argv shape for the vendor CLI lanes; flags without a codex meaning are
  reported on stderr and ignored.
- **Secrets stay out of argv.** `--mcp-config` servers become `-c
  mcp_servers.<name>=...` app-server overrides; a header whose value is
  exactly `${VAR}` becomes an `env_http_headers` entry (or
  `bearer_token_env_var` for `Authorization: Bearer ${VAR}`), so the value is
  read from the environment, never passed on a command line.

## Mapping notes

- A user envelope during a turn is a `turn/steer` against the running turn
  id; one that arrives while the turn is being interrupted, or when the steer
  is refused because the turn just ended, starts the next turn instead.
  Consecutive queued user envelopes start one turn together.
- `/compact` as a user envelope is `thread/compact/start`, reported as a turn.
- `--append-system-prompt` is the thread's `developerInstructions`; a
  non-empty `--system-prompt` is its `baseInstructions`. `--json-schema` is
  every turn's `outputSchema`; `--model` and `--effort` ride the thread and
  each turn, and `set_model` changes the model of the next turn.
- `--allowedTools` / `--disallowedTools` entries named `mcp__<server>__<tool>`
  become that server's `enabled_tools` / `disabled_tools`; other names and
  globs are warned about and ignored.
- Usage is reported in the Anthropic-native shape the daemon parses:
  `input_tokens` is fresh input, cache reads and writes ride their own
  fields. Rate-limit and overload failures carry `apiErrorStatus` 429 / 529.
- The server's approval and user-input requests are declined: dispatches run
  with `approvalPolicy: never`, so they are not expected.

## Tests

`src/tests.rs` drives the session over real pipes against a scripted
in-process app-server. The daemon side is covered by the codex cases in
`src/orchestration/providers/tests.rs`, `src/orchestration/mod.rs` and the
fleetd acceptance suite (`src/orchestration/dispatch_acceptance.rs`).
