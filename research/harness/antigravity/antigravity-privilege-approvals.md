---
title: "Antigravity · Privilege, Sandboxing & Approvals"
kind: research-finding
corpus: blackbox-research
track: harness
harness: antigravity
axis: privilege-approvals
version: "1.0.4"
last_verified: "1.0.4"
status: enriched
confidence: high
topic:
  - harness
  - antigravity
  - privilege-approvals
brief: "agy: a TWO-TIER sandbox on run_command — Standard (default: network blocked, fs restricted, AUTO-executes silently) vs BypassSandbox:true (unrestricted, REQUIRES user approval). The model is TOLD its envelope via a verbatim run_command prompt ('SANDBOX: …'). proceed-in-sandbox tool mode; auto-approve-all toggle; rules.json allow/exclusion; server-side loop detection; policy-guardian-config."
---

# Antigravity · Privilege, Sandboxing & Approvals

> Mined from the `agy` v1.0.4 Go binary (`strings` ~500K lines) + `~/.gemini/` config + docs/CHANGELOG by DeepSeek-v4-pro bros, 2026-06-02. **Caveat:** agy is a THIN gRPC client to Google's server-side "cortex" engine — tools/loop/compaction run server-side, so confidence is capped at *medium* for anything not a verbatim binary string or a live config file.
See axis: [Privilege, Sandboxing & Approvals](../privilege-approvals.md) · snapshot: [Antigravity 1.0.4](antigravity-1.0.4.md).

**Finding.** A **two-tier sandbox** on the `run_command` tool, declared to the model in a verbatim prompt: **Standard sandbox** (default — internet blocked, filesystem limited, **auto-executes silently**) vs **`BypassSandbox: true`** (unrestricted — **requires user approval**, "ONLY when a command strictly requires internet access"). Blocked network → "PCEP … blocked by the sandbox. Retry…". Tool permission mode `proceed-in-sandbox` (v1.0.1) auto-executes in-sandbox tools; an "Auto-approve all tools" toggle bypasses approvals; `KeySubagentApproveFast`. `rules.json` carries allowlist/exclusion (`EXCLUSION_ELEMENT_MISSING_DEPENDENCY`; absent on this host). `policy-guardian-config` string + server-side loop detection. Live MCP config disables 63 tools on one server (a privilege-narrowing surface).

**Evidence.**
- binary (~163965): "SANDBOX: Commands run inside a restricted sandbox that blocks internet access…"; "Set BypassSandbox: true ONLY when…"
- `CASCADE_COMMANDS_AUTO_EXECUTION_PROCEED_IN_SANDBOX`; "proceed-in-sandbox" (CHANGELOG v1.0.1)
- `~/.gemini/antigravity/mcp_config.json` — 63 `disabledTools` on `blackbox`

**Vs the axis.** Strongly confirms the axis AND the **envelope-declaration** facet — like codex/claude, **agy TELLS the model its sandbox posture** (the verbatim run_command prompt). This puts agy with codex/claude and against vibe (which keeps the model uninformed) — a clean 3-vs-1 split on the declaration question.

## Open
<!-- rules.json schema; policy-guardian-config detail; whether bypass-approval is per-call justified. -->
