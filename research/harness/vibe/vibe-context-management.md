---
title: "Vibe · Context Management"
kind: research-finding
corpus: blackbox-research
track: harness
harness: vibe
axis: context-management
version: "2.9.6"
last_verified: "2.9.6"
status: enriched
confidence: high
topic:
  - harness
  - vibe
  - context-management
brief: "Vibe context assembly: a 10-section universal system prompt (base+headless+commit+model+tooldocs+skills+subagents+scratchpad+project-context+AGENTS.md); replaceable prompt templates in ~/.vibe/prompts/; AGENTS.md (user+project) AND per-directory AGENTS.md injected lazily on read_file results."
---

# Vibe · Context Management

> Mined from open source (GLM-5.1 bro, 2026-06-02). **confidence: high.** See axis: [Context Management](../context-management.md) · snapshot: [Vibe 2.9.6](vibe-2.9.6.md).

**Finding.** `get_universal_system_prompt` concatenates ~10 sections: base prompt (by `system_prompt_id`, builtin ids cli/explore/tests/lean/minimal, or custom `.md` from `~/.vibe/prompts/` + `.vibe/prompts/`), headless note, commit signature, model info, per-tool prompt docs, skills (XML), subagents, scratchpad, project context (git branch/status/commits), and AGENTS.md (user `~/.vibe/AGENTS.md` + project). **Notable idiom:** per-directory `AGENTS.md` is injected **lazily on `read_file` results** (`get_result_extra`) — subdir context arrives when touched, not all up front. No per-turn reminder injection beyond middleware (plan/chat reminders, context warnings).

**Evidence.**
- `vibe/core/system_prompt.py:221` — `get_universal_system_prompt` assembly
- `vibe/core/tools/builtins/read_file.py:118` — `get_result_extra` injects per-dir AGENTS.md
- `vibe/core/prompts/__init__.py:44` — `load_prompt` user→project→builtin search

**Vs the axis.** Confirms overlay (AGENTS.md) + system-prompt assembly. **Idiom worth stealing:** lazy per-directory overlay injection on file-read results — context without up-front bloat.

## Open
<!-- Cache-stable-prefix vs volatile-tail split at the message-assembly level. -->
