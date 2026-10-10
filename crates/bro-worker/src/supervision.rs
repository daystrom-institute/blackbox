use std::time::Duration;
use tokio::process::Child;

/// Stop a worker when its adapter or event relay fails. Bound shutdown after
/// the event stream closes and bound the relay drain after process exit.
pub async fn wait_for_child(
    mut child: Child,
    mut adapter: tokio::task::JoinHandle<anyhow::Result<()>>,
    provider: &str,
) -> (Option<i32>, String) {
    tokio::select! {
        outcome = &mut adapter => {
            let mut error = match outcome { Ok(Ok(())) => String::new(), Ok(Err(e)) => format!("{provider} worker: {e:#}"), Err(e) => format!("{provider} worker task: {e}") };
            if !error.is_empty() { let _ = child.start_kill(); }
            let status = match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
                Ok(status) => status.ok().and_then(|s| s.code()),
                Err(_) => {
                    let _ = child.kill().await;
                    if error.is_empty() { error = format!("{provider} worker did not exit after input closed"); }
                    Some(1)
                }
            };
            (if error.is_empty() { status } else { Some(1) }, error)
        }
        status = child.wait() => {
            let error = match tokio::time::timeout(Duration::from_secs(2), &mut adapter).await {
                Ok(Ok(Ok(()))) => String::new(),
                Ok(Ok(Err(e))) => format!("{provider} worker: {e:#}"),
                Ok(Err(e)) => format!("{provider} worker task: {e}"),
                Err(_) => { adapter.abort(); format!("{provider} worker did not drain after process exit") }
            };
            (if error.is_empty() { status.ok().and_then(|s| s.code()) } else { Some(1) }, error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn failed_relay_stops_a_live_worker_and_reports_the_error() {
        let child = tokio::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let worker = tokio::spawn(async { anyhow::bail!("event log write failed") });
        let (code, error) = tokio::time::timeout(
            Duration::from_secs(3),
            wait_for_child(child, worker, "test"),
        )
        .await
        .unwrap();
        assert_eq!(code, Some(1));
        assert!(error.contains("event log write failed"));
    }
}
