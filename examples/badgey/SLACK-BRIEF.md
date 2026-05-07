# Daily Slack brief — runbook

Wire up the Badgey daily-triage cron to post per-project proposals
into bound Slack channels. Approve via `:white_check_mark:` reaction,
clarify via thread reply.

## Prerequisites

1. Daemon built + running on the prod port (default 7264).
2. Slack sidecar already installed for ask-mode (the sibling
   `slack-badgey-ask` workflow). If not, run
   `examples/slack/scripts/install.sh` first.
3. `SLACK_BOT_TOKEN` set in the daemon's environment (the workflow's
   `chat.postMessage` calls and the post-triage tool both use it).
4. `~/.bro/slack-identities.json` populated for any user you want to
   show as `bbox_user` on acks.

## Install / re-install

```bash
# Re-install the slack routing packet (now includes the
# `proposal-clarify` rule for thread replies).
bash examples/slack/scripts/install.sh

# Install / replace the badgey-cron-routing packet (no functional
# change for the daily flow now that cron-tick fanout intercepts
# `badgey_post_triage_brief` directly, but kept for the weekly
# close-loops cron).
curl -fsS -X POST -H 'Content-Type: application/json' \
  http://127.0.0.1:7264/admin/packet/compile \
  -d @examples/badgey/packets/badgey-cron-routing.json

# Install the daily cron — payload now points at the new tool.
curl -fsS -X POST -H 'Content-Type: application/json' \
  http://127.0.0.1:7264/admin/cron/install \
  -d "$(jq -nc --slurpfile spec examples/badgey/crons/badgey-triage-daily.json \
        '{spec: $spec[0]}')"
```

## Bind a channel

For each project you want a per-channel daily brief in:

```
bbox_project_register(path="/home/me/repos/transcript-search")
bro_slack_bind(
  action="bind",
  team_id="T0123ABCD",          # your workspace id (T-prefix)
  channel_id="C0123XYZ",        # channel id (C-prefix; rename-safe)
  channel_name="transcript-search",  # display only
  project="/home/me/repos/transcript-search"
)
```

Inspect with `bro_slack_bind(action="list")`.

## Trigger a brief

Wait for 06:00 UTC (cron schedule `0 0 6 * * *`) or fire manually:

```
badgey_post_triage_brief(
  scope="/home/me/repos/transcript-search",
  team_id="T0123ABCD",
  channel_id="C0123XYZ",
  channel_name="transcript-search"
)
```

Each proposal arrives as its own Slack message in the channel. Body
is human prose; `proposal_id` rides invisibly in Slack `metadata`.

## Inbound — approve

React to a proposal message with `:white_check_mark:`. Daemon's
webhook routes the event through the `signal_proposal_approved_on_check`
rule → `proposal-approved` signal correlated by `thread_ts=item_ts`.
The signal-hook in `dispatch_verdict` resolves the message back to
its `SlackProposalLink`, bumps the link version, and posts an ack
reply in-thread.

**v1 caveat.** The actual apply work (BadgeyProposalStore CAS +
dispatched task) is deferred until §6.3 sub-bro authoring stores
proposals under a registered `BadgeyInstance`. The hook is the
already-wired call site — when sub-bros land, replace the stub-ack
text with `badgey_apply_proposal_internal(badgey_id, proposal_id,
false).await`.

## Inbound — clarify

Reply in-thread to a proposal message. Daemon routes the message
event through the new `signal_proposal_clarify_on_thread_reply`
rule → `proposal-clarify` signal correlated by `thread_ts=thread_ts`.
Same hook pattern as approve, different reply text.

**v1 caveat.** Real refinement requires `bro_resume(authoring_session_id, …)`
which depends on §6.3 sub-bros having authored the proposal. Same
call-site swap as the apply path.

## Verification

- `bro_cron_list` — shows `badgey-triage-daily` registered.
- `bro_slack_bind(action="list")` — shows the channel binding.
- `error_details` in the `badgey_post_triage_brief` response — per-proposal
  failure reasons (Slack post errors, link-record failures with `partial: true`).
  Successful posts where the link record failed appear in `messages` with
  `link_recorded: false` AND in `error_details` so neither view loses the signal.
- `bro_signals` — ring buffer of signal-dispatch events; expect
  `proposal-approved` / `proposal-clarify` entries with
  `outcome=no_matching_wait` (signal hooks fire on idle).
- `bro_webhook_deliveries(name="slack")` — recent inbound events
  with `verdict_classification=signal_arc` for reactions and
  thread replies.

## Files touched by this work

- `src/slack_channel_bindings.rs` — new store
- `src/slack_proposal_links.rs` — new store
- `src/main.rs` — `bro_slack_bind` + `badgey_post_triage_brief` MCP
  tools, `triage_inbox_with_state` + `post_triage_brief_with_state`
  free fns, `try_slack_proposal_signal_hook` in `dispatch_verdict`
- `src/crons.rs` — cron-tick fanout shim (intercepts
  `badgey_post_triage_brief`-tagged crons, dispatches one call per
  channel binding)
- `examples/badgey/crons/badgey-triage-daily.json` — payload tool
  switched to `badgey_post_triage_brief`
- `examples/slack/packets/routing-slack.json` — added
  `signal_proposal_clarify_on_thread_reply` rule
- `examples/badgey/SLACK-BRIEF.md` — this runbook

## Future migration: foreach primitive

The cron-tick fanout (per-channel iteration in `src/crons.rs`)
exists as a Rust shim because the workflow engine has no `foreach`
transition yet (tracked in `bbox_thread thread-cba8bfa1`). When
`foreach` lands, the fanout migrates to a 1-node workflow whose
`foreach` iterates `slack_channel_bindings.list()` and per-iteration
calls `badgey_post_triage_brief`. The cron payload then routes
through the packet pipeline like every other cron, and the shim
disappears.
