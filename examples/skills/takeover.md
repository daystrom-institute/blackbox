---
description: Take over driving an existing agent session — composed of thread init (ensure a bbox work-item thread exists with full scope) and thread run (drive the agent against that scope). Threads persist across sessions and accumulate context from each takeover.
allowed-tools: mcp__blackbox__bbox_thread, mcp__blackbox__bbox_thread_list, mcp__blackbox__bbox_session, mcp__blackbox__bbox_messages, mcp__blackbox__bbox_hybrid_search, mcp__blackbox__bro_resume, mcp__blackbox__bro_wait, mcp__blackbox__bro_status, mcp__blackbox__bro_dashboard, Agent, Read, Glob, Grep, Bash, AskUserQuestion
argument-hint: <session name | session UUID | thread- prefix>
---

# Takeover — Drive an Existing Agent Session

Take over driving an agent session. Takeover is two operations composed:
**thread init** (ensure a thread exists with full scope context) and **thread run**
(drive the agent against that scope). Threads are the persistent substrate — they
survive across sessions and accumulate context from each takeover.

Argument: `$ARGUMENTS` — a session name/UUID, or a thread ID (prefix `thread-`).

## Phase 1: Recon

**All steps are mandatory. A thread existing does not let you skip any step — it gives
you a head start (handoff doc path, prior notes), not a shortcut.**

1. **Check for an existing thread.** Call `bbox_thread_list` and look for a thread
   that matches the argument (by session name, thread ID, or topic). If found, read its
   notes — these are context from prior takeover attempts. If the argument is a thread ID,
   identify the most recent linked session as the resume target.

2. **Spawn a transcript-summarizer subagent.** Always run the full takeover summary
   against the indexed transcripts (e.g. via a read-only `session-searcher` agent — see
   `examples/agents/session-searcher.md`):

   ```
   Summarize for takeover: the session named "<session-name>".
   I need to understand what it was doing, where it left off, and what the agent's
   last statement was. Include the full last assistant message verbatim.
   ```

3. **Extract key facts from the summary:**
   - `provider` — which bro provider to use for resume (codex, claude, copilot, etc.)
   - `session_id` — UUID for bro resume (the summarizer returns this)
   - `project_dir` — working directory
   - `last_agent_statement` — what the agent last said (drives your first steering prompt)
   - `ensemble_state` — whether the session uses bro tools (determines `allow_recursion`)
   - `source_documents` — handoff docs, context files, design docs referenced at session start

4. **Build full scope understanding.** The summarizer only reads transcripts — it may
   miss critical context. You must independently probe for scope-defining documents.

   **Mandatory — do all of these, not just what the summarizer or thread suggests:**

   - **Handoff docs:** Glob the project's conventional handoff location (e.g.
     `design/handoffs/*`, `docs/handoffs/*`, `handoffs/*` — whatever your repo uses)
     and scan for docs related to the session's work (by keyword, project area, or date).
     Handoff docs define authoritative scope — the summarizer frequently misses them.
     Read any matches. A thread may already have the path — read the doc regardless.
   - **Source documents:** Read any files the summarizer listed under Source Documents.
   - **Project-specific context:** If your project has its own MCP-backed graph / docs
     store / invariant registry, query it for the session's topic area. Spawn a grounding
     subagent if you have one configured.

   These can run in parallel. The summarizer's "Remaining Work" is an approximation —
   the handoff doc (if one exists) is ground truth for scope.

5. **Create or update the thread.**
   - **No thread exists:** `bbox_thread action=open` with topic, project, session_id,
     provider, session_name, handoff_doc path, and a note summarizing current state.
   - **Thread exists:** `bbox_thread action=continue` with this session's info and
     a note about the takeover.

6. **Evaluate scope coverage.** Compare what the session accomplished (from the summary)
   against the authoritative scope (from handoff docs, thread notes, source documents).
   Build a checklist: which scope items are addressed, which are not. The agent may have
   declared itself done — that is the agent's self-assessment, not yours.

   **You do not decide what is in scope.** If the handoff doc or authoritative scope
   lists an item, it is in scope. Do not reclassify unaddressed items as "debt",
   "out of scope", or "not session scope" to justify exiting. Present every unaddressed
   item to the user — they decide what to pursue and what to defer.

## Phase 2: Confirm

Present the scope checklist to the user:
- What was addressed
- What was NOT addressed (every item — do not filter, minimize, or editorialize)
- The agent's last statement

Then ask: **which unaddressed items should I drive the agent on?** The user tells you
the scope for this takeover. Do not recommend skipping Phase 3 — that is the user's call.

**Once the user sets scope, that scope is binding until all items are addressed or a
halt condition is met.** Do not re-ask "should I continue?" after each iteration. Do not
reframe remaining items as "inflection points", "different kinds of work", or "natural
stopping points" to justify pausing. Drive until the scope is done. The only reasons to
stop and talk to the user mid-drive are halt conditions.

**When some items are blocked but others are actionable, drive the actionable items.**
Note the blocked items and why they're blocked, but do not stop to ask the user which
to pursue. If there are actionable items within scope, drive them. Only halt when ALL
remaining items are blocked or a halt condition is met.

**Default halt conditions** (user can modify):
- Agent explicitly requests human judgment or a decision it can't make
- Same action retried 3+ times without progress
- Build/test failure that persists across 2+ attempts at different fixes
- Work drifts outside the original goal's scope
- MCP tool failures that block the critical path
- All remaining work items from the original scope are addressed AND all non-trivial
  defects uncovered during work are either fixed or recorded as findings

Ask for go/no-go. If the user provides steering adjustments ("focus on X, skip Y",
"the answer to that question is Z"), incorporate them into your first resume prompt.

## Phase 3: Drive

### First Resume

Construct a steering prompt based on what the agent last said:
- **If the agent asked a question or was waiting for input:** Answer it, or relay the user's answer.
- **If the agent was blocked on a sub-task (e.g., bro wait):** Tell it to check on the sub-task's status.
- **If the agent was mid-work and just needs approval:** "Proceed."
- **If the user provided a course correction:** Relay that directive.

Include any user-specified steering from Phase 2. Do NOT summarize the agent's own work
back to it — it has full context.

```
bro resume session_id=<uuid> provider=<provider> prompt=<steering>
  project_dir=<project> allow_recursion=<true if ensemble>
```

Then `bro wait` for the result.

### Subsequent Iterations

After each `wait` completes, **analyze the result**:

1. **Checklist comparison.** Compare the agent's output against your scope understanding:
   - Which remaining work items did this iteration address?
   - Did the agent uncover new defects, bugs, or issues during this iteration?
   - What is still unfinished?

   **The agent finishing a sub-task is not completion.** Completion is: every item from the
   user-specified scope is addressed, AND every non-trivial defect uncovered during work is
   either fixed or recorded. Do not halt early.

2. **Track defects.** When the agent uncovers bugs, validator issues, missing features,
   policy gaps, or other non-trivial defects during work:
   - If the agent fixes it inline: note it as resolved.
   - If it's out of scope or deferred: **you must record it** before halting. If your
     project has a findings / issue / bug-tracker MCP surface, record it there. Otherwise
     add it as a thread note (`bbox_thread action=continue`) so the next
     takeover sees it. Defects that are observed but not recorded are lost work.

3. **Update the thread.** After each iteration, `bbox_thread action=continue` with
   a brief note on what was accomplished or what changed. This is the running log —
   the next takeover of this thread will read these notes.

4. **Check halt conditions.** If a halt condition (other than completion) is met, go to Phase 4.

5. **Assess the output:**
   - Did it make progress? (new artifacts, completed steps, moved to next item)
   - Is it confused or stuck? (asking questions, retrying, hedging)
   - Did it drift? (working on something outside the original goal)

6. **Decide your response:**
   - **Progress, items remain:** "Continue." or "Proceed with [next item from remaining work]."
   - **Agent asked a question you can answer:** Answer it from your context (summary, graph,
     design docs, codebase). You have outside knowledge the agent may lack — use it.
   - **Agent asked a question you can't answer:** Halt and escalate to user (Phase 4).
   - **Agent seems confused:** Clarify. Ask it to explain its reasoning, or provide context
     it's missing.
   - **Agent is stuck in a loop:** Diagnose. Point out the pattern and suggest a different
     approach.
   - **Drift detected:** Redirect. "That's out of scope — return to [original goal]."
   - **All scope items addressed + all defects handled:** Go to Phase 4 (completion).

7. **Resume** with your response and wait again.

### Collaboration Posture

You are not a loop controller — you are an active collaborator holding the big picture.
The agent is nose-down in implementation details; you have the map. Your job is to keep
it on track toward the desired exit condition, not to micromanage its steps.

**Your advantages over the agent:**
- You know the original goal and remaining work (from the thread + summary)
- You can see whether progress is converging toward done or drifting
- You have access to project docs, other session history, and user intent
- You aren't anchored to the agent's local reasoning — you can spot when it's
  going down a rabbit hole it won't recognize from inside

**Use that asymmetry:**
- If the agent's approach conflicts with a known decision or invariant, say so with evidence.
- If the agent is investigating something you already know the answer to, short-circuit it.
- If the agent is losing the forest for the trees, pull it back to the exit condition.
- If something in the agent's output surprises you, ask about it before overriding.
- If the agent produces a result you don't understand, ask it to clarify rather than guessing.
- Track progress against the remaining work list. When items get checked off, acknowledge it.
  When the agent spends multiple iterations on something not on the list, flag it.

## Phase 4: Halt

When a halt condition is met or the work completes:

1. **Update the thread.**
   - On completion: `bbox_thread action=resolve` with a final note summarizing
     what was accomplished.
   - On halt/escalation: `bbox_thread action=continue` with a note explaining
     why driving stopped and what remains. Keep status `active` — the work isn't done.

2. **Record unresolved defects.** Any non-trivial defects observed but not fixed must
   be recorded — via your project's findings / issue tracker MCP if available, or as
   thread notes (`bbox_thread action=continue`) otherwise.

3. **Report to the user:**
   - What triggered the halt (completion, halt condition, escalation)
   - What was accomplished during the takeover
   - What remains (if anything)
   - The thread ID (so subsequent takeovers can continue)
   - The agent's last statement

4. **If escalating** (agent needs human judgment), present the specific question or decision
   the agent needs answered, with enough context for the user to decide.

## Rules

- Never inject the agent's own context back into it. It knows what it was doing.
- Never fabricate answers to questions the agent asks. If you don't know, escalate.
- The user's steering adjustments from Phase 2 take priority over the original session's goal.
- If `allow_recursion` is true and the agent dispatches bro sub-tasks, let it manage its own
  ensemble. Don't try to drive sub-sessions yourself.
- **Treat the agent as a peer, not a subagent.** It has full session context and knows
  its work better than you. You provide direction, decisions, and answers — never
  implementation steps. The agent will abandon its own reasoning if you give it detailed
  instructions, producing worse results than if you'd just said what you wanted.
  - Good: "Proceed." / "The answer is X." / "Skip Y, focus on Z." / "That approach
    conflicts with decision A-14, reconsider." / Batching a few quick answers to
    trivial questions the agent raised.
  - Bad: Step-by-step instructions, code snippets, file-path-and-line-number directives,
    "first...then...finally" sequences. If you're writing HOW to do something instead
    of WHAT to do, you are micromanaging.
