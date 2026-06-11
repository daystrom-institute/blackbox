//! Daemon-wide resume lease registry.
//!
//! `bro_resume` spawns a fresh `claude --resume <id>` process per call
//! — two concurrent resumes on the same provider session race the
//! session jsonl writes and corrupt the transcript. This registry
//! exposes a single async mutex per `(provider, session_id)` key so
//! every dispatch path can serialize against the same invariant.
//!
//! Operator-facing paths such as ad-hoc `bro_resume`, workflow durable
//! actors, and team advisor resumes use the non-blocking path and fail
//! fast with a `bro_wait` / `bro_cancel` instruction instead of
//! silently queuing a follow-up that can outlive the caller's tool timeout.
//!
//! Callers hold the guard across the full dispatch (`spawn_task` →
//! `wait_for_task`). Drop the guard to release; nothing else needs
//! to be done.

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

    /// Try to acquire the resume lease without waiting. This is the
    /// operator-facing path: if a session already has an in-flight
    /// resume, callers should `bro_wait` or `bro_cancel` that task
    /// instead of silently queuing another turn that may outlive the
    /// caller's tool timeout.
    pub fn try_acquire(&self, provider: Provider, session_id: &str) -> Option<OwnedMutexGuard<()>> {
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
        lock.try_lock_owned().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_acquire_returns_none_when_same_key_busy() {
        let reg = ResumeLeaseRegistry::new();
        let first = reg
            .try_acquire(Provider::Glm, "sid")
            .expect("first lease should acquire");
        assert!(reg.try_acquire(Provider::Glm, "sid").is_none());
        drop(first);
        assert!(reg.try_acquire(Provider::Glm, "sid").is_some());
    }
}
