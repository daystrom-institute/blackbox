use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::entity_ref::EntityRef;

const DEFAULT_CACHE_SIZE: usize = 100;
const EVICT_BATCH: usize = 30;

/// D3 uses one process-wide lane because the current rmcp tool handler
/// surface does not expose the MCP session id to individual tool calls.
/// The cache internals remain keyed so a later phase can swap this for
/// true per-session keys without changing callers.
pub const PROCESS_SESSION_KEY: &str = "process";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathStep {
    pub from: EntityRef,
    pub edge_kind: String,
    pub to: EntityRef,
    pub direction: PathDirection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathDirection {
    Out,
    In,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedPath {
    pub id: String,
    pub steps: Vec<PathStep>,
    pub accessed_at: u64,
}

#[derive(Default)]
struct SessionPathCache {
    next_id: u64,
    access_clock: u64,
    entries: Vec<CachedPath>,
}

pub struct PathCache {
    cache_size: usize,
    sessions: HashMap<String, SessionPathCache>,
}

impl Default for PathCache {
    fn default() -> Self {
        Self::new(DEFAULT_CACHE_SIZE)
    }
}

impl PathCache {
    pub fn new(cache_size: usize) -> Self {
        Self {
            cache_size,
            sessions: HashMap::new(),
        }
    }

    pub fn insert_paths(
        &mut self,
        session_key: &str,
        paths: Vec<Vec<PathStep>>,
    ) -> Vec<CachedPath> {
        let session = self.sessions.entry(session_key.to_string()).or_default();
        let mut cached = Vec::new();
        for steps in paths {
            session.next_id += 1;
            session.access_clock += 1;
            let path = CachedPath {
                id: format!("P{}", session.next_id),
                steps,
                accessed_at: session.access_clock,
            };
            session.entries.push(path.clone());
            cached.push(path);
        }
        while session.entries.len() > self.cache_size {
            session.entries.sort_by_key(|path| path.accessed_at);
            let evict_count = EVICT_BATCH.min(session.entries.len());
            session.entries.drain(0..evict_count);
        }
        cached
    }

    pub fn get(&mut self, session_key: &str, path_id: &str) -> Option<CachedPath> {
        let session = self.sessions.get_mut(session_key)?;
        session.access_clock += 1;
        let accessed_at = session.access_clock;
        let path = session.entries.iter_mut().find(|path| path.id == path_id)?;
        path.accessed_at = accessed_at;
        Some(path.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(n: u64) -> PathStep {
        PathStep {
            from: EntityRef::parse(&format!("knowledge:from{n}")).unwrap(),
            edge_kind: "SUPERSEDES".into(),
            to: EntityRef::parse(&format!("knowledge:to{n}")).unwrap(),
            direction: PathDirection::Out,
        }
    }

    #[test]
    fn path_cache_allocates_monotonic_ids_and_reads_non_consumingly() {
        let mut cache = PathCache::new(100);
        let inserted = cache.insert_paths(PROCESS_SESSION_KEY, vec![vec![step(1)], vec![step(2)]]);
        assert_eq!(inserted[0].id, "P1");
        assert_eq!(inserted[1].id, "P2");
        assert!(cache.get(PROCESS_SESSION_KEY, "P1").is_some());
        assert!(cache.get(PROCESS_SESSION_KEY, "P1").is_some());
    }

    #[test]
    fn path_cache_evicts_least_recently_used_batch_on_overflow() {
        let mut cache = PathCache::new(100);
        for i in 0..100 {
            cache.insert_paths(PROCESS_SESSION_KEY, vec![vec![step(i)]]);
        }
        assert!(cache.get(PROCESS_SESSION_KEY, "P1").is_some());
        cache.insert_paths(PROCESS_SESSION_KEY, vec![vec![step(101)]]);
        assert!(cache.get(PROCESS_SESSION_KEY, "P1").is_some());
        assert!(cache.get(PROCESS_SESSION_KEY, "P2").is_none());
        assert!(cache.get(PROCESS_SESSION_KEY, "P31").is_none());
        assert!(cache.get(PROCESS_SESSION_KEY, "P32").is_some());
        assert!(cache.get(PROCESS_SESSION_KEY, "P101").is_some());
    }
}
