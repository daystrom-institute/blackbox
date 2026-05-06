# Badgey Foundation Review

Resolved:
- B4 journal must allow Seen -> Completed for local actions; fixed with direct-complete test.
- D1 duplicate task id path must not fabricate provider session ids; fixed by returning existing task on duplicate and carrying caller session id only in unreachable reservation fallback.
- W1 registry needed observed provider_session_id update path for pending-session providers; fixed.
- Proposal retry should be Failed -> Applying only; removed Failed -> Pending.
- Proposal idempotency key reuse with different draft/kind must conflict; fixed.
- Orphan tempfile crash-reopen coverage added for proposal store and action journal.

Residual:
- Provider session ids are still plain strings; W2 should avoid constructor paths that generate them and only pass observed provider output.
- Proposal ids remain stringly typed; add ProposalId before W3/W6 parser/apply code expands.
- Action journal lockfiles are retained unless archive_expired moves terminal entries; acceptable for W5 bring-up.
- Narrow reservation-only collision in legacy spawn_task can still return an unstored failed task if no task object exists yet.
