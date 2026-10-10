# bro-config

- Connection and dispatch settings are shared by the headless CLI and daemon.
  This crate owns no execution, HTTP client, roster or transcript runtime.
- Read `fleet.json` beside the selected `config.toml`. Loading is best effort;
  missing, unreadable or malformed input produces empty dispatch settings.
- Read only active settings and ignore unknown fields. Never rewrite operator
  configuration while loading it.
- MCP header and environment values preserve secret references as opaque JSON;
  the dispatch provider resolves them before worker launch.
- Canonicalize project lookup paths, with a raw-path fallback for missing paths.
- Tests use temporary files. Environment mutations hold `test_env_lock` and
  restore the original values before returning.
