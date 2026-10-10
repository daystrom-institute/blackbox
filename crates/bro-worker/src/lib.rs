//! Execution-host provider adapters and durable worker event logging.
pub mod codex;
mod session_log;
pub use session_log::{SessionLogWriter, rfc3339_millis, session_log_record};

mod supervision;
pub use supervision::wait_for_child;

/// Relay CLI events only after their durable copy is written.
pub async fn relay_cli<R, F>(
    mut stdout: R,
    mut log: SessionLogWriter,
    provider: bro_core::Provider,
    events: tokio::sync::mpsc::UnboundedSender<String>,
    mut observe: F,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncBufRead + Unpin,
    F: FnMut(&str),
{
    use tokio::io::AsyncBufReadExt;
    let mut line = String::new();
    loop {
        line.clear();
        if stdout.read_line(&mut line).await? == 0 {
            break;
        }
        observe(line.trim_end());
        let line = log.record_provider_event(provider, line.trim_end()).await?;
        // Execution and durable logging survive loss of the owner connection.
        let _ = events.send(line);
    }
    Ok(())
}
