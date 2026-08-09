use super::background::start_background_tasks;
use super::instance_lock::{InstanceLockSet, InstanceRoot, acquire_instance_locks};
use super::mcp::build_http_app;
use super::open::open_shared_state;
use super::shutdown::serve_until_shutdown;
use super::startup::init_logging;
use crate::util;
use std::path::Path;
use tokio_util::sync::CancellationToken;

/// Where the one-time legacy migration puts each store: this daemon's
/// RESOLVED paths.
///
/// R33F2: the migration used to recompute its destinations from env vars and
/// `$HOME` defaults, which never consult the config file. A daemon isolated
/// through `[paths].state_dir` alone therefore renamed shared legacy state
/// into the production default paths — roots it had not claimed and does not
/// serve — racing whichever daemon owns them.
fn legacy_destinations(cfg: &crate::config::Config) -> util::LegacyMigrationDestinations {
    util::LegacyMigrationDestinations {
        knowledge_path: cfg.paths.knowledge_path.clone(),
        threads_path: cfg.paths.threads_path.clone(),
        notes_path: cfg.paths.notes_path.clone(),
        index_path: cfg.paths.index_path.clone(),
        global_common_md: cfg.paths.global_common_md.clone(),
        bro_home: cfg.paths.bro_home.clone(),
    }
}

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
///
/// R33F2 closes the other half: the destinations come from `cfg`, so every
/// one of them sits inside a root the claim above covers, and the migration
/// takes its own non-blocking claim on the legacy SOURCE (which hangs off
/// `$HOME` and belongs to no daemon) so two daemons with distinct roots still
/// cannot race the one-time move.
fn claim_roots_then_migrate(
    home: &Path,
    cfg: &crate::config::Config,
    roots: &[InstanceRoot],
) -> anyhow::Result<(InstanceLockSet, Vec<String>)> {
    let locks = acquire_instance_locks(roots)?;
    let migrated = util::migrate_legacy_defaults(home, &legacy_destinations(cfg))?;
    Ok((locks, migrated))
}

pub async fn run() -> anyhow::Result<()> {
    let home = dirs::home_dir().expect("cannot determine home directory");
    // Load once: the claim below and the store opens further down must agree
    // on which roots this daemon owns, and a reload between them could not
    // guarantee that.
    let loaded = crate::config::load()?;
    let roots = super::instance_lock::instance_lock_roots(&loaded);
    let (instance_locks, migrated) = claim_roots_then_migrate(&home, &loaded, &roots)?;
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
    crate::orchestration::install_configured_harness_executor(
        cfg.daemon.executor,
        store_dir.clone(),
        cfg.daemon.fleetd_endpoint.as_deref(),
        cfg.daemon.fleetd_token_file.as_deref(),
        shared.task_store.clone(),
        shared.tail_tx.clone(),
        Some(shared.system_events.clone()),
        Some(std::sync::Arc::new(
            crate::server::knowledge_source::DaemonWorkspaceBindingAuthority::new(shared.clone()),
        )),
    )?;

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
        // No faults, but hold the seam's lock so a concurrently-armed test in a
        // single-process `cargo test` run cannot leak a fault into this one.
        let _faults = crate::util::arm_legacy_migration_faults(&[]);
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonicalize the fixture");
        let home = root.join("home");
        let state = root.join("state");

        let mut env = crate::util::TestEnvGuard::new();
        env.set("BLACKBOX_CONFIG", root.join("absent-config.toml"));
        env.set("BLACKBOX_STATE_DIR", &state);
        // Keep every resolved migration destination inside the fixture: the
        // index and the vector root both default outside the state directory.
        env.set("TRANSCRIPT_SEARCH_INDEX_PATH", root.join("index"));
        env.set("BLACKBOX_VECTORS_PATH", root.join("vectors"));
        env.set("BLACKBOX_GLOBAL_COMMON_MD", root.join("BLACKBOX.md"));
        for var in [
            "BLACKBOX_KNOWLEDGE_PATH",
            "BLACKBOX_THREADS_PATH",
            "BLACKBOX_NOTES_PATH",
            "BRO_HOME",
        ] {
            env.remove(var);
        }
        let cfg = crate::config::load().expect("load the daemon config");

        // The legacy tree an upgrading daemon finds.
        let legacy = home.join(".claude-shared");
        std::fs::create_dir_all(&legacy).expect("create the legacy tree");
        std::fs::write(legacy.join("blackbox-knowledge.json"), b"[]").expect("legacy knowledge");
        std::fs::write(legacy.join("blackbox-threads.json"), b"[]").expect("legacy threads");
        std::fs::create_dir_all(home.join(".bro")).expect("create the legacy bro home");
        std::fs::write(home.join(".bro").join("tasks.json"), b"[]").expect("legacy bro state");

        let roots = vec![InstanceRoot::state_root(state.clone())];
        let live = acquire_instance_locks(&roots).expect("the healthy daemon claims its roots");

        let error = claim_roots_then_migrate(&home, &cfg, &roots)
            .expect_err("the duplicate startup must refuse");
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
        let (_locks, migrated) = claim_roots_then_migrate(&home, &cfg, &roots)
            .expect("a released root is claimable again");
        assert!(
            migrated.iter().any(|line| line.starts_with("knowledge:")),
            "the migration runs under the claim: {migrated:?}"
        );
        assert!(state.join("blackbox-knowledge.json").exists());
        assert!(state.join("bro").join("tasks.json").exists());
    }

    /// The R34F1 regression, driven through startup itself. An upgrading
    /// daemon that cannot INSPECT a legacy source (a transient `EACCES` on the
    /// parent, an `EIO` from the device) used to be told the source was
    /// absent, report the migration skipped, and go on to open a fresh
    /// destination. The next startup then saw the destination exist, took the
    /// destination-exists branch, and stranded the legacy source for good.
    ///
    /// Startup must refuse, and it must refuse having created no destination
    /// at all — that is what keeps the next startup on the migrating path.
    #[test]
    fn a_failed_legacy_inspection_refuses_startup_without_creating_a_destination() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonicalize the fixture");
        let home = root.join("home");
        let state = root.join("state");

        let mut env = crate::util::TestEnvGuard::new();
        env.set("BLACKBOX_CONFIG", root.join("absent-config.toml"));
        env.set("BLACKBOX_STATE_DIR", &state);
        env.set("TRANSCRIPT_SEARCH_INDEX_PATH", root.join("index"));
        env.set("BLACKBOX_VECTORS_PATH", root.join("vectors"));
        env.set("BLACKBOX_GLOBAL_COMMON_MD", root.join("BLACKBOX.md"));
        for var in [
            "BLACKBOX_KNOWLEDGE_PATH",
            "BLACKBOX_THREADS_PATH",
            "BLACKBOX_NOTES_PATH",
            "BRO_HOME",
        ] {
            env.remove(var);
        }
        let cfg = crate::config::load().expect("load the daemon config");

        let legacy = home.join(".claude-shared");
        std::fs::create_dir_all(&legacy).expect("create the legacy tree");
        std::fs::write(legacy.join("blackbox-knowledge.json"), b"[]").expect("legacy knowledge");
        std::fs::create_dir_all(home.join(".bro")).expect("create the legacy bro home");
        std::fs::write(home.join(".bro").join("tasks.json"), b"[]").expect("legacy bro state");

        let roots = vec![InstanceRoot::state_root(state.clone())];

        for (point, errno) in [
            (
                crate::util::LegacyMigrationFault::InspectSource,
                crate::util::INJECTED_EACCES,
            ),
            (
                crate::util::LegacyMigrationFault::InspectDestination,
                crate::util::INJECTED_EIO,
            ),
        ] {
            let faults = crate::util::arm_legacy_migration_faults(&[(point, errno)]);
            let error = claim_roots_then_migrate(&home, &cfg, &roots)
                .expect_err("an uninspectable legacy entry must refuse the startup");
            assert!(
                format!("{error:#}").contains(&format!("{point:?}")),
                "the refusal names the inspection that failed: {error:#}"
            );
            drop(faults);

            // The legacy sources are intact and nothing was created at any
            // destination beyond the lock the claim itself had to open.
            assert!(legacy.join("blackbox-knowledge.json").exists());
            assert!(home.join(".bro").join("tasks.json").exists());
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
        }

        // With the fault gone the same startup migrates, which is what the
        // refusals above deferred rather than lost.
        let (_locks, migrated) =
            claim_roots_then_migrate(&home, &cfg, &roots).expect("a readable legacy tree migrates");
        assert!(
            migrated.iter().any(|line| line.starts_with("knowledge:")),
            "the migration runs once the inspection succeeds: {migrated:?}"
        );
        assert!(state.join("blackbox-knowledge.json").exists());
    }

    /// The R33F2 regression, driven through the surface the finding names:
    /// two daemons isolated ONLY by `[paths].state_dir` in their own config
    /// files, sharing one `$HOME` and therefore one legacy source tree.
    ///
    /// The destinations must follow the loaded configuration, so the winner's
    /// stores receive the legacy state and the second daemon's stores stay
    /// empty. Recomputing destinations from env vars and `$HOME` sent both at
    /// the same production-default paths, which neither had claimed.
    #[test]
    fn config_file_isolated_daemons_migrate_into_their_own_resolved_roots() {
        let _faults = crate::util::arm_legacy_migration_faults(&[]);
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonicalize the fixture");
        let home = root.join("home");

        // The legacy tree both daemons can see.
        let legacy = home.join(".claude-shared");
        std::fs::create_dir_all(&legacy).expect("create the legacy tree");
        std::fs::write(legacy.join("blackbox-knowledge.json"), b"[]").expect("legacy knowledge");
        std::fs::create_dir_all(home.join(".bro")).expect("create the legacy bro home");
        std::fs::write(home.join(".bro").join("tasks.json"), b"[]").expect("legacy bro state");

        let load_for = |env: &mut crate::util::TestEnvGuard, name: &str| {
            let state = root.join(name);
            let config_path = root.join(format!("{name}.toml"));
            std::fs::write(
                &config_path,
                format!("[paths]\nstate_dir = {state:?}\n").as_bytes(),
            )
            .expect("write the daemon config file");
            // state_dir is deliberately config-file-only. The index, vector,
            // and global-memory roots default outside the state directory, so
            // pin them inside the fixture rather than at the host's real ones.
            env.set("BLACKBOX_CONFIG", &config_path);
            env.set(
                "TRANSCRIPT_SEARCH_INDEX_PATH",
                root.join(name).join("index"),
            );
            env.set("BLACKBOX_VECTORS_PATH", root.join(name).join("vectors"));
            env.set(
                "BLACKBOX_GLOBAL_COMMON_MD",
                root.join(name).join("BLACKBOX.md"),
            );
            for var in [
                "BLACKBOX_STATE_DIR",
                "BLACKBOX_KNOWLEDGE_PATH",
                "BLACKBOX_THREADS_PATH",
                "BLACKBOX_NOTES_PATH",
                "BRO_HOME",
            ] {
                env.remove(var);
            }
            (
                state,
                crate::config::load().expect("load the daemon config"),
            )
        };

        let mut env = crate::util::TestEnvGuard::new();
        let (first_state, first) = load_for(&mut env, "daemon-a");
        let (second_state, second) = load_for(&mut env, "daemon-b");
        assert_ne!(
            first.paths.knowledge_path, second.paths.knowledge_path,
            "the two config files isolate the knowledge store"
        );

        // The second daemon is mid-migration, holding the legacy source.
        let held = crate::util::try_lock_legacy_migration(&home)
            .expect("open the legacy source lock")
            .expect("the peer claims the legacy source");
        let skipped = claim_roots_then_migrate(
            &home,
            &first,
            &[InstanceRoot::state_root(first_state.clone())],
        )
        .expect("a contended legacy source is not a startup failure");
        assert!(
            skipped.1.is_empty(),
            "the daemon that lost the source claim migrates nothing: {:?}",
            skipped.1
        );
        assert!(!first.paths.knowledge_path.exists());
        drop(skipped);
        drop(held);

        // It releases; this daemon migrates into ITS resolved roots, once.
        let (_locks, migrated) = claim_roots_then_migrate(
            &home,
            &first,
            &[InstanceRoot::state_root(first_state.clone())],
        )
        .expect("a released legacy source is claimable");
        assert!(
            migrated.iter().any(|line| line.starts_with("knowledge:")),
            "the migration ran: {migrated:?}"
        );
        assert!(first.paths.knowledge_path.exists());
        assert!(first.paths.bro_home.join("tasks.json").exists());
        assert!(
            !second_state.exists(),
            "the other daemon's resolved roots were never written"
        );
        assert!(!legacy.join("blackbox-knowledge.json").exists());
    }
}
