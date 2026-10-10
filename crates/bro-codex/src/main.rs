//! `bro-codex`: the codex lane's worker.
//!
//! Speaks the claude CLI's headless stream-json contract on stdio (user
//! envelopes and `control_request`s in, the stream-json envelope out, exit at
//! end of input) and fronts one `codex app-server` child for the life of the
//! dispatch. The app-server owns the model loop, tools, sandbox, config and
//! transcript; this process only translates. See AGENTS.md.

mod args;
mod mcp;
mod rpc;
mod shim;
#[cfg(test)]
mod tests;

use std::process::{ExitCode, Stdio};
use std::time::Duration;

use tokio::io::BufReader;

use crate::args::ShimArgs;
use crate::shim::{SessionConfig, warn};

/// How long the app-server gets to exit after its stdin closes.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);
/// How long startup waits for the `initialize` answer.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            warn(&format!("cannot start the async runtime: {error}"));
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(run())
}

async fn run() -> ExitCode {
    let args = match ShimArgs::parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(error) => {
            warn(&format!("{error:#}"));
            return ExitCode::FAILURE;
        }
    };
    for warning in &args.warnings {
        warn(warning);
    }
    let overrides = mcp::build(
        &args.mcp_configs,
        &args.allowed_tools,
        &args.disallowed_tools,
    );
    for warning in &overrides.warnings {
        warn(warning);
    }

    let codex = std::env::var("CODEX_BIN")
        .ok()
        .filter(|bin| !bin.is_empty())
        .unwrap_or_else(|| "codex".to_string());
    let mut command = tokio::process::Command::new(&codex);
    command.arg("app-server");
    for value in &overrides.config_overrides {
        command.arg("-c").arg(value);
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            warn(&format!("cannot start `{codex} app-server`: {error}"));
            return ExitCode::FAILURE;
        }
    };
    let (Some(server_in), Some(server_out)) = (child.stdin.take(), child.stdout.take()) else {
        warn("the codex app-server has no stdio pipes");
        return ExitCode::FAILURE;
    };
    let (rpc, notifications) = rpc::Rpc::start(BufReader::new(server_out), server_in);

    match tokio::time::timeout(HANDSHAKE_TIMEOUT, shim::handshake(&rpc)).await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => {
            warn(&format!("the codex app-server handshake failed: {error}"));
            let _ = child.kill().await;
            return ExitCode::FAILURE;
        }
        Err(_) => {
            warn("the codex app-server did not answer initialize");
            let _ = child.kill().await;
            return ExitCode::FAILURE;
        }
    }

    let cwd = std::env::current_dir()
        .map(|dir| dir.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".to_string());
    let disabled_mcp_servers = if args.strict_mcp_config {
        shim::servers_to_disable(&rpc, &cwd, &overrides.servers).await
    } else {
        Vec::new()
    };
    let cfg = SessionConfig {
        resume: args.resume,
        model: args.model,
        effort: args.effort,
        developer_instructions: args.append_system_prompt,
        base_instructions: args.system_prompt,
        output_schema: args.json_schema,
        sandbox: if args.skip_permissions {
            "danger-full-access"
        } else {
            "workspace-write"
        },
        include_partial_messages: args.include_partial_messages,
        replay_user_messages: args.replay_user_messages,
        cwd,
        mcp_servers: overrides.servers,
        disabled_mcp_servers,
    };

    let outcome = shim::run_session(
        cfg,
        rpc.clone(),
        notifications,
        BufReader::new(tokio::io::stdin()),
        tokio::io::stdout(),
    )
    .await;
    if let Err(error) = outcome {
        warn(&format!("{error:#}"));
    }
    rpc.close().await;
    match tokio::time::timeout(SHUTDOWN_GRACE, child.wait()).await {
        Ok(_) => {}
        Err(_) => {
            warn("the codex app-server did not exit at end of input; killing it");
            let _ = child.kill().await;
        }
    }
    ExitCode::SUCCESS
}
