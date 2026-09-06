+++
title = "Caller-owned orchestration"
tags = ["workflow-orchestration", "retirement", "bro"]
order = 1
template = false
+++
# Caller-owned orchestration

Blackbox executes bro turns and reports their state. The caller owns sequencing, gates, retries, review loops, schedules and external integrations. Use bro_exec to start, bro_resume to continue, bro_status to inspect and bro_wait/bro_when_all/bro_when_any to wait. Record returned task and session handles. Use request_key on exec/resume to safely retry an uncertain admission with identical arguments; a conflict or execution_unknown receipt requires inspecting the original task before choosing new work.

Workflow, cron, poller, webhook, atom, reaction and whiteboard execution are retired. Existing workflow task records and threads are historical evidence. Pass include_workflows=true to list historical workflow threads. Artifact receipts remain readable with an explicit retired kind. No startup replay or automatic subsequent dispatch occurs.
