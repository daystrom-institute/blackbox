---
title: "Agent loop context and compaction audit against current Codex"
kind: design
lifecycle: partial
corpus: blackbox-design
topic: [bro-harness, agent-loop, context-management, compaction, audit]
brief: "Source-grounded review of initial context, ambient updates, compaction and context efficiency at Blackbox 7a20a8f8 versus Codex 242c5ce0. Identifies repair contracts, regression scenarios and reference mechanisms worth adopting."
---

# Agent loop context and compaction audit

The broad architecture is sound: separate stable instructions from conversation,
deliver changed context rather than repeating everything, preserve native tool
history, and invalidate context delivery after compaction. At the reviewed baseline, the implementation
did not apply those contracts consistently across startup, resume, manual
compaction, model changes and the three transports. The highest priority is
history integrity and reliable request accounting, followed by context placement
and modern Responses features. The [repair follow-up](#repair-follow-up) records
subsequent implementation and validation.

This review follows the earlier [loop audit](model-facing-audit/loop.md) and
[repair plan](model-facing-tools-repair-plan.md). It checks the current code after
those repairs. Findings from the older audit are not assumed to remain open.

## Evidence and scope

Reviewed on 2026-09-11:

| Source | Revision | Role |
| --- | --- | --- |
| `transcript-search` | `7a20a8f8a83e7631eca4a72a8077a09d3c7ff740` | Current implementation |
| Sibling `../codex` | `242c5ce01cd3388f7d23a87b68615b2042a04bfc` | Requested reference implementation, dated 2026-09-10 |

All numbered findings are confirmed control-flow or request-shape observations
from these snapshots. Backend-dependent consequences are identified explicitly.
During the original audit, existing tests were inspected, not rerun. No builds,
live model requests, service changes or runtime repairs were performed. Reproduction cases below are proposed
regression tests, not claims of executed probes. Compaction/efficiency paths were
reviewed separately; initial-context and explicit-clear findings received an
independent source cross-check.

Source anchors name these pinned snapshots. `bro/` below abbreviates
`crates/bro-harness/src/`; `codex/` abbreviates the sibling checkout's
`codex-rs/`. These are source references, not installed-binary claims. In
particular, model catalog values establish reference divergence, not a live
provider capacity measurement.

## What the model actually receives

| Boundary | Current bro-harness behavior | Current Codex behavior |
| --- | --- | --- |
| Stable instructions | Capability-derived base; explicit text, persona and standing directives; pinned-tool descriptions. Vibe also puts environment/scope/pins here. | Base instructions plus typed developer/context sections; request representation can depend on model metadata. |
| First user turn | CodexShaped emits scope/pins/environment, then task. Discovered filesystem instructions arrive afterward through the separate instruction ledger. | Full typed context is assembled before recording the real user input. |
| Later user turn | Environment and dispatch deltas, then raw task; pending changed filesystem instructions are appended at the model boundary. | Context state and durable baseline produce typed changes; removed sections can emit explicit revocation. |
| Later model step | Instruction graphs refresh every request; environment/dispatch user-context preparation is tied primarily to user-turn entry. Tool observations, hooks and the transport manifest have separate delivery paths. | A captured step supplies tools/settings and world state; changes are recorded before sampling. |
| Auto/overflow compaction | Rewrite history, invalidate context and instruction receipts, append fresh current context before inference. | Install replacement history with deliberately positioned initial context and a matching baseline; recompute usage. |
| Manual compaction | Rewrite, invalidate receipts, reestablish context on the next ordinary turn. Occupancy counters remain stale. | Replacement and token accounting are updated together. |
| Process resume | Restore native history and durable side state, reread instructions, announce loss of process-local cells/shells, reemit initial host context. Occupancy starts at zero. | Reconstruct rollout, restore usage and context baselines, emit changes against restored state. |

Primary anchors: `bro/agent_loop.rs:813`, `:1403`, `:1443`, `:1487`, `:2108`,
`:2283`, `:2530`; `codex/core/src/session/turn.rs:283`, `:365`, `:502`;
`codex/core/src/session/mod.rs:3548`, `:4064`, `:4426`.

The typed scoped-instruction ledger is a substantial improvement. Its admission
receipts and refresh semantics should be preserved. The issue is integrating
that ledger with the context placement and budgeting contracts around it.

## Findings in repair order

P1 denotes potential history loss or a direct instruction-placement contract
regression. P2 denotes another correctness defect, preventable recovery failure,
or reference compatibility gap. These are repair priorities, not measured
frequencies of production incidents.

### F1. P1: Partial Responses summaries can replace complete source history

**Path:** API-key Responses compaction. `bro/transport/openai_responses.rs:309`
collects text deltas until EOF. It ignores malformed JSON, `response.failed` and
`error`, and does not require `response.completed`. Nonempty extracted text is
accepted at `:336`; `:539` through `:549` then replace the original prefix with
that summary and the retained tail.

**Counterexample:** the stream sends half a summary, then ends cleanly or sends
an API failure event. The parser returns success and the next checkpoint no
longer contains the complete original prefix. A network read error already
propagates; clean EOF and ignored protocol failures are the uncovered cases.

**Reference:** `codex/core/src/compact.rs:765` drains to an explicit completion
and errors on EOF before `response.completed`. Success of the summary operation
is established before replacement history is installed.

**Repair/test:** share a terminal-aware stream decoder and stage replacement
history until validation succeeds. Mock delta-then-EOF, delta-then-failure,
malformed event, incomplete terminal and successful completion. On every failed
case require the original snapshot to remain intact. Tolerating a missing XML
closing tag is a separate decision from accepting a failed provider operation;
the existing summary-extraction test at `bro/transport/mod.rs:685` does not prove
transport completion.

### F2. P2: OAuth compaction accepts nonempty output without a valid summary

**Path:** `bro/transport/openai_responses.rs:415` only requires a nonempty
`output` array. Missing `compaction_summary` or missing `encrypted_content`
becomes an empty string, but `self.state.input = output` and `Ok(Some(summary))`
still execute at `:428` through `:437`.

**Counterexample:** a successful HTTP response containing only an assistant
message is classified as successful compaction and replaces all input. This is
a fail-open response-validation branch; it does not establish that the current
backend emits that malformed response.

**Reference:** `codex/core/src/compact_remote_v2.rs:427` requires a completed
stream and exactly one native compaction item. Other output items may occur;
the important check is the compaction item's cardinality and validity.

**Repair/test:** validate the selected compaction protocol's full response
contract before mutation. Test missing, empty, malformed and duplicate summary
items, preserving the old history on rejection. Do not blindly apply the V2
item name to the legacy unary endpoint. The existing ignored live happy-path
probe and request-shape test do not exercise this failure boundary.

### F3. P1: Actual filesystem instructions bypass provider placement and follow the task

**Path:** production startup discovers documents but sets `user_instructions =
None` at `bro/agent_loop.rs:825`. The real turn appends the task at `:1445`,
composes the system prompt, then unconditionally pushes the ledger's instruction
batch as user text at `:2308`.

This produces two distinct divergences:

- CodexShaped delivers scope/pins/environment, task, then AGENTS documents.
  Its intended ordering is instructions/context before the task.
- VibeShaped also receives AGENTS as user text after the task, although its
  explicit strategy puts memory in the leading system block to keep the user
  lane focused on the task. `system_sections()` reads the unused field at
  `:2116`, so it cannot implement that contract for discovered documents.

The instructions are present. This is a role/ordering regression, not missing
instruction content. Exact model-quality consequences need a wire-level trial;
the production routing mismatch is directly established. Chat construction at
`bro/transport/openai_chat.rs:182` and the normalizers do not relocate the text.

**Reference:** `codex/core/src/session/turn.rs:283` builds context before user
input is recorded; `core/src/session/mod.rs:4219` through `:4299` explicitly
orders developer and contextual-user fragments. Bro's own provider-specific
contract is in `bro/context/dispatch.rs:20` through `:34`.

**Coverage trap:** tests at `bro/agent_loop.rs:5465` and `:5513` manually populate
`user_instructions` and call a helper. They do not exercise startup discovery
and `deliver_instruction_context`, so they can pass while the real placement
is wrong.

**Repair/test:** route typed instruction delivery through the composition
strategy while retaining exact-version receipts and immutable request
generations. Build sessions from temporary AGENTS files and inspect actual
mocked request roles/order for each transport, including resume, changed docs
and compaction. Keep the new task after initial user-context fragments.

### F4. P2: Explicit dispatch clear fails to revoke historical scope and pins

**Path:** `bro/context/dispatch.rs:111` handles an explicitly empty flag or `{}`
with `Self::default()`, dropping both current state and last-emitted baselines.
The transport snapshot is restored unchanged at `bro/agent_loop.rs:915`.
Removal notices in `:2156` and `:2189` require those old baselines, so none is
emitted after explicit clear.

**Counterexample:** resume a CodexShaped session containing scope and pins with
`--dispatch-context '{}'`. The model still sees the old fragments, with no
revocation. Runtime dispatch state is cleared; this does not mechanically
rebind a task. Persona and standing directives are correctly removed from the
recomposed system block. Vibe's normal system-carried scope/pins have a different
replacement behavior.

**Reference:** typed removal in
`codex/core/src/context/world_state/agents_md.rs:47` through `:82` preserves
knowledge of the old section long enough to tell the model it no longer applies.
The general snapshot/diff contract lives in `context/world_state/mod.rs`.

**Repair/test:** retain emitted baselines until revocations have entered history.
Serialize real scope/pins, reconstruct with `{}` and `""`, and assert revocations
precede the new task. Cover absent flag, identical provided context and fresh
clear as distinct cases. Replace the defective expectation encoded by
`clear_wipes_context_and_baselines` at `bro/context/dispatch.rs:404`.

### F5. P2: Next-request projection omits retained assistant output and late additions

**Path:** projection at `bro/agent_loop.rs:1403` adds last observed input usage,
pending input estimates and the pending task. After inference `:1549` stores
only input usage, then `:1561` clears pending additions. Meanwhile Responses
has appended assistant output and replayable reasoning to history at
`bro/transport/responses_common.rs:677`.

For example, 195k input plus 20k retained output and a small tool result crosses
a 204k threshold, while the loop still projects roughly 195k. This is a
threshold example, not a measured request or a claim of immediate overflow.

The check also precedes initial/current instruction delivery and system
composition. Newly discovered instructions, changed leading tool schemas and
changed system text can increase the imminent request after its projection has
already passed. Adding their size to a later counter cannot protect the current
request. Cold-start instructions especially have no prior input measurement.

**Reference:** `codex/core/src/context_manager/history.rs:678` starts with last
usage's total tokens, adds subsequent local items, and accounts for older
reasoning when necessary. Its estimator is still an approximation, not an exact
tokenizer guarantee.

**Repair/test:** separate input-only telemetry from an internal projection of
the entire next request. Assemble the immutable request view before deciding
whether it fits, including retained output, fresh instructions and schema
changes. Test a long assistant response, large new instruction batch and large
tool activation independently. If authoritative instructions alone cannot fit,
diagnose that explicitly; repeatedly summarizing conversation or silently
truncating required rules is not a sufficient remedy.

### F6. P2: Occupancy is lost on resume and stale after manual compaction

These are opposite failures of the same lifecycle contract.

- Resume restores conversation but sets `last_prompt_tokens` and
  `pending_input_estimate` to zero at `bro/agent_loop.rs:1247`. The persisted
  side state at `:2495` has no occupancy checkpoint. A short continuation of
  a nearly full session bypasses preventive compaction on its first request.
- Manual compaction at `:1282` invalidates context receipts but changes neither
  counter. If the old estimate exceeded threshold, the next short user turn
  can immediately trigger another compaction before a normal response measures
  the smaller history.

**Reference:** Codex restores usage at
`codex/core/src/session/mod.rs:1512`, and recomputes after replacement at
`core/src/compact.rs:403` and `core/src/compact_remote_v2.rs:359`. The direct
resume regression is `core/tests/suite/compact.rs:2189`.

**Repair/test:** checkpoint usage with its history position or conservatively
recompute from restored/replaced history. Keep unknown measured telemetry
distinct from an internal estimate. Tests should construct a real resumed
session above threshold, and a manually compacted session below threshold,
asserting the exact ordering and count of subsequent compaction requests.

### F7. P2: The GPT-5 family capacity assumption is stale

**Path:** `bro/compaction.rs:224` gives all `gpt-5*` models a 400,000-token
window. The default 0.75 ratio yields a 300,000 trigger. Current Codex's GPT-5.5
entry has both selected/default and maximum capacity of 272,000 at
`codex/models-manager/models.json:808`. Several other current GPT-5 variants
default to 272,000 even where extended context is available.

Against the requested reference, GPT-5.5's preventive trigger is 28,000 tokens
beyond capacity, and the telemetry denominator understates pressure. A custom
deployment can override model capacity; this finding concerns the default and
does not assert the window of every backend using a similar model name.

**Reference:** `codex/protocol/src/openai_models.rs:509` also distinguishes
usable context and automatic-compaction limits. Changing Bro's 75% policy to
Codex's 90% would not fix an incorrect capacity and is not recommended as a
standalone change.

**Repair/test:** resolve per-model metadata, distinguishing selected/default,
maximum opt-in and effective usable window. Assert every supported catalog
fixture's threshold fits its selected capacity. Retain explicit operator
overrides and the current unknown-telemetry behavior for unknown models.

### F8. P2: Encrypted reasoning retrieval depends on an explicit effort argument

**Path:** `bro/transport/responses_common.rs:302` requests
`reasoning.encrypted_content` only inside the optional effort branch. CLI
effort defaults to absent (`bro/cli.rs:71`); `bro/agent_loop.rs:1168` passes it
through. The parser discards reasoning items without encrypted content at
`bro/transport/responses_common.rs:680`.

Thus an effort-unspecified request does not ask for replayable reasoning. If
the backend only returns encrypted content when requested, the authoritative
replay buffer loses that reasoning. This matters with `store:false`, full HTTP
replay and WS fallback. It is not a claim that every WS continuation loses
server-side reasoning immediately.

Effort is also absent from `Restored` and `SaveState` in `bro/session.rs:31`
through `:51`; resume cannot retain an explicit effort unless the caller
supplies it again. This differs from the persisted model and service tier.

**Reference:** `codex/core/src/client.rs:852` always requests encrypted
reasoning; `:770` resolves unspecified effort from model defaults.

**Repair/test:** decouple encrypted-state retrieval from explicit effort
selection and persist resolved session effort where continuity is intended.
Capture an unspecified-effort request, two tool steps, a resume and WS-to-HTTP
fallback, checking that replayable reasoning is requested and retained. Review
dispatch defaults separately before estimating how frequently this occurs.

### F9. P2: Overflow recovery does not ensure compaction itself fits

**Path:** `bro/transport/openai_responses.rs:376` sends full history unchanged
to remote compaction. If that endpoint's accepted window is exceeded, the
overflow recovery at `bro/agent_loop.rs:1499` repeats the oversized-history
problem inside compaction and ultimately returns the original error at `:1532`.
Actual backend failure depends on the compaction endpoint's capacity.

**Reference:** `codex/core/src/compact_remote_v2_attempt.rs:40` invokes a
pairing-preserving rewrite of oversized trailing tool outputs before sending
compaction; the algorithm is in `core/src/compact_remote_history.rs:68`.
Codex local compaction also removes older history on an overflow retry
(`core/src/compact.rs:316`). These mechanisms are bounded recovery, not a
promise that every possible prompt can be fitted.

**Repair/test:** use a disposable compaction input with an explicit fitting
policy, preserving tool identities and the newest user request. Mock a ceiling
at the compaction endpoint and supply oversized paired outputs. Require either
a fitted successful request or a clear failure with original history intact.

### F10. P2: Model downshift compacts with the smaller destination model

**Path:** `bro/agent_loop.rs:1273` immediately replaces `base_opts.model`.
Subsequent manual/automatic compaction receives the new options. History that
fits the previous window but not the new one must therefore already fit the
smaller model to be summarized by it.

**Reference:** `codex/core/src/session/turn.rs:1294` explicitly compacts with
the previous model before downshift when necessary. It also checks compaction
compatibility hashes, with a narrow fallback to the destination model.

**Repair/test:** retain previous model metadata through transition; compact
before applying a smaller inference window. Test history between the old and
new limits and assert old-model compaction precedes new-model inference. Treat
opaque summary compatibility as a separate contract if adopting Codex V2.

### F11. P2: Project AGENTS override discovery differs from Codex

**Path:** `bro/project_doc.rs:42` defaults to only `AGENTS.md`. Discovery at
`:105` and scoped refresh at `:632` load every configured filename rather than
selecting a preferred file. A project `AGENTS.override.md` is ignored by default.
Adding it to the configured list loads both, which still does not implement
Codex precedence.

**Reference:** `codex/core/src/agents_md.rs:269` tries override, ordinary file,
then configured fallbacks; `:251` selects the first existing candidate in each
directory. This is project discovery; Bro's separately additive global override
behavior should not be conflated with this case.

**Repair/test:** explicitly choose whether Codex-compatible project precedence
is the desired contract, then implement candidate selection in startup and
dynamic discovery together. Test both files present, override only, nested
overrides, and removal between requests. Preserve explicit alternate-doc
configuration as an intentional feature.

## Repair follow-up

The numbered findings above remain evidence about the pinned baseline. The
follow-up implementation addresses F1 through F11; it does not change those
historical source anchors or claim parity with every newer Codex feature.

| Findings | Implemented contract | Regression evidence |
| --- | --- | --- |
| F1, F2 | Responses replacement commits only after terminal success and validated summary structure; failures preserve native history. | `transport/openai_responses_compaction_tests.rs`: truncated, failed, incomplete and malformed SSE; invalid unary summaries; successful aliases; rollback. |
| F3, F4, F11 | Typed instructions use provider-specific placement before tasks; explicit clear emits revocations; project discovery selects override before AGENTS and reselects on refresh. | `agent_loop.rs`, `context/dispatch.rs`, `project_doc.rs`: startup, updates, removal, resume, compaction and generation isolation. |
| F5, F6 | Request projection includes retained output, native history, fresh instructions, activated schemas, ambient additions and hook directives. Occupancy checkpoints persist with history; compaction resets them. | `context/budget.rs`, `agent_loop/tests/budget.rs`, `session_startup_tests.rs`: request ordering, schema/document growth, manual reset and actual save/build/reopen. |
| F7, F8 | Explicit known model windows replace the broad GPT-5 assumption. Encrypted reasoning retrieval is independent of explicit effort; effort survives resume and CLI override. | `compaction.rs`, `transport/responses_common.rs`, `session_startup_tests.rs`. |
| F9 | Both Responses compaction paths fit disposable input, preserve protected history and validate the rendered request before sending. | `transport/openai_responses_compaction_tests.rs`: paired tool-output fitting, protected-content refusal before HTTP and failed-summary rollback. |
| F10 | Live and resumed downshifts refresh context, compact using the previous model, check destination occupancy and checkpoint rejection under the retained model. | `agent_loop/tests/budget.rs`, `session_startup_tests.rs`: previous-model ordering, instruction growth and successful/failed transitions. |

Implementation roots in this table are relative to `crates/bro-harness/src/`.
Tracked repairs: `gap-6bc48000`, `gap-edc4bc5e`, `gap-75da1a30`.

Validation uses source revision `5c4ae748` in the warm Linux lane. The final
`cargo nextest run --workspace --profile full -j 8 --no-fail-fast --retries 1`
passes 7,009 tests with no retries required (19 ignored/skipped), in 245.6 seconds.
The focused loop/lifecycle suite passes 103 tests. Two unrelated migration-lock
tests exceeded their three-second deadlines in a default-concurrency rerun;
both pass in isolation and in this final full run. The final gate saved its log
and exit status inside the lane to survive intermittent exec-connection loss.
Workspace formatting,
`cargo clippy --workspace`, and concurrency lint pass (108 handlers checked).
The expanded `--all-targets` Clippy check remains blocked by pre-existing test
lints in unchanged files: disallowed synchronous filesystem calls in
`crates/fleetd/src/workspace.rs`, and ignored socket-read counts in the Anthropic
and Chat Completions transport fixtures. These are separate from the required
workspace Clippy gate.

Request projection and compaction fitting use an approximate UTF-8 byte model,
with measured input as a separate floor. They are not tokenizer-exact capacity
guarantees, particularly for images and opaque reasoning. Ordinary inference
keeps a bounded proactive attempt and one reactive overflow recovery. Protected
instructions or schemas can still exceed capacity, and unknown model capacities
remain provider-validated. No live provider capacity or deployed-binary claim is
made. Codex V2 compaction, compatibility hashes and native discovery remain
separate adoption work described below.

## Codex mechanisms worth adopting

### A. One captured context state per request

Current Codex's `WorldStateSection` provides typed snapshots and rendering of
full, changed and removed state. `core/src/session/mod.rs:3548` uses the same
captured step for the model-facing state and tools; it persists the baseline
after recording the corresponding context.

Bro does not need Codex's entire extension system. A smaller equivalent should
unify environment, instructions, scope/pins and model configuration while keeping
stable, changed and per-request text explicit. A single preparation boundary
would make placement, removal, delivery receipts, compaction invalidation and
token projection testable together. Keep current structured filesystem
admission guarantees rather than replacing them with string recognition.

Environment's current-date refresh is user-turn based in Bro's CodexShaped
path. Codex additionally has step-level, cadence-controlled time reminders
(`core/src/session/time_reminder.rs`). Adopt such reminders only for work that
needs changing time; do not inject a fresh timestamp every model request merely
for parity.

### B. Modern remote compaction and local retention ownership

Current Codex appends `CompactionTrigger`, streams through the normal model
client, validates completion, and builds replacement history locally
(`core/src/compact_remote_v2_attempt.rs:70`, `compact_remote_v2.rs:491`). It
distinguishes real user/hook content from ambient context and applies a 64,000
token retained-message budget rather than delegating all retention to a unary
endpoint.

Bro's ChatGPT path still uses the legacy `/responses/compact` protocol. Its
generic local paths summarize a prefix and retain a message-count tail. After
a long tool loop, the current task can move into the summarized prefix even
though the latest tool traffic is retained. This is a fidelity tradeoff, not
proof that every summary loses the task.

Adopt explicit provenance for task/context/tool records, token-budgeted retention
and deliberate placement of restored context before the last real user input
or summary. Preserve essential current instructions verbatim through their
typed ledger. Protocol migration should be capability-gated and tested against
the chosen backend; legacy endpoint use alone is not evidence of breakage.

### C. Native Responses tool search to reduce schema churn

Bro's bounded activation is already better than an unbounded full catalog.
However, each activation changes the top-level `tools` array
(`bro/registry.rs:317`), causing WS incremental comparison to fail
(`bro/transport/openai_responses_ws.rs:163`). Activations persist across
compaction and eventually hit the 32-tool or 64 KiB lifetime limit
(`bro/registry.rs:691`). Full input transfer is source-confirmed; the size of
any server cache penalty still needs measurement.

Codex inserts discovered schemas into history as `ToolSearchOutput`
(`core/src/tools/context.rs:216`) while excluding deferred tools from the leading
specification list (`core/src/tools/spec_plan.rs:541`). Investigate a
Responses-specific native discovery path while keeping portable activation for
other providers. Do not simply evict old flat schemas while replay history
still relies on them. Test discovery, normalizing native search records,
compaction and resume as one contract.

### D. Model-aware wire behavior, including Responses Lite

Codex's current catalog enables Responses Lite for GPT-6 Astra and GPT-5.6
variants. Its client moves tools/base instructions into typed input-prefix items
and requests all-turn reasoning context for Lite (`core/src/client.rs:775`,
`:797`). Bro uses classic top-level fields without that selector
(`bro/transport/responses_common.rs:274`).

This is an adoption/compatibility investigation, not a proven quality defect or
claim that classic requests are rejected. Resolve these capabilities through
model metadata rather than additional broad string-prefix heuristics. Preserve
the compact capability-derived base instructions; copying model prompts that
assume tools Bro does not expose would undo an existing improvement.

## What is already correct and should stay

- A new task remains pending until proactive compaction completes, so pre-turn
  compaction cannot summarize that newly accepted task away
  (`bro/agent_loop.rs:1399` through `:1445`).
- Auto and overflow paths explicitly restore current context and invalidate
  scoped instruction delivery before continuing (`:1429`, `:1527`). The older
  blanket claim that compaction permanently loses all AGENTS context is false
  at this revision.
- Resume rereads document versions, does not restore delivery authority from
  arbitrary historical tool text, and announces unavailable process-local
  runtime state (`bro/agent_loop.rs:2283`, `bro/project_doc.rs:601`).
- Native compaction splitting protects function/custom call pairing;
  normalization repairs interrupted conversations
  (`bro/transport/responses_common.rs:705`, `:731`).
- Remote/local Responses history replacement resets the ambient hash and WS
  baseline (`bro/transport/openai_responses.rs:428`, `:549`).
- Ordinary ingress queues `/compact` at a turn boundary and preserves pending
  inputs (`bro/agent_loop.rs:614`). The apparent mid-turn compact branch in
  `drain_mid_turn_user_inputs` is not evidence of reachable context loss.
- Explicit resume fails closed on invalid snapshots and uncheckpointed effects
  rather than silently starting a fresh conversation. Snapshot recovery differs
  deliberately from Codex's rollout reconstruction (`bro/session.rs`).
- Input telemetry includes cache reads/creation; tool output has a 16 KiB
  default final backstop (`bro/bound.rs:4`); manifests are bounded; schemas have
  explicit activation limits; session cache keys and WS incremental replay
  exist. Neither "everything is unbounded" nor "there is no caching" is an
  accurate diagnosis.
- Bro's 75% general threshold and provider-specific exceptions are policy
  choices. Missing environment permission fields should only be added from
  actual runtime enforcement, not by copying Codex sandbox claims.

## Recommended implementation sequence and acceptance

1. **Make compaction replacement transactional (F1/F2).** Validate successful
   terminal and summary shape, retain original history on failure. Mocked
   provider-response tests are the first gate.
2. **Unify request projection and lifecycle accounting (F5-F10).** Resolve model
   capacity, count retained output and additions, restore/recompute occupancy,
   preserve reasoning, fit compaction requests and handle model transitions.
   Verify first-request behavior after process reconstruction, not just
   same-process helper calls.
3. **Integrate typed instruction delivery with placement and removal (F3/F4).**
   Capture real outbound requests for all three transports using temporary
   project docs, changed/removed scope and pins, and post-compaction history.
   Resolve project override semantics alongside this work (F11).
4. **Modernize Responses incrementally (A-D).** First introduce shared typed
   state/retention, then capability-gated compaction and discovery changes.
   Compare request bytes, input/cache usage, compaction frequency and task
   continuity on matched fixtures. Do not infer quality from fewer bytes alone.

Useful integration matrix: fresh startup; unchanged and changed second turn;
mid-step instruction change; explicit removal; automatic/manual/overflow
compaction; resume before and after compaction; smaller-model switch;
unspecified effort; newly activated tools; failed summary; WS-to-HTTP fallback.
Use isolated state and synthetic documents. Heavy gates belong on the project
lane infrastructure if implementation follows.

Tracking was deduped and submitted through `bbox_gap`:

- `gap-6bc48000`: context placement and explicit clearing.
- `gap-edc4bc5e`: compaction validation, fitting and model transition.
- `gap-75da1a30`: occupancy, model metadata and reasoning replay.

The gap tool accepted these through its asynchronous checkout-owner lane; all
three JSON records were subsequently verified in the local `.bbox/gaps/` store.
The report and gap records are committed together for review. No fixes or
successful runtime gates are claimed by this audit snapshot.
