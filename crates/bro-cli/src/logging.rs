//! Cockpit logging.
//!
//! `bro fleet` / `bro agent` own the terminal in raw-mode + alternate-screen,
//! so logging to stderr (the daemon's idiom in `server/startup.rs`) would paint
//! over the TUI. Instead we install a **file-only** rolling subscriber under the
//! cockpit store dir, and a panic hook that first restores the terminal (else a
//! panic leaves the shell wedged in raw/alt-screen) and then records the panic
//! to that log + the now-clean stderr.
//!
//! This is the durable "loud, TUI-safe" channel: the status poller, the reload
//! reconcile pass, and any future cockpit diagnostic call `tracing::*` and land
//! in `<store_dir>/logs/cockpit.<date>.log`. Until this existed, every
//! `tracing::*` in the thin client (e.g. `FleetConfig::load_from`) was dropped
//! on the floor for want of a subscriber.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Once;

static INIT: Once = Once::new();

/// Initialise the cockpit's file-only tracing subscriber and terminal-restoring
/// panic hook. Idempotent (guarded by a `Once`) and best-effort: a logging
/// failure must never stop the cockpit from launching. Returns the log directory
/// when initialised so the caller can surface it on exit.
pub fn init_cockpit_logging(store_dir: &Path) -> Option<PathBuf> {
    let log_dir = store_dir.join("logs");
    let mut out = None;
    INIT.call_once(|| {
        if std::fs::create_dir_all(&log_dir).is_err() {
            return;
        }
        let appender = match tracing_appender::rolling::Builder::new()
            .max_log_files(5)
            .rotation(tracing_appender::rolling::Rotation::DAILY)
            .filename_prefix("cockpit")
            .filename_suffix("log")
            .build(&log_dir)
        {
            Ok(a) => a,
            Err(_) => return,
        };

        // Scope the default to our own crates so dependency chatter (reqwest,
        // hyper, rustls) stays out; RUST_LOG overrides wholesale.
        let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "bro_cli=info,bro_fleet_client=info".into());

        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        // File only — no stderr layer (it would corrupt the alt-screen). ANSI off
        // so the log file is plain text. `try_init` so a second cockpit invocation
        // in the same process (or a test) is a no-op rather than a panic.
        let _ = tracing_subscriber::registry()
            .with(env_filter)
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(appender)
                    .with_ansi(false),
            )
            .try_init();

        install_panic_hook();
        tracing::info!(log_dir = %log_dir.display(), "cockpit logging initialised");
        out = Some(log_dir.clone());
    });
    out
}

/// Wrap the existing panic hook so a panic in the cockpit (TUI thread, a status
/// poller task, anywhere) restores the terminal before the process dies, then
/// records the panic to the log file and the restored stderr. Without this a
/// panic leaves the shell in raw-mode + alternate-screen — the classic TUI
/// wedge that forces a `reset`.
fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        tracing::error!("cockpit PANIC: {info}");
        let _ = writeln!(std::io::stderr(), "\r\n[bro] panic: {info}");
        prev(info);
    }));
}

/// Best-effort terminal restore, idempotent and safe to call when not in raw
/// mode. Mirrors the teardown order in `fleet_tui::run_tui_cockpit`.
fn restore_terminal() {
    use crossterm::event::DisableBracketedPaste;
    use crossterm::terminal::{LeaveAlternateScreen, disable_raw_mode};
    let mut stdout = std::io::stdout();
    let _ = crossterm::execute!(stdout, DisableBracketedPaste, LeaveAlternateScreen);
    let _ = disable_raw_mode();
}
