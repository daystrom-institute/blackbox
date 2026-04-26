# Keystone — issue → PR → review → merge arc

End-to-end demonstration of the workflow engine. A Forgejo webhook
fires on issue-opened. The arc dispatches an implementer team to fix
the bug and open a PR, suspends on a `Wait` for the PR-ready signal,
dispatches a reviewer ensemble that posts feedback, loops on
`pr-feedback` until merged or max-iterations reached, then runs
operator-blessed cleanup hooks at terminal state.

This README is a *reference* + *adaptation guide*: read it to
understand what's wired here, then customize for your stack. For the
underlying engine semantics see [`../../WORKFLOWS.md`](../../WORKFLOWS.md).

## What it exercises

| Engine feature                                                                  | Where in this example                                                     |
|---------------------------------------------------------------------------------|---------------------------------------------------------------------------|
| Workflow + subworkflow_ref composition                                          | `workflows/issue-to-merged-pr.json` calls `implementer-arc` and `reviewer-arc` by id |
| Subworkflow imports/exports contract                                            | Implementer exports `pr_number`/`branch`; parent threads them through      |
| `vars_schema` declaration + initial seeding from webhook                        | Every workflow declares one; webhook routing seeds via entity merge        |
| `${vars.x}` / `${meta.x}` / `${last_signal.x}` template resolution               | Every node prompt + every hook arg                                         |
| Hooks: SetVar / IncVar / WorktreeCreate / WorktreeRemove / ParseJson / Forgejo* | Setup, AwaitFeedbackOrMerge, on_arc_exit                                   |
| Hook gating via `when: domain:...` packet                                       | on_arc_exit cleanup conditional on `meta.arc_outcome`                      |
| Wait nodes with `any_of` race + timeout                                         | AwaitReviewTrigger (24h), AwaitFeedbackOrMerge (7d)                        |
| Synthetic `__timeout__` signal as graceful-degrade path                         | Both Wait gates accept it (route → halt)                                   |
| Gate packets routing graph branches by `last_signal.name`                       | merge-or-review, loop-or-exit                                              |
| Workflow-level policy packet (advisor-as-packet)                                | arc-budget caps step count                                                 |
| Domain-shaped packet refs (`domain:...`)                                        | Every gate/policy/hook-when reference                                      |
| Operator-blessed registries (workflows, webhooks, packets) persisted to disk    | All artifacts installed via `/admin/*` endpoints in `scripts/install.sh`   |
| Webhook ingress with HMAC-SHA256 signature verification                         | `webhooks/forgejo.json` + Forgejo bootstrap configures the hook            |
| Routing packet → start_arc / signal_arc / ignore                                | `packets/routing-forgejo.json`                                             |
| Webhook `default_project_dir` resolution                                        | Set in `webhooks/forgejo.json`; arcs created from the hook anchor here     |
| Capability tags (no-op here — every actor's `requires` is empty)                | Demonstrates the slot; populate it when picking models with hard requirements |
| Noop actor for hook-only nodes                                                  | `Setup` and `Done` nodes — fire hooks, no LLM dispatch                     |

## Prerequisites

- Docker (or Podman with `alias docker=podman`)
- `jq`, `curl`, `git`, `python3` (for HMAC signature when you replay manually)
- `blackboxd` running (default port `7264`; the dev daemon at `7265` works too — see `BBOX_PORT`)
- Daemon listener bound to `0.0.0.0` so the Docker'd Forgejo can deliver webhooks. The shipped `deploy/blackbox-dev.service` already does this; the prod template is loopback by default. See [`../../WORKFLOWS.md` § Operator-blessed registries](../../WORKFLOWS.md#operator-blessed-registries).
- Daemon environment must include:
  - `FORGEJO_BASE_URL`, `FORGEJO_TOKEN` — for the implementer/reviewer to call the Forgejo API
  - `FORGEJO_WEBHOOK_SECRET` — for the daemon to verify inbound webhook signatures

  Add via systemd drop-in (`~/.config/systemd/user/blackbox-dev.service.d/keystone-secrets.conf`) or whatever envsetup pattern your daemon uses. `scripts/bootstrap.sh` writes the values to `.env`; copy them into the drop-in.
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

## Layout

```
examples/keystone/
├── docker-compose.yaml          # Forgejo single-instance, loopback-only
├── scripts/
│   ├── bootstrap.sh             # admin user, API token, repo, seed bug, webhook config
│   ├── install.sh               # compile packets, install brofiles/teams/workflows/webhook
│   └── run.sh                   # docker up → bootstrap → install → wait | --dispatch
├── packets/
│   ├── routing-forgejo.json     # webhook event → routing verdict
│   ├── gate-merge-or-review.json
│   ├── gate-loop-or-exit.json
│   ├── cleanup-policy.json      # keep-on-fail / delete-on-success
│   └── policy-arc-budget.json   # arc-level budget guard
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

1. Forgejo POSTs `issues.opened` to `http://172.17.0.1:7265/webhook/forgejo` with `X-Gitea-Event: issues` + `X-Gitea-Signature: <hex>`.
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

```mermaid
stateDiagram-v2
    [*] --> Setup
    Setup --> Implement
    Implement --> AwaitReviewTrigger
    AwaitReviewTrigger --> Review : ready
    AwaitReviewTrigger --> Done : merged
    Review --> AwaitFeedbackOrMerge
    AwaitFeedbackOrMerge --> AddressFeedback : feedback
    AwaitFeedbackOrMerge --> Done : merged
    AddressFeedback --> AwaitReviewTrigger
    Done --> [*]
```

| Node                | Kind                       | What happens                                                                 |
|---------------------|----------------------------|------------------------------------------------------------------------------|
| `Setup`             | Noop + on_enter hooks      | Initialize counter vars, derive branch name, `WorktreeCreate` for this arc. |
| `Implement`         | subworkflow_ref            | Runs `implementer-arc`: fetch issue → LLM edits files → commit → open PR. Exports `pr_number`. |
| `AwaitReviewTrigger`| Wait `any_of [pr-ready, pr-merged]`, 24h timeout | Suspends arc until either PR is ready for review or already merged. |
| `Review`            | subworkflow_ref            | Runs `reviewer-arc`: ensemble reviews PR → posts consolidated comment + APPROVE / REQUEST_CHANGES. |
| `AwaitFeedbackOrMerge` | Wait `any_of [pr-feedback, pr-merged]`, 7d timeout | Suspends arc; `on_exit` hooks capture feedback into `vars.feedback_text` and `inc_var review_iteration`. |
| `AddressFeedback`   | subworkflow_ref            | Runs `implementer-feedback-arc`: revise + push → fires new `pull_request.synchronize` webhook → loops back to AwaitReviewTrigger. |
| `Done`              | Noop                       | Terminal node. Triggers `on_arc_exit` hooks at workflow level. |

`on_arc_exit` runs `WorktreeRemove` gated by `domain:workflow-cleanup/keep-on-fail` — keeps the worktree when `meta.arc_outcome` ∈ {`failed`, `cancelled`, `timeout`}, deletes on success.

### What the implementer / reviewer LLMs actually do

The implementer (`workflows/implementer-arc.json`):

1. `on_enter` hook: `forgejo_issue_fetch` → writes the issue title + body to `vars.issue_title` / `vars.issue_body`.
2. `on_enter` hook: `set_var` derives `vars.branch = "fix/issue-${vars.issue_number}"`.
3. **FetchIssue** node (`keystone-impl` actor) gets a prompt naming the worktree, branch, issue title + body, and instructions to fix-the-bug-then-commit-but-don't-push-yet.
4. **OpenPr** node uses the same durable session and asks the implementer to push and open the Forgejo PR (via `tea` if installed, else raw `curl`). The integer PR number is captured into `vars.pr_number` via `on_exit` hook.

The reviewer (`workflows/reviewer-arc.json`):

1. **Review** node — ensemble of two haiku reviewers each fetch the PR diff and respond with `APPROVE` or `REQUEST CHANGES: <reason>`.
2. **PostFeedback** node — same ensemble aggregates and posts ONE consolidated comment + a Forgejo PR review (APPROVED or REQUEST_CHANGES). The PR review fires `pull_request_review.submitted` → resumes the parent arc.

The feedback-loop subworkflow (`workflows/implementer-feedback-arc.json`) runs in the SAME worktree on the SAME branch — the implementer pushes additional commits, which fires `pull_request.synchronize` → resumes the parent on `pr-ready`.

## Live observation

```sh
# stream every event the engine emits (SSE)
curl -N http://127.0.0.1:7265/tail

# all in-flight arcs with current node + completed nodes + visit counts
curl http://127.0.0.1:7265/orchestrate/peek | jq

# arc-specific note trail (audit) + latest compaction anchor
bro orchestrate status <arc_thread_id>

# replay a webhook payload through extractor + routing packet WITHOUT
# dispatching — debugging gold for routing rule iteration
curl -X POST -H 'Content-Type: application/json' \
     -H 'X-Gitea-Event: issues' \
     -d '{"action":"opened","issue":{"number":42,"title":"x"},
          "repository":{"name":"r","owner":{"login":"o"}}}' \
     http://127.0.0.1:7265/webhook/forgejo/replay | jq
```

## Customization

### Adapting to a different code-host (GitHub instead of Forgejo)

1. **Extractor** — GitHub puts the event type in `X-GitHub-Event` instead of `X-Gitea-Event`. Edit `webhooks/forgejo.json` (rename to `webhooks/github.json` for clarity), change the `event` selector path to `$._headers.x-github-event`. Most other paths (`$.action`, `$.issue.number`, `$.pull_request.number`) are identical.
2. **Signature** — Switch the `signature.kind` to `github`; secret env name still works the same way.
3. **API ops** — Forgejo and GitHub diverge on the `pulls` endpoint shape. The implementer/reviewer prompts use raw `curl` to talk to the API; replace `${FORGEJO_BASE_URL}` (`https://api.github.com`) and adjust the path templates.
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

Capability validation will refuse to install/dispatch the arc if any team member's brofile resolves to a provider lacking that capability — see [`WORKFLOWS.md` § Capability tags](../../WORKFLOWS.md#capability-tags).

### Adapting the loop semantics

Concrete tweaks you'll likely want:

- **Concurrent arcs on different PRs.** Today both `Wait` nodes correlate on `{}` (broadcast match). For multi-arc concurrency, change to `{ "pr": "${vars.pr_number}" }` so a `pr-merged` for PR #117 only resumes the arc waiting on PR #117. Routing packet's signal_arc verdict needs to extract `pr_number` from the entity into `correlate` too — currently it ships `correlate: {}`.
- **Push-storm debouncing.** Three commits to the PR fire three `pull_request.synchronize` webhooks → three `pr-ready` signals → three reviewer dispatches. Add a `vars.review_in_progress` flag in `Review`'s `on_enter`/`on_exit`, then add a routing rule that ignores `synchronize` events while it's set. Pattern documented in [`WORKFLOWS.md` § Webhook ingress](../../WORKFLOWS.md#webhook-ingress); not wired here for clarity.
- **Different cleanup policy.** Edit `packets/cleanup-policy.json`. Three options ship as templates:
  - `keep-on-fail` (default here) — keep worktree for failed/cancelled/timeout, delete on success
  - `always-delete` — change the rule to fire-default on any outcome
  - `always-keep` — invert
- **Different arc budget.** Edit `packets/policy-arc-budget.json`. Today: warn at step 50, halt at step 100.
- **Different max-iterations on the feedback loop.** Edit `workflows/issue-to-merged-pr.json` Setup hooks — `set_var max_iterations 5` is the only knob. Beyond 5 review rounds, `gate-loop-or-exit.json` halts the arc.

### Adding a new actor

1. Pick a `kind` (`executor` / `ensemble` / `advisor` / `user` / `noop`).
2. Reference an existing brofile (or upsert a new one via `/admin/brofile/upsert` or `bro_brofile`).
3. Add to `actors` in the appropriate workflow spec.
4. Reference from a node via `actor: <name>`.
5. If it has hard provider requirements, declare `requires: [Capability,...]`.

### Adding a new hook op

The op catalog is in `src/workflow/ops.rs`. Add a variant to `OpKind` + a handler function + a test. Update [`WORKFLOWS.md` § Op catalog](../../WORKFLOWS.md#op-catalog-current) when you do.

For most external API calls, the `forgejo_*` ops are the model: they return a JSON value, and `into_var` writes it to `vars[<name>]` for downstream nodes to template against.

### Adding a new webhook source

1. Write an extractor projecting the source's payload shape into a flat entity matching whatever your workflow's `vars_schema` expects.
2. Write a routing packet whose rules emit JSON-encoded `start_arc` / `signal_arc` consequents.
3. Pick a signature scheme (HMAC-SHA256 supported as `forgejo` or `github`; pure-loopback testing accepts `none`).
4. Set `default_project_dir` if any arcs the webhook spawns will use `WorktreeCreate`.
5. Install via `bro_webhook_install` or `POST /admin/webhook/install`; persisted to `${BRO_HOME}/webhooks/<name>.json` and restored on daemon restart.

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
| Webhook correlation = `{}` (broadcast) | Concurrent PRs would cross-resume each other's waits                    | Change `correlate` to `{pr: "${vars.pr_number}"}` AND have routing extract `pr_number` into the verdict |
| Push-storm not debounced               | N commits → N reviewer dispatches                                       | `vars.review_in_progress` flag + routing-rule guard   |
| Implementer relies on LLM to `git push`| If model forgets, arc hangs in `Wait`                                   | Promote push to a dedicated `Shell` op or a `forgejo_push` op |
| `Shell` op has no allowlist enforcement| Trusted-actor assumption                                                | Wire shell-policy packet (design in `WORKFLOWS.md`)   |
| `cancel_arc` routing verdict           | Webhook can't terminate a running arc                                   | Engine cancellation primitive (phase-next)            |
| `bro_workflow_uninstall` / `bro_webhook_uninstall` don't exist | Tear-down requires `rm` + restart                       | Add inverse MCP tools + matching `/admin/` endpoints  |
| `WaitStore` is in-memory only          | Daemon restart loses every suspended arc                                | Disk-back: serialize on register, drop on resolve     |
| Real LLM dispatch every run            | Burns tokens. No simulator mode                                         | `--dry-run` validates spec only; for actor simulation, a `simulator` actor kind is phase-next |
