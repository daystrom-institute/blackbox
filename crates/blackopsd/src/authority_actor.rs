//! Single blocking owner for the durable operational authority.
#![allow(clippy::disallowed_methods)]

use std::path::PathBuf;
use std::sync::Mutex;
use std::thread::JoinHandle;

use blackops_core::{BlackopsAuthority, BlackopsResult, FileRepository, OperationalSnapshot};
use tokio::sync::{mpsc, oneshot};

use crate::{BlackopsdError, BlackopsdResult};

type Authority = BlackopsAuthority<FileRepository>;
type AuthorityJob = Box<dyn FnOnce(&mut Authority) + Send + 'static>;

enum ActorMessage {
    Call(AuthorityJob),
    Shutdown(oneshot::Sender<()>),
}

struct ActorInner {
    sender: mpsc::Sender<ActorMessage>,
    join: Mutex<Option<JoinHandle<()>>>,
}

#[derive(Clone)]
pub struct AuthorityActor {
    inner: std::sync::Arc<ActorInner>,
}

impl AuthorityActor {
    pub async fn open(state_root: PathBuf) -> BlackopsdResult<Self> {
        let (sender, mut receiver) = mpsc::channel::<ActorMessage>(256);
        let (started_tx, started_rx) = oneshot::channel();
        let join = std::thread::Builder::new()
            .name("blackops-authority".into())
            .spawn(move || {
                let startup = FileRepository::open(state_root).and_then(BlackopsAuthority::open);
                let mut authority = match startup {
                    Ok(authority) => {
                        let _ = started_tx.send(Ok(()));
                        authority
                    }
                    Err(error) => {
                        let _ = started_tx.send(Err(error));
                        return;
                    }
                };
                while let Some(message) = receiver.blocking_recv() {
                    match message {
                        ActorMessage::Call(job) => job(&mut authority),
                        ActorMessage::Shutdown(done) => {
                            let _ = done.send(());
                            break;
                        }
                    }
                }
            })?;
        started_rx
            .await
            .map_err(|_| BlackopsdError::AuthorityUnavailable)??;
        Ok(Self {
            inner: std::sync::Arc::new(ActorInner {
                sender,
                join: Mutex::new(Some(join)),
            }),
        })
    }

    pub async fn call<T, F>(&self, operation: F) -> BlackopsdResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Authority) -> BlackopsResult<T> + Send + 'static,
    {
        let (result_tx, result_rx) = oneshot::channel();
        let job = Box::new(move |authority: &mut Authority| {
            let _ = result_tx.send(operation(authority));
        });
        self.inner
            .sender
            .send(ActorMessage::Call(job))
            .await
            .map_err(|_| BlackopsdError::AuthorityUnavailable)?;
        result_rx
            .await
            .map_err(|_| BlackopsdError::AuthorityUnavailable)?
            .map_err(Into::into)
    }

    pub async fn snapshot(&self) -> BlackopsdResult<OperationalSnapshot> {
        self.call(|authority| Ok(authority.snapshot())).await
    }

    pub async fn shutdown(&self) {
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
