Blockers:
- `src/main.rs:4790-4808`: direct path never validates `args` against `manifest.inputs.schema`. AS-T4 and §5.3 require bad_input with field-level details before prompt expansion/spawn. Add validation + negative test.
- `src/main.rs:4832`: direct path only applies ambient; it drops the resolved brofile lens/persona. `bro_exec` composes `apply_brofile_lens(&apply_ambient(...), lens)`. Carry `bf.lens` / inline `lens` through and test prompt wrapping if practical.
- `src/main.rs:4868-4870`, `src/orchestration/mod.rs:1160-1218`, `src/main.rs:2703-2706`: `bro_label = agent:<name>@v<version>` is computed only for adapter ctx and never stamped onto direct-path `TaskInner`; `bro_status` also does not surface `bro_label`. AS-T4 gate is not met. Stamp the label on spawned direct tasks and expose it in `bro_status`/dashboard without breaking named-bro resume.

Issues:
- `src/main.rs:4760-4763`: invalid inline provider silently falls back to Claude. Return bad_input instead; add test.
- `src/main.rs:4811`: direct path pre-mints UUID session IDs for every provider, unlike `bro_exec` which uses UUID only for Claude and `"pending"` otherwise. Verify provider resume semantics or align with `bro_exec`.
- `src/main.rs:4708-4710`: adapter result serializes `result.session` and separately reads `result.session.task_id`; prefer deriving `task_id` before the JSON payload to avoid move-order fragility.

Tests reproduced locally:
- `rtk cargo test --release --bin blackboxd -- bro_agent_dispatch --nocapture`
- `rtk cargo test --release --bin blackboxd -- bro_agent --nocapture`
- `rtk cargo test --release --bin blackboxd -- 'agents::' --nocapture`
- `rtk cargo test --release --bin blackboxd -- every_registered_tool --nocapture`
- `rtk cargo test --release --bin blackboxd -- description_summary --nocapture`

Process nit persists: raw shell commands appeared again in worker logs; use `rtk`.
