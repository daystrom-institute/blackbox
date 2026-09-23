//! Durable code-collection producer scope claims.
//!
//! Configured producer scopes remain authoritative. This store records
//! first-onboard claims for otherwise unassigned published scopes so daemon
//! restarts and config reloads preserve the same producer assignment.

use std::path::Path;

use anyhow::{Context, Result, bail};
use bbox_corpus_core::identity::PublishedScope;
use serde::{Deserialize, Serialize};

use crate::store_persister::StoreSnapshot;

pub const PRODUCER_CLAIM_STORE_VERSION: u32 = 1;
pub const MAX_PRODUCER_CLAIMS_STORE_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProducerScopeClaim {
    pub producer_id: String,
    pub scope: PublishedScope,
    pub claimed_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProducerClaimStore {
    pub version: u32,
    pub claims: Vec<ProducerScopeClaim>,
}

impl Default for ProducerClaimStore {
    fn default() -> Self {
        Self {
            version: PRODUCER_CLAIM_STORE_VERSION,
            claims: Vec::new(),
        }
    }
}

pub struct ProducerClaims {
    store: ProducerClaimStore,
}

impl StoreSnapshot for ProducerClaims {
    type Snapshot = ProducerClaimStore;

    fn snapshot(&self) -> Result<Self::Snapshot> {
        Ok(self.store.clone())
    }
}

impl ProducerClaims {
    pub fn open(store_path: &Path) -> Result<Self> {
        let store = if store_path.exists() {
            let metadata = std::fs::metadata(store_path)
                .with_context(|| format!("inspecting {}", store_path.display()))?;
            if metadata.len() > MAX_PRODUCER_CLAIMS_STORE_BYTES {
                bail!(
                    "producer claims store exceeds {} bytes in {}",
                    MAX_PRODUCER_CLAIMS_STORE_BYTES,
                    store_path.display()
                );
            }
            let raw = std::fs::read_to_string(store_path)
                .with_context(|| format!("reading {}", store_path.display()))?;
            let store: ProducerClaimStore = serde_json::from_str(&raw)
                .with_context(|| format!("parsing {}", store_path.display()))?;
            if store.version != PRODUCER_CLAIM_STORE_VERSION {
                bail!(
                    "unsupported producer claims store version {} in {}",
                    store.version,
                    store_path.display()
                );
            }
            store
        } else {
            ProducerClaimStore::default()
        };
        Ok(Self { store })
    }

    pub fn records_snapshot(&self) -> ProducerClaimStore {
        self.store.clone()
    }

    pub fn claims(&self) -> &[ProducerScopeClaim] {
        &self.store.claims
    }

    pub fn claim(
        &mut self,
        producer_id: &str,
        scope: PublishedScope,
        claimed_at: String,
    ) -> Result<bool> {
        if let Some(existing) = self.store.claims.iter().find(|claim| claim.scope == scope) {
            if existing.producer_id == producer_id {
                return Ok(false);
            }
            bail!(
                "producer scope is already claimed by {}",
                existing.producer_id
            );
        }
        self.store.claims.push(ProducerScopeClaim {
            producer_id: producer_id.to_string(),
            scope,
            claimed_at,
        });
        Ok(true)
    }

    pub fn revoke(&mut self, producer_id: &str, scope: &PublishedScope) -> bool {
        let before = self.store.claims.len();
        self.store
            .claims
            .retain(|claim| claim.producer_id != producer_id || claim.scope != *scope);
        self.store.claims.len() != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store_persister::StorePersister;
    use parking_lot::RwLock;
    use std::sync::Arc;

    fn scope() -> PublishedScope {
        PublishedScope::try_new("repo-claims", ".").unwrap()
    }

    #[test]
    fn absent_store_opens_as_never_provisioned_empty_state() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("producer-claims.json");

        let store = ProducerClaims::open(&path).unwrap();

        assert_eq!(store.records_snapshot(), ProducerClaimStore::default());
        assert!(!path.exists());
    }

    #[test]
    fn empty_provisioned_store_reopens() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("producer-claims.json");
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&ProducerClaimStore::default()).unwrap(),
        )
        .unwrap();

        let store = ProducerClaims::open(&path).unwrap();

        assert!(store.claims().is_empty());
    }

    #[test]
    fn unknown_store_version_refuses() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("producer-claims.json");
        std::fs::write(&path, br#"{"version":0,"claims":[]}"#).unwrap();

        let error = ProducerClaims::open(&path)
            .err()
            .expect("an unknown version must fail closed");

        assert!(
            error
                .to_string()
                .contains("unsupported producer claims store version 0")
        );
    }

    #[test]
    fn oversized_store_refuses_before_reading() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("producer-claims.json");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_PRODUCER_CLAIMS_STORE_BYTES + 1).unwrap();

        let error = ProducerClaims::open(&path)
            .err()
            .expect("an oversized claims store must fail closed");

        assert!(error.to_string().contains("producer claims store exceeds"));
    }

    #[test]
    fn duplicate_claim_by_same_producer_is_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("producer-claims.json");
        let mut store = ProducerClaims::open(&path).unwrap();

        assert!(
            store
                .claim("producer-a", scope(), "2026-09-23T00:00:00Z".into())
                .unwrap()
        );
        assert!(
            !store
                .claim("producer-a", scope(), "2026-09-23T00:00:01Z".into())
                .unwrap()
        );
        assert_eq!(store.claims().len(), 1);
    }

    #[test]
    fn durable_persist_survives_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("producer-claims.json");
        let store = Arc::new(RwLock::new(ProducerClaims::open(&path).unwrap()));
        let persister = StorePersister::spawn("producer-claims-test", store.clone(), path.clone());
        store
            .write()
            .claim("producer-a", scope(), "2026-09-23T00:00:00Z".into())
            .unwrap();

        persister.flush_blocking().unwrap();

        let reopened = ProducerClaims::open(&path).unwrap();
        assert_eq!(reopened.claims().len(), 1);
        assert_eq!(reopened.claims()[0].producer_id, "producer-a");
    }
}
