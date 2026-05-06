Issue: `bro_agent_dispatch` tool/docs now say dispatch returns agent attribution, but the actual response at `src/main.rs:4937-4942` only returns `session`, `task_id`, `resolved_brofile`, and `merged_filters`. Either add `agentLabel` to the immediate response and cover it in a handler test, or tighten the ToolDoc / `#[tool(description)]` wording to say `agentLabel` is surfaced later through `bro_status` / `bro_dashboard`. Prefer adding the response field; it matches the current advertised contract and helps callers without an extra status round trip.

Nit: `src/orchestration/mod.rs:656-657` has misindented `recoverable: false` in the failed-spawn constructor. Fix surgically; do not run broad `cargo fmt` if it would reformat unrelated files.

Nit: raw `cargo` / `grep` appeared again in the checkpoint transcript. Use `rtk` for shell commands.
