No blockers from Claude review.

Residual non-blocking notes:
- Cache compiled agent.schema.json if install-path validation becomes hot.
- bro_agent_describe still resolves brofile_ref globally; dispatch handles project scope.
- Consider mirroring manifest_stale into bro_agent_dispatch degraded output.

Local verification:
- bro_agent_ tests: 38 passed.
- orchestration::agents::validate: 53 passed.
- workflow::tests: 25 passed.
- registry: 30 passed.
- artifacts::tests: 9 passed.
- D4 iterate/cluster tests passed.
- agent_eval_check: 2 passed.
- cargo test --bin blackboxd --no-run passed.
