---
title: "Vibe · Built-in Tools"
kind: research-finding
corpus: blackbox-research
track: harness
harness: vibe
axis: builtin-tools
version: "2.9.6"
last_verified: "2.9.6"
status: enriched
confidence: high
topic:
  - harness
  - vibe
  - builtin-tools
brief: "Vibe's 12 built-in tools (bash/read_file/write_file/search_replace/grep/webfetch/websearch/todo/ask_user_question/task/exit_plan_mode/skill); strong bash steering doc (DO-NOT-USE cat/grep/sed tables); fuzzy SEARCH/REPLACE (0.9 threshold) with debugging-hint errors; ask_user_question elicitation; no dedicated glob (bash covers it)."
---

# Vibe · Built-in Tools

> Mined from open source (GLM-5.1 bro, 2026-06-02). **confidence: high.** See axis: [Built-in Tools](../builtin-tools.md) · snapshot: [Vibe 2.9.6](vibe-2.9.6.md).

**Finding.** 12 tools in `vibe/core/tools/builtins/`: `bash`, `read_file` (64KB/call cap, safe-decode), `write_file`, `search_replace` (SEARCH/REPLACE blocks, **fuzzy match @0.9** via `difflib`, errors carry "Debugging tips" + closest-match), `grep` (ripgrep), `webfetch`, `websearch`, `todo` (full-replace, max 100), `ask_user_question` (structured questions + footer), `task` (subagent delegate), `exit_plan_mode`, `skill`. **Steering language:** `prompts/bash.md` has explicit "DO NOT USE" tables steering off `cat`/`grep`/`sed`/`head`/`tail`/`find` toward dedicated tools, + an "APPROPRIATE bash uses" list. `read_file` results inject per-dir AGENTS.md.

**Evidence.**
- `vibe/core/tools/builtins/bash.py:264` + `prompts/bash.md` — description + DO-NOT-USE steering tables
- `vibe/core/tools/builtins/search_replace.py:101` — fuzzy SEARCH/REPLACE block regex
- `vibe/core/tools/builtins/task.py:50` — subagent-only delegate guard

**Vs the axis.** Confirms the steering-language idiom AND the agent-authored elicitation extension (`ask_user_question`). **Idiom:** fuzzy search/replace with diagnostic errors is a robustness affordance for the edit surface.

## Open
<!-- Full verbatim tooldoc set; the structured-question schema for ask_user_question. -->
