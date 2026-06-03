---
corpus: blackbox-research
kind: research-hub
track: harness
status: researching
confidence: mixed
generated_by: codex
last_reviewed: 2026-06-02
topics:
  - narf
  - bro-harness
  - metatools
  - refactoring
  - atoms
  - clipboard
  - promises
---

# NARF: next-gen agentic refactoring framework

This is a loose research braindump from the NARF discussion session. It is not
a committed architecture. The goal is to preserve the concrete breadcrumbs and
the felt shape of the systems we inspected so another agent can pick this up,
argue with it, and add a better design pass.

> **Successor:** the canon pass — tighter, normative, and grounded in the
> atoms/code-mode code rather than session feel — lives in
> [narf-draft2.md](narf-draft2.md). It is adjacent, not a supersession: this doc
> keeps the breadcrumb map and the exploratory script sketches; draft 2 holds the
> proposed canon and the "authoring layer as primary harness interface" thesis.
> The related cross-harness axis is [metatools.md](metatools.md).

The working thesis: bro-harness V2 could become a first-class agentic
refactoring runtime rather than a collection of adjacent tools. The interesting
move is to fuse four ideas that are already present in separate places:

- Codex-style JS metatools: tool calls are programmable async functions inside
  a V8 sandbox, with intermediate values held in script variables instead of
  model context.
- bro-harness refs/promises: large values and pending work have durable handles
  and lifecycle, so the agent can compose work without pasting everything back
  into conversation.
- blackbox refactor/slice tooling: code edits are planned, validated, applied,
  rolled back, and repaired through typed operations rather than ad hoc text
  edits.
- atoms: reusable capability contracts with stable inputs, effects, outputs,
  traces, ownership, and implementation bindings.

NARF is the imagined fusion: a programmable refactoring harness where code
discovery, slice selection, semantic refactor plans, command diagnostics,
repair obligations, atom invocations, and clipboard-like values are all
first-class typed bindings in one composition runtime.

## Breadcrumb map

Places touched during the session:

- Existing harness research and metatool frame:
  [research/harness/metatools.md](metatools.md).
- Harness clipboard design:
  [design/bro-harness/bro-harness-clipboard.md](../../design/bro-harness/bro-harness-clipboard.md).
- Harness tool chaining design:
  [design/bro-harness/bro-harness-tool-chaining.md](../../design/bro-harness/bro-harness-tool-chaining.md).
- Harness stage-3 chaining backlog:
  [design/bro-harness/backlog-tool-chaining-stage-3.md](../../design/bro-harness/backlog-tool-chaining-stage-3.md).
- Brodex loop learnings:
  [design/bro-harness/brodex-agent-loop-learnings.md](../../design/bro-harness/brodex-agent-loop-learnings.md).
- Harness clipboard implementation:
  [crates/bro-tools/src/clipboard.rs](../../crates/bro-tools/src/clipboard.rs).
- Harness promise implementation:
  [crates/bro-tools/src/promise.rs](../../crates/bro-tools/src/promise.rs).
- Tool context carrying promises and clipboard:
  [crates/bro-tools/src/tool.rs](../../crates/bro-tools/src/tool.rs).
- Harness shell promise/stdout wiring:
  [crates/bro-tools/src/shell.rs](../../crates/bro-tools/src/shell.rs).
- Workspace read/write ref consumers and producers:
  [crates/bro-tools/src/workspace.rs](../../crates/bro-tools/src/workspace.rs).
- Agent loop promise wakeup:
  [crates/bro-harness/src/agent_loop.rs](../../crates/bro-harness/src/agent_loop.rs).
- Refactor runner core:
  [src/refactor/mod.rs](../../src/refactor/mod.rs).
- Slice tools:
  [src/slices.rs](../../src/slices.rs) and
  [src/tools/slices.rs](../../src/tools/slices.rs).
- Slice/refactor primitive design:
  [design/refactor-tools/context-clipboard-refactor-primitives.md](../../design/refactor-tools/context-clipboard-refactor-primitives.md).
- Codex code-mode tool definition lowering:
  [../codex/codex-rs/tools/src/code_mode.rs](../../../codex/codex-rs/tools/src/code_mode.rs).
- Codex code-mode tests:
  [../codex/codex-rs/core/tests/suite/code_mode.rs](../../../codex/codex-rs/core/tests/suite/code_mode.rs).
- Codex code-mode runtime globals:
  [../codex/codex-rs/code-mode/src/runtime/globals.rs](../../../codex/codex-rs/code-mode/src/runtime/globals.rs).
- Codex code-mode service/session:
  [../codex/codex-rs/code-mode/src/service.rs](../../../codex/codex-rs/code-mode/src/service.rs).
- Codex execute handler:
  [../codex/codex-rs/core/src/tools/code_mode/execute_handler.rs](../../../codex/codex-rs/core/src/tools/code_mode/execute_handler.rs).

## Live Codex JS metatool experience

The session included a concrete probe of Codex's code-mode surface. The user
asked for the open bbox threads for this project. The tool available to the
assistant was not the blackbox MCP tool directly in the message protocol. It
was the Codex exec tool: a raw JavaScript async-module cell with a global tools
object.

The useful experience was that once the surface was internalized, it felt like
normal typed async orchestration:

~~~js
const [rtk, bb, proj] = await Promise.all([
  tools.exec_command({
    cmd: "cat /Users/invidious/.codex/RTK.md",
    workdir: "/Users/invidious/repos/transcript-search",
    max_output_tokens: 2000
  }),
  tools.exec_command({
    cmd: "cat /Users/invidious/.blackbox/BLACKBOX.md",
    workdir: "/Users/invidious/repos/transcript-search",
    max_output_tokens: 6000
  }),
  tools.exec_command({
    cmd: "cat PROJECT.md",
    workdir: "/Users/invidious/repos/transcript-search",
    max_output_tokens: 6000
  })
]);

text(rtk.output);
text(bb.output);
text(proj.output);
~~~

The same interface also made tool discovery feel mechanical:

~~~js
const threadTools = ALL_TOOLS.filter(t =>
  t.name.includes("bbox_thread") || t.name.includes("thread")
);

text(JSON.stringify(threadTools, null, 2));
~~~

Then the blackbox MCP tool was called as just another nested JS function:

~~~js
const res = await tools.mcp__blackbox__bbox_thread_list({
  project: "/Users/invidious/repos/transcript-search",
  include_resolved: false,
  include_workflows: true
});

text(JSON.stringify(res, null, 2));
~~~

This found one open project thread:

- thread-cc07f395: Pathology ensemble redesign - loose ends & next steps.

The important point is not the thread. The important point is the shape:

- multiple host tools composed with Promise.all;
- intermediate results stayed in JS locals;
- discovery used ALL_TOOLS;
- MCP tools were callable through normalized names on tools;
- only selected output was emitted with text(...).

Codex source backs that shape directly. Code-mode lowers host tool schemas into
runtime tool definitions in
[../codex/codex-rs/tools/src/code_mode.rs](../../../codex/codex-rs/tools/src/code_mode.rs).
It converts function, freeform, and namespace tool specs into code-mode
definitions, and normalizes nested names with code_mode_name_for_tool_name.
The tests in
[../codex/codex-rs/tools/src/code_mode_tests.rs](../../../codex/codex-rs/tools/src/code_mode_tests.rs)
show generated TypeScript declarations being appended to tool descriptions,
including the freeform apply_patch declaration.

The runtime globals are the most relevant NARF breadcrumb. Codex installs
tools, ALL_TOOLS, text, image, store, load, notify, yield_control, timers, and
exit in
[../codex/codex-rs/code-mode/src/runtime/globals.rs](../../../codex/codex-rs/code-mode/src/runtime/globals.rs).
It also deliberately removes browser/host escape hatches such as console,
Atomics, SharedArrayBuffer, and WebAssembly in the current implementation.

The service/session layer in
[../codex/codex-rs/code-mode/src/service.rs](../../../codex/codex-rs/code-mode/src/service.rs)
is another key breadcrumb: a CodeModeSession owns stored values, cells, a
delegate, and cell ids. That makes the JS runtime more than a one-shot eval:
cells can yield, continue, and share stored state through store/load. Runtime
requests and nested tool calls are described in
[../codex/codex-rs/code-mode/src/runtime/mod.rs](../../../codex/codex-rs/code-mode/src/runtime/mod.rs),
where nested calls carry cell_id, runtime_tool_call_id, tool_name, tool_kind,
and input.

The Codex code_mode_only tests are directly relevant. They show a model surface
where the only top-level remote tools are exec and wait, while MCP tools still
appear inside the JS tools object. In other words, the user-facing tool protocol
collapses to a tiny metatool pair, while the internal composition surface
becomes richer.

That is the core NARF temptation: make the agent's actual working interface a
programmable, typed, stateful composition runtime, not a flat list of direct
tools.

## The friction we filed

One session friction point was filed as a gap: evidence bundling required
path_ids even for an entity-only bundle. The gap id was gap-58e800c6, with
dedupe key mcp_surface/evidence-bundling/path-ids-required-without-paths.

That matters for NARF because it is an example of a tool schema that exposes an
internal coupling in the wrong place. A next runtime should make the expected
composition obvious:

~~~js
const bundle = await evidence.bundle({
  entities: ["thread-cc07f395"],
  includePaths: false
});
~~~

If no path evidence is requested, no path-shaped field should be required. This
is a small example, but it captures a larger rule: if NARF has first-class refs,
plans, captures, and atoms, their schemas should encode the composition model
directly instead of forcing agents to discover hidden invariants by failure.

## bro-harness clipboard, refs, and promises

The bro-harness clipboard already points toward NARF. It is not merely a copy
buffer. It is a bounded, typed, named value store shared by tools through
ToolCx.

ToolCx carries both promise and clipboard state:

~~~rust
pub struct ToolCx<'a> {
    pub root: &'a Path,
    pub permissions: &'a ToolPermissions,
    pub working_dir: &'a Path,
    pub promises: &'a PromiseStore,
    pub clipboard: &'a ClipboardStore,
}
~~~

The implementation in
[crates/bro-tools/src/clipboard.rs](../../crates/bro-tools/src/clipboard.rs)
defines RefKind variants for text, file slices, tool results, and JSON. The
store has explicit bounds: a maximum item size, maximum register count, and LRU
eviction. Registers can be addressed as bare names or clip:name; promise: is
reserved and intentionally not normalized as a clipboard register.

The clipboard design solves a real context problem:

- producers can deposit data without returning it to the model;
- consumers can read by reference;
- transforms can operate register-to-register;
- clip_peek is an explicit, bounded egress path for inspection.

The current chain is already an ABI:

- file_read { into } stores a slice in a register.
- file_write { from } consumes a register.
- shell_run { stdin_from } consumes a register.
- shell_run { stdout_to } stores stdout in a register.
- clip_transform, clip_slice, and clip_grep transform values without turning
  the model context into the transport.

The promise layer is the sibling abstraction for pending work. In
[crates/bro-tools/src/promise.rs](../../crates/bro-tools/src/promise.rs),
promises are same-dispatch, not session-persisted. They move through Running,
Completed, Failed, and Cancelled states. The store supports start, settle,
cancel, status, list, wait, all, and any. Completed promises produce hidden
harness events that wake the agent loop rather than dumping large results
directly into context.

The interesting existing fusion is in shell_run. Running shell work in promise
mode can also specify stdout_to. When the command settles, run_shell_promise
deposits stdout into the clipboard if requested. This is a small but powerful
shape:

~~~text
pending process -> completed promise -> settled ref
~~~

That is almost the NARF lifecycle in miniature. A pending operation should be
able to resolve to a typed ref, and downstream tools should compose over the
ref without requiring the agent to paste the value.

## blackbox refactoring tools as typed transactions

The blackbox refactoring tools add the missing edit discipline. Their shape is
not just search then edit. It is:

1. ground the code;
2. plan a typed edit;
3. inspect file hashes, previews, leftovers, selected items, parse validation;
4. apply with stale-hash and dirty-worktree protection;
5. optionally compose commands and repairs under rollback.

The core operations are documented in the refactor system memories and
implemented in [src/refactor/mod.rs](../../src/refactor/mod.rs). The most
important NARF surface is bbox_refactor_run: it composes ordered plan and
command steps, captures diagnostics, tracks repair obligations, and rolls back
when required obligations remain unresolved.

The struct shape is direct:

~~~rust
pub struct RefactorRunParams {
    pub project: Option<String>,
    pub steps: Vec<RefactorRunStep>,
    pub dry_run: Option<bool>,
    pub confirm: Option<bool>,
    pub rollback_on_failure: Option<bool>,
    pub max_bytes: Option<usize>,
    pub dispatch_origin: Option<String>,
}
~~~

Steps are either plans or commands:

~~~rust
pub enum RefactorRunStep {
    Plan {
        params: RefactorPlanParams,
        optional: Option<bool>,
    },
    Command {
        command: String,
        args: Option<Vec<String>>,
        cwd: Option<String>,
        touches: Option<Vec<String>>,
        required: Option<bool>,
        capture: Option<CaptureSpec>,
        on_failure: Option<OnFailure>,
    },
}
~~~

Command capture is already a hidden typed value pipe. CaptureSpec::RustcJson
parses cargo check --message-format=json into diagnostics and stores them in a
capture context, conventionally as last. A later rust_compile_fix_round can
consume that capture and produce a repair plan.

Repair obligations are also promise-like. A command with soft failure can
continue for repair, but the run cannot finish successfully until obligations
are resolved. That gives the run a lifecycle beyond simple command status:

~~~text
command failed -> diagnostic capture -> repair obligation -> repair plan
-> validation command -> obligation resolved -> transaction commits
~~~

For NARF, the key insight is that blackbox refactor runs already have:

- typed plans as settled values;
- command captures as hidden refs;
- repair obligations as pending lifecycle;
- rollback scopes as transactions;
- macro expansion as a small domain-specific composition language.

NARF could make those primitives general instead of embedding them only inside
one MCP call.

## bbox_slice as selector vocabulary plus refactor discipline

The slice tools are part of the unification effort because they expose a
precise selector vocabulary and then lower mutations into refactor plans.

In [src/slices.rs](../../src/slices.rs), ranges can be selected by:

- line spans;
- marker spans;
- exact text;
- byte offsets.

Insert locations can be selected by:

- line;
- before marker;
- after marker;
- prepend;
- append.

Mutating slice tools do not bypass refactor discipline. They produce
RefactorPlans with previews and can route confirmed writes through
refactor::apply. That makes the slice layer a bridge between human-friendly
editing intent and the same transaction machinery as larger refactors.

The design note in
[design/refactor-tools/context-clipboard-refactor-primitives.md](../../design/refactor-tools/context-clipboard-refactor-primitives.md)
is especially relevant. It distinguishes raw MCP slice tools from refactor-plan
kinds and says refactor agents should prefer plan kinds. The lesson for NARF is
not that every operation must become a raw slice mutation. The lesson is that
selector resolution, byte coordinates, previews, plans, and apply results should
be shareable engine primitives.

In NARF terms:

~~~text
selector -> slice ref -> transform -> edit plan -> transaction apply
~~~

That could be one coherent value path.

## atoms as capability contracts

The atom system is the stable public capability layer. An atom is not just a
prompt. It is a contract:

- name/ref;
- input schema;
- output schema;
- declared effects;
- composition metadata;
- implementation binding;
- trace handle;
- owner-aware invocation and resume.

The sm-atoms surface framed atoms as capabilities with multiple possible
backends: profile, workflow, deterministic, adapter. Refactor atoms are already
special: they use a shared refactor protocol of ground, plan with deep analysis,
decide, apply-or-block, and done-note. They also preserve operator-authority
constraints such as not defaulting acknowledge_* flags.

For NARF, atoms are likely the unit of reusable expertise. A NARF runtime should
be able to call an atom like a typed async function:

~~~js
const result = await atoms.invoke("refactor.rust.extractModule", {
  project,
  item: "crate::server::state::SharedState",
  destination: "src/server/state/mod.rs"
}, {
  into: "ref:atom/extract-module"
});
~~~

The atom invocation itself may be pending. Its output should be a ref. Its
effects should be transaction-aware. Its trace should be inspectable. This
would let atoms compose with direct refactor tools instead of living in a
separate orchestration lane.

## The unification insight

The same few concepts kept reappearing under different names.

| Concept | Current incarnation | NARF reading |
| --- | --- | --- |
| Settled value | clipboard register, slice read, refactor plan, command capture, atom output | Ref<T> |
| Pending work | promise, code cell, async shell command, atom invocation, repair obligation | Promise<T> / Obligation<T> |
| Transaction | bbox_refactor_run, refactor_apply, slice apply, rollback-on-failure | Tx |
| Tool contract | MCP schema, Codex TS declaration, atom manifest | typed binding |
| Discovery | ALL_TOOLS, bbox code symbols, hybrid search, node/refs tools | queryable code/tool index |
| Composition | JS code-mode, refactor run steps, workflow graphs, shell pipelines | programmable orchestration |
| Egress control | text, clip_peek, previews, summaries | explicit bounded render |
| Repair loop | rustc capture plus compile-fix round | typed diagnostic -> plan feedback |

The possible evolution is not just adding more tools. It is giving these shared
concepts one runtime vocabulary.

## A compelling NARF script shape

This is the sketch that felt most compelling in the session: refactoring tools
as first-class bindings inside a JS sandbox, with refs/promises/transactions
available as normal values.

~~~js
const tx = await narf.tx.begin({
  project,
  rollbackOnFailure: true,
  label: "extract SharedState helpers"
});

const symbols = await code.symbols({
  project,
  query: "SharedState",
  language: "rust"
}, {
  into: "ref:json/shared-state-symbols"
});

const implBlock = await code.node({
  project,
  symbol: symbols.pick(s => s.kind === "impl" && s.name.includes("SharedState"))
}, {
  into: "ref:slice/shared-state-impl"
});

const plan = await refactor.plan({
  project,
  kind: "split_rust_impl_methods_to_submodule",
  input: {
    source: implBlock,
    destination: "src/server/state/shared_state.rs"
  },
  deepAnalysis: true
}, {
  into: "ref:plan/split-shared-state"
});

await tx.apply(plan);

const check = await shell.run({
  command: "cargo",
  args: ["check", "--message-format=json"],
  capture: "rustc_json",
  captureTo: "ref:diag/rustc/last",
  onFailure: "continue_for_repair"
}, {
  mode: "promise"
});

await promises.wait(check);

if (await tx.hasOpenObligations()) {
  const fix = await refactor.compileFixRound({
    project,
    diagnostics: "ref:diag/rustc/last"
  }, {
    into: "ref:plan/compile-fix"
  });

  await tx.apply(fix);
  await tx.command({
    command: "cargo",
    args: ["check", "--message-format=json"],
    capture: "rustc_json"
  });
}

await tx.commit();

text(await tx.summary({ maxBytes: 4000 }));
~~~

This example deliberately combines lessons from all surfaces:

- Codex code-mode gives the JS orchestration substrate.
- ALL_TOOLS/typed declarations become code, refactor, shell, atoms, refs,
  promises, and tx namespaces.
- bro-harness clipboard becomes a typed ref store.
- bro-harness promises become async operation handles.
- blackbox refactor plans become transaction-applied values.
- rustc capture becomes a typed diagnostic ref.
- repair obligations become transaction lifecycle, not ad hoc agent memory.
- output is explicitly bounded through text(...) or summary rendering.

## Initial draft design

This is a first pass at a NARF design, appended as a sketch rather than a
specification.

### 1. Core primitives

NARF should have a tiny set of primitive concepts:

- Ref<T>: a typed, settled value stored outside model context.
- Promise<T>: pending work that eventually resolves to a value, error, or
  cancellation.
- Plan<E>: a typed proposed effect, usually an edit effect, with previews,
  validation status, hashes, and apply preconditions.
- Tx: a transaction scope for applying plans and commands with rollback,
  obligations, and final validation.
- Atom<I, O>: a named capability contract with schema, effects, trace, and
  implementation binding.
- Script: a bounded JS composition cell with access to typed host bindings.

Clipboard registers, command captures, slice reads, refactor plans, and atom
outputs should all be representable as refs. Shell commands, dispatched agents,
long refactor jobs, code index refreshes, and atom invocations should all be
representable as promises.

### 2. Binding model

The Codex lesson is that tools become much easier to compose when they are
normal async functions in a sandbox:

~~~js
await tools.mcp__blackbox__bbox_thread_list({ project });
~~~

For NARF, the raw MCP names should be one layer, but the ergonomic layer should
be domain namespaces:

~~~js
await code.symbols(...);
await slices.read(...);
await refactor.plan(...);
await tx.apply(...);
await atoms.invoke(...);
await refs.peek(...);
~~~

Tool schemas should generate TypeScript declarations the same way Codex
code-mode augments tool descriptions today. This gives agents and humans an
inspectable contract without hand-writing prompt prose for every parameter.

ALL_TOOLS should evolve into a richer registry:

~~~js
const rustRefactors = registry.find({
  domain: "refactor",
  language: "rust",
  effects: ["edit"]
});
~~~

The direct model-facing tool surface could even collapse to narf_exec and
narf_wait, mirroring Codex code_mode_only, while internal bindings remain rich.

### 3. Ref store

bro-harness clipboard should generalize into a typed ref store rather than
remaining only clip: registers.

Possible namespaces:

- ref:text/*
- ref:json/*
- ref:slice/*
- ref:plan/*
- ref:diag/*
- ref:diff/*
- ref:atom/*
- ref:trace/*

The store should preserve the clipboard virtues:

- bounded size;
- explicit peek/render;
- hashes and previews;
- session persistence where appropriate;
- durable enough for resume;
- safe enough that huge values do not leak into model context by default.

The current clip: namespace can remain as a user-facing alias for simple text
and JSON refs.

### 4. Promise and obligation model

Harness promises are currently same-dispatch and not persisted. NARF likely
needs two layers:

- local promises for in-cell or same-dispatch async work;
- durable operation handles for atom invocations, workflow runs, long refactor
  jobs, and tool calls that cross harness resume boundaries.

The useful lifecycle is:

~~~text
Promise<T> -> Ref<T>
~~~

A resolved promise should be able to deposit its output directly into a ref,
just as shell_run(mode="promise", stdout_to=...) already does for stdout.

Repair obligations should be first-class, not just runner internals. They are
promise-adjacent but semantically different:

~~~text
Obligation<DiagnosticSet, Plan> means:
  the transaction may continue,
  but cannot commit until this diagnostic/effect obligation is resolved.
~~~

### 5. Transaction model

The refactor runner's rollback machinery should become the general transaction
primitive for code-changing scripts.

Inside a Tx, NARF should allow:

- applying refactor plans;
- applying slice-derived plans;
- running commands with declared touches;
- capturing diagnostics;
- opening and resolving obligations;
- collecting trace;
- committing only after required validation passes.

Outside a Tx, mutating operations should either refuse or require explicit
operator confirmation, depending on surface and policy.

### 6. Code discovery surfaces

NARF should make code discovery first-class and ref-producing:

~~~js
const refs = await code.refs({
  project,
  symbol: "crate::refactor::run_with_ctx"
}, {
  into: "ref:json/run-with-ctx-refs"
});
~~~

Discovery should include:

- symbols;
- references;
- AST nodes;
- slices;
- hybrid semantic/text search;
- call sites where supported;
- ownership/module boundaries;
- parse and LSP availability status.

The important rule: discovery results should be usable directly as inputs to
plans. An agent should not have to manually translate search output into byte
ranges when the system already knows the coordinates.

### 7. Atom integration

Atoms should be callable as typed functions, but they should not become opaque
magic. A NARF atom invocation should expose:

- input schema;
- output schema;
- effect declaration;
- owner and resume handle;
- trace;
- produced refs;
- opened obligations;
- final status.

That means an atom can participate in a transaction:

~~~js
const atomRun = await atoms.invoke("refactor.rust.moveItem", input, {
  mode: "promise",
  tx
});

const atomOut = await promises.wait(atomRun, {
  into: "ref:atom/move-item"
});

await tx.absorb(atomOut.effects);
~~~

This creates a path where deterministic refactor tools, profile-backed agents,
workflow-backed agents, and adapter-backed tools can compose under one runtime.

### 8. Soft daemon dependency

The current checked-in invariant says bro-harness must not have a runtime
dependency on the daemon. For this session, the operator explicitly relaxed
that future invariant for design exploration: a future harness may use a soft
dependency on blackbox daemon capabilities to enable atomics.

NARF can use that future direction without making every run daemon-hard:

- local/shared crates provide base refs, promises, shell, workspace, and JS
  runtime;
- daemon-backed bindings provide code index, semantic search, refactor backend,
  atom catalog, and durable coordination;
- scripts can inspect capability availability and fail closed for semantic
  operations that require LSP or daemon state.

This keeps the good property of independent harness operation while allowing
rich atom/refactor integration when the daemon is available.

### 9. Governance and safety

NARF should preserve the strict parts of the current systems:

- operator-authority flags are never inferred by agents;
- LSP-backed semantic refactors fail closed when LSP is unavailable;
- mutating command steps declare touches when needed;
- atom-dispatched runs respect narrow command allowlists;
- registered project and stale-hash checks stay in the apply path;
- direct render of large refs is opt-in and bounded;
- tools expose effects and permissions in the schema/registry.

The JS runtime should be powerful for composition, not a host escape hatch.
Codex's current runtime restrictions are instructive here: explicit host tools,
explicit globals, and no ambient filesystem/network access from JS itself.

### 10. First spike

A plausible first spike:

1. Add a narf_exec/narf_wait prototype to bro-harness, inspired by Codex
   code-mode.
2. Expose current bro-tools as JS bindings with generated TypeScript
   declarations.
3. Generalize clipboard registers into Ref<T> handles while keeping clip:
   compatibility.
4. Let promise completion write to typed refs, not only stdout clipboard
   registers.
5. Add daemon-backed optional bindings for bbox_code_symbols,
   bbox_refactor_status, bbox_refactor_plan, bbox_refactor_run, atoms, and
   slice tools.
6. Implement one proof script: discover Rust item -> plan refactor -> apply in
   tx -> cargo check capture -> compile-fix round -> commit or rollback.
7. Record all operations as a trace that can be rendered as a short summary or
   reopened by another agent.

Success for the spike would not be a beautiful API. Success would be proving
that an agent can perform a non-trivial refactor with most intermediate code,
diagnostics, plans, and traces staying out of model context while still being
inspectable and recoverable.

## Open questions for the next pass

- Should Ref<T> live primarily in the harness session, the daemon, or a shared
  store abstraction with both backends?
- Are durable promises part of bro-harness, blackbox daemon coordination, or an
  atom/workflow layer?
- How much TypeScript declaration generation can be reused from Codex's model,
  and what needs Rust-native schema support?
- Should narf_exec be the only model-facing metatool in some modes, or should
  direct tools remain visible alongside it?
- How should transaction ownership work when a script invokes an atom that
  itself wants to apply plans?
- What is the cleanest boundary between raw MCP tools, ergonomic JS namespaces,
  and atom capability contracts?
- Can repair obligations be generalized without making every command runner a
  refactor runner?
- What are the right persistence and garbage-collection rules for refs, traces,
  diagnostics, and plans?

## Working summary

The exciting version of NARF is not "bro-harness with more tools." It is a
runtime where the things agents already need for serious code work are native:
refs instead of pasted blobs, promises instead of polling chatter, transactions
instead of hopeful edits, diagnostics as typed values, atoms as callable
capabilities, and code discovery as composable input to plans.

The reason the Codex JS example landed is that it made tool composition feel
ordinary. The reason the blackbox refactor tools matter is that they make edit
effects disciplined. The reason bro-harness refs/promises matter is that they
keep values and lifecycle out of the transcript. The reason atoms matter is
that they give reusable agentic expertise a contract.

NARF is the marriage of those ideas.
