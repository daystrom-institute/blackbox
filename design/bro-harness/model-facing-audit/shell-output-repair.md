---
title: "Harness shell output integrity repair"
kind: design
lifecycle: implemented
corpus: blackbox-design
topic: [bro-harness, tools, output]
brief: "Bounded retained pages, streaming decoding and filtering, and complete build-gate capture."
---

# Shell output integrity repair

The prior shell buffer retained the first 8 MiB, so a noisy command could lose
its final error. Each poll drained the full buffer and only then applied the
small model response limit. Bytes outside that preview could never be recovered.
Lossy decoding at each poll corrupted split UTF-8 scalars, and filtering treated
each captured fragment as a complete line.

The output queue now retains the newest bounded data on overflow and counts
loss. It prepares decoded or filtered data without removing selected text, fits
both streams and metadata into the complete serialized JSON receipt, then
commits only the text actually delivered. Remaining bytes stay available through
`shell_poll`. `running` describes process activity; `output_pending` describes
unread data. An exited process keeps its session ID until pending data is read.
A zero output budget returns metadata without consuming text.

The UTF-8 decoder retains incomplete scalars between reads and pages. Invalid
input is replaced with explicit invalid-input counts. Regex filtering waits for
complete lines or EOF, and preserves a matching line across output pages. A
bounded oversized-line policy discards a whole oversized line through its next
newline, with explicit counters. A later filter applies prospectively to
unprepared lines, not already-selected page remainders. Reader failures and
aborted output drains disclose incomplete capture. Shutdown emits bounded final
facts and counts unread bytes discarded when the session is closed.

The host output limit travels in `ToolCx`, including flat and nested calls.
Page budgets account for JSON escaping, metadata and host argument-policy
riders before consumption. Large repeated rider values become explicit omission
counts without changing the arguments actually executed. The loop emits optional
hook and diagnostic context separately from shell JSON, so later context cannot
push already-consumed output out of the result budget.

`build.gate` declares both shell execution and polling dependencies. After process
exit it collects retained pages through the shared argument-policy path,
reassembles each stream and parses diagnostics once. It has a bounded aggregate
capture limit and stops on cancellation, refusal or nonprogress. Incomplete
capture retains continuation and sets `diagnostics_complete=false`; exit status
alone does not imply complete diagnostic evidence. Working-directory resolution
uses invocation policy, not optional presentation telemetry.

## Reference comparison

The Codex reference remains `242c5ce01cd3388f7d23a87b68615b2042a04bfc`.
`codex-rs/core/src/unified_exec/head_tail_buffer.rs` keeps bounded head and tail
segments and counts the omitted middle. `unified_exec/process_manager.rs`
drains collection, merges bounded head/tail output and decodes bytes lossily.
`tools/context.rs` separately tracks collection omission and model-budget
truncation. The harness adopts explicit bounded loss accounting; retained
`output_pending` paging and incremental decoding are local improvements, not a
claim that Codex offers lossless paging.

## Evidence and limits

`scripts/audit-harness-shell-output.py` exercises the real isolate and a local
scripted provider endpoint. No real provider requests are made. Its seven cases
cover exited multipage stdout/stderr, a metadata-only first page, split UTF-8,
split filtered lines, final-error retention beyond the buffer cap, and escaped
flat output under 1 KiB and 16 KiB host limits. Each reconstructed stream must
match exactly where no capture loss is expected. The baseline and candidate
results live in `shell-output-repair-receipts.json`.

The baseline is installed macOS arm64 `c384fb8a`; the candidate is Linux amd64.
Results prove the boundaries under test, not comparative platform performance.
Unit tests also exercise overflow accounting, invalid input, reader failures,
filter changes, argument-policy rider budgets, large hooks, build diagnostic
reassembly, and refused output collection.

Validation: 704 targeted tests and 6,817 full-workspace tests passed (19 skipped
in the full profile). All seven output probes and all fourteen earlier authority
and cancellation probes passed. Workspace Clippy, concurrency lint and pinned
formatting passed, with existing repository warnings.

Retained memory is bounded. Overflow, invalid input replacement, intentional
filter exclusions, oversized lines and session shutdown can still lose data;
those losses are disclosed. Buffer byte counts describe retained raw/decoded
storage, not original-source offsets after decoding. Paging is destructive
consumption, not replay: concurrent readers share one queue. Model-authored
JavaScript can still discard or overprint the data it receives. Process activity,
capture completeness and diagnostic completeness remain separate facts.

The authority and cancellation repairs were fast-forwarded to beta before this
slice. This output repair also lands directly in `beta/blackbox-v2`. Installed
macOS binaries and shared services are unchanged by this source repair.
