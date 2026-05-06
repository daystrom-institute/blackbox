Issue: `bbox_describe_schema` omits `agents` and `agents_by_dispatch_adapter` entirely when no agents are installed (`src/mcp_tools/describe_schema.rs`). AS-S1 is a schema-discovery phase; keep the response shape stable with `agents: []` and `agents_by_dispatch_adapter: {}` even when empty. Update `schema_no_agents_section_when_empty` accordingly.

Issue: the `bbox_describe_schema` MCP description still only advertises graph vocabulary (`src/main.rs` tool description). Add installed-agent discovery to the description so cold clients understand why/when to call it for agent selection.

Nit: `agents_by_dispatch_adapter` uses `HashMap`, so JSON object key order is nondeterministic. Prefer `BTreeMap` for stable snapshot/debug output.
