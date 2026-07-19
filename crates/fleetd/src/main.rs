//! `fleetd` entry point.
//!
//! Deliberately tiny: resolve paths, load or create the shared token, bind the
//! socket, serve until a termination signal, then SIGTERM the children on the
//! way out (fleetd's own restart killing its children is an accepted v1
//! limitation, so the least we can do is make it orderly).

use std::path::PathBuf;

use fleetd::paths::{FleetdPaths, default_state_dir};
use fleetd::server::{Fleetd, bind_listener, build_identity, serve};

const USAGE: &str = "\
fleetd - the per-machine blackbox fleet supervisor

USAGE:
    fleetd [--state-dir <path>]

OPTIONS:
    --state-dir <path>  Directory holding fleetd.sock and fleetd.token.
                        Defaults to $BLACKBOX_STATE_DIR, else
                        $XDG_STATE_HOME/blackbox, else ~/.local/state/blackbox.
    -h, --help          Print this help.
    -V, --version       Print version and build id.
";

fn parse_state_dir() -> anyhow::Result<Option<PathBuf>> {
    let mut args = std::env::args().skip(1);
    let mut state_dir = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--state-dir" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--state-dir requires a path"))?;
                state_dir = Some(PathBuf::from(value));
            }
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "-V" | "--version" => {
                let build = build_identity();
                println!("fleetd {} ({})", build.version, build.build_id);
                std::process::exit(0);
            }
            other => anyhow::bail!("unrecognized argument `{other}`\n\n{USAGE}"),
        }
    }
    Ok(state_dir)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let state_dir = match parse_state_dir()? {
        Some(dir) => dir,
        None => default_state_dir()?,
    };
    let paths = FleetdPaths::in_state_dir(&state_dir);
    tokio::fs::create_dir_all(&paths.state_dir).await?;

    // Whichever of daemon and fleetd starts first creates the token; the
    // other loads it. Hardening (private, non-symlink, single-hardlink,
    // owner-only) is enforced inside ServiceToken.
    let token = bro_rpc::ServiceToken::load_or_create(&paths.token)?;
    let listener = bind_listener(&paths.socket).await?;
    let state = Fleetd::new(token, build_identity());

    let build = build_identity();
    tracing::info!(
        socket = %paths.socket.display(),
        version = %build.version,
        build_id = %build.build_id,
        "fleetd listening"
    );

    let serving = tokio::spawn(serve(state.clone(), listener));
    wait_for_shutdown().await;

    tracing::info!(
        sessions = state.registry().len(),
        "shutting down; signalling supervised children"
    );
    serving.abort();
    state.registry().kill_all();
    let _ = tokio::fs::remove_file(&paths.socket).await;
    Ok(())
}

async fn wait_for_shutdown() {
    let mut terminate =
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(signal) => signal,
            Err(error) => {
                tracing::warn!(%error, "cannot listen for SIGTERM; waiting on SIGINT only");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
    tokio::select! {
        _ = terminate.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}
