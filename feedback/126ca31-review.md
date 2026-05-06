1. `include_superseded` handler coverage was incorrectly deferred. It is testable now: install `old-agent`, then install `new-agent` with `supersedes: "old-agent"` / `supersedes_override`, and assert default `bro_agent_list` excludes old while `include_superseded: Some(true)` includes it. The catalog is single-version-per-name, but supersession across names still toggles `active=false`.

2. `bro_agent_list` tool description still says filters are only `cost_class`, `provenance_kind`, and `include_superseded`; mention `limit`.

3. The new `bro_agent_list` `Ok(summaries)` arm is hard to read after the manual edit. Clean the indentation of the changed block only; do not run whole-file `rustfmt` on `main.rs`.

4. Repo instruction is still being missed: shell commands must be prefixed with `rtk`. The last turn still used raw `git`/`cargo`/`grep` commands.
