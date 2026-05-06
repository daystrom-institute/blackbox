Issue: top-level `agents` is now stable, but each `AgentSchemaEntry` still omits `when_to_use` and `anti_patterns` when empty via `skip_serializing_if` (`src/mcp_tools/describe_schema.rs`). AS-S1 says every agent entry has `when_to_use` and `anti_patterns`; keep those fields present as empty arrays. Add an assertion for the `badgey-agent` test fixture, which currently has empty lists and would catch this.

Nit: raw `cargo` commands appeared again in the checkpoint transcript. Use `rtk` for shell commands.
