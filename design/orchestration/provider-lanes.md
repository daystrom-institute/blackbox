---
title: "Provider lanes"
kind: design
lifecycle: partial
corpus: blackbox-design
topic:
  - orchestration
  - providers
  - process-isolation
brief: "The dispatch plane runs vendor CLIs: the claude CLI lane for Claude and the Anthropic-compatible endpoints, the codex app-server lane for Codex. The daemon composes argv, environment, controls and event handling per lane."
---

# Provider lanes

## 0. Decision

Every dispatchable provider belongs to exactly one lane, and the lane decides
how the daemon talks to the worker. Providers on one lane differ only in
credentials, endpoint and model catalog. `Provider::lane()` in `bro-core` is
the single authority; daemon code matches on the lane, never on provider
lists.

| Lane | Providers | Worker |
|---|---|---|
| `ClaudeCli` | `glm`, `deepseek`, `minimax`, `kimi` | one `claude -p` child per dispatch |
| `Codex` | `codex` | one `bro-codex` shim child per dispatch, fronting its own `codex app-server` |
| `Harness` | `brodex`, `vibebh` (transitional, retiring) | one `bro-harness` child per dispatch |
| `Workflow` | `workflow` | daemon-internal, no child |

The dispatch plane owns no model loop, tool runtime, compaction policy or
transcript format of its own. Those belong to the vendor CLIs.

## 1. Claude CLI lane

### Process

The daemon spawns `claude` (or `CLAUDE_BIN`) with:

```text
-p --input-format stream-json --output-format stream-json --verbose
--include-partial-messages --replay-user-messages --dangerously-skip-permissions
[--session-id <uuid> | --resume <uuid>] [--model <id>] [--effort <level>]
[--append-system-prompt <dispatch context>] [--system-prompt ""]
[--json-schema <schema>] [--mcp-config <json> --strict-mcp-config]
[--allowedTools <a,b>] [--disallowedTools <a,b>]
```

The initial prompt never rides argv. It is the first stream-json `user`
envelope on stdin, followed by later turns and controls. The session id the
daemon mints is a UUID, which is what `--session-id` requires.

### Credentials and configuration

Endpoint, credentials and model slots come from a `claude` config dir, never
from env the daemon lifts out of it: `CLAUDE_CONFIG_DIR` names
`~/.claude-zai`, `~/.claude-ds`, `~/.claude-mm` or `~/.claude-k`. Anthropic's
own models are not a dispatch provider; the operator's `~/.claude` is never a
worker's config dir. The config dir resolves against the execution home, so an off-host fleetd
reads its own worker-local dirs. Hooks, plugins and settings in that dir apply
to the dispatched child exactly as they apply to a terminal session started
with the same dir.

The non-secret project build env (`fleet.json` `project_dispatch.env`) is
placed in the child's process env: the CLI's shell children inherit it. The
daemon service env is removed from the child (`env_unset`); the CLI has no
grandchild scrub, so the daemon MCP bearer is visible to the agent's own
shell children, as it is to the agent itself.

### Daemon capabilities

The daemon MCP server is injected through `--mcp-config` as an HTTP server in
the CLI's `{"mcpServers":{…}}` shape, together with the fleet.json servers,
and `--strict-mcp-config` keeps the config dir's own MCP servers out of the
dispatch. Header secrets are `${VAR}` references expanded by the CLI from the
child env; argv never carries a bearer. There are no flat capability aliases:
tools are reached by their qualified `mcp__<server>__<tool>` names, and the
allow/deny filters use those names on `--allowedTools` / `--disallowedTools`.

### Dispatch context

Persona and scope have no typed slot on the CLI. The daemon renders them as
one `--append-system-prompt` text: persona verbatim, then a `## Dispatch
scope` block listing the scope ids in their fixed order. The same text rides
fresh dispatch and resume. `--system-prompt ""` carries provider-defaults
suppression.

### Controls

| Session command | Wire |
|---|---|
| user turn, steer | `{"type":"user","message":{"role":"user","content":[{"type":"text","text":…}]}}` |
| interrupt | `{"type":"control_request","request_id":…,"request":{"subtype":"interrupt"}}` |
| interrupt with redirect | the interrupt, then the redirect as the next user envelope |
| set model | `{"type":"control_request","request_id":…,"request":{"subtype":"set_model","model":…}}` |
| compact | the `/compact` slash command as a user envelope |

The CLI answers controls with `control_response` events carrying the request
id; the daemon ingests them like any other event.

### Turn end and exit

The CLI runs until stdin closes. After a turn's `result` event the daemon
drops the task's control lane, which closes stdin; the child finishes any
input it already read and exits, and process exit publishes terminal state.
This is the exit-when-idle contract. A steer that arrives after the result is
refused and goes through resume, which starts a new child with `--resume`.

### Session log

The CLI writes nothing under `BRO_HOME`. The daemon mirrors the child's
stdout into `$BRO_HOME/harness-sessions/<session>.events.jsonl` as `{ts,
event}` records, skipping `stream_event` partials, on every locality. That
file remains the transcript the cockpit tails, the indexer ingests and
closeout reads. Replayed user envelopes are the record of operator turns.
Harness-only system events (`context_pressure`, `harness_shell_sessions`,
`instruction_read_timeout`, `harness_milestone`, `compaction_threshold`) do
not exist on this lane and are not emulated; consumers treat them as absent.

## 2. Codex lane

The codex lane keeps the claude lane's process contract: one child per
dispatch, user envelopes and claude-shaped `control_request`s on stdin, the
stream-json envelope on stdout, exit at end of input, session log written by
the executor. The child is the `bro-codex` shim, a checkout-host binary that
owns one `codex app-server` child (JSON-RPC over stdio) for the life of the
dispatch and translates between the two protocols:

| stdin envelope | app-server |
|---|---|
| first user envelope | `thread/start` (or `thread/resume <id>` under `--resume`), then `turn/start` |
| user envelope during a turn | `turn/steer` with the current turn id |
| user envelope between turns | `turn/start` |
| user envelope while the turn is being interrupted | queued; starts the next turn |
| `/compact` user envelope | `thread/compact/start`, reported as a turn |
| `control_request` interrupt | `turn/interrupt` |
| `control_request` set_model | the next `turn/start` carries the model |
| end of input | wait for the running turn, then close the app-server's stdin and exit |

| app-server message | stdout envelope |
|---|---|
| `thread/start` or `thread/resume` result | `system`/`init` with the thread id as `session_id` |
| `item/agentMessage/delta` | `stream_event` text delta |
| `item/completed` agentMessage, reasoning | `assistant` text and thinking blocks |
| `item/started` and `item/completed` commandExecution, fileChange, mcpToolCall | `assistant` tool_use and `user` tool_result blocks |
| `turn/completed` | `result` with usage, `is_error` on a failed turn |

The shim performs no model loop, runs no tools and reads no provider config:
the app-server owns all of that, with `approvalPolicy: never` and the sandbox
the dispatch names (`danger-full-access` under
`--dangerously-skip-permissions`). The app-server has no shutdown request;
end of its input is its shutdown. The account selects `CODEX_HOME`, and with
no model pinned the app-server's own config chooses the model.

`--mcp-config` servers become `-c mcp_servers.<name>=…` overrides on the
app-server command line, with `${VAR}` header secrets carried as
`env_http_headers` / `bearer_token_env_var` names rather than values, and
`mcp__<server>__<tool>` filters as each server's `enabled_tools` /
`disabled_tools`. `--strict-mcp-config` asks the app-server for its effective
config (`config/read`) and disables every server the dispatch did not define
in the thread's config. `--append-system-prompt` is the thread's
`developerInstructions` and `--json-schema` each turn's `outputSchema`. The
lane has no provider-defaults suppression: `--system-prompt ""` has no
app-server equivalent that keeps codex's own tool instructions, so strict
suppression refuses the `codex` provider.

The thread id is the session id the daemon records and resumes with. The
app-server mints it, so a fresh codex dispatch starts its task `pending`,
passes no `--session-id`, and adopts the id from the shim's `init`; the
executor writes that dispatch's session log at the path pinned at spawn,
named by the task id, and a resume (`--resume <thread>`) writes under the
thread id. fleetd, the daemon and the session log otherwise see a worker
indistinguishable from a claude-lane child. The indexer labels a dispatched
Codex session `codex-dispatch`.

## 3. Harness lane

The `bro-harness` lane keeps its argv, environment, flat capability aliases,
tool placement and event log until the codex lane replaces `brodex`. It gains
nothing new.

## 4. Non-goals

- No daemon-side model loop, tool registry or compaction policy for any lane.
- No emulation of harness-only events on a vendor CLI lane.
- No reading of a config dir's settings file by the daemon.
