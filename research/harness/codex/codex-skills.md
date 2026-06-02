---
title: "Codex · Skills"
kind: research-finding
corpus: blackbox-research
track: harness
harness: codex
axis: skills
version: "0.136.0"
last_verified: "0.136.0"
status: enriched
confidence: high
topic:
  - harness
  - codex
  - skills
brief: "Codex skills: SKILL.md (YAML frontmatter: name/metadata/interface/dependencies.tools/policy) from 4+ roots (embedded system skills via include_dir!, ~/skills, .agents/, plugin paths; depth cap 6 / 2000 dirs). @skill mention triggers MCP-dependency auto-install. Plugin bundles (plugin.json + .claude-plugin/plugin.json alt) ship skills+MCP+apps+hooks. A code-mode V8 JS runtime (exec/wait) composes tools via a global tools object."
---

# Codex · Skills

> Mined from codex-rs source (`~/repos/codex/codex-rs`) by DeepSeek-v4-pro / GLM-5.1 bros, 2026-06-02. **confidence: high** (file:line).
See axis: [Skills](../skills.md) · snapshot: [Codex 0.136.0](codex-0.136.0.md).

**Finding.** Skills = `SKILL.md` with YAML frontmatter (`name`, `metadata` short-description/default-prompt, `interface`, `dependencies.tools[]` type/transport/command/url, `policy` allow_implicit_invocation). Roots: **embedded system skills** (`include_dir!` → `CODEX_HOME/skills/.system`, fingerprint-gated), `~/skills/`, project `.agents/`, plugin paths; scan capped at depth 6 / 2000 dirs/root. **Mention-triggered MCP install**: an `@skill` mention (`extract_tool_mentions`) whose `dependencies.tools[].type=="mcp"` are unmet prompts the user (Install / Continue anyway) and writes MCP config via `ConfigEditsBuilder`. **Plugin bundles** (`plugin.json`; alt `.claude-plugin/plugin.json`) carry skills + mcp_servers + apps + hooks + interface. **code-mode**: `exec`/`wait` tools run JS in a V8 isolate with a global `tools` object (`await tools.exec_command(...)`), helpers `store/load/notify/yield_control/exit/text/image`; MCP return types projected to TypeScript in the tool description.

**Evidence.**
- `core-skills/src/loader.rs:107-124` — SKILL.md, `.agents`, MAX_SCAN_DEPTH=6, MAX_SKILLS_DIRS_PER_ROOT=2000
- `core/src/mcp_skill_dependencies.rs:30` — mention → install prompt
- `code-mode/src/description.rs:12-43` — V8 `exec` runtime + global `tools` helpers

**Vs the axis.** Confirms progressive disclosure + the plugin-bundle and mention-triggered-provisioning extensions. **code-mode** is the frontier "programmable tool-composition runtime" candidate flagged in the discovery pass — confirmed real here.

## Open
<!-- code-mode security model; whether code-mode warrants its own axis (flag for CLI_INVESTIGATOR). -->
