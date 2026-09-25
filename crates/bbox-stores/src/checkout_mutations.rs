//! Durable pending checkout mutations: repo-owned file writes/deletes the
//! daemon computed and validated but cannot apply itself (zero checkout
//! authority). The checkout-owner collector polls pending mutations over the
//! authenticated producer channel, applies them byte-for-byte, and acks.
//!
//! Status lifecycle: `pending` -> `applied` | `failed` (both terminal; a
//! failed mutation stays visible for triage instead of poison-looping the
//! collector). The store is the delivery mechanism only — the mutation's
//! content is already fully validated at enqueue time.
//!
//! Guarded configuration mutations form one ordered chain per scope and path.
//! Each carries the exact-byte precondition of its immediate predecessor, is
//! delivered only after every earlier mutation on its path settled, and a
//! predecessor that conflicted or failed settles its queued successors as
//! blocked rather than letting them bypass it. A conflict is recorded as a
//! failed outcome with the owner's observed state, so the lifecycle and every
//! existing reader keep their two terminal states.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use bbox_code_source::{
    CHECKOUT_MUTATION_OUTCOME_APPLIED, CHECKOUT_MUTATION_OUTCOME_CONFLICTED,
    CHECKOUT_MUTATION_OUTCOME_FAILED, CHECKOUT_MUTATION_SCHEMA_VERSION, CheckoutMutationGuardV1,
    CheckoutMutationV1, MAX_CHECKOUT_MUTATIONS_PER_POLL,
};
use bbox_corpus_core::identity::PublishedScope;

use crate::store_persister::StoreSnapshot;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckoutMutationStore {
    pub version: u32,
    pub mutations: Vec<PendingCheckoutMutation>,
    /// Identity of this queue's guarded sequence space, minted with its
    /// first guarded mutation. A fresh or restored queue mints a new one,
    /// so an owner can tell its counter apart from an earlier queue's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guarded_epoch: Option<String>,
    /// Next guarded sequence. Durable and never reused within the epoch.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub next_guarded_sequence: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

impl Default for CheckoutMutationStore {
    fn default() -> Self {
        Self {
            version: 1,
            mutations: Vec::new(),
            guarded_epoch: None,
            next_guarded_sequence: 0,
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
    /// A guarded mutation whose precondition did not match the owner's file.
    /// The owner kept its bytes; this records what it found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict: Option<MutationConflict>,
    /// A guarded mutation settled without delivery because the named
    /// predecessor on its path conflicted or failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_by: Option<String>,
    /// Last poll that withheld this guarded mutation from a collector that
    /// did not declare guarded support. It stays pending for an upgraded
    /// owner; this makes the refusal visible instead of silent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_unsupported_at: Option<String>,
    /// An applied guarded mutation whose chain later broke (a successor
    /// conflicted or failed) and whose path the owner then published with
    /// other bytes. The publication, not this intent, is authoritative.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reconciled: Option<MutationReconciliation>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MutationReconciliation {
    /// SHA-256 of the accepted bytes that superseded the chain, `None` when
    /// the accepted publication has no file at the path.
    pub published_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MutationConflict {
    /// SHA-256 of the owner's current bytes, `None` when the path was absent.
    pub observed_sha256: Option<String>,
}

/// One poll's delivery decision.
#[derive(Debug, Default)]
pub struct CheckoutMutationPoll {
    pub mutations: Vec<CheckoutMutationV1>,
    pub deferred: u64,
    /// Guarded mutations withheld because the collector did not declare
    /// guarded support.
    pub withheld_unsupported: Vec<String>,
}

/// Delivery and publication state of one mutation, as a producer reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckoutMutationProgress {
    /// Durable in the queue, not yet applied by the owner.
    Queued,
    /// Applied in the owner's checkout; not yet committed and published.
    Delivered,
    /// Present in the accepted publication.
    Published,
    /// The owner's file did not match the precondition; its bytes were kept.
    Conflicted,
    /// Settled without delivery because a predecessor did not apply, or
    /// superseded when its broken chain was reconciled to a publication.
    Blocked,
    /// Applied, then superseded: its chain broke and the owner published
    /// other bytes for the path, which the edit base now starts from.
    Reconciled,
    Failed,
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
            conflict: None,
            blocked_by: None,
            owner_unsupported_at: None,
            reconciled: None,
        });
        Ok(true)
    }

    /// Pending mutations whose scope the producer grant covers, oldest
    /// first, capped per poll. The deferred count covers both the cap and
    /// pending mutations outside the grant, so operators can see a grant
    /// that does not cover an enqueued scope.
    ///
    /// A guarded path delivers only its oldest pending mutation, so the
    /// owner applies a chain strictly in order and a successor never reaches
    /// the owner before its predecessor settled. Guarded mutations go only to
    /// a collector that declared guarded support; they are never downgraded
    /// to an unguarded delivery, and withholding them never blocks the
    /// legacy mutations beside them.
    pub fn poll(
        &self,
        granted_scopes: &BTreeSet<PublishedScope>,
        guarded_supported: bool,
    ) -> CheckoutMutationPoll {
        let mut poll = CheckoutMutationPoll::default();
        let mut guarded_paths = BTreeSet::new();
        for pending in &self.store.mutations {
            if pending.status != CheckoutMutationStatus::Pending {
                continue;
            }
            let guarded = pending.mutation.guard.is_some();
            let first_on_path = !guarded
                || guarded_paths.insert((
                    pending.mutation.scope.clone(),
                    pending.mutation.relative_path.clone(),
                ));
            if !granted_scopes.contains(&pending.mutation.scope) || !first_on_path {
                poll.deferred += 1;
                continue;
            }
            if guarded && !guarded_supported {
                poll.deferred += 1;
                poll.withheld_unsupported
                    .push(pending.mutation.mutation_id.clone());
                continue;
            }
            if poll.mutations.len() >= MAX_CHECKOUT_MUTATIONS_PER_POLL {
                poll.deferred += 1;
                continue;
            }
            poll.mutations.push(pending.mutation.clone());
        }
        poll
    }

    /// Record that a poll withheld these guarded mutations from a collector
    /// without guarded support. Returns whether any row changed.
    pub fn note_owner_unsupported(&mut self, mutation_ids: &[String], now: &str) -> bool {
        let mut changed = false;
        for row in &mut self.store.mutations {
            if row.status == CheckoutMutationStatus::Pending
                && row.mutation.guard.is_some()
                && mutation_ids.contains(&row.mutation.mutation_id)
                && row.owner_unsupported_at.is_none()
            {
                row.owner_unsupported_at = Some(now.to_string());
                changed = true;
            }
        }
        changed
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
        self.ack_with_observation(mutation_id, outcome, error, content_sha256, None, now)
    }

    /// Terminal ack including a guarded conflict's observed owner state.
    /// A guarded mutation that did not apply settles every queued successor
    /// on its path as blocked: each successor's precondition names bytes
    /// that never landed, so delivering it could only conflict or, worse,
    /// be mistaken for recovery.
    pub fn ack_with_observation(
        &mut self,
        mutation_id: &str,
        outcome: &str,
        error: Option<String>,
        content_sha256: Option<String>,
        observed_sha256: Option<String>,
        now: &str,
    ) -> Result<bool> {
        let Some(index) = self
            .store
            .mutations
            .iter()
            .position(|pending| pending.mutation.mutation_id == mutation_id)
        else {
            return Ok(false);
        };
        let pending = &mut self.store.mutations[index];
        if pending.status != CheckoutMutationStatus::Pending {
            return Ok(false);
        }
        let guarded = pending.mutation.guard.is_some();
        if outcome == CHECKOUT_MUTATION_OUTCOME_CONFLICTED && !guarded {
            anyhow::bail!("only a guarded checkout mutation can conflict");
        }
        pending.attempts += 1;
        pending.acked_at = Some(now.to_string());
        match outcome {
            CHECKOUT_MUTATION_OUTCOME_APPLIED => {
                pending.status = CheckoutMutationStatus::Applied;
                pending.ack_content_sha256 = content_sha256;
                pending.last_error = None;
                return Ok(true);
            }
            CHECKOUT_MUTATION_OUTCOME_FAILED => {
                pending.status = CheckoutMutationStatus::Failed;
                pending.last_error = error;
            }
            CHECKOUT_MUTATION_OUTCOME_CONFLICTED => {
                pending.status = CheckoutMutationStatus::Failed;
                pending.conflict = Some(MutationConflict { observed_sha256 });
                pending.last_error = error;
            }
            other => anyhow::bail!("unknown checkout mutation outcome {other}"),
        }
        if guarded {
            let scope = pending.mutation.scope.clone();
            let path = pending.mutation.relative_path.clone();
            let settled = pending.mutation.mutation_id.clone();
            let class = if pending.conflict.is_some() {
                "conflicted"
            } else {
                "failed"
            };
            for successor in &mut self.store.mutations[index + 1..] {
                if successor.status == CheckoutMutationStatus::Pending
                    && successor.mutation.guard.is_some()
                    && successor.mutation.scope == scope
                    && successor.mutation.relative_path == path
                {
                    successor.status = CheckoutMutationStatus::Failed;
                    successor.acked_at = Some(now.to_string());
                    successor.blocked_by = Some(settled.clone());
                    successor.last_error = Some(format!(
                        "blocked: predecessor {settled} {class}; reconcile the owner's {path} and re-issue this edit"
                    ));
                }
            }
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
        self.observe_publication_with(scope, relative_path, published, same_json)
    }

    fn observe_publication_with(
        &mut self,
        scope: &PublishedScope,
        relative_path: &str,
        published: Option<&str>,
        same: fn(Option<&str>, Option<&str>) -> bool,
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
                            && row.publication.as_ref().is_some_and(|publication| {
                                publication.base_content_json.is_some()
                            })
                            && !self.store.mutations[..index].iter().any(|previous| {
                                &previous.mutation.scope == scope
                                    && previous.mutation.relative_path == relative_path
                                    && previous.status == CheckoutMutationStatus::Pending
                            })))
                    && row
                        .publication
                        .as_ref()
                        .is_some_and(|state| !state.observed)
                    && same(row.mutation.content_json.as_deref(), published)
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
                && same(
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
        self.write_base_with(scope, relative_path, published, same_json)
    }

    /// [`Self::write_base`] for a guarded configuration path. The owner
    /// checks exact bytes, so chain observation compares exact bytes too: a
    /// JSON-equivalent re-encoding is a different state, never a published
    /// intent. A broken chain whose path the owner has since published with
    /// other bytes is reconciled first, so the next edit starts from those
    /// accepted bytes with a fresh precondition.
    pub fn guarded_write_base(
        &mut self,
        scope: &PublishedScope,
        relative_path: &str,
        published: Option<&str>,
    ) -> Result<Option<String>> {
        self.reconcile_broken_guarded_chain(scope, relative_path, published);
        self.write_base_with(scope, relative_path, published, same_bytes)
    }

    /// Retire whatever the accepted publication settles on a guarded path:
    /// the exact-byte published prefix of its chain, or a broken chain the
    /// owner reconciled by publishing other bytes. Returns whether any row
    /// changed.
    pub fn observe_guarded_publication(
        &mut self,
        scope: &PublishedScope,
        relative_path: &str,
        published: Option<&str>,
    ) -> bool {
        let reconciled = self.reconcile_broken_guarded_chain(scope, relative_path, published);
        self.observe_publication_with(scope, relative_path, published, same_bytes) || reconciled
    }

    /// The explicit reconciliation transition for a broken guarded chain.
    ///
    /// A chain is broken when a guarded mutation on the path conflicted or
    /// failed after an applied, still-unpublished prefix. The owner was told
    /// to reconcile and publish; once the accepted bytes differ from both
    /// the chain's publication base and every outstanding intent's bytes,
    /// that publication is the reconciled state. Applied intents are kept
    /// as history marked reconciled, and outstanding undelivered ones are
    /// settled as superseded, so none can resurrect the abandoned chain.
    fn reconcile_broken_guarded_chain(
        &mut self,
        scope: &PublishedScope,
        relative_path: &str,
        published: Option<&str>,
    ) -> bool {
        let on_path = |row: &PendingCheckoutMutation| {
            row.mutation.guard.is_some()
                && &row.mutation.scope == scope
                && row.mutation.relative_path == relative_path
        };
        let outstanding = |row: &PendingCheckoutMutation| {
            row.status != CheckoutMutationStatus::Failed
                && row
                    .publication
                    .as_ref()
                    .is_some_and(|publication| !publication.observed)
        };
        let Some(first_outstanding) = self
            .store
            .mutations
            .iter()
            .position(|row| on_path(row) && outstanding(row))
        else {
            return false;
        };
        let Some(break_row) = self.store.mutations[first_outstanding..]
            .iter()
            .find(|row| {
                on_path(row)
                    && row.status == CheckoutMutationStatus::Failed
                    && row.blocked_by.is_none()
            })
            .map(|row| row.mutation.mutation_id.clone())
        else {
            return false;
        };
        let unreconciled = self.store.mutations.iter().any(|row| {
            on_path(row)
                && outstanding(row)
                && (same_bytes(
                    row.publication
                        .as_ref()
                        .and_then(|publication| publication.base_content_json.as_deref()),
                    published,
                ) || same_bytes(row.mutation.content_json.as_deref(), published))
        });
        if unreconciled {
            return false;
        }
        let published_sha256 = published.map(content_sha256);
        for row in &mut self.store.mutations {
            if !(on_path(row) && outstanding(row)) {
                continue;
            }
            if row.status == CheckoutMutationStatus::Applied {
                row.reconciled = Some(MutationReconciliation {
                    published_sha256: published_sha256.clone(),
                });
            } else {
                row.status = CheckoutMutationStatus::Failed;
                row.blocked_by = Some(break_row.clone());
                row.last_error = Some(format!(
                    "superseded: the chain broke at {break_row} and the owner published other bytes for {relative_path}; re-issue the edit from the accepted configuration"
                ));
            }
            if let Some(publication) = &mut row.publication {
                publication.observed = true;
            }
        }
        true
    }

    fn write_base_with(
        &mut self,
        scope: &PublishedScope,
        relative_path: &str,
        published: Option<&str>,
        same: fn(Option<&str>, Option<&str>) -> bool,
    ) -> Result<Option<String>> {
        self.observe_publication_with(scope, relative_path, published, same);
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
            && !same(publication.base_content_json.as_deref(), published)
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

    /// Enqueue one guarded configuration mutation while the caller holds the
    /// queue's exclusive lock. `base` must be what [`Self::write_base`]
    /// returned under that same lock: the accepted bytes overlaid with this
    /// path's outstanding intents. The precondition is derived from it (so a
    /// successor names its immediate predecessor's bytes), while `published`
    /// stays the accepted-publication base that reconciles the chain.
    #[allow(clippy::too_many_arguments)]
    pub fn enqueue_guarded(
        &mut self,
        scope: PublishedScope,
        relative_path: String,
        content: Option<String>,
        base: Option<&str>,
        published: Option<String>,
        reason: String,
        now: String,
    ) -> Result<CheckoutMutationV1> {
        let predecessor = self
            .outstanding_intents()
            .filter(|row| {
                row.mutation.scope == scope && row.mutation.relative_path == relative_path
            })
            .last()
            .map(|row| row.mutation.mutation_id.clone());
        let (epoch, sequence) = self.next_guarded_position();
        let mutation = CheckoutMutationV1 {
            schema_version: CHECKOUT_MUTATION_SCHEMA_VERSION,
            mutation_id: self.mint_id(),
            scope,
            relative_path,
            mode: if content.is_some() { "write" } else { "delete" }.into(),
            content_json: content,
            reason,
            enqueued_at: now,
            guard: Some(CheckoutMutationGuardV1 {
                expected_sha256: base.map(content_sha256),
                predecessor,
                epoch,
                sequence,
            }),
        };
        mutation
            .validate()
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        self.store.guarded_epoch = Some(epoch_of(&mutation));
        self.store.next_guarded_sequence = sequence + 1;
        self.store.mutations.push(PendingCheckoutMutation {
            mutation: mutation.clone(),
            status: CheckoutMutationStatus::Pending,
            attempts: 0,
            last_error: None,
            acked_at: None,
            ack_content_sha256: None,
            publication: Some(MutationPublication {
                base_content_json: published,
                observed: false,
            }),
            conflict: None,
            blocked_by: None,
            owner_unsupported_at: None,
            reconciled: None,
        });
        Ok(mutation)
    }

    /// The epoch and sequence the next guarded mutation carries. The
    /// sequence also stays above every sequence already stored in the epoch,
    /// so a hand-edited or partially restored counter can never reissue one.
    fn next_guarded_position(&self) -> (String, u64) {
        let epoch = self
            .store
            .guarded_epoch
            .clone()
            .unwrap_or_else(mint_guarded_epoch);
        let highest = self
            .store
            .mutations
            .iter()
            .filter_map(|row| row.mutation.guard.as_ref())
            .filter(|guard| guard.epoch == epoch)
            .map(|guard| guard.sequence)
            .max()
            .unwrap_or(0);
        (
            epoch,
            self.store.next_guarded_sequence.max(highest + 1).max(1),
        )
    }

    /// Every guarded mutation of one scope and path, settled or not, in
    /// queue order.
    pub fn guarded_rows_for_path<'a>(
        &'a self,
        scope: &'a PublishedScope,
        relative_path: &'a str,
    ) -> impl Iterator<Item = &'a PendingCheckoutMutation> + 'a {
        self.store.mutations.iter().filter(move |row| {
            row.mutation.guard.is_some()
                && &row.mutation.scope == scope
                && row.mutation.relative_path == relative_path
        })
    }

    pub fn get(&self, mutation_id: &str) -> Option<&PendingCheckoutMutation> {
        self.store
            .mutations
            .iter()
            .find(|row| row.mutation.mutation_id == mutation_id)
    }

    /// Where one mutation stands. Delivery is never reported as publication:
    /// an applied mutation is `delivered` until publication observed it.
    pub fn progress(&self, mutation_id: &str) -> Option<CheckoutMutationProgress> {
        let row = self.get(mutation_id)?;
        Some(match row.status {
            CheckoutMutationStatus::Pending => CheckoutMutationProgress::Queued,
            CheckoutMutationStatus::Applied if row.reconciled.is_some() => {
                CheckoutMutationProgress::Reconciled
            }
            CheckoutMutationStatus::Applied => match &row.publication {
                Some(publication) if publication.observed => CheckoutMutationProgress::Published,
                _ => CheckoutMutationProgress::Delivered,
            },
            CheckoutMutationStatus::Failed if row.conflict.is_some() => {
                CheckoutMutationProgress::Conflicted
            }
            CheckoutMutationStatus::Failed if row.blocked_by.is_some() => {
                CheckoutMutationProgress::Blocked
            }
            CheckoutMutationStatus::Failed => CheckoutMutationProgress::Failed,
        })
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
                guard: None,
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
                conflict: None,
                blocked_by: None,
                owner_unsupported_at: None,
                reconciled: None,
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
            guard: None,
        };
        let id = mutation.mutation_id.clone();
        self.enqueue(mutation)?;
        Ok(id)
    }
}

fn epoch_of(mutation: &CheckoutMutationV1) -> String {
    mutation
        .guard
        .as_ref()
        .map(|guard| guard.epoch.clone())
        .expect("guarded mutation")
}

/// 32 hex characters of a digest over the clock, process and thread: unique
/// per queue creation, never derived from content.
fn mint_guarded_epoch() -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(
        std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .to_be_bytes(),
    );
    hasher.update(std::process::id().to_be_bytes());
    hasher.update(format!("{:?}", std::thread::current().id()).as_bytes());
    hasher.update(format!("{:p}", &hasher as *const _).as_bytes());
    format!("{:x}", hasher.finalize())[..32].to_string()
}

/// Lowercase hex SHA-256 of exact bytes, the precondition encoding.
pub fn content_sha256(content: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(content.as_bytes()))
}

fn same_bytes(left: Option<&str>, right: Option<&str>) -> bool {
    left == right
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
            guard: None,
        }
    }

    #[test]
    fn tracked_delete_requires_published_presence_and_settled_delivery_prefix() {
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
        assert!(!store.observe_publication(&scope(), file, None));
        assert_eq!(store.write_base(&scope(), file, Some("{}")).unwrap(), None);
        assert_eq!(store.outstanding_intents().count(), 1);
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

    const BROFILE: &str = ".bro/brofiles/reviewer.json";

    /// One guarded edit exactly as the configuration service performs it:
    /// base selection and enqueue under the same exclusive borrow.
    fn guarded_edit(
        store: &mut CheckoutMutations,
        scope: &PublishedScope,
        published: Option<&str>,
        next: Option<&str>,
    ) -> Result<CheckoutMutationV1> {
        let base = store.guarded_write_base(scope, BROFILE, published)?;
        store.enqueue_guarded(
            scope.clone(),
            BROFILE.into(),
            next.map(str::to_owned),
            base.as_deref(),
            published.map(str::to_owned),
            "bro_brofile(scope=project)".into(),
            "2026-09-24T00:00:00Z".into(),
        )
    }

    fn expected(mutation: &CheckoutMutationV1) -> Option<String> {
        mutation.guard.as_ref().unwrap().expected_sha256.clone()
    }

    fn predecessor(mutation: &CheckoutMutationV1) -> Option<String> {
        mutation.guard.as_ref().unwrap().predecessor.clone()
    }

    #[test]
    fn guarded_chain_derives_each_precondition_from_its_immediate_predecessor() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("mutations.json");
        let mut store = CheckoutMutations::open(&path).unwrap();
        let accepted = r#"{"name":"reviewer","model":"a"}"#;
        let first = r#"{"name":"reviewer","model":"b"}"#;
        let second = r#"{"name":"reviewer","model":"c"}"#;

        let create = guarded_edit(&mut store, &scope(), Some(accepted), Some(first)).unwrap();
        assert_eq!(expected(&create), Some(content_sha256(accepted)));
        assert_eq!(predecessor(&create), None);
        // The second edit before publication builds on the first, not on the
        // accepted bytes, so it cannot discard the first.
        let replace = guarded_edit(&mut store, &scope(), Some(accepted), Some(second)).unwrap();
        assert_eq!(expected(&replace), Some(content_sha256(first)));
        assert_eq!(predecessor(&replace), Some(create.mutation_id.clone()));
        let delete = guarded_edit(&mut store, &scope(), Some(accepted), None).unwrap();
        assert_eq!(delete.mode, "delete");
        assert_eq!(expected(&delete), Some(content_sha256(second)));
        let recreate = guarded_edit(&mut store, &scope(), Some(accepted), Some(first)).unwrap();
        assert_eq!(expected(&recreate), None, "recreate asserts absence");
        assert_eq!(predecessor(&recreate), Some(delete.mutation_id.clone()));

        // Restart keeps the chain, its preconditions and its base.
        std::fs::write(
            &path,
            serde_json::to_vec(&store.snapshot().unwrap()).unwrap(),
        )
        .unwrap();
        let mut store = CheckoutMutations::open(&path).unwrap();
        assert_eq!(
            store
                .guarded_write_base(&scope(), BROFILE, Some(accepted))
                .unwrap()
                .as_deref(),
            Some(first)
        );
        assert_eq!(
            store.progress(&create.mutation_id),
            Some(CheckoutMutationProgress::Queued)
        );

        // Delivery is strictly ordered: one mutation per path per poll.
        let granted = BTreeSet::from([scope()]);
        for (current, next) in [
            (&create, &replace),
            (&replace, &delete),
            (&delete, &recreate),
        ] {
            let poll = store.poll(&granted, true);
            assert_eq!(
                poll.mutations
                    .iter()
                    .map(|mutation| mutation.mutation_id.as_str())
                    .collect::<Vec<_>>(),
                vec![current.mutation_id.as_str()]
            );
            assert!(poll.deferred >= 1);
            store
                .ack(&current.mutation_id, "applied", None, None, "now")
                .unwrap();
            assert_eq!(
                store.progress(&current.mutation_id),
                Some(CheckoutMutationProgress::Delivered),
                "an applied mutation is delivered, never published"
            );
            assert_eq!(
                store.poll(&granted, true).mutations[0].mutation_id,
                next.mutation_id
            );
        }
        store
            .ack(&recreate.mutation_id, "applied", None, None, "now")
            .unwrap();
        // Applied but unpublished intents remain the edit base.
        assert_eq!(
            store
                .guarded_write_base(&scope(), BROFILE, Some(accepted))
                .unwrap()
                .as_deref(),
            Some(first)
        );
        // Publication of the chain's final content retires the whole chain.
        assert_eq!(
            store
                .guarded_write_base(&scope(), BROFILE, Some(first))
                .unwrap()
                .as_deref(),
            Some(first)
        );
        assert_eq!(
            store.progress(&recreate.mutation_id),
            Some(CheckoutMutationProgress::Published)
        );
        assert_eq!(store.outstanding_intents().count(), 0);
    }

    #[test]
    fn guarded_partial_publication_advances_the_chain_and_divergence_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = CheckoutMutations::open(&dir.path().join("mutations.json")).unwrap();
        let accepted = r#"{"v":0}"#;
        let one = r#"{"v":1}"#;
        let two = r#"{"v":2}"#;
        let first = guarded_edit(&mut store, &scope(), Some(accepted), Some(one)).unwrap();
        let second = guarded_edit(&mut store, &scope(), Some(accepted), Some(two)).unwrap();
        store
            .ack(&first.mutation_id, "applied", None, None, "now")
            .unwrap();
        // The owner committed and published only the first edit.
        assert_eq!(
            store
                .guarded_write_base(&scope(), BROFILE, Some(one))
                .unwrap()
                .as_deref(),
            Some(two)
        );
        assert_eq!(
            store.progress(&first.mutation_id),
            Some(CheckoutMutationProgress::Published)
        );
        assert_eq!(
            store.progress(&second.mutation_id),
            Some(CheckoutMutationProgress::Queued)
        );
        let third = guarded_edit(&mut store, &scope(), Some(one), Some(r#"{"v":3}"#)).unwrap();
        assert_eq!(expected(&third), Some(content_sha256(two)));
        // A publication that incorporates neither queued edit is divergent:
        // it never authorizes overwriting either side.
        let error = guarded_edit(
            &mut store,
            &scope(),
            Some(r#"{"v":"external"}"#),
            Some("{}"),
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("error.checkout_mutation_conflict"),
            "{error}"
        );
        assert!(error.contains(&third.mutation_id), "{error}");
    }

    #[test]
    fn guarded_conflict_blocks_successors_and_resets_the_edit_base() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = CheckoutMutations::open(&dir.path().join("mutations.json")).unwrap();
        let accepted = r#"{"v":0}"#;
        let first = guarded_edit(&mut store, &scope(), Some(accepted), Some(r#"{"v":1}"#)).unwrap();
        let second =
            guarded_edit(&mut store, &scope(), Some(accepted), Some(r#"{"v":2}"#)).unwrap();
        let observed = "b".repeat(64);
        assert!(
            store
                .ack_with_observation(
                    &first.mutation_id,
                    "conflicted",
                    Some("local edit".into()),
                    None,
                    Some(observed.clone()),
                    "now",
                )
                .unwrap()
        );
        assert_eq!(
            store.progress(&first.mutation_id),
            Some(CheckoutMutationProgress::Conflicted)
        );
        assert_eq!(
            store.get(&first.mutation_id).unwrap().conflict,
            Some(MutationConflict {
                observed_sha256: Some(observed)
            })
        );
        assert_eq!(
            store.progress(&second.mutation_id),
            Some(CheckoutMutationProgress::Blocked)
        );
        assert_eq!(
            store
                .get(&second.mutation_id)
                .unwrap()
                .blocked_by
                .as_deref(),
            Some(first.mutation_id.as_str())
        );
        assert!(
            store
                .poll(&BTreeSet::from([scope()]), true)
                .mutations
                .is_empty()
        );
        // Recovery starts again from the accepted bytes with a precondition;
        // it never drops the precondition.
        let retry = guarded_edit(&mut store, &scope(), Some(accepted), Some(r#"{"v":1}"#)).unwrap();
        assert_eq!(expected(&retry), Some(content_sha256(accepted)));
        assert_eq!(predecessor(&retry), None);
        // A failed (not conflicted) guarded predecessor blocks the same way.
        let next = guarded_edit(&mut store, &scope(), Some(accepted), Some(r#"{"v":9}"#)).unwrap();
        store
            .ack(
                &retry.mutation_id,
                "failed",
                Some("disk full".into()),
                None,
                "now",
            )
            .unwrap();
        assert_eq!(
            store.progress(&next.mutation_id),
            Some(CheckoutMutationProgress::Blocked)
        );
        // Legacy mutations cannot report a conflict.
        store.enqueue(mutation("cm-0000000000000009")).unwrap();
        assert!(
            store
                .ack("cm-0000000000000009", "conflicted", None, None, "now")
                .is_err()
        );
    }

    /// Every guarded mutation carries the queue's epoch and a sequence that
    /// strictly increases across restarts and never repeats, including on
    /// another path; a separate queue mints a different epoch.
    #[test]
    fn guarded_sequences_are_durable_monotonic_and_epoch_scoped() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("mutations.json");
        let mut store = CheckoutMutations::open(&path).unwrap();
        let guard = |mutation: &CheckoutMutationV1| mutation.guard.clone().unwrap();
        let first = guarded_edit(&mut store, &scope(), None, Some("{}")).unwrap();
        let second = guarded_edit(&mut store, &scope(), None, Some(r#"{"v":2}"#)).unwrap();
        assert_eq!(guard(&first).sequence, 1);
        assert_eq!(guard(&second).sequence, 2);
        assert_eq!(guard(&first).epoch, guard(&second).epoch);
        std::fs::write(
            &path,
            serde_json::to_vec(&store.snapshot().unwrap()).unwrap(),
        )
        .unwrap();
        let mut store = CheckoutMutations::open(&path).unwrap();
        let other = store
            .enqueue_guarded(
                scope(),
                ".bbox/mcp.json".into(),
                Some("{}".into()),
                None,
                None,
                "test".into(),
                "now".into(),
            )
            .unwrap();
        assert_eq!(guard(&other).sequence, 3);
        assert_eq!(guard(&other).epoch, guard(&first).epoch);
        // A counter that fell behind the stored rows never reissues one.
        store.store.next_guarded_sequence = 1;
        let next = guarded_edit(&mut store, &scope(), None, Some(r#"{"v":4}"#)).unwrap();
        assert_eq!(guard(&next).sequence, 4);
        let mut fresh = CheckoutMutations::open(&root.join("fresh.json")).unwrap();
        let foreign = guarded_edit(&mut fresh, &scope(), None, Some("{}")).unwrap();
        assert_ne!(guard(&foreign).epoch, guard(&first).epoch);
        // A queue that never held a guarded mutation encodes as before.
        let legacy = CheckoutMutations::open(&root.join("legacy.json")).unwrap();
        let encoded = serde_json::to_value(legacy.snapshot().unwrap()).unwrap();
        assert!(encoded.get("guarded_epoch").is_none());
        assert!(encoded.get("next_guarded_sequence").is_none());
    }

    /// The owner compares exact bytes, so the chain does too: an intent that
    /// is JSON-equal to the accepted bytes but encoded differently is a real
    /// change that must be delivered in order, never retired as published.
    #[test]
    fn guarded_chains_compare_exact_bytes_not_json_equivalence() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = CheckoutMutations::open(&dir.path().join("mutations.json")).unwrap();
        let accepted = r#"{"name":"reviewer","v":1}"#;
        let pretty = "{\n  \"name\": \"reviewer\",\n  \"v\": 1\n}\n";
        let next = r#"{"name":"reviewer","v":2}"#;
        let first = guarded_edit(&mut store, &scope(), Some(accepted), Some(pretty)).unwrap();
        let second = guarded_edit(&mut store, &scope(), Some(accepted), Some(next)).unwrap();
        assert_eq!(predecessor(&second), Some(first.mutation_id.clone()));
        assert_eq!(expected(&second), Some(content_sha256(pretty)));
        assert_eq!(
            store.progress(&first.mutation_id),
            Some(CheckoutMutationProgress::Queued)
        );
        // A chain that returns to a JSON-equal re-encoding of the accepted
        // value is still a distinct state.
        let third = guarded_edit(&mut store, &scope(), Some(accepted), Some(pretty)).unwrap();
        assert_eq!(expected(&third), Some(content_sha256(next)));
        let granted = BTreeSet::from([scope()]);
        for mutation in [&first, &second, &third] {
            let poll = store.poll(&granted, true);
            assert_eq!(poll.mutations[0].mutation_id, mutation.mutation_id);
            store
                .ack(&mutation.mutation_id, "applied", None, None, "now")
                .unwrap();
        }
        // Only publication of the exact final bytes retires the chain.
        assert!(!store.observe_guarded_publication(&scope(), BROFILE, Some(accepted)));
        assert_eq!(store.outstanding_intents().count(), 3);
        assert!(store.observe_guarded_publication(&scope(), BROFILE, Some(pretty)));
        assert_eq!(store.outstanding_intents().count(), 0);
    }

    /// Accepted P, applied A, then B conflicts on the owner's local edit X.
    /// Following the conflict's instruction, the owner commits and publishes
    /// X. The applied A is reconciled (kept as history), an undelivered retry
    /// made before the publication is superseded, and the next edit starts
    /// from X with X's exact hash, including after a restart.
    #[test]
    fn a_broken_chain_reconciles_to_the_owners_publication() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("mutations.json");
        let mut store = CheckoutMutations::open(&path).unwrap();
        let accepted = r#"{"v":"P"}"#;
        let local = r#"{"v":"X"}"#;
        let a = guarded_edit(&mut store, &scope(), Some(accepted), Some(r#"{"v":"A"}"#)).unwrap();
        let b = guarded_edit(&mut store, &scope(), Some(accepted), Some(r#"{"v":"B"}"#)).unwrap();
        store
            .ack(&a.mutation_id, "applied", None, None, "now")
            .unwrap();
        store
            .ack_with_observation(
                &b.mutation_id,
                "conflicted",
                Some("local edit".into()),
                None,
                Some(content_sha256(local)),
                "now",
            )
            .unwrap();
        // Before the owner publishes, the applied A is still the base: the
        // chain is not reconciled by the conflict alone.
        let retry =
            guarded_edit(&mut store, &scope(), Some(accepted), Some(r#"{"v":"R"}"#)).unwrap();
        assert_eq!(expected(&retry), Some(content_sha256(r#"{"v":"A"}"#)));
        std::fs::write(
            &path,
            serde_json::to_vec(&store.snapshot().unwrap()).unwrap(),
        )
        .unwrap();
        let mut store = CheckoutMutations::open(&path).unwrap();

        // The owner publishes X.
        assert_eq!(
            store
                .guarded_write_base(&scope(), BROFILE, Some(local))
                .unwrap()
                .as_deref(),
            Some(local)
        );
        assert_eq!(
            store.progress(&a.mutation_id),
            Some(CheckoutMutationProgress::Reconciled)
        );
        assert_eq!(
            store.get(&a.mutation_id).unwrap().reconciled,
            Some(MutationReconciliation {
                published_sha256: Some(content_sha256(local))
            })
        );
        assert_eq!(
            store.progress(&b.mutation_id),
            Some(CheckoutMutationProgress::Conflicted)
        );
        assert_eq!(
            store.progress(&retry.mutation_id),
            Some(CheckoutMutationProgress::Blocked)
        );
        assert!(
            store
                .poll(&BTreeSet::from([scope()]), true)
                .mutations
                .is_empty()
        );
        let next = guarded_edit(&mut store, &scope(), Some(local), Some(r#"{"v":"C"}"#)).unwrap();
        assert_eq!(expected(&next), Some(content_sha256(local)));
        assert_eq!(predecessor(&next), None);
        std::fs::write(
            &path,
            serde_json::to_vec(&store.snapshot().unwrap()).unwrap(),
        )
        .unwrap();
        let store = CheckoutMutations::open(&path).unwrap();
        assert_eq!(
            store.progress(&a.mutation_id),
            Some(CheckoutMutationProgress::Reconciled)
        );
        assert_eq!(store.outstanding_intents().count(), 1);
    }

    /// An unbroken chain facing a divergent publication is still a conflict:
    /// reconciliation is only for a chain the owner already refused.
    #[test]
    fn an_unbroken_chain_never_reconciles_on_divergence() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = CheckoutMutations::open(&dir.path().join("mutations.json")).unwrap();
        let a = guarded_edit(&mut store, &scope(), Some("{}"), Some(r#"{"v":"A"}"#)).unwrap();
        store
            .ack(&a.mutation_id, "applied", None, None, "now")
            .unwrap();
        let error = store
            .guarded_write_base(&scope(), BROFILE, Some(r#"{"v":"X"}"#))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("error.checkout_mutation_conflict"),
            "{error}"
        );
        assert_eq!(
            store.progress(&a.mutation_id),
            Some(CheckoutMutationProgress::Delivered)
        );
    }

    #[test]
    fn guarded_mutations_are_scope_isolated_and_withheld_from_unsupported_owners() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = CheckoutMutations::open(&dir.path().join("mutations.json")).unwrap();
        let peer = PublishedScope::try_new("repo-family", "services/api").unwrap();
        let ours = guarded_edit(&mut store, &scope(), None, Some("{}")).unwrap();
        let theirs = guarded_edit(&mut store, &peer, None, Some(r#"{"x":1}"#)).unwrap();
        assert_eq!(predecessor(&theirs), None, "chains never cross scopes");
        assert_eq!(expected(&theirs), None);
        assert_eq!(
            store
                .guarded_write_base(&peer, BROFILE, None)
                .unwrap()
                .as_deref(),
            Some(r#"{"x":1}"#)
        );
        store.enqueue(mutation("cm-0000000000000001")).unwrap();
        let granted = BTreeSet::from([scope(), peer.clone()]);
        let legacy_owner = store.poll(&granted, false);
        assert_eq!(
            legacy_owner
                .mutations
                .iter()
                .map(|mutation| mutation.mutation_id.as_str())
                .collect::<Vec<_>>(),
            vec!["cm-0000000000000001"],
            "withholding guarded rows never blocks legacy ones"
        );
        assert_eq!(
            legacy_owner.withheld_unsupported,
            vec![ours.mutation_id.clone(), theirs.mutation_id.clone()]
        );
        assert!(store.note_owner_unsupported(&legacy_owner.withheld_unsupported, "now"));
        assert!(!store.note_owner_unsupported(&legacy_owner.withheld_unsupported, "later"));
        assert_eq!(
            store
                .get(&ours.mutation_id)
                .unwrap()
                .owner_unsupported_at
                .as_deref(),
            Some("now")
        );
        let upgraded = store.poll(&granted, true);
        assert_eq!(upgraded.mutations.len(), 3);
        assert!(upgraded.withheld_unsupported.is_empty());
        let only_peer = store.poll(&BTreeSet::from([peer]), true);
        assert_eq!(only_peer.mutations.len(), 1);
        assert_eq!(only_peer.mutations[0].mutation_id, theirs.mutation_id);
    }

    /// Queue files written before guarded mutations existed carry none of
    /// the new fields; they load, keep their rows, and still deliver.
    #[test]
    fn legacy_queue_records_load_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mutations.json");
        let legacy = serde_json::json!({
            "version": 1,
            "mutations": [{
                "mutation": {
                    "schema_version": 1,
                    "mutation_id": "cm-00000000000000aa",
                    "scope": serde_json::to_value(scope()).unwrap(),
                    "relative_path": ".bbox/gaps/gap-0123abcd.json",
                    "mode": "write",
                    "content_json": "{}",
                    "reason": "legacy",
                    "enqueued_at": "2026-08-12T00:00:00Z"
                },
                "status": "pending",
                "attempts": 0,
                "last_error": null,
                "acked_at": null,
                "ack_content_sha256": null,
                "publication": {"base_content_json": null, "observed": false}
            }, {
                "mutation": {
                    "schema_version": 1,
                    "mutation_id": "cm-00000000000000bb",
                    "scope": serde_json::to_value(scope()).unwrap(),
                    "relative_path": ".bbox/knowledge/abc.json",
                    "mode": "delete",
                    "content_json": null,
                    "reason": "legacy",
                    "enqueued_at": "2026-08-12T00:00:00Z"
                },
                "status": "failed",
                "attempts": 1,
                "last_error": "disk full",
                "acked_at": "2026-08-12T00:01:00Z",
                "ack_content_sha256": null
            }]
        });
        std::fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        let store = CheckoutMutations::open(&path).unwrap();
        let poll = store.poll(&BTreeSet::from([scope()]), false);
        assert_eq!(poll.mutations.len(), 1);
        assert_eq!(poll.mutations[0].guard, None);
        assert_eq!(
            store.progress("cm-00000000000000bb"),
            Some(CheckoutMutationProgress::Failed)
        );
        // Rewriting the loaded store keeps legacy rows free of new fields.
        let rewritten = serde_json::to_value(store.snapshot().unwrap()).unwrap();
        for row in rewritten["mutations"].as_array().unwrap() {
            assert!(row.get("conflict").is_none());
            assert!(row.get("blocked_by").is_none());
            assert!(row["mutation"].get("guard").is_none());
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
        let poll = store.poll(&granted, false);
        assert_eq!(poll.mutations.len(), 1);
        assert_eq!(poll.deferred, 0);
        let poll = store.poll(&BTreeSet::new(), true);
        assert!(poll.mutations.is_empty());
        assert_eq!(poll.deferred, 1);

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
        let poll = store.poll(&granted, true);
        assert!(poll.mutations.is_empty());
        assert_eq!(poll.deferred, 0);
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
