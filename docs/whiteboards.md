# Whiteboards

Structured deliberation tool for multi-agent reasoning. A whiteboard collects
structured posts from registered agents through a fixed phase sequence. Each
phase gates what agents can see and do.

## Phase sequence

```
blind ──► read ──► validate ──► debate ──► resolve ──► archived
```

| Phase | What agents can do | Visibility |
|---|---|---|
| **blind** | Post proposals, claims, concerns | Each agent sees only their own posts |
| **read** | Read all posts | All posts visible, no annotations yet |
| **validate** | Post validations (confirmed/refuted/inconclusive) | All posts + validations visible |
| **debate** | Post challenges, corroborations; vote (accept/reject/defer) | Full board visible |
| **resolve** | Post resolutions; archive the board | Full board visible |
| **archived** | Read-only | Board moved to archive |

## Opening a board

```json
whiteboard_open(
  board_id = "adr-2026-05-07",
  topic = "Should we add mkdocs or mdbook for documentation?",
  opened_by = "facilitator-claude"
)
```

## Registering agents

Agents must register before they can post, annotate, or vote:

```json
whiteboard_register(
  board_id = "adr-2026-05-07",
  agent_name = "pr-reviewer@v1.0.0",
  role = "specialist",
  domain = "documentation tooling"
)
```

Roles:

| Role | Powers |
|---|---|
| `specialist` | Post + annotate + vote |
| `facilitator` | Transition phases + post + annotate + vote |
| `operator` | Same as facilitator (convention: human / external joiner) |

## Posting

During the blind phase, each agent posts independently:

```json
whiteboard_post(
  board_id = "adr-2026-05-07",
  agent_name = "mkdocs-advocate",
  type = "proposal",
  title = "Adopt mkdocs-material",
  body = "mkdocs-material gives us search, nav, dark mode, and code highlighting out of the box. All existing docs are already in Markdown."
)
```

Post types: `proposal`, `claim`, `concern`, `informational`.

Optional structured fields enable conflict detection downstream:

| Field | Purpose |
|---|---|
| `target_file` | File this post targets |
| `target_location` | Line range or symbol |
| `severity` | `critical` / `high` / `medium` / `low` |
| `finding_refs` | References to shared findings |
| `cascade_targets` | Files that would be impacted |

## Annotating

```json
// Validation (validate phase only)
whiteboard_annotate(
  board_id = "adr-2026-05-07",
  agent_name = "fact-checker",
  post_id = "post-1",
  type = "validation",
  body = "Confirming mkdocs-material supports all listed features",
  result = "confirmed"
)

// Challenge (debate phase only)
whiteboard_annotate(
  board_id = "adr-2026-05-07",
  agent_name = "mdbook-advocate",
  post_id = "post-1",
  type = "challenge",
  body = "mdbook also supports all of those and has native Rust doc integration"
)
```

## Voting

During the debate phase, each agent casts one vote per post:

```json
whiteboard_vote(
  board_id = "adr-2026-05-07",
  agent_name = "facilitator-claude",
  post_id = "post-1",
  vote = "accept",
  reason = "Strongest fit for our existing Markdown docs"
)
```

Votes: `accept`, `reject`, `defer`.

## Transitioning phases

Only facilitators and operators can advance the board:

```json
whiteboard_transition(
  board_id = "adr-2026-05-07",
  agent_name = "facilitator-claude",
  target_phase = "read"  // blind → read
)
```

## Conflict detection

```json
whiteboard_conflicts(
  board_id = "adr-2026-05-07",
  agent_name = "facilitator-claude"
)
```

Returns three kinds:
- `direct_overlap` - same target_file + same target_location
- `cascade_collision` - post A cascades to post B's direct target
- `severity_disagreement` - same finding_ref, distinct severities

## Inspecting state

```json
// Full state (filtered for the requesting agent's visibility)
whiteboard_state(board_id = "adr-2026-05-07", agent_name = "mkdocs-advocate")

// Summary without full post bodies
whiteboard_summarize(board_id = "adr-2026-05-07", agent_name = "facilitator-claude")
```

## Archiving

```json
whiteboard_archive(board_id = "adr-2026-05-07", agent_name = "facilitator-claude")
```

Archiving is a phase transition in effect (`resolve -> archived`), so it
requires a facilitator or operator role on every path, exactly like
`whiteboard_transition`; a registered specialist cannot archive a board.
Normal archives are legal only from the `resolve` phase. `force = true`
archives from ANY phase - the abandon path for boards stranded mid-phase
when their arc fails (e.g. `on_arc_exit` cleanup hooks). Force only relaxes
the phase precondition, never the role check, and the archived board's
phase history records the phase it was abandoned from.

```json
whiteboard_archive(board_id = "adr-2026-05-07", agent_name = "facilitator-claude", force = true)
```

## Integration with workflows

Whiteboards integrate into the workflow engine:

- **Whiteboard nodes** - workflow nodes can open boards, register agents,
  wait for transitions, and collect results
- **Board-transitioned signals** - when a facilitator transitions a
  whiteboard phase, a `board-transitioned` signal fires correlated to
  `(board_id, target_phase)`. Any `Wait` node observing that board
  resumes.
- **Engine-driven auto-apply (`board` node binding)** - an ensemble node
  with `"board": "${vars.board_id}"` has each member's STRICT-JSON
  output parsed into typed board actions and applied by the engine, so a
  member that writes the deliberation but forgets the tool call still
  lands on the board. Output is one object or an array (code fences
  tolerated) of `{"agent_name"?, "action": "post"|"annotate"|"vote"|"none", …}`
  items whose fields match the `whiteboard_*` tool surface exactly.
  Attribution falls back to the ensemble member name when `agent_name`
  is absent. Real agents drift from the STRICT contract (prose
  preambles, provider tool-call echoes), so a salvage pass also tries
  every fenced block and the outermost bracket spans - candidates must
  match the action schema (the tagged `action` field), so unrelated
  JSON in the output cannot false-positive into board mutations.
  Every action passes the same phase/role/reference checks
  the tools enforce; failures are logged as `board_autoapply_*` arc
  events and never fail the node. Agents keep read access via
  `whiteboard_state`; mixing modes on one node (tool-written AND
  auto-applied) risks double-posting, so a `board`-bound node's prompt
  should say "do not call whiteboard mutation tools".

### Example: multi-round ADR workflow

Real deliberation is multi-round: a challenge deserves a response, a
response deserves re-examination, and votes should be informed by the
exchange rather than cast alongside it. A single "challenge + vote"
dispatch produces one-shot debate theatre - nobody ever sees the
challenges against their own posts. The shape that works:

```
 1. OpenBoard "adr-{topic}" (blind phase); Register agents
 2. BlindPost (ensemble)          → each specialist posts independently
 3. Transition → read → validate
 4. Validate (ensemble)           → evidence round: each specialist digs for
                                    concrete evidence on PEER posts, annotates
                                    validation confirmed/refuted/inconclusive
 5. Transition → debate
 6. DebateChallenge (ensemble)    → challenges + corroborations on peer posts;
                                    NO votes yet
 7. DebateRespond (ensemble)      → each specialist answers challenges against
                                    ITS OWN posts: concede (resolve), or rebut
                                    with new evidence; challengers withdraw
                                    (resolve) or leave challenges standing
 8. CheckDebate (gate)            → whiteboard_summarize → packet gate on
                                    unresolved_challenges + round counter:
                                    loop to 7 while challenges stand and
                                    rounds < N, else advance
 9. Vote (ensemble)               → votes informed by the full exchange;
                                    changing your mind under evidence is
                                    legitimate
10. Transition → resolve; Synthesize (facilitator) → ADR; ArchiveBoard
```

The loop machinery is stock: durable ensemble actors resume the same
specialist sessions each round, a hook-only gate node reads
`whiteboard_summarize` (`unresolved_challenges`) into a var, a packet
routes `another_round`/`settled` with a round-counter ceiling, and a
branch back-edge revisits the respond node. Surviving disagreement past
the ceiling is signal, not failure - unresolved challenges flow into
the synthesis for human judgment. Runnable version:
`examples/whiteboard/` (workflow `whiteboard-arc.json`, gate packet
`gate-debate-settled.json`).

## When to use whiteboards

| Situation | Tool |
|---|---|
| Multiple agents need to independently propose + critique | Whiteboard |
| Structured ADR (architecture decision record) | Whiteboard |
| One agent needs to deliberate with itself across turns | Neither - use `bbox_thread` |
| Simple yes/no from one agent | Neither - just `bro exec` |
