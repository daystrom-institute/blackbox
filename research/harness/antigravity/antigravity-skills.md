---
title: "Antigravity · Skills"
kind: research-finding
corpus: blackbox-research
track: harness
harness: antigravity
axis: skills
version: "1.0.4"
last_verified: "1.0.4"
status: enriched
confidence: high
topic:
  - harness
  - antigravity
  - skills
brief: "SDK source confirms Agent Skills support through skills_paths, loading directories that contain SKILL.md; either direct skill dirs or parent dirs are accepted. CLI binary/changelog still indicate plugin-derived slash commands and skills.json scanning, but the populated CLI schema is not live-confirmed on this host."
---

# Antigravity · Skills

> Evidence: installed agy 1.0.4 binary strings/changelog/local ~/.gemini state plus public google-antigravity SDK source at f74a23fc5f4026129a5b4498ce652d7d6018e23f. SDK claims are source-grounded for the SDK/localharness surface; CLI/cortex claims remain scoped to live state, logs, and binary-string evidence.
See axis: [Skills](../skills.md) · snapshot: [Antigravity 1.0.4](antigravity-1.0.4.md).

**Finding.** Skills surface as **plugin-based slash commands** (not a separate tool). `SkillsConfig{GetInheritUser, GetSkillsPaths}`; `skills.json` discovered via directory scan; `ScanSkillsConfigFileRequest`, `ProcessSkillsDirToSpecs`; "GetAllSkills: loaded %d skills". Plugin discovery (v1.0.1) for skills + agents; plugins install to `~/.gemini/config/` (v1.0.2). v1.0.4 fixed skill-derived slash-command autocompletion.

**Evidence.**
- `SkillsConfig{GetInheritUser,GetSkillsPaths}`; `ProcessSkillsConfigFileToSpecs`
- CHANGELOG v1.0.1 "plugin discovery for skills and agents"; v1.0.2 "~/.gemini/config/"

**Vs the axis.** Confirms SDK Agent Skills path loading with high confidence and CLI skills+plugin namespacing with medium confidence. The populated CLI skills.json schema is still inferred from protobuf/changelog strings because no on-disk skills.json exists on this host.

## SDK/local harness update (2026-06-02)

The public SDK confirms a first-class Agent Skills path independent of the inferred CLI plugin schema. AgentConfig accepts skills_paths. The getting-started example says each skill is a directory with a SKILL.md file, and the SDK can be pointed either at a direct skill directory or at a parent directory containing multiple skills. This closely matches the current Codex skill-loading shape and is stronger evidence than binary strings alone.

The CLI/plugin side remains only partially confirmed. Binary strings and changelog entries still indicate skills.json scanning, plugin discovery for skills and agents, installation under ~/.gemini/config/, and skill-derived slash-command autocomplete. No populated skills.json exists on this host, so schema claims should stay medium confidence even though the SDK skills_paths contract is high confidence.

## Open
<!-- skills.json schema; progressive-disclosure mechanism; allowed_tools-style pre-approval. -->
