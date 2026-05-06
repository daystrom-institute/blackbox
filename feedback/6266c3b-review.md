Blocker: current branch has a failing agent test. `cargo test --release --bin blackboxd -- agent_artifact_install_list_supersede_round_trip --nocapture --test-threads=1` fails at `src/main.rs:10102`: `agent artifact must have kind: "agent"`. Update the fixture to the current agent artifact shape instead of leaving this red.

Blocker: AS-C1 reference workflows still dispatch the `placeholder` actor body at every node (`actor: "placeholder"` / brofile `probe-haiku`) in addition to the `bro_agent_dispatch` hooks. These examples are supposed to demonstrate workflows hand-authored against `bro_agent_dispatch`; use hook-only nodes (`actor: ""`) or otherwise avoid requiring/provoking unrelated `probe-haiku` dispatches.

Issue: `chain.json` dispatches `code-reviewer` and then terminates without waiting for the second agent. A chain demo should prove A completes, then B is dispatched and completed, with B output captured.

Issue: `escalation.json` dispatches the expensive `code-reviewer` branch and then terminates without waiting for it. Capture an `expensive_output` via `bro_wait`, or the escalation path only proves launch, not completion.

Issue: `fan-out.json` uses `on_failure: "warn"` for both dispatches and waits; `Aggregate` can render unresolved/missing `left_output` / `right_output`. Reference workflows should halt on failed dispatch/wait unless the example is intentionally demonstrating degraded aggregation.

Nit: new code added non-ASCII comment text in `src/main.rs` (`—`, `§`). Prefer ASCII in new code unless the surrounding edit needs otherwise.
