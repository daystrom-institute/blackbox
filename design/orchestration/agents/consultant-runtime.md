---
title: "Consultant Runtime - Badgey dissolution into generic primitives"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - orchestration
  - agents
  - consultant-runtime
brief: "Dissolve the bespoke Badgey consultant machinery into a generic, atom/workflow-native consultant runtime; Badgey becomes one configured consumer. Resolves gap-9dae9a60."
---

# Consultant Runtime — Badgey dissolution into generic primitives

Tracking: `gap-9dae9a60` (`agent/consultant-runtime/atom-workflow-stateful-consultant`),
thread `thread-a6523755` (badgey-dissolution).

## 1. Thesis

Badgey was built as a forcing function: a stateful consultant whose needs —
instance identity across turns, a proposal lifecycle, privileged
wrapper-mediated dispatch — were supposed to surface the generic primitives the
substrate was missing. The forcing worked (the primitives exist and are tested:
ProposalStore, ActionJournal, ResumeQueue, instance registry, split-apply
protocol), but the extraction never happened. Every one of those primitives
shipped wearing the Badgey name, persisted under `state_dir/badgey/`, surfaced
through `badgey_*` MCP tools, and wired into `SharedState` as
`badgey_registry` / `badgey_proposals` / `badgey_journal`.

The dissolution inverts that: the generic machinery becomes a first-class
**consultant runtime** owned by the orchestration layer and reachable through
atom contracts and workflow nodes; Badgey shrinks to one *configured consumer*
— an agent artifact + brofile lens + consumer descriptor — with no bespoke
daemon code beyond configuration and (transitionally) tool-name shims.

`design/corpus/badgey.md` §14 already states the boundary ("not the agent
system"); `design/orchestration/agents/agent-system.md` §1.1 already frames
Badgey as "a consultant-flavored [agent] with extra producer machinery … Most
agents will not need that machinery." This design makes that machinery
something any agent *can* opt into.

## 2. What is generic vs. Badgey-specific (inventory)

Generic consultant machinery currently wearing the Badgey name:

| Facility | Today | Genericity |
| --- | --- | --- |
| Instance identity | `BadgeyId`, `BadgeyInstance`, `BadgeyRegistry` (`src/orchestration/badgey/types.rs:13`, `registry.rs:12`) | Fully generic: `(id, scope, provider, session, thread_of_record)` |
| Proposal lifecycle | `BadgeyProposal`, `ProposalState` (Pending→Applying→Applied/Failed), `ProposalStore` w/ idempotency keys + CAS file locks (`proposals.rs:91`) | Generic state machine; only `ProposalKind` vocabulary is consumer-flavored |
| Action journal | `ActionJournal`, `ActionJournalState` (Seen→Dispatching→Completed/Failed), first-write idempotency, archival (`journal.rs`) | Fully generic |
| Turn serialization | `ResumeQueue` (cap 3, priority enqueue, close-and-clear) (`queue.rs`) | Fully generic |
| Split apply | `begin_apply` / dispatch / `complete_apply` (`src/tools/badgey/proposals.rs:276` ff.) | Generic protocol; artifact-kind mapping is consumer config |
| Intent post-processor | `bg-action-*` note parsing post-turn, privileged dispatch outside the LLM loop | Generic pattern; note prefix + action vocabulary are consumer config |
| Restart recovery | `restore_badgey_registry_from_notes`, `recover_badgey_non_terminal_state` (`src/server/background.rs:49-56`) | Generic duty of the runtime |
| System instance per channel | `badgey_ensure_for_channel` get-or-create (`src/tools/badgey.rs:243-336`) | Generic "system instance per binding" pattern |

Genuinely Badgey-specific (stays with the consumer):

- `ThreadEvent` conversation vocabulary (Exec/Turn/PathCached/ScoutDispatched/
  SubbroSpawned/Proposal*/DisputeEscalated/Dismiss) and its note-kind mapping
  (`events.rs`).
- `BadgeyScope` brief/budget semantics, turn/path/scout observability counters,
  budget extensions.
- The persona: `badgey-persona` brofile lens, scout mode, wrapper command
  syntax (`commands.rs`).
- `ProposalKind` *vocabulary* (Workflow/Packet/Brofile/Lens/Agent/
  RedispatchTask/ArtifactPromotion) and the kind→artifact-kind mapping.
- The `system-defaults/badgey/` artifact family (agents, brofiles, workflows,
  packets, crons) and `eval/badgey/`.

## 3. Target shape

New module `src/orchestration/consultant/` owning the generic core:

- `ConsultantId` / `ConsultantInstance` / `ConsultantRegistry` — instance
  identity, session binding, per-instance `ResumeQueue`. Instances carry a
  `consumer: ConsumerRef` (e.g. `"badgey"`).
- `ProposalStore<state_dir/consultant/<consumer>/proposals/...>` with
  string-typed `kind` validated against the consumer's declared vocabulary;
  the Pending→Applying→Applied/Failed machine, idempotency keys, and CAS
  semantics move unchanged.
- `ActionJournal` — unchanged semantics, consumer-scoped paths.
- **Consumer descriptor** (the configuration boundary): adapter name, brofile
  ref, intent-note prefix (`bg-action-` for Badgey), action vocabulary +
  handlers, proposal-kind vocabulary + artifact-kind mapping, event
  vocabulary → note-kind mapping, scope-bind template. Installed/validated
  like other catalog data; the Badgey descriptor is the first entry.
- **Runtime services**: turn loop (launch/resume against the queue), intent
  post-processor (parse consumer-prefixed intent notes post-turn, journal,
  dispatch), split-apply executor, restart restore/recover. All keyed by
  consumer descriptor — no `badgey` branches in the runtime.

Exposure (the part the gap names "atom/workflow-native"):

- **Atom contract**: a `consultant` atom backend (sibling of
  profile/workflow/deterministic/adapter) where `atom_invoke` opens or resumes
  a consultant instance turn and `atom_resume` is a subsequent turn against the
  same instance — giving consultant sessions the standard invocation handle,
  effects/limits, and composition ownership instead of a bespoke adapter.
- **Workflow visibility**: proposal begin/complete-apply reachable as workflow
  ops/nodes (not just `mcp_call` to `badgey_*` by name), so apply arcs are
  consumer-agnostic.
- **MCP surface**: generic `consultant_*` tools (exec/resume/dismiss/status/
  list/proposals/apply/begin/complete) taking a `consumer` parameter;
  `badgey_*` tools become thin shims that pin `consumer="badgey"` during the
  transition.

## 4. Dissolution concerns

These are the load-bearing risks; each phase below must hold them.

1. **Persisted-state migration.** Proposals and journal live at
   `state_dir/badgey/{proposals,action_journal}` with file locks and an
   `_archive/` convention. The generic runtime must either read the legacy
   paths for the Badgey consumer or migrate once, atomically, preserving
   idempotency keys, lock semantics, and archived entries. Mid-flight
   non-terminal proposals/actions across a daemon upgrade are the hard case —
   recovery already marks orphans Failed; migration must not manufacture
   spurious failures.
2. **Notes are load-bearing state, not telemetry.** The registry is *restored
   from notes* at startup (Exec ThreadEvents, `badgey:<id>` thread names), and
   observability counters (turns, paths, budget extensions) are derived by
   scanning notes. Historical notes will keep the old shapes forever; the
   generic restore/derive paths need consumer-keyed parsers that still accept
   the legacy Badgey grammar.
3. **Intent grammar is split across prompt and parser.** The `bg-action-*`
   vocabulary lives half in the persona lens (prompt prose telling the model
   what to emit) and half in the post-processor (parser + handlers). They must
   move together; a generic runtime that changes the parser without the lens
   (or vice versa) silently drops intents — and intent notes are how proposals
   and scouts get created at all.
4. **The wrapper is a security boundary.** The intent post-processor is what
   lets a `bro_*`-denied persona cause privileged dispatch *outside* the LLM
   loop — that is the recursion-guard design. Generalizing it must not become
   "any consumer descriptor gets privileged dispatch": descriptors need the
   same install-time gating as agents/adapters today (`mod.rs` artifact tests
   gate `dispatch_adapter` on the adapter registry), and action handlers must
   stay an allowlisted, code-owned set — descriptors select from them, never
   define new ones in data.
5. **Tool-name coupling in shipped artifacts.** `system-defaults/badgey/`
   workflows/packets/crons call `badgey_*` tools by name through `mcp_call`,
   and operator muscle memory + docs do too. The `badgey_*` shim layer must
   survive until those artifacts are re-pointed (or made consumer-agnostic),
   and `tool_docs.rs` stanzas must track every shim and generic tool or the
   docs test fails.
6. **External identity persistence.** Slack channel bindings persist
   `badgey_id` and dismiss instances on unbind (`src/tools/config.rs`). Field
   renames here are a data-compat problem (existing bindings on disk), not
   just a code rename.
7. **Idempotency must survive bit-for-bit.** Proposal idempotency keys are
   `(kind, draft)`-derived; journal `record_seen` is first-write-idempotent.
   Any change to key derivation or path layout during the move can double-
   apply proposals or replay actions after an upgrade.
8. **Workflow/schema touchpoints.** `src/workflow/schema.rs:22` and
   `src/workflow/engine/actor_nodes.rs:152` couple workflow machinery to
   Badgey shapes (per gap evidence); the dissolution must replace these with
   consumer-agnostic shapes, not add a second special case.
9. **Test and eval tracking.** ~28 unit tests across the badgey modules and
   the `eval/badgey/` harness (9 manifests) encode the current semantics.
   They move with the code in the mechanical phase (proving behavior
   preservation) and only then get generalized counterparts.
10. **Atom-contract fit is not free.** Atoms are task/invocation-shaped;
    consultant instances are long-lived with their own queue and restart
    recovery. The atom backend must map instance turns onto invocation
    handles without pretending a consultant is terminal — `atom_status` on a
    consultant invocation reports the *turn*, not the instance lifetime; the
    instance outlives any invocation. Getting this mapping wrong re-creates
    the bespoke adapter under a new name.

## 5. Phases

- **Phase 0 — design + gap detail.** This document; gap-9dae9a60 updated with
  the concern inventory. (Done in the badgey-dissolution worktree.)
- **Phase 1 — mechanical core extraction (no behavior change).** Move
  `types/registry/proposals/journal/queue` from `src/orchestration/badgey/`
  to `src/orchestration/consultant/` with generic names (`ConsultantId`,
  `ConsultantRegistry`, …); `badgey` module re-exports aliases; `SharedState`
  fields rename with construction unchanged; state paths, note grammar, tool
  names, and artifacts untouched. Tests move and stay green
  (`cargo nextest run --workspace`). Concerns 1/2/5/6 are explicitly *not*
  triggered because no persisted shape changes.
- **Phase 2 — consumer descriptor boundary.** Introduce the descriptor;
  parameterize scope-bind, intent-note prefix, action vocabulary (selecting
  from code-owned handlers), proposal-kind vocabulary, event→note mapping.
  Badgey descriptor reproduces current behavior exactly; runtime loses its
  last `badgey` literals except the shim layer. Concern 3/4 live here.
- **Phase 3 — atom/workflow-native surface.** Consultant atom backend,
  workflow ops for split apply, generic `consultant_*` MCP tools; `badgey_*`
  become shims; `system-defaults/badgey/` arcs re-pointed or made
  consumer-agnostic. Concerns 5/8/10 live here.
- **Phase 4 — state migration + retirement.** Migrate
  `state_dir/badgey/` → `state_dir/consultant/badgey/` (or adopt legacy-path
  read for the Badgey consumer permanently), deprecate shims, update
  `docs/badgey.md` + add `docs/consultant-runtime.md`, archive this design to
  implemented. Concerns 1/6/7 live here.

## 6. Non-goals

- No change to Badgey's persona, modes, eval checklists, or operator-facing
  behavior; an operator should not be able to tell Phase 1–2 happened.
- No new consultant consumers shipped as part of the dissolution (the point is
  that a second consumer *could* be configuration, not that we ship one).
- No redesign of proposals/journal semantics — the state machines move as-is;
  semantic upgrades are follow-on work once generic.
