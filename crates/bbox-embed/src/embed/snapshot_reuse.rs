//! Snapshot-identity vector reuse.
//!
//! `project_file_v2` and `symbol_v2` entity ids embed the code-source
//! `snapshot_id`, so every generation activation re-mints the identity of
//! every chunk in the project even when the content (and therefore the
//! vector) is unchanged. The queue's `contains_active` dedupe is keyed on the
//! full entity id, so before this module each re-mint re-embedded the whole
//! project through the provider: the 2026-08 incident billed a full-project
//! re-embed per activation for two weeks (~50-60 USD/day) and left 875k
//! stale-identity rows active in one partition.
//!
//! The reuse index maps each snapshot-scoped identity, minus its
//! `snapshot_id`, to the snapshot id of the row the store currently holds.
//! On a dedupe miss for a snapshot-scoped entity, the queue asks this index
//! for the prior snapshot, reconstructs the old entity id, and re-keys the
//! stored row to the new id (`VectorStore::clone_active`) instead of calling
//! the provider. The content hash rides the reuse key AND is re-verified by
//! `clone_active`, so a reuse can only ever bind identical content.
//!
//! Staleness is tolerated in both directions: a missing index entry costs
//! one redundant provider call (yesterday's behavior), and a stale entry
//! fails the `clone_active` row lookup and falls through to a normal embed.

use std::collections::HashMap;

use bbox_corpus_core::entity_ref::EntityRef;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};

/// Reuse identity for one snapshot-scoped entity: the entity id with the
/// snapshot component removed, hashed to keep the resident index small.
fn reuse_key(kind: &str, fields: &[&str]) -> u128 {
    let mut hasher = Sha256::new();
    hasher.update(kind.as_bytes());
    for field in fields {
        hasher.update([0u8]);
        hasher.update(field.as_bytes());
    }
    let digest = hasher.finalize();
    u128::from_be_bytes(digest[..16].try_into().expect("sha256 is 32 bytes"))
}

/// Parse a snapshot-scoped entity id into (reuse key, snapshot id). Non-v2
/// entity types return None and take no part in reuse.
fn reuse_components(entity_id: &str) -> Option<(u128, String)> {
    if !entity_id.starts_with("project_file_v2:") && !entity_id.starts_with("symbol_v2:") {
        return None;
    }
    match EntityRef::parse(entity_id).ok()? {
        EntityRef::ProjectFileV2 {
            project_id,
            snapshot_id,
            rel_path_hash,
            chunk_hash,
            occurrence_idx,
        } => Some((
            reuse_key(
                "pfv2",
                &[
                    &project_id,
                    &rel_path_hash,
                    &chunk_hash,
                    &occurrence_idx.to_string(),
                ],
            ),
            snapshot_id,
        )),
        EntityRef::SymbolV2 {
            project_id,
            snapshot_id,
            qualified_name,
            defn_hash,
        } => Some((
            reuse_key("symv2", &[&project_id, &qualified_name, &defn_hash]),
            snapshot_id,
        )),
        _ => None,
    }
}

/// Rebuild `entity_id` with its snapshot component replaced.
fn with_snapshot(entity_id: &str, snapshot_id: &str) -> Option<String> {
    match EntityRef::parse(entity_id).ok()? {
        EntityRef::ProjectFileV2 {
            project_id,
            rel_path_hash,
            chunk_hash,
            occurrence_idx,
            ..
        } => EntityRef::ProjectFileV2 {
            project_id,
            snapshot_id: snapshot_id.to_string(),
            rel_path_hash,
            chunk_hash,
            occurrence_idx,
        }
        .try_render()
        .ok(),
        EntityRef::SymbolV2 {
            project_id,
            qualified_name,
            defn_hash,
            ..
        } => EntityRef::SymbolV2 {
            project_id,
            snapshot_id: snapshot_id.to_string(),
            qualified_name,
            defn_hash,
        }
        .try_render()
        .ok(),
        _ => None,
    }
}

#[derive(Default)]
struct RouteReuse {
    built: bool,
    /// reuse key -> snapshot id of the row the store holds for it.
    snapshots: HashMap<u128, String>,
}

/// Per-vector-route reuse index. Built lazily from the store's active rows
/// the first time a route sees a snapshot-scoped dedupe miss, then maintained
/// on enqueues and successful re-keys.
#[derive(Default)]
pub(crate) struct SnapshotReuseIndex {
    routes: Mutex<HashMap<String, RouteReuse>>,
}

impl SnapshotReuseIndex {
    /// Try to satisfy a dedupe miss by re-keying the prior snapshot's row.
    /// Returns true when the store now holds an active row under
    /// `entity_id` and no provider call is needed.
    pub(crate) fn try_reuse(
        &self,
        store: &bbox_vectors::VectorStore,
        vector_route: &str,
        entity_id: &str,
        content_hash: &str,
    ) -> bool {
        let Some((key, new_snapshot)) = reuse_components(entity_id) else {
            return false;
        };
        let old_snapshot = {
            let mut routes = self.routes.lock();
            let route = routes.entry(vector_route.to_string()).or_default();
            if !route.built {
                let mut snapshots = HashMap::new();
                let scan = store.for_each_active(vector_route, |row_entity, _hash| {
                    if let Some((row_key, row_snapshot)) = reuse_components(row_entity) {
                        snapshots.insert(row_key, row_snapshot);
                    }
                });
                if scan.is_err() {
                    // Leave unbuilt: the next miss retries the scan.
                    return false;
                }
                route.snapshots = snapshots;
                route.built = true;
            }
            match route.snapshots.get(&key) {
                Some(snapshot) if *snapshot != new_snapshot => snapshot.clone(),
                _ => return false,
            }
        };
        let Some(old_entity_id) = with_snapshot(entity_id, &old_snapshot) else {
            return false;
        };
        match store.clone_active(vector_route, &old_entity_id, entity_id, content_hash) {
            Ok(true) => {
                self.record(vector_route, entity_id);
                tracing::debug!(
                    vector_route,
                    entity_id,
                    old_entity_id,
                    "reused stored vector across snapshot re-mint (no provider call)"
                );
                true
            }
            Ok(false) => false,
            Err(err) => {
                tracing::warn!(
                    vector_route,
                    entity_id,
                    error = %err,
                    "snapshot vector reuse failed; falling back to a provider embed"
                );
                false
            }
        }
    }

    /// Record that the store is about to hold (or now holds) a row for this
    /// entity, keeping the index current for future re-mints. A recorded
    /// entry whose embed never lands is harmless: the re-key path re-checks
    /// the store and falls back to a provider embed.
    pub(crate) fn record(&self, vector_route: &str, entity_id: &str) {
        let Some((key, snapshot)) = reuse_components(entity_id) else {
            return;
        };
        let mut routes = self.routes.lock();
        let route = routes.entry(vector_route.to_string()).or_default();
        // An unbuilt route gets its entries from the store scan later;
        // recording into it anyway is correct (insert wins are idempotent
        // with the scan's view or newer).
        route.snapshots.insert(key, snapshot);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pfv2(snapshot: &str, chunk: &str) -> String {
        format!("project_file_v2:proj-a:{snapshot}:relhash:{chunk}:0")
    }

    #[test]
    fn reuse_components_strip_snapshot_and_keep_content_identity() {
        let (key_a, snap_a) = reuse_components(&pfv2("snap-1", "chunk-1")).unwrap();
        let (key_b, snap_b) = reuse_components(&pfv2("snap-2", "chunk-1")).unwrap();
        let (key_c, _) = reuse_components(&pfv2("snap-1", "chunk-2")).unwrap();
        assert_eq!(key_a, key_b, "snapshot must not participate in the key");
        assert_ne!(key_a, key_c, "content identity must participate");
        assert_eq!(snap_a, "snap-1");
        assert_eq!(snap_b, "snap-2");
        assert!(reuse_components("commit:repo:abc").is_none());
    }

    #[test]
    fn with_snapshot_rebuilds_the_exact_ref() {
        let rebuilt = with_snapshot(&pfv2("snap-1", "chunk-1"), "snap-9").unwrap();
        assert_eq!(rebuilt, pfv2("snap-9", "chunk-1"));
    }

    #[test]
    fn try_reuse_rekeys_across_snapshots_without_a_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let store = bbox_vectors::VectorStore::open(tmp.path()).unwrap();
        let route = "voyage-test";
        let old_id = pfv2("snap-1", "chunk-1");
        let new_id = pfv2("snap-2", "chunk-1");
        store
            .upsert(route, &old_id, "hash-1", vec![1.0, 0.0])
            .unwrap();

        let index = SnapshotReuseIndex::default();
        assert!(index.try_reuse(&store, route, &new_id, "hash-1"));
        assert!(store.contains_active(route, &new_id, "hash-1").unwrap());
        assert!(!store.contains_active(route, &old_id, "hash-1").unwrap());

        // A second re-mint reuses the freshly recorded snapshot.
        let third_id = pfv2("snap-3", "chunk-1");
        assert!(index.try_reuse(&store, route, &third_id, "hash-1"));
        assert!(store.contains_active(route, &third_id, "hash-1").unwrap());

        // Changed content refuses reuse and demands a real embed.
        let changed = pfv2("snap-4", "chunk-9");
        assert!(!index.try_reuse(&store, route, &changed, "hash-9"));
        // Non-snapshot-scoped ids take no part.
        assert!(!index.try_reuse(&store, route, "commit:repo:abc", "hash-1"));
    }
}
