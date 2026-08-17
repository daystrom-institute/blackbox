//! `bro render global`: pull this host's global guidance render from the
//! daemon and apply it locally.
//!
//! The daemon owns the knowledge store but may not be the host whose
//! provider guidance files (`~/.blackbox/BLACKBOX.md`, `~/.claude/CLAUDE.md`,
//! `~/.codex/AGENTS.md`, `~/.gemini/GEMINI.md`) should change: a remote
//! daemon renders into its own `$HOME`, which no interactive session reads.
//! This command asks the daemon for a global render PLAN computed against
//! this host's common include path, then applies the plan through the same
//! managed-region patcher a daemon-local render uses (backups, marker
//! discipline, destructive-shrink guard). The host that runs the command is
//! the target policy.

use anyhow::Context;
use bbox_util::global_render::{
    GlobalRenderApplyOutcomeV1, GlobalRenderPlanV1, apply_global_render_plan,
    global_common_target_path,
};
use clap::{Args, Subcommand};
use serde_json::json;

use crate::mcp_call::{McpClient, default_base_url};

#[derive(Debug, Args)]
#[command(
    after_help = "Subcommands:\n  global    pull the daemon's global guidance render and apply it to this host"
)]
pub(crate) struct RenderArgs {
    #[command(subcommand)]
    command: RenderCommand,
}

#[derive(Debug, Subcommand)]
enum RenderCommand {
    /// Apply the daemon's global guidance render to this host's provider files
    Global(RenderGlobalArgs),
}

#[derive(Debug, Args)]
struct RenderGlobalArgs {
    /// Preview the managed regions without writing anything
    #[arg(long)]
    check: bool,
    /// Render one provider only (claude, agents, gemini); default: all
    #[arg(long, value_name = "PROVIDER")]
    provider: Option<String>,
    /// Print the applied/previewed outcomes as JSON
    #[arg(long)]
    json: bool,
    /// Daemon base URL. Defaults to the origin of $BLACKBOX_MCP_URL, else
    /// config [client].daemon_url, else http://127.0.0.1:<[daemon].port>.
    #[arg(long, value_name = "URL")]
    daemon_url: Option<String>,
    /// MCP surface to call `bbox_render` on. The anonymous `default` surface
    /// hides operator tools, so this defaults to the operator surface.
    #[arg(long, value_name = "SURFACE", default_value = "interactive")]
    surface: String,
}

pub(crate) async fn run(args: RenderArgs) -> anyhow::Result<()> {
    match args.command {
        RenderCommand::Global(args) => global(args).await,
    }
}

async fn global(args: RenderGlobalArgs) -> anyhow::Result<()> {
    let host_common_target = global_common_target_path()?;
    let base_url = args.daemon_url.unwrap_or_else(default_base_url);
    let mut client = McpClient::connect_surface(&base_url, &args.surface).await?;
    let mut arguments = json!({
        "scope": "global",
        "global_plan": { "host_common_target": host_common_target.display().to_string() },
    });
    if let Some(provider) = &args.provider {
        arguments["provider"] = json!(provider);
    }
    let plan: GlobalRenderPlanV1 = client
        .call_tool_json("bbox_render", arguments)
        .await
        .context("requesting the global render plan from the daemon")?;
    let outcomes = apply_global_render_plan(&plan, args.check)?;
    print_outcomes(&plan, &outcomes, args.check, args.json)
}

fn print_outcomes(
    plan: &GlobalRenderPlanV1,
    outcomes: &[GlobalRenderApplyOutcomeV1],
    check: bool,
    as_json: bool,
) -> anyhow::Result<()> {
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "mode": if check { "check" } else { "apply" },
                "checksum": plan.checksum,
                "outcomes": outcomes,
                "diagnostics": plan.diagnostics,
            }))?
        );
        return Ok(());
    }
    for outcome in outcomes {
        let prefix = if check { "[CHECK] " } else { "" };
        match &outcome.backup {
            Some(backup) => println!("{prefix}{} (backup: {backup})", outcome.summary),
            None => println!("{prefix}{}", outcome.summary),
        }
        if check {
            if let Some(block) = &outcome.managed_block {
                println!("--- proposed managed region for {} ---", outcome.path);
                println!("{block}");
            }
        }
    }
    for diagnostic in &plan.diagnostics {
        eprintln!("diagnostic: {diagnostic}");
    }
    Ok(())
}
