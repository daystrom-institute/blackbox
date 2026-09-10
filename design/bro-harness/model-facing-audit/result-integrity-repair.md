---
title: "Provider and result integrity repair"
kind: design
lifecycle: archived
corpus: blackbox-design
topic: [bro-harness, tools, audit]
brief: "Strict provider admission, truthful terminal outcomes, MCP evidence, patch prefixes and bounded workspace observations."
---

# Contracts

This implements order 3 in the [repair plan](../model-facing-tools-repair-plan.md),
plus the workspace-observation portion of the builtin audit. The original audit
receipts describe the broken baseline, not current behavior.

Provider function arguments must decode to a JSON object. Missing/malformed,
null, scalar and array payloads cannot acquire configured tool defaults.
The same object admission applies through the shared nested/wrapper dispatcher;
explicit freeform strings retain their grammar-owned path. Rejected provider
responses preserve diagnostic evidence without committing partial native replay
or admitting client tools.

Chat requires a finish reason and terminal marker, with stream idle protection.
Anthropic requires terminal message and closed blocks. Responses reconciles
completed items with nonempty terminal output snapshots; metadata-only empty
terminal output retains fully completed streamed items. Partial client calls cannot
execute on length/incomplete outcomes. Responses refuses retry/fallback after
provider output has begun, including tool/item events before visible text.

Output-limit, step-limit, unsupported provider stops, empty terminal responses
and missing required structured results emit `subtype: incomplete` with
`is_error: true` and a stop reason. Interruption remains separately identified.
Result text comes from the terminal model step rather than earlier narration.
This is a mechanical completion contract, not an automatic judgement that the
user's task is solved.

An output schema must parse and compile at setup. A terminal `final_result`
must be alone in its response and conform to that schema. Invalid or sibling
batches get paired errors without executing any member. Valid structured
completion follows ordinary turn cleanup rather than returning around it.

MCP tools share an explicit envelope across remote, in-process, compatibility,
flat and nested paths: `content`, optional `structuredContent`, and `isError`.
Text warnings/cursors and structured rows are retained together with resource
references. Protocol-private metadata is removed at its protocol locations;
application fields inside structured results are not recursively scrubbed.
Image/audio/blob payloads are not rendered natively by this text-only boundary:
the envelope records omitted encoded byte counts, types and available references.
Media-only replies become explicit host presentation errors without rewriting
the server's original status. This does not claim lossless multimodal delivery.

Patch application remains sequential. A failure retains the completed prefix,
per-mutation pre/post byte evidence, created directories and uncertain paths
from failed writes. The edit tracker sees completed changes on failure as well
as success. Repeated-path edits and overwritten move destinations use actual
preimages captured at mutation time. This does not promise rollback or atomic
multi-file application.

Search/glob retain their bounded text surface and disclose observed skip/error
counts and scope. Explicit file/directory targets override name-based root
pruning; descendant pruning and ignore policy remain visible. Traversal limits
count visited entries independently of matches. Zero result limits are rejected.
Actual file reads are bounded even if a file grows after metadata inspection.
Empty exact-replacement needles reject before file access. Uniform CRLF patch
sources retain CRLF bytes; mixed-newline files keep their existing per-line
behavior rather than guessing a global style.

# Reference and validation

Reference remains Codex `242c5ce01cd3388f7d23a87b68615b2042a04bfc`.
Codex Responses rejects incomplete responses and EOF before completion. Our
multi-provider loop preserves valid text-only length output as an explicitly
incomplete result, and treats malformed SSE JSON as a strict diagnostic error.
No full SSE framing rewrite or multimodal transport parity is claimed.

`scripts/audit-harness-result-integrity.py` uses local HTTP fixtures, synthetic
credentials and temporary roots. The baseline demonstrates malformed direct and
nested calls mutating files through defaults, and falsely successful truncated,
limited or invalid structured results. Valid mutation and final-result controls
prove that refusal cases did not merely disable the tool surface.

Regression tests also traverse a real MCP connection, both dispatch adapters and
V8, check actual patch filesystem/tracker outcomes, and verify provider replay
stays unchanged after rejection. Full gate and before/after receipts are recorded
at the milestone below.

The [before/after receipt](result-integrity-repair-receipts.json) records all 16
provider/result and 10 workspace cases passing in the candidate. Baseline passed
only the two valid provider controls and one explicit-file workspace control.
All 21 prior authority/cancellation/output probes also pass. Baseline ran on
macOS arm64 and candidate on Linux amd64; this is not a latency comparison.

Focused gate: 797 passed. The initial full gate passed 6,848 tests. After the
three final edit regressions, the full run passed 6,849 and hit two existing
three-second store-lock test deadlines; both passed in isolation in 0.466 seconds.
Nineteen tests were skipped by configuration. Pinned formatting, workspace
Clippy and concurrency lint passed; Clippy retains existing repository warnings.


The later [session/catalog milestone](session-and-catalog-repair.md) records
installation of this slice, the live-discovered empty Responses terminal-output
compatibility correction, and successful live verification of the repaired build.
