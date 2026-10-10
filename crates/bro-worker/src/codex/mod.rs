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
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::BufReader;
use tokio::process::{Child, ChildStdin, ChildStdout};
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
    log_path: PathBuf,
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
        let mut log = crate::SessionLogWriter::open(&log_path)
            .await
            .context("cannot open Codex event log")?;
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
        log.finish().await;
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

/// A finished adapter closes app-server input. Bound process shutdown even when
/// app-server keeps running after EOF. A process exit also bounds adapter drain.
pub async fn wait_for_child(
    mut child: Child,
    mut adapter: tokio::task::JoinHandle<anyhow::Result<()>>,
) -> (Option<i32>, String) {
    tokio::select! {
        outcome = &mut adapter => {
            let mut error = match outcome { Ok(Ok(())) => String::new(), Ok(Err(e)) => format!("Codex adapter: {e:#}"), Err(e) => format!("Codex adapter task: {e}") };
            let status = match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
                Ok(status) => status.ok().and_then(|s| s.code()),
                Err(_) => {
                    let _ = child.kill().await;
                    if error.is_empty() { error = "Codex app-server did not exit after input closed".into(); }
                    Some(1)
                }
            };
            (if error.is_empty() { status } else { Some(1) }, error)
        }
        status = child.wait() => {
            let error = match tokio::time::timeout(Duration::from_secs(2), &mut adapter).await {
                Ok(Ok(Ok(()))) => String::new(),
                Ok(Ok(Err(e))) => format!("Codex adapter: {e:#}"),
                Ok(Err(e)) => format!("Codex adapter task: {e}"),
                Err(_) => { adapter.abort(); "Codex adapter did not drain after process exit".into() }
            };
            (if error.is_empty() { status.ok().and_then(|s| s.code()) } else { Some(1) }, error)
        }
    }
}
