//! `fleetd` entry point.
//!
//! Deliberately tiny: resolve paths, load or create the shared token, bind the
//! socket, serve until a termination signal, then SIGTERM the children on the
//! way out (fleetd's own restart killing its children is an accepted v1
//! limitation, so the least we can do is make it orderly).

use std::net::SocketAddr;
use std::path::PathBuf;

use fleetd::paths::{FleetdPaths, default_state_dir};
use fleetd::server::{Fleetd, bind_listener, bind_tcp_listener, build_identity, serve, serve_tcp};

const USAGE: &str = "\
fleetd - the per-machine blackbox fleet supervisor

USAGE:
    fleetd [--state-dir <path>] [--listen-tcp <ip:port> [--allow-nonloopback-tcp]]

OPTIONS:
    --state-dir <path>  Directory holding fleetd.sock and fleetd.token.
                        Defaults to $BLACKBOX_STATE_DIR, else
                        $XDG_STATE_HOME/blackbox, else ~/.local/state/blackbox.
    --listen-tcp <addr> Optional TCP owner listener. Loopback is allowed for
                        local tunnels. A non-loopback address also requires
                        --allow-nonloopback-tcp and MUST be protected by an
                        encrypted, ACL-restricted transport such as a tailnet.
    --allow-nonloopback-tcp
                        Explicitly allow --listen-tcp on a non-loopback IP.
    -h, --help          Print this help.
    -V, --version       Print version and build id.
";

struct Options {
    state_dir: Option<PathBuf>,
    listen_tcp: Option<SocketAddr>,
    allow_nonloopback_tcp: bool,
}

fn parse_options() -> anyhow::Result<Options> {
    let mut args = std::env::args().skip(1);
    let mut state_dir = None;
    let mut listen_tcp = std::env::var("BLACKBOX_FLEETD_LISTEN_TCP")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.parse::<SocketAddr>())
        .transpose()
        .map_err(|error| anyhow::anyhow!("invalid BLACKBOX_FLEETD_LISTEN_TCP: {error}"))?;
    let mut allow_nonloopback_tcp = std::env::var("BLACKBOX_FLEETD_ALLOW_NONLOOPBACK_TCP")
        .ok()
        .is_some_and(|value| matches!(value.trim(), "1" | "true" | "yes"));
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--state-dir" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--state-dir requires a path"))?;
                state_dir = Some(PathBuf::from(value));
            }
            "--listen-tcp" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--listen-tcp requires an ip:port"))?;
                listen_tcp = Some(value.parse().map_err(|error| {
                    anyhow::anyhow!("invalid --listen-tcp address `{value}`: {error}")
                })?);
            }
            "--allow-nonloopback-tcp" => allow_nonloopback_tcp = true,
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
    if allow_nonloopback_tcp && listen_tcp.is_none() {
        anyhow::bail!("--allow-nonloopback-tcp requires --listen-tcp");
    }
    Ok(Options {
        state_dir,
        listen_tcp,
        allow_nonloopback_tcp,
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let options = parse_options()?;
    let state_dir = match options.state_dir {
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
    let tcp_listener = match options.listen_tcp {
        Some(address) => Some(bind_tcp_listener(address, options.allow_nonloopback_tcp).await?),
        None => None,
    };
    let state = Fleetd::new(token, build_identity());

    let build = build_identity();
    tracing::info!(
        socket = %paths.socket.display(),
        version = %build.version,
        build_id = %build.build_id,
        "fleetd listening"
    );
    if let Some(listener) = tcp_listener.as_ref() {
        tracing::info!(address = %listener.local_addr()?, "fleetd TCP listener enabled");
    }

    let serving = tokio::spawn(serve(state.clone(), listener));
    let serving_tcp = tcp_listener.map(|listener| tokio::spawn(serve_tcp(state.clone(), listener)));
    wait_for_shutdown().await;

    tracing::info!(
        sessions = state.registry().len(),
        "shutting down; signalling supervised children"
    );
    serving.abort();
    if let Some(serving_tcp) = serving_tcp {
        serving_tcp.abort();
    }
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
