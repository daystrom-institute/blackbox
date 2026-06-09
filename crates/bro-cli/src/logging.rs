//! Cockpit logging.
//!
//! `bro fleet` / `bro agent` own the terminal in raw-mode + alternate-screen,
//! so logging to stderr (the daemon's idiom in `server/startup.rs`) would paint
//! over the TUI. Instead we install a **file-only** rolling subscriber under the
//! cockpit store dir, and a panic hook that first restores the terminal (else a
//! panic leaves the shell wedged in raw/alt-screen) and then records the panic
//! to that log + the now-clean stderr.
//!
//! This is the durable "loud, TUI-safe" channel: the roster subscriber and any
//! future cockpit diagnostic call `tracing::*` land in
//! `<store_dir>/logs/cockpit.<date>.log`. Until this existed, every
//! `tracing::*` in the thin client (e.g. `FleetConfig::load_from`) was dropped
//! on the floor for want of a subscriber.

use std::io::Write;
use std::path::Path;
use std::sync::Once;

static PANIC_HOOK: Once = Once::new();

/// Initialise the cockpit's file-only tracing subscriber and terminal-restoring
/// panic hook. Best-effort: a logging failure must never stop the cockpit from
/// launching. Returns a [`WorkerGuard`] the caller MUST hold for the cockpit's
/// lifetime — dropping it stops (and flushes) the non-blocking log worker.
///
/// The writer is `tracing_appender::non_blocking`, NOT a plain blocking file
/// appender: the cockpit emits only a handful of lines per run, which a blocking
/// `BufWriter` keeps buffered until the buffer fills or the process exits — so
/// the log appears effectively write-on-exit and is useless for live tailing
/// while debugging live cockpit state. The non-blocking worker flushes promptly
/// on its own thread.
#[must_use = "hold the returned WorkerGuard for the cockpit's lifetime, or logs are lost"]
pub fn init_cockpit_logging(store_dir: &Path) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let log_dir = store_dir.join("logs");
    if std::fs::create_dir_all(&log_dir).is_err() {
        return None;
    }
    let appender = match tracing_appender::rolling::Builder::new()
        .max_log_files(5)
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("cockpit")
        .filename_suffix("log")
        .build(&log_dir)
    {
        Ok(a) => a,
        Err(_) => return None,
    };
    let (writer, guard) = tracing_appender::non_blocking(appender);

    // Scope the default to our own crates so dependency chatter (reqwest, hyper,
    // rustls) stays out; RUST_LOG overrides wholesale.
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "bro_cli=info,bro_fleet_client=info".into());

    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    // File only — no stderr layer (it would corrupt the alt-screen). ANSI off so
    // the log file is plain text. `try_init` so a second invocation in the same
    // process (or a test) is a no-op rather than a panic.
    let _ = tracing_subscriber::registry()
        .with(env_filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(writer)
                .with_ansi(false),
        )
        .try_init();

    PANIC_HOOK.call_once(install_panic_hook);
    tracing::info!(log_dir = %log_dir.display(), "cockpit logging initialised");
    Some(guard)
}

/// Wrap the existing panic hook so a panic in the cockpit (TUI thread,
/// background subscriber task, anywhere) restores the terminal before the
/// process dies, then records the panic to the log file and the restored stderr. Without this a
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
