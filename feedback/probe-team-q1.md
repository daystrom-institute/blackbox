# Probe-team validation — cold-start grounding Q1

Captured 2026-05-06. First broadcast question to the `cold-start-probe`
team (claude-opus, codex-gpt55, glm-51, deepseek-v4-pro). Question shape:
WHERE — "Where is the `project=` filter for `bbox_hybrid_search`
implemented? Find the file/lines, the commit SHA, the producing session.
End with a methodology section."

## Per-provider results

### claude-opus — ⭐⭐⭐⭐⭐ (8m, $0.89, 9 turns)

Full agentic loop. Tools called in order:
1. `bbox_hybrid_search(query="bbox_hybrid_search project filter parameter")` — found `src/mcp_tools/hybrid_search.rs` rank-3 with the field excerpt
2. `Read` on the file — confirmed exact line numbers (43-52, 235-237, 344-379)
3. `bbox_blame(file=..., line=52)` — got commit `10a3e1a` + bbox anchor session `2ba56297` + prior-read provenance
4. `bbox_inspect_entity(commit:...:ee46669)` — disambiguated a similar-titled FALSE POSITIVE commit (cross-project pollution: ee46669 was a daystrom-mk2 commit, message-similarity collision)
5. `bbox_note` ×3 — recorded surprise (commit collision), learned (prior_reads provenance), done (summary)

Answer was complete and correct. Particularly notable: claude PROACTIVELY
detected the cross-repo commit collision and ruled it out via inspect_entity.
Used per-file collapse + project filter implicitly through the search
results.

### codex-gpt55 — ⭐⭐⭐⭐ (12m+, ~$ similar, ~25 turns)

Reached the same correct answer but verbose/slow. Key tools:
- `bbox_hybrid_search` then `bbox_blame` then transcript search
- `bbox_note(kind="surprise")` recording: bbox_blame's anchor metadata
  pointed at `src/mcp_tools/bundle_evidence.rs` while the actual blame
  target was `hybrid_search.rs` — caught a real bbox_blame quirk we should
  follow up on
- `bbox_note(kind="done")` summary

12 minute runtime is too long for interactive use. Codex tends to over-
explore — pulled the surrounding transcript turn for "the producing
conversation cleanly" even when the session ID was already established.

### glm-51 — ⭐ (5.5m, ~$0)

Used `grep` (not bbox_*) for the initial scan. Found the right code via
text search alone. Then died with `database is locked` — opencode SQLite
contention with the parallel deepseek bro using the same DB.

### deepseek-v4-pro — ⭐ (5.4m, ~$0)

Same pattern as glm: used `Read` (not bbox_*), then SQLite lock failure.

## Failure modes identified

1. **Opencode bros (glm, deepseek) don't reach for bbox MCP tools by
   default.** They have access — `~/.config/opencode/opencode.json`
   correctly configures the blackbox MCP. But their behavioral default
   under provider-native tools is `grep`/`read`. The rendered guidance in
   `~/.claude-shared/BLACKBOX.md` reaches them only if `~/.codex/AGENTS.md`-
   style includes are honored by opencode. **Investigation needed**:
   does opencode's instruction loading chase `@/path/...` includes? If
   not, we need to either (a) write the rendered tool reference directly
   to opencode's expected location, or (b) accept opencode bros as a
   weak-grounding path until a separate integration arc lands.

2. **Opencode SQLite lock contention.** Two opencode-based bros running
   in parallel = "database is locked" error. The opencode session DB
   isn't safe under concurrent writers from the same install. Mitigation:
   stagger glm and deepseek dispatches sequentially.

3. **Codex over-explores conceptual queries.** 12+ minutes for a single
   WHERE question. Quality is high but latency is interactive-unfriendly.
   Mitigation: tighten codex-gpt55 brofile lens with explicit "stop when
   you have the answer; do not pull surrounding context unless missing."

## Strengths confirmed

- Both successful agents (claude, codex) **followed the 5-step opening
  sequence** without needing prompts to do so. Discovery → inspect → blame
  → note. The rendered CORE RULE in BLACKBOX.md is moving behavior.
- Cross-project pollution that was a known quality gap PRE-fix surfaced
  as a real-world hit (similar-titled commit from daystrom-mk2) which
  claude correctly identified and rejected. The agentic graph (inspect_
  entity revealing COMMIT_TOUCHED_FILE in another project) made the
  disambiguation possible.
- `bbox_note` was used naturally by both successful agents for sparse
  high-signal records (`done`, `surprise`, `learned`). The note taxonomy
  is propagating.
- One real bug surfaced: bbox_blame's `bbox_anchor.metadata.anchor.file_
  path` may not match the actual blame target file when the anchor came
  from a multi-file edit session. Worth a follow-up but minor (the
  correct commit SHA + session were both still correct, just one anchor
  metadata field is off).

## Recommendations for follow-up probes (Q2+)

- Stagger opencode-based bros (glm THEN deepseek, not parallel)
- Tighten codex brofile lens to discourage exploratory turn-pulls
- Run the remaining question types (WHO/WHEN, WHY, HOW, REPLACEMENT,
  HISTORICAL, IMPACT) once heavy embed load subsides
- Consider authoring an explicit "scout" brofile that wraps the agentic
  loop into one structured-return call (per `thread-f4e4624f`)

## bbox_blame anchor-metadata bug (new issue surfaced)

Codex's surprise note flagged: when calling `bbox_blame(file=hybrid_search.rs,
line=52)`, the response correctly identified commit `10a3e1a` as the
producing commit, but `bbox_anchor.metadata.anchor.file_path` came back as
`src/mcp_tools/bundle_evidence.rs` — a DIFFERENT file from the one being
blamed. Likely cause: the matched anchor was emitted during a multi-file
session where multiple Edit tool calls landed in the same commit; the
anchor lookup returned the FIRST anchor matching commit_sha rather than
the one matching the queried file_path. Fix: filter anchors by file_path
when multiple share the same commit_sha. Track in deferred-items thread.
