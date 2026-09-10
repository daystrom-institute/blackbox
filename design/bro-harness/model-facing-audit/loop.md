---
title: "Loop, context, and session contract audit"
kind: design
lifecycle: partial
corpus: blackbox-design
topic: [bro-harness, tools, audit]
brief: "Source-grounded contract findings at c384fb8a, compared with Codex 242c5ce0; evidence and repair contracts, not a runtime repair claim."
---

[Audit overview](../model-facing-tools-comprehensive-audit.md). Source anchors refer to the pinned snapshots named below.

# Harness loop, context, and conversation-integrity audit

Audit baseline: Blackbox `c384fb8a1cc022f7a8bc00f94ad0462373ca1a8e`, checkout `/Users/invidious/repos/transcript-search`. Reference: local Codex `242c5ce01c` in `/Users/invidious/repos/codex`. Source paths below are relative to the respective checkout. This is a read-only source audit: this agent ran no builds, network/provider probes, service changes, or repository edits. Counterexamples are derived from reachable branches unless an independently produced artifact is explicitly identified. Existing tests were inspected, not rerun. Previously fixed output bounds, read/write reordering, nested admission gates, and immediate same-process context reinjection are not counted as new findings.

## Actual execution paths

| Path | Admission, state and termination |
| --- | --- |
| One shot | `agent_loop.rs:179`: build one Session, run `user_turn`, persist native snapshot and side state, exit. The cancellation receiver deliberately never fires. |
| Persistent stdin session | `agent_loop.rs:241,373`: reader task parses NDJSON into an unbounded queue; session loop polls the model/tool turn alongside input; interrupt toggles a watch channel; ordinary user input enters a separate mid-turn queue; other controls are acknowledged immediately and applied at the boundary. Persist after each turn. |
| Daemon child / until-idle | `agent_loop.rs:291,492,545,567`: first-input wait for stdin child mode, then nonblocking queue draining; a controlled turn duplicates the persistent loop's control handling; persistence again occurs at turn boundaries. |
| Ordinary model step | `agent_loop.rs:1220`: validate restored tool schemas, estimate appended input, optionally compact, compose context, normalize native conversation, run provider, record assistant, validate ambiguous batch, dispatch tools, append output/diagnostic/instruction riders, emit results, then drain steers. |
| Flat tools | Registry admission now shares a session RwLock with HostTools. Adjacent read batches overlap; writes are barriers. Results pass loop-level hooks, scoped-doc detection and diagnostics. |
| Nested code-mode tools | `code_mode.rs:123` to `capabilities.rs:65`: the service's own tasks invoke HostTools, which applies defaults and runs tools under the shared gate. The outer exec/wait tools bypass that gate. Nested results become JS values/errors. They do not pass the flat loop's instruction/rider/trace lifecycle individually. |
| Resume | `session.rs:137`, `agent_loop.rs:766`: read snapshot, reconstruct native transport, restore selected side cells, rebuild current tools/docs/env. Scoped-doc receipts come from the whole event log; model history comes from the latest snapshot. Runtime cells, KV, shell-session handles, and partial turns are not reconstructed. |
| Compaction | Anthropic/Chat/API-key Responses replace a rendered prefix with a summary plus a retained tail. ChatGPT Responses asks the remote compact endpoint to replace history. Same-process auto/overflow compaction now reinjects startup context. Manual compaction clears the baseline for the next user turn. |

## Priority findings

### L1. Model-proposed malformed arguments become different executable calls (high, proven in source)

Both Responses and Chat replace invalid JSON with `{}`: `transport/responses_common.rs:531-543`, `transport/openai_chat.rs:532-544`. The original malformed argument string remains in the native assistant message, while the dispatched ToolCall and visible assistant event carry the fabricated empty object. Defaults can then fill the object in Registry/HostTools.

Counterexample: a valid tool name/id with `arguments: "{\"path\":"` becomes a call with `{}`. For a tool with optional inputs or configured default/pinned arguments this can succeed, including a mutation. Even when the tool rejects missing fields, the model receives an inaccurate missing-argument error instead of the real parse error, and audit history diverges from native replay.

Codex: `codex-rs/core/src/tools/handlers/mod.rs:85-90` returns a model-facing parse failure; it does not fabricate arguments. Our Anthropic path already fails parsing explicitly in `transport/anthropic.rs:461`.

Coverage: Responses tests cover reasoning, context errors and normalization; Chat tests cover request construction and normalization, not malformed streamed tool arguments. Add terminal and incomplete streams with malformed, scalar, null and duplicate-id arguments; assert zero side effects and preservation of the original failure observation.

Recommendation: one strict provider-output decoding contract, preserving raw evidence and producing a paired tool error or rejected batch before admission. Remove `unwrap_or({})` recovery for provider-proposed arguments.

### L2. Chat treats a truncated stream as a completed response and can dispatch its tools (high, proven)

`transport/openai_chat.rs:399` consumes until EOF without requiring a terminal chunk; `[DONE]` is ignored. `:559-567` maps absent finish reason to Done and forces ToolCalls if any call accumulated. It also lacks the stream-idle timeout used by Responses and Anthropic; a request-wide timeout is a different limit.

Counterexample: connection ends cleanly after a tool-name/id delta and an arguments delta, without finish_reason. The tool runs. A text-only cut is reported as a successful final answer. A malformed cut compounds L1.

Codex's reference transport is Responses, not Chat, so there is no claim of a drop-in Chat parser. Its integrity invariant is useful: `codex-rs/codex-api/src/sse/responses.rs:598` rejects closure before completion, with `error_when_missing_completed` at `:953`. Our Responses HTTP path already implements a corresponding terminal check (`openai_responses.rs:226`).

Coverage: there are no network stream parser tests in the current Chat module; tests at `:773-1016` are body/normalizer/profile helpers. Add local-server fixture cases for EOF, idle stall, malformed event, explicit provider error, length termination and valid tools.

Recommendation: use an explicit accumulator state machine; commit native history and admit tools only after a valid terminal outcome. Apply the shared idle limit. Do not infer success from socket closure.

### L3. User interruption does not own the lifetime of code-mode cells (high, proven; intended cross-turn policy needs a decision)

`ExecTool::call` starts a cell and awaits a oneshot response (`code_mode.rs:321-350`). `bro-code-mode/src/service.rs:274-306` allocates an independent cancellation token and spawns cell control/tool tasks. Dropping the outer exec/wait future on the agent-loop watch cancellation does not terminate these service-owned cells. Session retains the tool Arcs, and `code_mode_tools` discards the explicit lifecycle handle (`code_mode.rs:454-460`). Shutdown exists but the harness session does not invoke it on interrupt. Ordinary live cells similarly survive a successful model stop in persistent mode.

Counterexample: a cell awaits a slow nested mutation and the operator interrupts. The outer tool result says interrupted; the cell can continue holding the shared gate or mutate later, while the redirect turn starts. The gate serializes live invocation futures; detached work can outlive its guard (see the builtin cancellation findings). It constrains invocation admission but does not make the interruption truthful. If exec is interrupted before returning its initial result, the model may not even have the cell ID.

Codex: `core/src/tasks/mod.rs:913-924` can terminate active cells on operator interrupt, explicitly feature-gated by CodeModeInterrupt; `tools/code_mode/mod.rs:146` owns active IDs; `session/handlers.rs:429` explicitly shuts down code-mode on session shutdown. This is an explicit policy/lifecycle, not something supplied automatically by vendoring V8.

Coverage: current tests demonstrate explicit wait(terminate), runtime shutdown, flat interruption and nested exclusion separately. None drives session interruption while an exec cell has a live mutator, or checks stop/resume after a yielded cell.

Recommendation: Session should own a lifecycle service with active-cell IDs and cancellation outcome records. Specify the policy: user interrupt cancels and joins active cell work; successful stop either deliberately retains background work with reported handles or terminates it. Keep explicit wait/terminate. Do not equate dropping a Rust future with stopping external work; completed effects must remain completed/uncertain, not be relabeled unexecuted.

### L4. Scoped instructions are late, missing across nested calls, and confused with historical delivery (high, multiple proven cases)

Detailed scope/repair contract is below. This is larger than adding a rider in HostTools.

### L5. Explicit resume can silently become a fresh session with the old identity (high, proven)

`session.rs:147-184` catches every file-read error, tries a legacy location, and on another error returns `restored: None` while keeping the requested ID. This includes permission/I/O errors, not merely absence. Subsequent persistence can write a fresh conversation at that ID, while the existing event log still contains old turns. There is no snapshot schema version and missing transport/native snapshot fields are defaulted; provider restore helpers accept invalid shape by doing nothing.

Counterexample: first-turn process dies before the first snapshot, leaving a durable event log and real tool effects. Resume names the same ID; model starts with no prior actions. A moved BRO_HOME or unreadable snapshot can have the same appearance.

Codex reconstructs history from its rollout (`core/src/session/rollout_reconstruction.rs`); recorder paths expose read errors, and the rollout writer has explicit writer-lock support (`codex-rs/rollout/src/recorder.rs:861`). The relevant principle is explicit history ownership and recovery, not that every Codex persistence choice must be copied.

Coverage: `session.rs` tests cover selected-field round trips and legacy fallback. They do not test explicit missing/unreadable resume, malformed snapshot schema, event-log-ahead-of-snapshot recovery, or concurrent writers of one ID.

Recommendation: explicit resume fails if the requested history cannot be restored, unless a separately requested recovery mode reconstructs an authoritative history. Validate snapshot version/type; distinguish NotFound from other I/O errors. Preserve the damaged/missing state for inspection. Add a session writer/admission lock if concurrent same-ID invocation is allowed outside the daemon.

### L6. Manual-compaction null baseline does not survive resume (high, proven)

`compact_manual` writes `reference_context_item = None` (`agent_loop.rs:1182`), but `reference_context_item_for_restore` (`:2184`) turns any restored None into a present environment marker. It cannot distinguish a legacy missing field from an explicit persisted null. That suppresses the intended full instruction reinjection after resume. The existing `compaction_clear_persists_null_and_next_turn_reinjects_full_context` test resumes the next turn on the same Session, not through serialize/rebuild.

Counterexample: manual compact, stop process, resume. The summary may have shortened the AGENTS instructions; startup discovers current text but the seeded marker prevents it being sent.

Codex retains the null baseline meaning through rollout reconstruction; `core/src/session/tests.rs:4147` explicitly covers restoring a cleared reference context after compaction.

Recommendation: preserve three states (legacy missing, explicit cleared, established baseline), or reconstruct from authoritative compaction records. Test the actual persist/open/build boundary, not only the live helper.

### L7. Steering is not handled at every model boundary; `/compact` can be dropped (medium/high, proven)

`drain_mid_turn_user_inputs` (`agent_loop.rs:2013`) pops `/compact`, logs 'deferring', and continues without retaining or executing it. Ordinary steers are drained after a tool batch or rejected batch, but not on the no-tools/end_turn=false continuation (`:1573`). Redirects are push_back in `queue_redirect_from_control` (`:359`), so a redirect follows earlier queued steers despite the scoped AGENTS contract saying it is front-queued.

Counterexamples: send `/compact` during a tool; it vanishes. Send a correction during a provider follow-up response without tools; another model call can proceed without it. Queue old steer A, then interrupt-with-redirect B; A becomes the next turn first.

Coverage: `stdin_steer_during_tool_turn_injects_before_next_model_call` covers only the successful tool path; `slash_compact_runs_compaction_not_a_turn` covers only idle input. No non-tool continuation, active compact, or steer-plus-redirect ordering test.

Recommendation: one boundary input-drain primitive used before every model request, with typed pending commands. Retain compact until safe execution. Specify redirect ordering and test it. Consolidate the duplicated persistent and until-idle control loops so fixes apply to both.

### L8. Interrupt acknowledgement can precede an uninterruptible compaction/diagnostic phase (medium/high, proven delay; duration depends on services)

Cancellation is selected around provider requests and tool dispatch, but not around proactive/manual/overflow compaction (`agent_loop.rs:1280,1380`) or `append_edit_diagnostics` (`:1728,2102`). The latter can initialize/synchronize Rust LSP and await diagnostics for every edited file (`diagnostics/engine.rs:82-113`). Control handling acknowledges interrupt immediately. Both outer loops keep selecting an already-closed input channel instead of disabling that branch (`agent_loop.rs:436,595`), creating a hot loop during delayed cancellation.

Counterexample: interrupt while compaction's HTTP request or an LSP diagnostic wait is pending. UI sees success acknowledgement; underlying phase keeps running until its own completion/timeout. This does not imply those helpers are infinite; the missing cancellation boundary is itself observable.

Coverage: interruption test blocks `run_turn`; it never blocks compaction or diagnostics. Existing diagnostics tests focus on baseline correctness.

Recommendation: propagate cancellation through every expensive phase; retain already-completed edits/results and flush their outcomes. Distinguish 'interrupt requested' from completed termination. Disable EOF receive branches after closure. Keep diagnostics useful but make expensive diagnostics bounded and cancellable; do not turn every edit into an opaque compulsory LSP wait.

### L9. Length limits and step exhaustion still look like successful task completion (high for orchestration, proven)

`has_tool_work`/stop handling accepts any no-tool response containing text, including Length (`agent_loop.rs:1537-1572`). `turn_end_diagnostics` considers outstanding async work and empty output, but neither Length nor max_turns (`:2044-2089`). `emit.rs:361-400` deliberately emits subtype success even for suspicious ends. Responses maps response.incomplete to Length (`responses_common.rs:415-436`) and then may force ToolCalls if output contains calls (`:551`).

Counterexamples: output cap cuts 'I have updated the parser and still need to...' and session reports success. Step cap is reached after a tool result, and the earlier narration becomes the result. No automatic judgement about goal completion is required to identify these mechanically incomplete stops.

Codex `codex-api/src/sse/responses.rs:472` returns an explicit error for response.incomplete, retaining the reason; task terminal handling distinguishes abort and error in `core/src/tasks/mod.rs:797-810`.

Independent runtime evidence supplied by root and inspected read-only: the synthetic runtime transcript summarized in the parent audit ends with `num_turns: 12`, `subtype: success`, and prior narration saying it is going to add the guard/assertion. Root confirms the requested verification had not run. This corroborates false-success-at-cap; it does not establish comparative efficacy of code-mode settings or represent the later controlled cohort.

Coverage: `turn_end_diagnostics_empty_max_turns_is_not_empty_output_stop` explicitly excludes the cap from that diagnostic; it does not verify a distinct incomplete terminal outcome. Empty-output retry tests cover exactly one different case.

Recommendation: separate transport completion, harness exhaustion, interruption, and model final response in terminal events. Preserve reason and terminal-step text. Decide bounded continuation policy for Length; never silently relabel it as successful completion. Keep the one-time empty-output recovery as a narrow fallback, not a generic completion detector.

### L10. Structured-output interception is neither schema validation nor batch-safe (medium/high, proven)

`agent_loop.rs:1580-1628` takes the first final_result args unchanged. It pads only siblings whose name is not final_result. Two final_result calls with distinct IDs leave the second unmatched. Ordinary siblings are skipped even if the model placed a required write before final_result. The input schema is advertised but no local conformance validation occurs. Malformed provider arguments from L1 can therefore become a successful `{}` structured result.

Counterexample: final_result id A and final_result id B in one accepted Responses/Chat batch. Only A gets a result; later normalization fabricates B's missing result. A schema requiring `ok: boolean` accepts `{ok: "not_boolean"}` if the provider sends it.

Coverage: `final_result_tool_result_is_pushed_to_transport_before_return` covers one well-formed call. Recent ambiguous-native-batch tests protect a narrower Anthropic transport case, not all final-result combinations.

Recommendation: admit exactly one terminal call with schema-valid args and no siblings, or explicitly reject the whole batch with paired errors. Do not silently skip earlier requested actions. Prefer native provider structured output only where its contract can be validated consistently.

### L11. Cleared/replaced context can remain authoritative in conversation history (medium/high, proven absence of a clear signal)

`context/dispatch.rs:101-127` can clear current context or restore it without scope. `emit_dispatch_context_changes_if_needed` (`agent_loop.rs:1952`) only emits Some(scope/pins); None emits no revocation. Previously delivered user-lane scope/pins remain in native history. Similarly startup rebuilds current project docs at `agent_loop.rs:750` but an established reference baseline gates their delivery; no instruction-content digest is compared when resuming in the same or a different cwd.

Counterexample: previous scope task A or pin 'work only on module A'; resume with context explicitly cleared or pin removed. Runtime no longer has it, model still sees it as its last scoped instruction. Resume in cwd B sends an environment delta but can retain cwd A's AGENTS content with no fresh B instructions.

Coverage: dispatch tests assert Rust state clears and effective directives disappear; baseline tests assert identical context is not repeated. They do not inspect provider-visible removal or changed AGENTS content after actual resume.

Recommendation: a versioned context snapshot/delta model must represent removal as well as addition, including document identities/hashes. Model-visible revocation or replacement should be recorded. Do not silently infer fresh authority from old user text.

### L12. Resume does not disclose loss of code-mode state and async handles (medium, proven state loss; intended lifetime should be explicit)

`agent_loop.rs:850,970,2158` rebuilds shell-session storage and CodeModeService from scratch; side_state contains no code-mode KV or active-cell state. Yet code-mode describes store/load as lasting for the 'same session' (`bro-code-mode/src/description.rs:31`), and resume keeps the session ID/native messages containing those keys and handles. Runtime state being process-local can be a good simplification, but the model gets no reset notice.

Counterexample: turn one stores helper source in a key, process resumes turn two, load returns undefined. A prior yielded cell ID or shell session ID cannot be waited. Tool activation continuity is carefully restored while runtime continuity is not described.

Coverage: `store_load_persists_across_cells_in_a_session` uses one live tool pair. No process-resume test covers the advertised session scope.

Recommendation: prefer explicit process-local lifetime and an authoritative resume reset event over attempting to serialize running processes. Persist serializable KV only if its identity, versioning and invalidation contract is deliberately supported. Never restore live handles as if runnable.

### L13. Model control requests can be acknowledged while ignored or only partially applied (medium, proven)

`apply_control` (`agent_loop.rs:1147`) only changes model, threshold and context-window telemetry; other controls explicitly no-op. Every control receives success. Model change leaves `base_opts.base_instructions` and effort untouched even though initial build chooses model-specific base instructions. This is a control-plane contract mismatch, not evidence of a particular model failing.

Coverage: `idle_set_model_control_mutates_model` checks only the model string. No unsupported-control rejection or model-dependent prompt refresh test.

Recommendation: advertise supported controls; reject unknown/unsupported changes. A model change should recompute all model-dependent options atomically or reject incompatible changes. Acknowledgement and actual application are distinct events for deferred controls.

## Scoped instruction problem and realistic repair contract

The current mechanism conflates four different facts: a file exists on disk, it was historically delivered, it remains in current model context, and the pending edit was authored after the model saw it.

1. **Late flat delivery:** the loop dispatches the whole batch before inspecting touched paths (`agent_loop.rs:1640-1720`). Even `[file_read(child), file_edit(child)]` gets child AGENTS only after the edit. Read-before-write ordering alone does not solve this.
2. **Nested absence:** HostTools never invokes ScopedProjectDocs. Sequential JS `await file_read(); await file_edit()` is still one model-authored cell; returning a document to JS is not another model reasoning opportunity.
3. **Path gaps:** `project_doc.rs:412` recognizes only file_read/smart_read/file_write/file_edit and apply_patch. Shell and structural binding mutations are outside it. Lexical paths are checked, not canonical target ancestry (`:465`), so a symlink alias can miss the target directory's instructions. Failed reads do not trigger instructions; first creation does so after mutation.
4. **Compaction loss:** ScopedProjectDocs tracks path-only delivered state and does not reset/reload on compaction. Dynamic rider text may leave retained history or be shortened to 2,000 characters in summarizer input. Startup-context reinjection does not include those riders.
5. **Resume false evidence:** `from_startup_and_event_log` (`:315`) scans the entire event log, not the snapshot's retained context or checkpoint. A receipt after the last snapshot, or before compaction, suppresses delivery even though current model context lacks it.
6. **Untrusted receipt parsing:** `delivered_paths_from_event_log` (`:532`) parses the marker and delivered-path list from ordinary tool-result text. Reading a fixture/document containing a rider-shaped block can manufacture a receipt. Receipt metadata must not be inferred from arbitrary rendered source text.
7. **No content freshness:** path-only dedupe means edits to AGENTS within a live session or across resume are not noticed. Startup discovered paths are marked delivered before the new text is actually sent.
8. **Different override semantics:** Blackbox loads both global AGENTS and override (`project_doc.rs:264`), and defaults to only project AGENTS. Codex prefers the first nonempty global override and picks one candidate per directory (`codex-home/src/instructions/mod.rs:26`, `core/src/agents_md.rs:270`). This is a proven divergence from the 'Codex-equivalent' description, not automatically a reason to discard Blackbox's user-authorized include policy. The project tests intentionally encode concatenation.

A practical repair should have the following contract:

- One session-owned instruction ledger shared by flat tools and nested dispatch, keyed by canonical doc identity, content hash, scope and conversation generation. Store typed evidence that the content was delivered in the current history; the event log is an audit source, not the current-context set.
- Tool-owned affected-path extraction before mutation for structured file/edit/patch/binding tools. Account for moves, destination creation and canonical ancestry. Do not guess shell effects from command substrings.
- If a mutation discovers instructions the model has not seen, return a structured `instructions_required` outcome and authoritative instruction content without executing that mutation. The model must get another turn to author/confirm the edit. Do not deliver a rider and auto-continue an already-authored mutation, including inside a running JS cell. An awaitable JS result alone is insufficient.
- Successful reads may reveal instructions, but admission must still enforce the fresh instruction generation for later writes. A batch must not run later writes past a newly discovered instruction boundary. Preserve completed results and give unstarted siblings precise blocked outcomes.
- Compaction atomically installs new history and its active instruction ledger. Preserve required scoped documents exactly or explicitly reinject them before affected work resumes. Distinguish retained from merely historical delivery. Do the same through serialize/open/resume, including the explicit-null baseline case.
- Deliver scope/pin/document changes and removals as typed context updates; record their text/hash/provenance in durable events. Do not parse authority receipts out of source-file text.
- Shell remains an explicitly documented escape hatch: the model must inspect applicable instructions before shell edits. A claim of mechanically universal enforcement would require filesystem-level instrumentation and is beyond this small repair. Keep that limitation honest.
- Decide override/include rules explicitly and test them. Avoid turning every incidental `@file.md` mention in prose/code blocks into an implicit include unless that is the declared syntax. Respect the project's explicit no-truncation instruction policy; use typed diagnostics for oversized instruction context instead of silently cutting it.

Required validation matrix: flat read/edit in one batch; first file creation; apply_patch move; nested sequential calls; nested Promise.all; structural binding edit; source symlink; file whose content contains fake rider markup; AGENTS changes at same path; failed doc read; compaction with rider outside retained tail; resume before/after compaction; event log ahead of snapshot; changed cwd; explicit system suppression. Test both whether the mutation occurred and which exact instructions reached the model before it, not only whether a string was appended eventually.

## Retain, simplify, remove

**Retain:** transport-native history buffers; complete assistant/tool-result events; explicit tool-pair normalization as a final integrity guard; bounded transient HTTP retries; WS fallback only for transport faults; strict Anthropic tool JSON handling; explicit output limits and paging; shared read/write admission; narrow one-shot empty-output recovery; per-turn snapshot persistence; deferred schema activation restoration; explicit code-mode wait/terminate; LSP diagnostics when bounded and cancel-aware.

**Simplify:** consolidate duplicated session control loops; one lifecycle seam for nested/flat calls; typed terminal outcomes and typed instruction/context receipts; one provider-output validation boundary; explicit runtime reset semantics on resume; less automatic work attached to a file edit.

**Remove:** silent malformed-JSON-to-empty-object recovery; silent resume-to-fresh fallback; successful acknowledgement of unsupported controls; 'deferred' commands that are actually discarded; historical-text parsing as proof of current instruction delivery; claims that successful protocol termination implies task completion.

**Keep opt-in/deferred:** preference nudges and structural smart tooling. This audit provides concrete correctness reasons to simplify contracts, but does not establish that every structural tool is less effective than shell/patch. Measuring model quality requires matched tasks, equal tool budgets and observable completion criteria, not a count of advanced features or unit tests.

## Coverage limits and useful reference distinctions

This audit inspected all loop entry modes, native snapshot restore, code-mode service lifetime, context composition, scoped instruction discovery/receipt handling, compaction paths, controls, interruption, stop handling and provider parsing. It did not run providers, induce process crashes, measure latency, or validate each individual tool's filesystem cancellation semantics. Those are integration tests needed before claiming runtime reproduction.

Codex has a single current Responses contract and several explicitly gated behaviors; our multi-provider adapters need equivalent invariants, not identical code. Codex also allows process-local state and optional interruption policy. Its strongest transferable ideas here are explicit history reconstruction, typed error/terminal states, one lifecycle around direct and nested calls, and deliberate insertion of initial context into replacement history (`core/src/compact.rs:65`, `compact_remote_v2.rs:325`). Our recent helper appends restored context after the retained tail rather than performing Codex's before-last-user/summary insertion; semantic presence is improved, but ordering equivalence is not established by the mock tests. Treat that as a compatibility hypothesis to probe, not another proven data-loss defect.
