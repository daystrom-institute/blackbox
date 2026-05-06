BLOCKER: `cargo test --release --bin blackboxd -- 'description_summary' --nocapture` fails. `bro_agent_search` `#[tool(description)]` does not exactly match `ToolDoc.summary`.

ISSUE: `matched_anti_patterns` is populated even when `exclude_anti_pattern_matches` defaults true. Design/result comment says it is populated only when `exclude=false`; default result should not expose anti-pattern hits unless caller asks.

ISSUE: `vector_status.coverage_ratio` is `results.len() / active_agents`, which is query hit ratio after limit/filtering, not embedding/vector coverage. In keyword-degraded mode, report a stable degraded vector status (`coverage_ratio: 0.0` or explicit unavailable reason), not a query-dependent coverage value.

NIT: Worker still runs raw `cargo`/`git`/`grep` commands; use `rtk` prefix in this repo.
