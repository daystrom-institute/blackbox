Issues:

1. `src/orchestration/agents/types.rs` omits `BadgeyAgentArgs { prompt, badgey_id }`, which AS-F1 explicitly requires for the badgey dispatch adapter boundary.

2. `src/orchestration/agents/types.rs` omits `CompositionShape::{Chain,FanOut,Escalation}`. The current `AgentComposition` fields are useful, but the AS-F1 skeleton also asks for composition shape constants.

3. AS-F1 gates call for validation helpers `validate_description_length` and `validate_when_to_use_nonempty`; neither helper nor tests exist. Add focused helpers without turning this into AS-I1 validation.

4. `AgentRef::parse` accepts `agent:foo@v0` and names containing `:`. Keep it aligned with `EntityRef::Agent`: no colon in name/rest and version must be > 0.

Nits:

5. `AgentLifecyclePolicy` contradicts `design/agent-system.md` §4.2 / §15.3 unless it stays out of manifest/schema use. Prefer removing it from AS-F1, or leave a narrow comment explaining it is not a manifest field.

6. Consider adding a full-agent wrapper type for `{kind,name,version,supersedes,manifest}` before AS-I1, or explicitly defer it. `AgentManifest` only models the nested `manifest` object.
