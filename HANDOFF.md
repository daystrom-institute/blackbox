# Context Handoff — Phase Decomposer Design Session

Date: 2026-05-10
Predecessor: Claude Opus 4.7 [1M] session; user lost trust in output quality
due to repeated failure to ground in code before proposing.
Successor: more capable agent (likely codex) takes over.

---

## 1. The charter

The user is designing **machinery to execute oversized "phase" docs reliably
across LLM providers with variable compaction behavior.** Motivating failure
mode: a vibe-class bro that compacted catastrophically 80% through a phase
and dropped intent.

The conversation evolved through several reframings. The current shape (per
the user's direction, not the predecessor's invention):

**Three orthogonal supervision layers, layered Swiss-cheese defense:**

1. **Mechanical counters** (daemon-internal, deterministic): loop hash,
   stall, token-burn rate, compact-boundary count, rate-limit. Detect
   structural anomalies. *Tripwires*, not judges. Emit signals; populate
   per-task anomaly state.

2. **Oracle classifier** (cheap LLM observer, e.g. Haiku): observes primary
   via polling; emits semantic classifications (fabrication, scope_creep,
   drift) as `bbox_note(kind=surprise|dispute)`. *Tripwire*, not judge.
   No destructive tools.

3. **Advisor** (smarter LLM judge): summoned at wait/node boundaries with a
   structured checkpoint that includes counts (from §1), notes (from §2),
   and pre-applied packet classification. Emits one of five verdicts.
   *This is the judgment layer.* Bounded by charter + packet_id +
   timeout + tool catalog.

**Pipeline shape (Swiss-cheese, both ends):**

- *Upstream*: discovery (fleet of scouts orchestrated by large-context
  Opus/Codex; output: aggregate evidence bundle) → decomposer panel
  (whiteboard deliberation) → DAG of sub-units → implementer dispatch
  (advised + supervised) → recompose (panel verdict on parent
  acceptance).
- *Downstream mediation* (Swiss-cheese tail): M1 mechanical merge → M2
  conflict-resolver agent → M3 mediation panel (advocate-per-sub-unit) →
  M4 post-merge regression fixer → M5 recompose-advisor verdict → M6
  human escalation.

The user explicitly framed both ends as fan-out + synthesize: discovery
fans out over question shapes; mediation fans out over interested parties.

---

## 2. Conversation provenance in bbox

- **Work-item thread:** `thread-ffe3c075` ("phase-decomposer design review").
- **Codex session:** `019e12d1-a673-7913-b191-9ea94a2ecc74` (provider `codex`).
  Two review rounds completed; resume to continue.
- **Codex notes on thread** (8 total, all unresolved as of handoff):
  - `note-e5449f76` (done) — second-round summary
  - `note-cdb653e7` (dispute) — `examples/tool-gap-analysis/` workflow JSON
    in wrong shape vs. real engine schema
  - `note-f72a5ee6` (dispute) — `bbox_note(kind=tool_gap)` invalid kind
  - `note-1ca7e6ef` (dispute) — `bbox_search` lacks `filter`/`doc_type`/`since`
  - `note-057d9d54` (surprise) — `bbox_apply` still leaks as write primitive
    in revised doc
  - `note-eaeb41a8` (dispute) — `tool_call_event` is post-index doc, not live
    signal bus; phase-decomposer dependency underspecified
  - `note-11f9afd3` (followup) — need third artifact for telemetry processing
    boundary
  - `note-c29245bd` (assumption) — predecessor treated tool-gap-analysis as
    intentionally non-runnable skeleton

Query: `bbox_notes(thread_id="thread-ffe3c075", full=true)`.

To resume codex: `bro_resume(provider="codex",
session_id="019e12d1-a673-7913-b191-9ea94a2ecc74", prompt=...)`.

---

## 3. Files currently authored (state at handoff)

### `design/phase-decomposer.md`
- **Status:** drafted, revised once after codex round 1 review.
- **Codex round 1 absorbed:** dispatch-v2 reframed as design-stage; whiteboard
  conflict caveats called out; recompose promoted; mediator memory deferred;
  build sequence reordered.
- **Codex round 2 found unfixed issues:**
  - `bbox_apply` still leaks as write primitive at §7 (around the
    scope-expansion paragraph).
  - Naming inconsistency: `tool_call` (doc-type) vs `tool_call_event`
    (consumer name).
  - Depends on `workspace-tools.md` for instrumentation and coercion.
  - Whole structure precedes the layered control-plane framing the user
    converged on later — needs another revision pass under that framing.

### `design/workspace-tools.md`
- **Status:** drafted, NOT revised after codex round 2 critique.
- **Codex round 2 findings (unfixed):**
  - `bbox_search(filter=tool_kind:edit)` doesn't work — `SearchParams`
    (`src/index/search.rs:27-57`) has no `filter`/`doc_type`/`since`. Would
    need a dedicated `bbox_tool_calls` query tool OR extension to
    `bbox_search`.
  - `bbox_note(kind=tool_gap)` invalid — valid kinds: `dispute`,
    `assumption`, `surprise`, `followup`, `blocked`, `learned`, `done`
    (`src/notes.rs:110-124`).
  - `bbox_notes(target_file=...)` invalid filter — `NoteListParams`
    (`src/notes.rs:50-85`) has no `target_file` field.
  - `tool_use_id` field not in index docs (`src/index/reindex.rs:494-530`);
    would need schema bump alongside `tool_name`/`tool_kind`/`tool_target`/
    `tool_outcome`.
  - `bbox_apply` leaks back as a write primitive in §3 and §7 despite the
    parity-table correction (it's a packet-evaluation tool, not file-edit;
    `src/tools/packets.rs:24-28`).
  - Live-signal seam underspecified: `tool_call` doc-type is post-index;
    phase-decompose §7 wants live scope-check signals; need a real
    signal pipeline.

### `examples/tool-gap-analysis/`
- **Status:** skeleton — `README.md`, `workflows/tool-gap-analysis.json`,
  `packets/group-and-rank.json`.
- **Codex round 2 findings — workflow JSON is structurally wrong:**
  - `nodes` is a *map* keyed by id with separate `start`, not an *array*
    (`src/workflow/schema.rs:15-23`).
  - `mcp_call` is a hook op with `args.server`/`tool`/`arguments`, not a
    node kind (`src/workflow/ops.rs:176-189`, `:628-674`).
  - `foreach` is a node *field* requiring `items`/`as_var`/child
    workflow/`collect`, not a kind (`src/workflow/schema.rs:193-275`).
  - Whiteboard params: tool wants `type`/`title`/`body`, stub sends
    `kind`/`claim` (`src/tools/bro_runtime_params.rs:153-172`).
  - Packet stub uses `match`, schema uses `antecedent`
    (`src/packets/ast.rs:479-482`).
  - Packets classify single entities; aggregation/ranking is wrong
    primitive — should be killed entirely or replaced with workflow-code
    aggregation.

### Deleted

- `design/control-plane.md` — predecessor authored this without being asked;
  user instructed deletion (rightly — predecessor had not done enough
  grounding to write a trustworthy design doc).

---

## 4. User directives (running list, captured verbatim where load-bearing)

These are the user's explicit corrections. Treat them as authoritative.

**On distillation:**
> "distillation doesn't need to be mechanical... it could just be a canned
> example/ or badgey/ workflow right? it's data - not code?"

**On scope of workspace-tools coercion:**
> "we don't need to suppress EVERY built-in (eg. let agents do
> webfetch/websearch themselves) - we just need to coerce the ones that
> matter."

**On escape hatch usage:**
> "escape hatch usage = signal source"

**On the daystrom reference for workspace tools:**
> "look at how advisors, signal processors, and acceptance criteria work in
> daystrom. then also examine the keystone example in THIS repo, then look
> at the whiteboard example in THIS repo"

**On daystrom donor framing — DON'T treat dispatch-v2 as implemented:**
> "you're just fabricating that they don't compose without actually looking
> or understanding anything.... these are LAYERS in swiss cheese defense."
> (Reframing my "they don't compose" critique as wrong: the three DAG layers
> in daystrom are Swiss-cheese defense, each catching what others miss.)

**On downstream mediation:**
> "this layering is/should be also reflected on tail end - downstream
> mediation. what happens when mechanical merge resolution isn't enough?
> which agent does the fixups?"

**On scout fleet (NOT one triage scout):**
> "expect that most submitted plans won't be pre-grounded in code, this
> isn't one triage scout - it's a fleet of them dispatched over the entire
> proposal so that the returned evidence bundles/lead collations can
> correctly inform sizing / downstream consumers. in my head this is a
> large-context opus or codex that is orchestrating 'discovery phase' --
> it aggregates, dedupes, and synthesizes the aggregate bundle required
> for downstream consumers to make informed decisions. for a scout - look
> at /home/invidious/repos/transcript-search/.claude/agents/corpus-pathfinder.md
> and imagine fleets of these dispatched over the proposed work sequence.
> if grounding for downstream phases requires scouting over upstream
> phases - we can do that too, think of scouting as woven preprocessor
> steps that incrementally build up the shared development context ->
> subsets of which are then used to seed implementer agents."

**On orthogonality of agent system and actor system (critical):**
> "They are orthogonal - and you are reinventing elements of BOTH"

**On stopping list-dumping and grounding instead:**
> "stop dumping massive useless fucking lists on me - you're still not
> grounding in fucking code. GO LOOK AT THE FUCKING ACTOR SYSTEM"
> "we're building toolboxes, not one-offs."

**On corpus-pathfinder as installed agent (not Claude-only):**
> "WE HAVE HARNESS-AGNOSTIC AGENT INFRASTRUCTURE. this claude exemplar
> needs to becomes an enhanced example agent that we install and use for
> workflows."

**On detection ≠ judgment (load-bearing):**
> "MECHANICAL OR ORACLE - IT DOESN'T FUCKING MATTER - EITHER ONE IS A
> SIGNAL TO SPIN UP A SMARTER ADVISOR THAT =====CAN===== MAKE THE FUCKING
> JUDGMENT CALL."

**On TeamAdvisorConfig migration:**
> "advisor is rip and replace - no migrations"

**On whiteboard as verdict-resolution substrate:**
> "look at examples/whiteboard/ to see verdict resolution"

**On advisor placement:**
> "I think this is ALSO calling for a rework of the current advisor system -
> having it embedded as a singleton in team feels weird. Rather, it feels
> like we should pull advisor OUT of team and then make 'advising' a
> workflow verb, where the advisor can be one or more agents (bro or
> team/council/whatever) that act as advisors for that subworkflow. This
> makes the act of 'advising' generally available in workflows, an
> intermediate that we can use to build decomposer/scout/implementer/
> recomposer flows?"

**On oracle co-session being a workflow verb too:**
> "ditto for oracle cosession - this isn't special one-off machinery, it's
> a new workflow verb and primitives + some lower level wiring"

**Loss of trust in predecessor:**
> "you don't understand the problem because you won't fucking look or do the
> work. I don't trust anything you are producing"

---

## 5. Grounded bbox primitives — verified file:line refs

These references the predecessor confirmed by reading. Successor should
treat as starting points, NOT take on faith — re-verify when consequential.

### Workflow engine

- **`src/workflow/schema.rs:79-105`** — `ActorKind` enum has only TWO values:
  `Executor`, `Ensemble`. Doc-comment explicitly says persona/role
  (advisor, planner, etc.) is workflow-author concern carried by
  brofile + prompt + `on_exit parse_json` + gate, NOT an engine type.
  Predecessor proposed new actor kinds and was correctly slapped down.
- **`src/workflow/schema.rs:15-23`** — `nodes` is a map keyed by node id,
  with separate `start` field. Not an array.
- **`src/workflow/schema.rs:107-179`** — `NodeSpec` carries `actor`,
  `prompt`, `gate`, `gate_mode`, `mode`, `retry`, `late_inject`,
  `subworkflow`, `subworkflow_ref`, `imports`, `exports`, `on_enter`,
  `on_exit`.
- **`src/workflow/schema.rs:193-275`** — `ForeachSpec` requires `items`,
  `as_var`, child workflow/ref, `collect`. Foreach is a NodeSpec field,
  not a node kind.
- **`src/workflow/schema.rs:34`** — `Workflow.policy_packet: Option<String>`
  — fires at every node boundary; verdicts halt/escalate/warn.
- **`src/workflow/engine.rs:1098-1165`** — `apply_policy_packet()`
  evaluation site.
- **`src/workflow/engine.rs:1173-1189`** — `write_compaction_anchor()`
  — rolling summary notes on arc thread; daystrom-pattern checkpoint
  primitive.
- **`src/workflow/engine.rs:1544-1591`** — durable actor session
  caching/resume via `bro_resume`.
- **`src/workflow/engine.rs:1575`** — `orch::wait_for_task_with_timeout`
  — the synchronous wait point. No per-turn observability today.
- **`src/workflow/engine.rs:2265+`** — `run_ensemble_node()` — parallel
  team broadcast with whiteboard auto-post.
- **`src/workflow/ops.rs:55-135`** — `OpKind` enum (32 hook op kinds).
- **`src/workflow/ops.rs:176-214`** — `execute_op()` match site.
- **`src/workflow/ops.rs:628-674`** — `exec_mcp_call()` impl.
- **`src/workflow/ops.rs:40-41`** — `HookOp.when: Option<String>` — per-op
  conditional gate.

### Advisor pipeline (ALREADY WIRED, on Team)

- **`src/orchestration/team.rs:56-65`** — `AdvisorMode { Blocking |
  Background }`.
- **`src/orchestration/team.rs:67-85`** — `TeamAdvisorConfig` schema. This
  is the judgment-layer contract. Fields: brofile, charter, context,
  halt_conditions, exit_conditions, packet_id, timeout_seconds, mode.
- **`src/orchestration/team.rs:93-101`** — `TeamAdvisor` — live instance
  with session_id + task_history.
- **`src/orchestration/team.rs:113-122`** — `Team.advisor: Option<TeamAdvisor>`.
- **`src/orchestration/team.rs:255-271`** — `instantiate_team` — clones
  advisor config from teamplate.
- **`src/tools/roster.rs:607-668`** — `build_team_advisor_init_prompt` —
  builds advisor system prompt with charter, member list, halt_conditions,
  exit_conditions as declarative cues, packet_id, and the five-verdict
  response format.
- **`src/tools/roster.rs:670+`** — `dispatch_team_advisor_prompt` —
  dispatch/resume advisor session.
- **`src/tools/roster.rs:855+`** — `await_team_advisor_task`.
- **`src/tools/roster.rs:870+`** — `initialize_team_advisor` — once-at-team-
  creation init.
- **`src/tools/roster.rs:926-1029`** — `build_advisor_checkpoint` — builds
  structured snapshot: wait_kind, team_name, packet_id, monitored_task_ids,
  status counts, note counts (per bbox_note kind), per-member checkpoint.
- **`src/tools/roster.rs:1031-1054`** — `apply_advisor_packet` — runs
  packet against checkpoint as entity; returns `{ruleId, classification,
  consequent, confidence}`.
- **`src/tools/roster.rs:1056+`** — `maybe_resume_team_advisor` — resumes
  advisor with checkpoint + packet result; advisor emits verdict.
- **Five advisor verdicts** (prompts at `roster.rs:657, 1085`):
  `CONTINUE | ESCALATE | CHARTER_DRIFT | EXIT_MET | REPLACE_BRO`.
  Vocabulary defined; *consumers* of each verdict only partially wired.
  `REPLACE_BRO` has no consumer in code beyond the prompt declaration —
  search showed only the two prompt strings.

### Task lifecycle / cancellation

- **`src/orchestration/mod.rs:1087-1156`** — streaming stdout reader for
  bros. Per-line NDJSON parsing at `:1094`. `inner.events.push(evt.clone())`
  at `:1097` captures raw events in memory.
- **`src/orchestration/mod.rs:1109`** — `provider.parse_event(&evt, &mut sink)`
  — per-event hook. Currently extracts only `last_assistant_message`,
  `usage`, `cost_usd`, `num_turns`, `session_id`. This is the seam where
  anomaly counters would be added.
- **`src/orchestration/mod.rs:1146`** — TaskProgress tail emission
  (lifecycle-only; not per-tool).
- **`src/orchestration/mod.rs:1414-1439`** — `cancel_task()` — sets status,
  SIGTERMs PID via `libc::kill`, `notify_waiters`.
- **`src/orchestration/tail.rs:14-40`** — `TailEvent` enum: only 5
  lifecycle variants (TaskStarted/Progress/Completed/Failed/Cancelled).
  TaskProgress carries only `activity: String` (snippet). No per-tool-call
  events on the tail stream.

### Signals

- **`src/server/routes.rs:1871-1934`** — `signal_arc_dispatch(state,
  signal, correlation, payload)` — resolves pending Wait, records
  SignalEvent. Used by webhook router + `bro_arc_signal` MCP tool.
- **`src/tools/orchestrate.rs:272-283`** — `bro_arc_signal` MCP tool.

### Cancel/observation MCP tools

- **`src/tools/dispatch.rs:11`** — `bro_exec` MCP tool.
- **`src/tools/dispatch.rs:118`** — `bro_resume` MCP tool.
- **`src/tools/dispatch.rs:251`** — `bro_wait` MCP tool.
- **`src/tools/dispatch.rs:674-683`** — `bro_status` MCP tool, takes
  `tail: Option<u32>`.
- **`src/tools/dispatch.rs:772-799`** — `bro_cancel` MCP tool. Calls
  `orch::cancel_task`.

### Parser / index

- **`src/parser.rs:127-180`** — `ToolCallInfo { name, tool_use_id, kind:
  ToolCallKind, input: Value }` is extracted at parse, but flattened into
  `content` blob as `tool:{name} {input}`. Tool name NOT a separate
  indexed field today.
- **`src/index/mod.rs:15`** — `INDEX_SCHEMA_VERSION` constant.
- **`src/index/mod.rs:28-70`** — schema fields list: content, session_id,
  account, project, role, timestamp, file_path, path_tokens, byte_offset,
  git_branch, is_subagent, agent_slug, doc_type, chunk_kind, language,
  symbol, symbol_exact, code_content, chunk_hash, entity_id,
  parser_version, commit_sha, repo_id, commit_author_name,
  commit_author_email. `doc_type` exists as discriminator.
- **`src/index/search.rs:27-57`** — `SearchParams` — no `filter`,
  `doc_type`, or `since` fields. `bbox_search(filter=...)` is **not
  supported**; predecessor proposed using it and was wrong.
- **`src/index/search.rs:201-214`** — query parser searches
  content/project/code_content/symbol only.
- **`src/index/reindex.rs:494-530`** — index doc construction; does NOT
  store `tool_use_id`.

### Notes

- **`src/notes.rs:50-85`** — `NoteListParams` — filters: kind, project,
  task, session, thread, bro, resolution, query, since. **NO
  `target_file`** filter. Predecessor invented one.
- **`src/notes.rs:110-124`** — `NoteKind` enum. Valid kinds: `dispute`,
  `assumption`, `surprise`, `followup`, `blocked`, `learned`, `done`.
  **`tool_gap` is NOT a kind.** Predecessor invented it.

### Code nav

- **`src/code_nav/mod.rs:657`** — `bbox_code_symbols` returns
  `semantic_status="syntax_only"`. Symbols are syntactic, not
  binding-resolved. Predecessor's "symbol-set as predicted_writes" is
  best-effort, not authoritative.
- **`src/system_memory/refactor.md:16`** — code-nav tools are syntax
  locators, not reference resolvers.

### Refactor

- **`src/refactor/mod.rs`** — plan kinds. Generic: only `move_file`,
  `replace_text`. Rust: `extract_rust_items`, `extract_rust_impl_methods`,
  `add_rust_router_to_sum`, `add_rust_mod_decl`, `add_rust_use_decl`.
  Java: `extract_java_methods`, `extract_java_class`,
  `extract_java_nested_classes`, `extract_java_interface`,
  `add_java_fields`, `add_java_constructor`, `add_java_delegate_field`,
  `add_java_implements`, `move_java_field`, `move_java_constant`,
  `lombokify_java_class`. Predecessor's "limited refactor surface"
  characterization was correct.

### `bbox_apply` — packet evaluation, NOT file edit

- **`src/tools/packets.rs:24-28`** — `bbox_apply` is `PacketApply`. Input:
  `{packet_id, entity, mode}`. Evaluates compiled rule packet against
  entity. **NO file edits.** Predecessor repeatedly miscast this.

### Whiteboards

- **`src/whiteboards.rs:46`** — phases: blind / read / validate / debate /
  resolve / archived.
- **`src/whiteboards.rs:351-365`** — `whiteboard_conflicts.direct_overlap`
  keyed on string equality of `target_file` + `target_location`. NOT a
  symbol-DAG check. For symbol-level conflict detection, council members
  must encode symbol IDs into those fields by convention.

### MCP surfaces

- **`src/orchestration/mcp.rs:140-176`** — `intersect_allow_from` — surface
  filter intersection.
- **`src/server/progress.rs`** — `surface_to_filters`.
- **`src/orchestration/mod.rs:493-670`** — `apply_ambient` — ambient
  scope prompt injection, runs `apply_brofile_lens` after. Extension point
  for coerce_workspace appendix is after `TASK_SHAPE_HINT`.
- **`src/orchestration/providers.rs:790-850`** — `build_filter_args` —
  per-provider tool disablement (Claude `--disallowedTools`, Copilot
  `--deny-tool=`, Codex `disabled_tools`, Gemini `--policy <tempfile>`).
  Operates on MCP tool names only. **Provider built-ins (Edit/Write/Bash)
  are outside the MCP catalog and cannot be disabled via this path
  without a parallel mechanism.**

### Agent system (typed install artifacts — distinct from actor system)

- **`examples/agents/code-reviewer.json`** — full agent manifest example.
  Fields: kind, name, version, manifest (description, when_to_use,
  anti_patterns, brofile_ref or brofile_inline, filter_overlay,
  inputs.schema, prompt_template, outputs.schema, evidence_density,
  composition {chainable_after, parallel_safe, fan_out_aggregator},
  cost_class, provenance).
- **`examples/agents/diff-narrator.json`** — example with `brofile_inline`.
- **`examples/agents/workflows/chain.json`** — chained `bro_agent_dispatch`
  + `bro_wait` pattern.
- **`examples/agents/workflows/fan-out.json`** — parallel
  `bro_agent_dispatch` + `bro_wait` + aggregate pattern.

### Scout exemplar

- **`.claude/agents/corpus-pathfinder.md`** — Claude-frontmatter agent.
  User-stated direction: this should be lifted to a JSON manifest in
  `examples/agents/` so it's harness-agnostic and usable from
  cross-provider workflows. The `.md` body becomes the lens; structured
  output (currently markdown sections) should be enhanced to strict-typed
  JSON so downstream consumers can dedup mechanically. Selection cues,
  filter_overlay, composition (`parallel_safe`, `fan_out_aggregator`),
  cost_class need to be added.

### Whiteboard arc as composition template

- **`examples/whiteboard/workflows/whiteboard-arc.json`** — full pipeline:
  Setup → OpenBoard → BlindPost (ensemble actor) → TransitionToDebate →
  Debate → TransitionToResolve → Synthesize (facilitator executor) →
  PushAndOpenPr → AwaitMerge → Done. **This is the canonical
  verdict-resolution pattern for multi-advisor panels.**

### Daystrom (reference donor; DON'T treat dispatch-v2 as implemented)

- **`../daystrom-mk2/design/dispatch-v2.md:5`** — *"Status: Design
  document. Not yet implemented."* Overminds, scope_expansion_request,
  cluster mediation are design-stage.
- **`../daystrom-mk2/design/work-order-process.md:399`** — overminds
  disabled for normal dispatch.
- **`../daystrom-mk2/src/Daystrom.Worker/Tools/FileTools.cs:63-101`** —
  daystrom workspace tools (`file_read`, `smart_read`, `file_edit`,
  `file_write`, `file_search`).
- **`../daystrom-mk2/src/Daystrom.Worker/Tools/GitWorkspaceTools.cs:51-78`** —
  daystrom git workspace tools (`git_commit`, `git_status`, `git_log`,
  `git_diff`, `git_show`).
- **`../daystrom-mk2/src/Daystrom.Worker/Tools/ContextSearchTools.cs:21`** —
  `context_search` with graph overlay.
- **`../daystrom-mk2/src/Daystrom.AgentSdk/AnomalyDetector.cs`** —
  5-pattern anomaly detection (Loop, Stall, TokenBurn,
  CompactBoundary, RateLimit). Each with Amber + Red thresholds.
  Loop: 3/6 in window of 10. Stall: 180s/360s. CompactBoundary: 2/4
  in 300s. TokenBurn: 2x/3x rolling baseline. RateLimit: 80%/95%.
- **`../daystrom-mk2/src/Daystrom.AgentSdk/OracleSink.cs:9-26`** — oracle
  per-instance behavioral classifier. Haiku-class. Classifications:
  `nominal | loop_detected | scope_creep | fabrication | stuck`.
  Synchronous `CanComplete()` gate via `SupervisionConfig.ClassificationThreshold`
  (default 0.7).
- **`../daystrom-mk2/src/Daystrom.AgentSdk/OracleSink.cs:236-256`** — lazy
  session creation (oracle not dispatched until first executor event).
- **`../daystrom-mk2/src/Daystrom.Worker/Services/AgentCheckpointService.cs`** —
  checkpoint includes provider/model/system prompt/sandbox/MCP/working dir/
  worktree context/account/CLI session id.
- **`../daystrom-mk2/design/advisor-refinement.md`** — advisor as
  post-completion quality gate, distinct from oracle. Design-stage; not
  implemented. Two-shot execution policy (cheap → single tier-escalation
  → fail).
- **`../daystrom-mk2/src/Daystrom.Worker/Services/BuildGateRunner.cs:108-138`** —
  acceptance criteria collection.
- **`../daystrom-mk2/src/Daystrom.Worker/Services/BuildGateHook.cs:343-358`** —
  `AcceptanceCriteriaPolicy` injects acceptance criteria as verification
  message after build gate passes (one-shot via `_acceptanceCriteriaInjected`
  flag).
- **`../daystrom-mk2/src/Daystrom.Worker/Services/BuildGateHook.cs:83-90`** —
  policy chain: `ChangedFilesRequirement` → `AcceptanceCriteria` →
  `SastDelta` → `TestDelta` → `BuildFailure`. Each emits
  `BuildGatePolicyOutcome(Injection?, FailureReason?, FailureOutput?)`.
  First failure verdict wins.

---

## 6. What predecessor never properly grounded

Honest list. Successor should look at these before authoring anything on
them.

1. **How `bbox_status(task_id, tail=N)` actually returns events.** The
   predecessor assumed it returns raw events from `inner.events`, but
   didn't verify the shape. `task_status_json` function — never read.

2. **Whether the workflow engine has any internal subscriber for the
   per-task event stream.** Predecessor reported there isn't one based
   on `engine.rs:1575` synchronous wait — but didn't survey all engine
   call sites that might consume events from `inner.events`.

3. **How `bro_orchestrate_run` actually dispatches a workflow** (the
   `mod.rs` engine internals around node lifecycle, signal binding to
   arcs, fork/wait_for synchronization).

4. **What `signal_arc_dispatch` does to a node currently in the middle
   of waiting on `wait_for_task_with_timeout`.** Predecessor assumed
   signal resolves a *separate* Wait, not the actor's task wait. Worth
   verifying the engine's signal-handler semantics inside a Wait/actor
   node.

5. **How `apply_advisor_packet`'s output is consumed** beyond producing
   the verdict prompt body. The downstream effect of `consequent` /
   `confidence` fields — not traced.

6. **What `keep_going` means in `AdvisorMemberCheckpoint`.** Predecessor
   transcribed it from `roster.rs:996-999` but didn't trace its origin
   or semantics.

7. **Whether `late_inject` (in `NodeSpec`) is the natural seam for advisor
   verdict feedback when `AdvisorMode::Background`.** Predecessor
   guessed yes but didn't read the late_inject implementation.

8. **The actual policy_packet evaluation entity shape.** Predecessor saw
   it cited but didn't read what fields the arc-level policy packet sees
   when fired at node boundaries.

9. **The relationship between `compaction_anchor: true` on `ActorSpec`
   and the broader checkpoint-on-wait-boundary pipeline.** Different
   anchoring mechanisms; predecessor conflated them at points.

10. **`bbox_arc_status`, `bbox_signals`, `bbox_webhook_deliveries`** —
    MCP tools that exist but weren't surveyed; relevant for
    understanding what an oracle co-session can observe of the arc.

11. **`bro_orchestrate_author`** (`tools/orchestrate.rs:11`) — workflow
    authoring tool, not read.

---

## 7. Anti-patterns predecessor exhibited

Successor should be skeptical of any of these:

1. **Authoring before grounding.** Multiple times the predecessor sketched
   architectures (`AdvisorChain`, `AdvisorRef`, `OracleSpec`, `OverseenMode`,
   `AdvisorVerdict`) without first reading the existing schemas. When
   reading happened, the proposals were revealed as either duplicating
   existing structures or misshapen.

2. **Dumping lists when asked for one thing.** User repeatedly asked for
   focused answers; predecessor responded with tables and 13-item lists.

3. **Confabulating reference patterns.** Predecessor said "daystrom's
   three DAG layers don't compose" without examining the actual
   architecture — user correctly reframed as Swiss-cheese defense.

4. **Conflating orthogonal systems.** Predecessor repeatedly conflated:
   - Actor system (workflow engine invocation, 2 kinds) with
   - Agent system (typed install artifacts via `bro_agent_dispatch`).
   User had to call this out explicitly: "They are orthogonal - and you
   are reinventing elements of BOTH."

5. **Pretending judgment-by-classifier is mechanical.** Predecessor
   repeatedly proposed "oracle emits classification, framework routes"
   theatre. User correctly named this as hiding the judgment call.

6. **Dressing the same answer in different clothes.** When user said
   "advisor is the judgment layer," predecessor agreed, then proceeded to
   propose oracle/advisor collapse — exactly the lazy move user had
   flagged.

7. **Inventing field names / tool params that don't exist.** Specifically:
   `bbox_search(filter=...)`, `bbox_note(kind=tool_gap)`,
   `bbox_notes(target_file=...)`.

8. **Calling structs "fully baked" after reading the struct definition
   only.** TeamAdvisorConfig — predecessor saw the fields, declared the
   schema correct without checking whether the supporting functions
   (`build_advisor_checkpoint`, `apply_advisor_packet`,
   `maybe_resume_team_advisor`) implement the lifecycle. Only on further
   prodding did predecessor actually read the implementation.

9. **Proposing third documents when out of ideas.** "Let me write
   `design/control-plane.md`" — without authorization, without grounding,
   producing prose user didn't trust. Doc was deleted on user
   instruction.

---

## 8. Where the work is open

Per the user's expressed direction (NOT predecessor's interpretation), the
open work is:

1. **Decide what to commit and what to drop** among the existing
   half-baked docs (`design/phase-decomposer.md`,
   `design/workspace-tools.md`, `examples/tool-gap-analysis/`). The
   docs have value as inputs for the next pass, but neither is in a
   shippable state.

2. **Author (or commission codex to author) a control-plane design**
   that lands the actual three-layer architecture with proper
   grounding:
   - Mechanical counters as daemon-side state on `TaskInner` updated
     in the existing per-event hook at `mod.rs:1109`.
   - Oracle classifier as an installed agent
     (`examples/agents/oracle.json`) with polling discipline.
   - Advisor as the existing pipeline (`roster.rs:607-1069`) rehoused
     from `Team.advisor` to `NodeSpec.advisor` (rip and replace, no
     migration).

3. **Specify how the existing 5 advisor verdicts route through workflow
   transitions** — `CONTINUE | ESCALATE | CHARTER_DRIFT | EXIT_MET |
   REPLACE_BRO`. Especially `REPLACE_BRO` whose consumer logic does NOT
   exist in code yet (only the prompt vocabulary).

4. **Specify the downstream mediation Swiss-cheese** (M1-M6) the user
   asked for — what each layer is, what its inputs/outputs are, where
   it composes with the existing primitives (mechanical merge, then
   conflict-resolver agent, then mediation panel via whiteboard, then
   regression-fixer, then recompose advisor, then human).

5. **Enhance `corpus-pathfinder.md`** to an installed JSON agent
   manifest in `examples/agents/`. Structured strict-typed output
   (Tldr/Leads-symbols/Leads-refs/Leads-paths/Next-hops/Bundle/Limits/
   Gap-check as JSON fields), `filter_overlay.disallow` matching the
   current Claude `disallowedTools`, composition metadata
   (`parallel_safe: true`, `fan_out_aggregator: "ensemble-merge"`),
   `cost_class: "cheap"`. The `.md` body becomes the lens.

6. **Define the discovery orchestrator** (large-context Opus/Codex) as
   an installed agent — its job: read proposal, fan out scouts (foreach
   over question shapes), dedup + synthesize aggregate bundle, drive
   iterative scouting until coverage is sufficient.

7. **Specify the daemon-side anomaly counter wiring concretely** — state
   on TaskInner, threshold defaults from daystrom, signal emission via
   `signal_arc_dispatch`, optional immediate cancel via `cancel_task`
   on Red.

8. **Fix the workspace-tools doc's concrete errors** flagged by codex
   round 2:
   - Naming consistency (`tool_call`, not `tool_call_event`).
   - Drop `bbox_apply` as a write primitive (it isn't one).
   - Specify `bbox_tool_calls` MCP tool (since `bbox_search` lacks the
     fields).
   - Replace `bbox_note(kind=tool_gap)` directive with a valid kind
     (probably `learned` or simply drop).
   - Drop `bbox_notes(target_file=...)` directive.

9. **Redraft `examples/tool-gap-analysis/workflows/*.json`** to match
   actual schema. Drop the packet stub entirely (wrong primitive).

10. **Address codex's "live-signal seam"** finding (`note-eaeb41a8`) —
    the gap between post-index `tool_call` documents and the live
    scope-check signal phase-decompose §7 needs. The per-event hook at
    `mod.rs:1109` is the seam; predecessor identified it but didn't
    bake into the design.

---

## 9. Resuming codex

`bro_resume(provider="codex",
session_id="019e12d1-a673-7913-b191-9ea94a2ecc74", prompt="<...>")`.

Suggested resume prompt content (do not paste verbatim; tighten as
needed):

> Pivot since your last review. The conversation evolved past the
> control-plane sketches into a layered architecture the user converged
> on:
>
> 1. Mechanical counters in daemon (per-task state at `mod.rs:1109`, fire
>    via `signal_arc_dispatch` and optionally `cancel_task` on Red).
> 2. Oracle classifier as installed agent (polls primary, emits
>    `bbox_note(kind=surprise|dispute)`).
> 3. Advisor pipeline (existing — `roster.rs:607-1069`) rehoused from
>    `Team.advisor` to `NodeSpec.advisor`. Rip and replace, no migration.
>
> 5 verdicts already defined (`CONTINUE | ESCALATE | CHARTER_DRIFT |
> EXIT_MET | REPLACE_BRO`); `REPLACE_BRO` consumer logic doesn't exist
> in code yet — that's the tier-escalation primitive.
>
> Downstream mediation is also Swiss-cheese (M1 mechanical merge → M2
> conflict-resolver agent → M3 mediation panel via whiteboard → M4
> regression-fixer → M5 recompose advisor → M6 human).
>
> The user lost trust in the predecessor's output due to repeated
> proposing without grounding. Your job: ground everything in code, do
> not propose anything you haven't verified by reading. Produce a
> control-plane design that respects what's already wired and surfaces
> what isn't.
>
> Specific things to verify before asserting:
> - `bro_status(task_id, tail=N)` event shape (`task_status_json`)
> - `apply_advisor_packet` consumer side (what consumes the verdict
>   downstream)
> - workflow engine signal handling inside actor waits
> - `late_inject` mechanism (is it the seam for Background-mode advisor
>   verdict feedback?)
> - whether `tool_call_event` should be a new doc-type in tantivy or
>   live elsewhere
>
> Open work list in `/home/invidious/repos/transcript-search/HANDOFF.md`.
> Conversation provenance: `thread-ffe3c075`, 8 codex notes from prior
> reviews. Existing artifacts at `design/phase-decomposer.md`,
> `design/workspace-tools.md`, `examples/tool-gap-analysis/` — all
> half-baked, none in committable state.
>
> Author the control-plane design IF you can ground it adequately.
> Otherwise scope a smaller task that you can ground.


The grounding bar for this design space is high because: (a) the existing
substrate is rich and mostly-correct; (b) the user knows the substrate
better than any agent reading the code for the first time; (c) the failure
mode is hallucinated architecture that looks plausible but duplicates or
contradicts what's already there.

The user will reward concrete grounding (file:line refs, verbatim quotes,
"I read X and Y means Z"). The user will not reward speculation, lists of
options, or proposed designs that haven't been verified against the
existing code.
