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
---

# The harness–daemon boundary: in-process consolidation

> **Status.** This was the synthesis of a design jam; it is now **partially
> implemented** on `beta/blackbox-v2`. The *direction* (API-native
> consolidation, harness-as-crate, a shared contract bottom, singleton
> execution) is the target; most of it is built and tested, with one large
> piece (the §7 thin-client crate decoupling) deliberately staged. See
> **§15 Implementation status** for what is live vs. remaining, tied to
> commits. The **Open decisions** section marks forks that were genuinely
> undecided. Verify against code before treating any of it as current behavior.

## 0. Thesis

> The "no daemon runtime dependency" rule was a temporary scaffold to find
> bro-harness's shape. Now that all surviving providers are **API-native through
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
> evaluation reusing the shipped surface evaluator). In-process V8
> (isolate-contained) plus supervised shell child processes keep "one process"
> from becoming "one fault domain" — OS sandboxing is deferred, because a
> trusted-agent box gets accident-containment from supervision, not security from
> a sandbox.

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
- **Subprocess** (spawn the thin harness binary, stream-json envelope, control
  over stdin) — retained only as the **legacy/foreign-CLI adapter shape** if one is
  ever re-added. CLI-shaped providers are dead-dead, so this is **not** a live
  provider-driving escape hatch: any future external adapter must justify itself
  behind the session/capability API. (Shell *tool* ops are separately child
  processes the in-process harness spawns and supervises — §5 — not the harness
  running as a subprocess.)

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
3. **Crash isolation — the survivor**, handled in §5 (in-process V8 + supervised
   shell), much lighter than a worker-process gradient.

## 4. API-native, no CLI holes

The migration is wholesale — every CLI-shaped provider is dropped:

- **codex, vibe** — subsumed into bro-harness's native transports; the CLI paths
  are frozen as legacy.
- **claude `-p`** — moves to API pricing (June 15), erasing the CLI cost
  advantage; dispatch goes through the native Anthropic transport.
  exist natively; cut.
- **Gemini** — dead: the CLI was deprecated in favour of the new Antigravity CLI,
  which doesn't work properly. Dropped.
- **Copilot** — dropped: it backs onto OpenAI/Anthropic anyway, is closed to new
  customers, and hit grandfathered users with a ~33× cost increase. Not worth
  carrying.

So `Provider::build_exec_args`/`build_filter_args` for all of these, the
All dispatch is API-native via the harness transports (Anthropic, OpenAI
Responses, OpenAI chat). If a CLI-shaped adapter is genuinely needed later,
re-add it as a subprocess leaf behind the same `Session` API.

## 5. Execution isolation: in-process V8, supervised shell

The daemon runs API-native sessions in-process. The crash/restart blast-radius
concern is real — the daemon holds the corpus *and* every live session *and* the
RPC surface — but the right isolation is much lighter than a worker-process
gradient, and a **trusted-agent** threat model (the agents are the operator's own,
on the operator's machine) deflates most of it.

- **Tokio gives task-level isolation for ordinary panics.** A logic panic in one
  session's turn is caught at the task boundary; the daemon and siblings survive
  — **if** the build is `panic = "unwind"` (not `abort`) and shared corpus/atom
  state uses **non-poisoning locks** (`parking_lot`). These two are prerequisites,
  not nice-to-haves.
- **V8 runs in-process.** The isolate, not a process, is the containment unit —
  the codex code-mode / Deno embedding model. A per-isolate heap bound +
  `add_near_heap_limit_callback` → `terminate_execution` contains script OOM;
  `terminate_execution` from another thread kills runaway loops; deleted globals
  (`console`/`Atomics`/`SharedArrayBuffer`/`WebAssembly`) deny ambient host access.
  This keeps NARF cells' capability calls **in-memory** (§6 applies to composition
  cells, not just flat-tool sessions). The price of in-process is config you must
  get right — see **Future concerns**.
- **Shell ops are already child processes.** `exec` spawns a separate process
  inherently; there is no "isolate shell in a worker" abstraction to build. What's
  worth doing is cheap and reliability-motivated, *not* security: a **timeout** to
  kill hangs, and a **ulimit/cgroup cap** on the spawned child so a runaway makes
  the OOM-killer target *it*, not the daemon. Ref I/O stays the existing clipboard
  ABI (`shell_run stdin_from`/`stdout_to`).
- **Cell-bounded `Tx` → rollback on abort.** `Tx` lifetime is the cell (narf-draft2
  §4); if a cell is terminated (heap-limit `terminate_execution`, a caught panic, a
  timeout), its uncommitted `Tx` rolls back. In-process, this is a drop-guard, not
  a cross-process protocol — the worker-leaf bridge an earlier draft sketched here
  is unnecessary once V8 is in-process.
- **OS sandboxing is not in the v1 picture.** Wrapping shell children in
  namespaces/Seatbelt confines *scope* (worktree, network, PIDs) but does **not**
  deny capability — a trusted agent with file-write + execute can do anything
  within its scope regardless of bash-vs-python. On a single-user box that is
  accident-containment, not security, so it stays a proposed escape hatch
  ([`leaf-sandbox-isolation.md`](./leaf-sandbox-isolation.md)) — see **Future
  concerns**.
- **Restart** drops live sessions. Mitigations in hand: session persistence +
  resume, the dev/prod daemon split, graceful drain on shutdown. The corpus/
  execution split (§12.1) stays a trait-enabled escape hatch.

### Future concerns

- **In-process V8 must be configured correctly or it can take the daemon.** With
  no near-heap-limit callback wired, V8's OOM path `abort()`s the *process*; a Rust
  panic that unwinds across a V8 callback into C++ frames (no `catch_unwind`) is
  UB. In-process V8 trades a process boundary for **config discipline** — get it
  wrong and a cell *can* crash the daemon. This is the residual the trusted-agent
  model accepts.
- **A low-probability V8 embedding/internal fault has no process boundary to catch
  it** — it would take the daemon (corpus + all sessions). Accepted as
  low-prob-with-a-mature-crate; revisit if it ever bites.
- **Allowlists (execpolicy / RX-V2) are speed bumps, not walls.** `cargo check`
  runs `build.rs` = arbitrary code; indirection defeats any name-based "what may
  run." Treat them as accident/predictability guards, not containment.
- **Scope-bounding cannot stop within-scope destruction.** An agent allowed to
  write its worktree can destroy *its own* worktree (bash or python — same thing);
  only blast radius to *peers/host* is containable, and only by the sandbox.
- **Two triggers flip the OS sandbox from optional to load-bearing:** (1) running
  genuinely **untrusted / third-party** agents, (2) **unattended autonomous** runs
  where no human is watching the blast radius. Until one is real,
  [`leaf-sandbox-isolation.md`](./leaf-sandbox-isolation.md) stays on the shelf.

Consolidate where it pays (atoms, corpus, control plane); isolate where it
actually protects you — which, for a trusted single-user agent, is mostly just
supervising shell children (timeout + cap), not building a sandbox.

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

> **Consolidation changes the capability *seam*, not the authoring *model*.**
> Daemon-backed code-graph/refactor/atom capabilities arrive as injected in-memory
> `bro-capabilities` traits instead of MCP tools — but the agent still sees the
> same high-level authoring concepts inside the sandbox. This doc owns
> **topology**; [narf-draft2](../../research/harness/narf-draft2.md) owns the
> **authoring substrate** (`Ref`/`Promise`/`Plan`/`Tx`/`Atom`/`Script`, the JS/TS
> bindings, bounded egress, the mode split); [narf.md](../../research/harness/narf.md)
> is the exploratory **breadcrumb** record. Treat this section as the *placement
> constraint*, not the full NARF design.

NARF (see [`../../research/harness/narf-draft2.md`](../../research/harness/narf-draft2.md))
splits at the durable/ephemeral line, like everything else:

- **Daemon-side (singleton, survives resume):** the ref store, durable
  promises/operation handles, the atom-invocation tree, traces. Coordination
  state the daemon already owns for bros; atom/corpus lookups from a cell are
  **in-memory `bro-capabilities` trait calls** (§2), not MCP.
- **Ephemeral (per cell):** the V8 runtime executes one composition cell
  **in-process** (isolate-contained, §5), materializing refs by handle and applying
  plans to the local worktree; cell state dies with the isolate, and an aborted
  cell rolls back its `Tx`.

The concrete ref taxonomy (`ref:slice/*`, `ref:plan/*`, `ref:diag/*`,
`ref:atom/*`, `ref:trace/*`) stays in the NARF design; this doc constrains only
**durability**: refs needed for resume/coordination are daemon-side; refs that
live and die within one cell execution may stay worker-local.

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

- **One process or two — DECIDED: monolith, in-process V8, supervised shell.**
  `blackbox` owns execution in-process; V8 runs in-process (isolate-contained) and
  shell ops run as supervised child processes (§5). The corpus/execution split (a per-host execution
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
  from narf-draft2 §8 (transaction unwind vs compensate-forward). Constraint: the
  `bro-capabilities` seam must **not bake in one accidentally** — keep the trait
  signatures neutral so either semantics stays buildable.
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
- **Siblings with** [`narf-capability-library.md`](./narf-capability-library.md)
  — this doc owns topology and durability placement; the sibling owns
  session-local helpers, decay-managed reusable functions, capability scout, and
  prepare-before-run script refs.
- **Extends** [`../surfaces/mcp/mcp-surfaces.md`](../surfaces/mcp/mcp-surfaces.md)
  with the in-process binding as a third consumer of `evaluate_tool_surface`.
- **Spins out** [`leaf-sandbox-isolation.md`](./leaf-sandbox-isolation.md) —
  OS-level *scope* sandboxing of shell child processes; a threat-model-change
  escape hatch (untrusted / unattended agents), **not** v1. V8 runs in-process; the
  v1 shell isolation is supervision (timeout/cap), not a sandbox.
- **Hub:** [`bro-harness.md`](./bro-harness.md); control-plane detail in
  [`../fleet-tui/fleet-tui-cockpit.md`](../fleet-tui/fleet-tui-cockpit.md) and
  [`bro-harness-api-robustness.md`](./bro-harness-api-robustness.md).

## 14. Feedback from NARF reread

After rereading the NARF v1 braindump and draft 2 against this boundary note,
the documents look broadly aligned. The most important clarification is
ownership:

- **This doc owns topology.** It decides the post-CLI harness/daemon shape:
  contract-bottom crates, in-process daemon sessions, in-memory
  `bro-capabilities` calls, singleton fleet ownership, and V8/shell worker
  isolation.
- **narf-draft2 owns the authoring substrate.** It keeps the vocabulary and model
  projection details: `Ref<T>`, `Promise<T>`, `Plan<E>`, `Tx`,
  `Atom<I,O>`, `Script`; generated JS/TS bindings; explicit sandbox globals;
  bounded egress; and the config-selected split between NARF composition mode and
  conventional flat-tool mode.
- **narf.md owns the breadcrumb map.** It preserves the session examples and the
  discovery path through Codex code-mode, bro-harness clipboard/promises,
  bbox_refactor, bbox_slice, and atoms. This boundary doc should not duplicate
  that material, but it should keep pointing through draft 2 so the breadcrumbs
  are not lost.

The NARF nuance is therefore not dropped, but it is easy to understate in a
boundary document. The key phrase to preserve is: **consolidation changes the
capability seam, not the authoring model**. In consolidated mode, daemon-backed
code graph/refactor/atom capabilities arrive as injected in-memory traits rather
than MCP tools; the agent still sees the same high-level authoring concepts
inside the sandbox.

Given the settled premise that CLI-shaped providers are dead-dead, future edits
should avoid treating provider subprocesses as a live escape hatch. Subprocesses
remain important only as **execution leaves**: V8 cells, shell operations, and
other crash/OOM/FFI-risk work that should not live in the daemon core. If a
future adapter is ever external, it should justify itself as an isolated leaf
behind the session/capability API, not as a return to stdin/stdout provider
driving.

The remaining design pressure points after the reread:

- **Worker leaf contract.** Section 5 says V8/shell run in separate worker
  processes; draft 2 says `Tx` lifetime is cell-bounded and refs are host-side
  handles. The bridge needs an explicit contract: how a worker materializes refs,
  reports produced refs, returns bounded egress, aborts/rolls back uncommitted
  transactions on crash, and records trace/journal entries for replay.
- **Tx vs saga stays the real NARF fork.** The topology is decided enough to
  proceed, but nested atom edit effects still need a semantic choice:
  shared-worktree transaction unwind vs compensate-forward saga. The
  `bro-capabilities` seam should not bake in one accidentally.
- **Flat tools and NARF mode must share policy.** The config-selected model
  projection in narf-draft2 is correct: lower-tier models keep flat tools,
  composition-capable models get `narf_exec`. Both projections must be filtered
  by the same surface evaluator and call-time visibility checks described in
  section 6.
- **Ref taxonomy belongs in NARF, but storage placement belongs here.** The
  concrete namespaces (`ref:slice/*`, `ref:plan/*`, `ref:diag/*`,
  `ref:atom/*`, `ref:trace/*`) should stay in the NARF design. This boundary
  doc should only constrain their durability: daemon-side for resume/coordination,
  worker-local only for ephemeral cell execution.
- **Crosslink hygiene.** The current chain is acceptable: boundary -> draft2 ->
  v1. If this doc gains more NARF detail later, add a direct note that v1 is the
  exploratory breadcrumb record and draft2 is the canon authoring-layer pass, so
  future readers do not treat section 9 as the full NARF design.

**Resolution (folded into the body).** The seam-not-model phrasing + the
ownership/crosslink note are now §9's headline; the V8/shell isolation is
simplified in §5 (V8 in-process, shell supervised — the worker-leaf abstraction
dropped on review); the "subprocess ≠ provider-driving" tightening is in §3;
the tx-vs-saga seam-neutrality constraint is in §12; the ref durability constraint
is in §9. The flat-tools/NARF-mode shared-policy point was already in §6/§9.

Net feedback: the boundary move is the right next step. It removes the accidental
MCP/provider-CLI transport tax while preserving the NARF core: typed refs instead
of pasted blobs, promises instead of polling chatter, plans/transactions instead
of hopeful edits, atoms as polymorphic supervised leaves, and a sandboxed program
as the agent's serious-work interface.

## 15. Implementation status (beta/blackbox-v2)

This section is the running ledger of what the design has actually become in
code on `beta/blackbox-v2`. It supersedes the aspirational tense elsewhere in
the doc for the items listed; the rest remains target-state. Verify against code
before relying on any line here.

### Live and tested

- **Contract bottom (§2).** `bro-core` / `bro-protocol` / `bro-capabilities`
  exist as the dependency-inverted bottom; the compile DAG is acyclic and
  `bro-harness` cannot depend on `blackbox`. Earlier the three crates were inert
  (no impls, no callers); they are now load-bearing.
- **Capabilities (§2/§6/§9).** `CorpusCapability`, `AtomCapability`, and
  `RefactorCapability` are each wired end-to-end: the daemon implements them over
  its in-memory stores and installs them into the harness at startup; the harness
  exposes them as in-process `Tool`s (direct trait dispatch, no MCP round-trip).
  Standalone harness leaves the slots empty → fail-closed by absence.
  `RefactorCapability` follows the §9 ref-handle model (plan stays host-side,
  only a handle + preview cross). Commits: "Wire {Corpus,Atom,Refactor}Capability
  through the contract bottom".
- **Control plane / `bro-protocol` (§8/§11).** `bro_protocol::SessionCommand` is
  the live control-plane contract: `apply_session_command` translates it to the
  harness's internal `SessionInput`, and steer/interrupt route through it. Every
  variant maps to a genuinely-handled path (UserTurn/Interrupt/SetModel/Compact).
- **Status plane (§7, partial).** `bro_protocol::TaskSnapshot`/`TaskStatus` now
  have a real producer (daemon `task_status_json` emits a typed `snapshot` field
  via `protocol_task_snapshot`) and consumer (the fleet poller deserializes it).
  Additive, so other `/control/status` readers are unaffected.
- **§5 prerequisite.** `panic = "unwind"` is pinned in `profile.release` with a
  comment; the non-poisoning-lock half was already satisfied by `parking_lot`.
  (In-process V8 and supervised-shell isolation themselves are NOT built — no V8
  yet; that part of §5 remains target-state.)
- **§6 surface governance.** The dispatch path is now a third consumer of
  `evaluate_tool_surface`: brofiles carry a `surface` selector, and
  `surface::dispatch_surface_filters` folds the verdict into the dispatch filter
  plane (disallow-wins) for atom + exec + resume + broadcast dispatch. This
  reverses the former "surface is MCP-endpoint-only" orthogonality for the
  in-process case (the old note in `progress.rs` is updated). No surface packet
  installed → passthrough → no-op.
- **§3 identity env.** Per-session identity (auth token, base URL, account home,
  transport kind, model) flows via a tokio task-local (`transport::with_session_env`
  / `session_var`), NOT process-global env. Transports resolve identity through
  `session_var` (task-local → env fallback for the standalone binary). Fixes a
  real credential-leak-into-bash-children bug. Live-validated against the real GLM
  endpoint (gated test `live_glm_turn_resolves_creds_from_task_local`).
- **§3 concurrency.** `harness_context_lock` (held across the whole session,
  serializing in-process sessions to one at a time) is **dissolved**: cwd via a
  `--cwd` flag → `ToolCx.root`, PATH augmented once at startup, display vars set
  per shell child, and daemon service env scrubbed from children via
  `bro_tools::shell::with_spawn_scrub`. Concurrent in-process sessions no longer
  collide.
- **/irc → /control (governance cleanup).** The generic `bro_*` control plane was
  re-homed from the IRC-named routes to a neutral `/control/*` namespace that the
  fleet client and the bro-irc sidecar both consume; `/irc/*` is retained as a
  back-compat alias. (Not a numbered section here, but it removed the "fleet rides
  the IRC interface" smell and is the transport the fleet TUI uses.)

The whole stack was exercised live through the real fleet TUI (tmux): a dispatch
flows TUI → `/control/exec` → in-process harness (task-local identity, `--cwd`,
spawn-scrub, no lock) → a real brodex/GLM agent → typed `TaskSnapshot` → fleet
roster render.

### Remaining

- **§7 thin-client crate (stage 2).** A multi-stage, ~50-file refactor; the
  sub-steps land as independent compiling commits.
  - **Step 1a — DONE: `Provider` relocated to the contract bottom.** The enum +
    `Capability` + the model/effort catalog (`ModelInfo`/`EffortInfo` + tables)
    and the *pure* methods (`ALL`/`is_dispatchable`/`capabilities`/`as_str`/
    `supports_resume`/`is_streaming_json`/`models`/`efforts`/`Display`) now live
    in `bro-core` (`crates/bro-core/src/provider.rs`); `bro-core` gained a
    `strum` dep for `EnumString`/`FromStr`. The *daemon-logic* methods convert
    from inherent `impl Provider` to four traits impl'd in `blackbox` —
    `ProviderExec` (bin/build_exec_args/build_resume_args), `ProviderEvents`
    (parse_event/detect_disruption/…), `ProviderMcp` (build_filter_args/
    build_fleet_mcp_args/build_mcp_*), `ProviderSession` (resolve_session_cwd) —
    reached through one `providers::dispatch_prelude` glob added at ~13 call
    sites. `orchestration::providers` does `pub use bro_core::{Capability,
    Provider}` so the ~45 type-users didn't churn. Validated: `bro-core` checks
    standalone, blackbox lib + bro-cli compile 0/0, 27 provider tests pass, both
    bins link, and a live isolated-daemon fleet-TUI smoke (port 7299) renders the
    provider/model/effort selector from the relocated type. The fleet client
    calling daemon-logic methods on `Provider` is what 1b/1c remove; until then
    `blackbox` is the only crate that needs the dispatch traits, which is correct
    (none are called by `bro-cli`).
  - **Step 1b — DONE: the fleet client is daemon-only.** `FleetOrchestrator`'s
    in-process dispatch branch is removed: `daemon` is now a required
    `DaemonFleetClient` (no `Option`), and `dispatch`/`resume`/`stop` only POST
    `/control/{exec,resume,cancel}`. `bro fleet` with no `--daemon-url` defaults
    to the local daemon (`default_daemon_url()` → `BBOX_PORT`/7264). This drops
    fleet.rs's coupling to `brofile::resolve_provider_env`, `cancel_task`,
    `spawn_task`/`spawn_task_interactive`, and `ProviderExec`/`ProviderMcp`
    (`build_exec_args`/`build_fleet_mcp_args`); `ProviderEvents::parse_event` and
    `SupervisionState` stay (the daemon status poller uses both). `AgentHandle`
    lost its in-process `stdin` field — control is daemon-only (steer/interrupt
    over `/control/*`); `set_model` now uniformly unsupported (awaits §8). The
    in-process helpers + their tests were deleted (−~460 lines net in fleet.rs).
    Validated: blackbox lib + lib-tests + bro-cli compile 0/0, 0 new clippy,
    fleet (21) + provider (27) tests pass, both bins link, and a live
    isolated-daemon smoke (port 7299) confirmed `bro fleet` **with no
    `--daemon-url`** resolves the default daemon URL and dispatches end-to-end
    (TUI → `/control/exec` → brodex agent → ✓ Finished). Two items are now
    orphaned-but-kept (`#[allow(dead_code)]`, removal deferred to a later step):
    `mod.rs::spawn_task_interactive` (+ `SpawnedTask.stdin`) and the fleet
    pin-tools / `ProviderMcp::build_fleet_mcp_args` translation — both candidates
    to wire into daemon-side `/control/exec` (forwarding fleet.json MCP/pin tools)
    or delete when the daemon dispatch is consolidated.
  - **Step 1c — DONE: pure view/wire DTOs relocated to `bro-protocol`; client
    `TaskStatus` unified.** The genuinely-pure command-plane + transcript DTOs
    now live at the contract bottom: `DispatchSpec`/`ResumeSpec`
    (`crates/bro-protocol/src/dispatch.rs`) and `TranscriptItem`/`TodoState`/
    `TodoItem`/`TodoItemStatus` (`crates/bro-protocol/src/transcript.rs`).
    `blackbox::fleet` re-exports all six (`pub use bro_protocol::{…}`) so the
    `bro-cli` consumers don't churn. **TaskStatus unification:**
    `blackbox::fleet::TaskStatus` now re-exports `bro_protocol::TaskStatus` (the
    5-variant wire enum with `Pending`), so the client settles on one enum; the
    fleet engine's task mirror still speaks the daemon-internal
    `orchestration::TaskStatus` (4 variants, aliased `OrchStatus` inside
    `fleet.rs`) and maps to the wire enum at the `snapshot()` boundary
    (`orch_status_to_wire`). When the mirror extracts into the client crate (1d)
    its status field becomes `bro_protocol::TaskStatus` directly and the map
    disappears. `bro-cli`'s only consequent edit: a `Pending` arm folded into the
    live Running buckets in `fleet_state_from_snapshot`.
    **Deliberately NOT moved to `bro-protocol`:** `ClassifierConfig`/`FleetConfig`
    (fleet.json config the daemon never reads — client-local, not a daemon↔client
    wire contract; they travel into the client crate in 1d), the transcript
    *parser* (`parse_transcript` + helpers — logic, stays daemon-side for 1c,
    moves with the engine in 1d), and `AgentHandle`/`TaskSnapshot`/
    `FleetOrchestrator` (live handles + the engine itself — 1d). Validated:
    `bro-protocol` checks standalone; `blackbox`+`bro-cli` `cargo check` clean;
    26 fleet + 27 provider lib tests pass; 0 new clippy.
  - NOT done: **1d** extract the fleet client engine (`FleetOrchestrator`/
    `AgentHandle`/`DaemonFleetClient` + the client `Task`/`TaskStore` mirror +
    `parse_transcript` + `ClassifierConfig`/`FleetConfig` IO + the needed
    transcript parsers) out of `blackbox` into a client crate; point `bro-cli` at
    the contract bottom + that crate and DROP the `blackbox = { path = ... }`
    dependency. (The remaining `bro-cli → blackbox` surface is
    `blackbox::fleet::*` (the now daemon-only `FleetOrchestrator` + engine +
    `intern_rider`/`provider_supports_bidi`), `blackbox::parser::*`, and
    `blackbox::config::load`.) Still a durable-bro + reviewer effort, not a single
    sprint.
- **§5 in-process V8 + supervised shell (execution isolation).** Not started;
  no V8/NARF execution exists yet. The shell side has the `with_spawn_scrub` +
  per-child env hook but not timeout/cap supervision.
- **NARF substrate (§9).** Authoring layer (`Ref`/`Promise`/`Plan`/`Tx`/`Atom`/
  `Script`, JS/TS bindings) is unbuilt; the capability seam it would ride is in
  place. (A peer `Design NARF capability library` commit on this branch is design,
  not implementation.)
- **§4 deletion ledger.** The CLI-hole providers / tmux / opencode scrub landed
  earlier on the branch; verify no live CLI-shaped dispatch path remains before
  treating §4 as fully closed.
