---
title: "The remote-worker boundary: what stays in the daemon when the harness leaves the box"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - bro-harness
  - orchestration
  - daemon-runtime
brief: "What irreducibly remains in blackboxd when a harness worker runs in its own container or on another machine — and what dissolves. The forcing scenario: 'current files on disk' stops being one thing; daemon and worker hold different filesystem truths. Two findings. (1) Placement follows truth domains, and semantic_status is already the placement function: lsp_verified/syntax_only are claims about the working set (computed where the files are → worker), indexed_hints is a claim about the corpus (computed where the index is → daemon). (2) Isolation does not relocate granular governance — it dissolves it. Per-call guards (RX-V2 allowlists, hash guards, cwd cross-worktree checks, runtime apply gating) were compensating for a shared host working tree; with a private working set they demote from governance to quality mechanics, and operator authority concentrates at exactly two boundaries the daemon owns for free: dispatch (capability composition — absence beats filtering) and integration (what artifacts re-enter canonical state). Residency test for everything else: daemon-side iff shared mutable state across workers/sessions, or the coordination point itself. Companion of refactor-tools-v2.md; extends harness-daemon-boundary.md to the distributed rung."
---

# The remote-worker boundary: what stays in the daemon when the harness leaves the box

> **Status: proposed.** The in-process consolidation
> ([`harness-daemon-boundary.md`](./harness-daemon-boundary.md)) is current
> reality and a deliberate forcing function for defects and friction. This doc
> designs the *return trip*: the harness as an isolated worker — its own
> container, eventually another machine. Nothing here is v1 work; the point is
> to know which of today's components are load-bearing architecture and which
> are scaffolding coupled to the shared-filesystem era, so we stop reinforcing
> the scaffolding.

## 0. Thesis

When a worker gets its own filesystem, "current files on disk" bifurcates: the
worker holds **working-set truth** (the mutable checkout it is editing) and the
daemon holds **corpus truth** (the indexed, historical, cross-project view).
Every tool placement question resolves by asking which truth the tool consumes —
and the answer key already shipped: `semantic_status`. `lsp_verified` and
`syntax_only` are claims about working-set bytes and can only be computed where
those bytes live; `indexed_hints` is a claim about a corpus snapshot and stays
with the index. Today's daemon conflates the lanes only because both happen to
read the same disk.

The second finding is the sharper one: **isolation dissolves granular
governance rather than relocating it.** The per-call enforcement that grew in
the refactor/apply machinery was protecting a *shared* host working tree from
concurrent agents. A worker on a private clone has nobody to protect its
scratch space from. What survives as governance is exactly two boundaries —
**dispatch** (what capabilities go into the box) and **integration** (what
artifacts come back out) — and the daemon owns both by construction. Everything
between them demotes to quality mechanics the worker runs for its own benefit.

What must remain daemon-side then passes a single test: **shared mutable state
across workers/sessions, or the coordination point itself.** The list is short:
the corpus, the shared stores, the orchestration singleton, ingress, credential
brokering, and the integration boundary. Notably absent: LSP, code-nav, refactor
machinery, validation, apply, and runtime surface enforcement.

## 1. The forcing scenario

Today every working-set tool in the daemon — the `bbox_code_*` family,
`bbox_refactor_*`, `bbox_slice_*` — takes `project_dir` + relative `file` and
reads the daemon's local disk per call (`resolve_path`,
`crates/bbox-refactor/src/lib.rs:4510`). The warm LSP pool
(`crates/bbox-lsp/src/session_manager.rs`, sessions keyed
`(canonical_project_root, Language)`) spawns rust-analyzer/jdtls/roslyn as
daemon children against the same disk. This is all correct while there is one
disk.

Put the worker in a container with a private clone and every one of those calls
answers questions about the *wrong filesystem*: the daemon's checkout, not the
working set the agent is mutating. `didOpen` shipping full file content over
LSP does not rescue the semantic lane — rust-analyzer's workspace indexing
reads `Cargo.toml`, the crate graph, and `target/` from disk; the server must
sit next to the files. So the question is not "how do daemon tools reach into
the container" but "which tools should never have implied daemon disk in the
first place."

## 2. Two truth domains

| | Working-set truth | Corpus truth |
|---|---|---|
| **What** | the agent's mutable checkout | indexed/historical/cross-project view |
| **Owner** | the worker (harness) | the daemon, permanently |
| **Tools** | tree-sitter lanes (`bbox_code_query/refs/outline`), slices, refactor plan+apply, LSP servers and their session pool, validation runs | tantivy symbols, `bbox_hybrid_search`, graph walks, knowledge, blame/provenance, transcripts |
| **`semantic_status`** | `lsp_verified`, `syntax_only` | `indexed_hints` |
| **Crossing mechanism** | does not cross; executes in the worker | MCP out-box, as today (`CorpusCapability` seam) |

Three consequences:

1. **The crates already cleave this way.** `bbox-code-nav`, `bbox-lsp`,
   `bbox-refactor` are daemon-independent workspace crates; `bro-lsp` is the
   harness-side precedent (Rust-only, async, explicitly daemon-free,
   `crates/bro-lsp/src/lib.rs:1-5`). The compile DAG already permits
   `bro-harness → bbox-*` (siblings, not the daemon; the forbidden edge is only
   `bro-harness → blackbox`). Moving the working-set lanes is re-binding, not
   rewriting. The richer shape of those bindings — a code-mode DSL rather than
   flat tool ports — is [`refactor-tools-v2.md`](./refactor-tools-v2.md).
2. **The index does not move; it gets a generation stamp.** Today
   index-vs-disk divergence is bounded by a reindex interval over shared disk.
   With isolated workers it is structural: the index describes the canonical
   checkout, the worker lives on a fork. Indexed-lane responses should carry
   the commit/generation they were built from so `indexed_hints` says *which*
   snapshot it hints about. Corollary, stated as a decision: **the corpus
   indexes published truth; working-set changes feed it only at
   publish/merge.** Scratch divergence being invisible to the corpus is
   correct, not a gap.
3. **LSP sessions follow the workspace, and the session key gains a workspace
   identity.** The pool pattern survives per filesystem domain; the key must
   stop being canonical path alone, because two workers mounting different
   clones at the same `/work` path would alias in a path-keyed pool. RX-V3
   fail-closed carries verbatim: an image missing rust-analyzer yields
   `error.lsp_unavailable`, never a silent downgrade to syntax.

## 3. Governance dissolves under isolation

Audit each granular mechanism by what it was actually compensating for:

| Mechanism | Was compensating for | Under isolation |
|---|---|---|
| RX-V2 cargo command allowlist (atom-dispatched `bbox_refactor_run`) | no composition runtime; prompt-discipline as containment | pointless — a worker with `shell.run` at shell trust does anything within its mount regardless (cargo runs `build.rs`; [`leaf-sandbox-isolation.md`](./leaf-sandbox-isolation.md): name allowlists are speed bumps) |
| `original_sha256` hash guards, atomic writes | concurrent agents mutating one shared host tree under you | quality mechanics — drift detection within the worker's own pipeline, no longer protecting anyone else |
| `cwd` cross-worktree apply check (`src/slices.rs:121-126`) | plan generated in one worktree, applied in another, same disk | subsumed by workspace identity on the artifact envelope |
| RX-V1 operator flags as *runtime* checks | the agent mutating canonical state in place | the flags survive as **dispatch-time inputs** in the brief; `operator_opt_outs_used` survives as **artifact audit metadata**; the runtime refusal point moves to integration |
| Per-call registry membership checks (`ensure_path_in_registered_project`) | path containment by convention | the mount *is* the path check — containment becomes mechanical |
| Runtime MCP surface evaluation inside the worker | one process serving many identities | composition at dispatch: the verdict is resolved before launch and enforced by what is simply present or absent |

The pattern: every per-call guard existed because the blast radius was shared.
Make the blast radius private and the guards demote from *governance*
(authority over shared state) to *quality mechanics* (the worker making its own
output better). They are worth keeping in that role — Blocked findings, FIXME
gates, validate-and-rollback all improve artifact quality — but nothing about
them requires daemon residency, daemon enforcement, or per-call granularity.

**Caveat that bounds the claim (§6):** this dissolution is load-bearing on the
working set being genuinely private. On the bind-mount rung of the isolation
gradient the old shared-blast-radius problem persists and the guards cannot be
deleted. The guards are coupled to that rung — write that down so nobody
defends the hash-guard layer as "governance" years after volume isolation made
it vestigial.

## 4. The two surviving boundaries

### 4.1 Dispatch — what goes in the box

Capability composition: which tool binaries and bindings exist in the image and
registry, which MCP surfaces are reachable, which credentials are injected,
which mounts and network the container gets, which operator-authority flags the
brief carries. This is "has tools available," upgraded by isolation: **absence
beats filtering.** No runtime surface evaluator runs inside the worker; the
surface verdict (`evaluate_tool_surface`) is resolved at dispatch and enforced
by construction. Policy *authoring* stays central — brofiles, surface packets,
the artifact catalog — but enforcement ships with the dispatch.

### 4.2 Integration — what comes back out

The authority that used to sit at apply-time belongs here once apply targets
scratch space. The worker may do anything to its working set; the operator
moment is whether the resulting artifact — a branch, an EditSet, a publish, a
durable knowledge write — is accepted into canonical state. Concretely:
merge/publish gates, provenance stamping and export, reindex-on-publish, review
lanes for shared-store writes. One gate at the boundary instead of N gates in
the pipeline — which is also the only gate shape that has survived contact with
reality (cf. the adjudication-boundary retirement recorded in
[`refactor-tools-v2.md`](./refactor-tools-v2.md) §2).

The one-line model: **policy decided centrally, enforced by construction,
audited at re-entry.**

## 5. The residency test and the irreducible list

> Daemon-side iff: **shared mutable state across workers/sessions, or the
> coordination point itself.**

| Stays in the daemon | Why it passes |
|---|---|
| **The corpus** — tantivy, graph, embeddings, transcripts, blame/provenance | aggregates across projects, sessions, machines; workers query out-box |
| **Shared stores** — knowledge, decisions, threads, notes, inbox, pins, roadmap, whiteboards | multi-writer state; daemon owns consistency and review lanes |
| **The orchestration singleton** — dispatch, teams, workflows, crons, cross-worker promise coordination, the seq-ordered steer/interrupt plane | singleton by definition; a worker cannot own the thing that owns workers |
| **Ingress** — webhooks, pollers, system events | needs a stable address; ephemeral workers have none |
| **Credential brokering** | keys are minted/injected per-dispatch, never baked into images (capability = tool + credential + scope) |
| **The integration boundary** (§4.2) | the re-entry point into every shared store above |

And the explicit not-list, because it is the answer to the question this doc
exists for: **LSP, code-nav, refactor machinery, validation, apply, slices/file
tools, runtime surface enforcement.** None of it. The daemon-side flat
`bbox_code_*` read adapters survive as the operator-attended projection — the
daemon's host checkout is itself a legitimate working set for interactive
sessions. The refactor surface does not even keep that projection: per
[`refactor-tools-v2.md`](./refactor-tools-v2.md) §7 (decided), refactor tooling
becomes **in-harness only**; external/MCP-only agents direct refactoring via
ad-hoc `bro_exec`/`bro_resume` orchestration or consume a canned atom.

## 6. The isolation gradient is the migration path

Three rungs, independently useful; design for rung 3, ship rung 2 first:

1. **In-process (today).** Shared disk, shared process. The deliberate
   defect-surfacing mode. All guards load-bearing.
2. **Container + bind-mount of the host worktree.** Process/network isolation,
   *shared* disk. Daemon-side tools keep working; near-zero code-nav work; this
   is [`leaf-sandbox-isolation.md`](./leaf-sandbox-isolation.md)'s
   accident-containment. **The in-flight guards must stay** — blast radius is
   still shared. If the mount path differs from the host path, the existing
   `project_dir` + relative-`file` contract already isolates the change to root
   resolution.
3. **Container/machine + private clone.** True divergence. Everything in this
   doc applies: workspace identity in session keys and artifact envelopes,
   generation-stamped index hints, dispatch-composed capabilities, integration
   gating — and the rung-2 guards become deletable.

Getting the contract work in early (workspace identity in `bro-core`,
generation stamps on indexed responses, EditSet/artifact envelopes) means rung 3
is operational work, not architecture work.

## 7. What this deletes from the in-process design

- **A `WorkspaceCapability` inverse seam** (daemon calling file operations into
  the worker's filesystem) — considered and rejected. If the worker-side
  code-mode cell is the execution locus for all working-set analysis and
  mutation, the daemon never reaches into the workspace: it orchestrates by
  dispatching briefs and receiving artifacts (EditSets, validation results,
  `bro_report`s). No second contract direction, no file-RPC plane.
- The `cwd` cross-worktree heuristic — subsumed by workspace identity.
- Per-call registry containment checks on working-set ops — the mount bounds
  the workspace.
- The temptation to proxy worker file reads over MCP — the worker owns its
  bytes.

## 8. Open questions

- **Control-plane transport off-host.** `bro-protocol` over what, when the
  worker is on another machine? The schema is settled (serde types, seq-ordered
  commands); the byte transport was deliberately left thin
  ([`harness-daemon-boundary.md`](./harness-daemon-boundary.md) §12). Remote
  workers force the pick.
- **Artifact return shape.** Branch push vs EditSet envelope vs both — what is
  the canonical unit the integration boundary accepts? Interacts with
  [`refactor-tools-v2.md`](./refactor-tools-v2.md) §3's EditSet artifact.
- **Image provisioning.** rust-analyzer/jdtls/toolchains in worker images.
  Fail-closed protects correctness; availability is real operational work.
- **Warm-session economics.** Per-worker LSP pools lose cross-agent sharing;
  cold rust-analyzer is multi-second. Pre-warmed workspace containers vs
  session persistence across dispatches.
- **Corpus read-path latency.** Out-box MCP from a remote worker adds a network
  hop to every `bbox_hybrid_search`/knowledge call. Likely fine (these are
  already interpretive, model-paced calls); measure before optimizing.

## 9. Relationship

- **Extends** [`harness-daemon-boundary.md`](./harness-daemon-boundary.md) to
  the distributed rung: the in-process consolidation is current reality and the
  forcing function; this doc is the shape consolidation must not paint over.
  The §12.1 corpus/execution split escape hatch is this doc's rung 2-3 seen
  from the daemon side.
- **Sibling of** [`refactor-tools-v2.md`](./refactor-tools-v2.md) — that doc
  owns the worker-side DSL that makes §7's deletion (no daemon reach-in) hold;
  this doc owns topology and residency.
- **Next rung beyond** [`leaf-sandbox-isolation.md`](./leaf-sandbox-isolation.md)
  — that doc bounds what a shell child can *touch* on a shared box; this doc
  gives the worker a private box and asks what is left to govern.
- **Carries forward** the repo invariants RX-V1/RX-V3 in relocated form (§3,
  §2.3); RX-V2 is coupled to the shared-filesystem rungs and retires with them.
- **Touches** `design/daemon-runtime/` (plane isolation; the daemon sheds
  worker execution load) and the fleet worktree conventions (worktree
  generalizes to workspace identity).
