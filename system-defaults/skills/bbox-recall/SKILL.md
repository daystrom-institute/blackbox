---
name: bbox-recall
description: Retrieve and verify Blackbox evidence for prior decisions, rules, earlier sessions or indexed code. Use when a question is about what was decided, why, when, by whom or what replaced it, when the user refers to an earlier session or "like last time", before asking the user something that may already be recorded, and before stating a project convention you have not verified this session.
---

# Answer from stored evidence

Blackbox holds things your context does not: rules and decisions the operator approved, recorded agent sessions, and indexed code, docs and commits across registered projects. An answer rebuilt from memory or a quick grep misses the decision made three sessions ago, and the user then has to repeat it.

This is a retrieval workflow for questions that depend on stored context, not an opening ceremony for every task. Skip it when the answer is already authoritative in front of you: a file you just read, a live command result, a session id you already hold.

## 1. Choose the reader the question needs

| The question is about | Start with |
|---|---|
| A rule, convention, decision or runbook | `bbox_knowledge(query="<short phrase>")` |
| An ongoing line of work | `bbox_thread_list(project="<project>")` |
| Code, docs, commits or past conversations | `bbox_hybrid_search(query="<short phrase>", project="<working checkout>", limit=5)` |
| A session you can already identify | The readers in [transcript-retrieval.md](transcript-retrieval.md) |

Use a short specific phrase from the task, not one generic keyword.

`bbox_knowledge` returns approved entries, cold recall entries and runbook signposts. It does not search the corpus, so a miss there says nothing about whether the answer exists: follow it with `bbox_hybrid_search` before concluding nothing is recorded.

## 2. Search with intent

- Adjacent terms broaden recall, `"quoted phrases"` stay exact, and `-term` excludes. Use `mode="fulltext"` only for deliberate raw boolean syntax.
- `doc_type` restricts to one kind: `transcript`, `project_file`, `commit`, `knowledge`, `thread`.
- `project` scopes project files and project graph vertices. Commits and knowledge pass through unfiltered, and so do transcripts unless the search is narrowed to conversations (by `doc_type="transcript"` or a conversation filter). Do not read a mixed result as belonging to one project.
- Results collapse to one hit per file, the best-scoring chunk.
- A relevant hit may share none of your wording. Ranking is a lead to inspect, not proof of the answer.
- After two or three failed rewordings, stop rewording: remove filters or change `doc_type`.

## 3. Read before relying

Inspect a returned ref with `bbox_inspect_entity(entity_ref="<ref as returned>")`. Previews are shortened; for exact wording choose a returned property key and read `property="<key>"`, following `body.next_cursor` as `property_cursor`. `property_mode="full"` expands stored values but cannot restore content lost at ingestion.

Use refs exactly as tools return them. Never build a ref from a guessed id, and when an error offers a `suggested_fix`, use it verbatim.

For conversations, attribution or exact earlier turns, read [transcript-retrieval.md](transcript-retrieval.md) before expanding a hit. A knowledge preview does not establish who said something or when.

## 4. Ground the answer

| Question | Cite |
|---|---|
| Where is X? | The `project_file` hit and its file path |
| What does X do, or what is the rule on X? | The `project_file` chunk you read, or the `knowledge` entry itself |
| Who or when? | Commit evidence (`doc_type="commit"`) or the conversation where it happened |
| Why? | The originating evidence. A current rule is not its own explanation. |
| What replaced X? | Both endpoints and the evidence linking them, with the direction stated |
| How did we get here? | A ref you read this turn for every step. Do not rebuild a chain from memory. |

Keep current rules, historical statements and your own inference separate in the answer. Say when evidence is stale, missing, degraded or truncated. Deleted or withdrawn material is history: it is not authority for what the operator wants now, and you should not attribute intent from it.

An empty result is a finding about the index, not proof that something never happened. Say what you searched. If results look wrong for content you know exists, check health (the `bbox-health` skill) before concluding.

Stored history describes the past. For the current state of code, read the owning checkout's file and cite that. Stop once the question is grounded; retrieval is not a reason to start unrequested maintenance.
