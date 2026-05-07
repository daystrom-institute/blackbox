# Slack daily-brief — design + status

Per-project Slack channels host a daily Badgey-driven triage brief.
Inbound flow: `:white_check_mark:` reaction approves, in-thread reply
clarifies/refines.

## Status

**Plumbing landed:** channel-binding store + `bro_slack_bind` MCP
tool, `SlackProposalLinks` store, `signal_proposal_approved_on_check`
+ `signal_proposal_clarify_on_thread_reply` rules in
`webhook-routing/slack`, `try_slack_proposal_signal_hook` in
`dispatch_verdict`, cron `tz` field for system-local schedules,
bind/unbind → BadgeyInstance dismissal.

**Outbound implementation pending:** the per-channel triage cycle
itself is being rebuilt as a workflow on top of the upcoming
`foreach`/`matrix` workflow primitives (work in `transcript-search-foreach-matrix`,
commit `0530ba2`). When that lands and merges to main, the daily
brief gets implemented natively as:

```
badgey-triage-fanout-arc (started by daily cron)
└── ForeachBinding (foreach over slack_channel_bindings)
    └── badgey-triage-channel-arc (subworkflow per binding)
        ├── EnsureInstance — get-or-create system Badgey for (project, channel)
        ├── TriageTurn — Badgey emits scout charters via bg-action-spawn-subbro
        ├── ForeachScout (foreach with parallelism cap)
        │   └── badgey-scout-arc — bro_exec(badgey-scout-persona) per charter
        ├── SynthesisTurn — Badgey reads scout dones, emits bg-action-emit-proposal
        ├── Branch — proposals empty? → DreamTurn else → PostToSlack
        ├── DreamTurn — Badgey corpus-mining mode (workflow/agent extraction)
        └── PostToSlack — foreach over proposals: chat.postMessage per
```

Until then the cron payload tool is `badgey_triage_inbox` (the simple
stale-thread synthesizer), which does not post to Slack. No automatic
brief lands; the inbound hooks still fire for any messages a human
posts manually.

## Configure a channel binding

```
bbox_project_register(path="/home/me/repos/transcript-search")
bro_slack_bind(
  action="bind",
  team_id="T0123ABCD",
  channel_id="C0123XYZ",
  channel_name="transcript-search",
  project="/home/me/repos/transcript-search"
)
```

Inspect with `bro_slack_bind(action="list")`. Unbinding dismisses
the system Badgey instance for that channel (best-effort, logged on
failure).

## Schedule

The cron runs at **08:00 system-local time** (`tz: "Local"` in the
spec). With DST, that's 14:00 UTC during MDT, 15:00 UTC during MST —
re-evaluated each tick so DST transitions are seamless.

## Inbound routing

Already wired via `webhook-routing/slack`:
- `:white_check_mark:` reaction on a posted proposal →
  `proposal-approved` signal correlated by `thread_ts=item_ts`.
- In-thread reply on a posted proposal →
  `proposal-clarify` signal correlated by `thread_ts`.

Signal hooks in `dispatch_verdict` resolve the message back to its
`SlackProposalLink` and post a threaded ack. When sub-bro authoring
fully lands, those hooks become the call sites for
`badgey_apply_proposal_internal` (approve) and `bro_resume`
(clarify).

## Future work checklist

- [ ] Wait for `foreach`/`matrix` to merge from
      `transcript-search-foreach-matrix` to main.
- [ ] Author `badgey-triage-fanout-arc`,
      `badgey-triage-channel-arc`, `badgey-scout-arc` workflow specs
      (see topology above).
- [ ] Switch the daily cron payload to `start_arc` of the fanout
      workflow.
- [ ] Replace stub-ack reply text in
      `try_slack_proposal_signal_hook` with the real
      `badgey_apply_proposal_internal` / `bro_resume` calls once
      proposals are stored under a registered `BadgeyInstance`.
