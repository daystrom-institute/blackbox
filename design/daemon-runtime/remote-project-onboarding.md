---
title: "Remote project onboarding through the collector backchannel"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - daemon-runtime
  - project-catalog
tags: [locality, onboarding, collector, catalog, transport, skills]
brief: "New-project onboarding with zero daemon checkout access and no per-project operator config: a producer may claim unclaimed scopes on first onboard, the checkout-owner collector enrolls projects into a live-reloaded sidecar, bbox_project_register routes a remote path to the collector that covers it, and the daemon serves an instance-rendered onboarding skill over MCP."
---

# Remote project onboarding through the collector backchannel

## 0. Problem

The daemon has no checkout access. A new project becomes searchable only
when the collector that owns its checkout publishes it, and the daemon only
accepts publications for scopes a producer is authorized for. Onboarding a
project therefore needs three facts joined: which checkout, which producer
owns it, and the committed identity. Every one of those facts is derivable
by the collector or the daemon. The only real decision is ownership, and it
has an answer that needs a human only when two producers claim the same
repository.

The target: an agent calls `bbox_project_register(path)`, commits the
`.bbox` identity files it is told to commit, and the project publishes. No
daemon config edit, no deploy, no collector restart, no grant question.

## 1. Model

Four cooperating pieces:

1. **Scope claims (daemon).** A producer whose policy allows it claims an
   unclaimed scope the first time it onboards that scope. The claim is
   durable daemon state, not config.
2. **Collector enrollment (checkout host).** `bbox-code-collector add`
   scaffolds `.bbox`, derives the scope and published ref, records the
   project in an enrolled-projects sidecar, and onboards immediately. The
   running collector reloads its configuration without a restart.
3. **Agent-initiated enrollment (MCP).** The collector reports the roots it
   is willing to enroll under. `bbox_project_register` on a path the daemon
   cannot see routes an enroll command to the covering collector over the
   producer channel and returns the onboard receipt.
4. **Onboarding skill (MCP).** The daemon serves an onboarding skill whose
   body is rendered from live instance facts: known producers, their hosts,
   enroll roots, config paths, claim policy, and the daemon's advertised URL.

## 2. Scope claims

A producer grant authorizes one bearer-token producer as the authoritative
source for exact published scopes `{repo_id, bbox_root_relpath}`: onboarding,
code publication, Git-history and knowledge transport, provenance import,
and delivery of queued checkout mutations. A scope belongs to at most one
producer, and every published member of one repository history belongs to
the same producer.

The effective assignment is the union of two sources:

- **Config pins**: `scopes` on `[[code_collection.producers]]`, as today.
- **Claims**: a durable daemon store of `{producer_id, scope, claimed_at}`
  records.

Producer config gains a claim policy:

```toml
[[code_collection.producers]]
producer_id = "checkout-host"
token_file = "/var/lib/blackbox/secrets/collector.token"
scopes = []                  # optional pins; may be empty under a claim policy
claim_scopes = "unclaimed"   # "none" (default) | "unclaimed"
```

Claim rules, enforced only on the onboard route:

- The producer's policy is `unclaimed`.
- The scope is not pinned or claimed by another producer.
- No other producer holds a scope of the same `repo_id` (the history
  same-producer invariant). A conflict refuses with
  `repo_history_scope_split` and names the owning producer id.
- The request validates as every onboard request does.

The claim is persisted before the register/attach composite runs, and the
producer auth snapshot is rebuilt from config plus claims afterwards, so
publication lanes admit the scope immediately and after every restart or
reload. A composite failure leaves a harmless claim that a retry reuses.

Precedence: a config pin wins over a claim. A claim that conflicts with a
pin for a different producer is ignored at auth build with a warning and
stays in the store for the operator to revoke. Claims made under a policy
remain valid after the policy is set back to `none`; the policy gates new
claims, not existing ones. `blackbox producer-claims list` and
`blackbox producer-claims revoke --producer <id> --scope <repo_id>/<relpath>`
administer the store offline.

An enabled producer with `claim_scopes = "unclaimed"` may have no pinned
scopes.

## 3. Collector enrollment

### 3.1 Enrolled-projects sidecar

The collector config keeps operator-authored `[[projects]]`. Enrolled
projects live in a sidecar with the same `[[projects]]` schema, by default
`<config stem>.enrolled.toml` beside the config, overridable with
`enrolled_projects_file`. The effective project list is config plus
sidecar. A root or scope present in both is a load error for `add` and a
warning with the config entry winning at reload.

### 3.2 Live reload

The running collector re-reads the config and sidecar when either file's
modification time changes, checked on the checkout-mutation cadence. Every
lane pass reads the current snapshot. An invalid replacement keeps the
previous snapshot and logs the error; token and server URL changes still
require a restart.

### 3.3 `add`

```text
bbox-code-collector --config <cfg> add <path> [--ref <full_ref>]
    [--no-git-history] [--no-provenance] [--no-published-knowledge]
```

1. Canonicalize `<path>`; require an existing directory inside a main Git
   worktree with complete (non-shallow) history.
2. Scaffold `.bbox` exactly as `init` does, recording the durable repo_id.
3. Derive the scope from the `.bbox` identity and its location relative to
   the repository root, so a subdirectory yields a subtree scope.
4. Derive the published ref: `--ref`, else the branch `origin/HEAD` names,
   else the current branch.
5. Record the project in the sidecar atomically with Git history,
   provenance, and published knowledge enabled unless disabled by flag.
   Re-adding an enrolled root is idempotent.
6. Onboard the project immediately over the producer channel.
7. Print a JSON receipt: project id, attachment id, scope, published ref,
   whether the identity is committed at that ref, and when it is not, the
   exact files to commit (`.bbox/config.toml`, `.bbox/mcp.json`,
   `.bbox/local/.gitignore`).

`add` never commits, pushes, or edits files outside `.bbox/` and the
sidecar. Publication lanes start on the next pass after the identity is
committed at the published ref.

### 3.4 Enroll roots

```toml
enroll_roots = ["~/repos", "~/orca/workspaces"]
```

Canonicalized absolute directories under which the collector accepts
daemon-routed enroll commands. Empty (the default) disables agent-initiated
enrollment on that host; `add` from the host shell is always available.

## 4. Agent-initiated enrollment

### 4.1 Producer command channel

```text
POST /internal/code-source/v1/producer-commands/poll
POST /internal/code-source/v1/producer-commands/ack
```

The collector polls on the checkout-mutation cadence and presents its
presence: enroll roots, a host label, its config path, an optional service
label, and its version. The daemon keeps presence per producer and returns
queued commands. The only command kind is `enroll {command_id, path,
full_ref?}`. The collector executes an enroll command with the `add`
procedure only when the path is inside its enroll roots, and acks with the
`add` receipt or a typed failure.

Presence and the command queue are daemon memory: a daemon restart drops
queued commands, and the caller retries.

### 4.2 `bbox_project_register` on a remote path

A path the daemon can stat keeps the local register behavior. Otherwise:

1. Select producers whose presence is fresh and whose enroll roots contain
   the path; the longest matching root wins.
2. None: `error.project_onboarding_no_producer`, listing known producers
   with their hosts and enroll roots, and the host-shell `add` command.
3. More than one at equal depth: `error.project_onboarding_ambiguous` with
   the candidate producer ids; the optional `producer` parameter selects.
4. Otherwise enqueue an enroll command (reusing a pending one for the same
   producer and path) and wait a bounded time for its ack.
5. Return the `add` receipt, or on timeout a pending result naming the
   command id; calling again is idempotent.

`bbox_project_init` on a remote path points at `bbox_project_register`,
which scaffolds as part of enrollment.

## 5. Publication defaults

A claimed or pinned project's knowledge and gaps become visible once an
accepted publication pointer exists. Producer config gains
`auto_publish = true`: for a project with no accepted pointer, the first
Ready candidate from the project's owning producer on its enrolled
published ref is established through the same acceptance path as
`bbox_project_publisher_advance(mode="establish")`, and the project's
auto-advance grant is installed on that pointer. Establish never happens
for a project that already has a pointer, and rollback and scope changes
stay manual.

## 6. Onboarding skill over MCP

The daemon declares MCP resources, prompts, and the
`io.modelcontextprotocol/skills` extension, and serves one onboarding skill
three ways:

- `skills/list`, for clients that consume the skills extension;
- `resources/list` and `resources/read` on the skill's `SKILL.md` URI, for
  clients that read MCP resources;
- a prompt, for clients that expose MCP prompts as commands.

The skill body is rendered at read time from instance facts: fresh producer
presence (host label, enroll roots, config path, service label), claim and
auto-publish policy, and the daemon's `advertise_url` when configured. It
states the register call, the commit step, the host-shell `add` fallback,
the verification reads, and for a host with no collector the collector
config to write, with `server_url` set from `advertise_url`. The
remote-path errors from section 4.2 name the skill's resource URI.

## 7. Trust model

A producer token already authorizes publishing the code, history, and
knowledge of every scope it holds. A claim policy extends that to scopes no
other producer holds. The daemon cannot verify checkout possession: a
`repo_id` is a first-commit hash that any clone presents. A claim policy
therefore means that the operator trusts the producer's token holder to
claim repositories nobody else owns. It never lets a producer take a scope
or repository history held by another producer, and config pins stay
authoritative.

Agent-initiated enrollment lets any MCP caller make a collector scaffold
`.bbox` and enroll a checkout, bounded collector-side by the operator's
enroll roots. The collector writes only `.bbox/` scaffolding and its own
sidecar, the same file class the checkout-mutation lane already delivers.

## 8. Startup ordering

- **Pending onboarding.** A pinned catalog-mode scope with no project is
  admitted as pending onboarding: excluded from every publication lane and
  accepted only by the onboard route. Bridge-mode resolution fails closed.
  A claimed scope with no project is admitted the same way.
- **Marker verify vs. crash-wedged staging.** The code-source locality
  startup verify accepts a covered project wedged at `StagingIndex` when the
  workspace manifest and activation journal agree that staging completed;
  any other non-Active state refuses.

## 9. Marker interaction

A newly onboarded project is not covered by any locality marker. It renders
through the named compatibility lanes until an operator runs the relevant
cutover ceremony for it. Knowledge transport coverage classifies it
uncovered; nothing fails open.

## 10. Non-goals

- Claims never reassign a scope or repository history held by another
  producer.
- No checkout writes outside `.bbox/` scaffolding; no commits or pushes by
  the collector.
- No automatic alias acceptance; declared aliases stay pending nominations.
- No automatic marker coverage for new projects.

## 11. Verification

- claims: first claim persists and admits publication lanes; a scope pinned
  or claimed by another producer refuses; a same-repo scope on another
  producer refuses with `repo_history_scope_split`; claims survive restart
  and reload; a pin for another producer overrides a claim; policy `none`
  refuses new claims and keeps old ones; empty pinned scopes allowed only
  under a claim policy; absent, empty, and legacy-shaped claim stores.
- collector: `add` scaffolds, derives root and subtree scopes and the
  published ref, is idempotent, refuses shallow and non-main worktrees,
  reports uncommitted identity; live reload picks up sidecar and config
  changes and keeps the previous snapshot on an invalid replacement.
- command channel: enroll outside enroll roots refuses collector-side;
  producer selection by longest root, ambiguity, no producer, stale
  presence; idempotent re-register; timeout pending result.
- publication defaults: auto-publish establishes once from the owning
  producer's Ready candidate and installs auto-advance; never re-establishes.
- skill: served through skills/list, resources, and prompts; body reflects
  presence and policy; digest matches the served body.
- end to end on the estate: register a throwaway repository from an MCP
  call, commit its identity, observe catalog attachment, first collection,
  search visibility, and published knowledge.
