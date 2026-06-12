# bro-code-mode — vendored Codex code-mode runtime (V8 cells)

Vendored near-verbatim from openai/codex `codex-rs/code-mode` (Apache-2.0).
Two disciplines follow from that and are not negotiable:

- **Zero `blackbox` dependency** (like bro-script: contract-bottom-adjacent;
  it embeds V8, so the pinned `v8` version is a deliberate, reported choice).
- **Mark every divergence** with a `Local addition (not vendored)` comment on
  the type/fn/branch. A future vendor refresh diffs against upstream;
  unmarked local changes are what gets silently clobbered or mis-merged.

## The exec description is a product surface

The string assembled by the description builder is model-facing prompt text,
not documentation. Rendering rules that bite:

- The **schema-rendered flat tool catalog renders only in code-mode-only**;
  the template + **namespace sections render in every mode**. Namespace
  declarations are hand-authored TS (`ToolNamespaceDescription.declarations`)
  — there is no compiler keeping them honest, only probes. Drift between a
  declaration and the serde reality is a real bug class; fix the declaration
  in the same change as the shape.
- Several template lines exist because live probe agents burned cells on the
  gap they close (no Node stdlib; reserved-global shadowing; fresh-scope
  redeclare; one-arg `store()`). They look like fluff. Do not trim them.

## Namespace projection

`ToolDefinition.namespace_binding` projects a tool as a nested namespace
global (`code.items(...)`) beside `tools` — same per-index trampoline, same
host seam, same deny filter; only the naming differs. A namespace global may
not shadow a runtime-owned global (reserved list in the globals installer).
`ToolDefinition` / `EnabledToolMetadata` have no `Default`: adding a field
means updating every literal constructor, including the test fixtures here
and in bro-harness.

## Cell semantics worth keeping in your head

- Fresh isolate per `exec` call. Nothing crosses cells except the session
  KV (`store`/`load`) — locals, imports, and revived functions all die.
- **Function store**: `store(key, fn)` persists the function's SOURCE under
  a sentinel envelope; `load` revives it by compiling `(source)` in the
  current context. The self-contained constraint is enforced by reality:
  lost closure variables throw ReferenceError at call time. That loudness is
  the design — do not "fix" it by capturing scope.
- Tool results resolve in batches at quiescent boundaries; `Promise.all`
  over independent calls is the parallelism model.
