---
title: "The harness–daemon boundary: in-process consolidation"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - bro-harness
  - orchestration
  - surfaces
  - fleet-tui
brief: "Restates the bro-harness/daemon boundary for the post-CLI, API-native era. The 'no daemon runtime dependency' rule was a temporary scaffold; with codex/vibe/claude/opencode CLI holes dropped and all providers API-native through bro-harness, the harness becomes a linkable crate the daemon runs in-process. The load-bearing half of the invariant survives as a compiler-enforced acyclic DAG. Capabilities and wire messages cross through a shared contract bottom — a small family of crates (bro-core / bro-protocol / bro-capabilities) cleaved by consumer, not by domain taxonomy — so the thin fleet client links only the protocol it needs and gRPC is unnecessary (the Rust types are the contract, serde is the wire). Consolidation collapses the fleet coordinator into the singleton daemon (TUI = thin view, agents = bros), turns corpus/atom lookups into in-memory trait calls, makes the MCP surface an in-process filter evaluation reusing the shipped surface evaluator, and keeps a V8/shell isolation gradient so 'one process' isn't 'one fault domain.' Supersedes the per-store fleet coordinator placement; refines narf-draft2 §6; extends mcp-surfaces."
---

# The harness–daemon boundary: in-process consolidation

> **Status.** This is the synthesis of a design jam, not a ratified plan. The
> *direction* (API-native consolidation, harness-as-crate, a shared contract
> bottom, singleton execution) is argued as the target; the **Open decisions**
> section marks the forks that are genuinely undecided. Verify against code
> before treating any of it as current behavior.

## 0. Thesis

> The "no daemon runtime dependency" rule was a temporary scaffold to find
> bro-harness's shape. Now that all surviving providers are **API-native through
> bro-harness** (the codex/vibe/claude/opencode CLI holes are legacy), the
> harness becomes a **linkable crate the daemon runs in-process**. The
> load-bearing half of the invariant survives, upgraded from a convention to a
> **compiler-enforced acyclic DAG**. Capabilities and cross-process messages
> meet at a **shared contract bottom** — a small family of crates cleaved by
> *who consumes them*, not by domain taxonomy — so the thin fleet client links
> only the protocol it needs, the harness never depends on the daemon, and gRPC
> is unnecessary (the Rust types *are* the contract). Consolidation then
> collapses three things into the singleton daemon: the fleet coordinator (TUI
> becomes a thin view, agents become bros), corpus/atom lookups (in-memory trait
> calls instead of MCP round-trips), and the MCP surface (an in-process filter
> evaluation reusing the shipped surface evaluator). A V8/shell isolation
> gradient keeps "one process" from becoming "one fault domain."

## 1. Why the invariant existed, and why it comes down now

The checked-in rule — *"bro-harness must never have a runtime dependency on the
daemon; the only daemon↔harness contract is the Claude stream-json envelope"* —
was an **under-construction sign**. During early fleet work, agents kept
conflating daemon concerns into the harness; the rule drew a hard boundary so
bro-harness could reach a coherent, provider-agnostic shape. It did its job
(`crates/bro-harness/src/mcp.rs` is a clean MCP-client-and-stream-json producer).
The sign was always meant to come down at the next step.

Two facts make now the time:

- **The CLI holes are legacy.** Codex and Vibe are subsumed into bro-harness;
  `claude -p` moves to API pricing (no CLI cost advantage to preserve); OpenCode
  is redundant once the OpenAI and Anthropic transports exist natively. The
  surviving dispatch targets are **our code on an HTTP transport**, not foreign
  binaries.
- **The boundary was never an architectural firewall.** The harness is *already*
  an MCP client (`mcp.rs`), and the daemon already injects itself
  (`configure_dispatch_mcp_env` exports `BLACKBOX_MCP_URL`). The rule was a
  *policy* about the harness's **core loop** not requiring the daemon — never
  about whether the agent inside it can reach daemon tools.

So "relax the invariant" splits into one real claim and two non-claims:

| Claim | Verdict |
|---|---|
| Agent-in-harness uses daemon tools | already true (dispatch injection) |
| Harness becomes a crate the daemon links and drives in-process | **the new move** |
| Harness *core loop* requires the daemon to run | **never** — the line that stays |

## 2. The crate graph: a shared contract bottom

Today bro-harness is **binary-only** (`crates/bro-harness/Cargo.toml`: `[[bin]]`,
no `[lib]`) and the `blackbox` crate has **zero compile coupling** to it
(`bro-harness`/`bro-tools` are workspace *members*, not *dependencies*). The
process boundary enforces the invariant by **total decoupling — no edge either
direction** — which is also why nothing is shared.

Crate-ification adds a `[lib]` target to bro-harness and introduces a **shared
contract bottom**: the dependency-inversion pattern (a C# "Abstractions project,"
in Rust just crates of traits + types) that lets two components agree on
contracts without either depending on the other. Rust's interface is the
**trait**; the bottom crates hold the traits + the serde data types that cross
boundaries, and depend on essentially nothing.

**Cleave the bottom by consumer, not by domain taxonomy.** The temptation is to
make one crate per topic (atoms, refactor, control). That's what *modules* are
for, and they're free. Promote a module to its own crate only when it earns a
**distinct consumer set** or a **distinct change/stability rate**. Mapping the
real consumers:

| Consumer | Needs |
|---|---|
| **fleet-client** | control plane (commands/events) + status/roster/transcript DTOs |
| **bro-harness** | capability traits it calls (atom/corpus/refactor) + status it emits + control plane it receives |
| **blackbox** | implements the capability traits + produces everything |

The consumers cleave roughly two ways plus a kernel, giving three bottom crates:

```
   bro-capabilities                  bro-protocol
   Atom/Corpus/Refactor traits       control plane (cmd/event, seq) + status/roster/
   (+ DTOs in their signatures)      transcript DTOs
   consumers: harness + daemon       consumers: client + harness + daemon
        │                                  │
        └──────────────┬───────────────────┘
                       ▼
                   bro-core
        ids, refs (AtomRef, SessionId, TaskId), error/time types
        consumer: everyone · serde only · ~no deps

   implementers:  blackbox ──▶ bro-harness ──▶ bro-tools   (all link down)
   thin client:   fleet-client ──▶ bro-protocol + bro-core (never bro-capabilities)
```

`bro-core` is the kernel that absorbs the "two domains both need this id/ref"
cases, so the domain crates never depend on each other. `bro-capabilities` and
`bro-protocol` are siblings over it. Everything points *down*; the bottom points
nowhere; acyclicity is trivial.

This is **stronger** than the old rule, not weaker: today a harness→daemon cycle
is impossible only because there are no edges at all; after crate-ification,
`bro-harness → blackbox` is a **compile error**. The benign directions (daemon
drives harness; everyone shares the bottom) are permitted; the forbidden
direction is structurally unbuildable. A structural consequence: `bro` lives *in*
the `blackbox` crate (`src/cli.rs`), so "the `bro` CLI inherits the harness
flags" **is** "`blackbox` depends on `bro-harness`" — one edge, decided once;
in-process-vs-subprocess is then a per-consumer choice on top.

### Capabilities cross as contract-defined traits

The harness needs daemon capabilities (atom catalog, code graph, refactor
backend). These cross as **traits defined in `bro-capabilities`** — implemented
by the daemon, called by the harness, owned by neither (the dependency-inversion
shape; cf. Codex's `CodeModeSessionDelegate`, `codex-rs/code-mode/src/service.rs:84`):

- `bro-harness` calls `self.atoms.lookup(ref)` against `bro_capabilities::AtomSurface`
  — never `blackbox::atoms::lookup(...)`.
- `blackbox` implements `bro_capabilities::AtomSurface` against its **in-memory**
  stores and injects the impl when it runs a session in-process. The call is a
  function call: no MCP, no robustness layer.
- The **standalone** `bro-harness` binary injects absent/no-op impls → corpus
  capabilities fail closed.

Putting the traits in the bottom crate dissolves a latent cycle: their signatures
reference `AtomRef`, which lives in `bro-core`, so neither implementer "owns" the
contract. This is the lynchpin that makes "in-memory atom calls" and "the harness
crate doesn't depend on the daemon" both true. "Harness runs without blackbox is
valid" survives as a **crate property** (absent impls) even as the *product*
consolidates into the daemon.

### The contract crate is the wire schema — so no gRPC

`bro-protocol` doubles as the daemon↔client wire schema: the seq-ordered command
plane (`SteerCommand`, `InterruptCommand`, `DispatchCommand` with `command_id`/`seq`)
and event/snapshot plane (`TranscriptAppended`, `RosterUpdated`, `Snapshot`) are
serde types both sides import. For a same-language (Rust), same-host,
**co-versioned** (shipped together) system, this beats gRPC outright:

| | gRPC | shared `bro-protocol` crate |
|---|---|---|
| Contract source | `.proto` + codegen per side | the Rust types **are** the contract |
| Enforcement | runtime wire-compat | **compile-time** — change a struct, both sides fail to build |
| Impedance | proto ↔ Rust mapping | none |
| Toolchain | protoc, build scripts | `cargo` + `serde` |

gRPC earns its keep for polyglot/cross-network/independently-versioned clients —
none of which apply. Keep `bro-core`/`bro-protocol`/`bro-capabilities` **pure**
(types + traits + serde, no I/O, not even tokio); the byte transport is a thin
layer above (UDS + newline-delimited JSON per the coherence doc, or the daemon's
existing SSE/HTTP). The schema is the contract; the transport is replaceable
plumbing.

> **Discipline (write this down so it doesn't sprawl):** crate boundaries follow
> dependency cleavage, not topic. Start with three bottom crates, `mod`-grouped
> by domain inside; promote a module to its own crate only when it gains a
> distinct consumer or stability rate. The test for "does this type belong at the
> bottom?" is: *does a contract-only consumer (the fleet client) need it?* If no,
> it stays in the implementing crate.

## 3. One library, two transports — and the daemon runs it in-process

The harness logic is one library; how a consumer reaches it is a transport choice
keyed to the isolation/control tradeoff:

- **In-process** (function call, typed events, method-call control plane) — the
  daemon (for the API-native providers), the `bro` CLI standalone, fleet's
  interactive sessions.
- **Subprocess** (spawn the thin binary, stream-json envelope, control over
  stdin) — retained for **isolated leaves** (§5) and as the legacy/foreign-CLI
  adapter shape if one is ever re-added.

This **inverts** the position taken before the API-native premise was on the
table. That earlier "keep spawning subprocesses for fan-out" rested on three
legs; the premise kicks out two:

1. **Env isolation — gone.** Per-session account env (`CLAUDE_CONFIG_DIR`,
   `CODEX_HOME`, the drone account homes) being process-global was the strongest
   subprocess argument — but it was an **artifact of the CLI holes**. API-native
   makes a credential an **auth token + base URL passed as session config**, not
   process env. The collision disappears; the repo's `test_env_lock()` (which
   exists because env is global) is irrelevant to API-native sessions.
2. **Foreign-CLI uniformity — gone.** "stream-json stays as the uniform
   subprocess contract" only mattered while foreign CLIs existed. None survive.
3. **Crash isolation — the survivor**, handled by the gradient in §5.

## 4. API-native, no CLI holes

The migration is wholesale — every CLI-shaped provider is dropped:

- **codex, vibe** — subsumed into bro-harness's native transports; the CLI paths
  are frozen as legacy.
- **claude `-p`** — moves to API pricing (June 15), erasing the CLI cost
  advantage; dispatch goes through the native Anthropic transport.
- **OpenCode/Inception** — redundant once the OpenAI and Anthropic transports
  exist natively; cut.
- **Gemini** — dead: the CLI was deprecated in favour of the new Antigravity CLI,
  which doesn't work properly. Dropped.
- **Copilot** — dropped: it backs onto OpenAI/Anthropic anyway, is closed to new
  customers, and hit grandfathered users with a ~33× cost increase. Not worth
  carrying.

So `Provider::build_exec_args`/`build_filter_args` for all of these, the
OpenCode/Inception transport, and the `claude -p` subprocess path are deleted.
All dispatch is API-native via the harness transports (Anthropic, OpenAI
Responses, OpenAI chat). If a CLI-shaped adapter is genuinely needed later,
re-add it as a subprocess leaf behind the same `Session` API.

## 5. In-process execution + the isolation gradient

The daemon runs API-native sessions in-process. The surviving cost is **crash
and restart blast radius** — the daemon now holds the corpus *and* every live
session *and* the RPC surface. Design for it, don't hand-wave it:

- **Tokio gives task-level isolation for ordinary panics.** A logic panic in one
  session's turn is caught at the task boundary; the daemon and siblings survive
  — **if** the build is `panic = "unwind"` (not `abort`) and shared corpus/atom
  state uses **non-poisoning locks** (`parking_lot`), so a task panicking under a
  write lock doesn't poison it for everyone.
- **Task isolation does *not* contain OOM or FFI/V8 crashes.** Hence the
  gradient: cheap, safe work (HTTP-transport turn loops, in-memory atom/ref
  state) runs in-process; **dangerous execution leaves run isolated** — NARF's
  V8 cells especially (V8 + arbitrary JS + thread-per-isolate is the exact
  OOM/FFI surface to keep out of the daemon core), and untrusted/heavy shell tool
  ops. The daemon runs sessions in-process and *dispatches* V8/shell execution to
  **separate worker processes** — the process boundary is the v1 isolation
  (crash containment). OS-level sandbox *hardening* of those workers
  (namespaces/Seatbelt/landlock) is a separate, forward-looking design —
  [`leaf-sandbox-isolation.md`](./leaf-sandbox-isolation.md) — **not v1**.
- **Restart** drops live sessions. Mitigations in hand: session persistence +
  resume, the dev/prod daemon split, graceful drain on shutdown. The starting
  topology is **decided** (§12.1: monolith + isolated leaves); the corpus/
  execution split is kept as a trait-enabled escape hatch, not initial work.

Consolidate where it pays (atoms, corpus, control plane); isolate where it
actually protects you (V8, shell). "One process" must not become "one fault
domain for the entire system."

## 6. Tool bindings: in-process, skip the wire

In-process execution lets harness agents call tools as **direct `Tool::call`
dispatches** instead of MCP round-trips. The reference for what this deletes is
Daystrom's `SdkMcpServer` (`daystrom-mk2/.../Tools/SdkMcpServer.cs`): it keeps
tool *handlers* in-process but still feeds them to a CLI agent over a **loopback
HTTP MCP listener** — per call a TCP round-trip, JSON-RPC framing, and payload
serialize/deserialize ×2. It is "in-process handlers, out-of-process agent" —
halfway home, forced there because its agent is a subprocess. bro-tools already
ported the handler half (`Tool::call`, `tool.rs:89`, "Ported from daystrom's
McpToolDefinition / ToolResult"); consolidation removes the wire because the
agent is in-process too.

**Scope: this kills *self*-MCP traffic, not all MCP.** Genuinely external MCP
servers stay on the wire (they *are* separate processes). The deletion targets
the harness dialing blackbox to call blackbox's own surfaces.

Two serialization layers, both eliminable for the payloads that matter:

- **MCP-transport serialization** — killed by in-process binding (the trait call).
- **Model-context serialization** — killed by **refs/NARF**: a large result stays
  a host-side `Ref<T>`, only a handle/preview enters the prompt. The model-JSON
  boundary itself is unavoidable (the LLM speaks JSON), but refs keep big
  payloads out of it.

Net: `model → tool_call → HTTP MCP → daemon → result → HTTP MCP → harness → JSON
into context → model` becomes `model → tool_call → cx.corpus.symbols(...) → Ref
handle → tiny preview → model`. Out of context **and** off the wire.

### The MCP surface becomes an in-process filter evaluation

The admission policy is **not** a hand-rolled `ToolFilter` — it is the shipped
**surface packet evaluator**, whose pure core is already transport-agnostic
(`src/server/surface.rs`):

- `build_surface_entity(surface, project)` (`:133`)
- `evaluate_tool_surface(packets, entity, project) -> ToolSurfaceVerdict` (`:145`)
- `tool_visible(name, decision, universe)` (`:203`), `filter_tools(...)` (`:210`)

The rmcp wire head (`src/server/handler.rs` `list_tools`/`call_tool`) and the
dispatch-filter merge (`resolve_dispatch_filters`) are two existing consumers.
**The in-process binding is a third consumer of the same pure core**, differing
only at the edges:

1. **Entity from dispatch identity, not URL.** Instead of
   `extract_surface_from_uri(query)` (`surface.rs:228`), build the surface entity
   from brofile, `dispatch_origin`, project, recursion-guard state. The agent's
   selector is *who it is*, not `?surface=`.
2. **Enforce by what gets bound + a call-time check.** `filter_tools` decides
   which `Tool`s land in the bound tool object (Codex's
   `build_tools_object`/`enabled_tools`, `codex-rs/code-mode/src/runtime/globals.rs:46`);
   `tool_visible` is the in-process `Tool::call` boundary — honoring the surface
   doc's rule that `list_tools` filtering alone is insufficient and the call path
   must reject hidden tools by name. `Deny` → refuse to build the session.

Payoffs: one **packet** is the single policy authority for wire callers *and*
in-process agents (auditable/replayable via `bbox_mcp_surface action=replay`,
versioned, hot-editable); because both heads call `evaluate_tool_surface`, **drift
is structurally impossible** — `replay` output is exactly what the agent sees.
And the in-process head is the *cleanest* consumer: it skips the
`Provider::build_filter_args` per-provider CLI-flag translation entirely
(subprocess machinery), filtering the Rust registry directly. The two filter
layers (surface packet + dispatch recursion guard) still compose with
disallow-wins, just at the binding instead of at `McpFilters`.

## 7. The singleton fleet system

The fleet's split-brain problem (two TUIs, disjoint state) has one clean model:

> For a given user, running fleet anywhere yields the **same view of one fleet
> system**; the only thing local to a launch is the **cwd** (and the processes
> that inherit it).

That is a singleton-system statement, and the daemon is already that singleton
(it dispatches and owns bros). So:

- **The fleet TUI is a thin view/controller**, not an owner. It does *not* link
  bro-harness or blackbox — it links **`bro-protocol` + `bro-core` only**,
  renders roster/transcript, and sends commands over the transport. View-local
  state (selection, scroll, composer draft, recall cursor) stays in the terminal;
  *system* state is the daemon's. (Structural consequence: the thin `bro`
  commands — dashboard/fleet/tail — extract out of the `blackbox` crate into a
  client crate that links only the contract bottom.)
- **Fleet agents are bros.** A fleet agent is a daemon-owned session — the same
  thing the daemon already dispatches. Nothing in "fleet" (classifier companions
  = sub-bros, mailbox = daemon coordination, input history = durable state) is
  not already a daemon concept.

This **supersedes** the per-store local-coordinator placement in
[`../fleet-tui/backlog-multi-instance-coherence.md`](../fleet-tui/backlog-multi-instance-coherence.md).
That doc's coordinator was an artifact of routing *around* the
under-construction invariant; with the sign down, it collapses into the daemon.
Its **seq-ordered command protocol** (every mutation gets a `command_id` +
monotonic `seq`; snapshot-then-stream attach; presence) is kept wholesale — it is
correct client-sync design regardless of which process owns the state, and it now
lives in `bro-protocol`.

## 8. The live control plane (steer / interrupt)

The interactive control plane already exists in the harness; the gap is purely
that the **daemon doesn't expose it**. Three distinct operations (canonical in
[`../fleet-tui/fleet-tui-cockpit.md`](../fleet-tui/fleet-tui-cockpit.md)):

1. **Steer while active → queues to the turn boundary, no cancel** (`:372`).
   `crates/bro-harness/src/agent_loop.rs` already implements this — inputs
   received while a turn runs are queued and replayed at the next model-call
   boundary (gated by `can_steer()`).
2. **Interrupt → cancel with buffer reconciliation** — `control_request`
   interrupt; role-alternation repair (`note_interrupted`, per
   [`bro-harness-api-robustness.md`](./bro-harness-api-robustness.md)).
3. **Interrupt-and-redirect** — a queued steer dequeued and sent immediately on
   interrupt (`:374-375`).

The daemon's continuation surface today is turn-boundary `bro_resume` only. The
upgrade is to **expose the harness's existing control plane over the
`bro-protocol` command plane** — `bro_steer` (enqueue, applied at boundary, no
cancel), `bro_interrupt`, interrupt-and-redirect — with the daemon owning the
live control channel (a method call on the in-process session, not a stdin
write). This generalizes beyond fleet: interactively-steerable bros help any
long-running orchestration. (`acquire-drone.md` is orthogonal — pre-dispatch
account selection; its only continuation primitive is the turn-based `bro_resume`
that needs this extension.)

## 9. NARF placement under consolidation

NARF (see [`../../research/harness/narf-draft2.md`](../../research/harness/narf-draft2.md))
splits at the durable/ephemeral line, like everything else:

- **Daemon-side (singleton, survives resume):** the ref store, durable
  promises/operation handles, the atom-invocation tree, traces. Coordination
  state the daemon already owns for bros; atom/corpus lookups from a cell are
  **in-memory `bro-capabilities` trait calls** (§2), not MCP.
- **Isolated leaf (ephemeral, per cell):** the V8 runtime executing one
  composition cell, materializing refs by handle and applying plans to the local
  worktree — kept out of the daemon core per the gradient (§5).

This **refines narf-draft2 §6**: when the harness runs in-daemon, the
corpus-capability seam is the in-memory **trait**, not an MCP client; the MCP path
remains only for standalone/degraded mode and external callers.

NARF is a **config-selected model-projection mode**, not the universal surface
(narf-draft2 §5): composition-capable models get the `narf_exec` sandbox; lower-
tier models (classifiers, supervision advisors, cheap leaf workers) keep a
**conventional flat-tool surface**. Both modes share the §6 admission + in-process
bindings — only the projection to the model branches.

## 10. The deletion ledger

The consolidation is plausibly **net-negative code**.

**Delete:** codex/vibe/copilot/gemini CLI arg-builders + dispatch branches;
OpenCode/Inception transport; the `claude -p` subprocess path; stream-json
*parsing* on the daemon side for in-process sessions; the MCP-client-for-corpus
robustness wrapping; `Provider::build_filter_args` translation for harness
agents; the fleet in-process `FleetOrchestrator` duplication; the entire
per-store fleet-coordinator design; `BRO_HARNESS_BIN` discovery/spawn for the
common path.

**Add:** a `[lib]` target + public API on bro-harness; the three contract crates
(`bro-core`/`bro-protocol`/`bro-capabilities` — types + traits, no logic) and the
in-memory capability impls (which wrap *existing* structures); a thin transport +
client crate; session-as-tokio-task plumbing. **Not** added: gRPC/proto
toolchain — the contract crate is the schema. The additions are thinner than what
they replace.

## 11. The restated invariant

The replacement for the under-construction sign, precise and compiler-backed:

> The shared **contract bottom** — `bro-core` (ids/refs/errors), `bro-protocol`
> (control plane + status DTOs), `bro-capabilities` (Atom/Corpus/Refactor traits)
> — holds the types and traits that cross boundaries; it is pure (serde, no I/O)
> and depends on nothing. `bro-harness` is a library; `blackbox` (daemon + `bro`
> CLI) may link it (in-process) or spawn its binary (subprocess + stream-json),
> per consumer. The compile DAG is `blackbox → bro-harness → bro-tools`, with all
> three plus the thin client depending **down** into the contract bottom — acyclic
> and compiler-enforced. **`bro-harness` must never depend on `blackbox`** (a
> compile error, not a convention). Daemon capabilities reach the harness only via
> `bro-capabilities` traits the daemon implements (in-process) or a runtime MCP
> client (standalone) — never a compile or RPC dependency *from* the harness — and
> **fail closed** when absent. The thin fleet client depends on **`bro-protocol` +
> `bro-core` only**, never `bro-capabilities` or either implementer. In-process
> consumers pass per-session env as **explicit config**; the harness must not read
> or mutate process-global env for session identity. Crate boundaries follow
> consumer cleavage, not topic; promote a module to a crate only on a distinct
> consumer or stability rate.

The repo's existing governance invariants (RX-V1 operator-authority opt-outs,
RX-V2 atom command allowlist, RX-V3 LSP fail-closed) carry forward unchanged.

## 12. Open decisions

The direction above is argued; these forks are not settled:

- **One process or two — DECIDED: monolith + isolated leaves.** `blackbox` owns
  execution in-process; V8 cells and shell ops run as separate worker processes
  for crash containment (§5). The corpus/execution split (a per-host execution
  singleton sibling of the corpus daemon) is **not** initial work — the
  `bro-capabilities` trait makes it a swap-the-injected-impl change, so it stays a
  pre-enabled escape hatch. **Triggers** to pull it: corpus-side crashes observed
  killing live sessions; interactive sessions dropped often by corpus-driven
  restarts; corpus maintenance needing restart while sessions stay live. Until a
  trigger fires, monolith stands; the prerequisites are the `panic = "unwind"` +
  `parking_lot` discipline (§5). The reattach question (socket/fifo stdin the
  owner re-dials) only matters if the split is taken.
- **Transport plumbing** (not the schema — that's `bro-protocol`). UDS +
  newline-delimited JSON (coherence doc) vs riding the daemon's existing
  SSE `/tail` + HTTP. gRPC is **decided against** (§2). This is a thin, reversible
  pick.
- **NARF tx vs saga** for nested atoms — the edit-rollback composition question
  from narf-draft2 §8 (transaction unwind vs compensate-forward).
- **Standalone harness scope.** Does the standalone binary stay a maintained
  product (degraded, absent capability impls) or a test/edge artifact only?
- **Contract granularity beyond three.** Whether any of `bro-protocol`/
  `bro-capabilities` later splits further (e.g. a standalone atom-runner that
  links only atom contracts gives `bro-capabilities` a reason to subdivide). Defer
  until a real consumer appears.

## 13. Relationship

- **Restates** the CLAUDE.md "bro-harness shares code, never runtime" invariant
  (§11) — keeps the load-bearing half, makes it compiler-enforced through the
  contract bottom.
- **Supersedes** the per-store coordinator placement in
  [`../fleet-tui/backlog-multi-instance-coherence.md`](../fleet-tui/backlog-multi-instance-coherence.md);
  keeps its seq-ordered command protocol (now in `bro-protocol`).
- **Refines** [`../../research/harness/narf-draft2.md`](../../research/harness/narf-draft2.md)
  §6 (in-memory `bro-capabilities` traits replace the MCP soft-dep seam when
  in-daemon).
- **Extends** [`../surfaces/mcp/mcp-surfaces.md`](../surfaces/mcp/mcp-surfaces.md)
  with the in-process binding as a third consumer of `evaluate_tool_surface`.
- **Spins out** [`leaf-sandbox-isolation.md`](./leaf-sandbox-isolation.md) — the
  OS-level sandbox hardening of the V8/shell worker leaves (proposed, forward-
  looking; the §5 process boundary is the v1 isolation).
- **Hub:** [`bro-harness.md`](./bro-harness.md); control-plane detail in
  [`../fleet-tui/fleet-tui-cockpit.md`](../fleet-tui/fleet-tui-cockpit.md) and
  [`bro-harness-api-robustness.md`](./bro-harness-api-robustness.md).
