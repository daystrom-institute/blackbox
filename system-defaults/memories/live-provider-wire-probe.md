+++
title = "Live provider wire probes — validate transport shapes on real endpoints"
tags = ["probe", "wire-probe", "live-probe", "wire-shape", "transport", "bro-harness", "chat-completions", "glm", "zai", "deepseek", "minimax", "mistral", "vibebh", "prompt-caching", "cache_control", "prompt_cache_key", "usage", "sse", "credentials", "curl", "validation", "runbook"]
order = 26
template = false
+++
# Live provider wire probes

A narrow live probe — a handful of minimal direct HTTP requests replicating the
exact harness wire shape — is the ONLY way to validate claims about provider
endpoint behavior: which request fields are accepted vs rejected, whether
caching actually engages, what the usage payload reports. Unit tests prove the
harness *sends* a shape; they cannot prove the endpoint *honors* it.

Cautionary tale (gap-d64d7a58): "zero cached tokens on MiniMax" was closed as
"endpoint ignores breakpoints — nothing to fix" without a probe. A later
doc-grounded probe found two real harness-side deviations AND that the endpoint
behavior had changed since measurement. Probe before concluding; conclusions
about endpoints decay.

## Ground rules

- **Narrow**: 2–4 requests per question, `max_tokens` ≤ 16, one model.
  Design the probe as an A/B comparison (old shape vs new shape) so one run
  answers the question.
- **Token hygiene**: read keys from their config files at runtime inside the
  script; never paste a key into a command line, transcript, or commit. When
  printing config for orientation, redact (`v[:12]+'...'`) anything matching
  `TOKEN`/`KEY`.
- **Non-streaming first**: `"stream": false` returns one JSON body with the
  full `usage` object — no SSE parsing. Only stream when the question IS about
  streaming event shape.
- Spend is pre-authorized for this kind of validation; keep probes narrow,
  then report exactly what was exercised.

## Credentials and endpoints

Same sources `resolve_provider_env` uses (`src/orchestration/brofile.rs`):

| Provider | Key location | Base URL | API shape |
|---|---|---|---|
| GLM | `~/.claude-zai/settings.json` → `env.ANTHROPIC_AUTH_TOKEN` | `https://api.z.ai/api/anthropic` | Anthropic Messages |
| DeepSeek | `~/.claude-ds/settings.json` → `env.ANTHROPIC_AUTH_TOKEN` | `https://api.deepseek.com/anthropic` | Anthropic Messages |
| MiniMax | `~/.claude-mm/settings.json` → `env.ANTHROPIC_AUTH_TOKEN` | `https://api.minimax.io/anthropic` | Anthropic Messages |
| vibebh (Mistral) | `MISTRAL_API_KEY` env, else `~/.vibe/.env` | `https://api.mistral.ai/v1` | OpenAI chat-completions |

The `settings.json` files carry an `env` block (`ANTHROPIC_AUTH_TOKEN`,
`ANTHROPIC_BASE_URL`, `ANTHROPIC_MODEL`); prefer reading base URL and default
model from there over hardcoding.

## Anthropic-shape probe (GLM / DeepSeek / MiniMax)

`POST <base>/v1/messages` with headers `Authorization: Bearer <token>`,
`anthropic-version: 2023-06-01`, and optionally the harness beta header
(`anthropic-beta: effort-2025-11-24,context-1m-2025-08-07,extended-cache-ttl-2025-04-11`
— see `DEFAULT_ANTHROPIC_BETAS` in `crates/bro-harness/src/transport/anthropic.rs`)
when the question involves beta-gated behavior.

Minimal body (mirror `build_body` — system as block array, messages with
array content, `cache_control` placement as shipped):

```json
{
  "model": "<from settings.json>",
  "max_tokens": 16,
  "stream": false,
  "system": [{"type": "text", "text": "<stable text>", "cache_control": {"type": "ephemeral"}}],
  "messages": [{"role": "user", "content": [{"type": "text", "text": "Say OK."}]}]
}
```

Read back `usage`: `input_tokens`, `output_tokens`, `cache_read_input_tokens`,
`cache_creation_input_tokens` (GLM emits full usage snapshots in
`message_delta` when streaming; MiniMax reports `cache_creation` as 0 — its
cache writes are free/uncounted).

For a cache probe, the stable prefix must clear the provider's minimum
(MiniMax documents 512 input tokens; ~60 repetitions of a sentence is plenty)
and the probe is: send the identical request twice, expect req2 to report
`cache_read_input_tokens` ≈ the prefix size with `input_tokens` collapsing to
the uncached tail. Caching docs:
MiniMax <https://platform.minimax.io/docs/api-reference/anthropic-api-compatible-cache>
(explicit breakpoints: plain `{"type":"ephemeral"}` only, no `ttl`, ≤4
breakpoints honoring the most recent, ~20-block lookback per breakpoint) and
<https://platform.minimax.io/docs/api-reference/text-prompt-caching>
(automatic, 512+ tokens); GLM <https://docs.z.ai/guides/capabilities/cache>
and DeepSeek <https://api-docs.deepseek.com/guides/kv_cache> are
implicit/automatic cachers — `cache_control` is tolerated metadata there.

## Chat-completions probe (Mistral / vibebh)

`POST https://api.mistral.ai/v1/chat/completions` with
`Authorization: Bearer <key>`. Body: `model` (e.g. `devstral-medium-latest`),
`max_tokens`, `messages` (plain string content), plus whatever field is under
test (e.g. `prompt_cache_key` — verified accepted 2026-06-11, with repeat
requests under a stable key serving `cached: 1520/1529`). Read
`usage.prompt_tokens_details.cached_tokens`; note `prompt_tokens` is
cache-INCLUSIVE (the harness subtracts the cached subset). Mistral has
historically 422'd on unknown fields ("Extra inputs are not permitted"), so
any NEW body field must be probed for acceptance before shipping — a rejected
field breaks every vibebh dispatch, not just caching.

## SSE capture (when streaming shape is the question)

Set `"stream": true`, keep `max_tokens` tiny, and capture raw lines (curl `-N`
or iterate the response). Anthropic-shape events arrive as
`event: <type>\ndata: <json>` pairs — grep for `message_start`,
`message_delta`, and the final `usage` fold. Chat-completions streams
`data: <json>` chunks with the usage-bearing chunk last (empty `choices`).
Save the raw capture to `/tmp` for inspection; don't paste full streams into
the transcript — quote only the lines that answer the question.

## Report shape

State: which endpoint+model, how many requests, the exact A/B difference, and
the usage numbers verbatim. Update the relevant gap note or thread with the
evidence (probe results decay — date them).
