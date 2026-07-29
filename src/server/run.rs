use super::background::start_background_tasks;
use super::instance_lock::{InstanceLockSet, InstanceRoot, acquire_instance_locks};
use super::mcp::build_http_app;
use super::open::open_shared_state;
use super::shutdown::serve_until_shutdown;
use super::startup::init_logging;
use crate::util;
use std::path::Path;
use tokio_util::sync::CancellationToken;

/// Claim every root this daemon will write to, then run the one-time legacy
/// migrations.
///
/// R32F2: the order is the point. `migrate_legacy_defaults` renames the
/// knowledge, threads, notes, index, and bro state into place, falling back to
/// a create-plus-copy across devices, and the rolling file logger creates
/// directories and opens a shared log. Both used to run before any claim
/// existed, so two upgrade startups could race the same one-time move. A
/// process that loses the claim now returns from here having renamed,
/// copied, and opened nothing.
fn claim_roots_then_migrate(
    home: &Path,
    roots: &[InstanceRoot],
) -> anyhow::Result<(InstanceLockSet, Vec<String>)> {
    let locks = acquire_instance_locks(roots)?;
    let migrated = util::migrate_legacy_defaults(home)?;
    Ok((locks, migrated))
}

pub async fn run() -> anyhow::Result<()> {
    let home = dirs::home_dir().expect("cannot determine home directory");
    // Load once: the claim below and the store opens further down must agree
    // on which roots this daemon owns, and a reload between them could not
    // guarantee that.
    let loaded = crate::config::load()?;
    let roots = super::instance_lock::instance_lock_roots(&loaded);
    let (instance_locks, migrated) = claim_roots_then_migrate(&home, &roots)?;
    init_logging(&home, migrated);

    let opened = open_shared_state(&home, loaded, &instance_locks)?;
    // R31F1: the single-writer claim outlives every store opened under it.
    // Binding it here keeps it held until `run` returns; process exit by any
    // other route releases it with the file descriptions.
    let _instance_locks = instance_locks;
    let cfg = opened.cfg;
    let shared = opened.shared;
    let store_dir = opened.store_dir;
    let bind_host = opened.bind_host;
    let bind_is_loopback = opened.bind_is_loopback;
    // Select the harness executor BEFORE anything can dispatch. Default is
    // fleetd, so a worker outlives this daemon process; `BLACKBOX_EXECUTOR=local`
    // (or `daemon.executor = "local"`) is the explicit escape back to
    // daemon-child workers. Installing here also arms fleetd re-adoption: the
    // first connection re-attaches whatever survived our restart.
    crate::orchestration::install_harness_executor(
        cfg.daemon.executor,
        store_dir.clone(),
        shared.task_store.clone(),
        shared.tail_tx.clone(),
        Some(shared.system_events.clone()),
    );

    // Bind before starting any background work that probes registered project
    // paths. A stalled mount must not prevent the daemon from claiming its
    // listener; the initial lifecycle pass itself is spawned below.
    let port = cfg.daemon.port;
    let listener = tokio::net::TcpListener::bind(format!("{bind_host}:{port}")).await?;

    start_background_tasks(shared.clone()).await?;

    // MCP service
    let ct = CancellationToken::new();
    let app = build_http_app(shared.clone(), &cfg, &ct);

    // Bind address resolved above (hoisted so SharedState gets the
    // loopback flag). Default `127.0.0.1`; BBOX_BIND=0.0.0.0 opens
    // the listener to docker-bridged peers — closed-network only.
    tracing::info!(
        "blackboxd listening on http://{bind_host}:{port}/mcp (loopback={bind_is_loopback})"
    );

    serve_until_shutdown(
        listener,
        app,
        shared,
        store_dir,
        ct,
        cfg.daemon.shutdown_grace_secs,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The R32F2 regression. Two upgrade startups race the same one-time
    /// legacy migration: the loser must return from the claim having renamed,
    /// copied, and created nothing, so the winner's move is the only one.
    ///
    /// The log assertion is the same ordering claim: `init_logging` runs
    /// downstream of this call in `run`, and it creates `<state>/logs` plus a
    /// rolling log file the moment it does.
    #[test]
    fn a_refused_claim_migrates_nothing_and_opens_no_log() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonicalize the fixture");
        let home = root.join("home");
        let state = root.join("state");

        let mut env = crate::util::TestEnvGuard::new();
        env.set("BLACKBOX_STATE_DIR", &state);
        // Keep every home-derived migration destination inside the fixture.
        env.set("TRANSCRIPT_SEARCH_INDEX_PATH", root.join("index"));
        for var in [
            "BLACKBOX_KNOWLEDGE_PATH",
            "BLACKBOX_THREADS_PATH",
            "BLACKBOX_NOTES_PATH",
            "BLACKBOX_GLOBAL_COMMON_MD",
            "BRO_HOME",
        ] {
            env.remove(var);
        }

        // The legacy tree an upgrading daemon finds.
        let legacy = home.join(".claude-shared");
        std::fs::create_dir_all(&legacy).expect("create the legacy tree");
        std::fs::write(legacy.join("blackbox-knowledge.json"), b"[]").expect("legacy knowledge");
        std::fs::write(legacy.join("blackbox-threads.json"), b"[]").expect("legacy threads");
        std::fs::create_dir_all(home.join(".bro")).expect("create the legacy bro home");
        std::fs::write(home.join(".bro").join("tasks.json"), b"[]").expect("legacy bro state");

        let roots = vec![InstanceRoot::state_root(state.clone())];
        let live = acquire_instance_locks(&roots).expect("the healthy daemon claims its roots");

        let error =
            claim_roots_then_migrate(&home, &roots).expect_err("the duplicate startup must refuse");
        assert!(
            error.to_string().contains("already holds"),
            "the refusal must name the contention: {error:#}"
        );

        // Nothing moved: the legacy sources are intact and no destination was
        // created.
        assert!(legacy.join("blackbox-knowledge.json").exists());
        assert!(legacy.join("blackbox-threads.json").exists());
        assert!(home.join(".bro").join("tasks.json").exists());
        assert!(!state.join("blackbox-knowledge.json").exists());
        assert!(!state.join("blackbox-threads.json").exists());
        assert!(!state.join("bro").exists());
        assert!(
            !crate::util::blackbox_log_dir(&home).exists(),
            "the refused startup must not reach file logging"
        );
        let residue: Vec<String> = std::fs::read_dir(&state)
            .expect("the claim created the state root")
            .map(|entry| {
                entry
                    .expect("read the state root")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(
            residue,
            vec!["instance.lock".to_string()],
            "the refused startup leaves only the lock file it had to open"
        );

        // The winner exits; the next startup performs the migration exactly
        // once, which is what the refusal above deferred rather than lost.
        drop(live);
        let (_locks, migrated) =
            claim_roots_then_migrate(&home, &roots).expect("a released root is claimable again");
        assert!(
            migrated.iter().any(|line| line.starts_with("knowledge:")),
            "the migration runs under the claim: {migrated:?}"
        );
        assert!(state.join("blackbox-knowledge.json").exists());
        assert!(state.join("bro").join("tasks.json").exists());
    }
}
