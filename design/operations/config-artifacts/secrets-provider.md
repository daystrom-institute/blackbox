---
title: "Pluggable Secrets Providers"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - operations
  - config-artifacts
tags:
  - secrets
  - auth
  - 1password
  - openbao
  - identity
  - connectors
brief: "Where blackbox gets its secrets from: a pluggable SecretsProvider layer over the shipped static resolver, with 1Password CLI as the first external concretion and OpenBao as the lease-shaped successor."
date: 2026-07-14
---

# Pluggable Secrets Providers

Status: proposed

## Problem

Blackbox has a shipped, well-behaved static secrets resolver and a growing set
of credential consumers that mostly bypass it. The question "where does bbox
get its secrets from?" currently has four different answers:

1. **The sanctioned resolver** (`crates/bbox-config/src/secrets.rs`,
   re-exported as `blackbox::secrets`): a fixed three-source priority chain in
   `resolve_with_sources` - systemd `LoadCredential`
   (`$CREDENTIALS_DIRECTORY/<name>`), file secrets
   (`$XDG_DATA_HOME/blackbox/secrets/<name>`, 0700 dir / 0600 file enforced),
   then env (`BLACKBOX_SECRET_<UPPER_SNAKE>`). Values come back as a
   `SecretValue` newtype exposed only via `.expose()`. Consumers today:
   Forgejo system events (`src/system_events_runtime/forgejo.rs`), workflow
   external ops (`src/workflow/ops/external.rs`), and the Slack sidecar
   (`crates/bro-slack/src/main.rs`).
2. **Provider dispatch env synthesis** (`src/orchestration/brofile.rs`,
   `resolve_provider_env`): reads `ANTHROPIC_AUTH_TOKEN` / `MISTRAL_API_KEY` /
   etc. directly from the daemon's process env and dotfiles, merged with
   per-account overrides from brofile account records. Never touches
   `secrets::resolve`.
3. **Embed provider keys** (`crates/bbox-embed`): `api_key_env` config fields
   name an env var read via `std::env::var` at provider construction
   (`VOYAGE_API_KEY`, with a legacy fallback name).
4. **MCP injection refs** (`src/orchestration/mcp.rs`): `Secret { name }`
   entries resolve as `std::env::var(name)`.

This was fine while every secret was an API key the operator exported once.
The remote-source connector program
(`design/connectors/remote-source-connectors.md`) breaks that assumption:
OAuth client secrets, refresh tokens, and per-mount service-account
credentials are numerous, rotated, and per-source. Hand-copying them into
`~/.local/share/blackbox/secrets/` or the daemon env does not scale and
duplicates a source of truth the operator already maintains in a password
manager.

The prerequisite this design answers: one pluggable resolution layer, with
external secret managers as adapters, that every credential consumer in the
daemon can converge on.

## Prior art

**In-repo.** The archived `config-and-artifact-locality.md` (this directory)
specified the static resolver and the `{"$secret": "name"}` by-reference
convention for `.bbox/mcp.json`; the three-source resolver shipped, with
the precedence the implementation phase specified (LoadCredential over
file over env; the archived draft's prose is inconsistent about env
precedence in one section, and the code is the authority). Its §3
explicitly deferred external managers ("OS keyring as opt-in later"). This
design is that deferral, taken up with a broader shape.

**Operator estate (private infra repos; patterns genericized here).** Two
working systems inform the concretions:

- A 1Password-backed bootstrap: an `ENVVAR=op://<vault>/<item>/<field>` map
  resolved via `op read` into a managed shell block. Auth is either the
  desktop-app CLI integration (interactive) or a read-only
  service-account token scoped to one vault (headless), with an explicit
  override env var because ambient `OP_SERVICE_ACCOUNT_TOKEN` values are
  often scoped to a different vault. Preflight is `op whoami` plus a
  vault-visibility check, failing loudly with remediation text.
- A policy-bound OpenBao credential broker (JVM reference implementation):
  `lease / renew / revoke / withLease` over KV v2 and dynamic backends, a
  `CredentialLease` carrying expiry, renewability, redacted material, and
  audit tags, and purpose-based policy gates (principal kind + role/scope
  decide which credential purposes may be leased). The estate's identity
  plane is a Keycloak realm (OIDC); OpenBao hangs off it via JWT auth.

Both are treated as pattern donors, not dependencies.

**Ecosystem (surveyed 2026-07-14).** The scheme-keyed provider design below
has strong external precedent: SecretSpec (Rust, 0.14) resolves
`op://`/`vault://`/`env://`/`keyring://`-style provider URIs through a
`Provider` trait with an ordered fallback chain, and the Go `vals` tool's
`ref+<backend>://` convention is widely copied (ArgoCD, helm-secrets).
Notable negative result: **there is no official 1Password Rust SDK** (Go /
JS / Python only, all pre-GA); the community `onepassword` crate (0.1.x,
wraps the native SDK core) is early. That settles CLI shell-out as the v1
concretion, with the trait boundary making a later SDK swap a
provider-internal change.

## Design principles

1. **The resolver is a chain of providers, and the chain is config.** The
   shipped three-source behavior becomes the default chain; external managers
   are additional links or explicit targets, never silent replacements.
2. **Two addressing modes: bare names and explicit refs.** A bare name
   (`voyage-api-key`) walks the chain, preserving today's semantics. An
   explicit ref (`op://vault/item/field`) targets exactly one provider and
   never falls through: a typo must fail, not leak into a lookalike source.
3. **Secrets resolve at the daemon, late, and in memory.** No provider may
   write plaintext to disk as a side effect of resolution. Caching is
   in-memory with TTL, per provider.
4. **Provider bootstrap credentials come from static links only.** A
   provider's own auth (a 1Password service-account token, an OpenBao token)
   resolves through LoadCredential/file/env exclusively. No provider may
   depend on another external provider; cycles are rejected at config load.
5. **Fail closed, fail loudly, never log values.** A missing secret names the
   secret and the sources tried; an unavailable provider binary degrades that
   provider only, with a typed error, and never silently falls back for
   explicit refs.
6. **Non-secret and secret env stay on separate lanes.** `fleet.json
   project_dispatch.env` and the harness `shell_env` lane remain non-secret
   by invariant. Resolved secrets reach dispatched work through two lanes
   only: a typed in-memory credential map handed to the harness
   transport/provider configuration for in-process providers (the normal
   case: GLM/DeepSeek/MiniMax/Brodex run inside the daemon via
   `bro-harness`, so there is no child process to env-inject and the
   daemon's own process env is never mutated), or spawn-time env synthesis
   for true subprocess consumers (sidecars, legacy paths). Never persisted
   config, never the shell_env lane.
7. **Repo-controlled artifacts cannot mint secret access.** Anything a
   checkout can edit (`.bbox/mcp.json`, project overlays) is
   attacker-influenceable by cloning a repo; a reference written there is
   a request, not a grant. External-manager and absolute-file refs from
   project-local artifacts are default-deny and resolve only against a
   host-local, operator-approved per-project grant list (exact selectors
   or prefixes). A new reference fails closed with a "grant this?"
   remediation instead of resolving.

## Core abstraction

Home: `crates/bbox-config/src/secrets/` (the existing `secrets.rs` splits
into a module; `blackbox::secrets` re-export unchanged).

```rust
/// A resolved secret. Debug/Display print "<redacted>"; memory is zeroized
/// on drop.
pub struct SecretValue(/* zeroizing inner */);

pub struct SecretRequest<'a> {
    /// Bare name ("voyage-api-key") or explicit ref ("op://v/i/f").
    pub selector: &'a str,
    /// What the secret is for, for audit/telemetry labels only.
    pub purpose: Option<&'a str>,
}

#[async_trait]
pub trait SecretsProvider: Send + Sync {
    /// Stable provider alias, e.g. "file", "env", "op-personal".
    fn id(&self) -> &str;
    /// URI schemes this provider claims for explicit refs, e.g. ["op"].
    fn schemes(&self) -> &[&str] { &[] }
    /// Resolve a bare name (chain mode). Ok(None) is the ONLY encoding of
    /// "not present in this source" and continues the chain; any Err
    /// aborts resolution. Providers handle their own retryable conditions
    /// (backoff on RateLimited) internally; the registry never retries.
    async fn resolve_name(
        &self,
        req: &SecretRequest<'_>,
    ) -> Result<Option<Arc<SecretValue>>, SecretError>;
    /// Resolve an explicit ref in one of this provider's schemes.
    /// Absence here is SecretError::NotFound: terminal, never retried,
    /// never a fall-through.
    async fn resolve_ref(
        &self,
        req: &SecretRequest<'_>,
    ) -> Result<Arc<SecretValue>, SecretError>;
    /// Availability/auth probe for doctor/status surfaces; also run once
    /// at startup before any resolution loop can spin.
    async fn health(&self) -> ProviderHealth;
}
```

Values return as `Arc<SecretValue>` so the mandatory cache and concurrent
consumers share one zeroized allocation instead of cloning secret bytes;
`SecretValue` itself stays non-`Clone`. `SecretRequest` (selector +
`purpose`) is what providers receive, so purpose labels reach provider
telemetry without a second signature.

```rust
pub enum SecretError {
    /// Terminal: the ref does not exist. Never retried.
    NotFound { selector: String },
    /// Retryable inside the provider only; retry_after is best-effort.
    RateLimited { retry_after: Option<Duration> },
    Auth { remediation: String },
    Unavailable { remediation: String },
    Timeout,
    UnknownScheme { scheme: String },
    /// Bare scheme with multiple claimants; names the candidate aliases.
    AmbiguousScheme { scheme: String, aliases: Vec<String> },
}
```

Notes against the current code:

- The trait is async, and the async registry is the primary API. The `op`
  and OpenBao concretions do subprocess/HTTP I/O; per the daemon
  concurrency model (`design/daemon-runtime/`, `clippy.toml`
  disallowed-methods gate) blocking calls cannot live on tool paths. The
  existing static sources are trivially async (their file I/O wraps in
  `spawn_blocking` inside the provider). The named consumers all run under
  tokio already (the Slack sidecar's `main` is `#[tokio::main]`;
  daemon tool paths are async by construction), so they migrate to the
  async registry directly. The synchronous `resolve`/`resolve_with_sources`
  functions remain ONLY as a documented static-sources compatibility API
  for pre-runtime startup reads; they never walk external providers, and
  no shim ever blocks a tokio runtime to fake a sync signature over an
  async provider.
- `SecretValue`'s current `#[derive(Debug)]` prints the inner string; any
  `{:?}` on an error path leaks the secret into logs. Phase 1 replaces it
  with a manual `Debug`/`Display` printing `SecretValue(<redacted>)` and
  adds zeroize-on-drop. The `secrecy` crate (`SecretString`, zeroize on
  drop, no Debug/Display/Clone, Deserialize-only serde) is the natural
  inner; raw provider output buffers use `Zeroizing<Vec<u8>>`. This is a
  defect fix independent of the rest of the design.
- `resolve_name` returning `Ok(None)` vs `Err` is the load-bearing contract:
  "not present in this source" continues the chain (today's behavior across
  the three sources); "present but unreadable" (bad permissions, auth
  failure, timeout) aborts resolution. The shipped resolver already makes
  exactly this distinction implicitly (a 0644 secret file is a hard error,
  not a fall-through); the trait makes it explicit.
- The error taxonomy above is structural rate-limit protection for
  external managers, not hygiene: `NotFound` terminal-never-retried and
  provider-internal-only backoff are what prevent a misconfigured
  reference from becoming a quota-burning retry loop (see the 1Password
  section for the concrete failure mode).

### The registry and chain

```rust
pub struct SecretsRegistry {
    providers: HashMap<String, Arc<dyn SecretsProvider>>, // by alias
    chain: Vec<Arc<dyn SecretsProvider>>,                 // ordered bare-name links
    scheme_defaults: HashMap<String, Arc<dyn SecretsProvider>>,
}
```

Resolution:

- Bare name: walk `chain` in order, first `Some` wins. Default chain is
  `["loadcredential", "file", "env"]`, byte-for-byte compatible with the
  shipped resolver.
- Explicit ref: parse the scheme, dispatch, no fallback. Unknown scheme is
  a config-shaped error naming the configured providers.
- Multiple providers of one type are first-class (two 1Password accounts,
  two OpenBao instances), so scheme dispatch is alias-aware rather than
  collision-rejecting: the bare scheme (`op://...`) is valid only while
  exactly one configured provider claims it and resolves through that
  default; once a scheme is claimed twice, bare-scheme refs are rejected
  at config load ("qualify which provider") and refs use the
  provider-qualified form `<scheme>+<alias>://...`
  (`op+op-personal://vault/item/field`), which is always accepted. The
  qualified form parses by splitting the scheme on `+`, keeping the
  remainder verbatim for the provider.

Built-in link providers (extractions of the current code, not rewrites):
`loadcredential`, `file`, `env`. Built-in ref schemes for orthogonality:
`env://VAR`, `file://<name>` (an entry in the managed 0700 secrets dir),
and `file:///abs/path` (0600-enforced), so explicit refs can address the
static world too.

### Configuration

House pattern: config-alias adapters with a `type` discriminator, as the
embed provider map already does (`[embed.providers.<alias>] type = ...`).

```toml
[secrets]
# Bare-name resolution order. Omitted = the compiled default chain.
chain = ["loadcredential", "file", "env"]

[secrets.providers.op-personal]
type = "1password"
# Optional vault allowlist; op:// refs outside it are rejected.
vaults = ["blackbox"]
# Bare name resolved through the STATIC links only (principle 4).
service_account_token_secret = "op-personal-sa-token"
# Optional; default is `op` on PATH.
binary = "/opt/homebrew/bin/op"
cache_ttl_secs = 300

[secrets.providers.bao]
type = "openbao"
address = "https://bao.internal:8200"
token_secret = "bao-token"        # static-links bare name, same rule
cache_ttl_secs = 60
```

Adding a provider alias makes its schemes available for explicit refs.
Adding it to `chain` additionally makes it a bare-name link (opt-in;
external managers as chain links are deliberate, not automatic, so a chain
lookup cannot hit the network unless the operator asked for that).

Load-time validation: unknown `type` rejected; duplicate scheme claims are
legal (two 1Password aliases both claim `op`) but leave that scheme with no
bare default, so any bare-scheme ref in loaded config is rejected at
validation and a bare ref arriving later resolves to
`SecretError::AmbiguousScheme` naming the candidate aliases; `chain`
entries must name configured or built-in providers; bootstrap `*_secret`
names must resolve through static links (checked lazily at first use,
reported through `health`). Provider-qualified refs are normalized before
dispatch: the registry strips the `+<alias>` qualifier and hands the
provider the canonical native URI (`op+op-personal://v/i/f` reaches the
`op-personal` provider as `op://v/i/f`, which is what `op read` accepts).

## First concretion: 1Password CLI

`type = "1password"`, scheme `op`. Shells out to the `op` binary; no SDK
dependency in v1 (the CLI is what the operator already authenticates, and
the shell-out pattern with availability gating has precedent in the OCR
pipeline's `pdftoppm`/`tesseract` handling).

- **Ref shape**: `op://<vault>/<item>/<field>` passed verbatim to
  `op read <ref>` (`op read` accepts the URI natively). Bare-name chain
  participation (if configured as a link) maps `<name>` to a configured
  `item_template`, default off; explicit refs are the expected mode.
- **Auth resolution order**: (1) `service_account_token_secret` resolved via
  static links and exported as `OP_SERVICE_ACCOUNT_TOKEN` to the child only;
  (2) ambient desktop-app integration (interactive hosts). The estate
  lesson is folded in: an explicitly configured token always overrides an
  ambient `OP_SERVICE_ACCOUNT_TOKEN`, because ambient tokens are frequently
  scoped to the wrong vault.
- **Preflight**: `health()` runs `op --version` (binary presence) and
  `op whoami` (auth); failures surface in `bbox_doctor`-style status output
  with remediation text, and resolution errors repeat it.
- **Execution discipline**: `tokio::process::Command` with `env_clear()`
  plus an explicit allowlist (`HOME`, `PATH`, `XDG_*`, the SA token in env
  and never argv), no shell interpolation, kill-on-timeout (default 10s),
  stdout captured into a zeroizing buffer only, stderr captured for the
  error message with any value line scrubbed. The `op://` ref itself is
  safe in argv (it is a pointer); resolved values never appear in argv of
  anything. Never `op inject`/`op run` (they template whole
  files/environments; the daemon wants single-value reads).
- **Rate limits are the binding constraint, not politeness.** Service
  accounts are limited per token per hour (1,000 reads/hr on
  Individual/Families and Teams plans; 10,000/hr Business) AND
  account-wide per day (as low as 1,000/day on non-Business plans), with
  HTTP 429 on breach. The documented ecosystem failure mode is a
  crash-looping or hot-retrying resolver exhausting the daily cap in
  minutes and locking the account out for 24 hours. Consequences baked
  into this design: `health()` validates the token once at startup before
  any retry loop can spin, `NotFound` is terminal and never retried,
  `RateLimited` backs off conservatively (the CLI does not document
  exposing a retry-after value, so parsing is best-effort with a fallback
  cooldown sized to the quota window, optionally sanity-checked via
  `op service-account ratelimit`), and the cache below is mandatory
  protection rather than a latency optimization. 1Password Connect (a
  self-hosted caching REST sidecar) is the documented escalation if usage
  ever approaches the caps; out of scope until then.
- **Caching**: per-ref in-memory TTL cache (default 300s) with a
  registry-level `invalidate()` hook wired to a `bbox_secrets` admin surface
  (list providers, health, cache flush; names and health only, never
  values).

## Second concretion: OpenBao (v2, lease-shaped)

`type = "openbao"`, schemes `bao` (and `vault` as an accepted alias). Two
tiers, mirroring the estate broker's split between static and dynamic
material:

- **v2a static**: `bao://<mount>/<path>#<field>` reads KV v2 over HTTP
  (`/v1/<mount>/data/<path>`), token from `token_secret`. This is a plain
  `SecretsProvider` and needs nothing beyond the trait.
- **v2b leases**: dynamic backends (database creds, cloud STS) return
  material with a `lease_id`, TTL, and renewability; a lease is not a value,
  it is a value plus an obligation. That does not fit `SecretsProvider` and
  should not be forced into it. A separate extension trait, modeled on the
  estate broker:

  ```rust
  #[async_trait]
  pub trait CredentialBroker: Send + Sync {
      async fn lease(&self, req: LeaseRequest) -> Result<CredentialLease>;
      async fn renew(&self, lease: &CredentialLease, ttl: Duration) -> Result<CredentialLease>;
      async fn revoke(&self, lease: &CredentialLease) -> Result<()>;
  }
  ```

  with `CredentialLease { path, lease_id, expires_at, renewable,
  redacted_material, audit_tags }`. Consumers that hold long-lived remote
  sessions (connector sync workers are the anticipated first case) take a
  broker, not a `SecretValue`. Deferred until a consumer exists; specified
  now so v1 naming does not paint over it.

**Identity plane note.** The estate direction pairs OpenBao with a Keycloak
realm: services authenticate to Keycloak (OIDC), exchange the JWT at
OpenBao's JWT auth method, and receive policy-scoped tokens, so no static
OpenBao token ships anywhere. For a single-operator local daemon that is
over-engineering today; the design keeps the seam (auth config on the
`openbao` provider is an enum: `token_secret` now, `jwt_oidc { issuer,
role }` later) rather than the machinery. If blackbox ever grows
multi-principal surfaces, principal-scoped secret policy belongs at that
boundary, not inside the resolver.

## Writable references: the token store

Resolution is read-shaped, but two real consumers write: the Forgejo
system-events runtime already persists generated API tokens through
`write_file_secret` (`src/system_events_runtime/forgejo.rs`), and the
connector program needs a durable home for rotating OAuth refresh tokens.
Pretending the layer is read-only would just push those writers back to ad
hoc paths.

Writes get their own narrow contract instead of riding the resolver:

```rust
#[async_trait]
pub trait TokenStore: Send + Sync {
    /// Persist a value at an explicitly writable reference, with atomic
    /// visibility (readers see old or new, never partial) and no
    /// UNPROTECTED intermediate plaintext.
    async fn put(&self, reference: &str, value: &SecretValue) -> Result<(), SecretError>;
}
```

- Writes go to one explicit, unambiguous reference; there is no chain on
  the write path, ever (a fallback write destination is a data-loss and
  disclosure hazard). Writable references use the explicit ref forms only:
  `file://<name>` (an entry in the managed 0700 secrets dir; the name form
  the read side also accepts) or `file:///abs/path`. Bare names are not
  writable references.
- v1 implementation is the `file` link, wrapping the existing
  `write_file_secret` machinery with its honesty stated: it stages a 0600
  temp file inside the 0700 secrets dir and renames over the target, so
  the intermediate is permission-protected rather than nonexistent; the
  wrapper adds best-effort temp cleanup on error plus startup scavenging
  of orphaned `.*.tmp.*` files, and fsyncs the parent directory after
  rename on a best-effort basis (atomic visibility is guaranteed;
  crash-durable replacement is best-effort and documented as such).
  Providers may opt in later (OpenBao KV v2 is a natural writer, 1Password
  is not in scope).
- Config marks writable references explicitly (`writable = true` on the
  grant/consumer side); everything else is read-only by default.
- The Forgejo generated-token path migrates onto `TokenStore` in the
  consumer-adoption phase; behavior is already equivalent.

## Consumers and unification

Phase-ordered; each consumer keeps working unmodified until its phase.

| Consumer | Today | Target |
|---|---|---|
| `secrets::resolve` call sites (Forgejo events, workflow external ops, Slack sidecar) | static chain | migrate to the async registry (all run under tokio); the sync functions remain as the static-sources compatibility API only |
| `.bbox/mcp.json` / MCP injection `Secret { name }` (`src/orchestration/mcp.rs`) | `std::env::var(name)` | `{"$secret": "<name-or-ref>"}` resolved through the registry at dispatch time, gated by the per-project grant list for project-local artifacts (principle 7); refuse dispatch on missing or ungranted, as the locality design already specified for missing |
| Embed providers (`api_key_env`) | `std::env::var` at provider construction (e.g. `voyage_multimodal.rs::from_config`) | additive `api_key_secret = "<name-or-ref>"` config field, resolved at provider construction; rotation mechanism in v1 is an explicit config/daemon reload that reconstructs embed providers (the cache-flush hook does NOT rebuild already-constructed providers; a rotation event the embed runtime consumes is possible later work); `api_key_env` stays as the legacy path |
| Brofile dispatch env (`resolve_provider_env`, account records) | process env + dotfiles + account `env` maps | account/brofile `env` values gain `{"$secret": ...}` support, resolved per dispatch into the typed in-memory credential map handed to the in-process harness transport (principle 6); for true subprocess consumers, into the child env at spawn; process-env fallback unchanged |
| Remote-source connectors (new) | n/a | born on the registry: connector configs carry secret refs, never values |

The connector design consumes this layer by reference and is the forcing
consumer for phases 1-3 (its network connectors need the registry, the
1Password provider, and `TokenStore` adoption; only its `local_mirror`
phase needs none of them).

## Boundaries and non-goals

- **shell_env stays non-secret.** The `fleet.json project_dispatch.env` /
  harness `shell_env` lane invariant is unchanged; the resolver never feeds
  it. Spawn-time env synthesis is the only secret-bearing lane to children.
- **No plaintext persistence as a side effect of reads.** Resolution never
  writes values to disk (no disk-backed cache). Deliberate writes exist
  but only through the explicit `TokenStore` contract above; resolution
  and persistence never share a code path.
- **Not a secrets manager.** Blackbox does not create/rotate/version
  secrets in external managers. The one write surface is the narrow local
  `TokenStore` contract above (daemon-generated/rotating tokens at
  explicit writable refs); general external-manager mutation and secret
  lifecycle management stay out of scope.
- **Not an identity/policy system.** Purpose labels are telemetry, not
  policy. The per-project grant list (principle 7) is a local capability
  check on who may *reference* a secret, not an authorization framework;
  policy-gated leasing arrives only with `CredentialBroker` consumers, and
  that policy lives in the external manager (OpenBao policies), not in
  bbox.

## Phases

1. **Extract + harden (no behavior change).** Split `secrets.rs` into the
   module layout, extract the three links behind `SecretsProvider`, build
   `SecretsRegistry` with the compiled default chain, keep
   `resolve`/`resolve_with_sources` as the documented static-sources
   compatibility API. Fix `SecretValue` Debug leak; add zeroize-on-drop.
   Gate: existing secrets tests green unmodified, plus redaction/zeroize
   tests.
2. **Config + 1Password.** `[secrets]` config parsing and validation, the
   `1password` provider with health/preflight/cache/timeout discipline, the
   `env://`/`file://` ref schemes, and a read-only status surface (provider
   list + health + cache stats; no values). Gate: live `op read` round trip
   on this host; unavailable-binary and wrong-vault failures produce the
   designed errors.
3. **Consumer adoption.** MCP `$secret` refs through the registry with the
   per-project grant boundary (principle 7); `api_key_secret` on embed
   providers; brofile/account `{"$secret": ...}` values resolved per
   dispatch into the in-process credential map (or child env for
   subprocess consumers); Forgejo token persistence onto `TokenStore`;
   named `secrets::resolve` consumers onto the async registry. Gate: an
   in-process dispatch and an embed request each run with a secret sourced
   from 1Password and no plaintext in any persisted artifact or log; an
   ungranted project-local ref fails closed.
4. **OpenBao static (`bao://` KV v2).** When an OpenBao deployment is in
   reach; estate prior art makes this mostly transcription. Gate: KV v2
   read against a live instance, token sourced per principle 4.
5. **CredentialBroker + leases.** Blocked on a real leasing consumer
   (connector sync workers with dynamic cloud creds are the candidate).

## Acceptance criteria

- Default behavior with an empty `[secrets]` section is byte-identical to
  the shipped resolver (same precedence, same permission enforcement, same
  error text for the three static sources).
- `op://` refs resolve end to end on a host with the CLI authenticated;
  the same ref with the binary missing yields a typed, remediation-bearing
  error and no chain fallback.
- A provider's own bootstrap token cannot be configured to resolve through
  another external provider (load/first-use rejection).
- `format!("{:?}", secret_value)` contains no secret material anywhere in
  the workspace (test-enforced).
- No resolved secret value appears in fleet.json, shell_env, brofile/account
  records on disk, logs, or error messages (scrub test on the `op` provider's
  stderr path).
- Chain lookups perform no network/subprocess I/O unless an external
  provider was explicitly added to `chain`.
- A project-local artifact referencing an external or absolute-file secret
  that has no per-project grant fails closed with a grant-shaped
  remediation; grants live host-local, never in the repo.
- With two providers claiming one scheme, a bare-scheme ref is rejected at
  config load and the provider-qualified form resolves against the named
  alias.
- `TokenStore::put` replaces atomically at exactly the named reference;
  there is no write-path fallback of any kind.

## Open questions

- **OS keychain link.** A keyring provider (macOS Keychain / Windows
  Credential Manager / Linux Secret Service) fits the trait trivially and
  was the locality design's original "later". Notes for whenever demand
  arrives: the `keyring` crate's API now lives in `keyring-core` plus
  per-store crates (depend on those for pluggability), the API is sync
  (wrap in `spawn_blocking`), and Linux Secret Service requires a D-Bus
  session that headless hosts often lack, so keychain is a desktop/dev
  convenience link, never the primary daemon backend. Demand-driven; not
  scheduled.
- **Grant UX.** Principle 7's per-project grant list needs an approval
  surface (operator prompt on first use vs explicit `bbox_secrets grant`
  admin verb). Leaning explicit admin verb plus a fail-closed remediation
  message; settle in phase 3 when the MCP consumer lands.
- **Doctor integration.** Whether provider health lands in an existing
  doctor/status surface or a dedicated `bbox_secrets` tool; leaning
  dedicated read-only tool in phase 2, name subject to the `bbox_*`
  namespace convention.
