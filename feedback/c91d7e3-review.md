1. `design/agent-system-impl.md` AS-T1 includes `limit?`; `AgentListParams` still has no `limit`, and `bro_agent_list` cannot cap responses. Add it or explicitly document why AS-T1a deferred it.

2. `bro_agent_list` direct tests still do not cover `include_superseded` or `provenance_kind`. Cost-class is covered; the other two AS-T1 filters are unguarded.

3. `bro_agent_get_pinned_ref` only proves `name@vN` works when `N` equals the active single stored version. Add mismatch coverage (`reviewer@v4` when only v5 exists) so ref parsing/result semantics are pinned despite the current single-version catalog limitation.

4. Repo instruction: shell commands must be prefixed with `rtk`. This checkpoint used raw `git`, `cargo`, and `rustfmt`; use `rtk ...` in future worker turns.
