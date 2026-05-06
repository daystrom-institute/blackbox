Blocker: checkpoint left the worktree dirty. `git status --short` shows modified `src/workflow/mod.rs` after commit/push.

Blocker: the dirty `src/workflow/mod.rs` does not compile. It contains an uncommitted duplicated stale test block starting near line 964, including old expectations like `agent_fan_out_uses_fork_and_wait_for` / `FanOut` / `WaitLeft` / `WaitRight`, plus mismatched braces. `cargo test --release --bin blackboxd -- agent_artifact_install_list_supersede_round_trip --nocapture --test-threads=1` fails with `unexpected closing delimiter`.

Fix by removing/reverting the uncommitted stale block or intentionally integrating a clean version, then run the requested tests from a clean worktree. Pause only after `git status --short` has no tracked changes except untracked `feedback/*.md`.
