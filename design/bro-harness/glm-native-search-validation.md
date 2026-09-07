---
title: "GLM native search consistency validation"
kind: design
corpus: blackbox-design
lifecycle: partial
topic:
  - bro-harness
  - provider-protocol
brief: "Captured live controls tie spurious native searches to a missing previously activated tool schema, with retry and failed-turn observation regressions."
---

# GLM native search consistency validation

Investigation owner: `thread-c130128f`. Related gaps: `gap-900d052c`
(native-search initiation), `gap-0ab38ab1` (retry after native execution),
and `gap-4c45ac63` (native observations lost on terminal parse failure).
`gap-13d2f1e6` tracks provider-emitted duplicate client calls in mixed native
search responses.

## Finding

Live controls on 2026-09-07 UTC reproduce spurious native search when resumed
history says a client tool was activated but its schema is absent from the
current request. Restoring only that schema makes the same request call the
intended client tool. This establishes a reproducible trigger and a successful
control, not the provider's internal routing algorithm or a universal absence
of spurious search.

The existing deferred-activation persistence and legacy-receipt recovery repair
prevents the demonstrated accidental loss on normal process resume. Current
policy/catalog filtering remains required. Intentionally unavailable tools can
still create a history/catalog mismatch; the provider must not be assumed to
handle that condition safely merely because normal resumes pass.

No query deny list, invented native result, or default search disablement was
used. Native `web_search_20250305` remained in every captured request. No client
tool executes provider-owned `server_tool_use` blocks.

## Live protocol controls

An installed standalone harness ran against a loopback capture proxy forwarding
to the configured Z.AI Anthropic endpoint. A synthetic MCP server supplied only
`bro_report`. All prompts and state were synthetic, with a private `BRO_HOME`.
Provider credentials stayed in the proxy and were not written into captures.
These isolated captures supplement the earlier normal `bro_exec`/`bro_resume`
admission checks; they are not claims about the worker binary revision.

| Control | Observed result |
| --- | --- |
| Flash fresh arithmetic, fresh report discovery/call, three report process resumes | No native search across ten provider requests |
| Flash fresh requested documentation search, tool continuation, replay-only process resume, second requested search | Two relevant native searches across seven requests; no new search on replay-only resume |
| Flagship arithmetic, report discovery/call, three report process resumes | No native search across ten requests |
| Flagship requested search and resumed requested search | Relevant native search works; client web-fetch exploration increased the first task's request count |
| Flagship clear only the probe snapshot's `side.tool_activations` to `[]`, then resume reporting | Five completed `placeholder` searches in one response, followed by malformed client/native call data |

Both requested and provider-reported models were captured: `glm-5.3-flash` and
`glm-5.3`. Response `X-LOG-ID` and SSE message IDs identify each request.
Token/window telemetry was not used to infer model identity or search absence.

The Flash search call/result blocks were preserved with their original IDs,
inputs and provider result content in the subsequent request, process-resume
request and durable assistant observation. The generic GLM `tool_result`
variant remains in assistant content for replay, separate from client results
in user content.

Two mixed Flash native-search responses also emitted duplicate client calls
with distinct IDs: duplicate `tool_search` during the first lookup and duplicate
`bro_report` during the resumed lookup. The latter reached the synthetic MCP
server twice. These duplicates were already present in a single raw provider
response, not manufactured by harness replay. They remain a separate provider
behavior to track; deduplicating arbitrary same-argument client calls would
change legitimate tool semantics.

## Exact-request schema comparison

The failing flagship request was replayed directly to the provider without
executing any returned client calls. The restored variant differed only by
adding the previously advertised `mcp__blackbox__bro_report` schema back to the
tool array. System text, history, user request and native search declaration
were unchanged.

| Requested model | Schema | Provider log ID | Native outcome |
| --- | --- | --- | --- |
| `glm-5.3` | Restored | `202609070916399b1fe217b3994d53` | Direct `bro_report`; no search |
| `glm-5.3` | Missing | `20260907091643f49689d2e915467c` | Five searches: `placeholder`, then `placeholder2` through `placeholder5` |
| `glm-5.3` | Restored | `2026090709171060605e49407d4e85` | Direct `bro_report`; no search |
| `glm-5.3-flash` | Missing | `202609070918054baba9eb3b524094` | Five searches about internal synthetic checkpoints and report tooling |
| `glm-5.3-flash` | Restored | `2026090709184664c495abaee34a88` | Direct `bro_report`; no search |

The initial lost-activation response has log ID
`202609070914472e22c6c864994599`. Its request contains no `placeholder` literal.
All five searches and their result blocks occurred inside one HTTP response;
there was no harness retry between them. Historical sessions lack exact
outbound catalogs, so their precise request bytes cannot be retrospectively
proven identical to this reproduction.

## Failure and observation boundary

After five native searches, the failing response starts a client `tool_search`
call with a two-byte partial JSON input, then starts a separate native
`web_search_prime` block reusing that client call ID and carrying the intended
client arguments. No result follows that sixth native start. The harness
correctly refuses malformed client JSON. Reconstructing client arguments from
the same-ID native block would invent an unsupported protocol repair.

The provider's terminal usage reports zero web-search requests despite five
result-bearing searches. Usage counters therefore cannot establish whether
search happened. Observe the native blocks themselves.

Before the failure-observation repair, successful-turn-only observation handling
lost those five completed native call/result pairs when the later client input
failed parsing. The durable log ended at the user turn; raw stdout/SSE and
stderr were the only evidence. Failed provider observations need a separate
terminal event, not a partially committed replayable assistant turn.

`11dc7105` adds a single nonreplayable `failed_turn_observation` system event
containing native blocks and malformed-input, duplicate-ID and missing-result
diagnostics. Missing results explicitly leave execution status unknown.
Terminal error emission now covers one-shot and controlled entry modes. The
malformed client call remains undispatched, and failed native blocks stay out
of conversation replay.

An independent retry defect also allowed the original request to be retried
after a native call/result arrived entirely in `content_block_start`, because
the retry guard only recognized deltas. `ed905a53` makes meaningful block starts
non-retryable while retaining precontent retry for empty text/thinking starts.
This does not explain the single-response placeholder reproductions.

## Reproduction and evidence

Use `scripts/probe-glm-native-search.py --help` for the opt-in live runner.
Supply `--live`, an explicit harness binary and an explicit settings file.
`--lost-activation` changes only the script's own synthetic session snapshot.
The runner retains private request JSON, raw SSE, process outputs, snapshots,
native replay checks and a machine-readable summary. A clean limited sample is
not a statistical guarantee. Never run the destructive control against an
operator session.

The checked-in runner's live Flash validation used the locally built retry-fix
binary and forwarded the harness's Anthropic beta header as well as the request
body. Fifteen requests completed all seven tasks: no searches in arithmetic or
report-only tasks, three relevant searches across the two requested lookups,
and exact native replay, snapshot and durable observation comparisons. The
runner correctly returned **fail**, with no inconclusive checks, because the
first search response again emitted duplicate client `tool_search` calls.
No duplicate report executed in this run. Passing search-intent checks is not
an overall mixed-tool protocol pass.

Raw search results remain in private local evidence storage, not the public
repository. The thread records the concrete paths and session IDs.

## Repair verification

At `11dc7105`, lane-side
`cargo nextest run --workspace --profile full -E 'package(bro-harness)'`
passed 520 tests with three skipped. The pinned formatter passed, and the
native arm64 release harness built successfully. Lane-side
`cargo clippy -p bro-harness --all-targets` completed successfully with warnings.

The actual failing provider SSE was then replayed through that rebuilt binary
using a local scripted endpoint, with no provider request. The harness made one
HTTP request and exited with failure, retaining one failure observation and one
terminal error. It preserved six native starts and five native results, marked
the sixth result missing with unknown execution status, and emitted no assistant
message or client tool call. The observation is explicitly nonreplayable.

Source repairs are committed and built; these checks are not a shared-runtime
replacement or deployment receipt. The original native-search consistency gap
remains open because catalog-unavailability behavior and duplicate client
emission are still material limits.

## Quota policy reconciliation

The [July 30 plan-update notice](https://docs.z.ai/devpack/notice/usage-revision)
explains why the operator's quoted multipliers differ from the current
[credit-plan overview](https://docs.z.ai/devpack/overview). Legacy Plan V2 lists
flagship 1x off-peak/3x peak and Flash 0.4x/1.2x. The newer plan uses separate
input, cached-input and output credit multipliers, with a 50% off-peak rate.
Existing active legacy subscriptions retain their calculation method, and the
notice directs users to the signed-in plan label to identify their version.
The quota endpoint's `level=max` alone does not identify that version.

The separate [Flash campaign](https://docs.z.ai/devpack/notice/event-glm-5.3-flash)
runs September 3-20, 2026, from 23:00 to 09:00 Singapore time: ZCode gets zero
quota consumption; other supported agents get doubled quota. These captured
runs began after 09:00 Singapore time, outside that daily campaign window.
No numerical quota policy change is justified without the account's plan label.
