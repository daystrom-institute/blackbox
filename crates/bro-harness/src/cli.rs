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

    /// Reasoning effort → mapped to a `thinking` budget when supported.
    #[arg(long = "effort")]
    pub effort: Option<String>,

    /// System prompt override. Empty string ⇒ suppress (provider-defaults).
    #[arg(long = "system-prompt")]
    pub system_prompt: Option<String>,

    /// Transient blackbox MCP config (`{"mcpServers":{name:{url}}}`). The URL
    /// carries the surface as a query param (`?surface=…`); the daemon picks
    /// it and the server enforces it. The harness just connects and lists.
    #[arg(long = "mcp-config")]
    pub mcp_config: Option<String>,

    // --- accepted, no-op (we always stream NDJSON; safety is the denylist) ---
    #[arg(long = "verbose", default_value_t = false)]
    pub verbose: bool,
    #[arg(long = "include-partial-messages", default_value_t = false)]
    pub include_partial_messages: bool,
    #[arg(long = "dangerously-skip-permissions", default_value_t = false)]
    pub dangerously_skip_permissions: bool,
}
