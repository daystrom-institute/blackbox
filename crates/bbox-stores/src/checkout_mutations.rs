//! Durable pending checkout mutations: repo-owned file writes/deletes the
//! daemon computed and validated but cannot apply itself (zero checkout
//! authority). The checkout-owner collector polls pending mutations over the
//! authenticated producer channel, applies them byte-for-byte, and acks.
//!
//! Status lifecycle: `pending` -> `applied` | `failed` (both terminal; a
//! failed mutation stays visible for triage instead of poison-looping the
//! collector). The store is the delivery mechanism only — the mutation's
//! content is already fully validated at enqueue time.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use bbox_code_source::{
    CHECKOUT_MUTATION_SCHEMA_VERSION, CheckoutMutationV1, MAX_CHECKOUT_MUTATIONS_PER_POLL,
};
use bbox_corpus_core::identity::PublishedScope;

use crate::store_persister::StoreSnapshot;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckoutMutationStore {
    pub version: u32,
    pub mutations: Vec<PendingCheckoutMutation>,
}

impl Default for CheckoutMutationStore {
    fn default() -> Self {
        Self {
            version: 1,
            mutations: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckoutMutationStatus {
    Pending,
    Applied,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingCheckoutMutation {
    pub mutation: CheckoutMutationV1,
    pub status: CheckoutMutationStatus,
    pub attempts: u32,
    pub last_error: Option<String>,
    pub acked_at: Option<String>,
    pub ack_content_sha256: Option<String>,
}

pub struct CheckoutMutations {
    store: CheckoutMutationStore,
}

impl StoreSnapshot for CheckoutMutations {
    type Snapshot = CheckoutMutationStore;

    fn snapshot(&self) -> Result<Self::Snapshot> {
        Ok(self.store.clone())
    }
}

impl CheckoutMutations {
    pub fn open(store_path: &Path) -> Result<Self> {
        let store = if store_path.exists() {
            let raw = std::fs::read_to_string(store_path)
                .with_context(|| format!("reading {}", store_path.display()))?;
            serde_json::from_str(&raw)
                .with_context(|| format!("parsing {}", store_path.display()))?
        } else {
            CheckoutMutationStore::default()
        };
        Ok(Self { store })
    }

    /// Queue a mutation for checkout-owner delivery. Ids are minted by the
    /// caller; an id that already exists is a no-op so retried enqueues
    /// stay idempotent.
    pub fn enqueue(&mut self, mutation: CheckoutMutationV1) -> Result<bool> {
        mutation.validate().map_err(|error| anyhow::anyhow!("{error}"))?;
        if self
            .store
            .mutations
            .iter()
            .any(|pending| pending.mutation.mutation_id == mutation.mutation_id)
        {
            return Ok(false);
        }
        self.store.mutations.push(PendingCheckoutMutation {
            mutation,
            status: CheckoutMutationStatus::Pending,
            attempts: 0,
            last_error: None,
            acked_at: None,
            ack_content_sha256: None,
        });
        Ok(true)
    }

    /// Pending mutations whose scope the producer grant covers, oldest
    /// first, capped per poll. The deferred count covers both the cap and
    /// pending mutations outside the grant, so operators can see a grant
    /// that does not cover an enqueued scope.
    pub fn poll(&self, granted_scopes: &BTreeSet<PublishedScope>) -> (Vec<CheckoutMutationV1>, u64) {
        let mut covered = Vec::new();
        let mut deferred = 0u64;
        for pending in &self.store.mutations {
            if pending.status != CheckoutMutationStatus::Pending {
                continue;
            }
            if !granted_scopes.contains(&pending.mutation.scope) {
                deferred += 1;
                continue;
            }
            if covered.len() >= MAX_CHECKOUT_MUTATIONS_PER_POLL {
                deferred += 1;
                continue;
            }
            covered.push(pending.mutation.clone());
        }
        (covered, deferred)
    }

    /// Terminal ack from the checkout owner. Unknown ids and acks for
    /// already-terminal mutations return false so the caller can answer
    /// `unknown_mutation` without rewriting history.
    pub fn ack(
        &mut self,
        mutation_id: &str,
        outcome: &str,
        error: Option<String>,
        content_sha256: Option<String>,
        now: &str,
    ) -> Result<bool> {
        let Some(pending) = self
            .store
            .mutations
            .iter_mut()
            .find(|pending| pending.mutation.mutation_id == mutation_id)
        else {
            return Ok(false);
        };
        if pending.status != CheckoutMutationStatus::Pending {
            return Ok(false);
        }
        pending.attempts += 1;
        pending.acked_at = Some(now.to_string());
        match outcome {
            "applied" => {
                pending.status = CheckoutMutationStatus::Applied;
                pending.ack_content_sha256 = content_sha256;
                pending.last_error = None;
            }
            "failed" => {
                pending.status = CheckoutMutationStatus::Failed;
                pending.last_error = error;
            }
            other => anyhow::bail!("unknown checkout mutation outcome {other}"),
        }
        Ok(true)
    }

    pub fn pending_count(&self) -> usize {
        self.store
            .mutations
            .iter()
            .filter(|pending| pending.status == CheckoutMutationStatus::Pending)
            .count()
    }

    /// The scope a mutation targets, regardless of status. Ack handlers
    /// check it against the producer grant before accepting the outcome.
    pub fn scope_of(&self, mutation_id: &str) -> Option<PublishedScope> {
        self.store
            .mutations
            .iter()
            .find(|pending| pending.mutation.mutation_id == mutation_id)
            .map(|pending| pending.mutation.scope.clone())
    }

    /// Latest pending mutation for one repo-relative path. Mutation arms
    /// overlay this on the published view so chained writes inside one
    /// collector cycle see each other.
    pub fn pending_for_path(&self, relative_path: &str) -> Option<&PendingCheckoutMutation> {
        self.store
            .mutations
            .iter()
            .rev()
            .find(|pending| {
                pending.status == CheckoutMutationStatus::Pending
                    && pending.mutation.relative_path == relative_path
            })
    }

    /// Every pending write's (relative_path, content_json) pair.
    pub fn pending_writes(&self) -> impl Iterator<Item = (&str, &str)> {
        self.store
            .mutations
            .iter()
            .filter(|pending| {
                pending.status == CheckoutMutationStatus::Pending
                    && pending.mutation.mode == "write"
            })
            .filter_map(|pending| {
                pending
                    .mutation
                    .content_json
                    .as_deref()
                    .map(|content| (pending.mutation.relative_path.as_str(), content))
            })
    }

    pub fn failed(&self) -> Vec<&PendingCheckoutMutation> {
        self.store
            .mutations
            .iter()
            .filter(|pending| pending.status == CheckoutMutationStatus::Failed)
            .collect()
    }

    /// Mint a mutation id (`cm-<16hex>`), collision-checked against every
    /// known mutation regardless of status.
    pub fn mint_id(&self) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        loop {
            let mut h = DefaultHasher::new();
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
                .hash(&mut h);
            std::process::id().hash(&mut h);
            std::thread::current().id().hash(&mut h);
            let id = format!("cm-{:016x}", h.finish());
            if !self
                .store
                .mutations
                .iter()
                .any(|pending| pending.mutation.mutation_id == id)
            {
                return id;
            }
        }
    }

    /// Build and enqueue a validated mutation in one step.
    pub fn enqueue_file_mutation(
        &mut self,
        scope: PublishedScope,
        relative_path: String,
        mode: &str,
        content_json: Option<String>,
        reason: String,
        now: String,
    ) -> Result<String> {
        let mutation = CheckoutMutationV1 {
            schema_version: CHECKOUT_MUTATION_SCHEMA_VERSION,
            mutation_id: self.mint_id(),
            scope,
            relative_path,
            mode: mode.to_string(),
            content_json,
            reason,
            enqueued_at: now,
        };
        let id = mutation.mutation_id.clone();
        self.enqueue(mutation)?;
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> PublishedScope {
        PublishedScope::try_new("repo-family", ".").unwrap()
    }

    fn mutation(id: &str) -> CheckoutMutationV1 {
        CheckoutMutationV1 {
            schema_version: CHECKOUT_MUTATION_SCHEMA_VERSION,
            mutation_id: id.into(),
            scope: scope(),
            relative_path: ".bbox/gaps/gap-0123abcd.json".into(),
            mode: "write".into(),
            content_json: Some("{}".into()),
            reason: "test".into(),
            enqueued_at: "2026-08-12T00:00:00Z".into(),
        }
    }

    #[test]
    fn enqueue_poll_ack_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mutations.json");
        let mut store = CheckoutMutations::open(&path).unwrap();
        assert!(store.enqueue(mutation("cm-0000000000000001")).unwrap());
        assert!(!store.enqueue(mutation("cm-0000000000000001")).unwrap());

        let granted = BTreeSet::from([scope()]);
        let (mutations, deferred) = store.poll(&granted);
        assert_eq!(mutations.len(), 1);
        assert_eq!(deferred, 0);
        let (mutations, deferred) = store.poll(&BTreeSet::new());
        assert!(mutations.is_empty());
        assert_eq!(deferred, 1);

        assert!(
            store
                .ack(
                    "cm-0000000000000001",
                    "applied",
                    None,
                    Some("a".repeat(64)),
                    "2026-08-12T00:01:00Z",
                )
                .unwrap()
        );
        assert_eq!(store.pending_count(), 0);
        // Terminal mutations do not re-poll and do not re-ack.
        let (mutations, deferred) = store.poll(&granted);
        assert!(mutations.is_empty());
        assert_eq!(deferred, 0);
        assert!(
            !store
                .ack("cm-0000000000000001", "applied", None, None, "2026-08-12T00:02:00Z")
                .unwrap()
        );
        assert!(
            !store
                .ack("cm-ffffffffffffffff", "applied", None, None, "2026-08-12T00:02:00Z")
                .unwrap()
        );
    }

    #[test]
    fn failed_ack_is_terminal_and_visible() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mutations.json");
        let mut store = CheckoutMutations::open(&path).unwrap();
        store.enqueue(mutation("cm-0000000000000002")).unwrap();
        assert!(
            store
                .ack(
                    "cm-0000000000000002",
                    "failed",
                    Some("disk full".into()),
                    None,
                    "2026-08-12T00:01:00Z",
                )
                .unwrap()
        );
        assert_eq!(store.pending_count(), 0);
        assert_eq!(store.failed().len(), 1);
        assert_eq!(
            store.failed()[0].last_error.as_deref(),
            Some("disk full")
        );
    }

    #[test]
    fn enqueue_rejects_invalid_mutations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mutations.json");
        let mut store = CheckoutMutations::open(&path).unwrap();
        let mut bad = mutation("cm-0000000000000003");
        bad.relative_path = "src/main.rs".into();
        assert!(store.enqueue(bad).is_err());
    }

    #[test]
    fn minted_ids_are_unique_and_shaped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mutations.json");
        let store = CheckoutMutations::open(&path).unwrap();
        let id = store.mint_id();
        assert!(id.starts_with("cm-"));
        assert_eq!(id.len(), 19);
        assert_ne!(id, store.mint_id());
    }
}
