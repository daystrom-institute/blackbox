# Badgey Artifacts Parser Queue Review

Resolved:
- B1 tests now validate agent install behavior, including badgey adapter-gated rejection/acceptance and scout no-adapter acceptance.
- W4 queue now has async `wait_until_turn` with a permit and `Notify`; second resume can actually wait behind the active turn.
- W3 parser preserves argument case and no longer aliases `dismiss P-N` to proposal rejection.
- Registry no longer exposes register-then-update provider session flow; provider session IDs remain observed-at-registration data.
- Badgey brofiles use catalog model id `claude-sonnet-4-6`.

Residual:
- Full `bbox_artifact_install` MCP round trip remains for M1/integration once the badgey adapter exists.
- Runtime filter denial for dispatched Badgey bros remains an integration gate, not a pure artifact parse gate.
- Queue priority drop policy is still simple tail-drop; revisit when W2 wires user-visible queued-turn errors.
