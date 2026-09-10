---
title: "Model-facing tool audit: read recovery and Codex comparison"
kind: design
lifecycle: partial
corpus: blackbox-design
topic:
  - bro-harness
  - tools
  - context-management
brief: "Codex comparison and synthetic probes drive bounded exact reads, simpler defaults, output-budget enforcement, commit isolation, and dispatch/compaction repairs; scoped instruction delivery remains open."
---

# Model-facing tool audit

This is the initial focused repair audit. The subsequent
[comprehensive contract audit](model-facing-tools-comprehensive-audit.md) inventories
the full local interface, compares the loop and transport lifecycle with Codex,
and records synthetic probes and live task trials.

The recursive dump reads are an affordance failure in the harness. The model
follows the recovery instruction supplied by the tool result, and that recovery
can reproduce the same oversized result indefinitely. Smaller original-source
reads work around it, but the ordinary tool contract should make that behavior
the default.

## Implementation outcome

The repair branch replaces recursive dump recovery with explicit bounded output.
File reads preserve source bytes, including line terminators, and paginate by
source line or byte offset. Unnumbered byte pages seek directly instead of
rescanning the prefix. Source output defaults to 200 lines and 8,000 bytes.

`smart_read` remains a deferred compatibility alias for the exact reader; its
regex outlines are removed. Git/grounding convenience tools remain discoverable
but leave the eager tool catalog. Globs default to name order, with recency an
explicit option. The heuristic toolbox-preference nudges default off, with
explicit opt-in retained. This changes runtime defaults, not operator memory.

The host now enforces exec/wait budgets, preserves lifecycle/error information,
reports image output as unsupported, and drives the isolate CLI through yielded
results using the new status prefix. Search, glob, shell, directory, and web
output are bounded; directory listings page in name order. `file_write` requires
content in its schema, malformed listing/show selectors fail, and `git_show`
ends option parsing before the revision argument.

Tool batches retain the model's read/write order; only adjacent reads overlap.
Completed reads survive sibling cancellation. One session gate now excludes
mutations across flat and nested calls while allowing cell controls to continue
or cancel live work. Automatic and overflow compaction restore startup context
before requesting another model step. `git_commit` validates literal files,
refuses foreign staged content, and uses selected-path commit semantics.

### Broader loop audit and remaining boundary

The Codex comparison also inspected
[`parallel.rs`](https://github.com/openai/codex/blob/242c5ce01cd3388f7d23a87b68615b2042a04bfc/codex-rs/core/src/tools/parallel.rs)
and
[`compact.rs`](https://github.com/openai/codex/blob/242c5ce01cd3388f7d23a87b68615b2042a04bfc/codex-rs/core/src/compact.rs).
It found that the old read-before-write partition was not equivalent to Codex's
per-invocation gate, nested calls had a separate admission path, and mid-turn
compaction cleared context state without immediate reinjection. These are fixed
in this branch with behavior tests.

**Open: gap-41e7593e.** Dynamically discovered scoped AGENTS documents still arrive
only after successful flat first-touch operations; nested calls do not share
that delivery lifecycle. Startup context restoration does not restore all prior
scoped riders. The next repair needs shared pre-edit instruction delivery and
compaction restoration with durable receipts. It must preserve full instruction
bodies, not treat them as ordinary truncated output. Until that is implemented,
explicitly read scoped instructions before editing. The current changes preserve
existing scoped riders intact rather than silently truncating them.

The broader audit supports retaining simple exact reads/search/patch/shell,
explicit discovery, and a bounded empty-response retry. No task-level A/B result
is claimed for removing smart defaults: the evidence is deterministic contract
failures and unsolicited preference steering. Real-provider efficacy comparison
remains useful after rollout.

## Evidence and scope

Audited 2026-09-10 against Blackbox checkout
`9d9dc270b26bf2dfbb819e4d8d00714b01df092e`. The clean local Codex `main`
checkout was fetched and fast-forwarded from `cbfd999db78cb088d2bd89b52051efe6f44555a4`
to upstream `242c5ce01cd3388f7d23a87b68615b2042a04bfc`.

The audit covers the registered built-in tools, their schemas/descriptions,
the agent-loop result boundary, code-mode host adaptation, and tool discovery.
It does not certify every domain-specific refactor namespace or remote MCP
implementation. The older [tool-surface design](bro-harness-tool-surface.md)
is archived and contains a historical inventory; it is not the current schema
authority.

Evidence levels used below:

- **Source-confirmed:** directly inspected executable code and schema builders.
- **Probe-confirmed:** synthetic calls through the installed `isolate`, whose
  relevant behavior agrees with the checkout, or a tiny Rust executable importing
  the checkout's actual `bound.rs`. No workspace build was needed. The installed
  binary was not rebuilt or claimed to be byte-identical to this checkout.
- **Inferred consequence:** follows from composing the confirmed paths, without
  replaying a complete production provider session.

All probe files and Git commits were confined to canonicalized temporary roots.
Read-only status checks showed the two operator-referenced tasks using bounded
source reads/search after steering. Their earlier complete dump chains were not
retrieved, so the fixture proves the mechanism rather than an exact incident
timeline. Client source and identifiers are excluded from this artifact.

## Findings

### 1. High: recovery recursively spills and loses source coordinates

[FileRead](../../crates/bro-tools/src/workspace.rs) defaults to 2,000 lines,
has no byte budget, and appends continuation guidance at the end. The
[agent loop](../../crates/bro-harness/src/agent_loop.rs) converts each result
to text and calls [bound_tool_result](../../crates/bro-harness/src/bound.rs).
That helper spills results above 16,384 bytes, retains only the first 2,048
bytes, and instructs the model to read the full dump with `file_read/smart_read`.
It has no original path, input range, or source-to-dump mapping.

A dump read receives exactly the same treatment. If `line_numbers=true`, each
read numbers the already-numbered payload again. The source reader's final
continuation hint disappears from the inline preview. Neither the dump rider
nor the reader description recommends a bounded recovery call.

**Probe-confirmed:** a 2,001-line synthetic file read returned 210,954 bytes
including its continuation marker. Three applications of the actual bounding
helper, adding line prefixes between recoveries, produced:

| Round | Incoming bytes | Inline bytes | New dump | Original continuation visible |
|---|---:|---:|---|---|
| Initial read | 210,954 | 2,248 | Yes | No |
| First recovery | 219,851 | 2,248 | Yes | No |
| Second recovery | 228,749 | 2,248 | Yes | No |

The first line accumulated `1\t1\t1\t1\t...`. A separate `max_lines=1`
call returned a 50,000-character line whole. Reducing the line default alone
cannot guarantee bounded recovery. `max_lines=0` also succeeds with a
non-progressing `start_line=1` continuation.

Tracked by **gap-49db33e3**.

### 2. High: code-mode budgeting was not adopted with the runtime

[ExecTool](../../crates/bro-harness/src/code_mode.rs) parses and passes
`max_output_tokens` into `ExecuteRequest`; the vendored runtime does not consume
it, and the harness host performs no per-call token truncation. The
[description](../../crates/bro-code-mode/src/description.rs) nevertheless promises
a 10,000-token default and caller control. `wait` advertises `max_tokens`, but
its actual schema and handler do not implement that argument.

**Probe-confirmed:** the following installed-isolate cell returned every emitted
character, 20,001 bytes including the CLI newline:

```javascript
// @exec: {"max_output_tokens": 1}
text("x".repeat(20000));
```

The live agent loop adds only the unrelated 16 KiB spill afterward. Thus an agent
cannot fix a large cell response using the documented budget parameter.

`response_to_result` appends the running-cell ID, notifications, and error text
after the body. **Inferred consequence:** a large body can push the cell ID or
error beyond the outer head preview, hiding the information needed to continue.
Success/error classification remains separate, but the explanation can vanish.

There is another source-confirmed capability mismatch: `image()` is advertised,
but `join_content` replaces image items with an omission message. The host
transport contract needs completing or the advertised capability needs narrowing.

Tracked by **gap-3b0f218b**.

### 3. Medium: producer budgets conflict with the outer renderer

- `smart_read` loads the complete file and returns all regex-matched definitions
  plus a 40-line head. Neither path has a byte bound. A 500-definition fixture
  returned 67,723 bytes, so the supposedly compact outline itself spills.
  Numbered dump lines also interfere with its definition regex.
- `content_search` limits content to 24,000 bytes and appends refinement guidance.
  That exceeds the outer 16,384-byte threshold. A fixture returned 24,284 bytes;
  the outer renderer would hide its useful refinement footer. The earlier fix in
  **gap-0c902d6d** addressed the producer in isolation but not this composition.
- `shell_run` and `shell_poll` default to approximately 40,000 bytes **per stream**.
  Their rendered JSON can therefore exceed 80 KB before escaping and metadata.
  The shell preserves the tail, then the outer renderer retains only the head of
  that result. A compact serialized JSON dump can be one very long physical line,
  making line-based recovery especially ineffective.
- `exec` can aggregate many individually modest nested results. Nested calls
  return data directly through the capability seam; the final emitted cell result
  is where the agent-loop spill applies. A correct solution should keep that
  useful in-cell data access while bounding model-visible emission.

The spill preserves the tool's returned payload, not necessarily the original
underlying data. Search, shell capture, and web extraction may already have
discarded content before the spill. Recovery language should disclose this.

Covered by **gap-49db33e3** and **gap-3b0f218b**.

### 4. High, independent: git_commit does not enforce its advertised scope

[GitCommit](../../crates/bro-tools/src/workspace.rs) checks only for an empty
path vector and sensitive-looking input strings, runs `git add -- <paths>`,
then runs an unrestricted `git commit -m <message>`.

**Probe-confirmed:** with `peer.txt` already staged, a call naming only `mine.txt`
committed both. A subsequent call with `paths=["."]` succeeded despite the tool's
explicit claim to reject that form. Directory/glob expansion also means checking
only the supplied path strings does not validate every file being staged.

This needs exact path validation and commit/index isolation, with explicit
handling of pre-existing staged changes. It is not resolved by improving prose.
Tracked by **gap-5abdc71f**.

## Comparison with current Codex

Upstream references below are pinned to the inspected commit.

| Concern | Current Codex | Blackbox implication |
|---|---|---|
| File/search authoring | The inspected [tool plan](https://github.com/openai/codex/blob/242c5ce01cd3388f7d23a87b68615b2042a04bfc/codex-rs/core/src/tools/spec_plan.rs) registers `exec_command` and, when enabled, `write_stdin`. No native `read_file`/`grep_files` implementation is registered there. | Do not invent a current Codex read-tool schema to copy. Our dedicated readers are useful if their contracts are coherent. |
| Code-mode output | [The host adapter](https://github.com/openai/codex/blob/242c5ce01cd3388f7d23a87b68615b2042a04bfc/codex-rs/core/src/tools/code_mode/mod.rs) truncates emitted content using the requested budget, then prepends script status/cell ID. | This host responsibility is missing from our runtime adoption. |
| Wait budget | [The wait handler](https://github.com/openai/codex/blob/242c5ce01cd3388f7d23a87b68615b2042a04bfc/codex-rs/core/src/tools/code_mode/wait_handler.rs) accepts `max_tokens` and passes it to result formatting. | Implement the advertised field consistently. |
| Shell result | [ExecCommandToolOutput](https://github.com/openai/codex/blob/242c5ce01cd3388f7d23a87b68615b2042a04bfc/codex-rs/core/src/tools/context.rs) separates command metadata from raw output, applies output policy, and reserves room for headers. | Preserve status and handles outside text truncation; account for serialization. |
| Truncation | [The shared output utility](https://github.com/openai/codex/blob/242c5ce01cd3388f7d23a87b68615b2042a04bfc/codex-rs/utils/output-truncation/src/lib.rs) uses explicit omission markers and middle truncation. The inspected shell/code-mode paths do not redirect recovery into recursive file dumps. | Use a shared result-budget contract, not an unconditional rendered-string spill as the normal reader UX. |
| Media | The code-mode host carries image content items through conversion and detail sanitization. | A described image helper should deliver images through the complete host path. |

This is a comparison of inspected paths and configuration-dependent registration,
not a claim that every Codex session exposes the same tool set. Copying the entire
upstream runtime is unnecessary to adopt these result-shaping principles.

## Remaining built-in surface review

| Surface | Assessment |
|---|---|
| `file_edit` | Unique-match failure and explicit `replace_all` form a coherent small-edit interface. Retain. |
| `file_write` | Runtime correctly refuses missing content, but schema makes content optional. Make requiredness match the handler. |
| `apply_patch` | Grammar-aware transport gating is coherent; preserve provider compatibility rather than exposing unsupported freeform grammar. |
| `glob` | Documents its string return and mtime ordering. Count cap alone cannot guarantee a byte cap; selection should remain deterministic for any future paging. |
| `list_dir` | No page/byte bound; malformed inputs silently fall back to root listing. Add bounded enumeration and reject malformed selectors. |
| `git_status/log/diff/show` | Small wrappers are understandable but overlap shell capabilities. Diff/show lack path and output selectors; `git_show` silently substitutes HEAD on malformed input. Candidates for deferral and tighter schemas. |
| `shell_run/poll/kill/list` | Yield, session, stdin and exit contracts are useful. Preserve these while fixing output budgets and outer rendering. |
| `web_fetch` | Character cap is documented, but truncation has no result marker or continuation. Entire response body is loaded before extraction. Characters are not a byte budget, especially for non-ASCII content. |
| `todo_write` | Clear replacement operation and persisted task state; no read-recovery defect identified. |
| `sandbox_status/grounding` | Orientation roles are sensible. Not a substitute for per-operation correctness or the shared-service approval boundary. |
| `tool_search` and deferred MCP | Compact activation results and subsequent schema delivery are useful. Preserve this design. Broad output reformatting must not destroy upstream continuation tokens or typed result fields. |

These are bounded audit observations, not a full security or correctness review
of every implementation. No wholesale tool renaming or larger ambient prompt is
needed to address the demonstrated failures.

## Proposed repair sequence and acceptance checks

1. **Make result shaping source-aware.** Bound source reads by both lines and
   bytes, preserve original path/range, and provide a progressing continuation.
   Reject zero-sized non-progressing pages. Handle long lines explicitly with a
   byte/chunk continuation rather than pretending a line cap is sufficient.
2. **Complete the code-mode host adapter.** Enforce exec/wait budgets, keep cell
   IDs/status and error identity outside body truncation, and preserve notifications
   deliberately. Account for the final model-facing envelope, including riders.
3. **Make overflow recovery terminate.** Source reads should point back to source.
   For non-source results that need durable overflow, use an immutable result
   handle with bounded byte-aware retrieval. Reading that handle must not create
   another overflow object or renumber source content. Disclose upstream omissions.
4. **Align producers with that contract.** Bound smart outlines, searches, shell
   streams, listings and web excerpts under the actual response envelope budget.
   Preserve refinement/continuation metadata; retain raw values inside code-mode
   until the caller explicitly emits them.
5. **Repair git_commit separately.** Validate expanded exact files and preserve
   peer index state. Test named paths, existing staged content, directory and
   wildcard pathspecs, deletions, and sensitive descendants in temp repositories.
6. **Validate the complete delivery path.** Mock provider tests should cover a
   large numbered source file, a huge single line, dense outlines, search overflow,
   simultaneous stdout/stderr, and a yielding cell that emits large output. Assert
   that continuation makes progress, budgets work, handles remain visible, and
   errors retain useful explanations. Include flat and code-mode paths. Follow
   with a narrow live provider read task to verify behavior, without client data.

The initial audit changed no runtime code. The implementation outcome above
records the subsequent repair pass; verification receipts are recorded below.
No shared service was restarted during development.


## Validation receipts

Validation ran in an isolated cluster lane on the repair branch, using synthetic
fixtures and temporary Git repositories. No installed harness or daemon was
replaced, and no paid live-provider A/B trial was run.

- Focused bro-tools, bro-harness, and bro-code-mode nextest selection:
  **704 passed, 4 skipped**.
- `cargo nextest run --workspace --profile full --no-fail-fast`:
  **6,770 passed, 19 skipped**, including tool ordering, interruption, shared
  admission, compaction restoration, output envelopes, source paging, and Git
  isolation regressions.
- `cargo clippy --workspace` completed successfully with existing warnings.
  A subsequent mechanical reader lint cleanup passed `cargo clippy -p bro-tools`
  without warnings, and its three dedicated reader tests passed again.
- `scripts/lint-concurrency.sh`: **108 handlers checked, passed**.
- `scripts/fmt.sh --check -p bro-tools -p bro-harness -p bro-code-mode` and
  `git diff --check`: **passed**.
- A rebuilt standalone `isolate` probe confirmed bounded source pages,
  progressing UTF-8 byte continuations, exact `smart_read` compatibility, and
  directory pagination. A second real V8/CLI probe requesting one output token
  retained only the head/tail selection and an explicit omission marker from a
  20,004-character emission, with completion status intact. Envelope/status
  overhead is separate from the requested payload budget.

These checks demonstrate repaired contracts. Whether the simpler default
surface improves completion rate or cost needs a controlled task-level trial
after rollout. The open scoped-instruction lifecycle gap is not covered by a
claim of parity with Codex.
