use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};

use anyhow::{Context, Result};
use parking_lot::RwLock;
use serde::Serialize;
use tokio::sync::oneshot;

pub trait StoreSnapshot: Send + Sync + 'static {
    type Snapshot: Serialize + Send + 'static;

    fn snapshot(&self) -> Result<Self::Snapshot>;
}

pub struct StorePersister<S: StoreSnapshot> {
    inner: Arc<StorePersisterInner<S>>,
}

impl<S: StoreSnapshot> Clone for StorePersister<S> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

struct StorePersisterInner<S: StoreSnapshot> {
    #[allow(dead_code)]
    name: String,
    store: Arc<RwLock<S>>,
    store_path: PathBuf,
    tx: mpsc::Sender<PersistRequest>,
}

struct PersistRequest {
    ack: Option<PersistAck>,
}

enum PersistAck {
    Async(oneshot::Sender<std::result::Result<(), String>>),
    Blocking(mpsc::Sender<std::result::Result<(), String>>),
}

impl PersistAck {
    fn send(self, result: std::result::Result<(), String>) {
        match self {
            PersistAck::Async(tx) => {
                let _ = tx.send(result);
            }
            PersistAck::Blocking(tx) => {
                let _ = tx.send(result);
            }
        }
    }
}

impl<S: StoreSnapshot> StorePersister<S> {
    pub fn spawn(name: impl Into<String>, store: Arc<RwLock<S>>, store_path: PathBuf) -> Self {
        let name = name.into();
        let (tx, rx) = mpsc::channel::<PersistRequest>();
        let store_w = store.clone();
        let path_w = store_path.clone();
        let thread_name = format!("{name}-persist");
        let spawned = std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                let _scope = bbox_util::util::BlockingScope::enter();
                run_actor(rx, store_w, path_w)
            });
        if let Err(err) = spawned {
            tracing::error!(error = %err, store = %name, "failed to spawn store persister thread");
        }

        Self {
            inner: Arc::new(StorePersisterInner {
                name,
                store,
                store_path,
                tx,
            }),
        }
    }

    #[allow(dead_code)]
    pub fn request(&self) {
        if self.inner.tx.send(PersistRequest { ack: None }).is_err() {
            self.write_now_on_fallback_thread();
        }
    }

    pub async fn request_durable(&self) -> Result<()> {
        let (ack_tx, ack_rx) = oneshot::channel();
        if self
            .inner
            .tx
            .send(PersistRequest {
                ack: Some(PersistAck::Async(ack_tx)),
            })
            .is_err()
        {
            return self.write_now_blocking_task().await;
        }

        match ack_rx.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(err)) => Err(anyhow::anyhow!(err)),
            Err(_) => self.write_now_blocking_task().await,
        }
    }

    pub fn flush_blocking(&self) -> Result<()> {
        let (ack_tx, ack_rx) = mpsc::channel();
        if self
            .inner
            .tx
            .send(PersistRequest {
                ack: Some(PersistAck::Blocking(ack_tx)),
            })
            .is_ok()
        {
            match ack_rx.recv() {
                Ok(Ok(())) => return Ok(()),
                Ok(Err(err)) => return Err(anyhow::anyhow!(err)),
                Err(_) => {}
            }
        }
        self.write_now()
    }

    fn write_now(&self) -> Result<()> {
        persist_once(&self.inner.store, &self.inner.store_path)
    }

    async fn write_now_blocking_task(&self) -> Result<()> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.write_now())
            .await
            .context("store persister fallback task panicked")?
    }

    #[allow(dead_code)]
    fn write_now_on_fallback_thread(&self) {
        let this = self.clone();
        let name = format!("{}-persist-fallback", self.inner.name);
        let spawned = std::thread::Builder::new().name(name).spawn(move || {
            if let Err(err) = this.write_now() {
                tracing::error!(error = %err, "store persister fallback write failed");
            }
        });
        if let Err(err) = spawned {
            tracing::error!(error = %err, "failed to spawn store persister fallback thread");
        }
    }
}

fn run_actor<S: StoreSnapshot>(
    rx: mpsc::Receiver<PersistRequest>,
    store: Arc<RwLock<S>>,
    store_path: PathBuf,
) {
    while let Ok(first) = rx.recv() {
        let mut acks = Vec::new();
        if let Some(ack) = first.ack {
            acks.push(ack);
        }
        while let Ok(next) = rx.try_recv() {
            if let Some(ack) = next.ack {
                acks.push(ack);
            }
        }

        let result = persist_once(&store, &store_path).map_err(|err| format!("{err:#}"));
        if let Err(err) = &result {
            tracing::error!(error = %err, path = %store_path.display(), "store persist failed");
        }
        for ack in acks {
            ack.send(result.clone());
        }
    }
}

fn persist_once<S: StoreSnapshot>(store: &RwLock<S>, store_path: &Path) -> Result<()> {
    let snapshot = {
        let guard = store.read();
        guard.snapshot()?
    };
    bbox_corpus_core::json_store::with_store_lock(store_path, || {
        bbox_corpus_core::json_store::atomic_write_json_locked(store_path, &snapshot)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tempfile::tempdir;

    #[derive(Clone, Serialize, Deserialize)]
    struct TestSnapshot {
        value: usize,
    }

    struct TestStore {
        value: usize,
        snapshot_count: Arc<AtomicUsize>,
        snapshot_delay: Option<Duration>,
    }

    impl TestStore {
        fn new(value: usize, snapshot_count: Arc<AtomicUsize>) -> Self {
            Self {
                value,
                snapshot_count,
                snapshot_delay: None,
            }
        }
    }

    impl StoreSnapshot for TestStore {
        type Snapshot = TestSnapshot;

        fn snapshot(&self) -> Result<Self::Snapshot> {
            self.snapshot_count.fetch_add(1, Ordering::SeqCst);
            if let Some(delay) = self.snapshot_delay {
                std::thread::sleep(delay);
            }
            Ok(TestSnapshot { value: self.value })
        }
    }

    #[tokio::test]
    async fn durable_ack_arrives_after_file_contents_reflect_mutation() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("store.json");
        let count = Arc::new(AtomicUsize::new(0));
        let store = Arc::new(RwLock::new(TestStore::new(0, count)));
        let persister = StorePersister::spawn("test-durable", store.clone(), path.clone());

        store.write().value = 7;
        persister.request_durable().await.unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        let saved: TestSnapshot = serde_json::from_str(&raw).unwrap();
        assert_eq!(saved.value, 7);
    }

    #[test]
    fn coalescing_collapses_a_queued_burst() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("store.json");
        let count = Arc::new(AtomicUsize::new(0));
        let store = Arc::new(RwLock::new(TestStore::new(0, count.clone())));
        store.write().snapshot_delay = Some(Duration::from_millis(100));
        let persister = StorePersister::spawn("test-coalesce", store.clone(), path.clone());

        persister.request();
        while count.load(Ordering::SeqCst) == 0 {
            std::thread::sleep(Duration::from_millis(5));
        }

        for value in 1..20 {
            store.write().value = value;
            persister.request();
        }
        store.write().value = 99;
        persister.flush_blocking().unwrap();

        // Exact snapshot counts are scheduling-dependent: the burst races the
        // actor's drain window (run_actor's try_recv loop runs between the
        // initial persist's file write and the next park), so under load an
        // extra wakeup can land mid-burst. The invariant is coalescing — far
        // fewer persists than the 21 requests — not an exact count.
        let snapshots = count.load(Ordering::SeqCst);
        assert!(
            (2..=4).contains(&snapshots),
            "expected 21 requests to coalesce into 2-4 snapshots, got {snapshots}"
        );
        let raw = std::fs::read_to_string(&path).unwrap();
        let saved: TestSnapshot = serde_json::from_str(&raw).unwrap();
        assert_eq!(saved.value, 99);
    }

    #[test]
    fn flush_blocking_drains_pending_request() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("store.json");
        let count = Arc::new(AtomicUsize::new(0));
        let store = Arc::new(RwLock::new(TestStore::new(0, count)));
        let persister = StorePersister::spawn("test-flush", store.clone(), path.clone());

        store.write().value = 42;
        persister.request();
        persister.flush_blocking().unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        let saved: TestSnapshot = serde_json::from_str(&raw).unwrap();
        assert_eq!(saved.value, 42);
    }
}
