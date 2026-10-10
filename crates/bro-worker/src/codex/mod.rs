//! Native Codex app-server adapter, linked by both execution hosts.
//! The host owns the app-server process; this module owns its RPC session.
mod mcp;
mod rpc;
mod session;
#[cfg(test)]
mod tests;

use anyhow::Context;
use bro_protocol::CodexSessionConfig;
use serde_json::Value;
use tokio::io::BufReader;
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::mpsc;

pub fn validate(config: &CodexSessionConfig) -> anyhow::Result<()> {
    mcp::overrides(config).map(|_| ())
}

pub async fn run(
    config: CodexSessionConfig,
    cwd: String,
    stdin: ChildStdin,
    stdout: ChildStdout,
    controls: mpsc::UnboundedReceiver<Value>,
    events: mpsc::UnboundedSender<String>,
    mut log: crate::SessionLogWriter,
) -> anyhow::Result<()> {
    let new_session = config.resume.is_none();
    let overrides = mcp::overrides(&config)?;
    let (rpc, notifications) = rpc::Rpc::start(BufReader::new(stdout), stdin);
    let setup = async {
        session::handshake(&rpc).await?;
        let names: Vec<String> = config.mcp_servers.keys().cloned().collect();
        let disabled = session::servers_to_disable(&rpc, &cwd, &names).await?;
        Ok::<_, anyhow::Error>(session::SessionConfig {
            resume: config.resume,
            model: config.model,
            effort: config.effort,
            service_tier: config.service_tier,
            developer_instructions: config.developer_instructions,
            output_schema: config.output_schema,
            config_overrides: overrides,
            sandbox: "danger-full-access",
            include_partial_messages: true,
            replay_user_messages: true,
            cwd,
            mcp_servers: names,
            disabled_mcp_servers: disabled,
            ..Default::default()
        })
    }
    .await;
    let cfg = match setup {
        Ok(cfg) => cfg,
        Err(error) => {
            rpc.close().await;
            return Err(error);
        }
    };
    let (output_writer, mut output_reader) = mpsc::unbounded_channel::<Value>();
    let mut output = tokio::spawn(async move {
        while let Some(event) = output_reader.recv().await {
            if new_session && event["type"] == "system" && event["subtype"] == "init" {
                log.bind_session(
                    event["session_id"]
                        .as_str()
                        .context("Codex init has no session id")?,
                )
                .await?;
            }
            let line = log.sequence_and_record(&event.to_string()).await?;
            // Loss of the owner connection must not stop execution or logging.
            let _ = events.send(line);
        }
        Ok::<_, anyhow::Error>(())
    });
    let drive = session::run_session(cfg, rpc.clone(), notifications, controls, output_writer);
    tokio::pin!(drive);
    let outcome = tokio::select! {
        outcome = &mut drive => outcome,
        written = &mut output => {
            rpc.close().await;
            written.context("joining Codex event writer")??;
            anyhow::bail!("Codex event writer stopped before the session ended");
        }
    };
    rpc.close().await;
    output.await.context("joining Codex event writer")??;
    outcome
}
