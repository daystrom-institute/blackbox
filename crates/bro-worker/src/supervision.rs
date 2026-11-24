use std::time::Duration;
use tokio::process::Child;

/// How long a worker may take to exit once its session has ended and its
/// input is closed.
const EXIT_GRACE: Duration = Duration::from_secs(5);

/// Stop a worker when its adapter or event relay fails. Bound shutdown after
/// the event stream closes and bound the relay drain after process exit.
///
/// Returns the exit code that describes the session and text for the worker's
/// stderr. The text is the failure when the code is nonzero. A session that
/// ended cleanly keeps exit code 0 when its process then has to be killed
/// for outliving [`EXIT_GRACE`]: the adapter returns only after the last
/// turn's result is durable, so the text is a teardown note, not a failure.
pub async fn wait_for_child(
    child: Child,
    adapter: tokio::task::JoinHandle<anyhow::Result<()>>,
    provider: &str,
) -> (Option<i32>, String) {
    wait_for_child_within(child, adapter, provider, EXIT_GRACE).await
}

async fn wait_for_child_within(
    mut child: Child,
    mut adapter: tokio::task::JoinHandle<anyhow::Result<()>>,
    provider: &str,
    exit_grace: Duration,
) -> (Option<i32>, String) {
    tokio::select! {
        outcome = &mut adapter => {
            let error = match outcome { Ok(Ok(())) => String::new(), Ok(Err(e)) => format!("{provider} worker: {e:#}"), Err(e) => format!("{provider} worker task: {e}") };
            if !error.is_empty() { let _ = child.start_kill(); }
            match tokio::time::timeout(exit_grace, child.wait()).await {
                Ok(status) if error.is_empty() => (status.ok().and_then(|s| s.code()), error),
                Ok(_) => (Some(1), error),
                Err(_) if error.is_empty() => {
                    // Name what is still running before the kill hides it.
                    let holders = match child.id() {
                        Some(pid) => live_descendants(pid).await,
                        None => UNLISTED.to_string(),
                    };
                    let _ = child.kill().await;
                    (Some(0), format!("{provider} worker did not exit within {exit_grace:?} after input closed and was killed; {holders}"))
                }
                Err(_) => {
                    let _ = child.kill().await;
                    (Some(1), error)
                }
            }
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

const UNLISTED: &str = "its child processes could not be listed";

/// Describe the processes below `root`, by executable name only: an argument
/// list can carry a credential.
async fn live_descendants(root: u32) -> String {
    let listing = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::process::Command::new("ps")
            .args(["-A", "-o", "pid=", "-o", "ppid=", "-o", "comm="])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await;
    match listing {
        Ok(Ok(output)) if output.status.success() => {
            let names = descendants(&String::from_utf8_lossy(&output.stdout), root);
            if names.is_empty() {
                "it had no live child processes".to_string()
            } else {
                format!("live child processes: {}", names.join(", "))
            }
        }
        _ => UNLISTED.to_string(),
    }
}

/// The `pid name` of every process below `root` in a listing of pid, parent
/// pid and executable per line, parents before their children.
fn descendants(listing: &str, root: u32) -> Vec<String> {
    let processes: Vec<(u32, u32, &str)> = listing
        .lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let (pid, rest) = line.split_once(char::is_whitespace)?;
            let (parent, command) = rest.trim_start().split_once(char::is_whitespace)?;
            let command = command.trim();
            let name = command.rsplit('/').next().unwrap_or(command);
            Some((pid.parse().ok()?, parent.parse().ok()?, name))
        })
        .collect();
    let mut found = Vec::new();
    let mut parents = vec![root];
    while let Some(parent) = parents.pop() {
        for &(pid, of, name) in &processes {
            // Each pid is taken once, so a listing whose links cycle still ends.
            if of == parent && pid != root && !found.iter().any(|(seen, _)| *seen == pid) {
                found.push((pid, name));
                parents.push(pid);
            }
        }
    }
    found
        .into_iter()
        .map(|(pid, name)| format!("{pid} {name}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHORT_GRACE: Duration = Duration::from_millis(200);

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

    #[tokio::test]
    async fn a_finished_session_stays_successful_when_its_worker_has_to_be_killed() {
        let child = tokio::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        let session = tokio::spawn(async { Ok(()) });
        let (code, note) = tokio::time::timeout(
            Duration::from_secs(10),
            wait_for_child_within(child, session, "test", SHORT_GRACE),
        )
        .await
        .unwrap();
        assert_eq!(code, Some(0), "{note}");
        assert!(
            note.starts_with("test worker did not exit within 200ms after input closed"),
            "{note}"
        );
        // The worker is gone: its pid no longer answers a signal probe.
        let probe = tokio::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .unwrap();
        assert!(!probe.success(), "worker {pid} survived");
    }

    #[tokio::test]
    async fn a_killed_worker_names_the_child_processes_it_was_waiting_on() {
        // The grandchild outlives the test, so it must not hold the test's
        // output open.
        let child = tokio::process::Command::new("sh")
            .args(["-c", "sleep 4 & wait"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        // Let the shell start its child before the grace period is judged.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let session = tokio::spawn(async { Ok(()) });
        let (code, note) = tokio::time::timeout(
            Duration::from_secs(10),
            wait_for_child_within(child, session, "test", SHORT_GRACE),
        )
        .await
        .unwrap();
        assert_eq!(code, Some(0), "{note}");
        assert!(note.contains("live child processes: "), "{note}");
        assert!(note.ends_with(" sleep"), "{note}");
    }

    #[tokio::test]
    async fn a_worker_that_exits_on_its_own_keeps_its_exit_code_and_adds_no_note() {
        for (script, expected) in [("exit 0", Some(0)), ("exit 3", Some(3))] {
            let child = tokio::process::Command::new("sh")
                .args(["-c", script])
                .spawn()
                .unwrap();
            let session = tokio::spawn(async { Ok(()) });
            let (code, note) = tokio::time::timeout(
                Duration::from_secs(10),
                wait_for_child_within(child, session, "test", Duration::from_secs(5)),
            )
            .await
            .unwrap();
            assert_eq!(code, expected, "{script}: {note}");
            assert_eq!(note, "", "{script}");
        }
    }

    #[test]
    fn descendants_follow_parent_links_and_drop_paths_and_unrelated_processes() {
        let listing = "\
    1     0 /sbin/launchd
  100     1 /usr/local/bin/codex
  200   100 /opt/homebrew/bin/node
  300   200 /bin/bash
  400     1 /usr/bin/unrelated
  500   100 Application Helper
garbage line
";
        assert_eq!(
            descendants(listing, 100),
            ["200 node", "500 Application Helper", "300 bash"]
        );
        assert!(descendants(listing, 400).is_empty());
        assert!(descendants("", 100).is_empty());
    }

    #[test]
    fn descendants_end_on_a_listing_whose_parent_links_form_a_cycle() {
        let listing = "100 300 a\n200 100 b\n300 200 c\n";
        assert_eq!(descendants(listing, 100), ["200 b", "300 c"]);
    }
}
