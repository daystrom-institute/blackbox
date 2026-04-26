# Keystone — issue → PR → review → merge arc

An end-to-end demonstration of the workflow engine: a Forgejo webhook
fires when an issue opens, a workflow arc dispatches an implementer
team to fix the bug and open a PR, suspends on a `Wait` for the
PR-ready signal, dispatches a reviewer ensemble that posts feedback,
loops on `pr-feedback` until merged or max-iterations reached, then
runs operator-blessed cleanup hooks at terminal state.

This is not a tutorial — it's a reference. Read the artifacts (they
have inline comments), then adapt to your stack.

## What it exercises

| Engine feature                  | Where in this example                              |
|---------------------------------|----------------------------------------------------|
| Workflow + subworkflow refs     | `workflows/issue-to-merged-pr.json` calls `implementer-arc` and `reviewer-arc` by id |
| `vars` schema + initial seeding | Every workflow declares `vars_schema`; webhook routing seeds `initial_vars` |
| `${vars.x}` / `${meta.x}` / `${last_signal.x}` templates | All node prompts |
| Hooks: SetVar / IncVar / WorktreeCreate / WorktreeRemove / ParseJson / Forgejo* | `Setup`, `AwaitFeedbackOrMerge`, `on_arc_exit` |
| Hook gating via `when: domain:...` | `on_arc_exit` cleanup conditional on `meta.arc_outcome` |
| Wait nodes with `any_of` race + timeout | `AwaitReviewTrigger`, `AwaitFeedbackOrMerge` |
| Gate packets with multiple verdicts → graph branching | Both Wait nodes route to `Review` / `Done` / `AddressFeedback` |
| Subworkflow imports/exports     | Implementer exports `pr_number`, parent threads it through |
| Domain-shaped packet refs       | `domain:webhook-routing/forgejo`, `domain:workflow-cleanup/keep-on-fail`, etc. |
| Webhook ingress (HMAC-SHA256)   | `webhooks/forgejo.json` + Forgejo bootstrap configures the hook |
| Routing packet → start_arc / signal_arc | `packets/routing-forgejo.json` |
| Operator-blessed registries     | Workflows + webhooks installed via MCP, persisted to disk |
| Capability tags                 | Actors declare `requires` (here: empty — every model used can do the job) |

## Prerequisites

- Docker (or Podman with `alias docker=podman`)
- `jq`, `curl`
- `blackboxd` running on port 7264 (default)
- Configured brofiles with real provider credentials. The install
  script tries to create:
  - `keystone-impl`   → Claude Sonnet 4.6 (implementer)
  - `keystone-review` → Claude Haiku 4.5  (reviewer)
  
  If you don't have Claude configured, edit `scripts/install.sh` to
  use whatever providers your daemon knows about. Capability validation
  will refuse to start the arc otherwise.

## One-shot

```sh
cd examples/keystone
./scripts/run.sh
```

This brings up Forgejo, bootstraps a buggy repo + seeded issue,
installs everything into `blackboxd`, and waits. Opening another
issue (or commenting on the seed issue, depending on what your
routing rules accept) will trigger the arc.

To skip the webhook wait and dispatch directly:

```sh
./scripts/run.sh --dispatch
```

## Layout

```
examples/keystone/
├── docker-compose.yaml       # Forgejo single-instance
├── scripts/
│   ├── bootstrap.sh          # admin user, repo, seed bug + issue, webhook config
│   ├── install.sh            # compile packets, install brofiles/teams/workflows/webhook
│   └── run.sh                # docker up → bootstrap → install → (dispatch | wait)
├── packets/
│   ├── routing-forgejo.json          # webhook event → routing verdict
│   ├── gate-merge-or-review.json     # AwaitReviewTrigger gate
│   ├── gate-loop-or-exit.json        # AwaitFeedbackOrMerge gate
│   ├── cleanup-policy.json           # keep-on-fail / delete-on-success
│   └── policy-arc-budget.json        # arc-level budget guard
├── webhooks/
│   └── forgejo.json                  # extractor + signature + routing packet ref
└── workflows/
    ├── implementer-arc.json          # subworkflow: fetch issue → fix → push → open PR
    ├── reviewer-arc.json             # subworkflow: review PR → post comment + verdict
    └── issue-to-merged-pr.json       # main keystone arc
```

## Live observation

```sh
# stream every event the engine emits
curl -N http://127.0.0.1:7264/tail

# poll arc state (returns ArcSnapshot list + pending waits)
curl http://127.0.0.1:7264/orchestrate/peek | jq

# inspect a specific arc by id
bro orchestrate status <arc_thread_id>

# inspect what the routing packet would emit for a payload, without
# actually firing anything:
curl -X POST -H 'Content-Type: application/json' \
  -d '{"action":"opened","issue":{"number":42,"title":"fix me"}}' \
  -H 'X-Gitea-Event: issues' \
  http://127.0.0.1:7264/webhook/forgejo/replay | jq
```

## Tear-down

```sh
docker compose down -v       # wipes Forgejo data
rm -f .env
# Daemon-side packet/workflow/webhook entries persist; remove via
#   bbox_forget <packet-id>
#   rm ~/.local/state/blackbox/bro/{webhooks,workflows}/*.json
# (no clean MCP unregister tool yet — TODO)
```

## Known gaps

- **Real PR-state correlation.** The example webhooks correlate by
  `{}` (broadcast). For multi-arc concurrency you'd correlate on
  `{repo, pr}` and the routing packet would extract those fields.
  Easy retrofit; left out of v1 for clarity.
- **Push-storm debouncing.** Three commits → three `synchronize`
  webhooks → three reviewer dispatches. Real deployment wants a
  `vars.review_in_progress` flag the routing packet checks. Pattern
  documented in the design discussion; not wired here.
- **Implementer's git push.** Currently relies on the implementer LLM
  to actually run `git push` from inside the worktree. A future
  iteration could promote that to a sandboxed `Shell` op with policy
  packet enforcement.
- **No simulator mode.** Every dispatch runs a real LLM. Burns
  tokens. To dry-run the spec without dispatch, use
  `bro_orchestrate_run(dry_run=true)`.
