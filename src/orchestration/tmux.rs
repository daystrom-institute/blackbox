//! Minimal tmux backend for terminal-mode actor dispatch (Phase B of the tmux
//! terminal-mode slice).
//!
//! This is the narrow subset of tmux control needed to launch a provider TUI in
//! a pane, type a prompt into it, check liveness, and tear it down. It is **not**
//! the portal apparatus: `link-window`/`unlink-window`, focus/overview, layout,
//! and zoom are deliberately omitted and tracked by
//! `design/orchestration/workflows/tmux-portal-workflows-impl.md`.
//!
//! Invariants (see `design/orchestration/workflows/tmux-terminal-mode-slice.md`):
//! - `capture_pane` is for process liveness only; its text is never parsed as
//!   node output. Completion is driven by the transcript read plane.
//! - Commands use fixed argv via `tokio::process::Command` — never a shell
//!   string — so a prompt body sent with `send_text` can never be interpreted
//!   as shell or as tmux key names (`send-keys -l` sends literally).
//
// The backend is landed ahead of its consumer: Phase C (terminal-mode dispatch)
// wires `TmuxBackend` into the dispatch path. Until then the surface is only
// exercised by tests, so allow dead_code module-wide; remove this when Phase C
// lands.
#![allow(dead_code)]

use async_trait::async_trait;
use tokio::process::Command;

/// Handle to a tmux window hosting an actor TUI.
///
/// Stores the `(session, window_id, pane_id)` triple — never `window_id` alone.
/// If portal linked-window projection is added later, a linked window shares its
/// `@id` across sessions, so ownership is only well-defined by the
/// session/window pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxHandle {
    /// Container session name, e.g. `bb-actors-<arc_id>`.
    pub session: String,
    /// tmux window id, e.g. `@7`. Globally unique within the server.
    pub window_id: String,
    /// tmux pane id, e.g. `%9`. Globally unique within the server.
    pub pane_id: String,
}

/// Result of `ensure_session`: the canonical session name plus whether it was
/// already present (vs. freshly created).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxSession {
    pub name: String,
    pub existed: bool,
}

/// Errors from tmux control operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TmuxError {
    /// The tmux binary is not available / not runnable. Terminal mode fails
    /// closed on this (cutover rule #6: headless workflows keep working
    /// without tmux installed).
    Unavailable,
    /// A tmux subcommand exited non-zero.
    Command {
        argv: Vec<String>,
        status: Option<i32>,
        stderr: String,
    },
    /// tmux output could not be parsed into a handle.
    Parse(String),
    /// The tmux process could not be spawned / its output read.
    Io(String),
}

impl std::fmt::Display for TmuxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TmuxError::Unavailable => write!(f, "tmux is not available"),
            TmuxError::Command {
                argv,
                status,
                stderr,
            } => write!(
                f,
                "tmux {} exited {}: {}",
                argv.first().map(String::as_str).unwrap_or("?"),
                status
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".into()),
                stderr.trim()
            ),
            TmuxError::Parse(s) => write!(f, "tmux output parse error: {s}"),
            TmuxError::Io(s) => write!(f, "tmux spawn/io error: {s}"),
        }
    }
}

impl std::error::Error for TmuxError {}

/// Narrow tmux control surface used by terminal-mode dispatch.
#[async_trait]
pub trait TmuxBackend: Send + Sync {
    /// Whether the tmux binary can be run at all.
    async fn tmux_available(&self) -> bool;
    /// Create the container session if absent; report whether it pre-existed.
    async fn ensure_session(&self, name: &str) -> Result<TmuxSession, TmuxError>;
    /// Create a new window in `session` running `command` (fixed argv), and
    /// return its window/pane handle.
    async fn create_window(
        &self,
        session: &str,
        name: &str,
        command: &[String],
    ) -> Result<TmuxHandle, TmuxError>;
    /// Type `text` literally into the pane (no Enter, no key interpretation).
    async fn send_text(&self, pane_id: &str, text: &str) -> Result<(), TmuxError>;
    /// Send a single Enter keypress to the pane.
    async fn send_enter(&self, pane_id: &str) -> Result<(), TmuxError>;
    /// Capture the last `lines` lines of the pane for liveness only. Never
    /// parsed as node output.
    async fn capture_pane(&self, pane_id: &str, lines: usize) -> Result<String, TmuxError>;
    /// Kill the window identified by `handle` (tears down the actor TUI).
    async fn kill_window(&self, handle: &TmuxHandle) -> Result<(), TmuxError>;
}

/// Deterministic container-session name for an arc's actor TUIs.
///
/// The slice design names this `bb-actors:<arc_id>`, but tmux reserves `:` as
/// the `session:window` target separator, so a literal colon in a session name
/// breaks every later `-t` reference. We sanitize to `bb-actors-<arc_id>` and
/// replace any `:`/`.`/whitespace in the arc id with `-`.
pub fn container_session_name(arc_id: &str) -> String {
    let sanitized: String = arc_id
        .chars()
        .map(|c| if matches!(c, ':' | '.' | ' ' | '\t') { '-' } else { c })
        .collect();
    format!("bb-actors-{sanitized}")
}

fn tmux_bin() -> String {
    std::env::var("BLACKBOX_TMUX_BIN").unwrap_or_else(|_| "tmux".to_string())
}

// ---- pure argv builders (unit-tested without spawning tmux) ----

pub(crate) fn version_argv() -> Vec<String> {
    vec!["-V".into()]
}

pub(crate) fn has_session_argv(session: &str) -> Vec<String> {
    vec!["has-session".into(), "-t".into(), session.into()]
}

pub(crate) fn new_session_argv(session: &str) -> Vec<String> {
    vec!["new-session".into(), "-d".into(), "-s".into(), session.into()]
}

/// Output format that lets us parse the created window/pane back out of
/// `new-window -P -F ...`. Session name is colon-free by construction
/// (`container_session_name`), and window/pane ids never contain `:`.
const NEW_WINDOW_FORMAT: &str = "#{session_name}:#{window_id}:#{pane_id}";

pub(crate) fn new_window_argv(session: &str, name: &str, command: &[String]) -> Vec<String> {
    let mut argv = vec![
        "new-window".into(),
        "-d".into(),
        "-P".into(),
        "-F".into(),
        NEW_WINDOW_FORMAT.into(),
        "-t".into(),
        format!("{session}:"),
        "-n".into(),
        name.into(),
    ];
    if !command.is_empty() {
        argv.push("--".into());
        argv.extend(command.iter().cloned());
    }
    argv
}

pub(crate) fn send_text_argv(pane_id: &str, text: &str) -> Vec<String> {
    // `-l` sends the literal string: it is not interpreted as tmux key names,
    // and because it is a single argv element it cannot be re-split or shell
    // expanded.
    vec![
        "send-keys".into(),
        "-t".into(),
        pane_id.into(),
        "-l".into(),
        text.into(),
    ]
}

pub(crate) fn send_enter_argv(pane_id: &str) -> Vec<String> {
    vec![
        "send-keys".into(),
        "-t".into(),
        pane_id.into(),
        "Enter".into(),
    ]
}

pub(crate) fn capture_pane_argv(pane_id: &str, lines: usize) -> Vec<String> {
    vec![
        "capture-pane".into(),
        "-p".into(),
        "-t".into(),
        pane_id.into(),
        "-S".into(),
        format!("-{lines}"),
    ]
}

pub(crate) fn kill_window_argv(window_id: &str) -> Vec<String> {
    vec!["kill-window".into(), "-t".into(), window_id.into()]
}

/// Parse the `new-window -P -F` line `session:@window:%pane` into a handle.
pub(crate) fn parse_new_window_output(out: &str) -> Result<TmuxHandle, TmuxError> {
    let line = out.lines().next().unwrap_or("").trim();
    let parts: Vec<&str> = line.split(':').collect();
    if parts.len() != 3 || parts.iter().any(|p| p.is_empty()) {
        return Err(TmuxError::Parse(format!(
            "expected 'session:@window:%pane', got '{line}'"
        )));
    }
    Ok(TmuxHandle {
        session: parts[0].to_string(),
        window_id: parts[1].to_string(),
        pane_id: parts[2].to_string(),
    })
}

/// Real backend: invokes the host `tmux` binary directly.
#[derive(Debug, Clone, Default)]
pub struct CliTmuxBackend;

impl CliTmuxBackend {
    pub fn new() -> Self {
        Self
    }

    async fn run(&self, argv: &[String]) -> Result<String, TmuxError> {
        let output = Command::new(tmux_bin())
            .args(argv)
            .output()
            .await
            .map_err(|e| TmuxError::Io(e.to_string()))?;
        if !output.status.success() {
            return Err(TmuxError::Command {
                argv: argv.to_vec(),
                status: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

#[async_trait]
impl TmuxBackend for CliTmuxBackend {
    async fn tmux_available(&self) -> bool {
        self.run(&version_argv()).await.is_ok()
    }

    async fn ensure_session(&self, name: &str) -> Result<TmuxSession, TmuxError> {
        if self.run(&has_session_argv(name)).await.is_ok() {
            return Ok(TmuxSession {
                name: name.to_string(),
                existed: true,
            });
        }
        self.run(&new_session_argv(name)).await?;
        Ok(TmuxSession {
            name: name.to_string(),
            existed: false,
        })
    }

    async fn create_window(
        &self,
        session: &str,
        name: &str,
        command: &[String],
    ) -> Result<TmuxHandle, TmuxError> {
        let out = self
            .run(&new_window_argv(session, name, command))
            .await?;
        parse_new_window_output(&out)
    }

    async fn send_text(&self, pane_id: &str, text: &str) -> Result<(), TmuxError> {
        self.run(&send_text_argv(pane_id, text)).await.map(|_| ())
    }

    async fn send_enter(&self, pane_id: &str) -> Result<(), TmuxError> {
        self.run(&send_enter_argv(pane_id)).await.map(|_| ())
    }

    async fn capture_pane(&self, pane_id: &str, lines: usize) -> Result<String, TmuxError> {
        self.run(&capture_pane_argv(pane_id, lines)).await
    }

    async fn kill_window(&self, handle: &TmuxHandle) -> Result<(), TmuxError> {
        self.run(&kill_window_argv(&handle.window_id))
            .await
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn container_session_name_sanitizes_colons_and_dots() {
        assert_eq!(container_session_name("arc-123"), "bb-actors-arc-123");
        // Colons (the tmux target separator) and dots must not survive.
        assert_eq!(
            container_session_name("arc:42.7"),
            "bb-actors-arc-42-7"
        );
        assert!(!container_session_name("a:b.c d").contains(':'));
    }

    #[test]
    fn send_text_uses_literal_flag_and_single_arg() {
        let argv = send_text_argv("%9", "rm -rf / ; echo $(whoami)");
        // The dangerous-looking text is one argv element after `-l`, never a
        // shell string and never split.
        assert_eq!(argv[0], "send-keys");
        assert!(argv.contains(&"-l".to_string()), "{argv:?}");
        assert_eq!(argv.last().unwrap(), "rm -rf / ; echo $(whoami)");
        assert_eq!(argv.iter().filter(|a| a.contains("whoami")).count(), 1);
    }

    #[test]
    fn send_enter_is_a_key_name_not_literal() {
        let argv = send_enter_argv("%9");
        assert_eq!(argv, vec!["send-keys", "-t", "%9", "Enter"]);
        assert!(!argv.contains(&"-l".to_string()));
    }

    #[test]
    fn new_window_argv_includes_format_and_command() {
        let argv = new_window_argv("bb-actors-arc1", "implementer", &["codex".into(), "--no-alt-screen".into()]);
        assert!(argv.contains(&"-P".to_string()));
        assert!(argv.contains(&NEW_WINDOW_FORMAT.to_string()));
        assert!(argv.contains(&"bb-actors-arc1:".to_string()));
        // Command follows a `--` separator.
        let sep = argv.iter().position(|a| a == "--").expect("-- present");
        assert_eq!(&argv[sep + 1..], &["codex", "--no-alt-screen"]);
    }

    #[test]
    fn new_window_argv_omits_separator_when_no_command() {
        let argv = new_window_argv("s", "w", &[]);
        assert!(!argv.contains(&"--".to_string()));
    }

    #[test]
    fn capture_pane_targets_pane_and_bounds_lines() {
        let argv = capture_pane_argv("%2", 80);
        assert_eq!(argv, vec!["capture-pane", "-p", "-t", "%2", "-S", "-80"]);
    }

    #[test]
    fn parse_new_window_output_happy_path() {
        let h = parse_new_window_output("bb-actors-arc1:@7:%9\n").unwrap();
        assert_eq!(h.session, "bb-actors-arc1");
        assert_eq!(h.window_id, "@7");
        assert_eq!(h.pane_id, "%9");
    }

    #[test]
    fn parse_new_window_output_rejects_malformed() {
        assert!(parse_new_window_output("garbage").is_err());
        assert!(parse_new_window_output("a:b").is_err());
        assert!(parse_new_window_output(":@7:%9").is_err());
    }

    /// In-memory fake backend: records argv-equivalent calls and serves
    /// scripted handles. Lets dispatch tests run without a live tmux.
    #[derive(Default)]
    pub struct FakeTmuxBackend {
        pub available: bool,
        pub existing_sessions: Vec<String>,
        pub calls: Mutex<Vec<String>>,
        pub next_handle: Mutex<Option<TmuxHandle>>,
    }

    impl FakeTmuxBackend {
        fn record(&self, s: impl Into<String>) {
            self.calls.lock().unwrap().push(s.into());
        }
        pub fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl TmuxBackend for FakeTmuxBackend {
        async fn tmux_available(&self) -> bool {
            self.available
        }
        async fn ensure_session(&self, name: &str) -> Result<TmuxSession, TmuxError> {
            let existed = self.existing_sessions.iter().any(|s| s == name);
            self.record(format!("ensure_session:{name}:existed={existed}"));
            Ok(TmuxSession {
                name: name.to_string(),
                existed,
            })
        }
        async fn create_window(
            &self,
            session: &str,
            name: &str,
            command: &[String],
        ) -> Result<TmuxHandle, TmuxError> {
            self.record(format!("create_window:{session}:{name}:{}", command.join(" ")));
            self.next_handle
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| TmuxError::Parse("no scripted handle".into()))
        }
        async fn send_text(&self, pane_id: &str, text: &str) -> Result<(), TmuxError> {
            self.record(format!("send_text:{pane_id}:{text}"));
            Ok(())
        }
        async fn send_enter(&self, pane_id: &str) -> Result<(), TmuxError> {
            self.record(format!("send_enter:{pane_id}"));
            Ok(())
        }
        async fn capture_pane(&self, pane_id: &str, lines: usize) -> Result<String, TmuxError> {
            self.record(format!("capture_pane:{pane_id}:{lines}"));
            Ok(String::new())
        }
        async fn kill_window(&self, handle: &TmuxHandle) -> Result<(), TmuxError> {
            self.record(format!("kill_window:{}", handle.window_id));
            Ok(())
        }
    }

    #[tokio::test]
    async fn fake_backend_dispatch_lifecycle() {
        let session = container_session_name("arc-xyz");
        let fake = FakeTmuxBackend {
            available: true,
            next_handle: Mutex::new(Some(TmuxHandle {
                session: session.clone(),
                window_id: "@1".into(),
                pane_id: "%1".into(),
            })),
            ..Default::default()
        };

        assert!(fake.tmux_available().await);
        let sess = fake.ensure_session(&session).await.unwrap();
        assert!(!sess.existed);
        let handle = fake
            .create_window(&session, "implementer", &["codex".into()])
            .await
            .unwrap();
        assert_eq!(handle.pane_id, "%1");
        fake.send_text(&handle.pane_id, "do the thing").await.unwrap();
        fake.send_enter(&handle.pane_id).await.unwrap();
        let _ = fake.capture_pane(&handle.pane_id, 40).await.unwrap();
        fake.kill_window(&handle).await.unwrap();

        let calls = fake.calls();
        assert_eq!(calls.first().unwrap(), &format!("ensure_session:{session}:existed=false"));
        assert_eq!(calls.last().unwrap(), "kill_window:@1");
        assert!(calls.iter().any(|c| c == "send_text:%1:do the thing"));
        assert!(calls.iter().any(|c| c == "send_enter:%1"));
    }
}
