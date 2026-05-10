# MCP Surfaces

A **surface** is a caller-selected view of the daemon's MCP tool catalog. Different callers
— a read-only reviewer agent, an executor with full write access, an audit-only operator —
connect to the same daemon but see a different set of tools based on which surface they
select at connect time.

Surfaces are not roles, not permissions, not per-user ACLs. They are named
configurations that filter the tool list and optionally inject surface-scoped
instructions into the caller's context.

---

## Selecting a surface

Surface selection is a URL parameter:

```
http://127.0.0.1:7264/mcp?surface=readonly
http://127.0.0.1:7264/mcp?surface=reviewer
http://127.0.0.1:7264/mcp?surface=executor
```

`/mcp` with no parameter is equivalent to `?surface=default`. The `default` surface
passes every tool through unchanged.

The daemon reads the surface once during the MCP `initialize` handshake and binds it
for the lifetime of the session. All subsequent `list_tools`, `call_tool`, and
`get_tool` frames in that session use the bound surface.

### Provider alias registration

Rather than asking every agent to remember the URL parameter, register named surface
aliases as separate MCP server entries via `bro_mcp`:

```
bro_mcp(action="add", name="blackbox-readonly",
        url="http://127.0.0.1:7264/mcp?surface=readonly",
        scope="global")
```

The broker then dispatches with `--mcp blackbox-readonly` (or equivalent per provider)
instead of the full URL. Brofiles and workflow steps can reference the alias by name.

---

## Defining surfaces with packets

Surfaces are evaluated by the same packet routing machinery used for webhooks, pollers,
and workflow gates. The packet domain is `mcp-surface/routing`.

A surface packet receives an entity describing the incoming `initialize` call
(the `surface` id, the client's `user_agent`, and the `project` from context). It
returns a verdict from the lattice `["tool_surface", "deny"]`:

| Verdict | Effect |
|---|---|
| `tool_surface` | Session proceeds; surfaces contains `allow`, `disallow`, and `instructions` |
| `deny` | MCP `initialize` fails with the verdict's `reason` — the client is rejected outright, not given an empty catalog |

### Verdict shape

```json
{
  "kind": "tool_surface",
  "allow": ["bbox_hybrid_search", "bbox_inspect_entity", "bbox_knowledge"],
  "disallow": ["bro_exec", "bro_resume", "bbox_learn", "bbox_decide"],
  "instructions": "You are operating in read-only mode. Do not propose writes."
}
```

`allow` and `disallow` support glob patterns. An empty `allow` list passes all tools;
a non-empty `allow` list is an explicit allowlist — only listed tools are visible.
`disallow` always takes precedence over `allow`.

### Example packet

```json
{
  "id": "surface-readonly",
  "domain": "mcp-surface/routing",
  "description": "Read-only surface — search and inspect, no writes",
  "rules": [
    {
      "when": { "surface": "readonly" },
      "verdict": {
        "kind": "tool_surface",
        "disallow": [
          "bbox_learn", "bbox_remember", "bbox_decide", "bbox_pin",
          "bbox_note", "bbox_note_resolve", "bbox_forget",
          "bbox_thread", "bbox_render", "bbox_absorb",
          "bro_exec", "bro_resume", "bro_broadcast",
          "bro_orchestrate_run", "bro_orchestrate_author",
          "bbox_refactor_*", "bbox_artifact_install"
        ],
        "instructions": "Read-only surface. Inspect and search freely; do not propose writes."
      }
    }
  ]
}
```

Install the packet:

```
bbox_artifact_install(kind="packet", source=".bbox/packets/surface-readonly.json")
```

---

## Enforcement

Enforcement is layered so filtering can't be bypassed:

- **`list_tools`** — returns only the tools the surface allows. Hidden tools are
  invisible to the caller.
- **`call_tool`** — rejects calls to hidden tools with a clear error, even if the
  caller somehow knows the tool name. The filter is not advisory.
- **`deny` verdict** — causes `initialize` to fail immediately. The session is not
  established; the caller receives the denial reason. Use `deny` for surfaces that
  should never be reachable except from an authorized dispatch path.

---

## Debug and replay

```
bbox_mcp_surface(action="replay", surface="readonly")
```

Returns the packet evaluation result without establishing a session:

- **entity** — the synthesized `initialize` entity the packet received
- **verdict** — the raw verdict from the winning rule (or the default pass-through)
- **visible_tools** — the filtered tool list after applying the verdict

Use `replay` to validate a surface packet before deploying it, or to diagnose why
a specific tool is or isn't visible to a given surface.

---

## Surface-scoped brofile dispatch

When dispatching a brofile into a surface-restricted context, pass the surface alias
as the MCP server reference rather than the default `blackbox` entry:

```
bro_exec(brofile="reviewer", mcp_servers=["blackbox-readonly", ...])
```

The executor sees only the readonly tool catalog for its entire session. Combined with
the `disallow` filter on `bro_*` tools in the default recursion guard, this creates
a layered defense: the surface filter is a packet-configured control plane, while the
recursion guard is a mechanical invariant applied at argument construction.

---

## Status

MCP surfaces are designed and the packet machinery is in place, but the surface
binding at `initialize` and the `call_tool` enforcement layer are still in progress.
The feature is tracked as **MCP surfaces implementation** on the roadmap
(thread `thread-1bf496fc`, priority: medium).

The `bbox_mcp_surface(action="replay")` debug tool is available now and will correctly
simulate the packet evaluation as the enforcement layer lands.
