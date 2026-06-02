---
title: "Brodex ↔ Responses API deep dive (vs. codex CLI)"
kind: design
lifecycle: partial
corpus: blackbox-design
topic:
  - bro-harness
  - providers
  - brodex
brief: "Deep scrutiny of how the bro-harness OpenAI-Responses transport (brodex) interacts with the API, measured against the upstream openai/codex Rust CLI (cloned at main c955f730, 2026-06-02). Per-axis ground truth from codex with brodex gaps ranked by severity: fault handling, beta/feature-flag headers, modernness of the Responses integration, harness/system injections, and how /fast is propagated. Mirrors the anthropic-harness deep dive. As-built: the Critical/High/most Medium gaps were closed in a clean-cut rewrite of openai_responses.rs (verified live against the ChatGPT backend, 2026-06-02)."
---

# Brodex ↔ Responses API deep dive (vs. codex CLI)

> **As-built (clean-cut rewrite, 2026-06-02).** No legacy/versioned shape kept —
> `openai_responses.rs` was rewritten to the modern codex contract (git holds the
> old shape). **Closed:** defunct `responses=experimental` dropped; random
> `session_id` → stable `session-id` + per-turn `thread-id`; stable
> `prompt_cache_key`; reasoning continuity via `include:["reasoning.encrypted_content"]`
> with encrypted reasoning items replayed; `/fast` → `service_tier`
> (`--service-tier` / `BRO_HARNESS_SERVICE_TIER` → body, `default` dropped);
> reasoning gated by model + `minimal`/`xhigh` effort + `reasoning.summary`
> (codex default `auto`); SSE per-event idle timeout; error-code classification
> (`context_length_exceeded`/quota/overload); one-shot `401`→token-refresh→retry;
> descriptive `User-Agent`; `Retry-After` HTTP-date parsing; `account_id`
> id_token fallback. **Verified live** against the ChatGPT backend (`gpt-5.5`):
> base shape and `service_tier:"priority"` both return a clean `result`
> envelope with cache reads. **Deferred (still open, see end):** WebSocket
> transport; `<environment_context>`/model-specific base-prompt injection
> (agent-loop concern, not transport); full mid-stream resume; rate-limit
> snapshot surfacing; `x-codex-*` observability headers; retry jitter; request
> compression; structured-output `text.format`. **Remaining wire-up:** the
> daemon must pass `--service-tier`/`BRO_HARNESS_SERVICE_TIER` for an end-user
> `/fast` to reach brodex (`providers/exec_args.rs` / `resolve_provider_env`).

> **Method.** Cloned `openai/codex` at `main` (`c955f730`, 2026-06-02) into
> `~/repos/codex`. Extracted ground truth from `codex-rs/core/src/client.rs`,
> `codex-api/`, `codex-client/`, `model-provider-info/`, `login/`, and
> `protocol/`. Compared against our `crates/bro-harness/src/transport/`
> (`openai_responses.rs`, `codex_auth.rs`, `http.rs`, `mod.rs`). Crux findings
> (transport family, beta headers, originator, `/fast`→service_tier) were
> re-verified by hand against codex source; the rest carry file-path citations.
> This is a **gap inventory**, not an implementation plan — fixes are scoped at
> the end but not yet applied.

## The single most important finding

**Codex no longer speaks the request shape brodex emulates.** The string
`responses=experimental` — which brodex sends as `OpenAI-Beta` on every ChatGPT-
backend call (`openai_responses.rs:125,464`) — **does not appear anywhere in
codex `main`** (`rg 'responses=experimental'` → 0 matches). Codex has moved to:

1. A **WebSocket-first** Responses transport (`OpenAI-Beta:
   responses_websockets=2026-02-06`, `client.rs:146,938`), with the HTTP-SSE
   path kept only as a session-scoped fallback.
2. A rich **`x-codex-*` header family** for sticky routing, turn metadata,
   installation/window/parent-thread identity, and beta-feature negotiation
   (`x-codex-beta-features`) — replacing the single `OpenAI-Beta` toggle.
3. A **stable `session-id` + per-turn `thread-id`** identity model
   (`codex-api/src/requests/headers.rs:5-14`), where brodex sends a **made-up
   `session_id` header with a fresh random UUID per request**
   (`openai_responses.rs:127`).

Brodex still works because the backend tolerates the legacy shape and ignores
unknown headers — but it is pinned to a frozen 2024-era request contract and is
blind to the routing/caching/observability machinery codex now relies on.

## Axis 1 — Modernness of the Responses integration

| Aspect | codex (`main`) | brodex today | Gap |
|---|---|---|---|
| Primary transport | **WebSocket** (`responses_websockets=2026-02-06`), cached + prewarmed per session (`generate=false`), incremental via `previous_response_id`; HTTP-SSE is fallback | HTTP-SSE only | Major — different protocol generation |
| Beta header | `x-codex-beta-features` (CSV keys) on HTTP; `OpenAI-Beta: responses_websockets=…` on WS | `OpenAI-Beta: responses=experimental` (defunct) | **Critical** — stale value |
| `prompt_cache_key` | Always sent; stable `thread_id` (`client.rs:371-375`) | **Not sent at all** | High — degraded cache hit rate / cross-session collisions |
| Reasoning continuity | `include:["reasoning.encrypted_content"]` when reasoning present (`client.rs:750-754`); encrypted reasoning replayed across turns | **Drops all reasoning items** (`store:false`, no `include`) — `openai_responses.rs:365-370` | High — loses cross-turn reasoning on o-series/GPT-5 |
| `store` | `true` only for Azure endpoint, else `false` (`client.rs:781`) | Always `false` | OK (matches ChatGPT path) — but no Azure awareness |
| `previous_response_id` chaining | Yes (WS incremental) | No — full `input[]` replayed every turn | Medium — more upload + no server-side continuity |
| `text.format` (structured output) | Supported (`codex_output_schema`, strict toggle) | Not supported | Low (brodex defers `--output-schema`) |
| `client_metadata` | `{x-codex-installation-id}` | None | Low |
| `parallel_tool_calls` | from prompt config (default false) | hardcoded `false` | OK |
| `tool_choice` | hardcoded `"auto"` | hardcoded `"auto"` | OK |
| function `strict` | hardcoded `false` | hardcoded `false` | OK (parity) |

**Notable:** even the *field set* is behind. Codex's `ResponsesApiRequest`
(`codex-api/src/common.rs`) carries `service_tier`, `prompt_cache_key`,
`include`, `text`, `client_metadata`; brodex sends `model/input/instructions/
tools/tool_choice/parallel_tool_calls/stream/store/reasoning` only.

## Axis 2 — Headers, beta flags, identity

Codex's Responses request carries (verified in `client.rs:490-650, 925-948,
1685-1742`, `default_client.rs:232-248`, `bearer_auth_provider.rs`):

- `Authorization: Bearer <token>` (OAuth access token or API key)
- `ChatGPT-Account-ID` (note casing) — account id
- `originator: codex_cli_rs` — **default header on the shared reqwest client**
  (`default_client.rs:234`), overridable via `CODEX_INTERNAL_ORIGINATOR_OVERRIDE`
- `User-Agent: codex_cli_rs/<ver> (<OS> <ver>; <arch>) <ua>` — version + OS +
  arch (`get_codex_user_agent`, `default_client.rs:133-157`)
- `session-id` (dash) — **stable** UUIDv7 per session; `thread-id` (dash) —
  fresh per turn (`requests/headers.rs:5-14`)
- `x-codex-beta-features` (CSV) / `OpenAI-Beta` (WS only)
- `x-codex-turn-state` — sticky-routing token captured from the server's
  turn-start response and replayed for the rest of the turn (`client.rs:243-251`)
- `x-codex-turn-metadata` (JSON observability bag), `x-codex-installation-id`,
  `x-codex-parent-thread-id`, `x-codex-window-id`, `x-client-request-id` (WS)
- `x-openai-subagent` (review/compact/memory_consolidation/collab_spawn),
  `x-openai-memgen-request`
- `X-OpenAI-Fedramp`, `x-openai-internal-codex-residency`, `x-oai-attestation`
  (conditional)
- `Content-Encoding: zstd` (request compression, `codex-client/src/request.rs`)

Brodex sends: `authorization`, `chatgpt-account-id`, `OpenAI-Beta:
responses=experimental`, `originator: codex_cli_rs`, `session_id: <random uuid>`
(`openai_responses.rs:119-128`). No `User-Agent` (reqwest default), no
`thread-id`, no `x-codex-*`, no compression.

| Gap | Severity | Note |
|---|---|---|
| `OpenAI-Beta: responses=experimental` is defunct | **Critical** | Drop it; HTTP path sends no `OpenAI-Beta` |
| `session_id` (underscore) is a non-standard name | High | Correct header is `session-id` (dash) |
| `session_id` is random per request, not stable | High | Defeats sticky routing + cache locality |
| Missing `thread-id` (per-turn) | Medium | Server expects session/turn split |
| No descriptive `User-Agent` | Low | codex sends originator/ver/OS/arch |
| No `x-codex-turn-state` replay | Low–Med | We don't run multi-request turns the same way |
| `x-openai-subagent` not set for sub-roles | Low | brodex has no subagent concept here |

## Axis 3 — How `/fast` is propagated to the API

**This is the headline ask, and the answer is concrete.** Codex maps a fast
intent onto the **`service_tier` request-body field**, not onto effort or model:

- `Feature::FastMode` (a feature flag) gates it (`session/mod.rs:566-568`,
  `turn_context.rs:500-502`).
- `get_service_tier(configured_tier, fast_mode_enabled, model_info)`
  (`session/mod.rs:797-809`): if fast mode is off → `None`; else pass the
  configured tier if the model supports it.
- `ServiceTier::Fast.request_value() == "priority"`, `Flex == "flex"`
  (`config_types.rs`); `service_tier_for_request` drops the literal `"default"`
  and validates `supports_service_tier` (`openai_models.rs:527-538`).
- Net: `/fast` ⇒ `service_tier: "priority"` on the Responses body (gated by
  feature flag + model support).

**Brodex has no `service_tier` field and no fast concept whatsoever.** The
daemon's `--effort`/`/fast` lever, if it reaches brodex at all, lands only on
`reasoning.effort` (`openai_responses.rs:291-293`). So **`/fast` is currently a
no-op against the Responses API in brodex** — the request that goes on the wire
is identical fast or not. This is the cleanest, highest-value fix: thread a
`service_tier` knob through `TurnOpts` and emit `"priority"` when fast.

## Axis 4 — Reasoning effort / summary / verbosity propagation

| Field | codex | brodex | Gap |
|---|---|---|---|
| `reasoning.effort` values | `none, minimal, low, medium, high, xhigh` (`openai_models.rs:47-55`) | `low, medium, high` only (`normalize_effort`, `openai_responses.rs:562-568`) | Missing `minimal` (latency knob) + `xhigh`; `max` collapses to `high` |
| effort gating | only when `model_info.supports_reasoning_summaries` (`client.rs:718-735`); `nearest_effort` maps to model's supported set | **sent unconditionally** for every model | Medium — sending `reasoning` to a non-reasoning model can 400 |
| `reasoning.summary` | `auto`(default)`/concise/detailed/none`, gated by model (`client.rs:723-731`) | **never sent** | Medium — no reasoning-summary stream to surface |
| `text.verbosity` | `low/medium/high`, gated by `support_verbosity` (`client.rs:755-765`) | **never sent** | Low–Med — GPT-5 verbosity control unused |
| model-family detection | capability flags on `ModelInfo` from a model catalog (no string prefixing) | none — flat treatment | Medium — brodex can't gate features per model |

`minimal` effort is the closest thing codex has to a "speed via less thinking"
knob (it exists in the enum but is operator-selected, not auto-applied); the
*latency* lever proper is `service_tier` (Axis 3).

## Axis 5 — Fault handling, retries, rate limits

| Aspect | codex | brodex (`http.rs`, `openai_responses.rs`) | Gap |
|---|---|---|---|
| Max retries | stream 5 / request 4 (cap 100), provider-configurable | 3 (`BRO_HARNESS_MAX_RETRIES`) | Low |
| Backoff | 200ms base ×2.0, **jitter 0.9–1.1** (`core/src/util.rs`) | 500ms→8s capped, **no jitter** | Low (add jitter) |
| Retryable | 5xx + transport by default; 429 special-cased; error-code aware | 408/425/429/5xx + connect/timeout | OK-ish |
| `Retry-After` | numeric **and** parses `"try again in N (s|ms)"` from error body (`sse/responses.rs`) | numeric seconds only (`http.rs:47-54`) | Low |
| Rate-limit snapshot | parses `x-<limit>-primary/secondary-used-percent / window-minutes / reset-at` into `RateLimitSnapshot`, surfaced to user; also a `codex.rate_limits` event (`codex-api/src/rate_limits.rs`) | **none** | Medium — no plan-limit visibility |
| Idle/stream timeout | **5-min idle timeout between SSE/WS events** → `Stream("idle timeout")` (`sse/responses.rs`) | none (only 600s whole-request timeout) | **Medium** — a hung stream blocks the full timeout |
| Stream-close handling | "closed before response.completed" is retryable | bails the turn | Medium |
| Error classification | `context_length_exceeded`→ContextWindowExceeded (drives compaction), `insufficient_quota`/`usage_not_included`/`cyber_policy`/`invalid_prompt` non-retryable; `server_is_overloaded`/`slow_down` retryable | generic `bail!` on `response.failed`/`error` (`openai_responses.rs:350-352`) | **Medium** — no context-exceeded→auto-compact, no quota distinction |
| 401 recovery | `UnauthorizedRecovery` state machine: reload auth.json (account-id guarded) → refresh token → retry (`login/.../manager.rs`) | **no 401 refresh** — only proactive | **Medium** — expired/revoked token fails hard |

Brodex's `incomplete` handling (`openai_responses.rs:337-348`) and `Retry-After`
honoring are good; the gaps are idle-timeout, error-code classification, and
401-driven refresh.

## Axis 6 — Harness / system injections

Codex injects far more context than brodex:

- **Base instructions** are **per-model**, pulled from a model catalog
  (`ModelInfo.base_instructions`, optional `instructions_template` with a
  `{{personality}}` slot — `openai_models.rs:376-395`), landing in the stable
  `instructions` field. There is no single hardcoded prompt.
- **Per-turn `input[]` developer items** (rebuilt each turn) for permissions,
  collaboration mode, personality, apps, available skills, plugins, extension
  context (`session/mod.rs:2667-2795`).
- A **`<environment_context>` XML block** (cwd, shell, current date, timezone,
  network allow/deny, filesystem roots + permission profile, subagents) and a
  **`<user_instructions>` block** (project docs / AGENTS.md) injected as a user
  item (`context/environment_context.rs`, `protocol/src/protocol.rs:92-95`,
  `session/mod.rs:2796-2848`), via a `ContextualUserFragment` trait.

Brodex sends one `instructions` string (daemon-supplied stable text) plus one
trailing volatile `developer` item (the deferred-tool manifest) —
`openai_responses.rs:262-294`. It injects **no structured environment grounding**
(the model doesn't get cwd / OS / date / sandbox / network posture from the
transport) and **no model-specific base prompt**.

Caveat: brodex composes its system prompt upstream (`registry.rs` /
`hooks.rs`), so some grounding may live in the stable text the daemon hands in.
Whether brodex injects an equivalent environment block is a **follow-up to check
against the agent-loop prompt assembly**, not a transport-only verdict. The
stable/volatile split itself (cacheable prefix + volatile tail) is sound and
mirrors codex's instructions-vs-input separation.

## Where brodex is actually *ahead* of codex

- **auth.json write safety.** Brodex uses an advisory file lock + atomic
  temp-then-rename + 0600 (`codex_auth.rs:40-58,190-200`). Codex does an
  in-place truncate write with no lock (`login/.../storage.rs`). Brodex's
  cooperative-with-codex-CLI locking is the more robust design; keep it.
- Brodex correctly subtracts `cached_tokens` to keep `input_tokens` fresh
  (`openai_responses.rs:324-336`) — clean cache accounting.

## Gap inventory, ranked

**Critical (correctness / staleness)**
1. Drop `OpenAI-Beta: responses=experimental` (defunct in codex).
2. Fix the identity headers: stable `session-id` (dash) + per-turn `thread-id`;
   stop sending random `session_id` per request.

**High (quality / cost)**
3. Send a stable `prompt_cache_key` (per-session) — codex uses `thread_id`.
4. Preserve reasoning continuity: `include:["reasoning.encrypted_content"]` +
   replay encrypted reasoning items (the path brodex's own comment defers).
5. Propagate `/fast` → `service_tier:"priority"` (currently a wire no-op).

**Medium (robustness / features)**
6. Gate `reasoning`/`summary`/`verbosity` by model capability; add `minimal`/
   `xhigh` effort; stop sending `reasoning` unconditionally.
7. Add a stream idle-timeout and classify error codes
   (`context_length_exceeded` → compaction, quota vs overload).
8. Add 401-triggered token refresh (not just proactive-on-expiry).

**Low (parity / polish)**
9. Descriptive `User-Agent`; retry jitter; `Retry-After` body parsing;
   rate-limit snapshot surfacing; request compression; structured-output
   `text.format`.

**Follow-up checks (not transport-local)**
10. Whether the agent loop injects an environment block (`<environment_context>`
    equivalent) and model-specific base instructions upstream of the transport.
11. Whether to track codex's WebSocket transport at all, or stay HTTP-SSE and
    just modernize the request/headers (likely the latter for brodex's scope).
