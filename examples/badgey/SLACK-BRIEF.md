# Slack daily-brief — runbook

Per-project Slack channels host a daily Badgey-driven triage brief.
Inbound flow: `:white_check_mark:` reaction approves, in-thread
reply clarifies/refines.

## Architecture

```
daily 08:00 system-local cron tick
  → routing packet badgey/cron-routing matches cron_name=badgey-triage-daily
    → start_arc badgey-triage-fanout-arc
      └── ListBindings  (mcp_call bro_slack_bind action=list)
      └── ForeachBinding (parallelism=2, on_item_failure=continue)
            └── badgey-triage-channel-arc  (per channel binding)
                  ├── ExtractBinding   (set top-level vars from binding)
                  ├── EnsureInstance   (mcp_call badgey_ensure_for_channel)
                  ├── TriageTurn       (mcp_call badgey_resume — corpus walk + scout-charter emission)
                  ├── ForeachScout     (parallelism=3, subworkflow_ref=badgey-scout-arc)
                  │     └── badgey-scout-arc  (executor=badgey-scout-persona, durable=false)
                  ├── SynthesisTurn    (mcp_call badgey_resume — proposals or dream/explore)
                  ├── ListProposals    (mcp_call badgey_proposals_list since=synthesis_started_at)
                  └── ForeachPostProposal (parallelism=1, on_item_failure=continue)
                        └── badgey-slack-emit-proposal-arc
                              ├── Render      (set_var rendered_text)
                              ├── Post        (http_json chat.postMessage with metadata)
                              └── RecordLink  (mcp_call bro_slack_link_record)
```

Empty-day handling falls out naturally:
- 0 scout charters → `ForeachScout` iterates 0×, `scout_findings` empty,
  synthesis charter pivots to dream/explore mode.
- 0 proposals → `ForeachPostProposal` iterates 0×, channel stays quiet.

Inbound side stays the same as before:
- `:white_check_mark:` reaction → `proposal-approved` signal correlated
  by `thread_ts=item_ts` → `try_slack_proposal_signal_hook` resolves
  via `SlackProposalLinks` and posts a threaded ack. Apply call-site
  becomes `badgey_apply_proposal_internal(link.instance_id, link.proposal_id, false)`.
- In-thread reply → `proposal-clarify` signal → same hook, refine
  call-site becomes `bro_resume(link.authoring_session_id, reply_text)`.

## Prerequisites

1. Daemon built + running with the merged foreach/matrix runtime
   (commit `0530ba2` and friends).
2. Slack sidecar `bro-slack.service` running.
3. `SLACK_BOT_TOKEN` in the daemon environment.
4. `~/.bro/slack-identities.json` populated for any user you want
   surfaced as `bbox_user` in ack messages.

## Install (idempotent — re-run after `git pull`)

Use `bbox_artifact_install` (MCP) or the `/admin/artifact/install`
HTTP endpoint — **not** `/admin/workflow/install` directly. The
artifact-install path updates BOTH the artifact catalog (durable,
versioned, source-tracked) AND the runtime workflow registry. The
admin/workflow/install path only updates the runtime registry,
leaving the catalog stale — which makes future audits and supersession
chains see drift between source files and the daemon's view.

```text
# MCP form (preferred):
bbox_artifact_install(kind="workflow", source="examples/badgey/workflows/badgey-scout-arc.json")
bbox_artifact_install(kind="workflow", source="examples/badgey/workflows/badgey-slack-emit-proposal-arc.json")
bbox_artifact_install(kind="workflow", source="examples/badgey/workflows/badgey-triage-channel-arc.json")
bbox_artifact_install(kind="workflow", source="examples/badgey/workflows/badgey-triage-fanout-arc.json")
bbox_artifact_install(kind="packet",   source="examples/badgey/packets/badgey-cron-routing.json")
```

The cron itself still installs via `bro_cron_install` (no artifact
catalog for crons today):

```text
bro_cron_install(spec=<contents of examples/badgey/crons/badgey-triage-daily.json>)
```

## Bind a channel

```
bbox_project_register(path="/home/me/repos/my-project")
bro_slack_bind(
  action="bind",
  team_id="T0123ABCD",
  channel_id="C0123XYZ",          # rename-safe lookup key
  channel_name="my-project",      # display only
  project="/home/me/repos/my-project"
)
```

`bro_slack_bind(action="list")` to inspect. `action="unbind"` also
dismisses the system Badgey instance for that channel (best-effort).

## Manual smoke

```
bro_orchestrate_run(
  workflow=<contents of badgey-triage-fanout-arc.json>,
  dry_run=true
)
```

Then for a real run (drop dry_run): the arc returns when every
per-channel subworkflow terminates. Per-channel timing depends on
Badgey's triage turn (~minutes), scout fanout (parallelism=3),
synthesis turn (~minutes), Slack post burst.

## Verification

- `bro_arc_status(arc_id=…)` — current node, completed nodes, in-flight
  fork branches, last verdict, visit counts. Works on both the top-level
  fanout arc and per-binding subworkflows.
- `bbox_inbox` — surfaces unresolved disputes/blocked notes from
  Badgey scouts and the synthesis turn.
- `bro_signals(signal="proposal-approved")` and
  `bro_signals(signal="proposal-clarify")` — inbound reaction and
  thread-reply traffic.
- `bro_webhook_deliveries(name="slack")` — every Slack event through
  the routing packet, with extracted entity + verdict classification.
- `badgey_proposals_list(badgey_id=…)` — snapshot of all proposals
  owned by a channel's system Badgey instance.

## Tunables

- **Cron schedule** — `examples/badgey/crons/badgey-triage-daily.json`,
  `schedule` field in 6-field cron form. `tz: "Local"` keeps it
  stable across DST transitions.
- **Channel-fanout parallelism** — `ForeachBinding.foreach.parallelism`
  in `badgey-triage-fanout-arc.json`. Default 2.
- **Scout-fanout parallelism** — `ForeachScout.foreach.parallelism`
  in `badgey-triage-channel-arc.json`. Default 3 (capped by the
  workflow engine's `MAX_FOREACH_PARALLELISM`).
- **Triage / synthesis timeouts** — `badgey_resume(timeout_seconds=…)`
  argument in the channel-arc's TriageTurn (600s) and SynthesisTurn
  (900s) nodes.

## Future-work checklist

- [ ] Wire the apply call-site in `try_slack_proposal_signal_hook`:
      replace the stub-ack text with
      `badgey_apply_proposal_internal(link.instance_id, link.proposal_id,
      retry_failed=false)` once a proposal lands. The link record
      already carries `instance_id` for this.
- [ ] Wire the refine call-site likewise: `bro_resume(link.authoring_session_id,
      reply_text)`. Requires populating `authoring_session_id` on
      link records — the synthesis turn could capture the badgey
      session_id used for that proposal.
- [ ] Per-proposal Slack message-update on refinement (chat.update on
      the original msg_ts; bumps `link.version`).
- [ ] User-gate on `proposal-approved` so only the project owner's
      reaction triggers apply (read `bbox_user` from the entity,
      check against an allowlist).
- [ ] Rate-limit / backoff on Slack 429 responses in
      `badgey-slack-emit-proposal-arc`.
