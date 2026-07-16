use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use fleet_core::LeasePolicy;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::authority_actor::AuthorityActor;
use crate::capability::HttpCapabilityBroker;
use crate::compatibility::CompatibilityProxy;
use crate::config::{FleetMode, FleetdConfig};
use crate::control::{AuthorityRuntime, ControlState, router};
use crate::launch::{ProcessWorkerLauncher, ProcessWorkerLauncherConfig};
use crate::provider::HostProviderRegistry;
use crate::records::{BlackboxRecordHttpClient, RecordPump};
use crate::roster::RosterHub;
use crate::shadow::ShadowReplica;
use crate::worker::WorkerServer;
use crate::worktree::WorktreeManager;
use crate::{CapabilityBroker, FleetdError, FleetdResult, WorkerLauncher};

pub struct Fleetd {
    config: FleetdConfig,
    state: ControlState,
    worker_server: Option<WorkerServer>,
    authority: Option<AuthorityActor>,
    worktrees: Option<WorktreeManager>,
    record_pump: Option<RecordPump>,
}

impl Fleetd {
    pub async fn open(config: FleetdConfig) -> FleetdResult<Self> {
        let config = config.normalized()?;
        let service_token = Arc::new(
            bro_rpc::ServiceToken::load_or_create(config.service_token_path()).map_err(
                |error| {
                    FleetdError::InvalidConfiguration(format!("loading service token: {error}"))
                },
            )?,
        );
        let broker: Arc<dyn CapabilityBroker> = Arc::new(HttpCapabilityBroker::new(
            config.blackopsd_url.clone(),
            config.blackboxd_url.clone(),
            config.capability_timeout(),
            service_token,
        )?);
        if config.mode == FleetMode::Shadow {
            return Self::open_with(config, None, broker).await;
        }
        let worker_socket = config
            .worker_socket
            .clone()
            .ok_or_else(|| FleetdError::InvalidConfiguration("worker socket is missing".into()))?;
        let launcher: Arc<dyn WorkerLauncher> = Arc::new(
            ProcessWorkerLauncher::new(ProcessWorkerLauncherConfig {
                binary: config.bro_harness_bin.clone(),
                worker_socket,
                fleet_state_dir: config.state_dir.clone(),
                worker_root: config.worker_root(),
                bro_root: config.state_dir.join("bro"),
                protected_peer_service_roots: config.protected_peer_service_roots(),
                service_token_file: config.service_token_path().to_path_buf(),
                denied_service_ports: config.denied_worker_service_ports(),
                external_sandbox_launcher: config.worker_sandbox_launcher.clone(),
            })
            .await?,
        );
        let home = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let provider_config = config.provider_config.clone();
        let providers = Arc::new(
            tokio::task::spawn_blocking(move || {
                HostProviderRegistry::load(provider_config.as_deref(), &home, now_ms())
            })
            .await
            .map_err(|error| {
                FleetdError::Conflict(format!("provider config task failed: {error}"))
            })??,
        );
        Self::open_with_provider(config, Some(launcher), broker, providers).await
    }

    pub async fn open_with(
        config: FleetdConfig,
        launcher: Option<Arc<dyn WorkerLauncher>>,
        broker: Arc<dyn CapabilityBroker>,
    ) -> FleetdResult<Self> {
        let config = config.normalized()?;
        let providers = Arc::new(HostProviderRegistry::testing(now_ms()));
        Self::open_with_provider(config, launcher, broker, providers).await
    }

    async fn open_with_provider(
        config: FleetdConfig,
        launcher: Option<Arc<dyn WorkerLauncher>>,
        broker: Arc<dyn CapabilityBroker>,
        providers: Arc<HostProviderRegistry>,
    ) -> FleetdResult<Self> {
        let config = config.normalized()?;
        let service_token = Arc::new(
            bro_rpc::ServiceToken::load_or_create(config.service_token_path()).map_err(
                |error| {
                    FleetdError::InvalidConfiguration(format!("loading service token: {error}"))
                },
            )?,
        );
        let compatibility = CompatibilityProxy::new(
            config.blackopsd_url.clone(),
            config.compatibility_timeout(),
            service_token.clone(),
        )?;
        match config.mode {
            FleetMode::Shadow => {
                if launcher.is_some() {
                    return Err(FleetdError::InvalidConfiguration(
                        "shadow mode must not receive a worker launcher".into(),
                    ));
                }
                let shadow = ShadowReplica::new();
                let roster = shadow.hub();
                let shadow_replication_enabled = config.shadow_source.is_some();
                Ok(Self {
                    config,
                    state: ControlState {
                        mode: FleetMode::Shadow,
                        roster,
                        shadow: Some(shadow),
                        authority: None,
                        compatibility,
                        worker_socket_ready: Arc::new(AtomicBool::new(true)),
                        shadow_replication_enabled,
                        service_token: service_token.clone(),
                    },
                    worker_server: None,
                    authority: None,
                    worktrees: None,
                    record_pump: None,
                })
            }
            FleetMode::Authority => {
                let launcher = launcher.ok_or_else(|| {
                    FleetdError::InvalidConfiguration(
                        "authority mode requires a worker launcher".into(),
                    )
                })?;
                let authority = AuthorityActor::open(
                    config.snapshot_path(),
                    config.authority_lock_path(),
                    now_ms(),
                )
                .await?;
                let snapshot = authority
                    .call(|authority| authority.snapshot().map_err(Into::into))
                    .await?;
                let roster = RosterHub::from_authority(&snapshot);
                let record_pump = config
                    .blackboxd_url
                    .as_ref()
                    .map(|url| {
                        BlackboxRecordHttpClient::new(
                            url,
                            config.capability_timeout(),
                            service_token.clone(),
                        )
                        .map(|client| {
                            RecordPump::new(
                                authority.clone(),
                                Arc::new(client),
                                roster.clone(),
                                config.record_pump_interval(),
                            )
                        })
                    })
                    .transpose()?;
                let configured_lane_ids = providers
                    .lanes()
                    .iter()
                    .map(|lane| lane.lane_id.clone())
                    .collect::<Vec<_>>();
                for lane in providers.lanes().iter().cloned() {
                    authority
                        .call(move |authority| {
                            authority
                                .configure_provider_lane(lane)
                                .map(|_| ())
                                .map_err(Into::into)
                        })
                        .await?;
                }
                authority
                    .call(move |authority| {
                        authority
                            .reconcile_provider_lanes(configured_lane_ids, now_ms())
                            .map_err(Into::into)
                    })
                    .await?;
                let worktrees = WorktreeManager::new(
                    config.worktree_root(),
                    config.fleet_config.clone(),
                    authority.clone(),
                );
                let lease_policy = LeasePolicy {
                    lease_duration_ms: config.lease_duration_ms,
                    heartbeat_interval_ms: config.heartbeat_interval_ms,
                    reattach_grace_ms: config.reattach_grace_ms,
                };
                let worker_server = WorkerServer::new(
                    config.worker_socket.clone().ok_or_else(|| {
                        FleetdError::InvalidConfiguration("worker socket is missing".into())
                    })?,
                    config.worker_root(),
                    authority.clone(),
                    roster.clone(),
                    broker,
                    config.allowed_capabilities.clone(),
                    lease_policy,
                );
                let runtime = AuthorityRuntime {
                    authority: authority.clone(),
                    launcher,
                    live: worker_server.live_workers(),
                    provider_resolver: providers,
                    worktrees: worktrees.clone(),
                };
                let socket_ready = Arc::new(AtomicBool::new(false));
                let worker_server = worker_server.with_ready_flag(socket_ready.clone());
                Ok(Self {
                    config,
                    state: ControlState {
                        mode: FleetMode::Authority,
                        roster,
                        shadow: None,
                        authority: Some(runtime),
                        compatibility,
                        worker_socket_ready: socket_ready,
                        shadow_replication_enabled: false,
                        service_token,
                    },
                    worker_server: Some(worker_server),
                    authority: Some(authority),
                    worktrees: Some(worktrees),
                    record_pump,
                })
            }
        }
    }

    pub async fn start(self) -> FleetdResult<FleetdHandle> {
        if let Some(worktrees) = &self.worktrees {
            worktrees.reconcile().await?;
        }
        let shutdown = CancellationToken::new();
        let mut tasks = Vec::new();

        if let Some(worker_server) = self.worker_server {
            let worker_shutdown = shutdown.clone();
            let task = tokio::spawn(async move { worker_server.run(worker_shutdown).await });
            let ready = self.state.worker_socket_ready.clone();
            let started = tokio::time::timeout(Duration::from_secs(5), async {
                while !ready.load(Ordering::Acquire) {
                    if task.is_finished() {
                        return Err(FleetdError::AuthorityUnavailable);
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                Ok(())
            })
            .await
            .map_err(|_| FleetdError::AuthorityUnavailable)?;
            if let Err(error) = started {
                shutdown.cancel();
                return match task.await {
                    Ok(Err(task_error)) => Err(task_error),
                    _ => Err(error),
                };
            }
            tasks.push(task);
        }

        if let Some(record_pump) = self.record_pump {
            let record_shutdown = shutdown.clone();
            tasks.push(tokio::spawn(async move {
                record_pump.run(record_shutdown).await
            }));
        }

        let listener = tokio::net::TcpListener::bind(self.config.bind).await?;
        let local_addr = listener.local_addr()?;
        let http_shutdown = shutdown.clone();
        let app = router(self.state.clone());
        tasks.push(tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(http_shutdown.cancelled_owned())
                .await
                .map_err(FleetdError::from)
        }));

        if let Some(authority) = self.authority.clone() {
            let roster = self.state.roster.clone();
            let reaper_shutdown = shutdown.clone();
            let interval = Duration::from_millis(
                self.config
                    .heartbeat_interval_ms
                    .min(self.config.lease_duration_ms)
                    .max(100),
            );
            let worktrees = self.worktrees.clone();
            tasks.push(tokio::spawn(async move {
                let mut ticker = tokio::time::interval(interval);
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tokio::select! {
                        _ = reaper_shutdown.cancelled() => return Ok(()),
                        _ = ticker.tick() => {
                            // A single tick must never take the whole reaper down.
                            // Lease expiry, provider release, and worktree
                            // reconciliation each hit git and the authority actor,
                            // any of which can fail transiently (e.g. a worktree
                            // vanishing under a concurrent closeout). Log per-tick
                            // errors and keep ticking; the loop only exits on the
                            // shutdown cancellation branch above.
                            let tick: FleetdResult<()> = async {
                                let expired = authority
                                    .call(|authority| authority.expire_stale(now_ms()).map_err(Into::into))
                                    .await?;
                                if !expired.is_empty() {
                                    let snapshot = authority
                                        .call(|authority| authority.snapshot().map_err(Into::into))
                                        .await?;
                                    roster.publish_authority(&snapshot);
                                }
                                let snapshot = authority
                                    .call(|authority| authority.snapshot().map_err(Into::into))
                                    .await?;
                                for task in snapshot.tasks.values().filter(|task| task.status.is_terminal()) {
                                    let task_id = task.task_id.clone();
                                    authority.call(move |authority| {
                                        authority.release_provider(&task_id, now_ms()).map(|_| ()).map_err(Into::into)
                                    }).await?;
                                }
                                if let Some(worktrees) = &worktrees {
                                    worktrees.reconcile().await?;
                                }
                                Ok(())
                            }
                            .await;
                            if let Err(error) = tick {
                                tracing::warn!(%error, "fleetd reaper tick failed; continuing");
                            }
                        }
                    }
                }
            }));
        }

        tracing::info!(%local_addr, mode = ?self.config.mode, "fleetd ready");
        Ok(FleetdHandle {
            local_addr,
            shutdown,
            tasks,
            authority: self.authority,
        })
    }
}

pub struct FleetdHandle {
    local_addr: SocketAddr,
    shutdown: CancellationToken,
    tasks: Vec<JoinHandle<FleetdResult<()>>>,
    authority: Option<AuthorityActor>,
}

impl FleetdHandle {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub async fn shutdown(mut self) -> FleetdResult<()> {
        self.shutdown.cancel();
        let mut first_error = None;
        for task in self.tasks.drain(..) {
            match task.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) if first_error.is_none() => first_error = Some(error),
                Err(error) if first_error.is_none() => {
                    first_error = Some(FleetdError::Conflict(format!(
                        "fleetd service task failed: {error}"
                    )))
                }
                _ => {}
            }
        }
        if let Some(authority) = &self.authority {
            authority.shutdown().await;
        }
        if let Some(error) = first_error {
            Err(error)
        } else {
            Ok(())
        }
    }
}

impl Drop for FleetdHandle {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
