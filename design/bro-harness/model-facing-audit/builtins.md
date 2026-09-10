---
title: "Builtin editing, process, and Git contract audit"
kind: design
lifecycle: partial
corpus: blackbox-design
topic: [bro-harness, tools, audit]
brief: "Source-grounded contract findings at c384fb8a, compared with Codex 242c5ce0; evidence and repair contracts, not a runtime repair claim."
---

[Audit overview](../model-facing-tools-comprehensive-audit.md). Source anchors refer to the pinned snapshots named below.

# Builtin editing, shell lifecycle, Git and trust audit

Snapshot: blackbox `c384fb8a1cc022f7a8bc00f94ad0462373ca1a8e`; Codex `242c5ce01cd3388f7d23a87b68615b2042a04bfc`. Read-only audit of repository source, with synthetic probes through the installed `isolate` executable. No repository edits, builds, service mutations, real credentials, or shared project files used. Probe fixture roots were temporary directories with a subprocess-local temporary HOME; fixtures were removed. Raw probe outcomes are in [builtin-probe-receipts.json](builtin-probe-receipts.json).

Evidence labels: **reproduced** means installed-isolate behavior observed; **source** means a directly identified code path, not a runtime reproduction. Installed binary provenance was not rebuilt or independently attested to the source SHA; observed results agree with the inspected source. One initial cancellation experiment did not actually trigger cancellation and is excluded as proof; the subsequent explicit-yield experiment did.

## Priority view

1. **Authority and lifecycle contracts:** subprocess credential scrubbing differs across raw Git and nested shell paths (findings 11-12, source); cancellation can report completion while a detached patch still changes files (5, reproduced). These outrank catalog ergonomics.
2. **Mutation and process truth:** partial patch writes are undisclosed (4), destination overwrite accounting is false (6), hard process deadlines and stdin deadlines do not hold (1-2). All have concrete probes.
3. **Evidence integrity:** true trailing errors disappear at the capture cap (3), matching lines disappear at poll boundaries (7), and completed processes are advertised as running (10).
4. **Surface simplification and input hygiene:** empty replacement, inconsistent newline handling, redundant Git views, arbitrary signal values and misleading descriptions. These are real but lower priority than authority violations.

The report deliberately distinguishes our stronger AddFile refusal from Codex's overwrite behavior, Codex's nontransactional filesystem failure delta from imaginary atomicity, and full-host path authority from a nonexistent sandbox escape.

## Contracts and disposition

| Surface | Actual contract | Decision |
|---|---|---|
| `file_edit` | Read entire UTF-8 file, count exact string matches, replace unique or all matches, overwrite file, record pre/post diagnostics. Relative and absolute paths; follows symlinks. | Keep as small exact editor, especially on transports without patch grammar. Reject empty search text for ordinary replacement, and give writes a shared cancellation/mutation completion contract. |
| `file_write` | Create parents and overwrite complete file contents; no create-only or expected-version check. Preimage failures become empty bytes. Follows symlinks. | Keep for creation and intentional full replacement, but make overwrite semantics explicit. Prefer create-only default or an explicit overwrite flag if preserving peer edits is a product invariant. |
| `apply_patch` | Codex-format parser with fuzzy line lookup and sequential synchronous filesystem mutations; available to grammar-capable providers. Add refuses an existing target; Move overwrites it. | Keep the format. Adopt verification and mutation accounting from the reference rather than extending the custom apply engine. Current partial failure and cancellation semantics need repair. |
| `shell_run` | Always `bash -lc`, pipes only, configurable cwd/env/stdin, 1s default yield, no default hard timeout; retains a session after yielding. | Keep as foundational process primitive. Fix supervision and capture before adding more wrappers. Optional shell/login/TTY choices are reference capabilities to consider, not automatic requirements. |
| `shell_poll` | Removes session from map, optionally writes/closes stdin/signals, drives until exit/yield/deadline, drains output, reinserts if running. 5s default yield; 0 waits indefinitely. | Keep. Use per-session interaction state with bounded waits and input error reporting. |
| `shell_kill` | Removes session, sends group signal, waits up to grace, then escalates; returns final output. | Keep as explicit cleanup handle. Validate signal names and make it usable while other sessions are waiting. |
| `shell_list` | Lists retained entries, including already-exited children, with command and elapsed time. | Keep only if listing state is truthful; distinguish running from exited-with-unread-output. |
| `output_filter` on shell tools | Regex-matches independently drained fragments as if they were complete lines; unmatched fragments are discarded permanently. | Remove from the default model-facing contract. It duplicates shell composition while changing evidence semantics and carrying session state. If retained, implement streaming line assembly and explicit capture/filter loss metadata. |
| `git_status`, `git_log` | Fixed commands, no path/revision/count controls, raw uncapped process capture. | Defer or remove from default catalog; shell handles these with a single lifecycle contract. |
| `git_diff` | Entire unstaged diff; optional sequential per-untracked-file `git diff --no-index`. No staged diff, path selectors or process deadline. | Remove or reduce to an explicit bounded view. Current wrapper has less control than shell and an expensive all-untracked mode. |
| `git_show` | `git show --end-of-options <rev>`; malformed input now rejects. Raw process capture. | Defer/remove default wrapper. The revision option-injection repair is present and is not repeated as an open defect. |
| `git_commit` | Explicit literal file selection; validates secrets/directory/glob/symlink paths; refuses foreign staged content; stages selected files and commits with `--only`. | Keep as optional guarded mutation if its safety value is desired. The prior foreign-staging/pathspec defects are repaired. Route its subprocesses through the same env/cancellation machinery as shell. |
| `SafetyPolicy` | Regex denylist of source command text and filename heuristics for commit. No parser, shell evaluation, filesystem sandbox, or override mechanism. | Keep only as an accurately described advisory/accident guard, or replace it with a real policy layer. Do not present it as categorical enforcement. |

The inventory above is the builtin implementation inventory, not a claim that every item is advertised on every provider or prompt. Root is auditing projection and discoverability separately. These entries originate in `crates/bro-tools/src/lib.rs:50`.

## Findings that affect correctness and supervision

### 1. A yielded command's hard timeout is inactive until another poll (reproduced, high)

Sources: `crates/bro-tools/src/shell.rs:95`, `:195`, `:718`, `:743`, `:874`.

`kill_at` is data in a retained session. Only an active `drive()` future selects on it. After `shell_run` yields, there is no timer task supervising that deadline. Thus `timeout_ms` is not the documented maximum process runtime.

Probe: run `sleep 0.3; printf survived > sentinel.txt; sleep 1` with `yield_time_ms=10, timeout_ms=50`. Wait 0.5 seconds using another fixture command, then read the sentinel. It contains `survived`. Polling the original session afterward finally reports `timed_out=true` and kills it. A model that spends time thinking or executing another tool permits post-deadline side effects.

Repair: own deadline enforcement in the process supervisor independently of model polling. Distinguish an observation wait from a hard process lifetime. Codex distinguishes interactive process lifetimes and completion-only execution in `core/src/tools/handlers/unified_exec/exec_command.rs:70` and `core/src/unified_exec/process_manager.rs:609`; do not imply Codex's interactive exec exposes exactly our timeout interface.

### 2. Stdin writes bypass deadlines and suppress input failures (reproduced/source, high)

Sources: `shell.rs:700-718`, `:855-874`.

Initial stdin `write_all` and `flush` happen before the timeout and yield deadlines are constructed. Poll stdin writes happen before `drive()` observes the original deadline. All write/flush failures are ignored; supplying stdin after `close_stdin` is silently ignored too.

Probe: `command="sleep 2", stdin="x" repeated 1,000,000, timeout_ms=100, yield_time_ms=10` returned after 2.025 seconds with `exit_code=0, timed_out=false`. The child never consumed the input. A long-running process that never reads stdin can block the tool arbitrarily despite both requested bounds. Poll has the same ordering issue. At initial stdin time `ShellSession` has not yet been constructed, so cancellation at this stage also lacks its group-wide Drop cleanup.

Repair: establish process ownership and deadlines immediately at spawn; send input through supervised bounded I/O; report closed/broken input explicitly. Codex has separate typed `write_stdin`, rejects unavailable stdin and reports write failures (`core/src/unified_exec/process_manager.rs:920`; `process.rs:150`).

### 3. Capture discards the actual tail even though output promises to retain it (reproduced, high)

Sources: `shell.rs:38`, `:66`, `:241`, `:379`.

`OutBuf` retains the first 8 MiB and drops all later bytes until drained. `cap_tail` then keeps the tail of that old retained prefix, not the tail of the process stream. The buffer-drop count is disclosed, but the advertised reason for tail retention, preserving final errors, fails precisely on noisy commands.

Probe: a Python command wrote 9,000,000 `x` bytes followed by `FINAL_FAILURE_MARKER`, then exited 17. With `yield_time_ms=0` the result retained the exit code but omitted the marker entirely, displaying only `x` characters and drop counters.

Repair: bounded head/tail or ring capture at ingestion, with omission counts. Codex already implements a stable prefix plus rolling suffix in `core/src/unified_exec/head_tail_buffer.rs:5-99` and uses it in the process manager. Its capture cap is 1 MiB (`unified_exec/mod.rs:80`); ours permits 16 MiB per retained session across two streams and up to 32 sessions, before extra render copies.

### 4. A failed multi-file patch can have already changed files without accounting (reproduced, high)

Sources: `crates/bro-apply-patch/src/apply.rs:66-138`; `crates/bro-tools/src/workspace.rs:1405-1425`.

The apply engine validates and executes each hunk in sequence. On a later error, `ApplyOutcome` is discarded; the adapter returns immediately and records none of the successfully applied earlier changes in `EditSink`.

Probe: patch adds `first.txt`, then updates nonexistent `missing.txt`. Result: error `failed to read missing.txt`, but `first.txt` exists with its new contents. The output does not tell the model the first operation succeeded. Retrying the envelope then conflicts with the newly created file.

Repair: verify all resolvable patch operations before mutation; preserve and report a committed partial delta on I/O failure. Do not claim filesystem-wide atomicity. Codex verifies all parsed hunks before executing (`core/src/tools/handlers/apply_patch.rs:403`; `apply-patch/src/invocation.rs:216-289`) and has `ApplyPatchFailure` carrying an `AppliedPatchDelta` (`apply-patch/src/lib.rs:311-334,431-443`), including uncertain partial writes. This is a meaningful reference contract missing from our port.

### 5. Patch mutations continue after code-mode cancellation (reproduced, high)

Sources: `workspace.rs:1381-1387`; `crates/bro-tools/src/tool.rs:65`; `crates/bro-code-mode/src/service.rs:695-700`; `crates/bro-harness/src/capabilities.rs:88-99`.

The patch runs in `spawn_blocking`; dropping the awaiting tool future does not stop an already-running blocking closure. Cancellation can release the shared dispatch gate while the closure continues doing filesystem work.

Probe: temporary FIFO blocks patch preimage capture. Cell starts with `// @exec: {"yield_time_ms":1}` and runs a patch adding `late.txt` then deleting the FIFO. Installed isolate is invoked with `--cell-timeout=1`. It reports `cell 1 exceeded --cell-timeout 1s after 1.0s`. A fixture-only writer unblocks the FIFO at 2 seconds. At process exit, `late.txt` exists and the FIFO has been deleted. These mutations happened after the tool was cancelled. Process wall time was 2.009s.

Repair: cancellation of a mutation must mean either confirmed stop before mutation or wait for committed work and report its actual outcome. Keep mutation admission ownership until the blocking work finishes. This is more important than adding another lock around the initial async future. No claim that all possible Codex filesystem backends are atomically cancellable; its committed-delta model is the useful reference for honest accounting.

### 6. Patch moves overwrite unrelated destination data and report a false preimage (reproduced/source, high)

Sources: `bro-apply-patch/src/apply.rs:109-126`; `bro-tools/src/workspace.rs:1405-1416,1443-1450`.

Probe: `source.txt="original\n"`, `destination.txt="peer content\n"`; update source and Move to destination succeeds, destroys destination's previous contents, and removes source. Only source paths are captured in `pre_images`; the destination edit event records `pre=[]` as if it were newly created.

Important comparison: Codex also supports replacing a move destination, so overwriting is not by itself an upstream incompatibility. Codex explicitly captures `overwritten_move_content` in the committed delta (`apply-patch/src/lib.rs:638-689`). Our adapter loses that information. Given the repository's peer-work invariant, choose either fail-closed collision handling or an explicit replace contract, and always capture the real destination preimage.

## Evidence-changing convenience behavior

### 7. Post-capture line filters discard matches split across polls (reproduced, medium)

Sources: `shell.rs:350-376,393-425,846-850`.

Probe command emits `ER`, sleeps across the initial yield, then emits the remaining `ROR` text. A stdout filter containing `ERROR` discards both fragments, reports one dropped line on each poll, and returns no matching evidence. The command's combined stdout contains `ERROR`; neither fragment does. The implementation treats every drain boundary as a line boundary and forgets partial lines. Changing the filter in a later poll also cannot recover earlier discarded data.

Recommendation: remove this convenience from the default model-facing tool; retain raw bounded process output. If the feature is retained as an explicit advanced option, carry incomplete lines across snapshots and disclose upstream buffer loss separately from filter omission. Codex's exec output is not subjected to an analogous model-configured persistent regex filter in the inspected manager.

### 8. Empty exact replacement is not rejected (reproduced, medium)

Source: `workspace.rs:864-889`.

`old_string="", new_string="X", replace_all=true` changes `abc` into `XaXbXcX` and reports four replacements. That follows Rust string semantics, but does not match ordinary model expectations of replacing existing text. There is no schema or description explaining empty-string insertion. Reject it or provide a distinct explicit insertion operation; do not silently interpret it as a global edit. Retain the useful unique-match refusal for nonempty search text.

### 9. CRLF patches produce mixed line endings (reproduced, medium)

Source: `bro-apply-patch/src/apply.rs:145-163`.

Probe: update `b` to `B` in bytes `a\r\nb\r\n`; output bytes become `a\r\nB\n`. The port retains CR in untouched lines and writes new lines with LF. This can produce needless diffs or violate file conventions.

Comparison nuance: Codex's legacy/default normalization branch uses the same basic algorithm (`apply-patch/src/file_update.rs:49`). The reference now has an explicit `PreserveLineEndings` implementation and corresponding feature selection (`file_update.rs:69`; `core/src/tools/handlers/apply_patch.rs:70`). This is available reference functionality, not evidence our current behavior diverges from every Codex configuration.

### 10. Shell status and parameter disclosures are inconsistent (reproduced/source, medium/low)

Sources: `shell.rs:187,463-470,801,923,1026-1055`.

- `shell_list` claims live/still-running sessions but does not query child state. Probe: yield `sleep 0.1`, wait 0.2 in another command, then list; the exited process is listed. Unread terminal results should be retained, but labeled truthfully.
- `signal` is arbitrary text; unknown names silently select SIGTERM. A misspelled signal should reject before changing process state.
- Poll and kill schemas still say default output budget 10,000; implementation uses 2,000 and caps at 3,000. This is an incomplete disclosure after the recent budget repair.
- Unlimited `yield_time_ms`, including 0 as an infinite wait, can monopolize the global dispatch gate. A second shell control call cannot enter the gate until that poll yields or is cancelled. Codex bounds interactive exec waits (`unified_exec/mod.rs:209`) and uses per-process interaction locks (`process_manager.rs:825-839`), rather than making an unrelated terminal poll block all file/tool work.
- Extreme timeout/yield/grace values use unchecked `Instant + Duration`; schema accepts all u64 values. Codex checks deadline overflow for completion execution (`process_manager.rs:612-615`). Validate before spawning.
- Session capacity is enforced only after a new command has already spawned and reached its yield. The 33rd retained command can perform side effects before being killed and returning a capacity error (`shell.rs:740-770`). Reserve admission before spawn.

## Trust and process boundary audit

### 11. Git subprocesses bypass the shared child environment and cancellation contract (source, high)

Sources: `workspace.rs:637-650,757-786`; `git_commit.rs:58-77`; `shell.rs:545-569`.

All Git helpers construct `tokio::process::Command` directly and inherit process environment. They never apply shell's credential scrub or host non-secret `shell_env` overlay. This affects hooks, external diff/textconv programs, and filters launched by Git, not only Git itself. Their `.output()` calls are unbounded capture with no timeout, process group, or `kill_on_drop`; cancellation can leave those processes running. The recent safe path-selection commit repair does not repair this separate boundary.

Recommendation: centralize child construction/supervision and explicit env on ToolCx, or remove the redundant read wrappers in favor of shell. Codex carries `env` in its execution request (`core/src/tools/runtimes/unified_exec.rs:77-81`) and owns process termination (`core/src/unified_exec/process.rs:648-652`). Do not rely on a shell-only task-local helper for every subprocess in the harness.

### 12. Nested code-mode shell calls lose task-local scrub context (source, high confidence)

Sources: `bro-harness/src/agent_loop.rs:110-117`; `bro-tools/src/shell.rs:524-569`; `bro-code-mode/src/service.rs:298,695`; `bro-harness/src/capabilities.rs:66-112`.

Daemon-worker entry wraps the outer run future in Tokio task-local `SPAWN_SCRUB`. Code-mode starts a new Tokio control task and further nested-tool tasks. Tokio task-local values are scoped to the task/future and are not inherited by spawned tasks. `HostTools` does not rebind the scrub list; `apply_child_env` deliberately treats missing task-local context as a no-op. Thus the same `shell_run` builtin can receive different environment filtering flat vs nested. This was sent to root for cross-check against the broader loop audit; no real credential probe was performed.

Recommendation: explicit non-secret child-env/scrub capability captured at session construction and passed through ToolCx, shared by all process-spawning tools. This repairs both the nested-call inconsistency and the raw Git helpers.

### 13. Paths have full host trust; the denylist is not a containment layer (source/probe, contract)

Sources: `workspace.rs:299-340,378-390,505-517,868-890`; `bro-apply-patch/src/apply.rs:266-294`; `safety.rs:9-12,30-38,64 onward`.

Read/write/edit/shell roots are convenience resolution bases. Absolute paths and `..` escape are explicitly allowed, and writes follow symlinks. Probe: `file_write` through a symlink changed its target. This is deliberate full-host authority, not a newly discovered sandbox escape; sandbox-status should continue describing it honestly. FileWrite's short description saying "in the worktree" is narrower than the accepted path schema.

`@` read mentions have a special lexical external-instruction allowlist, while equivalent plain absolute paths are unrestricted. This does not enforce a trust boundary; it merely creates different affordances for two spellings. Root is auditing read tools separately.

The command denylist scans raw text. Harmless probe `printf '%s\n' 'pkill is forbidden'` is refused. Conversely inserting valid Git global options between `git` and `reset` evades the literal `git\s+reset` pattern; this was identified statically, not executed. Shell functions/scripts/stdin are also outside that source-text heuristic. It cannot support the documentation's "categorical refusals" claim.

Codex's standalone patch API also follows symlinks by default (`apply-patch/src/lib.rs:72-86`). Its integrated runtime makes that decision with filesystem sandbox context (`core/src/tools/runtimes/apply_patch.rs:180-196`). The useful comparison is explicit authority propagated into every tool, not blindly banning paths or pretending a root string is a sandbox.

## Other implementation limits worth preserving in the complete audit

- File edit/write use full-file reads and direct truncating writes. External concurrent writers can be overwritten between read and write; failed writes can leave partially changed content without an EditEvent. Codex's patch delta explicitly marks failed writes inexact. Avoid promising atomic edits or cross-agent compare-and-swap semantics until implemented.
- ApplyPatch preimages and EditEvents use `cx.root.join(raw_path)` while actual patch application normalizes `..`. A nonexistent intermediate component or a symlink followed by `..` can make recorded preimages refer to a different path or appear empty. The actual mutation may succeed at the lexically normalized path. Use a single resolved path plan for both execution and accounting.
- ApplyPatch uses `cx.root` directly, whereas other workspace tools use `effective_root` after managed-root removal. This gives the tools divergent working roots in that recovery case.
- Empty patch returns `Applied patch (0 changes)` successfully (reproduced). Codex rejects no-op envelopes in its apply layer (`apply-patch/src/lib.rs:482`). This is low priority, but should be an explicit no-op outcome instead of success-looking edit telemetry.
- `git_diff(include_untracked=true)` captures every untracked patch into one growing String and starts one Git process per path. Final model-output bounding does not bound that work or memory. Filenames are decoded lossily before being passed back to Git, so non-UTF-8 paths can be misaddressed.
- A shared tool-admission gate cannot serialize background child side effects after `shell_run` yields. Describe it as tool-call admission ordering, not exclusive ownership of all future process mutations.

## Recommended implementation order

1. Restore lifecycle truth: autonomous hard deadlines, bounded/input-aware stdin, cancellation completion ownership, and partial mutation deltas.
2. Replace prefix-only capture with head/tail capture; remove session-persistent output filtering from the default contract.
3. Make child environment and process supervision explicit and shared across flat/nested shell and Git paths.
4. Keep a small default editing/process surface. Defer redundant Git views and advanced helpers rather than adding prompt instructions to compensate for inconsistent tools.
5. Add behavioral regressions from the concrete fixtures above, then compare task outcomes under the smaller surface. Preserve useful reference affordances such as bounded process continuation, exact edits, patch verification, truthful omission markers and explicit process handles.
