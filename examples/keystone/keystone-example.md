# Keystone — issue → PR → review → merge arc

End-to-end demonstration of the workflow engine. A Forgejo webhook
fires on issue-opened. The arc dispatches an implementer team to fix
the bug and open a PR, suspends on a `Wait` for the PR-ready signal,
dispatches a reviewer ensemble that posts feedback, loops on
`pr-feedback` until merged or max-iterations reached, then runs
operator-blessed cleanup hooks at terminal state.

This README is a *reference* + *adaptation guide*: read it to
understand what's wired here, then customize for your stack. For the
underlying engine semantics see [Workflow Engine](../../docs/workflows.md).

## What it exercises

| Engine feature                                                                  | Where in this example                                                     |
|---------------------------------------------------------------------------------|---------------------------------------------------------------------------|
| Workflow + subworkflow_ref composition                                          | `workflows/issue-to-merged-pr.json` calls `implementer-arc` and `reviewer-arc` by id |
| Subworkflow imports/exports contract                                            | Implementer exports `pr_number`/`branch`; parent threads them (incl. `worktree_path`) into AddressFeedback |
| `vars_schema` declaration + initial seeding from webhook                        | Every workflow declares one; webhook routing seeds via entity merge        |
| Template heads `${vars.x}` / `${outputs.x}` / `${meta.x}` / `${last_signal.x}` / `${env.X}` | Used everywhere; `${env.FORGEJO_*}` carries credentials into `http_json` URLs/headers |
| Hooks: `set_var` / `inc_var` / `parse_json` / `worktree_create` / `worktree_remove` / `shell` / `http_json` | Setup, AwaitFeedbackOrMerge, PushAndOpenPr, FetchDiff, PostReview, on_arc_exit |
| Hook gating via `when: domain:...` packet                                       | PushAndOpenPr (idempotent create-or-reopen), PostReview (auto-merge on approve), on_arc_exit cleanup |
| Wait nodes with `any_of` race + timeout                                         | AwaitReviewTrigger (24h), AwaitFeedbackOrMerge (7d)                        |
| Synthetic `__timeout__` signal as graceful-degrade path                         | Both Wait gates accept it (route → halt)                                   |
| Branch transitions routing on gate verdict                                      | `AwaitReviewTrigger.next.branch` consumes `merge-or-review`, `AwaitFeedbackOrMerge.next.branch` consumes `loop-or-exit` |
| Gate packets routing on `last_signal.name`                                      | merge-or-review, loop-or-exit                                              |
| Workflow-level policy packet (advisor-as-packet)                                | arc-budget caps step count                                                 |
| Domain-shaped packet refs (`domain:...`)                                        | Every gate/policy/hook-when reference                                      |
| Operator-blessed registries (workflows, webhooks, packets) persisted to disk    | All artifacts installed via `/admin/*` endpoints in `scripts/install.sh`   |
| Webhook ingress with generic `hmac_sha256` signature scheme                     | `webhooks/forgejo.json` (operator names header `X-Gitea-Signature`)        |
| Routing packet → start_arc / signal_arc / ignore                                | `packets/routing-forgejo.json` (operator's mapping; engine knows nothing of forgejo) |
| Webhook `default_project_dir` resolution                                        | Set in `webhooks/forgejo.json`; arcs created from the hook anchor here     |
| Capability tags (no-op here — every actor's `requires` is empty)                | Demonstrates the slot; populate it when picking models with hard requirements |
| Hook-only nodes (empty `actor`)                                                 | `Setup`, `PushAndOpenPr`, `FetchDiff`, `PostReview`, `Done` — fire hooks, no LLM dispatch |
| Generic `http_json` for any code-host integration                               | Issue fetch, PR list, PR create, PR diff (via `response_kind: text`), review post, merge — same op for all |
| Generic `find_first` for client-side array filtering                            | `PushAndOpenPr` GETs ALL open PRs (Forgejo's `head=` filter is unreliable), then `find_first { from: ${vars.all_open_prs}, where: { "head.ref": "${vars.branch}" } }` writes the matching PR (or null) into `vars.existing_pr`. Composable primitive — no platform-specific search op needed. |
| Idempotent re-dispatch                                                          | `PushAndOpenPr` reuses a matching open PR (via `set_var pr_data = ${vars.existing_pr}` gated by `domain:hook-when/has-existing-pr`) instead of paving the prior arc's PR. Re-running an arc on the same issue+branch is safe. |
| Auto-merge on approval                                                          | `reviewer-arc.PostReview` fires `http_json` POST `/merge` gated by `domain:hook-when/should-merge` (verdict-as-data from aggregator) — the merge fires `pull_request closed merged:true` webhook → `pr-merged` signal → arc terminates clean without manual intervention. |
| Reviewer feedback body propagated to AddressFeedback                            | Webhook extractor `Coalesce` projects `review_body` from `.review.body` OR `.comment.body` (Forgejo's review-comment subtypes diverge). Routing carries the extracted entity as `${last_signal.payload}`. AwaitFeedbackOrMerge.on_exit pulls `${last_signal.payload.review_body}` into `vars.feedback_text` — the implementer LLM in `implementer-feedback-arc` sees a clean string of the actual review comment, not the raw webhook body. |
| Typed correlation tuples on signals                                             | Routing rules emit `correlate: {pr: "${entity.pr_number}"}`; AwaitReviewTrigger / AwaitFeedbackOrMerge waits register with `correlate: {pr: "${vars.pr_number}"}`. The dispatch path runs `routing::resolve_entity_template` over the consequent before parse, substituting `${entity.X}` to the typed value from the extracted entity. Two arcs concurrently waiting on different PRs no longer cross-resume. |
| Poller inlet (alternative trigger to webhook)                                   | `pollers/forgejo-open-issues.json` schedules `GET /repos/.../issues?state=open`, explodes the array via `iterate: $`, extracts each issue (synthesizing `event:issues + action:opened` so the existing routing packet matches), dedups by `$.id` (per-poller in-memory ring), dispatches through the same `dispatch_routed_event` the webhook handler uses. Off by default — opt in with `KEYSTONE_INSTALL_POLLERS=1`. See "Choosing an inlet" below. |

## Prerequisites

- Docker (or Podman with `alias docker=podman`)
- `jq`, `curl`, `git`, `python3` (for HMAC signature when you replay manually)
- Forgejo 15 available on `FORGEJO_BASE_URL` (the bundled compose file starts `codeberg.org/forgejo/forgejo:15` on `http://127.0.0.1:3000`)
- `blackboxd` running (default port `7264`; override with `BBOX_PORT`)
- For webhook delivery from Docker, the daemon listener must be reachable from the Forgejo container. On Linux that usually means binding the daemon to `0.0.0.0` or using the poller/direct-dispatch path instead of webhooks. See [Workflow Engine § Operator-blessed registries](../../docs/workflows.md#operator-blessed-registries).
- Daemon environment must include:
  - `FORGEJO_BASE_URL`, `FORGEJO_TOKEN` — for the implementer/reviewer to call the Forgejo API
  - `FORGEJO_WEBHOOK_SECRET` — for the daemon to verify inbound webhook signatures

  Add via systemd drop-in or whatever envsetup pattern your daemon uses. `scripts/bootstrap.sh` writes the values to `.env`; copy them into the daemon environment before running arcs that use `${env.FORGEJO_*}`.
- Brofiles configured. The shipped install creates two:
  - `keystone-impl`   → Claude Sonnet 4.6
  - `keystone-review` (×2 instances `keystone-review` + `keystone-review-b`) → Claude Haiku 4.5

  Override at install time:
  ```sh
  IMPL_BROFILE=my-strong-coder \
  REVIEWER_BROFILE_A=my-haiku-a REVIEWER_BROFILE_B=my-haiku-b \
    ./scripts/install.sh
  ```
  Or edit `workflows/implementer-arc.json` + `workflows/reviewer-arc.json` to reference whatever brofile names you've already configured. **Capability validation refuses to start the arc** if a brofile resolves to a provider that doesn't cover the actor's `requires` set; today the arcs declare empty `requires` so any provider works.

## Quick start

```sh
cd examples/keystone
./scripts/run.sh                    # full path: docker up → bootstrap → install → wait
./scripts/run.sh --dispatch         # skip webhook wait; dispatch arc directly against issue #1
./scripts/run.sh --skip-forgejo     # if Forgejo is already up + bootstrapped
```

## Choosing an inlet: webhook vs poller

The example ships both a webhook spec (`webhooks/forgejo.json`) and a
poller spec (`pollers/forgejo-open-issues.json`). They are **alternatives**,
not complements — both feed the same routing packet → same workflow.
Running both against the same upstream duplicates work (webhook fires
on issue-open; poller's next tick picks up the same open issue).
The workflow-side idempotency (`find_first` + `PushAndOpenPr`'s
conditional create) catches the duplicate at the PR layer, but only
*after* the implementer LLM has already run twice. Pick one for the
common case:

| Deployment shape                                  | Inlet         | How                                                          |
|---------------------------------------------------|---------------|--------------------------------------------------------------|
| Public-ingress daemon, code-host can push         | **Webhook**   | Default install (`./scripts/install.sh`). Lower latency, no polling cost. |
| Closed network / no public ingress / poll-only API | **Poller**    | `KEYSTONE_INSTALL_POLLERS=1 ./scripts/install.sh` AND remove the webhook from Forgejo's hook config (or it'll dispatch in parallel). |
| Resilience-layered (catch missed webhooks)        | **Both**      | `KEYSTONE_INSTALL_POLLERS=1 …`, accept the duplicate-dispatch cost; workflow idempotency catches it. |

The poller and webhook share the routing packet (`packets/routing-forgejo.json`)
on purpose — the routing rules don't care whether the entity arrived
via push or pull. Adapting the demo to a polling-only deploy is just
the install flag plus dropping the webhook from the upstream's config.

## Layout

```
examples/keystone/
├── docker-compose.yaml          # Forgejo single-instance, loopback-only
├── scripts/
│   ├── bootstrap.sh             # admin user, API token, repo, seed bug, webhook config
│   ├── install.sh               # compile packets, install brofiles/teams/workflows/webhook
│   └── run.sh                   # docker up → bootstrap → install → wait | --dispatch
├── packets/
│   ├── routing-forgejo.json            # webhook event → routing verdict
│   ├── gate-merge-or-review.json       # AwaitReviewTrigger gate
│   ├── gate-loop-or-exit.json          # AwaitFeedbackOrMerge gate
│   ├── cleanup-policy.json             # keep-on-fail / delete-on-success
│   ├── policy-arc-budget.json          # arc-level budget guard
│   ├── hook-when-no-existing-pr.json   # PushAndOpenPr: gate POST /pulls when no open PR matches branch
│   ├── hook-when-has-existing-pr.json  # PushAndOpenPr: gate "reuse pr_data from find_first match" when one does
│   └── hook-when-should-merge.json     # PostReview: gate POST /merge when aggregator verdict is "merge"
├── webhooks/
│   └── forgejo.json             # extractor + signature scheme + routing packet ref
└── workflows/
    ├── implementer-arc.json          # subworkflow: fetch issue → fix → push → open PR
    ├── implementer-feedback-arc.json # subworkflow: revise based on review feedback → push
    ├── reviewer-arc.json             # subworkflow: review PR → post comment + verdict
    └── issue-to-merged-pr.json       # main keystone arc
```

## Walkthrough

### What happens when the webhook arrives

1. Forgejo POSTs `issues.opened` to `http://172.17.0.1:${BBOX_PORT:-7264}/webhook/forgejo` with `X-Gitea-Event: issues` + `X-Gitea-Signature: <hex>`.
2. Daemon verifies the HMAC against `${FORGEJO_WEBHOOK_SECRET}`.
3. Idempotency dedup against the `X-Gitea-Delivery` UUID (resends are dropped silently).
4. **Extractor** projects the body + headers into a flat entity:
   ```jsonc
   { "event": "issues", "action": "opened",
     "issue_number": 42, "issue_title": "...",
     "owner": "keystone-admin", "repo": "transcript-search-fork", ... }
   ```
5. **Routing packet** (`domain:webhook-routing/forgejo`) classifies → `start_arc` verdict with `workflow: "issue-to-merged-pr"`.
6. Daemon merges the extracted entity into `initial_vars` and spawns the arc with `project_dir = WebhookSpec.default_project_dir = /tmp/keystone-fork-clone`.

### What the arc does

Control flow is encoded as per-node `next` transitions in `workflows/issue-to-merged-pr.json`:

```
start: Setup
Setup            → goto:Implement
Implement        → goto:AwaitReviewTrigger
AwaitReviewTrigger → branch{ready→Review, merged→Done}        (verdict from gate: merge-or-review)
Review           → goto:AwaitFeedbackOrMerge
AwaitFeedbackOrMerge → branch{feedback→AddressFeedback, merged→Done}  (verdict from gate: loop-or-exit)
AddressFeedback  → goto:AwaitReviewTrigger                    (back-edge — the feedback cycle)
Done             → terminal
```

| Node                | Kind                       | What happens                                                                 |
|---------------------|----------------------------|------------------------------------------------------------------------------|
| `Setup`             | Hook-only (empty `actor`)  | Initialize counter vars, derive branch name, `WorktreeCreate`, capture `worktree_path` into vars for sub-arcs. |
| `Implement`         | subworkflow_ref            | Runs `implementer-arc`: `http_json` GET issue → LLM edits + commits → `shell` push → idempotent `http_json` POST/PATCH PR. Exports `pr_number`/`branch`. |
| `AwaitReviewTrigger`| Wait `any_of [pr-ready, pr-merged]`, 24h timeout, `gate: merge-or-review` | Suspends until pr-ready or pr-merged; gate emits `ready`/`merged` verdict; node's `next.branch.cases` routes accordingly. |
| `Review`            | subworkflow_ref            | Runs `reviewer-arc`: `http_json` GET diff (`response_kind: text`) → ensemble reviewers emit verdicts → single-actor aggregator emits `{event, body, action: "merge"\|"request_changes"}` JSON → PostReview parses, posts COMMENT, and `http_json` POST `/merge` gated by `should-merge`. |
| `AwaitFeedbackOrMerge` | Wait `any_of [pr-feedback, pr-merged]`, 7d timeout, `gate: loop-or-exit` | Suspends; `on_exit` captures last_signal payload + increments `review_iteration`. Gate emits `merged`/`feedback`/`halt`; node's `next.branch.cases` routes accordingly. |
| `AddressFeedback`   | subworkflow_ref            | Runs `implementer-feedback-arc`: revise + commit → on_exit `shell` push → fires `pull_request.synchronize` webhook → `next.goto: AwaitReviewTrigger` closes the cycle. |
| `Done`              | Hook-only + `next.terminal`| Terminal node. Triggers `on_arc_exit` hooks at workflow level. |

`on_arc_exit` runs `WorktreeRemove` gated by `domain:workflow-cleanup/keep-on-fail` — keeps the worktree when `meta.arc_outcome` ∈ {`failed`, `cancelled`, `timeout`}, deletes on success.

### What the implementer / reviewer LLMs actually do

LLMs do cognitive work only. Mechanical pieces (HTTP calls, git operations, JSON parsing, idempotency, gating) are workflow JSON composing the engine's generic ops. The engine knows nothing about Forgejo.

The implementer (`workflows/implementer-arc.json`):

1. **FetchIssue.on_enter hooks** (mechanical):
   - `http_json` GET `${env.FORGEJO_BASE_URL}/api/v1/repos/${vars.owner}/${vars.repo}/issues/${vars.issue_number}` with bearer auth from `${env.FORGEJO_TOKEN}` → response captured into `vars.issue_data`.
   - `set_var` extracts `vars.issue_title` / `vars.issue_body` from `${vars.issue_data.*}`.
   - `set_var` derives `vars.branch = "fix/issue-${vars.issue_number}"`.
2. **FetchIssue prompt** (LLM): edit files in `${vars.worktree_path}` on `${vars.branch}` to fix the issue, commit. Do NOT push or open PR — that's mechanical.
3. **PushAndOpenPr** (Noop with hooks — idempotent re-dispatch):
   - `shell` `git push -u origin ${vars.branch}` (cwd=`${vars.worktree_path}`).
   - `http_json` GET `pulls?state=open&limit=50` → `vars.all_open_prs`. Forgejo's `head=` query filter is broken in some versions, so we fetch the open set and filter client-side.
   - `find_first { from: ${vars.all_open_prs}, where: { "head.ref": "${vars.branch}" } }` → `vars.existing_pr` (PR object or null).
   - `http_json` POST `/pulls` (gated by `domain:hook-when/no-existing-pr`) → `vars.pr_data` — fires only when no open PR matches.
   - `set_var pr_data = ${vars.existing_pr}` (gated by `domain:hook-when/has-existing-pr`) — reuses the matching PR otherwise.
   - `set_var pr_number = ${vars.pr_data.number}` (always — both paths populate `pr_data`).

The reviewer (`workflows/reviewer-arc.json`):

1. **FetchDiff.on_enter** (mechanical): `http_json` GET `pulls/${vars.pr_number}.diff` with `response_kind: text` → `vars.pr_diff`.
2. **Review node** (ensemble of two reviewers): each gets the diff in their prompt, returns one line: `APPROVE` or `REQUEST CHANGES: <reason>`.
3. **Aggregate node** (single executor — must be single, not ensemble, so the JSON output is a single document not a merged blob): emits strict JSON `{event: "COMMENT", body, action: "merge"|"request_changes"}`. `event` is always COMMENT because Forgejo refuses APPROVED/REQUEST_CHANGES on a self-authored PR (single-user demo).
4. **PostReview** (Noop with hooks):
   - `parse_json ${Aggregate.output}` → `vars.review_payload`.
   - `http_json` POST `/pulls/${vars.pr_number}/reviews` with the comment body.
   - `http_json` POST `/pulls/${vars.pr_number}/merge` **gated by `domain:hook-when/should-merge`** (`vars.review_payload.action == "merge"`) — fires `pull_request closed merged:true` webhook → routes to `pr-merged` signal → AwaitFeedbackOrMerge resolves on merged → arc terminates clean.

The feedback-loop subworkflow (`workflows/implementer-feedback-arc.json`) runs in the SAME worktree on the SAME branch — the implementer addresses feedback + commits, on_exit `shell` push fires `pull_request.synchronize` → resumes the parent on `pr-ready`. The `vars.feedback_text` it receives is the literal reviewer comment body (extracted via the webhook's `Coalesce` projection over Forgejo's divergent `pull_request_review` / `pull_request_review_comment` payloads), not the raw webhook entity — so the LLM gets a focused string to act on.

## Live observation

The MCP-side observability surface (preferred — these stay inside the tool surface and don't need raw HTTP):

| Goal | Tool |
|---|---|
| Live in-flight arc state + pending waits | `bro_arc_status` |
| Recent signal-dispatch events: matched vs idle, with the pending-wait diff on idle | `bro_signals(signal=, since=, outcome=)` |
| Recent webhook deliveries: extracted entity + routing verdict + response | `bro_webhook_deliveries(name=, since=)` |
| Replay a synthetic webhook payload through the routing packet without firing an arc | `bro_webhook_replay(name, body, headers)` |
| Cancel a runaway / mis-dispatched arc | `bro_arc_cancel(arc_id)` |
| Arc audit trail + latest compaction anchor | `bbox_notes(thread_id=<arc>)` |

Canonical "an arc is stuck, why?" loop: `bro_arc_status` → see which node + the wait correlation → `bro_signals(signal=<name>)` to see if the signal arrived (and on `no_matching_wait`, what waits had the same signal name with what correlations) → if no signal at all, `bro_webhook_deliveries(name=<webhook>)` to walk back to whether the webhook arrived and how it routed → if routing classified `ignore` / `no_match` for an event you expected to route, `bro_webhook_replay` to iterate on the rule.

HTTP surfaces (when MCP isn't available — e.g. shell scripts):

```sh
# stream every event the engine emits (SSE)
curl -N http://127.0.0.1:${BBOX_PORT:-7264}/tail

# all in-flight arcs with current node + completed nodes + visit counts
curl http://127.0.0.1:${BBOX_PORT:-7264}/orchestrate/peek | jq

# arc-specific note trail (audit) + latest compaction anchor
bro orchestrate status <arc_thread_id>

# replay a webhook payload through extractor + routing packet WITHOUT
# dispatching — debugging gold for routing rule iteration
curl -X POST -H 'Content-Type: application/json' \
     -H 'X-Gitea-Event: issues' \
     -d '{"action":"opened","issue":{"number":42,"title":"x"},
          "repository":{"name":"r","owner":{"login":"o"}}}' \
     http://127.0.0.1:${BBOX_PORT:-7264}/webhook/forgejo/replay | jq
```

## Customization

### Adapting to a different code-host (GitHub instead of Forgejo)

1. **Extractor** — GitHub puts the event type in `X-GitHub-Event` instead of `X-Gitea-Event`. Edit `webhooks/forgejo.json` (rename to `webhooks/github.json` for clarity), change the `event` selector path to `$._headers.x-github-event`. Most other paths (`$.action`, `$.issue.number`, `$.pull_request.number`) are identical.
2. **Signature** — Keep `signature.kind: hmac_sha256`; change `header` to `X-Hub-Signature-256` and add `prefix: "sha256="`. Secret env name is operator-controlled.
3. **API calls** — Forgejo and GitHub diverge on the `pulls` endpoint shape. All API calls in this example are `http_json` hooks templated against `${env.FORGEJO_BASE_URL}` + `${env.FORGEJO_TOKEN}`; for GitHub, set `${env.GITHUB_BASE_URL}=https://api.github.com` (or a per-host env var of your choice) and adjust the path templates in the workflow JSON. No engine changes required.
4. **Routing packet** — keep the rule shapes; only `event` value matchers change in some edge cases (`pull_request_review` event has `action: "submitted"` on both — no change needed).

### Adapting to a different code-fix shape (not "fix a bug")

The keystone is a *fix-this-issue* arc, but the same skeleton works for:

- **Triage** arcs — replace `Implement` with a subworkflow that classifies the issue + adds labels via Forgejo API. No PR; arc terminates after the triage decision.
- **Spec authoring** arcs — `Implement` writes a design doc commit to a branch; reviewer proposes shape changes in PR comments; merge-on-approval.
- **Dependency update** arcs — `Implement` runs `cargo update` + `cargo test`; PR contains the lockfile diff.

The shape constant: webhook → extract → route → start_arc → subworkflow → wait-on-PR-event → loop until terminal verdict → cleanup.

### Adapting to a different model stack

Edit `workflows/*.json` `actors[*].brofile` (or pass `IMPL_BROFILE=` / `REVIEWER_BROFILE_A=` to `install.sh`). If the actor needs structured output, declare:

```jsonc
"actors": {
  "structured_review": {
    "kind": "ensemble",
    "team": "review-team",
    "requires": ["structured_output"]
  }
}
```

Capability validation will refuse to install/dispatch the arc if any team member's brofile resolves to a provider lacking that capability — see [Workflow Engine § Capability tags](../../docs/workflows.md#capability-tags).

### Adapting the loop semantics

Concrete tweaks you'll likely want:

- **Concurrent arcs on different PRs.** Wired by default: both `Wait` nodes correlate on `{ "pr": "${vars.pr_number}" }` and the routing packet's `signal_arc` verdicts emit `correlate: {"pr": "${entity.pr_number}"}`. The dispatch path runs `routing::resolve_entity_template` over the consequent before parse, so a `pr-merged` for PR #117 only resumes the arc waiting on PR #117 — concurrent arcs on different PRs don't cross-resume. To revert to broadcast match (single-arc demos that don't care about pinning), drop the `correlate` keys from both `Wait` nodes AND the routing rules.
- **Push-storm debouncing.** Three commits to the PR fire three `pull_request.synchronize` webhooks → three `pr-ready` signals → three reviewer dispatches. Add a `vars.review_in_progress` flag in `Review`'s `on_enter`/`on_exit`, then add a routing rule that ignores `synchronize` events while it's set. Pattern documented in [Workflow Engine § Webhook](../../docs/workflows.md#webhook); not wired here for clarity.
- **Different cleanup policy.** Edit `packets/cleanup-policy.json`. Three options ship as templates:
  - `keep-on-fail` (default here) — keep worktree for failed/cancelled/timeout, delete on success
  - `always-delete` — change the rule to fire-default on any outcome
  - `always-keep` — invert
- **Different arc budget.** Edit `packets/policy-arc-budget.json`. Today: warn at step 50, halt at step 100.
- **Different max-iterations on the feedback loop.** Edit `workflows/issue-to-merged-pr.json` Setup hooks — `set_var max_iterations 5` is the only knob. Beyond 5 review rounds, `gate-loop-or-exit.json` halts the arc.

### Adding a new actor

1. Pick a `kind` (`executor` / `ensemble` / `advisor` / `user`). For pure hook-host / routing-only nodes, leave `actor` empty (`""`) instead of declaring a placeholder actor.
2. Reference an existing brofile (or upsert a new one via `/admin/brofile/upsert` or `bro_brofile`).
3. Add to `actors` in the appropriate workflow spec.
4. Reference from a node via `actor: <name>`.
5. If it has hard provider requirements, declare `requires: [Capability,...]`.

### Adding a new hook op

The op catalog is in `src/workflow/ops.rs`. Add a variant to `OpKind` + a handler function + a test. Update [Workflow Engine § Op catalog](../../docs/workflows.md#op-catalog-current) when you do.

For most external API calls, `http_json` is the primitive: workflow author writes the URL/headers/body as templates over `${env.X}` (credentials, base URLs) and `${vars.X}` (workflow state); the response lands in `vars[into_var]` for downstream templates. The engine carries no platform-specific knowledge — adapting to GitHub, GitLab, Gitea, Bitbucket, or anything else is a workflow JSON change, not a code change.

### Adding a new webhook source

1. Write an extractor projecting the source's payload shape into a flat entity matching whatever your workflow's `vars_schema` expects.
2. Write a routing packet whose rules emit JSON-encoded `start_arc` / `signal_arc` consequents.
3. Pick a signature scheme (HMAC-SHA256 supported as `forgejo` or `github`; pure-loopback testing accepts `none`).
4. Set `default_project_dir` if any arcs the webhook spawns will use `WorktreeCreate`.
5. Install via `bro_webhook_install` or `POST /admin/webhook/install`; persisted to `${BRO_HOME}/webhooks/<name>.json` and restored on daemon restart.

## Identity extension (Phase 7 acceptance)

The base workflows (`implementer-arc`, `reviewer-arc`, `issue-to-merged-pr`)
authenticate every Forgejo API call with `${env.FORGEJO_TOKEN}` — a single
operator-supplied admin token. That works for a single-user demo, but it
means **Forgejo sees one principal for both the PR author and the
reviewer**, which on a multi-user host gets rejected as self-approval.

The identity extension adds three artifacts and wires them as a parallel
top-level workflow:

| Identity artifact | Role |
|---|---|
| `workflows/implementer-arc-with-identity.json` | Same shape as `implementer-arc.json`, but starts with `RequestIdentity` → `require_identity` (scope=forgejo, bro=keystone-impl), branches `ready`/`pending` on `domain:workflow-gate/identity-status`, loops via `AwaitIdentity` on `bro.identity.provisioned`. All Forgejo HTTP calls use `secret_headers` from `vars.identity_result.identity.token_ref` — no `${env.FORGEJO_TOKEN}`. |
| `workflows/reviewer-arc-with-identity.json` | Same shape, bro=keystone-review. Reviews post via the mapped reviewer token. |
| `workflows/issue-to-merged-pr-with-identity.json` | Top-level wrapper that calls the two identity subworkflows and threads `forgejo_instance` through their imports. It terminates after the reviewer subworkflow because that subworkflow posts the review and performs the merge itself. |

The base `implementer-feedback-arc.json` remains identity-agnostic. The
identity acceptance wrapper does not use the feedback loop; it proves the
Forgejo principal split on the implementer PR and reviewer review path.

Per-bro token storage lives in the reaction. `forgejo-ensure-bro-user`
provisions one Forgejo user per (bro, provider, model) triple, writes
its token to a `secret:<name>` slot, and persists the mapping via
`IdentityRegistry`. Workflows resolve `${vars.identity_result.identity.token_ref}`
to a `secret:<name>` reference, which `secret_headers` expands at HTTP
request time. The raw token never enters vars, outbox rows, replay
output, or daemon logs.

### Smoke

`scripts/identity-smoke.sh` is an operator-runnable acceptance check
(not a unit test — it touches a real Forgejo). It:

1. Verifies the four required workflows and the `forgejo-ensure-bro-user`
   reaction are installed by reading their on-disk JSON files under
   `${BRO_HOME}/workflows/` and `${BRO_HOME}/reactions/` — there is no
   HTTP list endpoint for workflows, reactions, or identity mappings on
   the current daemon, so the script verifies the persisted store
   directly.
2. Dispatches `issue-to-merged-pr-with-identity` against
   `${FORGEJO_OWNER}/${FORGEJO_REPO}` issue `${ISSUE_NUMBER}` on
   instance `${FORGEJO_INSTANCE}` via the real
   `POST /orchestrate/by-id` endpoint with `{workflow_id, project_dir,
   initial_vars}` (same shape `scripts/run.sh --dispatch` uses).
3. Polls the Forgejo API for the resulting PR and its first review.
4. **Fails non-zero** if the PR author and the review author are the
   same Forgejo login (i.e. the identity mapping did not separate
   principals), or if the on-disk identity file
   `${BRO_HOME}/identities/forgejo/${FORGEJO_INSTANCE}.json` lacks an
   exact `(subject, provider, model)` row for both `bro:keystone-impl`
   (`claude` / `claude-sonnet-4-6`) and `bro:keystone-review` (`claude`
   / `claude-haiku-4-5-20251001`).

Run it with `$PROJECT_DIR` (local clone path, like `run.sh`),
`$FORGEJO_BASE_URL`, `$FORGEJO_ADMIN_TOKEN` (admin token used for
verification reads only, never for the workflow itself),
`$FORGEJO_OWNER`, `$FORGEJO_REPO`, `$FORGEJO_INSTANCE`, and
`$ISSUE_NUMBER` set in the environment. `jq` and `curl` are required
on the operator's PATH. The base `bootstrap.sh` / `install.sh` flow
under this directory writes the operator token and webhook secret;
the smoke adds the identity-side checks on top.

Verified against local Forgejo 15.0.2 on May 14, 2026 with a fresh
`FORGEJO_INSTANCE`: the implementer identity was provisioned before
`AwaitIdentity` registered, the correlated system-event catch-up resolved
the wait, and the smoke completed with distinct PR/review principals.

## Tear-down

```sh
docker compose down -v          # wipes Forgejo data
rm -f .env                       # admin token + webhook secret per-host

# Daemon-side state (workflows + webhooks + compiled packets) persists.
# Remove via:
bbox_forget <packet-id>          # or POST /admin/... — bro_workflow_uninstall
                                 # / bro_webhook_uninstall don't exist yet;
                                 # rm the on-disk JSON files for now:
rm ~/.local/state/blackbox/bro/{webhooks,workflows}/*.json
systemctl --user restart blackbox.service   # picks up the deletion
```

## Known gaps + watch points

| Gap                                    | Impact                                                                 | Fix sketch                                            |
|----------------------------------------|------------------------------------------------------------------------|-------------------------------------------------------|
| Push-storm not debounced via `vars.review_in_progress` | First reviewer dispatch is not yet pinned per-PR via the engine's idempotency layer | Add a hook + packet that flips an in-progress flag on review entry/exit. Independent of the per-PR correlation already wired (waits + routing carry `{pr: ${entity.pr_number}}`); this would dedup duplicate _dispatches_ for the same PR. |
| Push-storm not debounced               | N commits → N `pull_request.synchronize` events → N reviewer dispatches  | `vars.review_in_progress` flag + routing-rule guard; see [Workflow Engine § Webhook](../../docs/workflows.md#webhook). |
| PR reuse only handles open PRs         | A closed-but-unmerged PR for the same head will conflict on POST → halt. The `state=open&limit=50` GET also doesn't paginate. | Either paginate + scan all PRs (need `find_first` over multiple pages, or a follow-up `find_all` op) OR widen to `state=all` and add a PATCH-reopen branch — the workflow is already wired to handle either via packet gating. |
| `Shell` op has no allowlist enforcement| Trusted-actor assumption                                                | Wire shell-policy packet; see [Workflow Engine](../../docs/workflows.md). |
| `cancel_arc` routing verdict           | Webhook can't terminate a running arc                                   | Engine cancellation primitive (phase-next)            |
| `bro_workflow_uninstall` / `bro_webhook_uninstall` don't exist | Tear-down requires `rm` + restart                       | Add inverse MCP tools + matching `/admin/` endpoints  |
| `WaitStore` is in-memory only          | Daemon restart loses every suspended arc                                | Disk-back: serialize on register, drop on resolve     |
| Real LLM dispatch every run            | Burns tokens. No simulator mode                                         | `--dry-run` validates spec only; for actor simulation, a `simulator` actor kind is phase-next |
| Self-review COMMENT event              | Forgejo refuses APPROVED / REQUEST_CHANGES on a self-authored PR. The aggregator emits `event: COMMENT` with the verdict in the body so single-user demos work; on a multi-user host you can switch to APPROVED / REQUEST_CHANGES. | Aggregator prompt + downstream routing on `pull_request_review.{approved,rejected}` events. |
