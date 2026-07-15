use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread::JoinHandle;

use fleet_core::{FileFleetRepository, FleetAuthority, UuidIdentityGenerator};
use fs2::FileExt;
use tokio::sync::{mpsc, oneshot};

use crate::{FleetdError, FleetdResult};

pub(crate) type Authority = FleetAuthority<FileFleetRepository, UuidIdentityGenerator>;

type AuthorityJob = Box<dyn FnOnce(&Authority) + Send + 'static>;

enum ActorMessage {
    Call(AuthorityJob),
    Shutdown(oneshot::Sender<()>),
}

struct ActorInner {
    sender: mpsc::Sender<ActorMessage>,
    join: Mutex<Option<JoinHandle<()>>>,
}

#[derive(Clone)]
pub(crate) struct AuthorityActor {
    inner: std::sync::Arc<ActorInner>,
}

impl AuthorityActor {
    pub(crate) async fn open(
        snapshot_path: PathBuf,
        authority_lock_path: PathBuf,
        now_unix_ms: u64,
    ) -> FleetdResult<Self> {
        let (sender, mut receiver) = mpsc::channel::<ActorMessage>(256);
        let (started_tx, started_rx) = oneshot::channel();
        let join = std::thread::Builder::new()
            .name("fleet-authority".into())
            .spawn(move || {
                let startup = (|| -> FleetdResult<(Authority, File)> {
                    let state_root = snapshot_path.parent().unwrap_or_else(|| Path::new("."));
                    prepare_private_directory(state_root)?;
                    let authority_lock = open_authority_lock(&authority_lock_path)?;
                    authority_lock.try_lock_exclusive().map_err(|error| {
                        FleetdError::Conflict(format!(
                            "another fleet authority holds {}: {error}",
                            authority_lock_path.display()
                        ))
                    })?;
                    let repository = FileFleetRepository::new(snapshot_path);
                    let authority =
                        FleetAuthority::open(repository, UuidIdentityGenerator, now_unix_ms)?;
                    Ok((authority, authority_lock))
                })();

                let (authority, _authority_lock) = match startup {
                    Ok(value) => {
                        let _ = started_tx.send(Ok(()));
                        value
                    }
                    Err(error) => {
                        let _ = started_tx.send(Err(error));
                        return;
                    }
                };

                while let Some(message) = receiver.blocking_recv() {
                    match message {
                        ActorMessage::Call(job) => job(&authority),
                        ActorMessage::Shutdown(done) => {
                            let _ = done.send(());
                            break;
                        }
                    }
                }
            })?;

        started_rx
            .await
            .map_err(|_| FleetdError::AuthorityUnavailable)??;
        Ok(Self {
            inner: std::sync::Arc::new(ActorInner {
                sender,
                join: Mutex::new(Some(join)),
            }),
        })
    }

    pub(crate) async fn call<T, F>(&self, operation: F) -> FleetdResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&Authority) -> FleetdResult<T> + Send + 'static,
    {
        let (result_tx, result_rx) = oneshot::channel();
        let job = Box::new(move |authority: &Authority| {
            let _ = result_tx.send(operation(authority));
        });
        self.inner
            .sender
            .send(ActorMessage::Call(job))
            .await
            .map_err(|_| FleetdError::AuthorityUnavailable)?;
        result_rx
            .await
            .map_err(|_| FleetdError::AuthorityUnavailable)?
    }

    pub(crate) async fn shutdown(&self) {
        let (done_tx, done_rx) = oneshot::channel();
        if self
            .inner
            .sender
            .send(ActorMessage::Shutdown(done_tx))
            .await
            .is_ok()
        {
            let _ = done_rx.await;
        }
        let join = self
            .inner
            .join
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(join) = join {
            let _ = tokio::task::spawn_blocking(move || join.join()).await;
        }
    }
}

fn prepare_private_directory(path: &Path) -> FleetdResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

        match std::fs::symlink_metadata(path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(FleetdError::InvalidConfiguration(format!(
                        "fleet state root {} is not a real directory",
                        path.display()
                    )));
                }
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut builder = std::fs::DirBuilder::new();
                builder.recursive(true).mode(0o700).create(path)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(path)?;
    Ok(())
}

fn open_authority_lock(path: &Path) -> FleetdResult<File> {
    if let Ok(metadata) = std::fs::symlink_metadata(path)
        && (metadata.file_type().is_symlink() || !metadata.is_file())
    {
        return Err(FleetdError::InvalidConfiguration(format!(
            "fleet authority lock {} is not a regular file",
            path.display()
        )));
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lifecycle_lock_rejects_a_second_authority() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let first = AuthorityActor::open(root.join("state.json"), root.join("lock"), 1)
            .await
            .unwrap();
        let second = AuthorityActor::open(root.join("state.json"), root.join("lock"), 1).await;
        assert!(matches!(second, Err(FleetdError::Conflict(_))));
        first.shutdown().await;
    }
}
