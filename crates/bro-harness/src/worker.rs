//! Additive worker-protocol bootstrap and executable probe.
//!
//! Production session ownership is introduced in later milestones. The probe
//! exists now so protocol, identity, version skew, and binary launch can be
//! verified before any authority moves.

pub mod capability_rpc;
mod command_journal;
mod supervisor;

pub use supervisor::run_worker;

use anyhow::{Context, Result, bail};
use bro_core::{CommandId, SessionId, TaskId, WorkerId};
use bro_protocol::{
    AuthenticationProof, BuildIdentity, CommandOutcome, Envelope, WorkerCommandKind, WorkerEvent,
    WorkerHello, WorkerLifecycleState, WorkerMessage, WorkerStatus,
};
use serde_json::json;
use tokio::net::UnixStream;
use uuid::Uuid;

use crate::cli::Cli;

fn required(value: &Option<String>, flag: &str) -> Result<String> {
    value
        .as_ref()
        .filter(|value| !value.is_empty())
        .cloned()
        .with_context(|| format!("{flag} is required in worker mode"))
}

pub async fn run_probe(cli: &Cli) -> Result<()> {
    let socket = required(&cli.fleet_socket, "--fleet-socket")?;
    let task_id = TaskId::new(required(&cli.task_id, "--task-id")?);
    let session_id = SessionId::new(required(&cli.session_id, "--session-id")?);
    let bootstrap_path = required(&cli.bootstrap_secret_file, "--bootstrap-secret-file")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = tokio::fs::metadata(&bootstrap_path)
            .await
            .with_context(|| format!("reading bootstrap file metadata {bootstrap_path}"))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            bail!("bootstrap secret file must not be accessible by group or other users");
        }
    }
    let bootstrap = tokio::fs::read_to_string(&bootstrap_path)
        .await
        .with_context(|| format!("reading bootstrap secret file {bootstrap_path}"))?;
    let bootstrap = bootstrap.trim_end().to_string();
    if bootstrap.is_empty() {
        bail!("bootstrap secret file is empty");
    }
    if cli.worker_protocol_versions.is_empty() {
        bail!("--worker-protocol-versions must advertise at least one version");
    }

    let hello = WorkerHello {
        protocol_versions: cli.worker_protocol_versions.clone(),
        worker_build: BuildIdentity {
            version: env!("CARGO_PKG_VERSION").to_string(),
            build_id: env!("BRO_HARNESS_BUILD_ID").to_string(),
        },
        worker_id: WorkerId::new(
            cli.worker_id
                .clone()
                .unwrap_or_else(|| format!("worker-{}", Uuid::new_v4())),
        ),
        task_id,
        session_id,
        bootstrap_or_resume_proof: AuthenticationProof::new(bootstrap),
        last_local_event_seq: 0,
        last_fleet_command_seq: 0,
        worker_capabilities: vec!["protocol_probe".to_string()],
    };

    let stream = UnixStream::connect(&socket)
        .await
        .with_context(|| format!("connecting to fleet worker socket {socket}"))?;
    let (mut framed, welcome) = bro_rpc::connect_worker(stream, hello.clone()).await?;
    let generation = welcome.connection_generation;
    let protocol = welcome.selected_protocol;

    framed
        .write_json(&Envelope {
            protocol_version: protocol,
            connection_generation: generation,
            message_id: "probe-event-1".to_string(),
            reply_to: None,
            body: WorkerMessage::Event(WorkerEvent {
                event_seq: 1,
                occurred_at_unix_ms: 0,
                event: json!({"type": "worker_probe_ready"}),
            }),
        })
        .await?;

    let mut event_acked = false;
    loop {
        let envelope: Envelope = framed.read_json().await?;
        if envelope.protocol_version != protocol || envelope.connection_generation != generation {
            bail!("received stale or incompatible probe envelope");
        }
        match envelope.body {
            WorkerMessage::EventAck(ack) => {
                if ack.through_event_seq != 1 {
                    bail!("probe received an invalid event acknowledgement");
                }
                event_acked = true;
                continue;
            }
            WorkerMessage::Heartbeat(_) => continue,
            WorkerMessage::Command(command) => {
                if !event_acked {
                    bail!("probe received a command before its event was acknowledged");
                }
                if !matches!(command.command, WorkerCommandKind::RequestStatus) {
                    bail!("probe expected request_status command");
                }
                let request_message_id = envelope.message_id;
                framed
                    .write_json(&Envelope {
                        protocol_version: protocol,
                        connection_generation: generation,
                        message_id: "probe-command-outcome".to_string(),
                        reply_to: Some(request_message_id.clone()),
                        body: WorkerMessage::CommandOutcome(CommandOutcome {
                            command_seq: command.command_seq,
                            command_id: CommandId::new(command.command_id.as_str()),
                            accepted: true,
                            terminal: true,
                            result_or_error: json!({"state": "probe_complete"}),
                        }),
                    })
                    .await?;
                framed
                    .write_json(&Envelope {
                        protocol_version: protocol,
                        connection_generation: generation,
                        message_id: "probe-status".to_string(),
                        reply_to: Some(request_message_id),
                        body: WorkerMessage::Status(WorkerStatus {
                            worker_id: hello.worker_id,
                            task_id: hello.task_id,
                            session_id: hello.session_id,
                            worker_build: hello.worker_build,
                            protocol_version: protocol,
                            connection_generation: generation,
                            last_local_event_seq: 1,
                            last_fleet_command_seq: command.command_seq,
                            state: WorkerLifecycleState::Terminal,
                        }),
                    })
                    .await?;
                return Ok(());
            }
            _ => bail!("probe received an unexpected worker message"),
        }
    }
}
