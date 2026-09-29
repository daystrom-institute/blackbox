# MCP Surfaces

A **surface** is a named view of the daemon's MCP tools.

Use surfaces when different callers connect to the same daemon but should see
different tool catalogs: a read-only reviewer, a dispatched bro, an interactive
coding session, or an operator.

Surfaces are not roles, not permissions, not per-user ACLs. They are named
filters over the tool list.

---

## Selecting a surface

Surface selection is a URL parameter:

```
http://127.0.0.1:7264/mcp?surface=readonly
http://127.0.0.1:7264/mcp?surface=ops
```

`/mcp` with no parameter is equivalent to `?surface=default`.

The daemon reads the surface once during the MCP `initialize` handshake,
computes the session's visible tool set, and binds it for the lifetime of the
session. All subsequent `list_tools`, `call_tool`, and `get_tool` frames in
that session use it.

## Built-in surfaces

The built-in table lives in `crates/bbox-config/src/default_surfaces.toml`:

| Surface | Caller |
| --- | --- |
| `default` | External clients (Claude Code, Codex CLI) and ordinary dispatches |
| `interactive` | Fleet cockpit sessions |
| `agent-internal` | Workflow-owned dispatches |
| `readonly` | Reviewers, evaluators and observers (allowlist) |
| `ops` | Operators: the full catalog |

## Configuring surfaces

A `[surfaces.<name>]` table in the daemon config file overrides the built-in
surface of the same name or adds a new one:

```toml
[surfaces.default]
disallow = ["bbox_project_*", "bbox_storage_*"]

[surfaces.reviewer]
allow = ["bbox_hybrid_search", "bbox_inspect_entity", "bbox_knowledge"]
```

- Patterns are globs over tool names. Bare names (`bbox_learn`), prefixed
  names (`mcp__blackbox__bbox_learn`) and provider spellings
  (`blackbox(bbox_learn)`) all match.
- A non-empty `allow` is an allowlist; an empty `allow` passes every tool not
  disallowed.
- `disallow` always wins over `allow`.
- Surfaces load with daemon configuration; a change applies on restart.

## Enforcement

- **`initialize`** refuses a surface missing from the table
  (`tool surface denied: unknown MCP surface: <name>`). The session is not
  established.
- **`list_tools`** returns only the surface's visible tools.
- **`call_tool`** rejects calls to hidden tools, even when the caller knows the
  tool name.
- **Dispatch**: a brofile's `surface` selector folds the same surface into the
  dispatched child's tool filters, so a child session is governed exactly like
  a wire caller. An unknown surface denies every tool. The recursion guard
  applies on top.

## Operator tools

Tools hidden from agent-facing surfaces stay reachable on `ops`:

```
bro mcp call <tool> '<json>' --surface ops
```

The built-in agent-facing surfaces hide `bbox_stats`, `bro_prune`,
`bro_allocator_*`, `bro_mcp`, project administration other than
`bbox_project_list`, `bbox_reindex`, `bbox_reembed`, `bbox_embed_*`,
`bbox_storage_*`, `bbox_edge_compact`, `bbox_doctor` and `bbox_artifact_*`.
