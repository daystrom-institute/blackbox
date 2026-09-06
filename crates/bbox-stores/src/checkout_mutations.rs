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
    /// Present for writes whose read/modify/write base is tracked across delivery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publication: Option<MutationPublication>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MutationPublication {
    pub base_content_json: Option<String>,
    #[serde(default)]
    pub observed: bool,
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
        mutation
            .validate()
            .map_err(|error| anyhow::anyhow!("{error}"))?;
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
            publication: None,
        });
        Ok(true)
    }

    /// Pending mutations whose scope the producer grant covers, oldest
    /// first, capped per poll. The deferred count covers both the cap and
    /// pending mutations outside the grant, so operators can see a grant
    /// that does not cover an enqueued scope.
    pub fn poll(
        &self,
        granted_scopes: &BTreeSet<PublishedScope>,
    ) -> (Vec<CheckoutMutationV1>, u64) {
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
        self.store.mutations.iter().rev().find(|pending| {
            pending.status == CheckoutMutationStatus::Pending
                && pending.mutation.relative_path == relative_path
        })
    }

    /// Latest outstanding write, isolated by durable scope. Applied tracked
    /// writes remain an overlay until their content is observed in publication.
    pub fn outstanding_writes(&self) -> impl Iterator<Item = &PendingCheckoutMutation> {
        self.outstanding_intents()
            .filter(|row| row.mutation.mode == "write")
    }

    pub fn outstanding_intents(&self) -> impl Iterator<Item = &PendingCheckoutMutation> {
        self.store.mutations.iter().filter(|row| {
            row.status != CheckoutMutationStatus::Failed
                && match &row.publication {
                    Some(publication) => !publication.observed,
                    None => row.status == CheckoutMutationStatus::Pending,
                }
        })
    }

    /// Retire the overlay prefix whose exact content has reached publication.
    /// Delivery acknowledgement alone is insufficient: the owner may not have
    /// committed and published the delivered file yet.
    pub fn observe_publication(
        &mut self,
        scope: &PublishedScope,
        relative_path: &str,
        published: Option<&str>,
    ) -> bool {
        let last = self
            .store
            .mutations
            .iter()
            .enumerate()
            .rposition(|(index, row)| {
                &row.mutation.scope == scope
                    && row.mutation.relative_path == relative_path
                    && row.status != CheckoutMutationStatus::Failed
                    && (published.is_some()
                        || (row.status == CheckoutMutationStatus::Applied
                            && !self.store.mutations[..index].iter().any(|previous| {
                                &previous.mutation.scope == scope
                                    && previous.mutation.relative_path == relative_path
                                    && previous.status == CheckoutMutationStatus::Pending
                            })))
                    && row
                        .publication
                        .as_ref()
                        .is_some_and(|state| !state.observed)
                    && same_json(row.mutation.content_json.as_deref(), published)
            });
        let Some(last) = last else {
            return false;
        };
        let previous_base = self.store.mutations[last]
            .publication
            .as_ref()
            .unwrap()
            .base_content_json
            .clone();
        let mut changed = false;
        // A published prefix advances only this chain's expected base. Older
        // completed chains never exempt a later write from conflict detection.
        for row in &mut self.store.mutations[last + 1..] {
            if &row.mutation.scope == scope
                && row.mutation.relative_path == relative_path
                && let Some(publication) = &mut row.publication
                && !publication.observed
                && same_json(
                    publication.base_content_json.as_deref(),
                    previous_base.as_deref(),
                )
            {
                publication.base_content_json = published.map(str::to_owned);
            }
        }
        for row in &mut self.store.mutations[..=last] {
            if &row.mutation.scope == scope
                && row.mutation.relative_path == relative_path
                && let Some(publication) = &mut row.publication
                && !publication.observed
            {
                publication.observed = true;
                changed = true;
            }
        }
        changed
    }

    /// Choose a write base while the caller holds the queue's exclusive lock.
    /// A changed publication that does not incorporate the outstanding write
    /// is a conflict, never permission to overwrite either side.
    pub fn write_base(
        &mut self,
        scope: &PublishedScope,
        relative_path: &str,
        published: Option<&str>,
    ) -> Result<Option<String>> {
        self.observe_publication(scope, relative_path, published);
        let latest = self
            .outstanding_intents()
            .filter(|row| {
                &row.mutation.scope == scope && row.mutation.relative_path == relative_path
            })
            .last();
        let Some(latest) = latest else {
            return Ok(published.map(str::to_owned));
        };
        if let Some(publication) = &latest.publication
            && !same_json(publication.base_content_json.as_deref(), published)
        {
            anyhow::bail!(
                "error.checkout_mutation_conflict: published content changed while mutation {} \
                 is awaiting publication; reconcile that mutation in the owning checkout and \
                 publish it before retrying",
                latest.mutation.mutation_id
            );
        }
        Ok(latest.mutation.content_json.clone())
    }

    /// Validate every file before appending any, so a paired update cannot
    /// leave half its intent queued when the other file is invalid.
    pub fn enqueue_tracked_writes(
        &mut self,
        scope: PublishedScope,
        writes: Vec<(String, String, Option<String>)>,
        reason: String,
        now: String,
    ) -> Result<Vec<String>> {
        self.enqueue_tracked_mutations(
            scope,
            writes
                .into_iter()
                .map(|(path, content, base)| (path, Some(content), base))
                .collect(),
            reason,
            now,
        )
    }

    pub fn enqueue_tracked_mutations(
        &mut self,
        scope: PublishedScope,
        mutations: Vec<(String, Option<String>, Option<String>)>,
        reason: String,
        now: String,
    ) -> Result<Vec<String>> {
        let mut rows = Vec::new();
        let mut paths = BTreeSet::new();
        for (relative_path, content, base) in mutations {
            anyhow::ensure!(
                paths.insert(relative_path.clone()),
                "duplicate checkout mutation path in one edit"
            );
            let mutation = CheckoutMutationV1 {
                schema_version: CHECKOUT_MUTATION_SCHEMA_VERSION,
                mutation_id: self.mint_id(),
                scope: scope.clone(),
                relative_path,
                mode: if content.is_some() { "write" } else { "delete" }.into(),
                content_json: content,
                reason: reason.clone(),
                enqueued_at: now.clone(),
            };
            mutation
                .validate()
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            rows.push(PendingCheckoutMutation {
                mutation,
                status: CheckoutMutationStatus::Pending,
                attempts: 0,
                last_error: None,
                acked_at: None,
                ack_content_sha256: None,
                publication: Some(MutationPublication {
                    base_content_json: base,
                    observed: false,
                }),
            });
        }
        let ids = rows
            .iter()
            .map(|row| row.mutation.mutation_id.clone())
            .collect();
        self.store.mutations.extend(rows);
        Ok(ids)
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

fn same_json(left: Option<&str>, right: Option<&str>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => {
            if left == right {
                return true;
            }
            matches!(
                (serde_json::from_str::<serde_json::Value>(left),
                 serde_json::from_str::<serde_json::Value>(right)),
                (Ok(left), Ok(right)) if left == right
            )
        }
        _ => false,
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
    fn tracked_delete_waits_for_delivery_of_the_entire_path_prefix() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("mutations.json");
        let mut store = CheckoutMutations::open(&path).unwrap();
        let file = ".bbox/knowledge/1234567890abcdef.json";
        let write = store
            .enqueue_tracked_writes(
                scope(),
                vec![(file.into(), "{}".into(), None)],
                "create".into(),
                "now".into(),
            )
            .unwrap()[0]
            .clone();
        let delete = store
            .enqueue_tracked_mutations(
                scope(),
                vec![(file.into(), None, None)],
                "delete".into(),
                "now".into(),
            )
            .unwrap()[0]
            .clone();
        assert!(!store.observe_publication(&scope(), file, None));
        assert_eq!(store.write_base(&scope(), file, None).unwrap(), None);
        assert_eq!(store.outstanding_intents().count(), 2);
        store.ack(&delete, "applied", None, None, "now").unwrap();
        assert!(!store.observe_publication(&scope(), file, None));
        std::fs::write(
            &path,
            serde_json::to_vec(&store.snapshot().unwrap()).unwrap(),
        )
        .unwrap();
        let mut store = CheckoutMutations::open(&path).unwrap();
        assert_eq!(store.write_base(&scope(), file, None).unwrap(), None);
        assert_eq!(store.outstanding_intents().count(), 2);
        store.ack(&write, "applied", None, None, "now").unwrap();
        assert!(store.observe_publication(&scope(), file, None));
        assert_eq!(store.outstanding_intents().count(), 0);
    }

    #[test]
    fn tracked_delete_is_scope_isolated_and_mixed_batches_validate_before_enqueue() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let mut store = CheckoutMutations::open(&root.join("mutations.json")).unwrap();
        let file = ".bbox/knowledge/1234567890abcdef.json";
        let other = PublishedScope::try_new("repo_example", "nested").unwrap();
        store
            .enqueue_tracked_mutations(
                scope(),
                vec![(file.into(), None, Some("{}".into()))],
                "delete".into(),
                "now".into(),
            )
            .unwrap();
        assert_eq!(store.write_base(&scope(), file, Some("{}")).unwrap(), None);
        assert_eq!(
            store
                .write_base(&other, file, Some("{}"))
                .unwrap()
                .as_deref(),
            Some("{}")
        );
        let count = store.pending_count();
        assert!(
            store
                .enqueue_tracked_mutations(
                    scope(),
                    vec![
                        (".bbox/knowledge/other.json".into(), None, Some("{}".into())),
                        ("../invalid.json".into(), Some("{}".into()), None)
                    ],
                    "invalid pair".into(),
                    "now".into()
                )
                .is_err()
        );
        assert_eq!(store.pending_count(), count);
    }

    #[test]
    fn an_old_completed_chain_never_authorizes_a_new_publication_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = CheckoutMutations::open(&dir.path().join("mutations.json")).unwrap();
        let file = ".bbox/gaps/gap-0123abcd.json";
        let b = r#"{"title":"B"}"#;
        let c = r#"{"title":"C"}"#;
        let d = r#"{"title":"D"}"#;
        store
            .enqueue_tracked_writes(
                scope(),
                vec![(file.into(), b.into(), Some("{}".into()))],
                "first".into(),
                "2026-09-06T00:00:00Z".into(),
            )
            .unwrap();
        assert!(store.observe_publication(&scope(), file, Some(b)));
        store
            .enqueue_tracked_writes(
                scope(),
                vec![(file.into(), d.into(), Some(c.into()))],
                "second".into(),
                "2026-09-06T00:00:00Z".into(),
            )
            .unwrap();
        assert!(
            store
                .write_base(&scope(), file, Some(b))
                .unwrap_err()
                .to_string()
                .contains("checkout_mutation_conflict")
        );
        let count = store.pending_count();
        assert!(
            store
                .enqueue_tracked_writes(
                    scope(),
                    vec![(file.into(), b.into(), None), (file.into(), c.into(), None)],
                    "duplicate".into(),
                    "2026-09-06T00:00:00Z".into()
                )
                .is_err()
        );
        assert_eq!(store.pending_count(), count);
    }

    #[test]
    fn tracked_writes_survive_ack_and_restart_and_recognize_intermediate_publications() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("mutations.json");
        let mut store = CheckoutMutations::open(&path).unwrap();
        let file = ".bbox/gaps/gap-0123abcd.json";
        let original = r#"{"title":"old"}"#;
        let first = r#"{"title":"new"}"#;
        let second = r#"{"title":"new","notes":"later"}"#;
        let enqueue = |store: &mut CheckoutMutations, content: &str| {
            store
                .enqueue_tracked_writes(
                    scope(),
                    vec![(file.into(), content.into(), Some(original.into()))],
                    "test".into(),
                    "2026-09-06T00:00:00Z".into(),
                )
                .unwrap()[0]
                .clone()
        };
        let id = enqueue(&mut store, first);
        store
            .ack(&id, "applied", None, None, "2026-09-06T00:00:01Z")
            .unwrap();
        std::fs::write(
            &path,
            serde_json::to_vec(&store.snapshot().unwrap()).unwrap(),
        )
        .unwrap();
        let mut store = CheckoutMutations::open(&path).unwrap();
        assert_eq!(
            store
                .write_base(&scope(), file, Some(original))
                .unwrap()
                .as_deref(),
            Some(first)
        );
        enqueue(&mut store, second);
        assert_eq!(
            store
                .write_base(&scope(), file, Some(first))
                .unwrap()
                .as_deref(),
            Some(second)
        );
        assert_eq!(
            store
                .write_base(&scope(), file, Some(second))
                .unwrap()
                .as_deref(),
            Some(second)
        );
        assert_eq!(store.outstanding_writes().count(), 0);
        assert_eq!(
            store
                .write_base(&scope(), file, Some(original))
                .unwrap()
                .as_deref(),
            Some(original)
        );
    }

    #[test]
    fn tracked_writes_refuse_publication_conflicts_and_isolate_scopes() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = CheckoutMutations::open(&dir.path().join("mutations.json")).unwrap();
        let file = ".bbox/gaps/gap-0123abcd.json";
        store
            .enqueue_tracked_writes(
                scope(),
                vec![(
                    file.into(),
                    r#"{"title":"queued"}"#.into(),
                    Some("{}".into()),
                )],
                "test".into(),
                "2026-09-06T00:00:00Z".into(),
            )
            .unwrap();
        assert!(
            store
                .write_base(&scope(), file, Some(r#"{"title":"external"}"#))
                .unwrap_err()
                .to_string()
                .contains("checkout_mutation_conflict")
        );
        let peer = PublishedScope::try_new("other-repo", ".").unwrap();
        assert_eq!(
            store
                .write_base(&peer, file, Some("{}"))
                .unwrap()
                .as_deref(),
            Some("{}")
        );
        assert!(!store.observe_publication(&peer, file, Some(r#"{"title":"queued"}"#)));
        assert_eq!(store.outstanding_writes().count(), 1);
        let count = store.pending_count();
        assert!(
            store
                .enqueue_tracked_writes(
                    scope(),
                    vec![
                        (file.into(), "{}".into(), None),
                        ("src/main.rs".into(), "{}".into(), None)
                    ],
                    "invalid pair".into(),
                    "2026-09-06T00:00:00Z".into()
                )
                .is_err()
        );
        assert_eq!(store.pending_count(), count);
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
                .ack(
                    "cm-0000000000000001",
                    "applied",
                    None,
                    None,
                    "2026-08-12T00:02:00Z"
                )
                .unwrap()
        );
        assert!(
            !store
                .ack(
                    "cm-ffffffffffffffff",
                    "applied",
                    None,
                    None,
                    "2026-08-12T00:02:00Z"
                )
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
        assert_eq!(store.failed()[0].last_error.as_deref(), Some("disk full"));
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
