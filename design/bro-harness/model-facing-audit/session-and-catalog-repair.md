---
title: "Session integrity and model-facing catalog repair"
kind: design
lifecycle: archived
corpus: blackbox-design
topic: [bro-harness, tools, context-management]
brief: "Typed instruction delivery, strict resume/control persistence, and smaller truthful tool catalogs."
---

# Session and catalog contracts

This follows the [result integrity repair](result-integrity-repair.md) and
addresses orders 4 and 5 of the [repair plan](../model-facing-tools-repair-plan.md).
The original [loop](loop.md), [composition](composition.md), and
[builtin](builtins.md) findings remain historical evidence. Codex source is the
local reference at `242c5ce01cd3388f7d23a87b68615b2042a04bfc`; adopting an idiom does
not establish equivalence with every upstream behavior.

## Instructions before effects

Structured file/edit tools and binding mutators declare their affected paths.
Shared admission applies typed host defaults first, discovers covering
instructions, and rejects mutations needing instructions the authoring model
request has not received. Reads can discover new instructions without rewriting
their returned source content. Patch paths include both source and destination;
`edits.apply` checks and applies the same cloned edit set.

The session keeps canonical document identity, scope, content hash, graph origin,
and delivery occurrence. Include removal and deleted documents produce explicit
revocations. Previously visited ancestry is refreshed, so newly created AGENTS
files become visible even without another file read. Global candidates are
tracked independently of successful startup reads. Explicit system overrides
suppress startup discovery, while subsequent structured accesses enroll their
scopes. Read errors stop admission instead of manufacturing empty instructions.

Exact pending batches enter authoritative provider context and the event stream
before acknowledgment. A parsed source string, a caught JavaScript exception, or
an old tool-result rider grants nothing. Each provider request has an immutable
instruction generation; yielded cells retain their original generation across
later exec and wait calls. Compaction invalidates delivery, and resume restores
discovery state before delivering current versions afresh.

This is a structured-tool contract. Shell commands and remote tools do not offer
an exact local path manifest. Filesystem paths remain host-trusted, and this
policy is not a filesystem sandbox or cross-process compare-and-swap mechanism.

## Durable sessions and truthful control responses

Explicit resume rejects missing or corrupt snapshots, unsupported versions or
transport shapes, conflicting IDs, and meaningful event-log tails not covered
by the snapshot. Versioned byte checkpoints include unsequenced user input;
legacy sequence checkpoints remain conservatively supported. Advisory writer
locks cover the whole session lifetime, including persistence workers.

Persistence flushes and checks the event writer, synchronizes data, and writes
through the common snapshot serializer. Failed or timed-out log writes cannot
produce successful control acknowledgments. Acknowledgments follow applied and
persisted controls. Unknown or malformed controls return errors. Model changes
update the model and context thresholds at a turn boundary. Default guidance is
capability-derived and model-independent; explicit instruction overrides remain
intact. Redirects are
prioritized and queued inputs are durable. Accepted prompts remain in history
when cancellation or a zero-step budget prevents their first model request.

Both session entry modes share the same control boundary. Manual compaction is
handled as a command and never injected as task text or silently discarded.
An interrupt waits for already admitted compaction work to settle before its
acknowledgment. EOF ends input admission; accepted work finishes and remaining
processes are drained when the session exits. Explicit interrupt owns active
cancellation.

Resume discloses that JavaScript cells, store/load values, and shell handles are
process-local. The structured output schema survives resume. Scope and pin
removals explicitly clear prior context. A checkpoint with retained cells or
shell handles records `runtime_work_outstanding:true`, including completed but
unconsumed output. Resume refuses that checkpoint and requires explicit recovery.
A later quiescent checkpoint clears the marker. Cell terminal replies follow
callback draining and catalog removal, so an immediately following checkpoint
does not mistake consumed terminal output for outstanding work.

## Smaller, truthful catalogs and typed policy

The default exec description contains runtime guidance and an admitted-method
index, replacing specialized namespace manuals. `ALL_TOOLS` exposes canonical
names, actual invocation coordinates, input/output schemas, and generated
per-method declarations in both optional and only modes. Default base guidance
is derived from exposed capabilities; it does not assume Codex-only tools.

Discovery supports bounded pages, deterministic ranking, explicit inspection
without activation, and atomic count/definition-byte limits. Historical schema
promises are never evicted silently. A catalog exceeding the activation budget
returns an actionable refusal; an oversized restored catalog also fails clearly.
Identical shared tool entries are deduplicated. Distinct canonical names that
collide after qualification or JavaScript normalization are rejected before
execution.

Ordinary defaults and pins are JSON values end to end, including dispatch-facing
APIs and stored brofiles. Legacy strings remain strings; numeric-looking strings
are not guessed into numbers. Rules validate against admitted parameter schemas.
Wildcard rules skip absent properties; unknown exact properties fail. Host-only
authority grants are declared separately and never inserted into model arguments
or model-facing policy observations.

Policy observations have a bounded, separate channel with explicit omission
counts. Tool results keep their original text/JSON shape, including primitive
values and fields whose names resemble policy metadata. Hooks and diagnostics
also enter separate context rather than modifying source bytes or JSON results.
Operator status reporting does not wait behind a workspace mutation.

MCP configuration is strict, including duplicate keys and unsupported SSE
configuration. Required and optional servers have bounded startup policies.
Readiness is sanitized and observable, and admitted metadata retains titles,
annotations, and output schemas through the actual MCP result-envelope contract.
Catalogs are fixed for one session; live tools/list reconciliation is not claimed.

Remote calls have a configured absolute deadline and cooperative cancellation
notifications. Deadline, connection loss, or interruption after dispatch records
unknown remote completion and quarantines the shared server connection, including
aliases. The loop preserves that uncertainty even if JavaScript catches the
error, ends incompletely, and persists a resume refusal. Neither a cancellation
notification nor a fresh session proves remote effects stopped. Local in-process
tools continue to retain their actual completion owner.

Git readers and commits use the finite internal subprocess supervisor with raw
argv and captured child environment. They do not dispatch through the shell tool
or inherit shell-specific argument defaults. Read helpers have 30-second budgets;
commit has one 120-second budget across its subprocesses. Incomplete commit
receipts disclose potentially changed index or commit state rather than claiming
rollback. Local render/blame consumers decode MCP envelopes at their explicit adapter
boundaries. Local results use the same envelope; remote results and error
evidence remain intact. Envelope-shaped integration fixtures verify both paths.

Exact `code.read` and `code.readLines` now reject invalid UTF-8, split code points,
non-regular sources, source files over 16 MiB, and serialized results over 1 MiB.
Oversized ranges return an explicit refusal, never replacement or partial text.
`code.items` pages complete per-file inventories within 128 files and 1 MiB of
serialized JSON, with `next_offset` for the same input list. Oversized per-file
inventories fail explicitly. This bounds returned inventories; the underlying
fact parser still materializes one source file. Arbitrary `lsp.executeCommand`
calls use exclusive admission; refusing `workspace/applyEdit` does not prove
server-specific commands are free of other effects.

## Operator migration

Use JSON values in `additional_context` rules for numeric and boolean parameters:

```json
{
  "default:shell_run.timeout_ms": 5000,
  "pin:shell_run.timeout_ms": 5000,
  "default:file_edit.replace_all": false
}
```

An existing `"5000"` remains a string and does not satisfy an integer schema.
Change such rules deliberately; invalid typed rules fail instead of being
coerced. Host authority grants use their separate declared lookup and are not
ordinary model argument defaults.

MCP policy is per server, for example:

```json
{
  "mcpServers": {
    "fixture": {
      "type": "http",
      "url": "https://fixture.invalid/mcp",
      "required": true,
      "startup_timeout_ms": 30000,
      "tool_timeout_ms": 300000
    }
  }
}
```

Defaults are `required:false`, 30,000 ms startup, and 300,000 ms remote calls.
Startup accepts 1 through 300,000 ms; remote call deadlines accept 1 through
3,600,000 ms. Required startup failure aborts construction; optional failure
produces sanitized readiness. The admitted catalog stays fixed for that session.
Legacy `in-box`, `out-box`, and `both` placement remains compatibility vocabulary
for visibility, not an independent authorization boundary.

## Source disposition of the original findings

These are implementation dispositions, not gate, deployment, or model-efficacy
receipts. The original audit documents retain the original counterexamples.

| Finding | Disposition | Current contract or deliberate limit |
| --- | --- | --- |
| C1, C2 | Repaired | Captured child environment and wrapper dependency/argument admission apply across flat, nested and wrapped calls. |
| C3, C5 | Repaired | Small default exec guidance; declarations and invocation coordinates derive from admitted schemas. Missing output contracts remain `unknown`. |
| C4 | Repaired with limit | MCP envelopes retain mixed content and error evidence. Unsupported media is explicit; native media rendering is not claimed. |
| C6 | Repaired | Typed ordinary defaults and separate bounded policy observations preserve domain return shapes. Internal envelope consumer migration has its own integration check. |
| C7, L3 | Repaired with limit | Local completion ownership drains before cancellation acknowledgment. Remote uncertain outcomes are explicit and quarantined; remote cancellation cannot be proven by this host. |
| D1 | Repaired | Ranked paging and bounded receipts; atomic 32-tool/64-KiB activation limits. No silent eviction; historical schemas require a fresh focused session at capacity. |
| D2 | Repaired | Both code modes expose exact nested schemas and callable coordinates. Discovery and structured completion remain explicit direct controls; report bypasses workspace exclusion. |
| D3 | Repaired with compatibility limit | Identical shared tools deduplicate; distinct collisions refuse admission. Placement vocabulary remains supported and does not grant authority. |
| D4 | Repaired with lifecycle limit | Strict config, required/optional startup, metadata, remote deadlines, uncertainty and quarantine. Live tools/list reconciliation and automatic recovery remain intentionally unsupported. |
| D5 | Repaired | Canonical, qualified and JavaScript collisions fail; runtime installation checks reserved globals and property writes. |
| L1, L2, L9, L10 | Repaired in prior milestone | Strict provider arguments/terminals, explicit incomplete outcomes, and batch-safe schema validation. |
| L4, L6, L11 | Repaired with trust limit | Typed instruction graph, exact delivery generations, revocation, compaction invalidation and current resume context. Shell/remote effects and external filesystem races remain outside path-manifest enforcement. |
| L5, L12 | Repaired with recovery limit | Strict snapshot/checkpoint ownership, runtime reset notice, and refusal of outstanding or uncertain runtime state. No automatic reconstruction or live-handle serialization. |
| L7, L8, L13 | Repaired with wait limit | Shared control boundary, durable queue/ACK ordering and unsupported-control errors. Already admitted compaction/diagnostic work can delay interruption acknowledgment while it settles. |

Optional compatibility surfaces remain: `build.gate` uses shared shell admission,
explicit shell output filters report their omissions, redundant Git views and
`smart_read` are deferred, and preference nudges stay opt-in. Their continued
existence is not a claim that they improve task quality. Semantic correctness of
every Java/Rust transform remains outside this interface and lifecycle repair.

## Evidence and limits

`scripts/audit-harness-session-integrity.py` runs real harness processes against
an isolated local Chat fixture. It exercises flat and nested instruction barriers,
caught same-cell retries, missing/corrupt/tail-ahead resume, output-schema restore,
valid runtime reset, and competing writers. It uses fake credentials and isolated
HOME/BRO_HOME state. Its baseline passed only the corrupt-JSON rejection control;
the other eight cases exposed missing behavior. The Linux candidate passes all
nine. The full workspace run passed 6,930 tests; the final focused run passed
866, with three later reader-boundary checks also passing. All 60 executable
probes pass. Pinned formatting, workspace clippy and concurrency lint pass
(clippy retains existing repository warnings).

The [machine-readable receipt](session-and-catalog-repair-receipts.json) records
these checks. Optional-mode tool definitions fell from 95,620 to 29,387 bytes,
including an exec description reduction from 72,909 to 6,289 bytes. Flat-mode
definitions grew from 19,593 to 20,785 bytes as contracts became more explicit.
The final native installation repeats all 60 executable checks successfully.
The deployment and live results below complete the milestone.

Source changes and deterministic tests establish specific boundaries, not broad
model efficacy. Live comparisons use synthetic editing and retrieval tasks with
the same model, effort, and step budget. An initial ambiguous retrieval prompt is
excluded from comparison; the corrected prompt names the JSON property explicitly.
Independent artifact checks and observed verification commands are separate
measurements. One trial per condition is exploratory, not statistical evidence.


## Live-discovered Responses follow-up

The first four live trials of revision `76447947` failed before tool dispatch.
Their retained SSE evidence contained complete `response.output_item.done`
events followed by `response.completed` with `status:completed` and `output:[]`.
The provider integrity check from the prior milestone incorrectly treated that
empty terminal summary as contradictory output. Pinned Codex consumes completed
item events and uses completion for metadata and usage.

The follow-up treats an empty terminal output like an omitted output snapshot,
retaining completed streamed items only when no started item is unfinished.
Nonempty snapshots still undergo strict reconciliation. Sanitized function-call,
custom-call and reasoning fixtures exercise the observed shape; malformed,
duplicate, changed and unfinished cases retain their refusal coverage. The
failed live cohort remains in the receipt and is not presented as a successful
comparison.


## Final installation and live comparison

The final runtime revision is `26b02877d0d3ec6a9df1bcd413ee44a07f3aa1a4`, pushed
directly to `beta/blackbox-v2`. Native `bro-harness` and `isolate` are rebuilt,
installed and verified with persistent signing. All 60 executable probes pass
against those installed paths. Cluster workflow `build-bbox-image-4jjlx` built
image tag `26b02877d0d3`; convergence completed with the new deployment ready and
HTTP health status 200. The machine-readable receipt records binary hashes and
the immutable image digest. The failed `76447947` image was not rolled out.

Both cohorts used the same model (`gpt-5.5`), low effort, a 12-step budget, and
macOS arm64. Each retained task passed the independent artifact assertions and
had a successful verification command in the event record.

| Starting mode | Task | Baseline steps | Repaired steps | Baseline seconds | Repaired seconds |
| --- | --- | ---: | ---: | ---: | ---: |
| off | edit | 11 | 12 | 36.06 | 33.11 |
| off | retrieval | 8 | 7 | 24.90 | 17.04 |
| only | edit | 6 | 10 | 29.63 | 23.54 |
| only | retrieval | 6 | 10 | 21.91 | 24.05 |

There is no consistent step-count improvement. The catalog reduction and fixed
correctness boundaries are established; this small sample does not establish
broad model efficacy. `only` is the existing compatibility mode that defers
builtins but permits direct activation through `tool_search`. Both repaired
`only` trials used direct tools, so the table compares starting configurations,
not forced JavaScript execution. Fixture runtime files can enter Git observations,
which further limits timing and token comparisons. The initial provider failure
and the ambiguous-prompt exclusions remain explicit in the receipt.
