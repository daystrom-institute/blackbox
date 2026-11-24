//! Native Antigravity adapter over the Agent Client Protocol, linked by both
//! execution hosts. The host owns the `agy acp` process; this module owns its
//! RPC session.
mod mcp;
mod session;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;

use anyhow::Context;
use bro_protocol::AgySessionConfig;
use serde_json::{Value, json};
use tokio::io::BufReader;
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::mpsc;

/// The arguments the execution host starts the server with. Permission
/// prompts are off: a dispatch has no one to answer them.
pub const SERVER_ARGS: &[&str] = &["acp", "--dangerously-skip-permissions"];

/// The server runs with permission prompts off, so it should not ask; a
/// request that arrives anyway is granted, which is what that posture means.
pub(crate) const PEER: crate::rpc::Peer = crate::rpc::Peer {
    name: "agy",
    process: "the agy ACP server",
    answer: |method, params| match method {
        "session/request_permission" => Some(grant(params)),
        _ => None,
    },
};

fn grant(params: &Value) -> Value {
    let options = params["options"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let chosen = options
        .iter()
        .find(|option| option["kind"] == "allow_once")
        .or_else(|| {
            options.iter().find(|option| {
                option["kind"]
                    .as_str()
                    .is_some_and(|kind| kind.starts_with("allow"))
            })
        })
        .and_then(|option| option["optionId"].as_str());
    match chosen {
        Some(id) => json!({"outcome": {"outcome": "selected", "optionId": id}}),
        None => json!({"outcome": {"outcome": "cancelled"}}),
    }
}

/// Refuse a malformed session before a process is spawned. Worker variables
/// are resolved at run time; here every reference counts as set.
pub fn validate(config: &AgySessionConfig) -> anyhow::Result<()> {
    mcp::servers(config, &|_| Some(String::new())).map(|_| ())
}

/// Drive one dispatch against a started server. `env` is the worker
/// environment the dispatch's MCP definitions may reference.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    config: AgySessionConfig,
    cwd: String,
    env: BTreeMap<String, String>,
    stdin: ChildStdin,
    stdout: ChildStdout,
    controls: mpsc::UnboundedReceiver<Value>,
    events: mpsc::UnboundedSender<String>,
    mut log: crate::SessionLogWriter,
) -> anyhow::Result<()> {
    let new_session = config.resume.is_none();
    let servers = mcp::servers(&config, &|name| env.get(name).cloned())?;
    let (rpc, notifications) = crate::rpc::Rpc::start(BufReader::new(stdout), stdin, PEER);
    if let Err(error) = session::handshake(&rpc, !new_session).await {
        rpc.close().await;
        return Err(error.into());
    }
    let cfg = session::SessionConfig {
        resume: config.resume,
        model: config.model,
        effort: config.effort,
        system_prompt_append: config.system_prompt_append,
        cwd,
        mcp_server_names: config.mcp_servers.keys().cloned().collect(),
        mcp_servers: servers,
    };
    let (output_writer, mut output_reader) = mpsc::unbounded_channel::<Value>();
    let mut output = tokio::spawn(async move {
        while let Some(event) = output_reader.recv().await {
            if new_session && event["type"] == "system" && event["subtype"] == "init" {
                log.bind_session(
                    event["session_id"]
                        .as_str()
                        .context("agy init has no session id")?,
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
            written.context("joining agy event writer")??;
            anyhow::bail!("agy event writer stopped before the session ended");
        }
    };
    rpc.close().await;
    output.await.context("joining agy event writer")??;
    outcome
}
