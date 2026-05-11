# Badgey — agentic-corpus consultant persona

> **Note (post-agent-system).** Badgey is a consultant-flavored
> **agent** under the broader registry described in
> `design/agent-system.md` (`agent:badgey@v1`). This doc predates
> the agent-system doc; per agent-system §11.1, every reference
> here to "the badgey brofile" should be read as "the badgey
> agent's `brofile_ref`". The wrapper-mediated dispatch path
> (§2.2 below) is the canonical mechanism for *consultant-flavored*
> agents; generic agents reach the substrate through `bro_agent_*`
> instead. Both paths converge at `bro_exec` underneath.

## 1. Thesis

The agentic-corpus search surface (`design/agentic-corpus.md` §4) gives the
calling LLM graph-native primitives but expects it to learn the
seed → inspect → traverse → bundle protocol. The MCP descriptions cue the
loop, but cold providers without `CLAUDE.md` baked don't reliably follow
it, and even warm providers re-pay the discovery cost every session.

Badgey is the embodied protocol. A long-lived MCP-addressable agent
identity — exec once, resume for an hour or a day — that hides the
graph-nav loop behind a handoff. Outside callers (Codex, Gemini, or a cold
Claude session) reach into the corpus through one of three modes: answer,
teach, or scout. The scout returns evidence bundles. The teacher returns
worked walkthroughs anchored on the caller's actual current corpus. The
answer is just the bundle.

This is **agentic-corpus made conversational**. The substrate stays the
same (entity graph, hybrid search, edge taxonomy, provenance machinery).
What changes is who navigates it: a specialist with accumulated state,
instead of a fresh client every turn.

### 1.1 Why not just sharpen the tool descriptions?

Agentic-corpus §4.2 takes the position that "if the descriptions aren't
enough, the answer is sharper descriptions, not a wrapper." Badgey does
not contradict that for the synchronous read path; the `bbox_*` graph
primitives stay primary, their descriptions stay the cuing layer for
direct use.

Badgey solves a different problem:

1. **State across turns is the differentiator.** Descriptions teach the
   protocol once per session; they do not carry path-cache, entity-ref
   accumulation, or proposal drafts across turns. A 4-hour investigation
   building on its own intermediate results cannot be encoded as a tool
   description.
2. **Cross-provider parity.** `CLAUDE.md` rendering reaches Claude only.
   Codex and Gemini cannot consume rendered Claude memory; they only see
   MCP tool descriptions. Sharper descriptions help, but they don't carry
   the worked-example density that an agent loaded with the design doc
   does in teach mode.
3. **Producer-side capabilities are inherently agent-shaped.** Inbox
   triage, completion-contract closer, and proposal recursion (§6.3-6.5)
   require LLM judgment over the corpus. They are not synchronous reads;
   they are background-batch arcs. Badgey is the conversational handle on
   that batch surface.
4. **Compounding.** Every turn refines what the next turn knows. That is
   the cosession property. A stateless tool, no matter how well-described,
   cannot offer it.

The honest comparison is: badgey vs. (sharper descriptions + better
worked examples in CLAUDE.md). The latter helps. Badgey is additive on
top — it carries state, reaches non-Claude providers, and exposes
producer-side capabilities the synchronous tools cannot.

The eval-arc (§12) is the empirical check: if `bbox_*` direct use with
sharpened descriptions can match badgey's score on the answer-mode
suite, badgey's value is teach + scout + producer side, not answer. The
doc accepts that risk explicitly and treats answer-mode as the
collapsible mode if descriptions improve enough.

## 2. Architecture

Badgey is **not a new daemon, not a new index, not a new dispatcher**. It
is three things composed:

| Layer | What |
|---|---|
| Brofile + lens | persona prompt steering badgey to graph-native investigation |
| Tool wrapper | dedicated `badgey_*` MCP tools wrapping `bro_exec` / `bro_resume` |
| Wrapper-side state | path cache, entity-ref bag, proposal index — held in `src/orchestration/badgey.rs`, mirrored to durable stores |

Critical correction from the substrate's actual shape: a "badgey instance"
is **logical, not a single process**. `bro_resume` spawns a fresh provider
CLI process per turn; only the provider's session history persists between
turns. Therefore:

- The wrapper module (`src/orchestration/badgey.rs`) is the canonical
  state owner.
- Each turn's bro process loads context from (a) provider session history
  (provider-side), (b) the wrapper-injected scope-bind block (§7.3), and
  (c) thread-of-record posts (§8.1) the wrapper surfaces.
- "In-instance state" means state *associated with the badgey_id*, not
  state *resident in a process*. It survives per-turn process churn,
  daemon restarts, and provider failures.

### 2.1 Layering

```
caller LLM (Claude / Codex / Gemini)
   │
   ▼  (MCP transport)
bbox daemon
   │
   ├─ bbox_*  ──────► graph primitives ──► tantivy + vectors + edges
   │
   ├─ bro_*   ──────► generic dispatch (existing tool-name filters)
   │
   └─ badgey_* ─────► wrapper (src/orchestration/badgey.rs)
                          │
                          ├─ owns badgey_id ↔ (provider, session_id) map
                          ├─ owns BadgeyProposalStore (§8.3)
                          ├─ owns resume FIFO queue per instance
                          ├─ owns turn-post-processor (parses badgey
                          │   emissions, dispatches privileged actions)
                          ├─ owns scout dispatch (off the resume queue)
                          ├─ recognizes wrapper-direct commands
                          │   ("apply P-N", "dismiss", "expand bg-path-N")
                          │   without invoking badgey
                          │
                          ▼ (LLM-mediated turns only)
                     bro_resume(brofile="badgey")
                          │
                          ▼
                     ephemeral bro process for THIS turn
                          │
                          ├─ bbox_* via MCP back to daemon
                          ├─ bbox_thread (writes to thread-of-record)
                          ├─ bbox_note (writes proposal drafts +
                          │   structured `bg-action-*` request notes)
                          │
                          ✗ bro_exec / bro_resume DENIED for badgey
                            (and badgey-scout) by the standard
                            tool-name filter chain
```

The wrapper is the only actor with `bro_*` privileges in badgey's
execution lane. Badgey writes data; the wrapper dispatches.

### 2.2 Recursion guard — wrapper-mediated, not arg-shape-validated

Codex review finding: the existing daemon filter chain is tool-name
allow/deny; it cannot express "allow `bro_exec` only with
`brofile=badgey-scout` and `allow_recursion=false`", and the daemon has
no inbound caller-identity to gate on. The doc previously hand-waved a
new substrate capability. Removed.

Cleaner architecture: badgey does NOT call `bro_exec` at all.

| Caller flavor | `bro_*` filter |
|---|---|
| External (default callers) | existing rules apply |
| Badgey | `mcp__blackbox__bro_*` DENIED entirely |
| Badgey-scout | `mcp__blackbox__bro_*` DENIED entirely |

Sub-bro spawning, proposal application, and re-dispatch all happen
through the **wrapper's privileged dispatch path** (the wrapper is
ordinary trusted Rust code in `src/orchestration/badgey.rs`, not an
MCP client), driven by data badgey emits as `bbox_note` calls during
its turn.

The mechanical model:

1. badgey runs a turn, emits structured intent via `bbox_note(kind=followup,
   body={event:"bg-action-...", action_id:"<uuid>", ...})` posts on its
   thread-of-record. `action_id` is badgey-minted (UUIDv4); badgey is
   instructed by the lens to mint a fresh one per action and re-use the
   same id only when explicitly retrying.
2. after the bro process terminates, the wrapper queries
   `bbox_notes(thread_id=<thread_of_record>, since=<turn_start>)` and
   filters for `body.event LIKE 'bg-action-%'`.
3. for each action, the wrapper consults the **action journal** at
   `$BLACKBOX_STATE_DIR/badgey/action_journal/<action_id>.json` (§8.3).
   If the journal entry already exists with `state IN
   {dispatching, completed, failed}`, the action is skipped or
   resumed per state. If absent or `state=seen`, the wrapper:
   a. atomically writes journal `state=seen` (no-op if already seen)
   b. validates request shape; on bad shape transitions to
      `failed(invalid_shape)`
   c. transitions journal `seen → dispatching` and invokes the
      privileged action (`bro_exec` for sub-bros,
      `bbox_artifact_install` for accepted-and-applied proposals, etc.)
   d. on action completion, transitions journal
      `dispatching → completed(result_ref)` and posts
      `bbox_note(kind=learned, body={event:"bg-action-completed",
      action_id:..., result_ref:...})`
   e. on action failure, transitions to `failed(reason)` and posts
      a `bbox_note(kind=learned, body={event:"bg-action-failed", ...})`
4. the wrapper returns an enriched result to the caller that includes
   both badgey's bundle AND the dispatched-action results.

Crash recovery: if the wrapper dies between 3a and 3d, the journal
entry is in `seen` or `dispatching`. On restart, the wrapper scans
the journal for non-terminal entries and:
- `seen` → re-validate + re-dispatch (no privileged action ran yet,
  per the journal contract)
- `dispatching` → the wrapper pre-mints the task id at the lower-
  level Rust spawn plumbing (NOT through the MCP `bro_exec` surface,
  which mints task ids daemon-side). Sequence: wrapper allocates
  `task_id`, fsyncs `dispatching.task_id` into the journal entry,
  THEN calls the daemon's internal spawn function with that
  pre-allocated id. On restart with a `dispatching` entry, the
  wrapper queries `bro_status(task_id)` to determine
  spawned/running/completed/never-started and resumes per result.
  This requires the daemon to expose an internal spawn API that
  accepts a caller-provided task id; currently the MCP-shaped
  `bro_exec` does not (OQ §15)

Action journal entries are append-only and never deleted; expired
entries (>30 days) are archived under `action_journal/_archive/`.

Recognized action events:
- `bg-action-spawn-subbro` — wrapper invokes the daemon's internal
  bro-spawn helper (the pre-mint-task-id Rust API named in OQ §15 #1,
  NOT the MCP `bro_exec` surface) with `brofile="badgey-scout",
  allow_recursion=false, ...`
- `bg-action-emit-proposal` — wrapper writes the draft to BadgeyProposalStore (§8.3) in `pending` state
- `bg-action-escalate-dispute` — wrapper wires user-arbitration follow-up
- `bg-action-extend-budget` — wrapper raises the per-turn cap and re-resumes

User-driven mechanical commands bypass badgey entirely:
- `badgey_resume(id, "apply P-N")` — wrapper executes the §6.3 apply
  state machine; badgey bro is NOT invoked
- `badgey_resume(id, "reject P-N")` — wrapper marks proposal failed
- `badgey_resume(id, "dismiss")` — wrapper runs §2.4 dismiss path
- `badgey_resume(id, "expand bg-path-N")` — wrapper looks up the path in
  badgey's path-cache mirror and returns it without consuming a turn

Anything else routes to badgey as an LLM-mediated turn. The wrapper's
command parser uses prefix-strict matching; ambiguous prompts go to
badgey unmolested.

### 2.3 Process model

| Concept | Concrete shape |
|---|---|
| Badgey instance | a `(badgey_id, scope, thread_of_record_id, provider_session_id)` tuple owned by the wrapper |
| `badgey_id` | `bg-<project_id_8hex>-<rand_8hex>` — stable for the lifetime of the thread-of-record |
| Per-turn process | ephemeral; spawned via `bro_resume(session_id=...)`; loads provider session history + wrapper-injected ambient scope |
| Discriminator | the thread-of-record's `name` follows the convention `badgey:<project_id_short>:<rand>` and `kind=work_item`; daemon discovery scans by name prefix |
| Wrapper module | `src/orchestration/badgey.rs` — owns instance registry, resume queue, scout dispatch |
| State store | thread-of-record posts (durable) + BadgeyProposalStore (proposals, §8.3) + artifact catalog (installed brofiles only) + wrapper memory (live caches, lost on daemon restart, rebuildable) |

Note on threads: the existing `Thread` struct (`src/threads.rs:151`) does
NOT carry a `tag` field. Badgey uses the `name` field with the
`badgey:...` prefix as the addressing convention. Per-event filtering
(triage / scout / proposal) lives in note JSON bodies (§8.1), not in
thread-level tags.

### 2.4 Cosession framing

The instance is durable across:
- caller turns
- caller sessions (the `badgey_id` and the underlying
  `AgentSession` handle — see `agent-system.md` §1.2 — are stable
  until dismiss or substrate TTL)
- daemon restarts (state replays from thread-of-record +
  BadgeyProposalStore; the wrapper's `badgey_id ↔ AgentSession`
  mapping is rebuilt from the `exec` event note's
  provider/session_id fields)

The instance is bounded by:
- explicit `badgey_dismiss`
- substrate TTL (TaskStore 24h since last activity; provider
  session GC). Per `agent-system.md` §1.2, badgey does NOT enforce
  its own additional idle timeout — sessions live as long as
  someone resumes them, subject to the substrate's TTL.
- daemon restart with `--evict-badgeys` (rare; for breaking-change rollouts)

This is the load-bearing distinction from one-shot. Triage proposals
stay addressable within the instance across hours. Investigation
context compounds turn over turn.

The term **cosession** here means the exec/resume property of a badgey
instance, not the existing `cosession` skill (informal Claude+Codex
pair work). Both coexist; they target different problems (§14).

### 2.5 MCP surface, not slash skill

Badgey is exposed as `badgey_*` MCP tools. NOT as a slash skill.

- slash skills bind to the caller's harness (Claude Code only); MCP
  tools work for every provider.
- slash skills run in the caller's context window; badgey runs in its
  own bro process per-turn.
- the MCP tool descriptions are the cuing layer — they teach outside
  callers when to reach for badgey vs. when to use `bbox_*` directly.

The line that has to land in the descriptions: **`bbox_*` tools are
graph primitives; `badgey_*` is a guided expedition.** Reach for
`bbox_*` when you know the entity. Hand off to badgey when you'd benefit
from a scout.

## 3. Sources

- `design/agentic-corpus.md` — substrate badgey navigates. §6 entity
  model, §4 search surface, §9 edge taxonomy, §14-15 provenance machinery.
- `src/orchestration/brofile.rs` + lens composition — badgey is one
  brofile (plus `badgey-scout`).
- `src/orchestration/` `bro_exec` / `bro_resume` — wrapped, not replaced.
- `src/main.rs` `bbox_artifact_install` / `bbox_artifact_list` /
  `bbox_artifact_supersede` — proposal install path.
- `src/workflow/ops.rs` `McpCall` hook op — workflow integration shape
  (§10.1).
- `src/threads.rs` Thread struct — addressing constraints (§2.3).
- `src/notes.rs` Note struct + JSON body — event filtering substrate.
- `bro_cron_install` — periodic triage / closer scheduling.
- The existing `cosession` skill — pattern adapted, elevated.
- daystrom-mk2 agentic-tools spike — protocol-cuing-via-tool-descriptions.

## 4. MCP surface

Eight tools.

### 4.1 Tool inventory

| Tool | Purpose |
|---|---|
| `badgey_exec` | Instantiate a badgey for a project scope. Returns `badgey_id` + `session_id`. |
| `badgey_resume` | Send a turn to an existing instance. Returns reply with citations. |
| `badgey_ask` | One-shot sugar: exec, ask, dismiss. For callers without continuity needs. |
| `badgey_scout` | Dispatch async investigation. Returns `scout_id` + `thread_id`. |
| `badgey_collect` | Retrieve scout result, or "still walking". |
| `badgey_triage_inbox` | Structured inbox triage with one-tap-apply proposal IDs. |
| `badgey_close_loops` | Completion-contract auditor. Structured proposal sheet. |
| `badgey_status` / `badgey_list` / `badgey_dismiss` | Lifecycle (one tool family). |

`triage_inbox` and `close_loops` have first-class shape because their
output is structured, schedulable as a poller, and consumed by
dashboards. Other capabilities (narrated blame, propose-workflow,
explain-decision) are conversational moves dispatched via
`badgey_resume`.

### 4.2 Scope

```rust
pub struct BadgeyScope {
    pub project_id: ProjectId,           // realpath hash, agentic-corpus §5.6
    pub initial_brief: Option<String>,   // optional user-supplied charter
}
```

V1 ships private-only. There is no `visibility` field; all instances
have full read access to the corpus they're scoped to. A
public-facing read-only scope is a v2 question (§16) — it requires
data-row filtering at the bbox_* tool layer (knowledge / notes /
transcripts have no row-level visibility today), which is out of v1
scope.

### 4.3 `badgey_id` format

`bg-<project_id_8hex>-<rand_8hex>`, e.g. `bg-3f7a91c4-d2e810ab`. Stable
for the lifetime of the thread-of-record. Globally unique within one
daemon. Not portable across daemons.

### 4.4 Tool descriptions — cuing the protocol

Per-tool description carries:
- one-sentence purpose
- when-to-prefer over `bbox_*` direct use
- example invocation pattern
- composition hint (what to call next)
- anti-pattern warning where the failure mode is predictable

Concrete example for `badgey_ask`:

```
Ask badgey a question that would otherwise require multiple bbox_* graph
calls. Returns an EvidenceBundle: { entity_refs[], paths[], narrative,
citations[] }. Prefer over hand-walking bbox_discover_seed_entities →
bbox_inspect_entity → bbox_find_paths → bbox_bundle_evidence when:
  1) you don't yet know which entity types to seed against
  2) the question crosses 2+ edge families (governance, lineage, blame)
  3) you'd benefit from a narrated answer over raw refs

Anti-pattern: do not ask badgey questions whose answer is one bbox_search
call. Direct primitives are cheaper.

After receiving a bundle, drill into specific refs with bbox_inspect_entity
or follow up with badgey_resume to keep state hot.
```

### 4.5 EvidenceBundle response shape

```json
{
  "narrative": "knowledge:8a3f12cd governs the F3 schema migration arc; ...",
  "entity_refs": ["knowledge:8a3f12cd", "thread:abc123", "commit:def456"],
  "paths": [
    {
      "id": "bg-path-1",
      "nodes": ["knowledge:8a3f12cd", "thread:abc123", "commit:def456"],
      "edges": ["DERIVED_FROM", "EDITED_BY_SESSION"],
      "summary": "the decide entry was authored in the F3 thread, which spawned the commit"
    }
  ],
  "citations": [
    { "entity": "knowledge:8a3f12cd", "claim": "stale_days default is 3", "kind": "exact", "verified_via": "bbox_inspect_entity" },
    { "entity": "thread:abc123", "claim": "F3 charter set this", "kind": "structural", "verified_via": "edge_present" }
  ],
  "follow_ups": [
    { "label": "drill into the supersession chain", "next": "badgey_resume(id, 'show me what 8a3f12cd superseded')" },
    { "label": "see the originating whiteboard", "next": "bbox_inspect_entity('whiteboard:wb-stale')" }
  ],
  "degraded": null
}
```

Two corrections from prior design:

**Paths are materialized, not referenced.** Prior version returned
`path_ids: ["P1", "P2"]` and assumed callers could pass those into
`bbox_bundle_evidence`. They cannot — agentic-corpus §5.7 makes the path
cache per-MCP-session and restart-droppable, so badgey's session-local
P-IDs are not portable to the caller's session. Badgey instead returns
full path payloads (nodes, edges, summary). The `id: "bg-path-1"` is
badgey-internal, addressable only within the same badgey instance for
follow-up turns ("expand bg-path-1"). It will not work in `bbox_*`
calls.

**Citation kinds are structural, not semantic.** A citation's `kind`
field reports what was mechanically verified before bundle return:
- `exact` — `bbox_inspect_entity` confirmed the entity exists with the
  property the claim references (e.g. claim says "stale_days = 3" and
  the entity's body contains that literal)
- `structural` — the entity exists and has the expected edge in the
  expected direction, but the claim's content is LLM-asserted not
  literal-matched
- `weak` — entity exists but the expected edge or property is absent;
  surfaced in `degraded.weak_citations[]`
- `unverified` — round-trip skipped (budget exhausted); citation
  present but not cross-checked

The doc does NOT claim badgey verifies semantic substantiation. That's
LLM judgment, calibrated only by the eval arc (§12) and user feedback.

`degraded` follows the agentic-corpus §4.4 shape extended with badgey
signals: `weak_citations[]`, `dispute_pending`, `budget_exhausted`,
`scout_recovered_with_losses`.

## 5. Modes — answer / teach / scout

Three usage patterns. Mode is implicit (badgey infers from question
shape) or explicit via a `mode` arg.

### 5.1 Answer

`badgey_ask(question)` or `badgey_resume(id, question)`. Caller wants
the bundle. Badgey runs the loop, returns narrative + refs + citations
+ follow-up offers.

### 5.2 Teach

`badgey_ask("how would i find ...")` or `mode="teach"`. Caller wants to
learn the protocol. Badgey returns a worked walkthrough using the
caller's actual current corpus, naming specific tools and example refs.

The walkthrough adds two structured fields beyond the standard bundle:

```json
{
  "narrative": "to find why a knowledge entry got superseded, you...",
  "steps": [
    { "tool": "bbox_inspect_entity", "args": "...", "rationale": "..." },
    { "tool": "bbox_find_paths", "args": "edge_family=SUPERSEDES", "rationale": "..." }
  ],
  "result_in_your_corpus": "...",
  "next_time_skip_badgey_when": "the question is single-edge or you already have the seed entity"
}
```

The `next_time_skip_badgey_when` field is load-bearing: badgey actively
pushes the caller toward direct `bbox_*` use when appropriate. Healthy
outcome is callers graduating off badgey for simple cases. Failure mode
(addressed in §11): badgey teaches anecdotes from its own search
trajectory rather than the protocol; eval-arc tracks "caller repeats
the walkthrough via direct `bbox_*` and gets the same bundle".

### 5.3 Scout

`badgey_scout(badgey_id, charter)` returns `(scout_id, thread_id)`.

Two phases:

1. **Charter authoring** — a regular badgey turn (queued through the
   resume queue). Badgey decomposes the user charter into focused
   sub-bro charters, writes them to the scout thread, returns. Brief.
2. **Sub-bro execution + monitoring** — wrapper-owned, OFF the resume
   queue. The wrapper invokes the daemon's internal bro-spawn helper
   (OQ §15 #1; pre-mint-task-id Rust API, not the MCP `bro_exec`
   surface) with `brofile="badgey-scout", allow_recursion=false, ...`
   for each authored sub-charter, watches for done-notes on each
   sub-bro's thread, collects results into the scout thread. Badgey
   itself is not blocked; the user can resume against the same
   instance for unrelated questions.
3. **Synthesis** (only on demand) — `badgey_resume(id, "synthesize
   scout ${scout_id}")` is a regular turn that reads the scout thread
   and returns an EvidenceBundle. Optional; the caller can also read
   the scout thread directly.

`badgey_collect(scout_id)` reads the scout thread without entering the
resume queue. Returns either `still_walking` with progress or `done`
with the result-set.

Use cases:
- questions whose answer requires walking >2 hops
- contradiction sweeps across the project
- cross-arc archeology

### 5.4 Worked end-to-end example

Codex session, no prior CLAUDE.md, wants to understand why a particular
line of code exists.

```
# turn 1: codex calls badgey_ask
> badgey_ask(question="why does src/inbox.rs:247 exist? show provenance",
>            scope={project_id:"3f7a91c4"})

# wrapper exec's a fresh instance, runs answer-mode, dismisses
< { bundle: {
<     narrative: "line 247 (fn aggregate_threads) added in commit:ed01724
<                 by claude-opus-4-7 in session:claude:xyz; that session
<                 IN_THREAD thread:abc123 (charter: ...);
<                 the function SUPERSEDES aggregate_stale_threads from
<                 commit:bca3003, DERIVED_FROM whiteboard:wb-stale ...",
<     entity_refs: ["commit:def:ed01724", "thread:abc123",
<                   "commit:def:bca3003", "whiteboard:wb-stale-ranking"],
<     paths: [{id:"bg-path-1", nodes:[...], edges:[...], summary:"..."}],
<     citations: [...],
<     follow_ups: [...]
<   }
< }

# turn 2: codex drills directly via bbox_*
> bbox_inspect_entity(ref="whiteboard:wb-stale-ranking", edge_types="DERIVED_FROM,REFERENCES")
< { ... whiteboard contents + linked entities ... }

# turn 3: codex re-engages badgey for synthesis with continuity
> badgey_exec(scope={project_id:"3f7a91c4"},
>             initial_brief="continuing the F3 supersession chain investigation")
< { badgey_id: "bg-3f7a91c4-91ff04cc", session_id: "..." }

# turn 4: codex passes its own context back via prompt
> badgey_resume(badgey_id="bg-3f7a91c4-91ff04cc",
>               prompt="i just inspected whiteboard:wb-stale-ranking and
>                       found <summary>; given that, propose a packet rule
>                       that would catch similar superseded-symbol cases")
< { reply: { narrative: "...", proposal: { id: "P-1", kind: "packet", ... } } }

# turn 5: apply
> badgey_resume(badgey_id="bg-3f7a91c4-91ff04cc", prompt="apply P-1")
< { applied: true, artifact: "packet:catch-superseded-symbols-v1",
<   bbox_decide_id: "...", thread_post: "..." }

> badgey_dismiss(badgey_id="bg-3f7a91c4-91ff04cc")
< { final_summary: "...", proposals_applied: 1 }
```

Note the path-id `bg-path-1` from turn 1's bundle was bundle-internal;
in turn 4 codex passes a free-text summary instead because the new
badgey instance is a different cosession with a fresh wrapper-side path
cache. Within ONE instance, "expand bg-path-1" works across resumes;
across instances, it does not.

## 6. Capabilities

### 6.1 Graph-native scout

The substrate. Already covered in §5.

### 6.2 Narrated provenance

`badgey_resume(id, "explain why src/inbox.rs:247 exists")`. Walks
`EDITED_BY_*` → transcript event → originating user turn → containing
thread → first commit chain, narrates with citations.

Variants:
- `explain decision <knowledge_id>` — supersession + derivation chains
- `explain thread <thread_id>` — what was decided, who participated
- `explain brofile <name>` — when created, by what arc, last edited

This is the demo. The cosession wedge: badgey already has the
entity-refs hot; "drill in" is one more turn.

### 6.3 Inbox triage with morning brief

`badgey_triage_inbox(scope, since=24h)`. Runs as a poller (cron, §10.3)
or on-demand:

1. read `bbox_inbox`, classify items
2. for meaty items, scout sub-bros (via §5.3 mechanism) with focused
   charters
3. aggregate sub-bro `done` notes into a proposal sheet:

```json
{
  "proposals": [
    {
      "id": "P-3",
      "kind": "re-dispatch",
      "what": "thread:abc-investigation idle 6d",
      "root_cause": "blocked on bro-7 task that errored silently 2026-04-22",
      "proposal": "re-dispatch with refined charter (draft attached)",
      "blast_radius": "single thread; no downstream dependencies",
      "draft_artifact_ref": "draft:re-dispatch-abc-7",
      "apply_via": "badgey_resume(id, 'apply P-3')",
      "state": "pending"
    }
  ],
  "meta_note_id": "note-xyz"
}
```

4. emit `bbox_note(kind=followup, body={event:"badgey-triage", ...})`
5. user taps `badgey_resume(id, "apply P-3")`

#### Sub-bro pattern

Sub-bro spawning is wrapper-mediated (§2.2). Badgey emits a
`bg-action-spawn-subbro` request as a `bbox_note(kind=followup,
body={event:"bg-action-spawn-subbro", action_id:"<uuid>",
scout_id:..., charter:..., expected_return:..., timeout_secs:600})`
on the scout thread. After the
turn, the wrapper:

1. validates the note's body shape against a JSON schema (charter
   length cap, timeout in valid range, expected_return is a
   recognized shape)
2. checks the per-scout sub-bro budget (3 parallel, 8 sequential)
3. dispatches `bro_exec(brofile="badgey-scout", allow_recursion=false,
   prompt=charter, project_dir=...)`
4. records the spawned task id in the scout thread, links the sub-bro
   thread back to the scout thread

Sub-bros use `badgey-scout` brofile (one-question, structured-return,
no further dispatch). The badgey-scout brofile callers also have
`mcp__blackbox__bro_*` denied via the standard tool-name filter; they
cannot spawn further bros.

Sub-bro `done` notes post on the sub-bro's own thread; the wrapper's
scout monitor polls these and aggregates into the scout thread.

#### Apply-proposal mechanics

`badgey_resume(id, "apply P-N")` is recognized by the wrapper's command
parser (§2.2) and handled directly without invoking the badgey bro.
State and CAS semantics live in BadgeyProposalStore (§8.3).

States:

```
pending → applying → applied
                  ↘ failed
```

Wrapper apply path:

1. read the proposal from BadgeyProposalStore. If not found →
   `error.not_found`. If `state=applied` → `{already_applied: true,
   prior_action: ...}`. If `state=applying` →
   `error.bad_input(code=already_in_progress)`. If `state=failed` →
   surface prior failure; require explicit "retry apply P-N".
2. take the per-proposal advisory file lock; re-read; transition
   `pending → applying` via fsync-write-rename. CAS by checking prior
   state under the lock.
3. invoke the kind-specific dispatch under the lock:
   - `kind=workflow|packet|brofile|lens` →
     `bbox_artifact_install(kind, source=draft_path)`
   - `kind=artifact-promotion` →
     `bbox_artifact_install` at new scope, then
     `bbox_artifact_supersede` on prior
   - `kind=re-dispatch` → `bro_exec(prompt=refined_charter,
     project_dir=..., allow_recursion=false)` using the proposal's
     pre-recorded `idempotency_key` so a duplicate fires no second
     bro
4. on success: transition `applying → applied`, record
   `applied_artifact_ref` / `applied_task_id`. Then write audit trail
   (`bbox_decide` citing proposal id, thread post). Audit failures
   leave state `applied` but trigger §11.6 audit-replay on next
   restart or retry.
5. on action failure: transition `applying → failed`; surface error.
6. release lock; return enriched result to caller.

Idempotency:
- `bbox_artifact_install` is naturally idempotent on `(kind, name,
  version)`.
- `bro_exec` is NOT naturally idempotent; the proposal's
  `idempotency_key` (a wrapper-minted UUID stored at proposal
  creation) is passed through ambient scope, and the wrapper records
  the dispatched task id in the proposal record before
  `applying → applied`. A retry that finds a recorded task id checks
  `bro_status` and treats a still-running or successful task as the
  prior dispatch.

### 6.4 Completion-contract closer

`badgey_close_loops(window=14d)`. Targets dispatched tasks where the
contract was never satisfied and nobody noticed.

1. find tasks with `completion_contract` set in window
2. for each: did `done` note land? does it satisfy contract semantically
   (badgey reads both)? did the agent crash, pivot, or trail off?
3. classify: `stalled` / `crashed` / `pivoted` / `forgot-emit-done`
4. propose resolution per case:
   - **stalled** → re-dispatch with refined charter
   - **crashed** → mark failed in proposal sheet, surface root cause
   - **pivoted** → ask whether to amend contract retroactively or treat
     as failed
   - **forgot-emit-done** → emit a structured `bbox_note(kind=learned,
     body={event:"closer-suspected-completion", task_id:..., contract:...,
     evidence_session:..., evidence_summary:..., synthesized_by:"badgey",
     does_not_replace_executor_done: true})`
5. all gated; apply via §6.3 state machine

#### Why `kind=learned` not `kind=done` for synthesis

Codex review finding: synthesizing a `done` note retroactively is
dangerous because `task_id` is the correlation key — a synthesized done
can mask a real failure. Badgey explicitly does NOT emit a `done` note
on behalf of the executor. Instead it emits `learned` with the structure
above. If the user accepts the closer's suspicion, the user issues an
explicit `done` themselves (via the proposal apply path), with the
learned-note as cited evidence.

This preserves "executor emits its own done" as an invariant while
letting the closer surface suspected-but-unmarked completions.

### 6.5 Producer-side recursion (bounded)

Five proposal types, all gated:
- `propose workflow` — pattern detection across N sessions
- `propose packet` — repeated user correction → packet rule
- `propose brofile lens` — when a brofile is missing a behavioral cue
- `propose artifact promotion` — scope changes
- `propose agent` — distill a new agent from a recurring task-shape
  cluster. Drafts manifest + brofile binding + evidence bundle per
  `agent-system.md` §8.2. Apply path installs via
  `bbox_artifact_install(kind="agent", source=draft)` and writes
  `DERIVED_FROM` provenance edges per `agent-system.md` §8.1.

Each lives as a thread post + draft artifact under
`$BLACKBOX_STATE_DIR/artifacts/_drafts/`. Apply mechanics same as §6.3.

#### Bounded learning loop

Badgey tracks accept/reject on its own proposals. When the threshold
fires (10 accept/reject decisions OR 30 days), badgey emits a `propose
brofile lens` artifact draft — NOT a passive `learned` note — that the
user must approve through the §6.3 apply path. Approved lens proposals
are installed via `bbox_artifact_install(kind="brofile", ...)` which
supersedes the prior version in the catalog. Next badgey instance
exec'd reads the new version naturally.

Codex review finding: passive learned-notes do not actually tune the
next badgey because they're side-channel, not lens-rendered. Drafting an
artifact proposal is the only durable channel.

Recursion bound mechanically:
- badgey can't apply its own proposals — user is the gate
- mechanical filter chain (§2.2) prevents self-dispatch loops
- eval arc (§12) regresses scores when accepted lens proposals degrade
  quality → drift visible
- self-audit thresholds visible in `bbox_thread_list --kind=work_item`
  filtered on `name LIKE 'badgey-tuning:%'`

## 7. Brofile lens

Concrete persona content. Three sections; the first two are durable
artifact (installed via `bbox_artifact_install`), the third is ambient
per-resume (composed by the wrapper at exec / resume time).

### 7.1 Persona (durable)

```
You are Badgey, the agentic-corpus consultant for this project.

The corpus you navigate is a typed entity graph. Entity types:
  knowledge, project_file, transcript, session, thread, note, symbol,
  brofile, whiteboard, commit (plus virtual: task, bash_call).
Edges include: SUPERSEDES, DERIVED_FROM, EDITED_BY_*, IN_SESSION,
  IN_THREAD, CONTRADICTS, REFERENCES, DESCRIBES, plus tool-call provenance.

Your tools are the bbox_* graph primitives. You CANNOT call bro_exec or
bro_resume — the daemon filter chain denies these for your brofile.

Sub-bro spawning is wrapper-mediated. To spawn a scout sub-bro, emit a
structured note. EVERY bg-action-* note MUST carry a fresh `action_id`
(UUIDv4) you mint yourself; the wrapper uses it for exactly-once
dispatch. Re-use the same action_id only when explicitly retrying.

  bbox_note(
    kind="followup",
    body={
      event: "bg-action-spawn-subbro",
      action_id: "<uuidv4 minted by you>",
      scout_id: "<scout_id>",
      charter: "<one focused question>",
      expected_return: "<schema description for the done-note>",
      timeout_secs: <int, max 1800>
    }
  )

The wrapper post-processes your turn: it sees these notes and
dispatches the sub-bro on your behalf. Same pattern for emitting
proposals (event: "bg-action-emit-proposal"), escalating disputes
(event: "bg-action-escalate-dispute"), and budget extension
(event: "bg-action-extend-budget"). All require action_id.

You serve three modes:
  - answer: caller wants the bundle, run the seed→inspect→traverse→bundle
    loop quietly, return narrative + entity_refs + paths + citations +
    follow_ups.
  - teach: caller wants to learn. Walk through the loop step-by-step on
    their actual corpus. Always include `next_time_skip_badgey_when`.
  - scout: caller wants async investigation. Emit scout sub-bro
    charter requests as bg-action-spawn-subbro notes. You do not block
    on sub-bros.

Sub-bro budget per scout: 3 parallel, 8 sequential. The wrapper
enforces this; over-budget requests return a rejection note you can
surface to the caller.
```

### 7.2 Lens (durable)

```
Operating constraints:

1. Cite every claim. Every sentence in your narrative must trace to a
   specific entity_ref. If you cannot cite, omit the claim.

2. Round-trip your citations BEFORE returning. For each citation, call
   bbox_inspect_entity on the target. If the target's edges/properties
   substantiate the claim shape, mark citation kind="exact" or
   "structural". If the expected edge or property is absent, mark
   kind="weak" and surface in degraded.weak_citations[]. Round-trip
   verifies entity existence and edge shape only — semantic claim
   correctness is YOUR judgment, not the verifier's.

3. Materialize paths in bundles. Return path nodes + edges + summary,
   not session-local IDs the caller cannot dereference. Use bg-path-N
   IDs only for follow-ups within the same instance.

4. Prefer typed search. Use bbox_hybrid_search with entity_types filter
   over bbox_search when the question is conceptual.

5. Sub-bro charters are scoped. One focused question, one expected
   return shape, one explicit timeout. The wrapper's scout dispatcher
   validates these before bro_exec.

6. Never apply destructive actions. Drafts go through bbox_artifact_install
   only after user types "apply P-N". Re-dispatches go through bro_exec
   only after user approval. Mark every state mutation with bbox_decide
   citing the proposal id.

7. Teach toward graduation. In teach mode, every walkthrough ends with
   `next_time_skip_badgey_when`: name the conditions under which the
   caller can use bbox_* directly without you.

8. Surface disagreements. If two sub-bros return contradictory done-notes,
   do not synthesize. Emit bbox_note(kind=dispute) with both notes, set
   degraded.dispute_pending, ask the caller to arbitrate.

9. Stay in budget. Each resume turn has a 50k-token soft budget; if
   approaching, return a partial bundle with degraded.budget_exhausted
   and ask before continuing.

10. Never emit kind=done for completion-contract synthesis. Use
    kind=learned with does_not_replace_executor_done=true. The user
    emits done themselves if they accept your synthesis.

11. On dismiss, write a final thread post: scouts_drained,
    proposals_applied, accept_rate_this_instance.
```

### 7.3 Scope-bind (ambient per resume)

Composed by the wrapper at exec / resume time:

```
[scope]
project_id: <hash>
project_root: <path>
session_id: <provider_session_id>
badgey_id: <id>
thread_of_record: <thread-id>
current_time: <ts>
budget_remaining: <tokens>
recent_proposals: <list of P-N with state, surfaced from BadgeyProposalStore>
recent_paths: <list of bg-path-N with summaries, surfaced from path-cache mirror>
```

The persona + lens live in the artifact catalog as `brofile:badgey`
with version tracking. They are themselves valid targets of
`propose brofile lens` (§6.5).

## 8. State

| State | Lifetime | Persistence path |
|---|---|---|
| Path cache (`bg-path-1`…) | instance | mirrored to thread-of-record post; rebuildable on cold start |
| Entity-ref bag | instance | mirrored to thread-of-record `refs_consumed[]` |
| Proposals (`P-3`…) | instance + global | BadgeyProposalStore (§8.3) at `$BLACKBOX_STATE_DIR/badgey/proposals/<instance_id>/<P-N>.json` |
| Scout threads | global | full `bbox_thread` persistence |
| Self-tuning learned-notes | per-scope | `bbox_remember(scope=project)` |
| Accept/reject log per proposal | per-scope | thread + structured note bodies |
| Live wrapper memory (resume queue, scout monitors) | wrapper process | rebuilt on daemon start from durable stores |

### 8.1 Thread-of-record post format

Every badgey instance has a backing thread `kind=work_item, name=badgey:<project_id_short>:<rand>`.
Posts are JSON-bodied notes. The note `kind` is one of the existing
seven (`learned`, `followup`, `done`, `blocked`, `dispute`, `surprise`,
`assumption`); the structured `event` field in the body is what badgey's
restart replay scans on:

| Event (in body) | Note kind | Body shape |
|---|---|---|
| `exec` | learned | `{event:"exec", brofile_version:..., scope:..., charter:..., provider:"codex", provider_session_id:"019df..."}` |
| `turn` | learned | `{event:"turn", turn_id:N, mode:..., caller:{provider,session_id}, question:..., bundle_summary:..., refs_consumed:[...], proposals_emitted:[...]}` |
| `path_cached` | learned | `{event:"path_cached", id:"bg-path-N", nodes:[...], edges:[...], summary:...}` |
| `scout_dispatched` | followup | `{event:"scout_dispatched", scout_id:..., scout_thread_id:..., charters:[...]}` |
| `subbro_spawned` | learned | `{event:"subbro_spawned", task_id:..., scout_id:..., charter:...}` |
| `proposal_emitted` | followup | `{event:"proposal_emitted", proposal_id:..., kind:..., draft_ref:..., state:"pending"}` |
| `proposal_applied` | done | `{event:"proposal_applied", proposal_id:..., artifact_ref:..., decide_id:...}` |
| `proposal_rejected` | learned | `{event:"proposal_rejected", proposal_id:..., reason:...}` |
| `dispute_escalated` | dispute | `{event:"dispute_escalated", subbro_results:[...]}` |
| `dismiss` | done | `{event:"dismiss", reason:..., summary:...}` |

Filtering: `bbox_notes(thread_id=..., kind=learned)` returns the stream;
the wrapper post-filters by `body.event` field. This avoids requiring a
schema change to threads or notes.

Compaction: every emission is mirrored to durable store BEFORE the
provider's next turn could compact context. For paths and refs, the
durable mirror is the thread-of-record note. For proposals, the durable
mirror is BadgeyProposalStore (§8.3). Live in-process state is
rebuilt-on-demand from these stores.

### 8.2 Restart replay

On daemon start:

1. wrapper calls `bbox_thread_list(kind="work_item", name="badgey:",
   include_resolved=false)`. The `name` arg is substring-match per the
   `bbox_thread_list` API; the wrapper post-filters with strict prefix
   `badgey:`. No idle-timeout filter (per agent-system §1.2 + this
   doc §2.4): unresolved threads with provider-resumable sessions are
   restored regardless of last_activity. Stale instances become
   unreachable when the substrate's TaskStore TTL or the provider's
   session GC evicts them, not when the wrapper sweeps.
2. for each candidate thread:
   a. read all notes via `bbox_notes(thread_id=...)`, filter by
      structured `body.event`
   b. extract `provider` and `provider_session_id` from the `exec`
      note's body (this is the badgey_id ↔ session bridge)
   c. rebuild entity-ref bag from `turn.refs_consumed[]` accumulator
   d. rebuild path cache from `path_cached` events
   e. rebuild proposal index from `proposal_emitted/applied/rejected`
      (latest event wins per proposal_id; cross-check against
      BadgeyProposalStore for proposals stuck in `applying`)
   f. rebuild `subbros_active[]` from `bg-action-spawn-subbro` events
      without matching done note on the sub-bro thread; check via
      `bro_status`
3. if `provider_session_id` is missing from the `exec` note (legacy
   instances pre-this-doc), mark the instance unrestorable and
   auto-dismiss with a `surprise` note documenting the gap
4. for each successfully restored instance: do not re-spawn a process;
   the instance is dormant until next caller resume. The wrapper holds
   the `badgey_id ↔ (provider, provider_session_id)` mapping in memory.
5. proposals stuck in `applying`: run §11.6 recovery procedure

Threads with `status=resolved` are not restored (already dismissed).
Threads with provider-evicted sessions surface
`degraded.provider_evicted=true` on attempted resume; the wrapper
emits a final `dismiss` event note when it confirms the underlying
provider session is unrecoverable.

### 8.3 BadgeyProposalStore

Proposals are NOT stored in the artifact catalog (artifacts are
already-installed things; proposals are drafts with state machines).
The wrapper owns a separate small store at
`$BLACKBOX_STATE_DIR/badgey/proposals/<instance_id>/<P-N>.json`
(see file layout below for the full directory shape).

```rust
// src/orchestration/badgey/proposals.rs
pub struct BadgeyProposal {
    pub id: String,                       // "P-3"
    pub instance_id: String,              // "bg-3f7a91c4-d2e810ab"
    pub scope_project_id: String,
    pub kind: ProposalKind,               // Workflow | Packet | Brofile | Lens | Agent | RedispatchTask | ArtifactPromotion
    pub state: ProposalState,             // Pending | Applying | Applied | Failed
    pub created_at: DateTime<Utc>,
    pub last_updated: DateTime<Utc>,

    // Draft payload — varies by kind:
    pub draft_artifact_path: Option<PathBuf>,  // for kinds that install artifacts
    pub draft_payload: Option<Value>,          // for re-dispatch (charter, project_dir, etc.)

    // Idempotency:
    pub idempotency_key: String,          // wrapper-minted UUID at create time

    // Apply outcome:
    pub applied_artifact_ref: Option<String>,
    pub applied_task_id: Option<String>,
    pub failure_reason: Option<String>,

    // Audit:
    pub history: Vec<ProposalEvent>,
}

pub enum ProposalState { Pending, Applying, Applied, Failed }

pub struct ProposalEvent {
    pub at: DateTime<Utc>,
    pub from: ProposalState,
    pub to: ProposalState,
    pub note: Option<String>,
}
```

File layout — proposals are nested under instance to avoid `P-N` id
collisions across instances:
```
$BLACKBOX_STATE_DIR/badgey/
  proposals/
    <instance_id>/         # e.g. bg-3f7a91c4-d2e810ab
      P-1.json
      P-2.json
      ...
  drafts/
    <draft_artifact_id>/
      artifact.json
      ...
  action_journal/
    <action_id>.json       # see §2.2 wrapper post-processing
    _archive/
      ...
```

Apply-resolution: `badgey_resume(badgey_id, "apply P-N")` resolves to
`proposals/<badgey_id>/P-N.json`. Display ids stay local
(human-friendly within an instance); on-disk paths are namespaced
globally.

CAS semantics:
- per-proposal advisory file lock (`flock` on `<id>.json` while
  reading-checking-writing)
- atomic write via tempfile + rename + fsync on the directory
- state transitions only valid on the documented edges; an attempt to
  transition `applied → applying` or `pending → applied` (skipping
  `applying`) is rejected at the store layer

Lifecycle:
- created in `pending` by wrapper on `bg-action-emit-proposal`
- transitioned by wrapper on user apply / reject / retry commands
- never deleted; expired proposals (>30 days `pending`) get a `surprise`
  note + are surfaced in `bbox_inbox` for explicit user disposition

Cross-restart: the store is on-disk; restart replay (§8.2) re-reads it.
Wrapper's in-memory proposal index is a cache; the store is canonical.

## 9. Concurrency

The badgey resume queue serializes turns against a single provider
session. Multiple `badgey_resume(id, …)` calls cannot execute
concurrently against the same instance.

### 9.1 Wrapper resume queue

Per-instance FIFO:

- `badgey_resume` enqueues; returns when bro process completes.
- `badgey_status` and `badgey_list` bypass the queue (read durable state).
- `badgey_dismiss` enqueues at head with high priority; queued resumes
  after dismiss return `instance_dismissed`.
- `badgey_collect` reads scout thread; never touches queue.
- Soft cap of 3 queued resumes; exceeding returns `error.bad_input`
  with `code=queue_full`.

### 9.2 Multi-caller addressability

Two callers (Codex and Claude) holding the same `badgey_id` is
expected. Resumes serialize. The thread-of-record's `turn` events carry
a `caller` field for post-hoc audit.

### 9.3 Scout — the off-queue path

`badgey_scout` deliberately splits to avoid blocking the resume queue:

- **Charter authoring** (queued turn): badgey decomposes the user
  charter into sub-bro charters. Brief turn; result is a list of
  charters written to the scout thread.
- **Sub-bro execution** (wrapper-owned background): the wrapper's scout
  dispatcher pulls authored charters from the scout thread, dispatches
  via `bro_exec(brofile="badgey-scout", allow_recursion=false)`, watches
  for done-notes on each sub-bro thread, aggregates into the scout
  thread. This loop runs in the wrapper, NOT in badgey's resume queue.
- **Synthesis** (queued turn, only on demand): user asks
  `badgey_resume(id, "synthesize scout ${scout_id}")`; that turn reads
  the scout thread and produces the bundle.

`badgey_collect` reads the scout thread without entering badgey's
queue. It returns either `{state: "still_walking", progress: ...}` or
`{state: "done", evidence: ...}`.

Scout charters that require >3 parallel sub-bros are refused at
dispatch (§7.1 budget rule, enforced by the wrapper's scout dispatcher).

## 10. Integration

### 10.1 Workflow composition

Badgey integrates via the `mcp_call` hook op (`src/workflow/ops.rs`).
`mcp_call` is a hook op (fires on `on_enter` / `on_exit`), not a
node-level action; the actual node body is a `prompt` driven by an
actor.

Pattern: a node uses `on_enter: mcp_call → badgey_ask` to populate a
workflow var, then the node's prompt references that var.

```json
{
  "name": "investigate-stale-thread",
  "version": 1,
  "actors": {
    "synthesizer": {
      "kind": "executor",
      "brofile": "executor"
    }
  },
  "start": "Probe",
  "nodes": {
    "Probe": {
      "actor": "synthesizer",
      "on_enter": [
        {
          "op": "mcp_call",
          "args": {
            "server": "blackbox",
            "tool": "badgey_ask",
            "arguments": {
              "question": "find every contradiction adjacent to thread:${vars.thread_id}",
              "scope": { "project_id": "${vars.project_id}" }
            },
            "timeout_secs": 600
          },
          "into_var": "evidence"
        }
      ],
      "prompt": "given evidence ${evidence.bundle.narrative}, propose next step",
      "next": { "type": "terminal" }
    }
  }
}
```

Templating: workflow vars are referenced as `${vars.<name>}`; node
outputs as `${<NodeName>.output}`; metadata as `${meta.project_dir}`.
Unresolved `${...}` references pass through unchanged (no
silent-fallback to top-level scope). The arc operator is responsible
for populating `vars.thread_id` and `vars.project_id` before dispatch
(or piping them in via `bro_orchestrate_run` arc params).

`mcp_call` retry policy: standard hook-op behavior. The arc operator
sets `retry` at node level to handle `degraded.budget_exhausted` from
badgey.

Workflows that benefit from badgey are workflows that need cross-edge
synthesis at one node; deterministic graph walks should use direct
`mcp_call` to `bbox_*` ops.

### 10.2 Schema discovery

`bbox_describe_schema` adds a `consultants` section:

```json
{
  "consultants": [
    {
      "name": "badgey",
      "tools": ["badgey_exec", "badgey_resume", "badgey_ask",
                "badgey_scout", "badgey_collect", "badgey_triage_inbox",
                "badgey_close_loops", "badgey_status", "badgey_list",
                "badgey_dismiss"],
      "use_cases": ["graph-native scout", "narrated provenance",
                    "inbox triage", "completion-contract closer",
                    "producer-side proposals"],
      "anti_patterns": ["single-edge questions", "freetext transcript search"],
      "example": "badgey_ask(question='why does X exist?', scope={project_id:'...'})"
    }
  ]
}
```

This is how outside callers discover badgey exists. Purely additive.

### 10.3 Cron + IaC layout

Badgey ships its IaC under `examples/badgey/`:

```
examples/badgey/
  brofiles/
    badgey.json              # main persona + lens (§7)
    badgey-scout.json        # sub-bro brofile (§6.3)
  crons/
    badgey-triage-daily.json # daily 06:00 local triage
    badgey-close-loops-weekly.json # weekly Sunday completion-contract sweep
  packets/
    badgey-self-eval.json    # self-eval gates (§12)
  workflows/
    badgey-eval-arc.json     # nightly eval arc
```

Install:

```
bbox_artifact_install(kind="brofile", source="examples/badgey/brofiles/badgey.json")
bbox_artifact_install(kind="brofile", source="examples/badgey/brofiles/badgey-scout.json")
bro_cron_install(spec_path="examples/badgey/crons/badgey-triage-daily.json")
```

None fire by default — opt-in.

## 11. Failure modes

### 11.1 Hallucinated narrative claims

Failure shape: badgey's narrative claims X, citation points at entity
Y, but Y does not actually substantiate X.

Mitigation:
- Round-trip validation (lens rule §7.2 #2) checks **entity existence
  and edge shape**. If the expected edge / property is missing,
  citation is downgraded to `weak`.
- Round-trip does NOT verify semantic correctness of the claim's content
  against the entity's content. That is LLM judgment.
- Quality of LLM judgment is the eval-arc's job (§12). Wrong-but-plausible
  narratives are caught only by:
  - structural mismatch (verified mechanically, surfaced as weak)
  - eval-arc gold-standard regression

The honest framing: badgey's prose is **entity-backed**, not
**semantically verified**. Callers consuming a bundle should treat
narrative claims as LLM-asserted with citation refs, and drill into
specific refs via `bbox_inspect_entity` if a claim is load-bearing.

Bundle return rules:
- 0 weak citations → bundle marked reliable
- 1-2 weak citations → bundle returned, suspect citations flagged
- 3+ weak citations → bundle returned with
  `degraded.unreliable_bundle=true`; narrative omitted, refs returned raw

### 11.2 Sub-bro disagreement

Failure shape: two sub-bros return contradictory done-notes.

Mitigation: lens rule §7.2 #8. Badgey emits `bbox_note(kind=dispute,
body={event:"dispute_escalated", subbro_results:[A,B]})` on the scout
thread. Returns to caller with `degraded.dispute_pending=true`. User
arbitrates via `badgey_resume(id, "trust A" | "trust B" | "tie-breaker")`.

### 11.3 Lens drift via accepted proposals

Failure shape: cumulative effect of accepted lens proposals degrades
quality.

Mitigation: §12 eval arc nightly run regresses against fixed
gold-standard query set. >5% regression triggers `bbox_inbox` alert and
auto-revert proposal: `badgey_resume(id, "revert to prior brofile
version")` re-installs the predecessor via `bbox_artifact_supersede`.
User gates the revert too.

### 11.4 Budget exhaustion mid-bundle

Failure shape: turn hits token budget partway through the loop.

Mitigation: lens rule §7.2 #9. Partial bundle returned with
`degraded.budget_exhausted=true`, narrative marks "(truncated at depth
N)". Caller decides: extend budget or take partial.

### 11.5 Daemon restart mid-scout

Failure shape: daemon restarts while a scout's sub-bros are in flight.

Mitigation: bro processes are ephemeral; sub-bros that completed wrote
done-notes to their threads (durable). On restart §8.2:
- scan `subbro_spawned` events without matching done-notes
- check each via `bro_status`; provider-side may have completed during
  daemon downtime
- for each truly missing, the wrapper offers re-dispatch (once) or
  marks failed (after retry)
- `badgey_collect` on a partially-recovered scout returns
  `degraded.scout_recovered_with_losses=true` with the missing sub-bro
  task ids

### 11.6 Apply-proposal stuck in `applying`

Failure shape: §6.3 step 3 (the dispatch) succeeded but step 4 (audit
writes) failed; proposal state in BadgeyProposalStore (§8.3) is
`applying`.

Recovery: on daemon start (or on retry-apply user request):
- check the proposal's underlying action:
  - `bbox_artifact_install` → was the artifact installed? (read catalog)
  - `bro_exec` → was the task spawned? (check by idempotency key)
- if action committed: replay audit writes idempotently
  (`bbox_decide` with the proposal id as supersedes-key prevents
  duplicates), transition to `applied`
- if action did not commit: transition to `pending`, emit a `surprise`
  note documenting the partial state, surface in `bbox_inbox`
- never silently transition `applying → applied` without verifying the
  underlying action

## 12. Eval surface

Badgey is graded against a fixed query suite with gold-standard
answers. Lives at `eval/badgey/`.

### 12.1 Query suite

Three categories, ~20 queries per category:
- **answer** — fixed corpus question + gold EvidenceBundle
- **teach** — question + gold walkthrough + required graduation cue
- **scout** — multi-hop charter + gold result-set

### 12.2 Pass criteria — structural first

Per query, in order; later checks only run if earlier pass:

1. **Required entity refs present** — bundle `entity_refs ⊇ gold_required_refs`
2. **Citation kind distribution** — no claim has `kind=weak`; required
   structural citations have `kind=structural` or `kind=exact`
3. **Path coverage** — for queries requiring multi-hop, paths include
   the gold edge sequences in expected direction
4. **Narrative shape (regex / keyword)** — required terms present,
   forbidden stale claims absent. THIS IS SECONDARY; passing only this
   without 1-3 is a fail
5. **Teach-specific** — walkthrough includes gold tool steps in order;
   `next_time_skip_badgey_when` field is present and substantive
6. **Scout-specific** — scout thread contains gold refs within timeout;
   no `degraded.dispute_pending` for non-disputed gold queries

Codex review finding: prior version inverted these by leading with
regex narrative checks. Structural checks first means
wrong-but-plausible narratives fail before plausibility scores them
acceptable.

### 12.3 Eval arc

`examples/badgey/workflows/badgey-eval-arc.json` runs nightly:

1. for each query, dispatch via `badgey_ask` against a throwaway
   instance (clean state)
2. compare bundle to gold per §12.2
3. emit per-query `pass | fail` + reason
4. aggregate; baseline tracked in thread `badgey-eval-baseline`
5. on regression > 5% from baseline: alert + suggest revert

### 12.4 Teach-mode graduation eval

Separate, run weekly (more expensive):

1. take a teach-mode walkthrough
2. dispatch a clean codex bro with the walkthrough's tool steps as
   prompts; do NOT include badgey
3. check whether the resulting `bbox_*`-only bundle matches the gold
   answer-mode bundle for the same question

If badgey's teach mode does not graduate callers, badgey's teach mode
is failing its premise (§1.1 #1). The eval-arc surfaces this directly.

### 12.5 Baseline calibration

Gold-standard bundles hand-authored on the dogfood corpus before ship.
Eval gates start permissive (50% pass rate) and tighten as the suite
stabilizes.

## 13. Observability + cost

### 13.1 Metrics

Per instance:
- turns served (per mode), tool calls (per `bbox_*` and `bro_*`)
- sub-bros spawned, alive, dead, timed out
- proposals emitted, applied, rejected
- weak-citation count, dispute escalations
- tokens consumed (input + output), elapsed wall time

Per scope:
- instances exec'd / dismissed / restored
- monthly token spend
- accept rate on proposals
- eval-arc pass rate trend

### 13.2 Cost controls

Badgey is meaningfully more expensive than direct `bbox_*`. Each
`badgey_ask` is a full LLM round trip; each scout sub-bro is another.

- **Per-turn soft budget** (default 50k tokens). On approach, partial
  bundle with `degraded.budget_exhausted`.
- **Per-instance soft budget** (default 500k tokens). On approach,
  dismissal warning.
- **Per-scope monthly budget** (advisory in v1, surfaced in `bro_dashboard`).

Tool descriptions push callers to direct `bbox_*` for cheap cases.
Teach mode's `next_time_skip_badgey_when` is the same idea inside
conversations.

### 13.3 Surfaces

- `badgey_status(id)`, `badgey_list(scope)`
- `bro_dashboard` — aggregate badgey activity
- `bbox_inbox` — triage notes, stale drafts, eval-arc regression alerts

## 14. Boundaries + non-goals

What badgey is NOT:
- a new daemon or index
- a destructive actor (drafts only; user gates apply)
- a wrapper around `bbox_*` (graph primitives stay accessible directly)
- a slash skill (MCP-exposed for cross-provider parity)
- a workflow (workflows can call `badgey_ask`; badgey doesn't replace
  workflow engines)
- a replacement for the `cosession` skill (different problems; both
  coexist)
- an LLM in the bbox synchronous read path (agentic-corpus §4.3
  invariant; badgey is reached only through `badgey_*`)
- privileged (visibility filtering at instance-construction; v1 is
  private-only)
- **the agent system.** Badgey is a single consultant-flavored
  agent (`agent:badgey@v1`) registered in the broader
  `agent-system.md` registry. `bro_agent_*` is the cross-provider
  surface for any agent; `badgey_*` is the consultant-flavored
  superset for badgey-specific orchestration (proposals, scout,
  triage, closer). Most agents will never need that superset.

Non-goals:
- auto-applying proposals
- replacing direct `bbox_*` use (descriptions actively push callers off)
- cross-project state (per-project scope; transfer via
  `propose-artifact-promotion`)
- persistent context across dismiss
- multi-tenant security
- replacing human review of producer-side artifacts
- real-time multi-user collaboration on one instance

## 15. Open design questions

1. **Daemon internal spawn API with caller-provided task ids.** §2.2's
   action-journal recovery contract requires the wrapper to pre-mint
   the sub-bro task_id and write it to the journal BEFORE the spawn
   call returns. The MCP `bro_exec` surface mints task_ids
   daemon-side; the wrapper instead uses an internal Rust API on the
   bro registry. Confirm a pre-allocate-task-id internal API is
   feasible to expose, or fall back to a less-strict idempotency
   check (e.g. dispatch_charter_hash) at the cost of weaker
   exactly-once guarantees.
2. **Wrapper post-processing latency.** §2.2 wrapper-mediation pattern
   adds a per-turn post-process step (read notes, dispatch actions).
   Is the latency acceptable, or should the wrapper switch to a
   streaming model that intercepts `bbox_note` writes inline? Bias:
   batch post-process is fine for v1; revisit if turn latency is
   user-noticeable.
2a. **Public-scope visibility.** v1 is private-only. v2 question:
   data-row filtering at `bbox_*` tool layer (knowledge / notes /
   transcripts have no row-level visibility today). Out of v1.
3. **Lens-flavored variants.** RESOLVED post-agent-system:
   architect-badgey / security-badgey / perf-badgey are *separate
   agents* (each with its own `agent:` artifact + manifest +
   brofile_ref) sharing or differing on the underlying brofile.
   Not multi-lens-on-one-badgey. v1 ships one badgey agent;
   variants land as additional installs.
4. **Idle eviction.** RESOLVED post-agent-system: no badgey-side
   eviction. Substrate TTL (TaskStore 24h since last activity +
   provider session GC) is the only timeout. Per
   `agent-system.md` §1.2.
5. **Self-audit cadence.** Triggered by 10 accept/reject decisions OR
   30 days, whichever first.
6. **Eval gates on accepted proposals.** Workflow + packet proposals
   route through nightly eval arc; accepted lens / brofile changes
   surface in eval but don't gate at install time. Confirm.
7. **Sub-bro budget.** 3 parallel, 8 sequential per scout, enforced
   by the wrapper's scout dispatcher (NOT the brofile prompt).
8. **Compaction interception reliability.** §8.1 says emissions are
   mirrored to durable store before provider compaction. Is this
   reliable across providers (Claude / Codex / Gemini all compact at
   different points)? Bias: rely on bbox-side write happening before
   the next turn boundary; use turn-boundary as the compaction-safe
   point.
9. **Multi-caller priority.** FIFO at wrapper queue; no priority hints
   in v1.
10. **Self-eval ownership.** Same maintainer as agentic-corpus 30-query
    suite; co-located.
11. **Wrapper command parser strictness.** §2.2 lists wrapper-direct
    commands ("apply P-N", "dismiss", etc.). Prefix-strict matching
    risks the user typing a near-match that gets routed to badgey
    when they meant the mechanical command. Bias: strict prefix +
    explicit help on first ambiguous match; revisit if users hit
    this in practice.

## 16. Glossary

- **Badgey instance** — `(badgey_id, scope, thread_of_record_id,
  provider_session_id)` tuple owned by the wrapper. Logical, not
  process-resident.
- **Badgey-scout** — separate brofile used by sub-bros. Mechanically
  forbidden from calling `bro_exec` via filter chain.
- **EvidenceBundle** — narrative + entity_refs + paths + citations +
  follow_ups + degraded.
- **Mode** — answer / teach / scout.
- **Proposal** — drafted artifact (workflow / packet / brofile / lens
  / re-dispatch / artifact-promotion) with state machine
  (pending/applying/applied/failed) durable in BadgeyProposalStore
  (§8.3). Applied via `badgey_resume(id, "apply P-N")`.
- **Cosession** (in this doc) — exec/resume property of a badgey
  instance; state compounds across turns. Distinct from the
  `cosession` *skill*.
- **Scope** — `{project_id, initial_brief?}`. v1 is private-only.
- **Scout** — async investigation. Charter authoring is queued; sub-bro
  dispatch + monitoring is wrapper-owned off-queue.
- **Sub-bro** — bro spawned by badgey under `badgey-scout` brofile with
  `allow_recursion=false`. One question, one done-note, no
  recursion.
- **Self-tuning** — accept/reject log feeds a `propose brofile lens`
  artifact draft; goes through user gate.
- **Thread-of-record** — `kind=work_item, name=badgey:<...>` thread
  that mirrors a badgey instance's durable state.
- **Weak citation** — `kind=weak`: entity exists but expected edge or
  property is absent. Surfaced via `degraded.weak_citations[]`.
