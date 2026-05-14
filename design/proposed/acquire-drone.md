# acquire_drone - pooled ad hoc bro dispatch for external LLM sessions

Date: 2026-05-14
Status: superseded design proposal

Superseded by `design/proposed/runtime-allocation-tier-mapping.md`.

The probe taxonomy, account health fusion, cooldown model, selection trace
shape, and session/account/cwd continuity notes remain useful reference
material. The dedicated `acquire_drone` MCP surface and drone-specific public
vocabulary are superseded: pooled workers should be ordinary brofiles, agents,
atoms, or workflow actors with runtime allocation defaults.

## 1. Problem

External LLM sessions need a cheap way to ask Blackbox for "the next available
worker" without knowing which provider account is currently healthy, which model
slug belongs to that provider, or which account-specific environment variables
must be set.

Today the closest primitive is `bro_exec` in ad hoc provider mode:

```text
bro_exec(provider="codex", prompt="...", project_dir="/repo/x")
```

That is too thin for external callers. It only chooses a provider named by the
caller and resolves the provider default account. It does not pick across an
account pool, does not apply the drone model/effort mapping, and does not expose
why one account was selected over another.

The desired primitive is:

```text
acquire_drone(prompt="...", pool=["codex", "claude", "glm"], project_dir="/repo/x")
```

The daemon selects the next-most-available account/provider lane, fills in
provider, account, model, effort, env, and dispatch filters, then starts a
normal bro task. Callers continue with the existing `bro_resume(session_id,
provider, ...)` pattern.

## 2. Non-goals

- No new provider runtime.
- No new continuation protocol for callers.
- No team instance required for pooled drones.
- No credential broker in v1. Blackbox continues using local account env
  synthesis and configured account env overrides.
- No claim that all providers expose real quota. Some only support credential
  freshness, active acceptance checks, or no quota probe at all.

## 3. Shape

`acquire_drone` is a veneer over the same spawn path as `bro_exec`, but with a
pool selector before dispatch.

```text
caller
  -> acquire_drone
     -> resolve drone pool entries
     -> read cached fused account state
     -> choose account/provider lane
     -> resolve model/effort mapping
     -> call bro_exec-style spawn
     -> persist session -> account binding
  <- {taskId, sessionId, provider, account, model, effort, selectionTrace}
```

It should reuse existing bro execution machinery:

- `Provider::build_exec_args` — same CLI arg construction as ad hoc exec.
- `resolve_provider_env(provider, account, model, store_dir)` — the drone
  selector picks the `(provider, account, model)` tuple; the existing env
  synthesis function produces the process env from that tuple unchanged.
- ambient prompt + completion contract handling.
- MCP filter resolution and recursion guard.
- task store, tail events, and `bro_report`.

The drone selector is **not** a new dispatch path. It is a pre-dispatch decision:
expand the pool into `(provider, account)` lanes, score them with fused
probe+runtime state, pick one, then feed the winner's `(provider, account, model,
effort)` into the existing `resolve_provider_env` / `build_exec_args` / `spawn_task`
chain. Every spawn argument already exists — the selector just picks which tuple
to use.

The new code should live beside bro dispatch rather than inside teams. Teams are
named conversational ensembles. A drone acquisition is a one-off pooled worker
lease.

## 4. Public MCP tools

### `acquire_drone`

Starts a fresh pooled worker task.

Inputs:

```json
{
  "prompt": "string",
  "pool": ["codex", "claude", "glm"],
  "project_dir": "/repo/x",
  "pool_name": "default",
  "strategy": "least_utilized | round_robin | weighted",
  "allow_recursion": false,
  "allow_tools": [],
  "disallow_tools": [],
  "surface": "agent-internal",
  "coerce_workspace": true
}
```

`pool` is a provider alias set. Precedence: explicit `pool` wins; if absent,
resolve `pool_name` from the drone config's `named_pools` map; if that is also
absent, use `default_pool`. `pool` and `pool_name` are mutually narrowing:
providing both means "intersect the explicit providers with the named pool's
provider set." This is unlikely in practice but must be defined.

`allow_tools` and `disallow_tools` are overlays, not replacements for the
mechanical recursion guard. They merge with the guard's deny-`bro_*` list and
any brofile-level filters using the same filter resolution path as `bro_exec`.

Default `strategy` should be `weighted`; the other strategies are operator/debug
modes.

`coerce_workspace` (default `true`) forces the spawned bro task into the
caller's `project_dir` regardless of what cwd the provider binary would
otherwise infer. Set `false` only when the caller wants the drone to inherit
the account's default working directory (rare — almost always keep `true`).

Outputs:

```json
{
  "taskId": "uuid",
  "sessionId": "provider-session-id-or-pending",
  "status": "running",
  "provider": "codex",
  "account": "account3",
  "model": "gpt-5.3-codex-spark",
  "effort": null,
  "pool": "default",
  "selectionTraceId": "drone-select-...",
  "resume": {
    "tool": "bro_resume",
    "provider": "codex",
    "session_id": "..."
  }
}
```

The returned `resume` object is documentation, not a new protocol. The daemon
must make the existing `bro_resume(session_id, provider)` path account-aware by
looking up a persisted drone session binding.

### `drone_pool_status`

Read-only operator/caller view of known pool state.

Inputs:

```json
{
  "pool": ["codex", "claude"],
  "pool_name": "default",
  "include_probe_evidence": true
}
```

Outputs include every candidate account with provider, model/effort mapping,
last probe status, utilization, in-flight count, max concurrency, selectable
flag, capacity_score components (preference, quota, concurrency, cooldown),
exclusion reason, and probe staleness (seconds since last_probe_at). When
`include_probe_evidence=true`, also returns sanitized `raw_summary`,
`quota_confidence`, and the last error/last usage summaries.

### `drone_probe`

Refreshes or reads probe state.

Inputs:

```json
{
  "provider": "codex",
  "account": "account3",
  "mode": "read | refresh",
  "include_raw": false
}
```

`mode=read` returns cached state only. `mode=refresh` performs the provider
probe if the account allows active probing.

### `drone_selection_trace`

Returns a previous acquisition's selection packet.

Inputs:

```json
{
  "selectionTraceId": "drone-select-..."
}
```

This is the snoop surface for "why did it pick this one?"

## 5. Configuration

Add a drone config under the existing bro store config rather than inventing a
separate store.

```json
{
  "accounts": {
    "account2": {"env": {"CLAUDE_CONFIG_DIR": "/home/me/.claude-account2"}},
    "codex2": {"env": {"CODEX_HOME": "/home/me/.codex-account2"}}
  },
  "provider_defaults": {},
  "drone": {
    "default_pool": ["glm", "claude", "codex", "deepseek", "gemini", "vibe"],
    "named_pools": {
      "coding": ["glm", "claude", "codex", "deepseek"],
      "any": ["glm", "claude", "codex", "deepseek", "gemini", "vibe"]
    },
    "preference_order": ["glm", "claude", "codex", "deepseek", "gemini", "vibe"],
    "provider_weights": {
      "glm": 1.0,
      "claude": 0.82,
      "codex": 0.68,
      "deepseek": 0.55,
      "gemini": 0.45,
      "vibe": 0.25
    },
    "providers": {
      "codex": {
        "model": "gpt-5.3-codex-spark",
        "effort": null,
        "accounts": ["default", "account2", "account3"],
        "probe": "usage-endpoint"
      },
      "claude": {
        "model": "claude-sonnet-4-6",
        "effort": "high",
        "accounts": ["default", "account2", "account3"],
        "probe": "rate-limit-headers"
      },
      "glm": {
        "model": "glm-5.1",
        "effort": null,
        "accounts": ["default"],
        "probe": "zai-usage-endpoint"
      },
      "deepseek": {
        "model": "deepseek-v4-flash",
        "effort": null,
        "accounts": ["default"],
        "probe": "deepseek-balance"
      },
      "gemini": {
        "model": "gemini-3-flash-preview",
        "effort": null,
        "accounts": ["default"],
        "probe": "credential-freshness"
      },
      "vibe": {
        "model": null,
        "effort": null,
        "accounts": ["default"],
        "probe": "none"
      }
    },
    "max_concurrent_per_account": 1,
    "max_concurrent_overrides": {},
    "quota_capacity_defaults": {
      "payg_available": 0.95,
      "active_probe_success": 0.95,
      "credential_only": 0.25,
      "none": 0.10
    },
    "payg_balance_ceiling_usd": 10.00,
    "payg_min_balance_usd": 0.20,
    "probe_cache_ttl_seconds": 300,
    "probe_refresh_interval_seconds": 600,
    "cooldown_duration_seconds": 900,
    "spawn_failure_cooldown_seconds": 300,
    "weekly_ceiling": 0.95,
    "max_selection_traces": 1000
  }
}
```

Starter model mappings should match the existing `drones` team template
brofiles:

| Provider | Model | Effort |
|---|---|---|
| `codex` | `gpt-5.3-codex-spark` | `null` |
| `claude` | `claude-sonnet-4-6` | `high` |
| `glm` | `glm-5.1` | `null` |
| `deepseek` | `deepseek-v4-flash` | `null` |
| `inception` | `inception/mercury-2` | `null` |
| `gemini` | `gemini-3-flash-preview` | `null` |
| `vibe` | provider default | `null` |

The config may alternatively point at brofiles for mappings, but acquisition
should still dispatch as ad hoc. Brofiles are a convenient source of provider,
model, effort, lens, filter, and workspace-coercion defaults, not a requirement
to instantiate a team member.

## 6. Probe mechanisms from Daystrom

This section remains donor material for
`design/proposed/runtime-allocation-tier-mapping.md`. The `acquire_drone` MCP
surface is superseded, but the probe taxonomy, quota-confidence vocabulary,
cooldown behavior, and runtime observation fusion below are still intended to
feed the allocator design.

Daystrom already has the useful taxonomy and several concrete probes. These are
not the whole accounting model. They seed and periodically refresh account
state, then live provider responses from actual bro calls update the same
runtime picture.

Blackbox should adopt the mechanism names and provider behavior, then fuse probe
observations with runtime observations from spawned drone tasks.

| Mechanism | Providers | Evidence | Selection fields |
|---|---|---|---|
| `rate-limit-headers` | Claude | Minimal Anthropic `https://api.anthropic.com/v1/messages` call with Haiku probe model, `anthropic-version: 2023-06-01`, and `anthropic-beta: oauth-2025-04-20`; parse `anthropic-ratelimit-unified-5h-utilization`, `anthropic-ratelimit-unified-7d-utilization`, `anthropic-ratelimit-unified-status`, reset, overage status, and overage utilization headers. | `five_hour_utilization`, `seven_day_utilization`, `status`, `resets_at`, `overage_*` |
| `usage-endpoint` | Codex | Read `auth.json` tokens and call `https://chatgpt.com/backend-api/wham/usage` with bearer token and optional `ChatGPT-Account-Id`; parse `rate_limit.primary_window.used_percent`, `primary_window.reset_at`, `secondary_window.used_percent`, `allowed`, `limit_reached`, and `plan_type`. | `five_hour_utilization`, `seven_day_utilization`, `status`, `resets_at`, `plan` |
| `credential-freshness` | Gemini | Read `oauth_creds.json` from the Gemini home directory selected by account env, confirm `access_token`, and compare `expiry_date` milliseconds to now. Daystrom notes no public quota API. Drone-acquired Gemini sessions must store cwd in the drone registry (Section 8), because cwd/session lookup is provider-specific and should not be rediscovered on resume. | `credential_status`, `expires_at`; quota utilization is unknown |
| `zai-usage-endpoint` | GLM/Z.AI Coding Plan via Claude Code custom model config | Read `ANTHROPIC_AUTH_TOKEN` from the selected Claude config dir (`~/.claude-zai/settings.json` for the default account) and call `https://api.z.ai/api/monitor/usage/quota/limit` with `Authorization: <key>`, `Accept-Language: en-US,en`, and `Content-Type: application/json`. Parse `data.limits[]`: `type=TOKENS_LIMIT, number=5, unit=3` is the five-hour window; `type=TOKENS_LIMIT, number=1, unit=6` is the weekly/seven-day window. Use `percentage` as utilization and `nextResetTime` milliseconds as reset. If the quota endpoint fails, fall back to `glm-active-probe` behavior for launchability and error-code classification. | `five_hour_utilization`, `seven_day_utilization`, `resets_at`, `plan_level`, `provider_cooldown_until` |
| `glm-active-probe` | GLM/Z.AI via Claude Code · Inception via OpenCode | Fallback probe for GLM/Z.AI when the usage endpoint cannot be called, and primary probe for Inception. Run a minimal provider invocation against the configured model with the selected account env. A success proves the account is currently accepted for work but does not reveal a utilization percentage. Parse provider error codes into `quota_status`, `resets_at`, and cooldown fields (see error-code → state mapping table below). | `credential_status`, `quota_status`, `resets_at`, `provider_cooldown_until`; quota utilization is unknown on success |
| `deepseek-balance` | DeepSeek via Claude Code custom model config | Call `https://api.deepseek.com/user/balance` with `ANTHROPIC_AUTH_TOKEN` from the selected Claude config dir (`~/.claude-ds/settings.json` for the default account) as `Authorization: Bearer <key>`. `is_available=false` marks the account unavailable; `is_available=true` with positive `balance_infos[0].total_balance` proves pay-as-you-go availability but does not map to 5h/7d utilization. If no direct token is extractable, fall back to a minimal Claude Code invocation and treat success as `active_acceptance`, not `payg_balance`. | `credential_status`, `quota_status`, `balance_available`, `balance_total`, `balance_currency`; quota utilization is unknown |
| `file-presence` | OpenCode fallback only | Check provider account auth file presence when no active provider probe is configured or extractable. This is a launchability preflight, not a quota signal. | `credential_status`; quota utilization is unknown |
| `none` | Vibe or unsupported providers | No active probe. Select only by task in-flight count and failure cooldown. | `status=unknown`; quota utilization is unknown |

Claude Code custom-model config stores the default GLM and DeepSeek credentials
under `~/.claude-zai/settings.json` and `~/.claude-ds/settings.json`
respectively. Each config supplies `ANTHROPIC_BASE_URL`,
`ANTHROPIC_AUTH_TOKEN`, and default model env vars. Existing OpenCode auth
(`~/.local/share/opencode/auth.json` keys `zai-coding-plan` and `deepseek`) can
remain a probe-token fallback, but dispatch should prefer the Claude config dirs
for consistent JSONL/session behavior.

Z.AI source anchors:

- The official Z.AI `glm-plan-usage` plugin documents the user-facing
  `/glm-plan-usage:usage-query` command for quota and usage statistics.
- `zai-org/zai-coding-plugins` script
  `plugins/glm-plan-usage/skills/usage-query-skill/scripts/query-usage.mjs`
  calls `https://api.z.ai/api/monitor/usage/model-usage`,
  `https://api.z.ai/api/monitor/usage/tool-usage`, and
  `https://api.z.ai/api/monitor/usage/quota/limit`. For allocator probes,
  `quota/limit` is the normalized quota source.
- The script currently post-processes every `TOKENS_LIMIT` row as
  `Token usage(5 Hour)`. Do not copy that label literally; discriminate by
  `number` and `unit` so the weekly/seven-day row is not mislabeled.

DeepSeek source anchors:

- The official DeepSeek API reference documents `GET /user/balance` under
  `https://api.deepseek.com`, returning `is_available` and `balance_infos[]`
  with `currency`, `total_balance`, `granted_balance`, and
  `topped_up_balance`.

Daystrom source anchors:

- `../daystrom-mk2/src/Daystrom.Core/Auth/ProviderProbeMechanism.cs` defines
  `none`, `rate-limit-headers`, `usage-endpoint`, `credential-freshness`, and
  `file-presence`.
- `../daystrom-mk2/src/Daystrom.Core/Auth/TransitionalProviderDefaults.cs`
  maps providers to account-home env vars: Claude `CLAUDE_CONFIG_DIR`, Codex
  `CODEX_HOME`, Gemini `HOME` plus `GEMINI_CLI_NO_RELAUNCH=true`, and GLM
  `XDG_DATA_HOME`.
- `../daystrom-mk2/src/Daystrom.Worker/Services/AccountProbeService.cs`
  contains the concrete Claude, Codex, Gemini, and GLM probe implementations.
- `../daystrom-mk2/src/Daystrom.Worker/Services/AccountBalancer.cs` contains
  useful selection mechanics to reuse: active-only, weekly ceiling, max
  concurrent leases, tier/provider filters, utilization sort, and in-flight
  tie-break. Do not copy its file-presence-as-zero-utilization behavior.
- `../daystrom-mk2/src/Daystrom.AgentSdk/Providers/AgentMessage.cs` defines
  normalized provider events: `RateLimit`, `UsageUpdate`, and `Completed`
  metrics.
- `../daystrom-mk2/src/Daystrom.AgentSdk/Providers/AgentSession.cs`
  accumulates per-turn `UsageUpdate` events and fixes up completed session
  metrics before yielding the terminal `Completed` message.
- `../daystrom-mk2/src/Daystrom.Worker/Services/AgentInstance.cs` releases the
  selected account lease with the completed message and last rate-limit event.
- `../daystrom-mk2/src/Daystrom.Worker/Services/AgentSessionRecorder.cs`
  records final token usage, cost, model, duration, and rate-limit details to
  the graph session record.

### Runtime observation fusion

Drone account state has three input streams:

1. **Startup probe** - initial account health/utilization snapshot at daemon boot.
2. **Periodic probe** - background refresh on a schedule
   (`probe_refresh_interval_seconds`, default 600s). Independent of cache TTL.
3. **Provider-response observations** - normalized messages emitted by actual
   bro calls.

### Probe freshness and acquisition behavior

Probe state has two time fields: `last_probe_at` (when the probe actually ran at
the provider) and `probe_cache_ttl_seconds` (how old a cached result can be
before it is considered stale).

`acquire_drone` **never blocks on probe refresh**. It uses cached state only. If
a lane's `last_probe_at` is older than `probe_cache_ttl_seconds`, the lane is
marked `probe_stale=true` and its capacity score uses the stale data but the
selection trace records the staleness. Stale probes do **not** cause exclusion —
they are just lower-confidence. Only severely stale data, such as multiple
missed refresh intervals, should degrade to `quota_status=probe_failed`.

`drone_probe(mode="refresh")` is the explicit synchronous refresh path.
Background periodic probes run on a separate timer and are fire-and-forget.

### Cooldown triggers

Cooldown is per-(provider, account) and sets `cooldown_capacity=0.0` for
`cooldown_duration_seconds`. Cooldown triggers:

- **Spawn failure**: the subprocess failed to launch (binary missing, env error,
  immediate exit). Duration: `spawn_failure_cooldown_seconds` (default 300s,
  shorter than generic cooldown because this can be transient).
- **Provider-exhausted error**: a `RateLimit` event with `status=rejected` or
  `quota_status=exhausted` with a known future `resets_at`. Cooldown until reset.
- **Probe failure**: `quota_status=probe_failed`. Generic cooldown duration.
- **Consecutive task failures**: after `max_consecutive_failures` (default 3)
  consecutive NonZero (non-rate-limit, non-session-fork) task failures within a
  rolling window, apply generic cooldown.

Cooldown is separate from hard exclusion. A hard-excluded lane (missing creds,
disabled, expired) is not selectable; cooldown makes a lane score zero but still
selectable as a last resort in `round_robin` when no candidate has positive
score.

### Provider-response observation handling

- `RateLimit` events update selectable account state. If the event identifies a
  five-hour or seven-day limit, update the matching utilization field. If the
  type is unknown, conservatively raise both utilization fields to at least the
  observed value. A rejected status marks the account exhausted until reset.
- `UsageUpdate` events accumulate token consumption for the session. They do not
  replace provider quota probes, but they should be stored on the drone session
  and selection trace so operators can correlate account pressure with actual
  task cost.
- `Completed` metrics finalize the session accounting: input/output/cache
  tokens, cost, model, duration, turn count, success/failure, and any terminal
  rate-limit fields.

### Probe confidence

Eligibility and utilization are separate fields.

Credential-only probes (`credential-freshness`, `file-presence`, `none`) can
prove that an account is launchable or not launchable. They do not prove spare
quota. They must never write `five_hour_utilization=0` or
`seven_day_utilization=0` merely because credentials exist.

Represent that explicitly:

- `credential_status`: `present | missing | expired | unknown`
- `quota_status`: `known | unknown | exhausted | probe_failed`
- `five_hour_utilization`: number only when quota is known
- `seven_day_utilization`: number only when quota is known
- `quota_confidence`:
  `quota_probe | runtime_rate_limit | payg_balance | active_acceptance | credential_only | none`

This prevents unknown-quota OpenCode accounts from outranking quota-aware Claude
or Codex accounts just because the OpenCode auth file exists.

### Synthetic capacity

Provider limits are not the same shape:

- Claude exposes utilization percentages for unified five-hour and seven-day
  windows.
- Codex exposes primary and secondary window usage percentages.
- GLM/Z.AI Coding Plan exposes five-hour and seven-day utilization via
  `https://api.z.ai/api/monitor/usage/quota/limit`.
- DeepSeek exposes pay-as-you-go balance availability, not a rolling window.
- Inception, and GLM/Z.AI when the usage endpoint is unavailable, expose hard
  failures and reset times through active calls, but no successful-call
  percentage.
- Gemini and Vibe may only expose launchability.

Each mechanism maps to quota_capacity via a fixed, mechanical derivation table
(ordered — the first matching mechanism sets the calculation):

| Mechanism | quota_confidence | quota_capacity formula |
|---|---|---|
| `rate-limit-headers` · `usage-endpoint` (probe success) | `quota_probe` | `1.0 - max(5h, 7d)` |
| `rate-limit-headers` · `usage-endpoint` (probe failed, runtime data exists) | `runtime_rate_limit` | `1.0 - max(5h, 7d, runtime_observed)` |
| `zai-usage-endpoint` (probe success) | `quota_probe` | `1.0 - max(5h, 7d)` |
| `deepseek-balance` (`is_available=true`, balance known) | `payg_balance` | `min(1.0, balance / ceiling) * payg_available_multiplier` |
| `deepseek-balance` (direct token unavailable, minimal OpenCode call succeeds) | `active_acceptance` | `active_probe_success` bucket |
| `glm-active-probe` (success) | `active_acceptance` | `active_probe_success` bucket |
| `zai-usage-endpoint` · `glm-active-probe` · `deepseek-balance` (probe failed) | — | lane excluded by cooldown, not scored |
| `credential-freshness` (present, not expired) | `credential_only` | `credential_only` bucket |
| `file-presence` · `none` | `credential_only` · `none` | `credential_only` or `none` bucket |

Selection should convert all probe/runtime evidence into a synthetic
`capacity_score` rather than comparing raw fields directly.

```text
capacity_score = provider_preference_weight
               * quota_capacity
               * concurrency_capacity
               * cooldown_capacity
```

Where:

- `provider_preference_weight` comes from `provider_weights` or the
  `preference_order` fallback. This is the steering knob for "favor GLM, then
  Claude, then Codex, then DeepSeek, then Gemini, then Vibe".
- `quota_capacity` is `1.0 - max(five_hour_utilization, seven_day_utilization)`
  when utilization is known. GLM/Z.AI Coding Plan should use real utilization
  from `zai-usage-endpoint`; only fallback active-probe success uses the
  configured `active_probe_success` bucket. Credential-only and no-probe
  providers use the lower `credential_only` or `none` buckets.

  DeepSeek PAYG: when `is_available=true` and `balance_available` is known,
  compute a balance-scaled capacity rather than using a flat bucket:

  ```text
  quota_capacity = min(1.0, balance_available / payg_balance_ceiling_usd)
                 * configurable payg_available multiplier
  ```

  This makes a $50 balance score higher than a $0.50 balance. Accounts with
  `balance_available < payg_min_balance_usd` are hard-excluded (not just
  scored lower) — a near-zero balance is effectively exhausted. A failed balance
  endpoint is **not** enough evidence for the PAYG bucket; mark
  `quota_status=probe_failed` and cool the lane. Only a valid balance response
  with `is_available=true` may produce `quota_confidence=payg_balance`. If the
  direct balance token is unavailable but a minimal OpenCode DeepSeek call
  succeeds, use `quota_confidence=active_acceptance` and the
  `active_probe_success` bucket instead.
- `concurrency_capacity` is `1.0 - in_flight / max_concurrent_per_account`,
  clamped to `[0.0, 1.0]`.
- `cooldown_capacity` is `0.0` during provider/account cooldown, otherwise
  `1.0`.

Hard exclusions still run before scoring: missing credentials, expired
credentials, exhausted quota with a future reset, disabled accounts, missing
provider binaries, and maxed concurrency are not candidates. Weights only steer
among eligible lanes; they do not resurrect broken accounts.

With the sample defaults, an active GLM account scores `1.0 * 0.95 = 0.95`
before concurrency/cooldown, while a completely unused Claude account scores
`0.82 * 1.0 = 0.82`. That makes "favor GLM, then Claude" real. A
credential-only GLM account scores only `1.0 * 0.25 = 0.25`, so blind auth-file
presence still does not beat real capacity evidence.

The selection trace should include each score component so operators can tell
whether a choice came from provider preference, quota pressure, concurrency, or
cooldown.

Probe records should store:

```json
{
  "provider": "codex",
  "account": "account3",
  "mechanism": "usage-endpoint",
  "credential_status": "present",
  "quota_status": "known | unknown | exhausted | probe_failed",
  "quota_confidence": "quota_probe | runtime_rate_limit | payg_balance | active_acceptance | credential_only | none",
  "five_hour_utilization": 0.21,
  "seven_day_utilization": 0.44,
  "overage_utilization": 0.0,
  "resets_at": 1770000000,
  "expires_at": null,
  "balance_available": null,
  "balance_total": null,
  "balance_currency": null,
  "in_flight": 1,
  "last_probe_at": "2026-05-14T00:00:00Z",
  "last_runtime_observation_at": "2026-05-14T00:04:30Z",
  "last_usage": {
    "input_tokens": 12000,
    "output_tokens": 1800,
    "cache_read_tokens": 0,
    "cache_creation_tokens": 0,
    "cost_usd": 0.42,
    "model": "gpt-5.3-codex-spark",
    "task_id": "..."
  },
  "last_error": null,
  "raw_summary": {
    "plan": "plus",
    "headers_seen": ["..."]
  }
}
```

Do not persist raw bearer tokens, full auth files, or full provider responses.
`include_raw=true` may expose sanitized probe evidence and sanitized runtime
event summaries only.

## 7. Selection algorithm

For `least_utilized`:

1. Expand provider aliases into `(provider, account, model, effort)` lanes.
2. Drop missing provider binaries.
3. Drop disabled accounts.
4. Drop exhausted or expired accounts unless the reset/expiry is now in the
   past and a refresh succeeds.
5. Drop credential-missing accounts.
6. Drop accounts at `max_concurrent_per_account`.
7. Drop quota-known accounts above `weekly_ceiling`.
8. Partition candidates by quota confidence:
   - `known`: quota probe or runtime rate-limit observation has real utilization.
   - `synthetic`: pay-as-you-go balance, active acceptance, credential-only, or
     no-probe evidence; launchable but quota is not directly comparable.
9. If any `known` candidates remain, sort them by:
   - lower `five_hour_utilization`
   - lower `in_flight`
   - older `last_selected_at`
   - provider/account stable name for deterministic tie-break
10. Use `synthetic` candidates only when:
    - the caller explicitly pinned synthetic-quota providers, or
    - all known-quota candidates are unavailable.
11. Sort `synthetic` candidates by:
   - higher synthetic quota bucket: `payg_available`, then
     `active_probe_success`, then `credential_only`, then `none`
   - lower `in_flight`
   - no recent launch or provider failure cooldown
   - older `last_selected_at`
   - provider/account stable name for deterministic tie-break
12. Acquire under a daemon-local mutex and increment in-flight before spawning.
13. If spawn fails, roll back in-flight and write the failure to the probe state.

For `weighted`, use the same eligibility filters (hard exclusions run first:
missing creds, exhausted quota with future reset, disabled accounts, missing
binaries, maxed concurrency). Then calculate `capacity_score` for every
surviving candidate and pick the highest score. The `capacity_score` formula
already handles the known-vs-synthetic distinction through `quota_capacity`:
known-quota providers use real utilization percentages; PAYG-balance providers
scale with remaining balance (see below); credential-only and no-probe providers
use the configured `credential_only`/`none` buckets. No separate known/synthetic
partition is needed because the score penalty is built in.

Tie-break by older `last_selected_at`, lower `in_flight`, then stable
provider/account name.

For `round_robin`, use the same eligibility filters but choose the next lane by
pool cursor. Round-robin should still skip candidates with `capacity_score=0.0`
unless every candidate in the caller's pinned pool is unknown/blind.

### GLM/Z.AI quota and hard-limit mapping

The preferred Z.AI Coding Plan probe calls `quota/limit` and stores the
five-hour and seven-day `percentage` values directly. The `nextResetTime` value
is milliseconds since Unix epoch. Store the five-hour reset separately from the
weekly reset when both are present, and set the generic `resets_at` to the reset
for the dominant utilization window used for scoring.

GLM active probes are still needed when the usage endpoint cannot be called, and
for provider errors emitted by real tasks. Active probes only learn a quota limit
when the call is rejected. The probe must parse provider error codes into
exhaustion state for the selection eligibility filter at step 4:

| GLM/Z.AI signal | quota_status | resets_at |
|---|---|---|
| usage limit reached, including business code `1308` | exhausted | provider `next_flush_time` when present; otherwise now + remaining-day-seconds |
| weekly/monthly exhausted, including business code `1310` | exhausted | provider `next_flush_time` when present; otherwise end-of-period heuristic |
| package expired, including business code `1309` | exhausted | null (requires operator renewal) |
| unsupported model, including business code `1311` | exhausted | null (permanent for this account/model mapping) |
| temporary rate limit, including HTTP `429` or business code `1305` | known, temporarily blocked | retry-after header or configurable cooldown |
| high traffic / model degraded, including business code `1312` | known, temporarily blocked | configurable cooldown |
| unavailable account/balance/auth rejection, including business codes `1113` or `1000`-`1004` | exhausted or credential-missing | null unless provider supplies reset |
| active-probe success | unknown (no utilization %) | null |

Hard exclusions (step 4) apply to `exhausted` with `resets_at` in the future or
`resets_at=null` (permanent exhaustion). Temporarily-blocked accounts are not
hard-excluded; they go through cooldown scoring. Active-probe success with
`quota_status=unknown` follows the `active_probe_success` synthetic capacity
bucket only when there is no fresh `zai-usage-endpoint` record. It proves the
account works now but says nothing about remaining quota.

## 8. Session/account continuity

This is load-bearing.

Provider sessions live under provider account homes. A Codex session created
with `CODEX_HOME=/home/me/.codex-account2` may not be resumable from the default
`CODEX_HOME`. The same applies to Claude config dirs and Gemini homes.

`acquire_drone` must persist:

```json
{
  "session_id": "...",
  "task_id": "...",
  "provider": "codex",
  "account": "account2",
  "model": "gpt-5.3-codex-spark",
  "effort": null,
  "project_dir": "/repo/x",
  "cwd": "/repo/x",
  "created_at": "...",
  "last_seen_at": "..."
}
```

Then `bro_resume(session_id, provider, ...)` should check this registry before
falling back to provider-default env. This keeps the caller-visible resume
pattern unchanged while preserving the selected account behind the scenes.

Consultation order for raw `session_id + provider` resume:

1. Parse provider from the raw-provider string.
2. Look up `session_id` in `drone/sessions.json` (the drone session registry).
3. If found: use the stored account, model, effort, **and cwd** to drive
   `resolve_provider_env` and the resume spawn. The registry must store the
   working directory used at spawn time because cwd/session lookup is
   provider-specific and not always recoverable from a raw provider session id.
   Without cwd in the registry, drone-acquired sessions can be unresumable or
   can resume in the wrong workspace.
4. If not found in drone registry: fall back to existing behavior —
   `resolve_provider_env(provider, None, None, ...)` with no account/model
   and caller-supplied or auto-resolved cwd.
5. If no safe cwd can be resolved, refuse the resume with a descriptive error
   rather than silently starting a continuation in the wrong workspace.

Named bro/team resumes keep their existing behavior. The drone session registry
only applies when the caller resumes by raw `session_id + provider`.

## 9. State and observability

Durable state:

- `drone/probes.json` or sharded records under `drone/probes/`
- `drone/sessions.json` for session/account bindings
- `drone/selection-traces/` for recent selection packets

Task completion must decrement `in_flight` for the selected lane. The terminal
task path should merge the last known `RateLimit` event and completed usage
metrics into the drone account/session state, same as Daystrom releases an
account lease with the completed message and last rate-limit event.

Tail events should include `provider`, `account`, `model`, `effort`, and
`selectionTraceId` when the task came from `acquire_drone`.

## 10. Implementation sketch

New modules:

- `src/orchestration/drone.rs` — config structs, probe state, selection, session
  registry.
- `src/tools/drones.rs` — MCP tools (`acquire_drone`, `drone_pool_status`,
  `drone_probe`, `drone_selection_trace`).

Key function signatures:

```rust
// Orchestration layer — pre-selects (provider, account, model, effort) from
// a pool, then delegates to the existing bro_exec spawn chain.
pub fn acquire_drone(
    prompt: &str,
    pool: &[Provider],
    pool_name: Option<&str>,
    strategy: Strategy,
    project_dir: &Path,
    store_dir: &Path,
    drone_state: &DroneState,
    task_store: &TaskStore,
    tail_tx: &Sender<TailEvent>,
) -> Result<DroneAcquisition, DroneError>;

// Probe implementations — one per mechanism.
fn probe_claude_rate_limit(account: &str) -> Result<ProbeRecord>;
fn probe_codex_usage(account: &str) -> Result<ProbeRecord>;
fn probe_gemini_credential(account: &str) -> Result<ProbeRecord>;
fn probe_zai_usage(account: &str) -> Result<ProbeRecord>;
fn probe_glm_active(account: &str, model: &str) -> Result<ProbeRecord>;
fn probe_deepseek_balance(account: &str) -> Result<ProbeRecord>;
```

Integration points with existing code:

- **`bro_exec` spawn path**: `acquire_drone` calls the drone selector, then feeds
  the result into the existing `resolve_provider_env` / `build_exec_args` /
  `spawn_task` chain verbatim. No new spawn path.
- **`bro_resume` raw session+provider path**: insert drone session registry
  lookup in `src/tools/dispatch.rs::resolve_resume_target` before the fallback
  `resolve_provider_env(provider, None, None, ...)` call.
- **Task terminal hook**: the `orch::spawn_task` reader loop in `mod.rs` already
  runs a `finalizer` closure on task completion. Add a drone finalizer that
  decrements `in_flight`, merges the last `RateLimit` event, and updates the
  probe record. The tail event path in `tail.rs` is where normalized
  `RateLimit`/`UsageUpdate`/`Completed.Metrics` observations are produced.
- **Background probe timer**: add a `tokio::spawn`-ed background task in `main.rs`
  that ticks at `probe_refresh_interval_seconds` and fans out probe calls per
  configured provider/account. Failures update probe state; successes refresh
  the cache.
- **Config loading**: drone config + probes state + session registry all live
  under `BRO_HOME/drone/`. Load at startup; write changes atomically (same
  pattern as `brofile::load_config`/`save_config`).

Tests:

- selection excludes exhausted/expired accounts
- selection respects max concurrency
- lower 5h utilization beats lower in-flight only after eligibility
- round-robin uses eligibility filters
- spawn failure rolls back in-flight
- raw `bro_resume(session_id, provider)` recovers drone account env
- probe parsers for Claude headers and Codex usage JSON
- probe parser for Z.AI `quota/limit` maps `TOKENS_LIMIT number=5 unit=3` to
  five-hour utilization and `TOKENS_LIMIT number=1 unit=6` to seven-day
  utilization
- probe parser for DeepSeek balance availability and balances
- DeepSeek account with balance below `payg_min_balance_usd` is hard-excluded even
  when `is_available=true`
- DeepSeek balance-tiered capacity: $50 balance scores higher than $0.50 balance
  under the same `payg_balance_ceiling_usd`
- GLM/Z.AI active-probe fallback maps usage exhausted, weekly/monthly exhausted,
  expired plan, unsupported model, temporary rate limit, and high-traffic
  responses into quota/cooldown state
- weighted selection favors configured provider order when candidates have
  comparable synthetic capacity
- weighted selection does not pick a preferred provider whose hard eligibility
  checks failed
- runtime `RateLimit` updates account utilization/exhaustion after a task
- runtime `UsageUpdate` and `Completed.Metrics` update session accounting
- Gemini expired credential marks account non-selectable
- OpenCode file-presence probe maps missing auth to non-selectable

## 11. Open questions

- Should `drone_probe(mode="refresh")` be available on the default MCP surface,
  or ops-only because it can hit provider endpoints?
- Should active probes run synchronously during `acquire_drone` on stale data, or
  should acquisition only use cached probe state and leave refresh to a
  background worker?
  - **Recommendation:** acquisition uses cached state only. Synchronous probes
    during acquire add latency proportional to the slowest provider's API
    response time (Claude ~200ms, Codex ~500ms, GLM hundreds of ms). Background
    refresh on a TTL timer keeps probe data fresh enough without blocking
    dispatch. If cached probe age exceeds `probe_cache_ttl_seconds * 3` (three
    missed refresh cycles), the account is marked `quota_status=probe_failed`
    and excluded until the background worker can retry.
- Should `pool=["codex"]` mean "all configured Codex accounts" or "only Codex
  lanes in the default pool"? The former is simpler for callers; the latter is
  stricter for operators.
- Should the drone config use brofile refs as the canonical mapping source, or
  copy provider/model/effort into `drone.providers`? Copying is clearer for
  selection; refs reduce drift with the `drones` team template.
- How should custom-model Claude providers map additional accounts? V1 supports
  `accounts: ["default"]` automatically with `~/.claude-zai` and
  `~/.claude-ds`. Additional GLM/DeepSeek accounts should use explicit account
  env overrides for `CLAUDE_CONFIG_DIR` until a stable suffix convention is
  chosen. Inception remains OpenCode-backed and still needs explicit account env
  overrides for non-default accounts.
