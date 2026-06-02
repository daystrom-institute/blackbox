---
title: "Claude · Privilege, Sandboxing & Approvals"
kind: research-finding
corpus: blackbox-research
track: harness
harness: claude
axis: privilege-approvals
version: "2.1.160"
last_verified: "2.1.160"
status: enriched
confidence: high
topic:
  - harness
  - claude
  - privilege-approvals
brief: "Claude permissions: 5 modes (default/plan/auto/acceptEdits/bypassPermissions); --dangerously-skip-permissions (double-gated); a declarative allow/deny rules DSL with tool-specific matchers (Bash(npm run:*), Edit(src/**)); PreToolUse hooks return permissionDecision allow/deny/ask (programmatic override); 'auto' mode = Claude self-assesses risk and auto-approves low-risk calls."
---

# Claude · Privilege, Sandboxing & Approvals

> Mined from the Claude Code 2.1.160 binary (Bun-compiled JS bundle, `strings` + grep) by a GLM-5.1 bro, 2026-06-02. **confidence: high** (verbatim string literals) + live `~/.claude/` config. This cell was added in the claude *new-axes* pass; two findings **correct** session-1 assumptions (durable goal + memory-consolidation DO exist).
See axis: [Privilege, Sandboxing & Approvals](../privilege-approvals.md) · snapshot: [Claude 2.1.160](claude-2.1.160.md).

**Finding.** Five permission modes: `default`, `plan`, `auto`, `acceptEdits`, `bypassPermissions` (`PERMISSION_MODES` / `INTERNAL_` / `EXTERNAL_PERMISSION_MODES`). `--dangerously-skip-permissions` (double-gated behind `--allow-dangerously-skip-permissions`; can be disabled by feature gate/settings) enables bypass. A **declarative allow/deny rules DSL** with tool-specific matchers — `Bash(npm run:*)` prefix, `Edit(src/**)` glob. **PreToolUse hooks** can override the interactive prompt by returning `permissionDecision: "allow"|"deny"|"ask"` (programmatic gating; the live host config has a `PreToolUse` Bash matcher calling `rtk-rewrite.sh`). **`auto` mode** lets Claude self-assess risk and auto-approve low-risk calls while prompting for the rest.

**Evidence.**
- `permissionModes: "default"|"plan"|"acceptEdits"|"bypassPermissions"` (~269614); `auto` mode string (~268680)
- allow/deny schema: `y.array(Zg$()).describe("List of permission rules…")` (~290221)
- `--dangerously-skip-permissions` (~296292); `permissionDecision … (PreToolUse only)` (~268570)

**Vs the axis.** Strongly confirms the axis incl. **envelope declaration** (modes) + a **rules DSL** + hook-programmatic decisions. Claude's `auto` (model self-risk-assessment) mirrors codex's reviewer/guardian idea in a single-process form. Places Claude with codex/agy (declares envelope) vs vibe (doesn't).

## Open
<!-- Whether the active mode/rules are surfaced to the model in-prompt, or only enforced; the auto-mode risk heuristic. -->
