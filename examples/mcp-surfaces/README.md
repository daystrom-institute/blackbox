# MCP Tool Surfaces — Default Packet

`routing.json` is the default `mcp-surface/routing` packet shipped with blackbox.
It defines four named surfaces plus a catchall deny. Install it once globally;
clients select which surface they want via the `?surface=<id>` query parameter
on the daemon's `/mcp` endpoint.

```bash
bbox_compile path=examples/mcp-surfaces/routing.json scope=global
```

## Surfaces

| Surface | Audience | What's visible |
|---|---|---|
| `default` | external clients (Claude Code, Codex CLI) doing day-to-day work | transcripts, agentic graph, knowledge r/w, threads, notes, inbox, code-nav, refactor (plan/run/status — not apply), bro core dispatch (exec/resume/status/cancel/dashboard/team/brofile/agents), `bbox_mcp_surface` |
| `readonly` | reviewers, evaluators, observer agents | search/cite/inspect/find_paths/bundle, knowledge read, thread/note/inbox read, code-nav read, council observation, bro status/dashboard. **No writes, no dispatch, no admin.** |
| `agent-internal` | dispatched bros inside a workflow or arc | superset of `default` plus whiteboards, councils, when_all/any, signals, broadcast, arc_*. `bro_exec`/`resume`/`cancel` denied as a backstop to the mechanical recursion guard. |
| `ops` | operator/admin sessions | full daemon access — setup tools (slack/webhook/poller/cron), workflow authoring, lifecycle (render/absorb/bootstrap/lint/review), index/embedding admin, artifact management, provenance, destructive refactor. |

## URL examples

```
http://127.0.0.1:7264/mcp                          # → "default"
http://127.0.0.1:7264/mcp?surface=readonly         # → "readonly"
http://127.0.0.1:7264/mcp?surface=agent-internal   # → "agent-internal"
http://127.0.0.1:7264/mcp?surface=ops              # → "ops"
http://127.0.0.1:7264/mcp?surface=anything-else    # → init fails: deny catchall
```

## Provider config

Register one alias per surface in your provider's MCP config:

```jsonc
// ~/.claude/.claude.json
{
  "mcpServers": {
    "blackbox":          { "type": "http", "url": "http://127.0.0.1:7264/mcp" },
    "blackbox-readonly": { "type": "http", "url": "http://127.0.0.1:7264/mcp?surface=readonly" },
    "blackbox-ops":      { "type": "http", "url": "http://127.0.0.1:7264/mcp?surface=ops" }
  }
}
```

Or use `bro_mcp action=add` with the `surface` field — the daemon appends the
query string for you and fans the registration out to every installed provider:

```text
bro_mcp action=add name=blackbox-readonly surface=readonly scope=global
```

## Customizing

Copy `routing.json` to a project-local location, edit the rules, and compile
with `scope=project project=<path>`. Project-scoped packets override the global
packet for requests bound to that project.

To debug a surface decision without hitting the wire:

```text
bbox_mcp_surface action=replay surface=readonly
bbox_mcp_surface action=describe surface=readonly
bbox_mcp_surface action=list
```

## Design notes

- **Disallow wins over allow.** The `agent-internal` surface uses an empty
  allow list and a long disallow list — same passthrough-with-exclusions
  pattern as `default`, plus whiteboards/councils stay visible by *not* being
  listed in either column.
- **`ops` keeps `refactor_apply`.** Operator sessions are explicitly in
  destructive territory; lower-privilege surfaces hide it.
- **Recursion guard is mechanical and orthogonal.** Even if a bro got an
  `agent-internal` surface that allowed `bro_exec`, the dispatch-time recursion
  guard at the provider argv layer would still strip it. The surface filter
  here is belt-and-suspenders.
- **Catchall deny fails initialize.** Selecting an unknown surface causes
  `initialize` to return an MCP error rather than silently returning an empty
  tool catalog (which clients would treat as a valid but tool-less server).
