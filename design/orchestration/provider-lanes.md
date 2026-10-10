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
| `ClaudeCli` | `claude`, `glm`, `deepseek`, `minimax`, `kimi` | one `claude -p` child per dispatch |
| `Codex` | `codex` | one `codex app-server` child per provider, one thread per dispatch |
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
from env the daemon lifts out of it:

- `claude` default account: the CLI's own `~/.claude`; `account<N>` selects
  `~/.claude-account<N>` through `CLAUDE_CONFIG_DIR`.
- `glm`, `deepseek`, `minimax`, `kimi`: `CLAUDE_CONFIG_DIR` names
  `~/.claude-zai`, `~/.claude-ds`, `~/.claude-mm`, `~/.claude-k`.

The config dir resolves against the execution home, so an off-host fleetd
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

The indexer labels a dispatched Claude session `claude-dispatch`, distinct
from the interactive `claude` source read from the CLI's own project
transcripts.

## 2. Codex lane

One `codex app-server` child per provider, JSON-RPC over stdio, hosts every
dispatch as a thread. The executor keeps the child, hands out per-thread
handles, and maps the session commands onto `turn/start`, `turn/steer`,
`turn/interrupt`, `thread/start` and `thread/resume`; `item/*` notifications
translate into the stream-json envelope the daemon already ingests. The
account selects `CODEX_HOME`.

## 3. Harness lane

The `bro-harness` lane keeps its argv, environment, flat capability aliases,
tool placement and event log until the codex lane replaces `brodex`. It gains
nothing new.

## 4. Non-goals

- No daemon-side model loop, tool registry or compaction policy for any lane.
- No emulation of harness-only events on a vendor CLI lane.
- No reading of a config dir's settings file by the daemon.
