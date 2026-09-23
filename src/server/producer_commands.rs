use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bbox_code_source::{
    EnrollReceiptV1, MAX_PRODUCER_COMMANDS_PER_POLL, ProducerCommandErrorV1,
    ProducerCommandPollRequestV1, ProducerCommandPollResponseV1, ProducerCommandV1,
    ProducerPresenceV1,
};
use parking_lot::Mutex;
use tokio::sync::watch;

pub(crate) const PRODUCER_COMMAND_REDELIVERY_SECS: u64 = 60;
pub(crate) const PRODUCER_PRESENCE_FRESH_SECS: u64 = 120;

pub(crate) trait ProducerCommandClock: Send + Sync {
    fn now_secs(&self) -> u64;
}

struct SystemProducerCommandClock;

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
        state.presences.insert(
            producer_id.to_string(),
            PresenceRecord {
                presence: request.presence,
                last_seen_secs: now,
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
}
