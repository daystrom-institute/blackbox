//! Claude-compatible CLI surface. The daemon's Claude arm
//! (`providers/exec_args.rs`) emits a fixed flag set; the harness must accept
//! all of it, honouring what's meaningful and ignoring the rest. No new
//! daemon argv code is required — we conform to the bytes it already sends.

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "bro-harness", disable_help_flag = false)]
pub struct Cli {
    /// The user turn. May be omitted when the daemon moves a large prompt to
    /// stdin (`move_large_prompt_arg_to_stdin`); in that case stdin is read.
    #[arg(short = 'p', long = "prompt")]
    pub prompt: Option<String>,

    /// Only `stream-json` is supported; asserted at startup.
    #[arg(long = "output-format")]
    pub output_format: Option<String>,

    /// Bidirectional control plane. When `stream-json`, the harness keeps a
    /// persistent session: it reads successive user-turn messages and
    /// `control_request`s (interrupt, set_model, …) from stdin as NDJSON, stays
    /// alive between turns, and treats `/compact` as an in-stream slash command.
    /// Absent ⇒ legacy one-shot mode (read one prompt, run, exit).
    #[arg(long = "input-format")]
    pub input_format: Option<String>,

    /// Re-emit each stdin user message back on stdout as a `user` event (gated
    /// on stream-json in/out). Mirrors the claude CLI's `--replay-user-messages`.
    #[arg(long = "replay-user-messages", default_value_t = false)]
    pub replay_user_messages: bool,

    /// Session id to mint/persist under.
    #[arg(long = "session-id")]
    pub session_id: Option<String>,

    /// Resume a prior transcript and continue it.
    #[arg(long = "resume")]
    pub resume: Option<String>,

    /// Model id (already normalized by the daemon: GLM strips
    /// `zai-coding-plan/`, DeepSeek strips `deepseek/`).
    #[arg(long = "model")]
    pub model: Option<String>,

    /// Working directory for this session. File/shell tools resolve against it
    /// (`ToolCx.root`) and project-doc discovery starts here. The daemon passes
    /// the dispatch cwd here instead of mutating the process cwd, so concurrent
    /// in-process sessions never collide (harness-daemon-boundary.md §3). Absent
    /// ⇒ the process cwd (standalone binary).
    #[arg(long = "cwd")]
    pub cwd: Option<String>,

    /// Reasoning effort → mapped to a `thinking` budget when supported.
    #[arg(long = "effort")]
    pub effort: Option<String>,

    /// Service tier — the latency/priority lever. `priority` is codex's `/fast`;
    /// `flex` and `default` are also accepted. Forwarded on the OpenAI Responses
    /// body (`default` is dropped). Env fallback: `BRO_HARNESS_SERVICE_TIER`.
    #[arg(long = "service-tier")]
    pub service_tier: Option<String>,

    /// System prompt. Non-empty ⇒ explicit override used verbatim. Empty string
    /// ⇒ suppress (no system prompt). Absent ⇒ Codex-style AGENTS.md overlay
    /// discovery (global `$CODEX_HOME/AGENTS.md` + repo `AGENTS.md`, project
    /// scope); see `project_doc`.
    #[arg(long = "system-prompt")]
    pub system_prompt: Option<String>,

    /// Transient MCP config (`{"mcpServers":{name:{...}}}`). HTTP URLs may
    /// carry a server-enforced surface as a query param (`?surface=…`); stdio
    /// entries carry command/args/env. The harness just connects and lists.
    #[arg(long = "mcp-config")]
    pub mcp_config: Option<String>,

    /// Comma-separated client-side DENY patterns for MCP tools, as
    /// fully-qualified `mcp__<server>__<tool>` names (exact or trailing-`*`
    /// prefix). This is the allow/deny permission plane — the daemon's
    /// recursion guard + brofile + per-dispatch disallow — NOT surface
    /// (surface is server-side, via the URL). Matching MCP tools are excluded
    /// entirely: not listed, not tool_search-loadable, refused if called.
    #[arg(long = "deny-tools")]
    pub deny_tools: Option<String>,

    /// Comma-separated client-side ALLOW patterns (same form). When non-empty,
    /// only matching MCP tools are admitted. Built-in harness tools are never
    /// subject to either list.
    #[arg(long = "allow-tools")]
    pub allow_tools: Option<String>,

    /// Code-mode selector — `off` (flat tools only, no `exec`/`wait`),
    /// `optional` (flat tools AND `exec`/`wait`; default), or `only`
    /// (`exec`/`wait` are the authorial surface, flat builtins hidden from the
    /// wire array but still `tool_search`-loadable). On resume this falls back
    /// to the value persisted with the session (the daemon need not re-pass it),
    /// then to `BRO_HARNESS_CODE_MODE`, then to the default. Unknown values warn
    /// and use the default.
    #[arg(long = "code-mode")]
    pub code_mode: Option<String>,

    /// JSON schema for structured output. When supplied, a synthetic
    /// `final_result` tool is registered whose `input_schema` is this schema,
    /// and the agent is instructed to call it with its final answer. The
    /// captured arguments become the structured result. Env fallback:
    /// `BRO_HARNESS_OUTPUT_SCHEMA`.
    #[arg(long = "output-schema")]
    pub output_schema: Option<String>,

    /// Host-supplied tool arg default/pin table as a JSON object whose keys are
    /// `<flavor>:<tool-pattern>.<param>`. Env fallback:
    /// `BRO_HARNESS_TOOL_DEFAULTS`.
    #[arg(long = "additional-context")]
    pub additional_context: Option<String>,

    /// Host-supplied NON-SECRET env overlay for shell children, as a JSON
    /// string map (e.g. '{"RUSTC_WRAPPER":"sccache"}'). Separate lane from
    /// transport/session env: build env reaches shells, credentials do not.
    /// Env fallback: `BRO_HARNESS_SHELL_ENV`.
    #[arg(long = "shell-env")]
    pub shell_env: Option<String>,

    // --- accepted, no-op (we always stream NDJSON; safety is the denylist) ---
    #[arg(long = "verbose", default_value_t = false)]
    pub verbose: bool,
    #[arg(long = "include-partial-messages", default_value_t = false)]
    pub include_partial_messages: bool,
    #[arg(long = "dangerously-skip-permissions", default_value_t = false)]
    pub dangerously_skip_permissions: bool,
}
