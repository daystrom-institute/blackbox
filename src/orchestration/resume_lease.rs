//! Daemon-wide resume lease registry.
//!
//! `bro_resume` spawns a fresh `claude --resume <id>` process per call
//! — two concurrent resumes on the same provider session race the
//! session jsonl writes and corrupt the transcript. This registry
//! exposes a single async mutex per `(provider, session_id)` key so
//! every dispatch path can serialize against the same invariant.
//!
//! Council's drain worker is the first adopter (multi-producer
//! collisions are *expected* there). Other paths — `bro_broadcast`,
//! ad-hoc `bro_resume`, workflow durable actors, team advisor resume
//! — should adopt incrementally; until then they remain a latent
//! single-resume-at-a-time assumption that this lease can later
//! mechanize.
//!
//! Lease acquisition is async: callers `await` the guard, holding it
//! across the full dispatch (`spawn_task` → `wait_for_task`). Drop
//! the guard to release; nothing else needs to be done.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex as SyncMutex;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use crate::orchestration::providers::Provider;

#[derive(Debug, Default)]
pub struct ResumeLeaseRegistry {
    inner: SyncMutex<HashMap<LeaseKey, Arc<AsyncMutex<()>>>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct LeaseKey {
    provider: Provider,
    session_id: String,
}

impl ResumeLeaseRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Acquire the resume lease for `(provider, session_id)`. Blocks
    /// asynchronously until any prior holder releases. The returned
    /// guard must outlive the dispatch — drop it after `wait_for_task`
    /// completes (or fails).
    pub async fn acquire(&self, provider: Provider, session_id: &str) -> OwnedMutexGuard<()> {
        let key = LeaseKey {
            provider,
            session_id: session_id.to_string(),
        };
        let lock = {
            let mut map = self.inner.lock();
            map.entry(key)
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };
        lock.lock_owned().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lease_serializes_same_key() {
        let reg = Arc::new(ResumeLeaseRegistry::new());
        let order = Arc::new(SyncMutex::new(Vec::<u8>::new()));

        let r1 = reg.clone();
        let o1 = order.clone();
        let h1 = tokio::spawn(async move {
            let _g = r1.acquire(Provider::Claude, "sid").await;
            o1.lock().push(1);
            tokio::time::sleep(Duration::from_millis(50)).await;
            o1.lock().push(2);
        });

        // ensure h1 grabs the lock first
        tokio::time::sleep(Duration::from_millis(10)).await;

        let r2 = reg.clone();
        let o2 = order.clone();
        let h2 = tokio::spawn(async move {
            let _g = r2.acquire(Provider::Claude, "sid").await;
            o2.lock().push(3);
        });

        h1.await.unwrap();
        h2.await.unwrap();
        assert_eq!(*order.lock(), vec![1, 2, 3]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn different_keys_run_concurrently() {
        let reg = Arc::new(ResumeLeaseRegistry::new());

        let r1 = reg.clone();
        let h1 = tokio::spawn(async move {
            let _g = r1.acquire(Provider::Claude, "sid-a").await;
            tokio::time::sleep(Duration::from_millis(40)).await;
        });
        let r2 = reg.clone();
        let h2 = tokio::spawn(async move {
            let _g = r2.acquire(Provider::Claude, "sid-b").await;
            tokio::time::sleep(Duration::from_millis(40)).await;
        });

        let start = std::time::Instant::now();
        h1.await.unwrap();
        h2.await.unwrap();
        // both ran in parallel — total elapsed should be ~40ms,
        // not ~80ms.
        assert!(
            start.elapsed() < Duration::from_millis(75),
            "leases for different keys serialized: {:?}",
            start.elapsed()
        );
    }
}
