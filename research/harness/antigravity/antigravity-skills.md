---
title: "Antigravity · Skills"
kind: research-finding
corpus: blackbox-research
track: harness
harness: antigravity
axis: skills
version: "1.0.4"
last_verified: "1.0.4"
status: researching
confidence: low
topic:
  - harness
  - antigravity
  - skills
brief: "agy skills are plugin-based slash commands: skills.json discovered via directory scan; SkillsConfig (InheritUser, SkillsPaths); plugin discovery for skills+agents installs to ~/.gemini/config/. Inferred from protobuf + CHANGELOG; no skills.json on this host to confirm schema."
---

# Antigravity · Skills

> Mined from the `agy` v1.0.4 Go binary (`strings` ~500K lines) + `~/.gemini/` config + docs/CHANGELOG by DeepSeek-v4-pro bros, 2026-06-02. **Caveat:** agy is a THIN gRPC client to Google's server-side "cortex" engine — tools/loop/compaction run server-side, so confidence is capped at *medium* for anything not a verbatim binary string or a live config file.
See axis: [Skills](../skills.md) · snapshot: [Antigravity 1.0.4](antigravity-1.0.4.md).

**Finding.** Skills surface as **plugin-based slash commands** (not a separate tool). `SkillsConfig{GetInheritUser, GetSkillsPaths}`; `skills.json` discovered via directory scan; `ScanSkillsConfigFileRequest`, `ProcessSkillsDirToSpecs`; "GetAllSkills: loaded %d skills". Plugin discovery (v1.0.1) for skills + agents; plugins install to `~/.gemini/config/` (v1.0.2). v1.0.4 fixed skill-derived slash-command autocompletion.

**Evidence.**
- `SkillsConfig{GetInheritUser,GetSkillsPaths}`; `ProcessSkillsConfigFileToSpecs`
- CHANGELOG v1.0.1 "plugin discovery for skills and agents"; v1.0.2 "~/.gemini/config/"

**Vs the axis.** Confirms skills+plugin namespacing (matches the codex-lens plugin-bundle extension). **Low confidence:** schema inferred from protobuf; no on-disk `skills.json` here.

## Open
<!-- skills.json schema; progressive-disclosure mechanism; allowed_tools-style pre-approval. -->
