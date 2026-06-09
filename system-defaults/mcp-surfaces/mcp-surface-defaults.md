# MCP Tool Surfaces — Default Packet

`routing.json` is the default `mcp-surface/routing` packet shipped with blackbox.
It defines five named surfaces plus a catchall deny. Install it once globally;
clients select which surface they want via the `?surface=<id>` query parameter
on the daemon's `/mcp` endpoint.

```bash
bbox_compile path=system-defaults/mcp-surfaces/routing.json scope=global
```

## Surfaces

| Surface | Audience | What's visible |
|---|---|---|
| `default` | constrained day-to-day clients | transcripts, agentic graph, knowledge r/w, threads, notes, inbox, code-nav, atom discovery/invoke/status/resume/delegate, refactor plan/run/status/apply, bro core dispatch (exec/resume/status/cancel/dashboard/team/brofile/agents), `bbox_mcp_surface` |
| `interactive` | day-to-day interactive coding sessions | default-permissive working set: transcripts, graph, knowledge, threads, notes, inbox, code-nav/refactor, packets, artifacts, project listing, macro tools, identity read, and core bro operations. Hides Badgey, Slack/IRC, allocator/agent/atom catalogs, cron/workflow/arc/webhook/poller/signal tools, councils, whiteboards, workspace tools, system events, and reactions. |
| `readonly` | reviewers, evaluators, observer agents | search/cite/inspect/find_paths/bundle, knowledge read, thread/note/inbox read, code-nav read, atom discovery, council observation, bro status/dashboard. **No writes, no dispatch, no admin.** |
| `agent-internal` | dispatched bros inside a workflow or arc | superset of `default` plus whiteboards, councils, when_all/any, signals, broadcast, arc_*. `bro_exec`/`resume`/`cancel` denied as a backstop to the mechanical recursion guard; atom invocation remains policy-gated by atom composition/effect limits. |
| `ops` | operator/admin sessions | full daemon access — setup tools (slack/webhook/poller/cron), workflow authoring, lifecycle (render/absorb/bootstrap/lint/review), index/embedding admin, artifact management, provenance, destructive refactor. |

## URL examples

```
http://127.0.0.1:7264/mcp                          # → "default"
http://127.0.0.1:7264/mcp?surface=readonly         # → "readonly"
http://127.0.0.1:7264/mcp?surface=interactive      # → "interactive"
http://127.0.0.1:7264/mcp?surface=agent-internal   # → "agent-internal"
http://127.0.0.1:7264/mcp?surface=ops              # → "ops"
http://127.0.0.1:7264/mcp?surface=anything-else    # → init fails: deny catchall
```

## Provider config

For normal interactive use, register a single canonical `blackbox` MCP entry
and point it at the `interactive` surface. Add separate aliases only for
intentionally different contexts such as read-only reviewers, workflow actors,
or admin/operator sessions that need `ops`.

```jsonc
// ~/.claude/.claude.json
{
  "mcpServers": {
    "blackbox": { "type": "http", "url": "http://127.0.0.1:7264/mcp?surface=interactive" }
  }
}
```

```toml
# ~/.codex/config.toml
[mcp_servers.blackbox]
url = "http://127.0.0.1:7264/mcp?surface=interactive"
```

Do not register both `blackbox` and `blackbox-ops` by default. The duplicate
catalog adds context noise, and dispatch-time recursion filters are anchored on
the canonical `blackbox` server name. Temporarily switch the canonical entry to
`ops` only for setup, lifecycle, or admin work.

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

- **Disallow wins over allow.** The `default`, `interactive`, and
  `agent-internal` surfaces use an empty allow list plus a disallow list. This
  keeps the packet default-permissive for normal tools while hiding whole
  coordination/admin clusters from narrower surfaces.
- **`default` keeps `refactor_apply`.** Applying a reviewed refactor plan is a
  day-to-day code-editing operation, not an ops-only daemon administration
  surface. `ops` remains the full passthrough surface for setup, lifecycle,
  indexing, provenance, artifact administration, workflow internals, and other
  operator-only tools.
- **Recursion guard is mechanical and orthogonal.** Even if a bro got an
  `agent-internal` surface that allowed `bro_exec`, the dispatch-time recursion
  guard at the provider argv layer would still strip it. The surface filter
  here is belt-and-suspenders.
- **Catchall deny fails initialize.** Selecting an unknown surface causes
  `initialize` to return an MCP error rather than silently returning an empty
  tool catalog (which clients would treat as a valid but tool-less server).
