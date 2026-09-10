//! Internal argv subprocess capture using the shell's existing group owner and
//! supervisor. No shell parsing, model-tool dispatch, or argument policy occurs.

use super::*;

#[derive(Debug, Default)]
pub(crate) struct CommandCapture {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_dropped: u64,
    pub stderr_dropped: u64,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub cancelled: bool,
    pub errors: Vec<String>,
}

impl CommandCapture {
    pub fn complete(&self) -> bool {
        self.stdout_dropped == 0
            && self.stderr_dropped == 0
            && self.errors.is_empty()
            && !self.timed_out
            && !self.cancelled
    }

    pub fn facts(&self) -> Value {
        serde_json::json!({
            "exit_code": self.exit_code, "timed_out": self.timed_out,
            "cancelled": self.cancelled, "stdout_dropped_bytes": self.stdout_dropped,
            "stderr_dropped_bytes": self.stderr_dropped, "reader_errors":self.errors,
        })
    }
}

#[derive(Default)]
struct RawCapture {
    bytes: Vec<u8>,
    dropped: u64,
    error: Option<String>,
    complete: bool,
}

struct RawReaderCompletion(Arc<Mutex<RawCapture>>);
impl Drop for RawReaderCompletion {
    fn drop(&mut self) {
        let mut capture = self.0.lock().unwrap();
        if !capture.complete && capture.error.is_none() {
            capture.error =
                Some("output reader stopped before EOF (drain deadline or cancellation)".into());
        }
    }
}

fn raw_reader<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    mut reader: R,
    capture: Arc<Mutex<RawCapture>>,
    cap: usize,
    newest: bool,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let _completion = RawReaderCompletion(capture.clone());
        let mut bytes = [0u8; 8192];
        loop {
            let count = match reader.read(&mut bytes).await {
                Ok(0) => {
                    capture.lock().unwrap().complete = true;
                    return;
                }
                Ok(count) => count,
                Err(error) => {
                    capture.lock().unwrap().error = Some(error.to_string());
                    return;
                }
            };
            let mut capture = capture.lock().unwrap();
            if newest {
                capture.bytes.extend_from_slice(&bytes[..count]);
                let excess = capture.bytes.len().saturating_sub(cap);
                if excess > 0 {
                    capture.bytes.drain(..excess);
                    capture.dropped += excess as u64;
                }
            } else {
                let keep = count.min(cap.saturating_sub(capture.bytes.len()));
                capture.bytes.extend_from_slice(&bytes[..keep]);
                capture.dropped += (count - keep) as u64;
            }
        }
    })
}

/// Finite argv command with closed stdin, bounded exact byte capture, a passive
/// deadline, cooperative cancellation, and cleanup of its entire owned group.
/// Output readers continue draining on overflow, retaining a stdout prefix and
/// newest stderr tail. Machine parsers must require complete() before use.
pub(crate) async fn run_supervised_command(
    mut command: tokio::process::Command,
    cx: &ToolCx,
    timeout: Duration,
    stdout_cap: usize,
    stderr_cap: usize,
) -> Result<CommandCapture, String> {
    if cx.cancellation.is_cancelled() {
        return Err("command cancelled before spawn".into());
    }
    if timeout.is_zero() {
        return Err("command deadline exhausted before spawn".into());
    }
    let kill_at = Instant::now()
        .checked_add(timeout)
        .ok_or("command deadline overflow")?;
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let child = command
        .spawn()
        .map_err(|error| format!("spawn failed: {error}"))?;
    let pgid = child.id().expect("new command has pid");
    let mut owned = OwnedChild {
        child,
        pgid,
        active: true,
    };
    let stdout = Arc::new(Mutex::new(RawCapture::default()));
    let stderr = Arc::new(Mutex::new(RawCapture::default()));
    let readers = vec![
        raw_reader(
            owned.child.stdout.take().expect("stdout piped"),
            stdout.clone(),
            stdout_cap,
            false,
        ),
        raw_reader(
            owned.child.stderr.take().expect("stderr piped"),
            stderr.clone(),
            stderr_cap,
            true,
        ),
    ];
    let (controls, control_rx) = mpsc::unbounded_channel();
    let (state_tx, mut state) = watch::channel(None);
    let mut guard = InvocationGuard {
        controls: controls.clone(),
        armed: true,
    };
    tokio::spawn(supervise(
        owned,
        control_rx,
        state_tx,
        readers,
        Some(kill_at),
        true,
    ));
    let mut cancellation_sent = false;
    let terminal = loop {
        if let Some(terminal) = state.borrow().clone() {
            break terminal;
        }
        tokio::select! {
            biased;
            _ = cx.cancellation.cancelled(), if !cancellation_sent => {
                cancellation_sent = true;
                let _ = controls.send(Control::Cancel);
            }
            changed = state.changed() => {
                if changed.is_err() { return Err("command supervisor ended without terminal outcome".into()); }
            }
        }
    };
    guard.armed = false;
    let mut stdout = stdout.lock().unwrap();
    let mut stderr = stderr.lock().unwrap();
    let mut errors = Vec::new();
    if let Some(error) = terminal.wait_error {
        errors.push(format!("wait: {error}"));
    }
    if let Some(error) = stdout.error.take() {
        errors.push(format!("stdout: {error}"));
    }
    if let Some(error) = stderr.error.take() {
        errors.push(format!("stderr: {error}"));
    }
    Ok(CommandCapture {
        stdout: std::mem::take(&mut stdout.bytes),
        stderr: std::mem::take(&mut stderr.bytes),
        stdout_dropped: stdout.dropped,
        stderr_dropped: stderr.dropped,
        exit_code: terminal.exit_code,
        timed_out: terminal.timed_out,
        cancelled: terminal.cancelled,
        errors,
    })
}
