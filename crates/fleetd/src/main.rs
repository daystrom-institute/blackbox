//! `fleetd` entry point.
//!
//! Deliberately tiny: resolve paths, load or create the shared token, bind the
//! socket, serve until a termination signal, then SIGTERM the children on the
//! way out (fleetd's own restart killing its children is an accepted v1
//! limitation, so the least we can do is make it orderly).

use std::net::SocketAddr;
use std::path::PathBuf;

use fleetd::paths::{FleetdPaths, default_state_dir};
use fleetd::server::{
    Fleetd, TcpBindBackoff, bind_listener, build_identity, resolve_tcp_listen_addresses, serve,
    serve_tcp_retrying, validate_tcp_listen_address_with_tls,
};
use fleetd::tls::{Identity, IdentityPaths, names_for_addresses};

const USAGE: &str = "\
fleetd - the per-machine blackbox fleet supervisor

USAGE:
    fleetd [--state-dir <path>] [--listen-tcp <ip:port>]... [--tls-identity-dir <path>]
           [--allow-nonloopback-tcp]
    fleetd identity init [--state-dir <path>] [--tls-identity-dir <path>] [--listen-tcp <ip:port>]...
    fleetd identity show [--state-dir <path>] [--tls-identity-dir <path>]

OPTIONS:
    --state-dir <path>  Directory holding fleetd.sock and fleetd.token.
                        Defaults to the daemon's BRO home under the same
                        environment: $BRO_HOME, else $BLACKBOX_STATE_DIR/bro,
                        else $XDG_STATE_HOME/blackbox/bro (absolute, not on
                        macOS), else ~/.local/state/blackbox/bro. fleetd reads
                        no daemon config: a daemon whose paths.state_dir or
                        paths.bro_home is set in its config file needs a
                        matching --state-dir here.
    --listen-tcp <addr> Optional TCP owner listener; repeat the flag to serve
                        several addresses. Loopback is allowed for local
                        tunnels. A non-loopback address is served only with
                        a TLS identity (--tls-identity-dir) or, in plaintext,
                        with --allow-nonloopback-tcp behind an encrypted,
                        ACL-restricted transport such as a tailnet. When an
                        address cannot be bound, fleetd keeps serving its
                        Unix socket and every other address, and retries
                        that bind with backoff (1s doubling to 60s). With no
                        flag, BLACKBOX_FLEETD_LISTEN_TCP supplies a
                        comma-separated list.
    --tls-identity-dir <path>
                        Serve every TCP listener over TLS with the identity
                        in this directory (fleetd-tls.crt and fleetd-tls.key,
                        made by `fleetd identity init`). The daemon pins the
                        certificate by the digest `identity init` prints.
                        BLACKBOX_FLEETD_TLS_IDENTITY_DIR supplies it when the
                        flag is absent.
    --allow-nonloopback-tcp
                        Explicitly allow a PLAINTEXT --listen-tcp on a
                        non-loopback IP.
    -h, --help          Print this help.
    -V, --version       Print version and build id.

IDENTITY:
    identity init       Generate the TLS identity (refuses to replace one)
                        and print the certificate digest to pin in the
                        daemon (daemon.fleetd_tls_fingerprint). The
                        certificate names the --listen-tcp addresses given.
    identity show       Print the digest of the existing identity.
";

enum Command {
    Serve(Options),
    IdentityInit {
        identity_dir: Option<PathBuf>,
        state_dir: Option<PathBuf>,
        listen_tcp: Vec<SocketAddr>,
    },
    IdentityShow {
        identity_dir: Option<PathBuf>,
        state_dir: Option<PathBuf>,
    },
}

struct Options {
    state_dir: Option<PathBuf>,
    listen_tcp: Vec<SocketAddr>,
    allow_nonloopback_tcp: bool,
    tls_identity_dir: Option<PathBuf>,
}

fn parse_command() -> anyhow::Result<Command> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let identity = match args.first().map(String::as_str) {
        Some("identity") => {
            let verb = args.get(1).cloned().ok_or_else(|| {
                anyhow::anyhow!("`fleetd identity` requires `init` or `show`\n\n{USAGE}")
            })?;
            args.drain(..2);
            Some(verb)
        }
        _ => None,
    };
    let options = parse_options(args.into_iter())?;
    match identity.as_deref() {
        None => Ok(Command::Serve(options)),
        Some("init") => Ok(Command::IdentityInit {
            identity_dir: options.tls_identity_dir,
            state_dir: options.state_dir,
            listen_tcp: options.listen_tcp,
        }),
        Some("show") => Ok(Command::IdentityShow {
            identity_dir: options.tls_identity_dir,
            state_dir: options.state_dir,
        }),
        Some(other) => anyhow::bail!("unrecognized identity command `{other}`\n\n{USAGE}"),
    }
}

fn parse_options(mut args: impl Iterator<Item = String>) -> anyhow::Result<Options> {
    let mut state_dir = None;
    let mut listen_tcp_flags = Vec::new();
    let mut tls_identity_dir = std::env::var_os("BLACKBOX_FLEETD_TLS_IDENTITY_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
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
                listen_tcp_flags.push(value);
            }
            "--allow-nonloopback-tcp" => allow_nonloopback_tcp = true,
            "--tls-identity-dir" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--tls-identity-dir requires a path"))?;
                tls_identity_dir = Some(PathBuf::from(value));
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
    let listen_tcp = resolve_tcp_listen_addresses(
        std::env::var("BLACKBOX_FLEETD_LISTEN_TCP").ok().as_deref(),
        &listen_tcp_flags,
    )?;
    if allow_nonloopback_tcp && listen_tcp.is_empty() {
        anyhow::bail!("--allow-nonloopback-tcp requires --listen-tcp");
    }
    Ok(Options {
        state_dir,
        listen_tcp,
        allow_nonloopback_tcp,
        tls_identity_dir,
    })
}

/// The identity directory: the flag or environment value, else the state
/// directory, beside the socket and the token.
async fn identity_paths(
    identity_dir: Option<PathBuf>,
    state_dir: Option<PathBuf>,
) -> anyhow::Result<IdentityPaths> {
    let dir = match identity_dir {
        Some(dir) => dir,
        None => match state_dir {
            Some(dir) => dir,
            None => default_state_dir()?,
        },
    };
    tokio::fs::create_dir_all(&dir).await?;
    Ok(IdentityPaths::in_dir(dir))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let options = match parse_command()? {
        Command::Serve(options) => options,
        Command::IdentityInit {
            identity_dir,
            state_dir,
            listen_tcp,
        } => {
            let paths = identity_paths(identity_dir, state_dir).await?;
            let identity = Identity::generate(&paths, &names_for_addresses(&listen_tcp))?;
            println!("certificate: {}", paths.certificate.display());
            println!("private key: {}", paths.private_key.display());
            println!("sha256 fingerprint to pin: {}", identity.fingerprint());
            return Ok(());
        }
        Command::IdentityShow {
            identity_dir,
            state_dir,
        } => {
            let paths = identity_paths(identity_dir, state_dir).await?;
            let identity = Identity::load(&paths)?;
            println!("certificate: {}", paths.certificate.display());
            println!("sha256 fingerprint to pin: {}", identity.fingerprint());
            return Ok(());
        }
    };
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
    // A TCP address the operator has not granted is a configuration error
    // and stops startup. Whether a granted address can be bound right now is
    // not: the listener task retries, and the Unix listener serves meanwhile.
    // A TLS identity is loaded before anything listens: a missing or
    // unreadable identity is a configuration error, like an ungranted address.
    let tls = match &options.tls_identity_dir {
        Some(dir) => {
            let identity = Identity::load(&IdentityPaths::in_dir(dir))?;
            tracing::info!(
                fingerprint = %identity.fingerprint(),
                "fleetd TCP listeners serve TLS with the pinned identity"
            );
            Some(identity.acceptor()?)
        }
        None => None,
    };
    for address in &options.listen_tcp {
        validate_tcp_listen_address_with_tls(
            *address,
            options.allow_nonloopback_tcp,
            tls.is_some(),
        )?;
    }
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
    // One task per address: each binds, retries and serves on its own, so an
    // address that is unavailable never delays or stops another.
    let serving_tcp: Vec<_> = options
        .listen_tcp
        .iter()
        .map(|address| {
            tokio::spawn(serve_tcp_retrying(
                state.clone(),
                *address,
                TcpBindBackoff::default(),
                tls.clone(),
            ))
        })
        .collect();
    wait_for_shutdown().await;

    tracing::info!(
        sessions = state.registry().len(),
        "shutting down; signalling supervised children"
    );
    serving.abort();
    for serving_tcp in serving_tcp {
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
