---
title: "Search provider abstraction: native passthrough, hosted backends, one governance plane"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - bro-harness
  - providers
  - tools
  - web-search
  - transports
brief: "Web search is the one tool in the bro-harness surface that bypasses the unified governance plane: it is a bare `web_search: bool` (default ON) that each transport hand-translates into its provider's native server-side tool def — three different spellings (Anthropic `web_search_20250305` capped at a hardcoded max_uses=5; OAI Responses `{type:web_search}` uncapped; chat-completions emits NOTHING, so vibe-bh can't search) — appended to the provider request OUTSIDE the registry, so the ToolFilter allow/deny list silently does not gate it. This doc proposes a search-provider abstraction along two orthogonal axes — emission SHAPE (per-transport native projection) and execution LOCUS (provider-native passthrough vs a hosted backend like Brave behind a SearchProvider trait) — fed by one normalized SearchConfig, and folds search-enable back through the same ToolFilter/surface machinery as every other tool. Subsumes the backlog-transport-polish.md 'client-side web_search fallback' + 'server result normalization' bullets. Open questions: the Mistral/vibe-bh surface, exact OAI Responses/codex parameters we should expose, GLM's web_search_prime result divergence, where SearchConfig lives, and whether a hosted backend earns an in-box binding."
---

# Search provider abstraction

> **Status.** Proposed. Sibling of
> [`anthropic-harness.md`](./anthropic-harness.md) (transport mechanics),
> [`narf-tool-placement.md`](./narf-tool-placement.md) (in-box/out-box taxonomy —
> web search is out-box/interpretive there), and the
> [`harness-daemon-boundary.md`](./harness-daemon-boundary.md) §6 surface
> governance. **Subsumes** two backlog bullets in
> [`backlog-transport-polish.md`](./backlog-transport-polish.md) (the client-side
> `web_search` fallback backend, and server-result normalization). Code facts are
> grounded against `crates/bro-harness/` on `beta/blackbox-v2`; external API
> parameter sets were verified against vendor docs (June 2026) and are tier-marked;
> genuinely-unresolved points are collected in §6.

## 0. The gap

Every dispatch-capable provider in the catalog has *some* native server-side web
search, but the harness exposes it as a bare per-session **`web_search: bool`**
that each transport hand-translates into that provider's native tool def. The
result is three divergent spellings, one missing one, a hardcoded magic cap, and
— most importantly — a tool that **bypasses the unified tool-governance plane
entirely**. This doc is the plan to replace the bool with a real abstraction.

Two framing facts set the scope:

- **Search is out-box.** Ranked web results are interpretive — the model must
  judge them — so per [`narf-tool-placement.md`](./narf-tool-placement.md) §2.1
  web search is model-facing only, never an in-box NARF binding. (`web_fetch`,
  the exact URL-keyed read, is the in-box one and is already wired; search is
  not.) This doc is about the **out-box / transport** layer, orthogonal to the
  NARF MVP arc.
- **Scope is the three bro-harness transports.** `anthropic`,
  `openai_responses` (+ the WebSocket variant), and `openai_chat`. The
  CLI-backed providers (`claude` → Claude Code CLI, `codex` → codex CLI,
  Inception → OpenCode) configure search through *their own* config, out of our
  request builder — see §6 OQ-7.

## 1. Current state (grounded in code)

The control is a single bool, **default ON**:

```rust
// agent_loop.rs:515
let web_search = std::env::var("BRO_HARNESS_WEB_SEARCH")
    .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
    .unwrap_or(true);   // <-- on unless explicitly disabled
```

Each transport then appends a native tool def to the provider request, keyed only
on `opts.web_search`:

| Transport (`BRO_HARNESS_TRANSPORT`) | Providers | What we emit today | Source | Cap |
| --- | --- | --- | --- | --- |
| `anthropic` | GLM, DeepSeek, MiniMax | `{"type":"web_search_20250305","name":"web_search","max_uses":5}` | `anthropic.rs:69` | **hardcoded 5/turn** |
| `openai_responses` (+ `_ws`) | Brodex | `{"type":"web_search"}` | `responses_common.rs:193` | none (uncapped) |
| `openai_chat` | vibe-bh (→ Mistral) | **nothing** | — | n/a — no search at all |

Provider→transport mapping is in `src/orchestration/brofile.rs`
(`resolve_provider_env`): GLM/DeepSeek/MiniMax → `anthropic` (`:380`), Brodex →
`responses` (`:543`), vibe-bh → `chat` + Mistral base URL (`:400`,`:546`).

### 1.1 The governance bypass (the real defect)

`web_search` is appended to the request **outside the registry**. The
`ToolFilter` (allow/deny CSV → `mcp.rs`) gates only registry tools (built-ins +
MCP). Therefore, today:

- `--allow-tools web_search` → **no effect**.
- `--deny-tools web_search` → **silently ignored** (a deny that does not deny).
- The only control is the all-or-nothing `BRO_HARNESS_WEB_SEARCH` env, on by
  default.

This is the one tool in the whole surface that sits in a side channel instead of
the one governance plane the surface evaluator / `ToolFilter` otherwise enforces
(cf. [`harness-daemon-boundary.md`](./harness-daemon-boundary.md) §6 — "the call
path must reject hidden tools by name"). Fixing this is a soundness requirement,
not polish: an operator who denies search must get it denied.

### 1.2 The flow (so "calling" search is understood)

The model does not call search through us. We emit the def; the *provider* runs
the search server-side and streams `server_tool_use` + result blocks back inline;
the harness passes them through (`anthropic.rs` parses `server_tool_use` as a
passthrough event, ~`:998`). We never dispatch it, never fill a `tool_result`.
That is exactly why it never touched the registry/filter — and exactly what
changes if we ever host our own backend (§4, Axis B), because *then* we execute
it as a real tool.

## 2. The three native shapes (vendor-verified, June 2026)

What each provider's API actually accepts — the menu the abstraction must be able
to project into. `[verified]` = checked against vendor docs this pass.

### 2.1 Anthropic (`anthropic` transport: GLM / DeepSeek / MiniMax) `[verified]`

Two tool versions exist:

- `web_search_20250305` — the basic tool. **This is what we pin today.**
- `web_search_20260209` — adds **dynamic filtering** (Claude writes/executes code
  to post-process results before they hit context; requires the code-execution
  tool). We do **not** use it.

Parameters (all optional):

| Param | Meaning | Our value today |
| --- | --- | --- |
| `max_uses` | cap searches per request; over → `max_uses_exceeded` error block. **No API default — omit = unlimited.** | hardcoded `5` |
| `allowed_domains` | allowlist results to these domains | unset |
| `blocked_domains` | blocklist domains | unset |
| `user_location` | `{type:"approximate", city, region, country, timezone}` localization | unset |

Pricing: $10 / 1,000 searches + the result tokens; usage reported as
`server_tool_use.web_search_requests`. Long server loops can stop with
`pause_turn` (continue by resending). **Caveat:** GLM/DeepSeek/MiniMax are
Anthropic-*compatible* endpoints — that they accept this exact def (and return
the canonical `web_search_tool_result`) is provider-specific and not guaranteed;
see §3 and OQ-3 (GLM is known to differ).

### 2.2 OpenAI Responses (`openai_responses` transport: Brodex) `[verified]`

| Param | Meaning | Our value today |
| --- | --- | --- |
| `type` | `"web_search"` (new) / `"web_search_preview"` (legacy) | `"web_search"` |
| `search_context_size` | `low`/`medium`/`high` — result-context depth | unset (provider default) |
| `return_token_budget` | `default`/`unlimited` — for long research runs | unset |
| `external_web_access` | bool, live fetching (default `true`) | unset |
| `filters.allowed_domains` / `filters.blocked_domains` | ≤100 each | unset |
| `user_location` | `{type:"approximate", country, city, region, timezone}` | unset |

So Brodex search is currently uncapped and at the provider-default context size —
none of these knobs surfaced.

### 2.3 OpenAI Chat Completions `web_search_options` `[verified, but see OQ-1]`

| Param | Meaning |
| --- | --- |
| `user_location.approximate.{country,city,region}` | localization only |

OpenAI's chat-completions search uses specialized search models
(`gpt-*-search-api`) that **always** search and lack domain filtering / context
sizing / live-access controls. **But our `openai_chat` transport points at
Mistral, not OpenAI** — so this OpenAI shape may not even be the relevant one for
vibe-bh. That is OQ-1.

### 2.4 Mistral (the actual vibe-bh backend) `[open — OQ-1]`

Mistral's web search has historically been an **Agents/Conversations API
connector** (`built-in/websearch`), *not* a plain chat-completions `tools`
param. As of June 2026 Mistral added "direct tool calling" making built-in
connectors available across model/agent calls — so it *may* now be reachable from
a chat-completions-shaped call, but in what exact wire shape (and whether it
matches `web_search_options` or needs the Conversations endpoint, which is a
different transport than `openai_chat`) is unverified. This is the crux open
question for vibe-bh.

## 3. The nuances this surfaces

- **`max_uses: 5` is self-imposed, and low.** The Anthropic default is
  *unlimited*; we chose 5 as a hardcoded literal. Fine as a cheap-turn guard,
  wrong as a global default for a research turn — and invisible/unconfigurable.
- **We're pinned to the old Anthropic tool version.** `web_search_20250305`, not
  `web_search_20260209` — so no dynamic filtering, which is exactly the
  token-reduction win NARF cares about elsewhere.
- **Responses is uncapped + default context.** Opposite posture to Anthropic's
  capped-at-5 for the *same* logical "enable search" intent. No `max_uses`
  equivalent exists on Responses; the nearest levers are `search_context_size` /
  `return_token_budget`, which we don't set.
- **Result shapes diverge even within one transport.** GLM returns a
  `web_search_prime` / non-canonical `tool_result` variant rather than Anthropic's
  `web_search_tool_result` (noted in
  [`backlog-transport-polish.md`](./backlog-transport-polish.md)). So "anthropic
  transport" ≠ "uniform Anthropic search" across GLM/DeepSeek/MiniMax — both
  *emission acceptance* and *result shape* may differ per provider. (OQ-3.)
- **vibe-bh has no search at all.** The chat transport emits nothing.
- **Governance bypass** (§1.1) — the deny that doesn't deny.
- **CLI providers are a separate world.** `claude`/`codex` get search from their
  own CLI config; we neither emit nor govern it. The abstraction should *say so*,
  not pretend to cover them. (OQ-7.)

## 4. The proposed abstraction — two axes + one plane

The bool conflates two orthogonal decisions; separate them.

### Axis A — emission *shape* (per-transport projection)

Replace the three inline `if opts.web_search { push(json!{...}) }` blocks with a
per-transport projection of one **normalized `SearchConfig`** (§5):

```rust
trait SearchEmit {                       // impl per transport
    /// Project the normalized config into THIS provider's native tool def,
    /// or None when search is disabled / unsupported by the provider.
    fn search_tool_def(&self, cfg: &SearchConfig) -> Option<serde_json::Value>;
}
```

- `anthropic`: maps `max_uses`/domains/location onto `web_search_20250305`
  (or `…20260209`) — and is the place a per-provider quirk (GLM's variant) is
  handled or normalized.
- `openai_responses`: maps onto `{type:web_search, search_context_size,
  filters.*, user_location, …}`.
- `openai_chat`: today a no-op; **filling this closes the vibe-bh hole** (pending
  OQ-1 on the Mistral shape).

This alone gives uniform behavior, kills the magic `5`, and surfaces the knobs.

### Axis B — execution *locus* (native vs hosted)

```rust
enum SearchBackend {
    ProviderNative,                    // Axis A: provider runs it server-side
    Hosted(Arc<dyn SearchProvider>),   // WE run it; uniform across all providers
}

trait SearchProvider {                 // Brave, Tavily, …, or our own
    async fn search(&self, q: &str, cfg: &SearchConfig) -> CapabilityResult<SearchResults>;
}
```

- **`ProviderNative`** (today's behavior): emit the native def (Axis A), provider
  executes, we pass through. Out-box, "free" (provider-billed), zero new code path
  beyond Axis A.
- **`Hosted`**: search stops being a transport tool-def and becomes a **real
  client-side tool the harness implements** (like `web_fetch` in
  `crates/bro-tools/src/web.rs`), uniform across *every* provider regardless of
  transport — and, critically, this is what gives **vibe-bh / any
  search-less provider** a search at all, and what a "Brave docs callouts"-style
  own-backend future needs. Results land via the normal tool path → bounded
  egress applies, and the [`backlog-transport-polish.md`] Brave bullet is its
  seed.

The two axes compose: a deployment can be all-native, all-hosted, or
native-where-available + hosted-fallback-where-not (the natural default once a
hosted backend exists — native for the providers that have a good one, hosted for
vibe-bh).

### Plane — fold search-enable back into governance (the §1.1 fix)

Whichever locus, **search-enable must flow through the same `ToolFilter` /
surface evaluator as every other tool**, so `deny web_search` is honored
uniformly:

- `ProviderNative`: before emitting the native def, check
  `filter.permits("web_search")`; a deny suppresses emission. (Cheap, and the
  whole fix for the native side.)
- `Hosted`: it's a registry tool, so it's gated automatically — same as any
  built-in.

This also means the existing `BRO_HARNESS_WEB_SEARCH` bool becomes one input to
`SearchConfig.enabled`, not a parallel authority.

## 5. The normalized `SearchConfig`

One config the harness owns; each transport projects it (Axis A) or the hosted
tool consumes it (Axis B):

```rust
struct SearchConfig {
    enabled: bool,                       // supersedes BRO_HARNESS_WEB_SEARCH
    backend: SearchBackend,              // native | hosted(provider)
    max_uses: Option<u32>,               // None = provider default (Anthropic: unlimited)
    context_size: Option<ContextSize>,   // low|medium|high (Responses; ignored where unsupported)
    allowed_domains: Vec<String>,        // ≤100 (Responses); Anthropic allowlist
    blocked_domains: Vec<String>,
    user_location: Option<UserLocation>, // {city, region, country, timezone}
    // version/dynamic-filtering selector for Anthropic — OQ-5
}
```

Projection is **lossy by design**: each transport takes what it supports and
drops the rest (e.g. `context_size` is meaningless on Anthropic; `max_uses` has
no Responses equivalent). The point is one authority + honest per-transport
projection, not a lowest-common-denominator. Where it lives (env? brofile?
fleet.json? per-dispatch?) is OQ-4.

## 6. Open questions

1. **OQ-1 (vibe-bh / Mistral) — the big one.** Does our `openai_chat` transport's
   Mistral endpoint expose web search via a chat-completions `tools` /
   `web_search_options`-shaped param now that Mistral has "direct tool calling,"
   or only via the Agents/Conversations API (a different endpoint than
   `openai_chat`)? If the latter, vibe-bh search needs *either* a new
   transport/endpoint shape *or* the Hosted backend (Axis B). Decide before
   committing Axis A for chat.
2. **OQ-2 (codex/OAI Responses parameters).** Which Responses knobs do we
   actually surface — just `enabled`, or `search_context_size` /
   `return_token_budget` / `filters` / `user_location` too? `search_context_size`
   has real cost/quality impact for Brodex; the rest may be premature. (Note:
   `codex` the CLI is OQ-7, distinct from Brodex the Responses transport.)
3. **OQ-3 (GLM result divergence + emission acceptance).** Does GLM accept
   Anthropic's `web_search_20250305` def, or need its own (`web_search_prime`)?
   And do we normalize GLM's result variant into the canonical
   `web_search_tool_result` (the backlog "server result normalization" bullet) or
   relay verbatim? Same question latent for MiniMax. Verbatim is simpler and has
   worked; normalize only on evidence of model confusion.
4. **OQ-4 (config surface + scope).** Where does `SearchConfig` live and at what
   granularity — process env (today), brofile per-provider, fleet.json,
   per-dispatch override? Likely brofile-default + per-dispatch override, but
   unsettled. Interacts with the §1.1 governance fold-in (ToolFilter is already
   per-dispatch).
5. **OQ-5 (Anthropic tool version).** Adopt `web_search_20260209` (dynamic
   filtering) — and if so, gate on code-execution-tool availability and model
   support — or stay on `…20250305`? Dynamic filtering is the token-reduction win;
   the dependency is the cost.
6. **OQ-6 (hosted backend choice + in-box eligibility).** Which `SearchProvider`
   first — Brave (pg_recon's pick, paid key), Tavily, our own? And: a *hosted*
   exact-ish search could later earn an in-box `search.web` NARF binding
   (`narf-tool-placement.md` §2.1) — but only if results are trustable without
   model judgment, which ranked web search generally is *not*. Default: stays
   out-box; revisit per real evidence.
7. **OQ-7 (CLI providers).** `claude`/`codex`/Inception search is configured by
   their own CLIs, not our request builder. Confirm we explicitly scope them OUT
   (document-only), rather than leaving a false impression the abstraction governs
   them.
8. **OQ-8 (defaults + posture).** Should `enabled` stay default-ON? Should there
   be a sane default `max_uses` for native Anthropic (vs today's 5 / vs
   unlimited), and a default `context_size` for Responses? A single posture across
   transports, or per-transport tuned?

## 7. Relationship

- **Subsumes** the [`backlog-transport-polish.md`](./backlog-transport-polish.md)
  "client-side `web_search` fallback backend" and "server result normalization"
  bullets — this is their fuller treatment; retire them there when this lands.
- **Orthogonal to** [`narf-tool-placement.md`](./narf-tool-placement.md): web
  search is out-box there; this doc owns *how the out-box search is configured and
  executed across transports*, and the hosted backend would be the one case that
  could later cross into in-box (OQ-6).
- **Extends** [`anthropic-harness.md`](./anthropic-harness.md) /
  [`brodex-responses-deep-dive.md`](./brodex-responses-deep-dive.md) /
  [`bro-harness-tool-surface.md`](./bro-harness-tool-surface.md) with a single
  search-config + emission seam in place of the scattered inline bool→json.
- **Honors** [`harness-daemon-boundary.md`](./harness-daemon-boundary.md) §6: one
  governance plane — search-enable goes through the same filter/surface as every
  other tool (§4 Plane).
