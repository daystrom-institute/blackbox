---
title: "Secret custody across the checkout and corpus planes"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - operations
  - config-artifacts
tags:
  - secrets
  - auth
  - identity
  - openbao
  - locality
  - producer-grants
  - connectors
brief: "Secret custody splits on the same locality axis as everything else: producer-plane processes hold their own credentials, the corpus-plane daemon holds only what corpus-plane work needs, and nothing durable ever stores secret material. A reference registry (file, env for bootstrap only, 1Password, OpenBao) resolves at the edge in the process that owns the secret, grounded in the estate's deployed identity and secrets plane."
date: 2026-08-11
---

# Secret custody across the checkout and corpus planes

> **Status: proposed (blackbox integration); deployed (estate plane), 2026-08-11.**
> Two things with different truth values live here. Section 5 describes the
> operator's **deployed** cluster identity and secrets plane: an OIDC identity
> provider, an in-cluster OpenBao, external secret syncing, and an off-cluster
> password vault as root of trust. That plane runs today, independent of
> blackbox. Everything else is **proposed** blackbox work. What is already
> implemented in blackbox is narrow and named where it appears: the static
> secret resolver in `bbox-config` (systemd credential directory, managed file
> secrets directory, prefixed env), the file-sourced `ServiceToken` bearer used
> by producer transports and the fleet supervisor, and the harness
> credential-scrub and non-secret `shell_env` lanes. Every other consumer named
> in section 1 reads process env directly today. Line cites rot; reverify
> contracts against code before building on this snapshot.

## 0. Decision

Secret custody follows **locality**, the axis the daemon decomposition already
follows. There is no longer one process that could hold every credential, so
stop designing as if there were.

1. **Custody follows the plane that uses the secret.** Producer-plane
   processes (code collectors, connector satellites, harness children, the
   operator's `bro` CLI) hold their own credentials from their own host. The
   corpus-plane daemon holds only what corpus-plane work needs. Neither plane
   brokers for the other.
2. **References travel; material does not.** Config, manifests, catalog
   records, grants, graph facts, MCP arguments, task state, and logs carry a
   *reference*, never the secret. A reference is a request to resolve, not a
   grant.
3. **Resolution happens at the edge, late, in memory,** in the process that
   owns the secret, and never writes plaintext to disk as a side effect.
4. **Fail closed, with remediation text** naming the selector, the provider
   tried, and the operator action. Never a silent fall-through.
5. **Writes are explicit and unambiguous.** Rotating credentials go to exactly
   one declared writable reference. There is no chain on the write path, ever.
6. **Secret and non-secret env stay on separate lanes.** `shell_env` remains
   non-secret by invariant; provider credentials reach a harness child in its
   own process env and are scrubbed from shell grandchildren.
7. **Repo-controlled artifacts cannot mint secret access.** Anything a checkout
   can edit is attacker-influenceable by cloning a repo, so references there
   resolve only against a host-local, operator-approved grant list.

## 1. Why the single-resolver shape does not survive

The unlanded predecessor proposed one pluggable resolver inside one daemon,
with external managers as chain links. That assumed a monolith. The locality
program removed the assumption: checkout-coupled work moved to producer-plane
satellites and the harness, and the corpus plane moved into the cluster. A
single daemon-side chain now has two possible implementations, both bad.
Either producers call the corpus for their own credentials (recreating the
daemon reach-in the locality program spent itself deleting, and making the
corpus an availability dependency for a connector's OAuth refresh), or every
process grows private resolution logic with no shared contract at all.

The resolution: keep the **shared vocabulary** (one reference grammar, one
provider-type config pattern, one error taxonomy, one redacted value type, in
`bbox-config`, linkable by any binary) and drop the **shared process**.

| Consumer | Plane | Secrets it needs | Custody |
|---|---|---|---|
| Corpus daemon (cage) | corpus | Embedding route API keys, webhook signing secrets, external system-event tokens, producer-grant verification material | Cluster secrets plane, delivered as file-shaped references |
| Code collector | producer | Its `ServiceToken` for the corpus transport | Producer host, file-sourced |
| Connector satellite (proposed) | producer | Its `ServiceToken`, plus per-source OAuth client secrets and rotating refresh tokens | Producer host, file-sourced, with a writable reference for refresh |
| Harness child | producer-adjacent | Provider transport credentials for its dispatch | Composed centrally today, delivered per child (section 13) |
| `bro` CLI | checkout | A scope-bound `ServiceToken` for blame, provenance, and MCP routes | Operator host, file-sourced |

The daemon's own consumers are not on this contract yet: embedding providers
read a named env var at construction, MCP injection resolves a secret entry
through process env, webhook verification reads a per-endpoint env var, and
provider dispatch env is synthesized from process env plus brofile account
records. The sanctioned static resolver exists and serves three call sites.
Converging them is phase work, not a precondition for the custody rules.

## 2. The reference registry

### 2.1 Grammar

Two addressing modes, and the distinction is load-bearing. A **bare name**
(`voyage-api-key`) walks an ordered chain of static local sources, preserving
today's shipped behavior byte for byte. An **explicit reference**
(`op://vault/item/field`) targets exactly one provider and never falls
through: a typo must fail, not resolve against a lookalike source.

| Scheme | Form | Resolves against | Notes |
|---|---|---|---|
| `file` | `file://<name>` | Managed secrets dir, `0700` dir and `0600` file enforced | Primary local form on producer hosts |
| `file` | `file:///abs/path` | Absolute path, ownership and mode checked | The cage form: projected volumes, synced secret mounts, systemd credential entries |
| `env` | `env:NAME` | Process environment | **Bootstrap only**, see below |
| `op` | `op://<vault>/<item>/<field>` | Operator password vault, via its CLI | Operator hosts and interactive producers |
| `bao` | `bao://<mount>/<path>#<field>` | In-cluster secrets plane, KV v2 over HTTP | Corpus plane, and hosted producers later |

`env:` is deliberately demoted. It stays legal for exactly two purposes: a
provider's own bootstrap credential on a host with no better delivery
mechanism, and compatibility with shipped consumers during their migration. It
is never a general lane, because process environment leaks into children by
default, appears in process listings and crash reports, and has no rotation
story. The prefixed-env source in the shipped resolver stays as the last
bare-name chain link and grows no new consumers.

### 2.2 Provider configuration

House pattern: config-alias-with-type, matching the embed provider map and the
producer grant list.

```toml
[secrets]
chain = ["loadcredential", "file", "env"]   # bare-name order; omitted = compiled default

[secrets.providers.vault-op]
type = "1password"
vaults = ["blackbox"]                       # allowlist; refs outside it are rejected
service_account_token = "file://op-sa-token"
cache_ttl_secs = 300

[secrets.providers.bao]
type = "openbao"
address = "<openbao-api-endpoint>"
auth = { kind = "jwt", role = "blackbox-corpus" }
# auth = { kind = "token", token = "file:///run/credentials/bao-token" }
cache_ttl_secs = 60
```

Adding an alias makes its schemes available for explicit references. Adding it
to `chain` additionally makes it a bare-name link, opt-in on purpose: a
bare-name lookup must not reach the network unless the operator asked.

Load-time validation: unknown `type` rejected; a provider's own bootstrap
credential resolves through **static local sources only**, so the provider
dependency graph is one level deep by construction and has no cycles to
detect; when two aliases claim one scheme the bare form loses its default and
is rejected with a "qualify which provider" message, while the
alias-qualified form `<scheme>+<alias>://...` is always accepted and
normalized to the provider's native URI before dispatch; `chain` entries must
name configured or built-in providers.

### 2.3 Error taxonomy

Structural protection, not hygiene. **NotFound is terminal and never retried**,
so a misconfigured reference cannot become a retry loop. **RateLimited backs
off inside the provider**; the registry never retries on a provider's behalf.
External vault accounts enforce per-token and account-wide quotas, and the
documented ecosystem failure mode is a crash-looping resolver exhausting a
daily cap in minutes and locking every other consumer out of that account for
the rest of the window; a startup health probe validates credentials once
before any loop can spin, and the per-reference cache is mandatory protection
rather than a latency optimization. **Auth and Unavailable carry remediation
text** and degrade only their own provider. **UnknownScheme and
AmbiguousScheme** are config-shaped errors naming the configured aliases.

### 2.4 The value type

Resolved material lands in a type that is non-`Clone`, prints `<redacted>`
from both `Debug` and `Display`, and zeroizes on drop; provider output buffers
(subprocess stdout, HTTP bodies) are zeroizing buffers for the same reason.
The currently shipped type derives `Debug` over an inner `String`, so any
`{:?}` on an error path prints the secret. That is a defect fix independent of
the rest of this design, and it is phase 1.

## 3. Custody by plane

**Producer plane.** A producer satellite is a small, dependency-clean binary on
the machine that owns the thing it observes. It holds its `ServiceToken` for
the corpus transport, and for connector satellites the per-source credentials
of the remote store: OAuth client id and secret, and a rotating refresh token.
All of it resolves on the producer host, by the producer process, from
producer-host references. The corpus never sees it, never stores a reference
to it, and cannot ask for it. A satellite whose refresh token expires is
broken locally and reports that through its publisher status; it does not fail
over to a central broker, because there is not one. This is what makes the
connector program deployable on a machine the corpus cannot reach into.

**Corpus plane.** The daemon holds embedding route API keys, webhook signing
secrets, tokens for external system-event integrations, and producer-grant
verification material. In the deployed topology these arrive as file-shaped
references produced by the cluster secrets plane (section 5), so the daemon's
resolution path is the boring one: read a file, check ownership and mode,
redact, cache in memory.

**What never happens.** No durable artifact holds secret material: not the
project catalog, producer grants, manifests and generations, blob metadata,
knowledge and gap entries, thread and note records, graph facts and edge
sidecars, task and event logs, brofile and account records, workflow
definitions, rendered provider memory, `.bbox/` entry files, MCP tool arguments
and results, or any log line at any level. References are permitted in all of
them, subject to rule 7 for anything a checkout can edit.

## 4. Writable references: the producer-side token store

Resolution is read-shaped, but rotating credentials write. OAuth refresh
tokens for document stores, mail and calendar APIs, accounting APIs, and chat
workspaces are the forcing case: the provider hands back a new refresh token
on redemption, and losing it means a reauthorization ceremony with a human in
the loop. The predecessor design got this contract right; it survives intact,
except that it now lives **producer-side**.

```rust
#[async_trait]
pub trait TokenStore: Send + Sync {
    /// Persist a value at an explicitly writable reference, with atomic
    /// visibility (readers see old or new, never partial) and no unprotected
    /// intermediate plaintext.
    async fn put(&self, reference: &str, value: &SecretValue) -> Result<(), SecretError>;
}
```

- **One unambiguous writable reference.** Explicit ref forms only
  (`file://<name>` or `file:///abs/path`); bare names are not writable and
  there is no chain on the write path. A fallback write destination is
  simultaneously a data-loss and a disclosure hazard.
- **Explicitly marked writable** in config, consumer-side; everything else is
  read-only by default.
- **Atomic visibility, honestly stated.** It stages a `0600` temp file inside
  the `0700` secrets dir and renames over the target, so the intermediate is
  permission-protected rather than nonexistent; parent-dir fsync is best
  effort. Atomic visibility is guaranteed, crash-durable replacement is not.
- **Single writer per reference.** The satellite owns its own refresh tokens;
  concurrent redemption from two processes against one reference is
  unsupported, prevented by process ownership rather than locking cleverness.
- **Rotation is local**: redeem, write back, swap the in-memory value. No
  corpus round trip, no daemon involvement, no manifest entry.

The corpus daemon keeps its own narrow instance of the same contract for
tokens it generates itself (the existing Forgejo system-events path already
persists generated API tokens into the managed secrets directory). That is
corpus-plane custody of a corpus-plane secret and stays where it is.

## 5. The deployed estate identity and secrets plane

Infrastructure that exists and runs today, described generically. Blackbox
designs *toward* it; it does not depend on blackbox.

**Identity.** An OIDC identity provider is the estate's identity plane. The
operator group federates to an upstream account provider and authenticates
with passkeys; tenant groups are local accounts in the same realm. The cluster
API server trusts the provider's OIDC issuer directly, and cluster RBAC binds
identity-plane groups rather than per-user certificates. Tenancy is a stamped
template: namespace, quotas, network policy walls, and one role binding per
tenant group.

**Secrets.** An in-cluster OpenBao is the secrets plane, and its custody
properties are the interesting part. Auto-unseal is static-key, chained to the
operator's password vault: the 32-byte key lives on server host disk,
root-owned mode `0600`, mounted by host path and referenced as a file, and
**never** in the cluster key-value store or a cluster Secret object. That is
backup-path isolation, so a cluster backup does not contain the key that
decrypts the cluster's own secrets. Recovery keys live off cluster in the
password vault, and the initial root token is revoked once policy bootstrap
completes, leaving no standing root credential inside the cluster. It runs as
a single instance on replicated block storage; multi-node consensus is
deferred, with the availability consequence handled by the distribution choice
below rather than by clustering.

**Root of trust.** The operator's password vault. Bootstrap secrets,
break-glass credentials, the unseal key, and the recovery keys all originate
there, which yields the invariant every decision on this plane is checked
against: **the cluster alone must never be able to decrypt itself.**

**Distribution, chosen per consumer.** Platform consumers use external secret
syncing: a controller syncs selected secrets-plane paths into cluster Secret
objects and caches the last synced value, so a sealed or unreachable secrets
plane pauses rotation but takes nothing down. This is the default, because
availability decoupling is worth more than freshness for long-lived
credentials. Direct API access, including short-TTL dynamic credentials, is
reserved for consumers that explicitly accept availability coupling; choosing
it means choosing to be down when the secrets plane is down, and that choice
must be deliberate and recorded.

**Service auth.** Services present an identity-plane JWT at the secrets
plane's JWT auth method and receive policy-scoped credentials, rather than
holding a static token. A static token is a durable bearer with no identity
binding and no natural expiry; the exchange gives both.

**The cage daemon is a platform consumer.** It deploys from an immutable image
digest through the operator's infrastructure overlay, and its corpus-plane
secrets arrive on the platform lane by default:

| Secret | Lane | Why |
|---|---|---|
| Embedding route API keys | Synced | Long-lived vendor keys; a stale cached value beats an embedding outage. Rotation lands on the config reload that reconstructs embed providers. |
| Webhook signing secrets | Synced | Verification must keep working during a secrets-plane outage or every inbound event fails closed at once. |
| External system-event API tokens | Synced | Same reasoning; those integrations already tolerate token replacement at reload. |
| Producer-grant verification material | Synced | Transport auth must survive an outage; otherwise a sealed vault silently stops all corpus ingestion. |
| Any future short-TTL dynamic credential | Direct | Dynamic credentials are the whole reason the direct lane exists. |

The rule of thumb behind the table: use the synced lane unless a stale value
is *worse* than an outage. For bearer verification and vendor API keys it
never is.

## 6. Relationship to ServiceToken producer grants

Producer grants are landed and are the **wire-auth** story. A producer loads a
bearer token from a file with owner and mode checks, presents it on the
internal transport endpoints, and the server binds it to an immutable
`producer_id` plus an allowlist of durable published scopes from the producer
grant list. Tokens never appear in env, query strings, MCP arguments, or logs.
Scope authority lives entirely server-side, so a leaked token is bounded by
the scopes its grant names.

This document is the **custody** story around that:

- **Where the token file comes from.** Daemon side: a synced reference, so the
  grant entry points at a projected file path rather than a value. Producer
  side: an entry in the managed secrets directory, materialized once at
  provisioning from the operator's password vault, never fetched per request.
- **Rotation needs an overlap window and does not have one.** A grant entry
  names exactly one token file today, making rotation a simultaneous two-sided
  cutover with a guaranteed failure window. Proposal, tracked as
  gap-bb84c77f: let a grant accept an ordered list of accepted token files for
  one `producer_id`, so rotation is add-new, redeploy-producer, remove-old,
  each step independently safe. Verification tries each accepted token in
  constant time and records which matched, so the operator can see when the
  old credential fell out of use.
- **Revocation is grant removal.** Deleting the producer's token file is best
  effort and only affects a process that has not already loaded it. Removing
  the grant entry daemon-side is authoritative, applies on reload, and is the
  operation an incident runbook should name.
- **Connector satellites reuse this unchanged**: a `producer_id`, a token
  file, a scope allowlist, same contract. What they do not reuse is scope
  minting for non-git sources, which is open design surface in the connector
  docs and not a secrets problem.

## 7. Hosted multi-principal (delivery gate, not current behavior)

The connector program's hosted milestone introduces principals that are not
the operator. Nothing here describes current behavior; it is the gate a hosted
milestone must clear.

Shape: a principal authenticates to the identity plane and receives an OIDC
identity; a service acting for that principal exchanges the resulting JWT at
the secrets plane's JWT auth method for **policy-scoped, short-TTL
credentials** bound to that principal's paths. Policy lives in the secrets
plane over per-tenant path prefixes; blackbox holds no principal credential
material and no policy table of its own.

1. Every hosted secret path is namespaced per principal, and the policy is
   provably restrictive: a cross-principal read is denied by a test, not by
   convention.
2. Credential acquisition is **fail-closed**. If the identity provider or the
   secrets plane is unavailable, a new principal session refuses with a typed
   error. There is no cached-credential fallback for a principal not already
   served, and existing leases run to expiry rather than being extended on
   faith.
3. Lease-shaped consumers exist before lease machinery ships. A credential
   with a lease id, an expiry, and a renewability flag is a value plus an
   obligation, which does not fit a read-shaped resolver and should not be
   forced into it; it gets its own narrow lease, renew, and revoke contract
   when a consumer holds long-lived remote sessions.
4. Audit is per principal and per purpose in the secrets plane's own audit
   log, not reconstructed from blackbox telemetry.
5. Cluster-layer tenant isolation (namespace, quotas, network policy) is in
   place for hosted workloads before any hosted credential is issued.

Until every item holds, hosted multi-principal does not ship and the
single-operator posture stands.

## 8. Threat model

| Threat | Control |
|---|---|
| Backup-path key exfiltration | Unseal key on host disk, root-owned `0600`, never in the cluster key-value store or a Secret object; recovery keys off cluster |
| Cluster compromise yielding full disclosure | The cluster alone cannot decrypt itself; root of trust is the operator's off-cluster password vault |
| Secret-in-etcd | Synced Secret objects are cluster-resident by construction, so the control is scope: only derived, rotatable material is synced, never unseal or root-of-trust material. Consumers that cannot accept a cluster-resident copy take the direct lane. |
| Env leakage into child processes | `env:` is bootstrap-only; provider credentials are delivered per harness child and named on the spawn scrub list so shell grandchildren do not inherit them; `shell_env` is non-secret by invariant |
| Secret-in-log | Redacted, non-`Clone`, zeroize-on-drop value type; zeroizing provider buffers; subprocess stderr scrubbed before becoming an error; errors name the selector, never the value |
| Plaintext left on disk by a read | No disk-backed cache; resolution and persistence never share a code path; the only writes go through the explicit writable-reference contract |
| A repo minting secret access | References in checkout-editable artifacts resolve only against a host-local operator grant list; a new reference fails closed with grant-shaped remediation |
| Quota lockout of a shared vault account | Terminal `NotFound`, provider-internal backoff only, startup health probe before any loop, mandatory per-reference cache |
| Stolen producer token | Server-side scope allowlist bounds blast radius to named published scopes; revocation is a grant-entry removal |
| Corpus compromise yielding producer credentials | The corpus never holds them; there is nothing to steal on that path |

## 9. Non-goals

- **Not a secrets manager.** Blackbox does not create, rotate, or version
  secrets inside external managers. The one write surface is section 4.
- **Not an identity or policy system.** Purpose labels are telemetry; policy
  lives in the secrets plane's policies and the identity provider.
- **Not a broker for producer credentials.** The corpus does not hold, mint,
  proxy, or cache anything a producer needs.
- **Not a replacement for producer-grant wire auth.** Section 6 wraps it.
- **Not a change to the `shell_env` lane.** It stays non-secret.
- **Not operator-host credential management** beyond naming the lane.

## 10. Rejected alternatives

- **A daemon-central secret broker serving producers.** Wrong plane. It
  recreates the corpus-reaches-into-producer coupling the locality program
  deleted, makes the corpus an availability dependency for a connector's OAuth
  refresh, and hands the corpus custody of credentials it has no reason to
  hold. Rejected on all three counts independently.
- **Cluster Secret objects as the only mechanism.** No rotation story, no
  audit trail, and the source of truth becomes the cluster itself, breaking
  the "cluster cannot decrypt itself" invariant outright.
- **A hosted cloud key-management service.** Violates the estate's zero
  cloud-credential posture, moves the root of trust outside the operator's
  control, and adds an external availability and billing dependency to
  unsealing. The unseal chain terminating in a vault the operator physically
  controls is the point.
- **One universal daemon-side chain (the predecessor design).** Correct for a
  monolith, wrong after locality. Kept: the reference grammar, the
  alias-with-type config pattern, the error taxonomy, the redaction
  discipline, the writable-reference contract. Dropped: one resolving process.
- **Env-var-first resolution (the de facto current shape).** Leaks into
  children, process listings, and crash reports, and cannot express rotation.
  Demoted to bootstrap and compatibility.
- **Static tokens for service-to-secrets-plane auth.** A durable bearer with
  no identity binding and no natural expiry, when the estate already has an
  identity plane that issues short-lived policy-scoped credentials.

## 11. Phases

1. **Redaction and the value type.** Manual `Debug`/`Display`, non-`Clone`,
   zeroize on drop, zeroizing provider buffers. No behavior change. Gate:
   existing secrets tests green unmodified, plus a workspace test that no
   formatting of the value type yields secret material.
2. **Reference grammar and registry in `bbox-config`.** `file` and `env`
   schemes, alias-qualified parsing, chain compatibility, load-time
   validation, and a read-only status surface listing providers and health but
   never values. Gate: default behavior with an empty config section is
   byte-identical to today.
3. **Corpus-plane consumer adoption.** Embed key references, webhook signing
   references, MCP injection references behind the per-project grant list, and
   generated-token persistence onto the writable contract. Gate: an embed
   request and an inbound webhook each run from a reference with no plaintext
   in any persisted artifact or log; an ungranted project-local reference
   fails closed.
4. **Producer-plane adoption.** Collector and connector satellite config take
   references; producer-side writable references for OAuth refresh; the
   password-vault provider on operator hosts. Gate: a satellite rotates a
   refresh token with no corpus round trip and no plaintext outside the
   managed directory.
5. **Secrets-plane provider.** `bao://` KV v2 reads with JWT auth; the
   synced-versus-direct choice recorded per secret with its justification.
   Gate: a live read against the deployed instance using JWT exchange.
6. **Producer-grant rotation overlap** (gap-bb84c77f). Multi-token grants,
   constant-time verification across accepted tokens, matched-token
   observability. Gate: a producer token rotates with zero failed requests
   across the window.
7. **Hosted multi-principal.** Blocked on the section 7 gate.

## 12. Acceptance criteria

- Default behavior with no secrets configuration is byte-identical to the
  shipped resolver: same precedence, permission enforcement, and error text.
- No durable artifact contains secret material, verified by a scrub pass over
  persisted stores and logs in a run exercising one reference per configured
  provider type.
- An explicit reference whose provider is unavailable produces a typed,
  remediation-bearing error and never falls through; a provider's own
  bootstrap credential cannot resolve through another external provider.
- Bare-name lookups perform no network or subprocess work unless an external
  provider was explicitly added to the chain.
- A writable reference is replaced atomically at exactly the named reference
  with no write-path fallback; a crash mid-write leaves the old value or the
  new one.
- A connector satellite rotates a refresh token with the corpus daemon
  stopped, and resumes publishing when the corpus returns.
- A project-local artifact referencing an external or absolute-file secret
  with no host-local grant fails closed with grant-shaped remediation.
- A producer token rotation completes with no request failures once
  multi-token grants land; until then the doc and runbook both state plainly
  that rotation has a failure window.
- No harness shell grandchild observes a provider credential in its
  environment.

## 13. Open questions

- **Provider dispatch credentials with an off-host fleet supervisor.** The
  spawn spec is composed centrally and the supervisor deliberately never reads
  credential stores, which was clean when both ran on one machine. With the
  corpus in the cluster and the supervisor on the agent machine, provider
  credentials now cross a network boundary inside the spawn spec. The
  authenticated control seam protects them in transit, so this is a custody
  question, not a disclosure one: should they resolve supervisor-side from
  producer-host references instead, making a harness child's credentials a
  producer-plane concern like everything else it touches? Leaning yes on
  custody-rule-1 grounds, but it inverts a settled contract (policy decided
  centrally, enforced by construction) and is not worth reopening until there
  is a second fleet endpoint.
- **Scope identity for non-git connector sources.** Producer grants bind to
  durable published scopes minted from a committed repo identity; a document
  folder or a chat workspace has none. Whatever the connector docs settle on
  becomes the thing a connector's grant names, and the token custody story
  rides it unchanged.
- **Who records the synced-versus-direct decision.** It is per secret and
  needs a reviewable justification. Leaning the cage stack config with an
  inline reason field, so the decision travels with the deployment rather than
  living only in this document's table.
- **Operator-host keychain link.** A platform keychain provider fits the
  registry trivially and was the original deferral in the archived locality
  design. Demand-driven: a desktop convenience lane, never a daemon backend,
  and headless hosts frequently lack the session bus it needs.

## 14. Relationship

- **Extends**
  [locality-first-decomposition.md](../../daemon-runtime/locality-first-decomposition.md):
  adopts its plane split as the custody axis and its file-sourced bearer-token
  model as the wire-auth substrate, then answers what that design leaves open,
  namely where the token files come from and how they rotate.
- **Companion of** the connector design set, which depends on this document
  for its credential story:
  [remote-source-connectors.md](../../connectors/remote-source-connectors.md)
  (per-source OAuth credentials and rotating refresh tokens, producer-side),
  [slack-ingestion-connector.md](../../connectors/slack-ingestion-connector.md)
  (workspace credentials on the same contract), and
  [reflective-graph-connector-program.md](../../connectors/reflective-graph-connector-program.md)
  (the campaign arc that sequences them).
- **Companion of**
  [remote-project-onboarding.md](../../daemon-runtime/remote-project-onboarding.md):
  onboarding is two-sided operator config, and the producer token file is one
  of the two sides.
- **Continues**
  [config-and-artifact-locality.md](config-and-artifact-locality.md)
  (archived): it specified the static three-source resolver and the
  by-reference convention for project-local MCP config, and explicitly
  deferred external managers. This is that deferral taken up, with the custody
  split the locality program made necessary.
- **Supersedes** the unlanded "Pluggable Secrets Providers" draft, which never
  reached this branch. Salvaged: the reference grammar and no-fall-through
  rule, the alias-with-type config pattern, the error taxonomy and its
  rate-limit rationale, the redaction and zeroize discipline, the explicit
  writable-reference contract, and the repo-cannot-mint-access grant rule.
  Dropped: the single daemon-side resolution chain as the universal answer,
  and its treatment of every credential consumer as a daemon consumer.
