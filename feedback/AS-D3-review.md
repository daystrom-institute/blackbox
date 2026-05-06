# AS-D3 review

Commit `aa305db`.

## Issues

1. **`bbox_inspect_entity(ref="agent:...")` still cannot inspect installed agents.**
   AS-D3 requires the agent provider to read from the artifact catalog
   and `bbox_inspect_entity(ref="agent:badgey@v1")` to return the
   manifest with edges (`design/agent-system-impl.md:99-112`).
   Current `AgentProvider::get_entity` only derives `name` and
   `version` from the ref (`src/providers/agent.rs:23-31`) and
   `BlackboxServer::inspect_extra_properties` has no `EntityRef::Agent`
   branch (`src/main.rs:315-408`). Because `inspect_entity_exists`
   only returns true when extra properties exist or edges exist
   (`src/main.rs:411-420`), an installed agent with no AS-I3 edges will
   still render `not_found`. Add an agent branch that resolves
   `agent:<name>@v<version>` against the artifact catalog and returns
   manifest fields such as description / when_to_use / brofile_ref.

2. **Agent schema advertises manifest fields the provider never returns.**
   `AgentProvider::schema` lists `description` and `brofile`
   (`src/providers/agent.rs:34-40`), but `get_entity` only returns
   `name` and `version`. Either populate those from the catalog via
   `inspect_extra_properties`, or keep the schema limited until catalog
   resolution lands. Prefer the catalog path because AS-D3 calls for it.

## Nits

3. `EntityRef::Agent::try_render` does not reject empty names, colon in
   names, or version 0 (`src/entity_ref.rs:223-265`). Parsing rejects
   empty/colon names, so invalid in-memory values can render strings
   that later fail to parse or violate the intended grammar. Add
   render-side validation or route AgentRef through the AS-F1 parser.

4. `schema_lists_all_d1_entity_types` now asserts `13` but still uses
   the old D1 name and only checks `knowledge` / `bash_call`
   (`src/mcp_tools/describe_schema.rs:151-163`). Add an explicit
   `agent` assertion.
