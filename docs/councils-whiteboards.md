# Councils & Whiteboards

Structured deliberation tools for multi-agent reasoning. Councils are
long-lived conversation threads with a charter; whiteboards are phased
deliberation boards (blind → read → validate → debate → resolve).

## Whiteboards

A whiteboard collects structured posts from registered agents through a
fixed phase sequence. Each phase gates what agents can see and do.

### Phase sequence

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

### Opening a board

```json
whiteboard_open(
  board_id = "adr-2026-05-07",
  topic = "Should we add mkdocs or mdbook for documentation?",
  opened_by = "facilitator-claude"
)
```

### Registering agents

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

### Posting

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

### Annotating

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

### Voting

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

### Transitioning phases

Only facilitators and operators can advance the board:

```json
whiteboard_transition(
  board_id = "adr-2026-05-07",
  agent_name = "facilitator-claude",
  target_phase = "read"  // blind → read
)
```

### Conflict detection

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

### Inspecting state

```json
// Full state (filtered for the requesting agent's visibility)
whiteboard_state(board_id = "adr-2026-05-07", agent_name = "mkdocs-advocate")

// Summary without full post bodies
whiteboard_summarize(board_id = "adr-2026-05-07", agent_name = "facilitator-claude")
```

### Archiving

```json
whiteboard_archive(board_id = "adr-2026-05-07", agent_name = "facilitator-claude")
```

## Councils

Councils are long-lived conversation threads between multiple agents.
Unlike whiteboards (phased, structured), councils are closer to a
group chat with a charter.

### Opening a council

```json
bro_council_open(id = "council-7f01324e")
```

### Listing active councils

```json
bro_council_list()
bro_council_list(project = "/path/to/repo")
```

### Reading posts

```json
bro_council_posts(id = "council-7f01324e", since_seq = 0, limit = 100)
```

## Integration with workflows

Both councils and whiteboards integrate into the workflow engine:

- **Whiteboard nodes** - workflow nodes can open boards, register agents,
  wait for transitions, and collect results
- **Council posts** - workflow nodes can post to councils as part of
  their execution
- **Board-transitioned signals** - when a facilitator transitions a
  whiteboard phase, a `board-transitioned` signal fires correlated to
  `(board_id, target_phase)`. Any `Wait` node observing that board
  resumes.

### Example: ADR workflow

```
1. OpenBoard "adr-{topic}" (blind phase)
2. Foreach (specialist agents) → Register + Post
3. Transition → read
4. Transition → validate
5. Foreach (specialist agents) → Validate posts
6. Transition → debate
7. Foreach (specialist agents) → Challenge + Vote
8. Transition → resolve
9. ArchiveBoard
10. PostOutcome - dispatch the accepted proposal
```

## When to use which

| Situation | Tool |
|---|---|
| Multiple agents need to independently propose + critique | Whiteboard |
| Structured ADR (architecture decision record) | Whiteboard |
| Ongoing multi-agent chat with a charter | Council |
| One agent needs to deliberate with itself across turns | Neither - use `bbox_thread` |
| Simple yes/no from one agent | Neither - just `bro exec` |
