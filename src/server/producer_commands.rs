use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bbox_code_source::{
    EnrollReceiptV1, LinkedWorktreeV1, MAX_PRODUCER_COMMANDS_PER_POLL, ProducerCommandErrorV1,
    ProducerCommandPollRequestV1, ProducerCommandPollResponseV1, ProducerCommandV1,
    ProducerPresenceV1,
};
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::project_catalog::{
    AttachmentKind, AttachmentSnapshotV1, AttachmentStatus, CatalogSnapshotV2, CheckoutAttachment,
    ProjectScope,
};
use bbox_corpus_core::project_selector::WorktreeAlias;
use parking_lot::Mutex;
use tokio::sync::watch;

pub(crate) const PRODUCER_COMMAND_REDELIVERY_SECS: u64 = 60;
pub(crate) const PRODUCER_PRESENCE_FRESH_SECS: u64 = 120;

pub(crate) trait ProducerCommandClock: Send + Sync {
    fn now_secs(&self) -> u64;
}

pub(crate) struct SystemProducerCommandClock;

impl ProducerCommandClock for SystemProducerCommandClock {
    fn now_secs(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProducerCommandResult {
    Applied(EnrollReceiptV1),
    Failed(ProducerCommandErrorV1),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KnownProducerPresence {
    pub producer_id: String,
    pub presence: ProducerPresenceV1,
    pub fresh: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProducerSelectionError {
    NoProducer { known: Vec<KnownProducerPresence> },
    Ambiguous { producer_ids: Vec<String> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProducerCommandAckStatus {
    Accepted,
    AlreadySettled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProducerCommandAckError {
    UnknownCommand,
    WrongProducer,
}

#[derive(Debug, Clone)]
struct PresenceRecord {
    presence: ProducerPresenceV1,
    last_seen_secs: u64,
    /// Changes exactly when this producer's reported linked worktrees do.
    worktrees_revision: u64,
}

struct QueuedCommand {
    producer_id: String,
    command: ProducerCommandV1,
    delivered_at_secs: Option<u64>,
    result_tx: watch::Sender<Option<ProducerCommandResult>>,
}

struct ProducerCommandState {
    presences: BTreeMap<String, PresenceRecord>,
    commands: BTreeMap<String, QueuedCommand>,
    pending_by_target: BTreeMap<(String, String), String>,
    next_command_id: u64,
    next_worktrees_revision: u64,
    /// The fresh linked-worktree reports the alias generation last saw, as
    /// `(producer_id, worktrees_revision)`.
    alias_generation_key: Vec<(String, u64)>,
    alias_generation: u64,
    /// Alias conflicts already logged; a conflict is logged again only
    /// after it cleared.
    logged_alias_conflicts: BTreeSet<String>,
}

pub(crate) struct ProducerCommandRuntime {
    state: Mutex<ProducerCommandState>,
    clock: Arc<dyn ProducerCommandClock>,
}

impl ProducerCommandRuntime {
    pub(crate) fn new() -> Self {
        let seed = SystemProducerCommandClock.now_secs().rotate_left(17);
        Self::with_clock(Arc::new(SystemProducerCommandClock), seed)
    }

    fn with_clock(clock: Arc<dyn ProducerCommandClock>, next_command_id: u64) -> Self {
        Self {
            state: Mutex::new(ProducerCommandState {
                presences: BTreeMap::new(),
                commands: BTreeMap::new(),
                pending_by_target: BTreeMap::new(),
                next_command_id,
                next_worktrees_revision: 1,
                alias_generation_key: Vec::new(),
                alias_generation: 0,
                logged_alias_conflicts: BTreeSet::new(),
            }),
            clock,
        }
    }

    pub(crate) fn poll(
        &self,
        producer_id: &str,
        request: ProducerCommandPollRequestV1,
    ) -> ProducerCommandPollResponseV1 {
        let now = self.clock.now_secs();
        let mut state = self.state.lock();
        let worktrees_revision = match state.presences.get(producer_id) {
            Some(previous)
                if previous.presence.linked_worktrees == request.presence.linked_worktrees =>
            {
                previous.worktrees_revision
            }
            _ => {
                let revision = state.next_worktrees_revision;
                state.next_worktrees_revision = revision.wrapping_add(1);
                revision
            }
        };
        state.presences.insert(
            producer_id.to_string(),
            PresenceRecord {
                presence: request.presence,
                last_seen_secs: now,
                worktrees_revision,
            },
        );

        let mut commands = Vec::new();
        for queued in state.commands.values_mut() {
            if queued.producer_id != producer_id
                || queued.result_tx.borrow().is_some()
                || queued.delivered_at_secs.is_some_and(|delivered| {
                    now.saturating_sub(delivered) < PRODUCER_COMMAND_REDELIVERY_SECS
                })
            {
                continue;
            }
            queued.delivered_at_secs = Some(now);
            commands.push(queued.command.clone());
            if commands.len() == MAX_PRODUCER_COMMANDS_PER_POLL {
                break;
            }
        }
        ProducerCommandPollResponseV1 { commands }
    }

    pub(crate) fn select_producer(
        &self,
        path: &Path,
        explicit_producer: Option<&str>,
    ) -> Result<KnownProducerPresence, ProducerSelectionError> {
        let now = self.clock.now_secs();
        let state = self.state.lock();
        let known = state
            .presences
            .iter()
            .map(|(producer_id, record)| KnownProducerPresence {
                producer_id: producer_id.clone(),
                presence: record.presence.clone(),
                fresh: now.saturating_sub(record.last_seen_secs) <= PRODUCER_PRESENCE_FRESH_SECS,
            })
            .collect::<Vec<_>>();

        let mut candidates = Vec::new();
        for producer in &known {
            if !producer.fresh
                || explicit_producer.is_some_and(|selected| selected != producer.producer_id)
            {
                continue;
            }
            let depth = producer
                .presence
                .enroll_roots
                .iter()
                .map(PathBuf::from)
                .filter(|root| path.starts_with(root))
                .map(|root| root.components().count())
                .max();
            if let Some(depth) = depth {
                candidates.push((depth, producer.clone()));
            }
        }

        let Some(longest) = candidates.iter().map(|(depth, _)| *depth).max() else {
            return Err(ProducerSelectionError::NoProducer { known });
        };
        let mut winners = candidates
            .into_iter()
            .filter(|(depth, _)| *depth == longest)
            .map(|(_, producer)| producer)
            .collect::<Vec<_>>();
        if winners.len() > 1 {
            return Err(ProducerSelectionError::Ambiguous {
                producer_ids: winners
                    .into_iter()
                    .map(|producer| producer.producer_id)
                    .collect(),
            });
        }
        Ok(winners.remove(0))
    }

    pub(crate) fn fresh_presences(&self) -> Vec<KnownProducerPresence> {
        let now = self.clock.now_secs();
        self.state
            .lock()
            .presences
            .iter()
            .filter(|(_, record)| {
                now.saturating_sub(record.last_seen_secs) <= PRODUCER_PRESENCE_FRESH_SECS
            })
            .map(|(producer_id, record)| KnownProducerPresence {
                producer_id: producer_id.clone(),
                presence: record.presence.clone(),
                fresh: true,
            })
            .collect()
    }

    /// A generation that changes whenever the fresh linked-worktree reports
    /// change: a producer reports a different set, or a reporting producer's
    /// presence lapses or returns. Catalog changes move the project
    /// authority epoch instead, so a cache of resolved selectors keyed by
    /// both sees every input of the alias view except a producer grant
    /// reload.
    pub(crate) fn worktree_alias_generation(&self) -> u64 {
        let now = self.clock.now_secs();
        let mut state = self.state.lock();
        let key = state
            .presences
            .iter()
            .filter(|(_, record)| {
                !record.presence.linked_worktrees.is_empty()
                    && now.saturating_sub(record.last_seen_secs) <= PRODUCER_PRESENCE_FRESH_SECS
            })
            .map(|(producer_id, record)| (producer_id.clone(), record.worktrees_revision))
            .collect::<Vec<_>>();
        if key != state.alias_generation_key {
            state.alias_generation_key = key;
            state.alias_generation = state.alias_generation.wrapping_add(1);
        }
        state.alias_generation
    }

    /// The live linked-worktree alias view over one catalog snapshot, from
    /// fresh presences only. `grants` yields the effective producer grant
    /// rows `(scope, project_id, producer_id)` and is read only when some
    /// fresh presence reports a worktree. Conflicts are dropped and logged
    /// once.
    pub(crate) fn worktree_aliases(
        &self,
        catalog: &CatalogSnapshotV2,
        attachments: &AttachmentSnapshotV1,
        grants: impl FnOnce() -> Vec<(PublishedScope, String, String)>,
    ) -> Vec<WorktreeAlias> {
        let now = self.clock.now_secs();
        let reports = {
            let state = self.state.lock();
            state
                .presences
                .iter()
                .filter(|(_, record)| {
                    !record.presence.linked_worktrees.is_empty()
                        && now.saturating_sub(record.last_seen_secs) <= PRODUCER_PRESENCE_FRESH_SECS
                })
                .map(|(producer_id, record)| {
                    (
                        producer_id.clone(),
                        record.presence.linked_worktrees.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        let derived = if reports.is_empty() {
            WorktreeAliasDerivation::default()
        } else {
            derive_worktree_aliases(&reports, catalog, attachments, &grants())
        };
        let mut state = self.state.lock();
        for conflict in &derived.conflicts {
            if !state.logged_alias_conflicts.contains(conflict) {
                tracing::warn!(conflict = %conflict, "dropping ambiguous linked-worktree alias");
            }
        }
        state.logged_alias_conflicts = derived.conflicts;
        derived.aliases
    }

    #[cfg(test)]
    pub(crate) fn age_presence_for_test(&self, producer_id: &str, age_secs: u64) {
        let now = self.clock.now_secs();
        if let Some(record) = self.state.lock().presences.get_mut(producer_id) {
            record.last_seen_secs = now.saturating_sub(age_secs);
        }
    }

    pub(crate) fn enqueue_enroll(
        &self,
        producer_id: &str,
        path: &Path,
        full_ref: Option<String>,
    ) -> ProducerCommandV1 {
        let path = path.to_string_lossy().into_owned();
        let target = (producer_id.to_string(), path.clone());
        let mut state = self.state.lock();
        if let Some(command_id) = state.pending_by_target.get(&target)
            && let Some(queued) = state.commands.get(command_id)
            && queued.result_tx.borrow().is_none()
        {
            return queued.command.clone();
        }

        let command_id = format!("pc-{:016x}", state.next_command_id);
        state.next_command_id = state.next_command_id.wrapping_add(1);
        let command = ProducerCommandV1 {
            command_id: command_id.clone(),
            kind: "enroll".into(),
            path,
            full_ref,
        };
        let (result_tx, _result_rx) = watch::channel(None);
        state.pending_by_target.insert(target, command_id.clone());
        state.commands.insert(
            command_id,
            QueuedCommand {
                producer_id: producer_id.to_string(),
                command: command.clone(),
                delivered_at_secs: None,
                result_tx,
            },
        );
        command
    }

    pub(crate) fn ack(
        &self,
        producer_id: &str,
        command_id: &str,
        result: ProducerCommandResult,
    ) -> Result<ProducerCommandAckStatus, ProducerCommandAckError> {
        let mut state = self.state.lock();
        let Some(queued) = state.commands.get_mut(command_id) else {
            return Err(ProducerCommandAckError::UnknownCommand);
        };
        if queued.producer_id != producer_id {
            return Err(ProducerCommandAckError::WrongProducer);
        }
        if queued.result_tx.borrow().is_some() {
            return Ok(ProducerCommandAckStatus::AlreadySettled);
        }
        let target = (queued.producer_id.clone(), queued.command.path.clone());
        queued.result_tx.send_replace(Some(result));
        state.pending_by_target.remove(&target);
        Ok(ProducerCommandAckStatus::Accepted)
    }

    pub(crate) async fn wait_for_result(
        &self,
        command_id: &str,
        timeout: Duration,
    ) -> Option<ProducerCommandResult> {
        let mut receiver = {
            let state = self.state.lock();
            state.commands.get(command_id)?.result_tx.subscribe()
        };
        if let Some(result) = receiver.borrow().clone() {
            return Some(result);
        }
        tokio::time::timeout(timeout, async {
            loop {
                if receiver.changed().await.is_err() {
                    return None;
                }
                if let Some(result) = receiver.borrow().clone() {
                    return Some(result);
                }
            }
        })
        .await
        .ok()
        .flatten()
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct WorktreeAliasDerivation {
    pub(crate) aliases: Vec<WorktreeAlias>,
    pub(crate) conflicts: BTreeSet<String>,
}

/// Derive the alias view from fresh linked-worktree reports. Each entry
/// joins its base checkout id to the ACTIVE `Base`-kind attachments carrying
/// that id, and is accepted for one of them only when the reporting
/// producer's grant covers that attachment's published scope. The alias
/// root is the worktree directory joined with the attachment's project root
/// relpath, so each subtree project of a checkout aliases the same subtree
/// inside the worktree. Fail closed: a worktree reported by more than one
/// producer, and an alias root that maps to more than one attachment, are
/// dropped and named in `conflicts`.
pub(crate) fn derive_worktree_aliases(
    reports: &[(String, Vec<LinkedWorktreeV1>)],
    catalog: &CatalogSnapshotV2,
    attachments: &AttachmentSnapshotV1,
    grants: &[(PublishedScope, String, String)],
) -> WorktreeAliasDerivation {
    let mut conflicts = BTreeSet::new();
    let mut reporters: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for (producer_id, entries) in reports {
        for entry in entries {
            reporters
                .entry(entry.worktree_dir.as_str())
                .or_default()
                .insert(producer_id.as_str());
        }
    }
    for (worktree_dir, producers) in &reporters {
        if producers.len() > 1 {
            conflicts.insert(format!(
                "worktree {worktree_dir} is reported by producers {}",
                producers.iter().copied().collect::<Vec<_>>().join(", ")
            ));
        }
    }

    let mut bases: HashMap<&str, Vec<&CheckoutAttachment>> = HashMap::new();
    for attachment in attachments.attachments.values() {
        if attachment.status == AttachmentStatus::Attached
            && attachment.kind == AttachmentKind::Base
        {
            bases
                .entry(attachment.checkout_id.as_str())
                .or_default()
                .push(attachment);
        }
    }
    let grants = grants
        .iter()
        .map(|(scope, project_id, producer_id)| (scope, project_id.as_str(), producer_id.as_str()))
        .collect::<BTreeSet<_>>();

    let mut candidates: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (producer_id, entries) in reports {
        for entry in entries {
            if reporters
                .get(entry.worktree_dir.as_str())
                .is_some_and(|producers| producers.len() > 1)
            {
                continue;
            }
            for base in bases
                .get(entry.base_checkout_id.as_str())
                .into_iter()
                .flatten()
            {
                let Some(project) = catalog.projects.get(&base.project_id) else {
                    continue;
                };
                let ProjectScope::Published(scope) = &project.scope else {
                    continue;
                };
                if !grants.contains(&(scope, base.project_id.as_str(), producer_id.as_str())) {
                    continue;
                }
                candidates
                    .entry(alias_root(&entry.worktree_dir, &base.project_root_relpath))
                    .or_default()
                    .insert(base.attachment_id.as_str().to_string());
            }
        }
    }

    let mut aliases = Vec::new();
    for (alias_root, attachment_ids) in candidates {
        if attachment_ids.len() > 1 {
            conflicts.insert(format!(
                "alias root {alias_root} maps to attachments {}",
                attachment_ids.into_iter().collect::<Vec<_>>().join(", ")
            ));
            continue;
        }
        if let Some(base_attachment_id) = attachment_ids.into_iter().next() {
            aliases.push(WorktreeAlias {
                alias_root,
                base_attachment_id,
            });
        }
    }
    WorktreeAliasDerivation { aliases, conflicts }
}

/// `worktree_dir` joined with a validated project root relpath, lexically:
/// the directory names a path on the checkout host.
fn alias_root(worktree_dir: &str, project_root_relpath: &str) -> String {
    if project_root_relpath == "." {
        worktree_dir.to_string()
    } else {
        format!(
            "{}/{project_root_relpath}",
            worktree_dir.trim_end_matches('/')
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bbox_code_source::{
        PRODUCER_COMMAND_SCHEMA_VERSION, ProducerCommandAckRequestV1, ProducerCommandErrorV1,
    };
    use bbox_corpus_core::identity::PublishedScope;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct FakeClock(AtomicU64);

    impl FakeClock {
        fn new(now: u64) -> Self {
            Self(AtomicU64::new(now))
        }

        fn advance(&self, seconds: u64) {
            self.0.fetch_add(seconds, Ordering::Relaxed);
        }
    }

    impl ProducerCommandClock for FakeClock {
        fn now_secs(&self) -> u64 {
            self.0.load(Ordering::Relaxed)
        }
    }

    fn presence(roots: &[&str], host: &str) -> ProducerPresenceV1 {
        ProducerPresenceV1 {
            enroll_roots: roots.iter().map(|root| (*root).to_string()).collect(),
            host_label: host.into(),
            config_path: format!("/etc/blackbox/{host}.toml"),
            service_label: None,
            collector_version: "0.0.1".into(),
            interval_secs: None,
            linked_worktrees: Vec::new(),
        }
    }

    fn poll(
        runtime: &ProducerCommandRuntime,
        producer: &str,
        roots: &[&str],
    ) -> Vec<ProducerCommandV1> {
        runtime
            .poll(
                producer,
                ProducerCommandPollRequestV1 {
                    schema_version: PRODUCER_COMMAND_SCHEMA_VERSION,
                    presence: presence(roots, producer),
                },
            )
            .commands
    }

    fn receipt() -> EnrollReceiptV1 {
        EnrollReceiptV1 {
            project_id: Some("p_00000000000000000000000000000001".into()),
            attachment_id: Some("pa_000000000000000000000000000001".into()),
            created_project: true,
            already_attached: false,
            scope: PublishedScope::try_new("repo-a", ".").unwrap(),
            published_ref: "refs/heads/main".into(),
            identity_committed: true,
            commit_paths: Vec::new(),
            onboard_error: None,
        }
    }

    #[test]
    fn producer_selection_uses_components_longest_root_ambiguity_and_explicit_choice() {
        let clock = Arc::new(FakeClock::new(100));
        let runtime = ProducerCommandRuntime::with_clock(clock, 1);
        poll(&runtime, "producer-a", &["/a/re", "/a/repos"]);
        poll(&runtime, "producer-b", &["/a/repos/team"]);
        poll(&runtime, "producer-c", &["/a/repos/team"]);

        assert!(matches!(
            runtime.select_producer(Path::new("/a/repository"), None),
            Err(ProducerSelectionError::NoProducer { .. })
        ));
        assert_eq!(
            runtime
                .select_producer(Path::new("/a/repos/project"), None)
                .unwrap()
                .producer_id,
            "producer-a"
        );
        assert!(matches!(
            runtime.select_producer(Path::new("/a/repos/team/project"), None),
            Err(ProducerSelectionError::Ambiguous { .. })
        ));
        assert_eq!(
            runtime
                .select_producer(Path::new("/a/repos/team/project"), Some("producer-c"))
                .unwrap()
                .producer_id,
            "producer-c"
        );
    }

    #[test]
    fn producer_selection_excludes_stale_presence_and_handles_absence() {
        let clock = Arc::new(FakeClock::new(100));
        let runtime = ProducerCommandRuntime::with_clock(clock.clone(), 1);
        assert!(matches!(
            runtime.select_producer(Path::new("/a/repos/project"), None),
            Err(ProducerSelectionError::NoProducer { known }) if known.is_empty()
        ));
        poll(&runtime, "producer-a", &["/a/repos"]);
        clock.advance(PRODUCER_PRESENCE_FRESH_SECS + 1);
        assert!(matches!(
            runtime.select_producer(Path::new("/a/repos/project"), None),
            Err(ProducerSelectionError::NoProducer { known })
                if known.len() == 1 && !known[0].fresh
        ));
    }

    #[test]
    fn queue_deduplicates_pending_and_redelivers_after_sixty_seconds() {
        let clock = Arc::new(FakeClock::new(100));
        let runtime = ProducerCommandRuntime::with_clock(clock.clone(), 7);
        poll(&runtime, "producer-a", &["/a/repos"]);
        let first = runtime.enqueue_enroll("producer-a", Path::new("/a/repos/project"), None);
        let duplicate = runtime.enqueue_enroll("producer-a", Path::new("/a/repos/project"), None);
        assert_eq!(first.command_id, duplicate.command_id);
        assert_eq!(poll(&runtime, "producer-a", &["/a/repos"]).len(), 1);
        assert!(poll(&runtime, "producer-a", &["/a/repos"]).is_empty());
        clock.advance(PRODUCER_COMMAND_REDELIVERY_SECS);
        assert_eq!(poll(&runtime, "producer-a", &["/a/repos"]).len(), 1);
    }

    #[tokio::test]
    async fn ack_wakes_waiter_and_cross_producer_access_is_refused() {
        let clock = Arc::new(FakeClock::new(100));
        let runtime = Arc::new(ProducerCommandRuntime::with_clock(clock, 1));
        poll(&runtime, "producer-a", &["/a/repos"]);
        poll(&runtime, "producer-b", &["/b/repos"]);
        let command = runtime.enqueue_enroll("producer-a", Path::new("/a/repos/project"), None);
        assert!(poll(&runtime, "producer-b", &["/b/repos"]).is_empty());
        assert_eq!(
            runtime.ack(
                "producer-b",
                &command.command_id,
                ProducerCommandResult::Applied(receipt())
            ),
            Err(ProducerCommandAckError::WrongProducer)
        );

        let waiter_runtime = runtime.clone();
        let command_id = command.command_id.clone();
        let waiter = tokio::spawn(async move {
            waiter_runtime
                .wait_for_result(&command_id, Duration::from_secs(1))
                .await
        });
        runtime
            .ack(
                "producer-a",
                &command.command_id,
                ProducerCommandResult::Applied(receipt()),
            )
            .unwrap();
        assert!(matches!(
            waiter.await.unwrap(),
            Some(ProducerCommandResult::Applied(_))
        ));
    }

    #[tokio::test]
    async fn waiter_timeout_returns_none() {
        let runtime = ProducerCommandRuntime::with_clock(Arc::new(FakeClock::new(100)), 1);
        let command = runtime.enqueue_enroll("producer-a", Path::new("/a/repos/project"), None);
        assert!(
            runtime
                .wait_for_result(&command.command_id, Duration::from_millis(1))
                .await
                .is_none()
        );
    }

    #[test]
    fn ack_request_shape_maps_to_runtime_results() {
        let applied = ProducerCommandAckRequestV1 {
            command_id: "pc-0123456789abcdef".into(),
            outcome: "applied".into(),
            receipt: Some(receipt()),
            error: None,
        };
        assert!(matches!(applied.outcome.as_str(), "applied"));
        let failed = ProducerCommandResult::Failed(ProducerCommandErrorV1 {
            code: "enroll_failed".into(),
            message: "failed".into(),
        });
        assert!(matches!(failed, ProducerCommandResult::Failed(_)));
    }

    mod worktree_aliases {
        use super::*;
        use bbox_corpus_core::project_catalog::{
            AttachmentCapabilities, AttachmentId, CorpusProject, ProjectId,
        };

        const PROJECT_ROOT: &str = "p_00000000000000000000000000000001";
        const PROJECT_SUBTREE: &str = "p_00000000000000000000000000000002";
        const PROJECT_DETACHED: &str = "p_00000000000000000000000000000003";
        const PROJECT_TWIN_A: &str = "p_00000000000000000000000000000004";
        const PROJECT_TWIN_B: &str = "p_00000000000000000000000000000005";
        const CHECKOUT_BASE: &str = "0000000000000000000000000000000a";
        const CHECKOUT_WORKTREE: &str = "0000000000000000000000000000000b";
        const CHECKOUT_DETACHED: &str = "0000000000000000000000000000000c";
        const CHECKOUT_TWIN: &str = "0000000000000000000000000000000d";

        fn scope(repo: &str, relpath: &str) -> PublishedScope {
            PublishedScope::try_new(repo, relpath).unwrap()
        }

        fn attachment(
            id: &str,
            project_id: &str,
            checkout_id: &str,
            checkout_dir: &str,
            relpath: &str,
            kind: AttachmentKind,
            status: AttachmentStatus,
        ) -> CheckoutAttachment {
            CheckoutAttachment {
                attachment_id: AttachmentId::parse(id).unwrap(),
                project_id: ProjectId::parse(project_id).unwrap(),
                checkout_id: checkout_id.into(),
                checkout_dir: checkout_dir.into(),
                checkout_project_dir: alias_root(checkout_dir, relpath),
                project_root_relpath: relpath.into(),
                kind,
                validated_scope: None,
                computed_repo_hint: None,
                branch_ref: None,
                capabilities: AttachmentCapabilities::default(),
                status,
                attached_at: "2026-08-01T00:00:00Z".into(),
                detached_at: None,
            }
        }

        /// One base checkout `/work/alpha` carrying a root project and a
        /// `svc` subtree project, a worktree-kind attachment, a detached
        /// base, and two projects whose bases share one checkout id at the
        /// same relpath (the alias-root conflict shape).
        fn fixture() -> (CatalogSnapshotV2, AttachmentSnapshotV1) {
            let mut catalog = CatalogSnapshotV2::empty(1).unwrap();
            for (id, repo, relpath) in [
                (PROJECT_ROOT, "repo-alpha", "."),
                (PROJECT_SUBTREE, "repo-alpha", "svc"),
                (PROJECT_DETACHED, "repo-gamma", "."),
                (PROJECT_TWIN_A, "repo-twin-a", "."),
                (PROJECT_TWIN_B, "repo-twin-b", "."),
            ] {
                let project_id = ProjectId::parse(id).unwrap();
                catalog.projects.insert(
                    project_id.clone(),
                    CorpusProject {
                        project_id,
                        scope: ProjectScope::Published(scope(repo, relpath)),
                        operator_aliases: Default::default(),
                        nominated_aliases: Default::default(),
                        display_name: id.into(),
                        created_at: "2026-08-01T00:00:00Z".into(),
                        registered_at_compat: None,
                        repo_history: None,
                        languages: Default::default(),
                    },
                );
            }
            let mut attachments = AttachmentSnapshotV1::empty(1).unwrap();
            for row in [
                attachment(
                    "att_00000000000000000000000000000001",
                    PROJECT_ROOT,
                    CHECKOUT_BASE,
                    "/work/alpha",
                    ".",
                    AttachmentKind::Base,
                    AttachmentStatus::Attached,
                ),
                attachment(
                    "att_00000000000000000000000000000002",
                    PROJECT_SUBTREE,
                    CHECKOUT_BASE,
                    "/work/alpha",
                    "svc",
                    AttachmentKind::Base,
                    AttachmentStatus::Attached,
                ),
                attachment(
                    "att_00000000000000000000000000000003",
                    PROJECT_ROOT,
                    CHECKOUT_WORKTREE,
                    "/work/alpha-wt",
                    ".",
                    AttachmentKind::Worktree,
                    AttachmentStatus::Attached,
                ),
                attachment(
                    "att_00000000000000000000000000000004",
                    PROJECT_DETACHED,
                    CHECKOUT_DETACHED,
                    "/work/gamma",
                    ".",
                    AttachmentKind::Base,
                    AttachmentStatus::Detached,
                ),
                attachment(
                    "att_00000000000000000000000000000005",
                    PROJECT_TWIN_A,
                    CHECKOUT_TWIN,
                    "/work/twin",
                    ".",
                    AttachmentKind::Base,
                    AttachmentStatus::Attached,
                ),
                attachment(
                    "att_00000000000000000000000000000006",
                    PROJECT_TWIN_B,
                    CHECKOUT_TWIN,
                    "/work/twin",
                    ".",
                    AttachmentKind::Base,
                    AttachmentStatus::Attached,
                ),
            ] {
                attachments
                    .attachments
                    .insert(row.attachment_id.clone(), row);
            }
            (catalog, attachments)
        }

        fn grants() -> Vec<(PublishedScope, String, String)> {
            [
                ("repo-alpha", ".", PROJECT_ROOT),
                ("repo-alpha", "svc", PROJECT_SUBTREE),
                ("repo-gamma", ".", PROJECT_DETACHED),
                ("repo-twin-a", ".", PROJECT_TWIN_A),
                ("repo-twin-b", ".", PROJECT_TWIN_B),
            ]
            .into_iter()
            .map(|(repo, relpath, project)| {
                (scope(repo, relpath), project.into(), "producer-a".into())
            })
            .collect()
        }

        fn worktree(checkout_id: &str, dir: &str) -> LinkedWorktreeV1 {
            LinkedWorktreeV1 {
                base_checkout_id: checkout_id.into(),
                worktree_dir: dir.into(),
            }
        }

        fn alias(root: &str, attachment: &str) -> WorktreeAlias {
            WorktreeAlias {
                alias_root: root.into(),
                base_attachment_id: attachment.into(),
            }
        }

        #[test]
        fn derivation_accepts_granted_active_base_attachments_only() {
            let (catalog, attachments) = fixture();
            let reports = vec![
                (
                    "producer-a".to_string(),
                    vec![
                        worktree(CHECKOUT_BASE, "/work/wt/alpha-feature"),
                        // Only a worktree-kind attachment carries this id.
                        worktree(CHECKOUT_WORKTREE, "/work/wt/alpha-nested"),
                        // Only a detached base carries this id.
                        worktree(CHECKOUT_DETACHED, "/work/wt/gamma-feature"),
                        // No attachment carries this id.
                        worktree("0000000000000000000000000000000f", "/work/wt/unknown"),
                    ],
                ),
                (
                    // Holds no grant for the base it names.
                    "producer-b".to_string(),
                    vec![worktree(CHECKOUT_BASE, "/work/wt/alpha-other")],
                ),
            ];
            let derived = derive_worktree_aliases(&reports, &catalog, &attachments, &grants());
            assert_eq!(
                derived.aliases,
                vec![
                    alias(
                        "/work/wt/alpha-feature",
                        "att_00000000000000000000000000000001"
                    ),
                    alias(
                        "/work/wt/alpha-feature/svc",
                        "att_00000000000000000000000000000002"
                    ),
                ]
            );
            assert!(derived.conflicts.is_empty());
        }

        #[test]
        fn derivation_drops_shared_worktrees_and_ambiguous_alias_roots() {
            let (catalog, attachments) = fixture();
            let mut grants = grants();
            grants.push((
                scope("repo-alpha", "."),
                PROJECT_ROOT.into(),
                "producer-b".into(),
            ));
            let reports = vec![
                (
                    "producer-a".to_string(),
                    vec![
                        worktree(CHECKOUT_BASE, "/work/wt/shared"),
                        worktree(CHECKOUT_BASE, "/work/wt/alpha-feature"),
                        worktree(CHECKOUT_TWIN, "/work/wt/twin-feature"),
                    ],
                ),
                (
                    "producer-b".to_string(),
                    vec![worktree(CHECKOUT_BASE, "/work/wt/shared")],
                ),
            ];
            let derived = derive_worktree_aliases(&reports, &catalog, &attachments, &grants);
            assert_eq!(
                derived.aliases,
                vec![
                    alias(
                        "/work/wt/alpha-feature",
                        "att_00000000000000000000000000000001"
                    ),
                    alias(
                        "/work/wt/alpha-feature/svc",
                        "att_00000000000000000000000000000002"
                    ),
                ]
            );
            assert_eq!(derived.conflicts.len(), 2, "{:?}", derived.conflicts);
            assert!(
                derived
                    .conflicts
                    .iter()
                    .any(|conflict| conflict.contains("/work/wt/shared")
                        && conflict.contains("producer-a, producer-b"))
            );
            assert!(
                derived
                    .conflicts
                    .iter()
                    .any(|conflict| conflict.contains("/work/wt/twin-feature"))
            );
        }

        fn poll_worktrees(
            runtime: &ProducerCommandRuntime,
            producer: &str,
            worktrees: Vec<LinkedWorktreeV1>,
        ) {
            let mut presence = presence(&["/work"], producer);
            presence.linked_worktrees = worktrees;
            runtime.poll(
                producer,
                ProducerCommandPollRequestV1 {
                    schema_version: PRODUCER_COMMAND_SCHEMA_VERSION,
                    presence,
                },
            );
        }

        #[test]
        fn runtime_view_follows_fresh_reports_and_its_generation_moves_with_them() {
            let clock = Arc::new(FakeClock::new(100));
            let runtime = ProducerCommandRuntime::with_clock(clock.clone(), 1);
            let (catalog, attachments) = fixture();
            let view = || runtime.worktree_aliases(&catalog, &attachments, grants);

            let empty = runtime.worktree_alias_generation();
            assert!(view().is_empty());
            // A presence without worktrees moves nothing.
            poll(&runtime, "producer-a", &["/work"]);
            assert_eq!(runtime.worktree_alias_generation(), empty);

            poll_worktrees(
                &runtime,
                "producer-a",
                vec![worktree(CHECKOUT_BASE, "/work/wt/alpha-feature")],
            );
            let reported = runtime.worktree_alias_generation();
            assert_ne!(reported, empty);
            assert_eq!(view().len(), 2);

            // An identical report keeps the generation.
            clock.advance(10);
            poll_worktrees(
                &runtime,
                "producer-a",
                vec![worktree(CHECKOUT_BASE, "/work/wt/alpha-feature")],
            );
            assert_eq!(runtime.worktree_alias_generation(), reported);

            // A lapsed presence leaves the view and moves the generation.
            clock.advance(PRODUCER_PRESENCE_FRESH_SECS + 1);
            assert!(view().is_empty());
            let lapsed = runtime.worktree_alias_generation();
            assert_ne!(lapsed, reported);

            // A returning presence comes back into the view; a removed
            // worktree leaves it on the next report.
            poll_worktrees(
                &runtime,
                "producer-a",
                vec![worktree(CHECKOUT_BASE, "/work/wt/alpha-feature")],
            );
            assert_eq!(view().len(), 2);
            let returned = runtime.worktree_alias_generation();
            assert_ne!(returned, lapsed);
            poll_worktrees(&runtime, "producer-a", Vec::new());
            assert!(view().is_empty());
            assert_ne!(runtime.worktree_alias_generation(), returned);
        }

        #[test]
        fn runtime_records_each_conflict_until_it_clears() {
            let runtime = ProducerCommandRuntime::with_clock(Arc::new(FakeClock::new(100)), 1);
            let (catalog, attachments) = fixture();
            poll_worktrees(
                &runtime,
                "producer-a",
                vec![worktree(CHECKOUT_TWIN, "/work/wt/twin-feature")],
            );
            assert!(
                runtime
                    .worktree_aliases(&catalog, &attachments, grants)
                    .is_empty()
            );
            assert_eq!(runtime.state.lock().logged_alias_conflicts.len(), 1);
            poll_worktrees(&runtime, "producer-a", Vec::new());
            runtime.worktree_aliases(&catalog, &attachments, grants);
            assert!(runtime.state.lock().logged_alias_conflicts.is_empty());
        }
    }
}
