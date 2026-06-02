---
title: "Brodex WebSocket Responses transport (alongside HTTP-SSE)"
kind: design
lifecycle: partial
corpus: blackbox-design
topic:
  - bro-harness
  - providers
  - brodex
brief: "Scope and design for a new WebSocket Responses transport for bro-harness (brodex), added ALONGSIDE the current HTTP-SSE transport (which stays the default and the fallback) — selected by BRO_HARNESS_TRANSPORT=openai-responses-ws. Tracks codex's responses_websockets=2026-02-06 path: a session-cached connection, generate=false prewarm, previous_response_id delta input, x-codex-turn-state sticky routing, and session-permanent HTTP fallback. Ground truth from openai/codex main (c955f730)."
---

# Brodex WebSocket Responses transport (alongside HTTP-SSE)

> **As-built (phases 1–5 done, live-verified).** Shipped: a shared
> `responses_common::ResponsesState` (one buffer, used by both paths); the WS
> channel (`openai_responses_ws::WsChannel`) — handshake with the shared
> identity/auth headers + `OpenAI-Beta: responses_websockets=2026-02-06`,
> `response.create` framing, connection **reuse** across turns/steps, codex-
> faithful **incremental input** (`previous_response_id` + delta, full-replay
> otherwise), and a stale-connection re-dial; and the unified routing transport
> (`openai_responses::OpenAiResponsesTransport`) — **auto-routing by auth mode**
> (ChatGPT-OAuth → WS, API-key → HTTP-SSE; **no env knob**) with **automatic
> session-permanent WS→HTTP fallback** on a WS transport failure (API errors
> propagate, they don't trigger fallback), and compaction always over HTTP.
> Verified live: default dispatch auto-routes to WS (one connect across a
> tool-using turn); a dead WS endpoint falls back to HTTP-SSE.
>
> **Phase 5 — scoped.** `x-codex-turn-state` sticky routing is implemented
> (captured first-wins from the handshake, replayed on reconnect handshakes and
> on HTTP-fallback requests). It is a routing/cache-warmth *hint*, not a
> correctness mechanism in this design: we reset the delta baseline on any
> reconnect (full-replay on a fresh socket), so `previous_response_id` is never
> carried across a reconnect and never needs another backend's state. Observed
> live: the ChatGPT backend did not even stamp a turn-state on our handshakes
> (`turn_state=None`), so the capture/replay is currently a no-op kept for codex
> parity + future-proofing.
>
> **Deliberately deferred (low value under our reuse model):**
> `generate=false` prewarm — connection reuse already keeps the socket warm
> across turns, so there is no per-turn reconnect for a prewarm to hide; it would
> only help the very first turn after a pre-prompt idle window. Proactive
> reconnect-on-idle — the stale-connection re-dial already recovers the ~60-min
> server idle close on the next send. Both are revisitable if the fleet
> persistent-session path shows a measurable first-token-latency win.
>
> The decisions below reflect the original proposal; the env-knob option was
> dropped in favor of auth-mode routing.

> **Relationship to the HTTP-SSE transport.** This is the "new brodex alongside
> the fixed legacy path" split: `openai_responses.rs` (HTTP-SSE) stays the
> default and is the fallback target. The WS transport is a *second*
> `TransportKind`, opt-in via `BRO_HARNESS_TRANSPORT=openai-responses-ws`. The
> two share the request body (`build_body`), the SSE/event parser (`parse_sse`),
> auth (`codex_auth`), and the modern header set; only the wire framing and
> connection lifecycle differ.

## Why (and when it pays off)

WS changes **transport**, not **correctness** — the model output is identical.
The wins are latency and bandwidth, and they only materialize in specific modes:

| Benefit | Mechanism | One-shot `bro_exec` | Multi-step turn (tool calls) | Persistent `session_loop` (fleet) |
|---|---|---|---|---|
| Skip per-call TLS/connect | cached connection on the transport | — (1 call) | ✅ reused across steps | ✅ reused across turns |
| Don't re-upload the transcript | `previous_response_id` + delta input | — | ✅ each step sends only new items | ✅ each turn sends only new items |
| First-token latency | `generate=false` prewarm | marginal | marginal | ✅ prewarm during idle |

**Verdict:** worth it for multi-step turns and the persistent fleet session;
near-neutral for a single one-shot call. Because brodex already keeps the
conversation in `self.input` and makes several `run_turn` calls per user turn
(model → tool → model …), even one-shot tool-using turns benefit from
within-turn connection + delta reuse. So WS is broadly useful, but the
persistent fleet path is where it shines.

## Codex WS mechanics (ground truth, `openai/codex` `c955f730`)

- **Connection lifecycle** (`core/src/client.rs`): lazy `websocket_connection()`,
  cached session-scoped in `ModelClientState.cached_websocket_session`
  (`StdMutex<WebsocketSession>`), reused across turns; handed to each per-turn
  `ModelClientSession` and stored back on `Drop`. Server idle limit ~60 min
  (`websocket_connection_limit_reached`). Connect timeout 15s
  (`WEBSOCKET_CONNECT_TIMEOUT`).
- **Prewarm** (`preconnect_websocket`): opens the socket and sends a
  `response.create` with `generate=false` — no inference, just establishes the
  connection and captures the sticky-routing token early; doubles as the turn's
  first connect attempt.
- **Wire framing** (`codex-api/src/common.rs`, `codex-api/src/endpoint/responses_websocket.rs`):
  up = `ResponsesWsRequest` enum, tag `response.create`
  (`ResponseCreateWsRequest` = `ResponsesApiRequest` + `previous_response_id` +
  `generate`) / `response.processed`. Down = the *same* `ResponseEvent` variants
  as HTTP-SSE (`OutputItemDone`, `OutputTextDelta`, `Completed { response_id,
  token_usage, end_turn }`, …) — so the event parser is shared. `permessage-deflate`
  is negotiated.
- **Incremental input** (`get_incremental_items`, `prepare_websocket_request`):
  if the non-input fields match the prior request AND the new `input`
  `starts_with` the prior baseline, send only the delta items + `previous_response_id`
  + `generate=true`; otherwise full replay with `previous_response_id=None`.
- **Sticky routing** (`x-codex-turn-state`): captured from the WS upgrade
  response headers, replayed (WS and HTTP) for the turn via an `OnceLock`.
- **Headers**: `OpenAI-Beta: responses_websockets=2026-02-06` on the handshake,
  plus `session-id` / `thread-id` / turn-metadata; auth is the same bearer +
  `chatgpt-account-id`.
- **Fallback** (`try_switch_fallback_transport`, `force_http_fallback`): on
  exhausted stream retries, set `disable_websockets` (session-permanent atomic),
  clear the cached WS session, and re-run the request over HTTP-SSE. Fallback is
  **session-scoped**, not per-turn.

## Design — mapping onto bro-harness

### Transport selection
Add `TransportKind::OpenAiResponsesWs`, selected by
`BRO_HARNESS_TRANSPORT=openai-responses-ws` (`transport/mod.rs::from_env`). The
daemon/fleet sets the env per dispatch, exactly as it does for the other
transports. Default stays `anthropic`; `openai-responses` (HTTP-SSE) is
unchanged.

### Where the connection lives
On the new transport struct (`OpenAiResponsesWsTransport`), as
`conn: Option<WsConnection>`. The `Transport` trait is a natural fit: the
transport instance outlives individual `run_turn` calls (it IS `self`), so the
socket persists across the steps of a turn and across turns in the persistent
`session_loop` — no session/turn/request scoping gymnastics like codex needs
(codex re-creates a `ModelClientSession` per turn and shuttles the cached
connection in/out; brodex's transport is already the long-lived object).

### Shared vs new code
- **Shared (factor out of `openai_responses.rs`):** `build_body` (the
  request body), `parse_sse`/event reconstruction, `codex_auth`, the modern
  header set, `service_tier`/effort/reasoning helpers, `classify_*`. Extract
  these into a `responses_common` module both transports import. This is the
  bulk of the value already built in the HTTP-SSE clean-cut — WS reuses it.
- **New (WS-specific):**
  1. `transport/openai_responses_ws.rs` — the transport impl: connect/handshake,
     framing (`response.create` envelope ↔ downstream events), prewarm,
     reconnect, the `previous_response_id`+delta computation, and the consume
     loop translating WS frames into the same sink events + `parse_sse` input.
  2. Delta-input state: track `last_response_id` + a snapshot of the `input`
     baseline sent last; compute the appended-suffix delta (mirror
     `get_incremental_items`), fall back to full replay when it doesn't line up.
  3. Sticky `turn-state` capture/replay (per-turn).
  4. Fallback: on WS failure past retry, transparently construct/borrow the
     HTTP-SSE path for the rest of the session (a `disable_ws` flag on the
     struct + delegate to the shared HTTP send). Simplest: hold an
     `OpenAiResponsesTransport` inside and delegate when `disable_ws` flips.

### Reuse of the (A) mid-stream retry
The request-level retry from (A) generalizes: a WS frame fault re-sends the
`response.create` (restart-from-buffer, dedup-safe on emitted text), and when
retries exhaust, flip to HTTP-SSE fallback (codex's exact ladder:
retry → fallback). The retry/fallback decision helper can be shared.

## Dependencies
`tokio-tungstenite` + `tungstenite` are already in the workspace lockfile
(codex's `codex-api` and other crates use them) — adding them to
`crates/bro-harness/Cargo.toml` is a dep-entry, not a new external pull. Use the
rustls feature to match bro-harness's existing `reqwest` rustls-tls. Note: the
WS *handshake* needs the auth + beta + identity headers on the upgrade request
(tungstenite supports custom handshake headers).

## Risks
- **Private, undocumented protocol.** `responses_websockets=2026-02-06` is
  reverse-engineered from codex; the framing/version can change server-side with
  no notice. The HTTP-SSE default + automatic fallback is the safety net.
- **Reconnect/turn-state correctness.** Sticky routing + the 60-min idle limit
  mean reconnect logic must re-capture turn-state and rebuild cleanly.
- **Delta-input edge cases.** If the baseline check is wrong, the server sees a
  malformed continuation; the conservative full-replay fallback must be the
  default whenever anything is uncertain.
- **Compression.** `permessage-deflate` negotiation must match or be disabled.

## Phasing
1. **Refactor:** extract `responses_common` from `openai_responses.rs` (no
   behavior change; HTTP-SSE keeps passing its tests). Lands first, low-risk.
2. **WS skeleton:** connect + handshake (auth/beta/identity headers) + single
   `response.create` (full input, `generate=true`) + consume → reuse `parse_sse`.
   Verify live against the ChatGPT backend.
3. **Reuse + delta:** cache the connection across `run_turn`; add
   `previous_response_id` + delta input with full-replay fallback.
4. **Prewarm + fallback:** `generate=false` prewarm; WS→HTTP session-permanent
   fallback wired to the shared retry helper.
5. **Sticky routing + reconnect:** capture/replay `x-codex-turn-state`; reconnect
   on idle/drop.

## Open decisions for the operator
1. **Scope now:** do all of 1–5, or land 1–2 (refactor + a working single-shot WS)
   and defer delta/prewarm/sticky until the fleet path needs them?
2. **Fallback richness:** mirror codex's session-permanent fallback, or simpler
   per-turn "try WS, on any failure use HTTP for this turn"?
3. **Default for fleet:** once stable, should the fleet persistent session default
   to WS (HTTP for one-shot `bro_exec`), or stay opt-in via env?
