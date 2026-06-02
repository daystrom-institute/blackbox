---
title: "Antigravity - Privilege, Sandboxing & Approvals"
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
brief: "SDK policy is fail-closed for dangerous tools: default Agent is read-only; enabling write tools or MCP without policies/hook decisions raises ValueError; default LocalAgentConfig denies run_command and allows others; policy precedence is specific deny/ask/allow before wildcard deny/ask/allow, and predicate failures fail closed. CLI still has sandbox/bypass flags."
---

# Antigravity - Privilege, Sandboxing & Approvals

> Evidence: public google-antigravity SDK source at f74a23fc5f4026129a5b4498ce652d7d6018e23f, installed agy 1.0.4 binary strings/changelog, and local CLI help. SDK policy claims are high confidence; standalone CLI sandbox internals remain medium where only binary strings expose them.
See axis: [Privilege, Sandboxing & Approvals](../privilege-approvals.md) - snapshot: [Antigravity 1.0.4](antigravity-1.0.4.md).

## Finding

The SDK gives a clean, fail-closed policy model. AgentConfig defaults to read-only builtins. If write-capable tools or MCP servers are enabled without policies or hook decisions, Agent.__aenter__ raises ValueError rather than silently allowing execution. Broader capability must be paired with an approval/denial story.

Policies have three decisions: approve, deny, and ask_user. Precedence is specific deny, specific ask, specific allow, prefix wildcard deny/ask/allow, then global wildcard deny/ask/allow. Predicate failures are treated as matches, which fails closed for restrictive predicates. workspace_only constrains view/create/edit operations to configured workspace roots.

The default LocalAgentConfig policy confirms run_command as the special risky builtin: run_command is denied by default while other builtins are allowed. Reference examples upgrade run_command to interactive ask_user when desired. CapabilitiesConfig also separates privilege from context: disabled tools are removed from model context, while policy-denied tools remain visible and reject at execution time.

Standalone agy adds a CLI sandbox layer. agy --help exposes --sandbox and --dangerously-skip-permissions. Binary strings and changelog entries indicate a two-tier run_command envelope with normal sandbox execution and bypass requiring stronger approval, plus proceed-in-sandbox and auto-approve surfaces. Those are strong signals, but exact enforcement lives outside the public SDK source.

## Design Takeaways

- Antigravity treats capability enablement, tool visibility, and runtime approval as distinct controls.
- The SDK refuses dangerous capability expansion without explicit policy/hook authority.
- CLI sandbox posture is agent-facing in strings, but the corpus should summarize the envelope rather than copy proprietary prompt prose.

## Open

- Full standalone agy sandbox implementation and per-call bypass approval contract.
- rules.json schema and how it combines with SDK-style policy.
- Whether CLI MCP policies use the same precedence model as the SDK.
