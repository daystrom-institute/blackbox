---
title: "Model-facing tools and agent loop: comprehensive contract audit"
kind: design
lifecycle: archived
corpus: blackbox-design
topic: [bro-harness, tools, context-management, audit]
brief: "Complete local tool-interface inventory, execution-path comparison with Codex, synthetic contract probes, and ten live fixture trials. Establishes repair priorities and a smaller default surface; does not claim the findings are repaired."
---

# Model-facing tools and agent loop audit

The default surface is too broad, and the larger problem is inconsistent
contracts underneath it. Composition, wrappers, prompt templates and automatic
diagnostics do not share one trustworthy invocation lifecycle. Some additions
only duplicate ordinary tools; others change authority, output or completion
semantics. The first [read-recovery audit](model-facing-tools-audit.md) fixed
real defects but did not establish the quality of this broader system.

Keep exact reads, explicit searches, small edits/patches, supervised processes,
and discovery as the ordinary workflow. Retain composition and semantic refactor
capabilities as deliberately selected facilities. Remove execution wrappers,
result rewriting, and unconditional domain manuals from that ordinary workflow.
Fix the authority and lifecycle defects before treating a reduced catalog as
the solution by itself.

Implementation sequencing and acceptance criteria now live in the
[prioritized repair plan](model-facing-tools-repair-plan.md).

## Snapshot, evidence, and deliverables

Audited 2026-09-10 at Blackbox
`c384fb8a1cc022f7a8bc00f94ad0462373ca1a8e`. The installed macOS harness and isolate
were rebuilt from that commit immediately before this pass, stable-signed and
smoke-tested. Source comparisons use the fetched local Codex checkout at
`242c5ce01cd3388f7d23a87b68615b2042a04bfc`. Live reference trials use the separately
installed `codex-cli 0.154.0`; that executable is not asserted to be a build of
the pinned source commit.

- [Complete tool inventory](model-facing-audit/tool-inventory.json): all 104
  installed local schemas, parameter names, required fields, and placement.
- [Loop and context audit](model-facing-audit/loop.md): all session paths,
  provider response handling, steering, cancellation, compaction and resume;
  source anchors, counterexamples and test gaps.
- [Composition audit](model-facing-audit/composition.md): registry, discovery,
  MCP, domain bindings, defaults, model-visible declarations and transports.
- [Builtin audit](model-facing-audit/builtins.md): edit/patch, process lifecycle,
  Git, policy and evidence-preservation contracts.
- [Wire receipts](model-facing-audit/wire-probe-receipts.json),
  [invocation-policy receipts](model-facing-audit/composition-probe-receipts.json),
  [builtin receipts](model-facing-audit/builtin-probe-receipts.json),
  [reader receipts](model-facing-audit/reader-probe-receipts.json), and
  [live-trial measurements](model-facing-audit/live-trial-receipts.json).

Evidence is labeled by what was actually done: source-confirmed, reproduced in
an installed executable with synthetic inputs, or observed in a live model
trial. Source-confirmed counterexamples are not counted as runtime tests.
Passing the earlier workspace tests is not evidence against the uncovered
cases. This pass adds audit artifacts and reproduction tools; the new findings
below remain open runtime work.

## Coverage ledger

| Surface | Scope examined | Validation and boundary |
|---|---|---|
| Ordinary tools | All 21 bro-tools builtins: read/edit/write/patch/list/search/glob; four shell tools; five Git tools; smart_read, web_fetch, todo_write, two grounding/status tools | Every schema captured; handlers and common helpers reviewed; synthetic probes for failure-prone contracts. This is not a claim that every possible argument combination was tested. |
| Domain interfaces | 83 bindings: code 8, edits 10, LSP 8, build 1, analysis 8, Java 33, Rust 15 | Complete name/schema/declaration inventory and shared dispatch/authority/reader paths. Individual Java/Rust transformation algorithms were not all semantically reverified. |
| Harness-only tools | exec, wait, tool_search, report, conditional final_result | Construction order, visibility, schemas, runtime state, outputs and cancellation reviewed. These five are conditional and are additional to isolate's 104. |
| MCP | Discovery/filtering, aliases, in/out/both placement, metadata, startup, calls, remote result conversion | Shared adapter reviewed, rather than assuming each remote server behaves like an in-process mock. Every third-party server implementation is outside the audit. |
| Provider projection | Responses HTTP/WS, Anthropic-compatible, Chat-compatible | Source matrix; full Chat mock captures and incomplete/invalid-result probes; live Responses trials. Not every provider/model/endpoint combination was exercised. |
| Loop | One-shot, persistent stdin, daemon until-idle, ordinary steps, flat/nested dispatch, stop, retry, interrupt, steer, report, final_result | Reachable paths and tests inspected. Synthetic terminal probes and real step-exhaustion observation supplement source evidence. |
| Context | Base templates, dispatch persona/directives/scope/pins, project docs and riders, environment, diagnostics, compaction, resume | Injection and clearing paths reviewed, including changes across process reconstruction. Scoped-doc validity is tracked as an unresolved contract, not an assumed prompt obligation. |
| Planning and extensions | todo persistence, controls, instructions/skills, external bro tools | todo_write is local state; there is no local update_plan or native general skill/subagent tool family in builtin_tools. External bro/MCP capabilities depend on the admitted catalog. No recommendation to copy every Codex feature merely for parity. |

## Findings in repair order

P0 means an isolation contract has a reproduced violation. P1 means authority,
mutation, evidence or completion correctness is affected. P2 covers ordinary
ergonomics and avoidable context. These are repair priorities, not assertions
that the entire host is sandboxed.

| Priority | Finding | Evidence and consequence | Tracking |
|---|---|---|---|
| P0 | Nested shell loses credential scrub context | Full-harness synthetic canary is absent through flat shell_run and present through exec calling the same tool. Tokio task-local scrub state is lost at spawned tasks. Raw Git subprocesses also bypass the shared child environment in source. | gap-74966891; composition C1; builtins 11-12 |
| P1 | Wrapped execution bypasses shell denial | With shell_* denied, build.gate still executes a synthetic command and returns its exit code. A wrapper name is a new authority path. | gap-74966891; composition C2 |
| P1 | Cancellation does not imply stopped mutation | A cancelled patch blocked on a synthetic FIFO later creates/deletes files after the fixture unblocks it. Dropping spawn_blocking's waiter releases admission before the underlying work finishes. Active cells also outlive interruption of their observer. | gap-74966891; builtins 5; loop L3 |
| P1 | Shell deadlines and retained output are misleading | Yielded timeout is enforced only when polled; initial stdin can block before deadlines start; capture retains the first 8 MiB and discards the actual terminal error. Poll filters lose matching lines split across reads. | gap-cf64b0da; builtins 1-3, 7, 10 |
| P1 | Patch failure loses partial mutation accounting | Adding a file followed by updating a missing file returns error after creating the first file. Move destination preimages are incorrectly recorded as empty. | gap-4d6e6709; builtins 4, 6 |
| P1 | Provider output is repaired into different calls | Responses/Chat malformed JSON becomes {}, then defaults may make it executable. A cleanly closed but unterminated Chat stream becomes success. | gap-84d52d4d; loop L1-2; reproduced EOF case |
| P1 | Incomplete or invalid work reports success | Mock final_result accepts answer:false where the schema requires integer. A live 12-step-cap trial ended just after editing, skipped requested verification, and emitted success with earlier progress narration. | gap-84d52d4d; loop L9-10 |
| P1 | Instructions and resume state are not authoritative | Late flat riders, missing nested riders, historical-text dedupe, explicit null restore confusion, changed docs/scope without replacement, and missing snapshots silently becoming fresh sessions. | gap-41e7593e and gap-5a1f21b4; loop L4-6, L11-12 |
| P1 | Steering/control delivery has holes | Active /compact is popped and discarded; non-tool follow-up requests can precede queued steers; compaction/diagnostic waits are not governed by interruption; some controls are acknowledged without an implementation. | gap-5a1f21b4; loop L7-8, L13 |
| P1 | MCP conversion discards useful evidence | structuredContent replaces accompanying text, including potential completeness/continuation warnings. Media/resources can become empty success. Structured/non-text error details are discarded; the MCP error flag/text survive conversion, while Responses/Chat later omit the flag. | gap-eb974f4c; composition C4 and transport matrix |
| P1/P2 | Defaults and riders alter typed interfaces | String-only defaults cannot populate numeric/boolean inputs faithfully; metadata riders can change a primitive return into an object. | gap-f9a4e7eb; composition C6 |
| P2 | Discovery and declarations drift from admission | Selective namespace filtering still emits whole declarations; lsp.assist lacks a declaration; ALL_TOOLS names are not a uniform invocation API; repeated broad tool_search cannot reach later tied matches; activation grows without a budget. | gap-f9a4e7eb; composition C5, D1-5 |

Codex is a reference for stronger contracts, not an assumed perfect baseline.
For example, its patch implementation is not globally transactional and it can
replace move destinations. Its useful difference is pre-verification and an
explicit committed/uncertain mutation delta, not an imaginary no-partial-write
guarantee. Our path resolver deliberately grants full-host paths; that is not a
new sandbox escape. Codex strips private MCP _meta from model-facing code-mode
values too; the recommendation is a sanitized envelope with metadata retained
internally, not indiscriminate metadata disclosure.

## The "smart" defaults that cost the most

### Catalog size and prompt truth

A captured gpt-5.5 Chat request with no MCP servers contained:

| Mode | Direct tools | Serialized tool definitions | Serialized system text |
|---|---:|---:|---:|
| off | 14 | 19,565 bytes | 21,935 bytes |
| optional, default | 16 | 94,954 bytes | 22,097 bytes |
| only | 3 | 92,562 bytes | 22,598 bytes |

These are serialized byte counts, not token estimates. The optional exec
description itself is 72,240 UTF-8 bytes. All seven domain namespaces are
included before the optional-mode early return; Java alone contributes 24,800
bytes of description and declarations. Only-mode largely relocates the same
catalog into exec instead of reducing it. There is no workspace-language gate.

The shared base templates also describe a different product's tools. The
fallback promises update_plan and a command-array patch call; gpt-5.5 mandates
multi_tool_use.parallel and refers to exec_command. Those names are absent from
our builtins. Patch is removed on Anthropic/Chat, yet base prose still requires
it. The non-Responses exec schema accepts {source}, while shared prose says raw
JavaScript instead of JSON. Sources are
`crates/bro-harness/prompts/base_instructions/{fallback,gpt-5.5}.md`,
`transport/mod.rs:307`, `agent_loop.rs:878`, and composition's transport matrix.

The corrective direction is a host-owned, capability-derived instruction layer
and generated per-method declarations, with optional specialized discovery.
Adding more aliases to satisfy stale copied prose would preserve the underlying
maintenance problem. Long general prompt text should not substitute for honest
tool schemas or an implemented approval/control capability.

### Search and read convenience changes the evidence

- content_search skips files above 2,000,000 bytes without reporting incomplete
  coverage. A fixture with a known match returned exactly "no matches". The
  retrieval trial's 2.31 MB file forced agents to recover via shell.
- The default walk omits named build/dependency directories. Explicitly naming
  the build directory succeeds, a useful control; the generic negative answer
  still does not disclose exclusions. No broad search should imply exhaustive
  absence after silent omissions or I/O errors.
- content_search max_results=0 returns a match and says truncated at 0. Its
  filename-only glob differs from glob's path-pattern behavior. The latter is
  documented, so it is an ergonomic inconsistency rather than a hidden parser
  bug. Its refinement hint overpromises an exhaustive search after increasing
  a count while the fixed byte cap and file exclusions still apply.
- code.read accepts a span inside a UTF-8 character and returns replacement
  text with byte_length:1 and truncated:false. The claimed exact-byte read is
  not exact. code.readLines and batch code.items also have different size
  behavior from the ordinary bounded reader.
- web_fetch strips HTML regardless of Content-Type. A local JSON fixture
  containing `List<T>` and `a < b > c` returned altered values. It also reads the
  entire body before truncation and loses source layout/links. Treat this as
  lossy extraction, or preserve content-type-specific text; do not imply it is
  an exact general fetch operation.
- Automatic Rust diagnostics run before returning edit results, and first-use
  baseline absence treats all current diagnostics as new. Expensive LSP waits
  and that attribution should be explicit. Opt-in or bounded cancellable
  diagnostics have value; a silent compulsory analyzer pass is a poor general
  edit contract. See `diagnostics/engine.rs:85` and `agent_loop.rs:2102`.

## Live task trials

Ten runs were executed with gpt-5.5 at low effort: four bro-harness runs with
normal ambient instructions, four with a small isolated fixture instruction,
and two installed-Codex reference runs. Every run used a fresh synthetic Git
workspace and no external MCP servers. No client source was used.

Tasks: retrieve an exact marker from a 2.31 MB file and verify answer.json;
and fix average([]), preserve a scoped instruction's first-line comment, add a
regression assertion, and run verification. Artifacts were checked independently
of the model's final claim. Transcripts were inspected for the requested
verification invocation.

Controlled cohort, with a 30-step bro limit:

| Runtime/profile | Task | Artifact correct | Requested verification performed | Model steps | Final request input tokens | Wall time |
|---|---|---|---|---:|---:|---:|
| bro, optional | retrieval | yes | yes | 10 | 29,291 | 30.64 s |
| bro, off | retrieval | yes | yes | 10 | 12,048 | 25.87 s |
| bro, optional | edit | yes | yes | 11 | 26,880 | 34.85 s |
| bro, off | edit | yes | yes | 13 | 9,743 | 32.79 s |
| Codex CLI 0.154.0 | retrieval | yes | yes | different event schema | not captured | 26.04 s |
| Codex CLI 0.154.0 | edit | yes | yes | different event schema | not captured | 32.34 s |

Neither controlled optional-mode run invoked exec. They incurred its catalog
overhead while using ordinary tools. Off did not consistently reduce steps:
its edit run used todo_write four times. Both profiles also recovered from
trying the unavailable python command by using python3. These observations
argue for a smaller default catalog, not for eliminating optional composition.

The ambient cohort is a separate diagnostic. Global instructions required
Blackbox graph/gap discovery although the fixture had no MCP servers, producing
wasted tool_search calls. Its 12-step off/edit trial produced correct files but
did not run verification before the cap; the harness nevertheless said success.
That is evidence about instruction/catalog mismatch and terminal honesty, not
a controlled claim that off is worse at editing.

There is one run per task/profile in each cohort, not a statistically reliable
benchmark. Runtime prompts, sandboxing and provider event formats differ for
native Codex; provider latency/cache and model randomness also differ. No
general speed, reliability, token-saving percentage or semantic-refactor-quality
claim follows from these ten runs. They establish observable catalog overhead,
working simple-task controls, and specific failure/recovery paths.

## Keep, simplify, remove

| Decision | Surface | Reason |
|---|---|---|
| Keep ordinary | bounded file_read, honest search/list/glob, file_edit/write, apply_patch, shell run/poll/kill | Direct, composable operations with user-recognizable effects. Repair supervision and evidence rather than inventing substitutes. |
| Keep optional | small exec/wait, code/LSP spans, edits algebra, semantic Java/Rust transformations and host-side reductions | Composition and semantic authority can do useful work ordinary string operations cannot. Require explicit discovery/profile and shared invocation policy. |
| Simplify | tool_search and namespace description delivery, provider/base prompts, defaults, completion and resume | One admitted catalog, typed values, explicit state transitions and truthful outcomes. |
| Remove from the ordinary surface | unconditional domain manuals, build.gate execution wrapper, persistent shell output_filter, redundant Git views, heuristic tool nudges | Added choices or convenience currently introduce overhead, evidence loss or parallel authority paths. Preserve compatibility deliberately where needed. |
| Preserve safeguards | host-only operator grants, hash/stale-span validation, fail-closed LSP authority, source continuations, explicit errors and completion handles | These add correctness rather than merely steering the model toward a preferred toolbox. |

## Repair acceptance contract

The first repair should establish one explicit invocation context carrying
admission, child environment, cancellation, completion ownership, instruction
generation and mutation accounting. Every flat/nested/wrapped path must use it.
Status/control traffic needs a separate lifetime from workspace mutation locks.
Do not add another prompt asking the model to compensate for these boundaries.

Next, require valid provider arguments and terminal events; validate structured
results and represent exhaustion/interruption distinctly. Rebuild context and
resume from typed authority records, not text-shaped historical receipts.
Then reduce default catalog size and generate descriptions from what is
actually admitted. The detailed appendices give per-finding regression cases.

Acceptance must include the installed/full-loop seams: synthetic environment
canaries, denied-wrapper execution, cancellation after blocking work starts,
partial patch deltas, passive process deadlines, split output, malformed and
truncated provider responses, mixed MCP envelopes, actual persist/open/resume,
and newly scoped instructions before mutation. Tool-body tests alone are
insufficient. A later task corpus should include broad refactors, long sessions,
provider faults and parallel tool use before claiming efficacy improvements.

## Reproducing the deterministic audit

Run against an explicitly selected installed or newly built binary:

```sh
python3 scripts/audit-harness-contracts.py --harness /path/to/bro-harness --isolate /path/to/isolate
python3 scripts/audit-harness-invocation.py --harness /path/to/bro-harness
```

These commands use temporary files and a local scripted HTTP endpoint with fake
authentication. They report observations and save receipts; exit zero means the
probe ran, not that the observed contracts are correct. They make no live model
requests. The live-trial receipt file includes task definitions, configuration,
outcome checks and measurements for the separate provider runs. No shared
service was restarted or reconfigured during this audit pass.


The [completed repair plan](model-facing-tools-repair-plan.md) and
[final implementation dispositions and live receipts](model-facing-audit/session-and-catalog-repair.md)
record the follow-up. Findings in this document describe the audited baseline.
