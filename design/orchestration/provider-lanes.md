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
| `Codex` | `codex` | one `codex app-server` child per dispatch, driven by `bro-worker` |
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
[--disallowedTools <a,b>]
```

`code_mode`, `edit_discipline` and `service_tier` have no supported mapping
on this lane. Dispatch and resume refuse explicit values before spawning.

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
deny filters use those names on `--disallowedTools`. Global tool allowlists
are rejected before launch: `--allowedTools` grants permissions rather than
restricting availability, and `--tools` only selects built-in tools.

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
The execution host opens and validates the durable log before launching a worker.
It persists and sequences each durable event before relay. An unreadable or
incomplete log refuses launch; a write or relay failure stops the worker and
reports an error. Owner disconnection does not stop execution or logging.
Harness-only system events (`context_pressure`, `harness_shell_sessions`,
`instruction_read_timeout`, `harness_milestone`, `compaction_threshold`) do
not exist on this lane and are not emulated; consumers treat them as absent.

## 2. Codex lane

The daemon composes `CodexSessionConfig` and `SessionCommand` values. Both
execution hosts link the same `bro-worker` library and launch `codex app-server`
directly (`CODEX_BIN` selects the executable). The host owns process lifetime;
the adapter owns JSON-RPC, session state, normalized events, and the event log.
`bro-protocol` contains only wire data and has no runtime dependency.

| Session operation | Native app-server operation |
|---|---|
| First input | `thread/start` or `thread/resume`, then `turn/start` |
| Input during a turn | `turn/steer` with the expected turn id |
| Input between turns | `turn/start` |
| Input during interruption | Queue for the next turn |
| Compact | `thread/compact/start` |
| Interrupt | `turn/interrupt`, acknowledge only after RPC success |
| Set model | Carry the model on the next turn |
| End input | Drain the active turn, close app-server input, bound shutdown |

Model, effort, service tier, output schema and developer instructions use native
RPC fields. A fresh unpinned model uses the provider catalog default; resume
inherits the native thread settings unless explicitly overridden. The account
selects `CODEX_HOME`; app-server owns authentication, model execution, and tools.
Headless dispatch uses `approvalPolicy: never` and `danger-full-access`.
Unexpected approval requests are declined. RPC requests have bounded deadlines.

MCP definitions travel in typed dispatch settings and become thread config
overrides, never command-line secrets. Effective configuration is read before
opening a thread, and every undeclared MCP server is disabled. Failed or malformed
config reads abort dispatch. HTTP bearer references use `bearer_token_env_var`;
other header references use `env_http_headers`. Exact MCP denials become native
`disabled_tools`. Global allowlists, native tool denials, unsupported transports,
harness execution settings and provider-default suppression fail explicitly.

Only notifications belonging to the active thread and turn can complete it.
The adapter normalizes messages, tool activity, usage and results for daemon
observation. Durable events receive monotonic sequence numbers before relay and
use the same `{ts, event}` log as other workers; transient deltas are not replayed.
The fresh task log is linked to its app-server thread name when init arrives,
retaining the pinned recovery path. Resume appends under the thread name and
continues its sequence. The indexer skips the task alias and labels the canonical
session `codex-dispatch`.

Fleet protocol version 2 is required for typed settings and explicit end of input.

## 3. Harness lane

The `bro-harness` lane keeps its argv, environment, flat capability aliases,
tool placement and event log until the codex lane replaces `brodex`. It gains
nothing new.

## 4. Non-goals

- No daemon-side model loop, tool registry or compaction policy for any lane.
- No emulation of harness-only events on a vendor CLI lane.
- No reading of a config dir's settings file by the daemon.
