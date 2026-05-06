Issues:
1. `bro_agent_list.cost_class` silently ignores invalid values because parse failure becomes `None`, which returns an unfiltered list. Return `BadInput`/error for unknown cost classes.
2. `validate_agent_install` only validates `supersedes` when it is a string; non-string `supersedes` values are ignored and accepted. Reject non-string values if present.
3. The checkpoint claims "Tests for parameter parsing + output", but visible tests only cover agent modules and tool-doc parity. Add direct tests for `bro_agent_list`/`bro_agent_get` outputs, invalid cost_class, missing agent, and pinned ref behavior.
4. `bro_agent_get` currently returns tool error text for not found. Existing list/get-style tools may prefer structured `{agent:null}` or `{found:false}`; align with local convention before clients depend on it.
5. `bro_agent_list` uses `serde_json::to_value(...).unwrap()` inside response construction. Serialization should not fail for current enums, but avoid unwraps in tool handlers.
6. Filter grammar hardening is still not actual MCP/native grammar validation. The checkpoint should name this as deferred, or implement a stricter validator.

Nits:
7. `AgentListParams.cost_class` is a `String`; consider using `AgentCostClass` directly in the schema if schemars supports it cleanly.
8. `AgentGetParams.name` should probably be named `name_or_ref` if pinned refs are supported.
