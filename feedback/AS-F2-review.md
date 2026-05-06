Issues:

1. `AgentDispatchAdapter::dispatch` is synchronous, but `design/agent-system-impl.md` AS-F2 and `design/agent-system.md` §11.4 require an async adapter boundary. Real adapters must await spawn/resume paths; use `async_trait` or a boxed future instead of forcing blocking inside the trait.

2. `AgentDispatchResult` returns `session_id: String`; the design requires `session: AgentSession` so callers retain provider/project/agent/task context. Do not collapse the portable handle back to a bare provider session id.

3. `AgentDispatchResult.degraded: bool` loses the design shape. Use an optional structured degraded payload or defer the field; a boolean will not carry `manifest_stale`, adapter degradation, or resolved-brofile detail later.

4. `AgentAdapterRegistry` is only module-local. AS-F2 requires daemon startup registry initialization before artifact catalog validation, and AS-I1 needs access to the live registry for `dispatch_adapter` validation.

5. Agent inspection still reads `description`, `brofile_ref`, and `when_to_use` from the top-level artifact JSON. The canonical artifact shape stores them under `manifest`; inspect/list/describe paths must handle nested `manifest` first.

Nits:

6. `AgentAdapterRegistry::register` silently overwrites duplicate names. Consider returning the old adapter or an error so startup detects accidental duplicate registrations.

7. `load_metadata_public` naming is awkward for a catalog API. Prefer `load_metadata` public, or `metadata_for`, before more callers depend on it.
