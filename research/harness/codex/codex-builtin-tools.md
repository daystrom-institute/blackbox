---
title: "Codex · Built-in Tools"
kind: research-finding
corpus: blackbox-research
track: harness
harness: codex
axis: builtin-tools
version: "0.136.0"
last_verified: "0.136.0"
status: enriched
confidence: high
topic:
  - harness
  - codex
  - builtin-tools
brief: "Codex tools: 12+ specs as ToolSpec::Function (JSON args) or ToolSpec::Freeform (Lark grammar — apply_patch is non-JSON, 'do not wrap in JSON'). Typed output_schema on several (spawn_agent, view_image, list/wait_agent) but None on shell/patch. Per-tool parallel-safety (read_only_hint auto-qualifies MCP tools). Steering language is inline Rust string literals (spawn_agent anti-delegation framework; goal blocked-audit)."
---

# Codex · Built-in Tools

> Mined from codex-rs source (`~/repos/codex/codex-rs`) by DeepSeek-v4-pro / GLM-5.1 bros, 2026-06-02. **confidence: high** (file:line).
See axis: [Built-in Tools](../builtin-tools.md) · snapshot: [Codex 0.136.0](codex-0.136.0.md).

**Finding.** 12+ named specs: `exec_command`/`write_stdin`/`shell_command`, `apply_patch`, `view_image`, `update_plan`, `create/get/update_goal`, `request_user_input`, `request_permissions`, MCP resource tools, `spawn_agents_on_csv`/`report_agent_job_result`, plugin-install tools, + dynamic/MCP. Two tool *kinds*: `ToolSpec::Function(ResponsesApiTool)` (JSON) and `ToolSpec::Freeform(FreeformTool)` (**Lark grammar, non-JSON** — `apply_patch` uses `*** Begin Patch` markers, "do not wrap the patch in JSON"). **Typed output schemas** on several tools (spawn/view_image/list/wait) but `None` on shell/patch (freeform raw text). **Per-tool parallel-safety**: MCP `read_only_hint` auto-qualifies; `parallel.rs` runs concurrent calls via JoinSet. **Steering** is inline Rust string literals: `spawn_agent` carries a "when to delegate vs do it yourself" framework; `update_goal` an anti-pattern ("Do not use blocked merely because the work is hard").

**Evidence.**
- `core/src/tools/handlers/apply_patch_spec.rs:8` — "This is a FREEFORM tool, so do not wrap the patch in JSON."
- `core/src/tools/handlers/multi_agents_spec.rs:319-380` — per-tool output schemas
- `core/src/tools/handlers/mcp.rs:77` — `supports_parallel_tool_calls || read_only_hint`

**Vs the axis.** Confirms ALL the tool-I/O-contract extensions: output schemas, invocation format (JSON vs Lark freeform), per-tool concurrency, agent-authored elicitation (`request_user_input`), and self-describing steering.

## Open
<!-- Full verbatim tooldoc for exec_command (sandbox params already in privilege-approvals). -->
