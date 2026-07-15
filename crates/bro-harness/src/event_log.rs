//! Durable append-only JSONL outbox for one harness session.
//!
//! The session snapshot answers how to resume a conversation. This log answers
//! what happened and is also the worker event outbox. New lines add an
//! `event_seq` while preserving the legacy `{ts, event}` fields consumed by
//! transcript readers:
//!
//! ```json
//! {"ts":"2026-06-10T12:34:56.789Z","event_seq":1,"event":{}}
//! ```
//!
//! A line becomes committed only after `write_all` and `sync_data` succeed.
//! Subscribers receive committed lines, never merely queued lines. Complete
//! lines are never rewritten or removed. Startup may discard only an invalid,
//! unterminated final tail, which could not have reached the committed
//! subscription.

use chrono::{SecondsFormat, Utc};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::sync::broadcast;

/// File suffix of the sidecar log, appended to the session id.
pub const EVENT_LOG_SUFFIX: &str = ".events.jsonl";

pub const DEFAULT_MAX_DISK_BYTES: u64 = 1024 * 1024 * 1024;
pub const DEFAULT_MAX_QUEUE_BYTES: u64 = 32 * 1024 * 1024;
pub const DEFAULT_MAX_QUEUE_EVENTS: usize = 4096;
pub const DEFAULT_COMMITTED_CHANNEL_EVENTS: usize = 1024;
pub const DEFAULT_REPLAY_MAX_EVENTS: usize = 512;
pub const DEFAULT_REPLAY_MAX_BYTES: u64 = 8 * 1024 * 1024;
/// Reserve room for the RPC envelope around one serialized `WorkerEvent`.
pub const MAX_EVENT_WIRE_BYTES: usize = bro_rpc::DEFAULT_MAX_FRAME_BYTES - 64 * 1024;
const MAX_EVENT_LOG_LINE_BYTES: usize = bro_rpc::DEFAULT_MAX_FRAME_BYTES;

static WARNED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventLogLimits {
    pub max_disk_bytes: u64,
    pub max_queue_bytes: u64,
    pub max_queue_events: usize,
    pub committed_channel_events: usize,
}

impl Default for EventLogLimits {
    fn default() -> Self {
        Self {
            max_disk_bytes: DEFAULT_MAX_DISK_BYTES,
            max_queue_bytes: DEFAULT_MAX_QUEUE_BYTES,
            max_queue_events: DEFAULT_MAX_QUEUE_EVENTS,
            committed_channel_events: DEFAULT_COMMITTED_CHANNEL_EVENTS,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayLimits {
    pub max_events: usize,
    pub max_bytes: u64,
}

impl Default for ReplayLimits {
    fn default() -> Self {
        Self {
            max_events: DEFAULT_REPLAY_MAX_EVENTS,
            max_bytes: DEFAULT_REPLAY_MAX_BYTES,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventLogFailureKind {
    Disabled,
    Recovery,
    Serialization,
    EventTooLarge,
    WriterSpawn,
    Open,
    Write,
    Sync,
    QueueBudgetExceeded,
    DiskBudgetExceeded,
    WriterDisconnected,
    FlushTimeout,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventLogFailure {
    pub kind: EventLogFailureKind,
    pub path: PathBuf,
    pub event_seq: Option<u64>,
    pub message: String,
}

impl EventLogFailure {
    fn new(
        kind: EventLogFailureKind,
        path: &Path,
        event_seq: Option<u64>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            path: path.to_path_buf(),
            event_seq,
            message: message.into(),
        }
    }
}

impl fmt::Display for EventLogFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.message)
    }
}

impl std::error::Error for EventLogFailure {}

pub type EventLogResult<T> = Result<T, EventLogFailure>;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoverySummary {
    pub legacy_lines: usize,
    pub truncated_tail_bytes: u64,
    pub completed_final_newline: bool,
    pub available_from: Option<u64>,
    pub available_through: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventLogHealth {
    Disabled,
    Healthy {
        next_event_seq: u64,
        committed_through_event_seq: u64,
        durable_bytes: u64,
        queued_bytes: u64,
        recovery: RecoverySummary,
    },
    Fatal {
        failure: EventLogFailure,
        next_event_seq: u64,
        committed_through_event_seq: u64,
        durable_bytes: u64,
        queued_bytes: u64,
        recovery: RecoverySummary,
    },
}

/// The committed subscription speaks the same value type as the worker wire.
/// JSONL retains its human-readable `ts`; recovery converts that timestamp to
/// the wire's millisecond value.
pub type CommittedEvent = bro_protocol::WorkerEvent;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayDiagnostic {
    RequestedBeforeAvailable {
        requested: u64,
        available_from: u64,
    },
    RequestedAfterAvailable {
        requested: u64,
        available_through: u64,
    },
    SequenceGap {
        line: usize,
        expected: u64,
        found: u64,
    },
    TruncatedTailRecovered {
        removed_bytes: u64,
    },
    ReplayBudgetReached {
        next_event_seq: u64,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReplayBatch {
    pub requested_from: u64,
    pub available_from: Option<u64>,
    pub available_through: Option<u64>,
    pub events: Vec<CommittedEvent>,
    pub next_event_seq: Option<u64>,
    pub diagnostics: Vec<ReplayDiagnostic>,
}

#[derive(Debug)]
struct PreparedLine {
    committed: CommittedEvent,
    bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EventOffset {
    byte_offset: u64,
    line_bytes: u64,
}

enum LogMsg {
    Line(PreparedLine),
    Flush(SyncSender<EventLogResult<()>>),
}

struct SharedState {
    path: PathBuf,
    inert: bool,
    fatal: Mutex<Option<EventLogFailure>>,
    next_event_seq: AtomicU64,
    committed_through_event_seq: AtomicU64,
    durable_bytes: AtomicU64,
    queued_bytes: AtomicU64,
    retained_bytes: AtomicU64,
    recovery: RecoverySummary,
    recovery_diagnostics: Vec<ReplayDiagnostic>,
    offsets: RwLock<BTreeMap<u64, EventOffset>>,
    committed_tx: broadcast::Sender<CommittedEvent>,
}

impl SharedState {
    fn failure(&self) -> Option<EventLogFailure> {
        self.fatal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn fail(&self, failure: EventLogFailure) -> EventLogFailure {
        let mut guard = self
            .fatal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let stored = guard.get_or_insert_with(|| failure.clone()).clone();
        drop(guard);
        if !WARNED.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                path = %stored.path.display(),
                event_seq = stored.event_seq,
                error = %stored.message,
                "session event log entered a fatal state"
            );
        }
        stored
    }
}

pub struct EventLog {
    path: PathBuf,
    limits: EventLogLimits,
    writer: Mutex<Option<SyncSender<LogMsg>>>,
    shared: Arc<SharedState>,
}

impl EventLog {
    pub fn for_session(session_id: &str) -> Self {
        Self::at_path(
            crate::session::sessions_dir().join(format!("{session_id}{EVENT_LOG_SUFFIX}")),
        )
    }

    pub fn at_path(path: PathBuf) -> Self {
        Self::at_path_with_limits(path, EventLogLimits::default())
    }

    pub fn at_path_with_limits(path: PathBuf, limits: EventLogLimits) -> Self {
        let (committed_tx, _) = broadcast::channel(limits.committed_channel_events.max(1));
        let recovered = recover_path(&path);
        let (
            durable_bytes,
            next_event_seq,
            recovery,
            recovery_diagnostics,
            offsets,
            initial_failure,
        ) = match recovered {
            Ok(recovered) => {
                let next = recovered
                    .summary
                    .available_through
                    .map_or(1, |seq| seq.saturating_add(1));
                let budget_failure = (recovered.durable_bytes > limits.max_disk_bytes).then(|| {
                    EventLogFailure::new(
                        EventLogFailureKind::DiskBudgetExceeded,
                        &path,
                        None,
                        format!(
                            "recovered log is {} bytes, above the {} byte disk budget",
                            recovered.durable_bytes, limits.max_disk_bytes
                        ),
                    )
                });
                (
                    recovered.durable_bytes,
                    next,
                    recovered.summary,
                    recovered.diagnostics,
                    recovered.offsets,
                    recovered.failure.or(budget_failure),
                )
            }
            Err(failure) => (
                0,
                1,
                RecoverySummary::default(),
                Vec::new(),
                BTreeMap::new(),
                Some(failure),
            ),
        };
        let shared = Arc::new(SharedState {
            path: path.clone(),
            inert: false,
            fatal: Mutex::new(initial_failure),
            next_event_seq: AtomicU64::new(next_event_seq),
            committed_through_event_seq: AtomicU64::new(next_event_seq.saturating_sub(1)),
            durable_bytes: AtomicU64::new(durable_bytes),
            queued_bytes: AtomicU64::new(0),
            retained_bytes: AtomicU64::new(durable_bytes),
            recovery,
            recovery_diagnostics,
            offsets: RwLock::new(offsets),
            committed_tx,
        });
        Self {
            path,
            limits,
            writer: Mutex::new(None),
            shared,
        }
    }

    pub fn disabled() -> Self {
        let (committed_tx, _) = broadcast::channel(1);
        let shared = Arc::new(SharedState {
            path: PathBuf::new(),
            inert: true,
            fatal: Mutex::new(None),
            next_event_seq: AtomicU64::new(1),
            committed_through_event_seq: AtomicU64::new(0),
            durable_bytes: AtomicU64::new(0),
            queued_bytes: AtomicU64::new(0),
            retained_bytes: AtomicU64::new(0),
            recovery: RecoverySummary::default(),
            recovery_diagnostics: Vec::new(),
            offsets: RwLock::new(BTreeMap::new()),
            committed_tx,
        });
        Self {
            path: PathBuf::new(),
            limits: EventLogLimits::default(),
            writer: Mutex::new(None),
            shared,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn health(&self) -> EventLogHealth {
        if self.shared.inert {
            return EventLogHealth::Disabled;
        }
        let next_event_seq = self.shared.next_event_seq.load(Ordering::Acquire);
        let committed_through_event_seq = self
            .shared
            .committed_through_event_seq
            .load(Ordering::Acquire);
        let durable_bytes = self.shared.durable_bytes.load(Ordering::Acquire);
        let queued_bytes = self.shared.queued_bytes.load(Ordering::Acquire);
        match self.shared.failure() {
            Some(failure) => EventLogHealth::Fatal {
                failure,
                next_event_seq,
                committed_through_event_seq,
                durable_bytes,
                queued_bytes,
                recovery: self.shared.recovery.clone(),
            },
            None => EventLogHealth::Healthy {
                next_event_seq,
                committed_through_event_seq,
                durable_bytes,
                queued_bytes,
                recovery: self.shared.recovery.clone(),
            },
        }
    }

    /// Subscribe to future events after their line and file metadata have been
    /// synchronized. Subscribe before replay, then discard duplicate sequence
    /// numbers, to close the replay-to-live race.
    pub fn subscribe_committed(&self) -> broadcast::Receiver<CommittedEvent> {
        self.shared.committed_tx.subscribe()
    }

    pub fn last_committed_event_seq(&self) -> u64 {
        self.shared
            .committed_through_event_seq
            .load(Ordering::Acquire)
    }

    /// Compatibility wrapper. New worker code should use
    /// [`EventLog::try_append_event`] and inspect the typed result or health.
    pub fn append_event(&self, event: &Value) {
        let _ = self.try_append_event(event);
    }

    pub fn try_append_event(&self, event: &Value) -> EventLogResult<u64> {
        let (occurred_at, occurred_at_unix_ms) = now_timestamp();
        self.try_append_line(occurred_at, occurred_at_unix_ms, event.clone())
    }

    pub fn append_milestone(&self, milestone: &str, session_id: &str, fields: Value) {
        let _ = self.try_append_milestone(milestone, session_id, fields);
    }

    pub fn try_append_milestone(
        &self,
        milestone: &str,
        session_id: &str,
        fields: Value,
    ) -> EventLogResult<u64> {
        let mut event = json!({
            "type": "harness_milestone",
            "milestone": milestone,
            "session_id": session_id,
        });
        if let (Some(obj), Some(extra)) = (event.as_object_mut(), fields.as_object()) {
            for (key, value) in extra {
                obj.entry(key.clone()).or_insert_with(|| value.clone());
            }
        }
        self.try_append_event(&event)
    }

    fn try_append_line(
        &self,
        occurred_at: String,
        occurred_at_unix_ms: u64,
        event: Value,
    ) -> EventLogResult<u64> {
        if self.shared.inert {
            return Err(EventLogFailure::new(
                EventLogFailureKind::Disabled,
                &self.path,
                None,
                "event log is intentionally disabled",
            ));
        }

        let mut writer = self
            .writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(failure) = self.shared.failure() {
            return Err(failure);
        }

        let event_seq = self.shared.next_event_seq.load(Ordering::Acquire);
        let committed = CommittedEvent {
            event_seq,
            occurred_at_unix_ms,
            event,
        };
        let wire_bytes = serde_json::to_vec(&committed).map_err(|error| {
            self.shared.fail(EventLogFailure::new(
                EventLogFailureKind::Serialization,
                &self.path,
                Some(event_seq),
                format!("serializing worker event failed: {error}"),
            ))
        })?;
        if wire_bytes.len() > MAX_EVENT_WIRE_BYTES {
            return Err(self.shared.fail(EventLogFailure::new(
                EventLogFailureKind::EventTooLarge,
                &self.path,
                Some(event_seq),
                format!(
                    "serialized worker event is {} bytes, above the {} byte per-event limit",
                    wire_bytes.len(),
                    MAX_EVENT_WIRE_BYTES
                ),
            )));
        }
        let mut bytes = serde_json::to_vec(&json!({
            "ts": occurred_at,
            "event_seq": committed.event_seq,
            "event": &committed.event,
        }))
        .map_err(|error| {
            self.shared.fail(EventLogFailure::new(
                EventLogFailureKind::Serialization,
                &self.path,
                Some(event_seq),
                format!("serializing event log line failed: {error}"),
            ))
        })?;
        bytes.push(b'\n');
        if bytes.len() > MAX_EVENT_LOG_LINE_BYTES {
            return Err(self.shared.fail(EventLogFailure::new(
                EventLogFailureKind::EventTooLarge,
                &self.path,
                Some(event_seq),
                format!(
                    "serialized event log line is {} bytes, above the {} byte line limit",
                    bytes.len(),
                    MAX_EVENT_LOG_LINE_BYTES
                ),
            )));
        }
        let line_bytes = bytes.len() as u64;
        let queue_reservation = self.shared.queued_bytes.fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |queued_bytes| {
                queued_bytes
                    .checked_add(line_bytes)
                    .filter(|projected| *projected <= self.limits.max_queue_bytes)
            },
        );
        match queue_reservation {
            Ok(_) => {}
            Err(current) => {
                return Err(self.shared.fail(EventLogFailure::new(
                    EventLogFailureKind::QueueBudgetExceeded,
                    &self.path,
                    Some(event_seq),
                    format!(
                        "event would raise queued bytes from {current} to {}, above the {} byte queue budget",
                        current.saturating_add(line_bytes),
                        self.limits.max_queue_bytes
                    ),
                )));
            }
        }
        let retained_reservation = self.shared.retained_bytes.fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |retained_bytes| {
                retained_bytes
                    .checked_add(line_bytes)
                    .filter(|projected| *projected <= self.limits.max_disk_bytes)
            },
        );
        match retained_reservation {
            Ok(_) => {}
            Err(current) => {
                self.shared
                    .queued_bytes
                    .fetch_sub(line_bytes, Ordering::AcqRel);
                return Err(self.shared.fail(EventLogFailure::new(
                    EventLogFailureKind::DiskBudgetExceeded,
                    &self.path,
                    Some(event_seq),
                    format!(
                        "event would raise retained bytes from {current} to {}, above the {} byte disk budget; committed transcript lines are not removed",
                        current.saturating_add(line_bytes),
                        self.limits.max_disk_bytes
                    ),
                )));
            }
        }

        if writer.is_none() {
            match self.spawn_writer() {
                Ok(spawned) => *writer = spawned,
                Err(failure) => {
                    self.shared
                        .queued_bytes
                        .fetch_sub(line_bytes, Ordering::AcqRel);
                    self.shared
                        .retained_bytes
                        .fetch_sub(line_bytes, Ordering::AcqRel);
                    return Err(failure);
                }
            }
        }
        let tx = writer.as_ref().expect("writer initialized above");
        let prepared = PreparedLine { committed, bytes };
        let send_result = match tx.try_send(LogMsg::Line(prepared)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(message)) => tx.send(message).map_err(|_| ()),
            Err(TrySendError::Disconnected(_)) => Err(()),
        };
        if send_result.is_err() {
            self.shared
                .queued_bytes
                .fetch_sub(line_bytes, Ordering::AcqRel);
            self.shared
                .retained_bytes
                .fetch_sub(line_bytes, Ordering::AcqRel);
            *writer = None;
            return Err(self.shared.fail(EventLogFailure::new(
                EventLogFailureKind::WriterDisconnected,
                &self.path,
                Some(event_seq),
                "event log writer disconnected before accepting the line",
            )));
        }
        self.shared
            .next_event_seq
            .store(event_seq.saturating_add(1), Ordering::Release);
        Ok(event_seq)
    }

    pub fn flush_blocking(&self) {
        let _ = self.flush_blocking_result();
    }

    pub fn flush_blocking_result(&self) -> EventLogResult<()> {
        if self.shared.inert {
            return Ok(());
        }
        let writer = self
            .writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(tx) = writer.as_ref() else {
            return self.shared.failure().map_or(Ok(()), Err);
        };
        let (ack_tx, ack_rx) = sync_channel(1);
        tx.send(LogMsg::Flush(ack_tx)).map_err(|_| {
            self.shared.fail(EventLogFailure::new(
                EventLogFailureKind::WriterDisconnected,
                &self.path,
                None,
                "event log writer disconnected during flush",
            ))
        })?;
        match ack_rx.recv_timeout(Duration::from_secs(30)) {
            Ok(result) => result,
            Err(error) => Err(self.shared.fail(EventLogFailure::new(
                EventLogFailureKind::FlushTimeout,
                &self.path,
                None,
                format!("event log flush did not complete: {error}"),
            ))),
        }
    }

    pub fn replay_from(&self, event_seq: u64) -> EventLogResult<ReplayBatch> {
        self.replay_from_with_limits(event_seq, ReplayLimits::default())
    }

    #[allow(clippy::disallowed_methods)]
    pub fn replay_from_with_limits(
        &self,
        event_seq: u64,
        limits: ReplayLimits,
    ) -> EventLogResult<ReplayBatch> {
        let durable_bytes = self.shared.durable_bytes.load(Ordering::Acquire);
        let requested_from = event_seq.max(1);
        let (available_from, available_through, start) = {
            let offsets = self
                .shared
                .offsets
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let is_durable = |offset: &&EventOffset| {
                offset.byte_offset.saturating_add(offset.line_bytes) <= durable_bytes
            };
            let available_from = offsets
                .iter()
                .find(|(_, offset)| is_durable(offset))
                .map(|(seq, _)| *seq);
            let available_through = offsets
                .iter()
                .rev()
                .find(|(_, offset)| is_durable(offset))
                .map(|(seq, _)| *seq);
            let start = offsets
                .range(requested_from..)
                .find(|(_, offset)| is_durable(offset))
                .map(|(seq, offset)| (*seq, *offset));
            (available_from, available_through, start)
        };
        let mut diagnostics = self.shared.recovery_diagnostics.clone();
        if self.shared.recovery.truncated_tail_bytes > 0 {
            diagnostics.push(ReplayDiagnostic::TruncatedTailRecovered {
                removed_bytes: self.shared.recovery.truncated_tail_bytes,
            });
        }
        if let Some(first) = available_from
            && event_seq < first
        {
            diagnostics.push(ReplayDiagnostic::RequestedBeforeAvailable {
                requested: event_seq,
                available_from: first,
            });
        }
        if let Some(last) = available_through
            && event_seq > last.saturating_add(1)
        {
            diagnostics.push(ReplayDiagnostic::RequestedAfterAvailable {
                requested: event_seq,
                available_through: last,
            });
        }

        let mut events = Vec::new();
        let mut replayed_bytes = 0_u64;
        let mut next_event_seq = None;
        if let Some((indexed_seq, offset)) = start {
            let metadata_bytes = std::fs::metadata(&self.path)
                .map_err(|error| replay_io_failure(&self.path, None, "reading metadata", error))?
                .len();
            if metadata_bytes < durable_bytes {
                return Err(EventLogFailure::new(
                    EventLogFailureKind::Recovery,
                    &self.path,
                    None,
                    format!(
                        "event log shrank from the committed {durable_bytes} bytes to {metadata_bytes} bytes"
                    ),
                ));
            }
            let mut file = File::open(&self.path).map_err(|error| {
                replay_io_failure(&self.path, Some(indexed_seq), "opening replay", error)
            })?;
            file.seek(SeekFrom::Start(offset.byte_offset))
                .map_err(|error| {
                    replay_io_failure(
                        &self.path,
                        Some(indexed_seq),
                        "seeking to indexed event",
                        error,
                    )
                })?;
            let remaining = durable_bytes.saturating_sub(offset.byte_offset);
            let mut reader = BufReader::new(file.take(remaining));
            let mut expected_seq = indexed_seq;
            let mut byte_offset = offset.byte_offset;
            while let Some(line) = read_bounded_jsonl_line(&mut reader, MAX_EVENT_LOG_LINE_BYTES)
                .map_err(|error| {
                    if error.kind() == std::io::ErrorKind::InvalidData {
                        EventLogFailure::new(
                            EventLogFailureKind::EventTooLarge,
                            &self.path,
                            Some(expected_seq),
                            format!(
                                "indexed event log line exceeds the {} byte line limit",
                                MAX_EVENT_LOG_LINE_BYTES
                            ),
                        )
                    } else {
                        replay_io_failure(
                            &self.path,
                            Some(expected_seq),
                            "streaming indexed replay",
                            error,
                        )
                    }
                })?
            {
                if !line.ends_with(b"\n") {
                    return Err(EventLogFailure::new(
                        EventLogFailureKind::Recovery,
                        &self.path,
                        Some(expected_seq),
                        "committed replay prefix ended with an incomplete line",
                    ));
                }
                let line_start = byte_offset;
                byte_offset = byte_offset.saturating_add(line.len() as u64);
                let raw = &line[..line.len() - 1];
                if raw.is_empty() {
                    continue;
                }
                let record = parse_indexed_record(&self.path, raw, line_start, expected_seq)?;
                if events.is_empty() && record.event.event_seq != indexed_seq {
                    return Err(EventLogFailure::new(
                        EventLogFailureKind::Recovery,
                        &self.path,
                        Some(record.event.event_seq),
                        format!(
                            "event index points to sequence {indexed_seq}, but the line contains sequence {}",
                            record.event.event_seq
                        ),
                    ));
                }
                if record.wire_bytes > MAX_EVENT_WIRE_BYTES as u64 {
                    return Err(EventLogFailure::new(
                        EventLogFailureKind::EventTooLarge,
                        &self.path,
                        Some(record.event.event_seq),
                        format!(
                            "serialized worker event is {} bytes, above the {} byte per-event limit",
                            record.wire_bytes, MAX_EVENT_WIRE_BYTES
                        ),
                    ));
                }
                let would_exceed_count = events.len() >= limits.max_events;
                let would_exceed_bytes =
                    replayed_bytes.saturating_add(record.wire_bytes) > limits.max_bytes;
                if !events.is_empty() && (would_exceed_count || would_exceed_bytes) {
                    next_event_seq = Some(record.event.event_seq);
                    diagnostics.push(ReplayDiagnostic::ReplayBudgetReached {
                        next_event_seq: record.event.event_seq,
                    });
                    break;
                }
                replayed_bytes = replayed_bytes.saturating_add(record.wire_bytes);
                expected_seq = record.event.event_seq.saturating_add(1);
                events.push(record.event);
            }
        }

        Ok(ReplayBatch {
            requested_from,
            available_from,
            available_through,
            events,
            next_event_seq,
            diagnostics,
        })
    }

    fn spawn_writer(&self) -> EventLogResult<Option<SyncSender<LogMsg>>> {
        let (tx, rx) = sync_channel::<LogMsg>(self.limits.max_queue_events.max(1));
        let shared = self.shared.clone();
        std::thread::Builder::new()
            .name("bro-evlog-writer".into())
            .spawn(move || writer_loop(rx, shared))
            .map(|_| Some(tx))
            .map_err(|error| {
                self.shared.fail(EventLogFailure::new(
                    EventLogFailureKind::WriterSpawn,
                    &self.path,
                    None,
                    format!("event log writer thread failed to spawn: {error}"),
                ))
            })
    }
}

fn writer_loop(rx: Receiver<LogMsg>, shared: Arc<SharedState>) {
    let mut file = None;
    while let Ok(message) = rx.recv() {
        match message {
            LogMsg::Line(line) => {
                let line_bytes = line.bytes.len() as u64;
                if shared.failure().is_none() {
                    let result = commit_line(&shared, &mut file, &line);
                    if let Err(failure) = result {
                        shared.fail(failure);
                    }
                }
                shared.queued_bytes.fetch_sub(line_bytes, Ordering::AcqRel);
            }
            LogMsg::Flush(ack) => {
                let result = shared.failure().map_or(Ok(()), Err);
                let _ = ack.try_send(result);
            }
        }
    }
}

fn commit_line(
    shared: &Arc<SharedState>,
    file: &mut Option<std::fs::File>,
    line: &PreparedLine,
) -> EventLogResult<()> {
    if file.is_none() {
        *file = Some(open_append(&shared.path).map_err(|error| {
            EventLogFailure::new(
                EventLogFailureKind::Open,
                &shared.path,
                Some(line.committed.event_seq),
                format!("opening event log for append failed: {error}"),
            )
        })?);
    }
    let file = file.as_mut().expect("opened above");
    let byte_offset = shared.durable_bytes.load(Ordering::Acquire);
    file.write_all(&line.bytes).map_err(|error| {
        EventLogFailure::new(
            EventLogFailureKind::Write,
            &shared.path,
            Some(line.committed.event_seq),
            format!("writing event log line failed: {error}"),
        )
    })?;
    file.sync_data().map_err(|error| {
        EventLogFailure::new(
            EventLogFailureKind::Sync,
            &shared.path,
            Some(line.committed.event_seq),
            format!("synchronizing event log line failed: {error}"),
        )
    })?;
    shared
        .offsets
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(
            line.committed.event_seq,
            EventOffset {
                byte_offset,
                line_bytes: line.bytes.len() as u64,
            },
        );
    shared.durable_bytes.store(
        byte_offset.saturating_add(line.bytes.len() as u64),
        Ordering::Release,
    );
    shared
        .committed_through_event_seq
        .store(line.committed.event_seq, Ordering::Release);
    let _ = shared.committed_tx.send(line.committed.clone());
    Ok(())
}

#[derive(Debug)]
struct RecoveredPath {
    durable_bytes: u64,
    summary: RecoverySummary,
    diagnostics: Vec<ReplayDiagnostic>,
    offsets: BTreeMap<u64, EventOffset>,
    failure: Option<EventLogFailure>,
}

#[derive(Debug)]
struct ScannedRecord {
    event: CommittedEvent,
    wire_bytes: u64,
}

#[allow(clippy::disallowed_methods)]
fn recover_path(path: &Path) -> EventLogResult<RecoveredPath> {
    if !path.exists() {
        return Ok(RecoveredPath {
            durable_bytes: 0,
            summary: RecoverySummary::default(),
            diagnostics: Vec::new(),
            offsets: BTreeMap::new(),
            failure: None,
        });
    }
    let file = File::open(path).map_err(|error| {
        EventLogFailure::new(
            EventLogFailureKind::Recovery,
            path,
            None,
            format!("reading existing event log failed: {error}"),
        )
    })?;
    let mut reader = BufReader::new(file);
    let mut summary = RecoverySummary::default();
    let original_bytes = reader
        .get_ref()
        .metadata()
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let mut durable_bytes = 0_u64;
    let mut line_number = 0_usize;
    let mut prior_seq = None;
    let mut offsets = BTreeMap::new();
    let mut diagnostics = Vec::new();
    let mut failure = None;
    while let Some(mut line) = read_bounded_jsonl_line(&mut reader, MAX_EVENT_LOG_LINE_BYTES)
        .map_err(|error| {
            EventLogFailure::new(
                EventLogFailureKind::Recovery,
                path,
                prior_seq,
                format!("reading bounded event log line failed: {error}"),
            )
        })?
    {
        line_number += 1;
        let terminated = line.ends_with(b"\n");
        if !terminated {
            if parse_line_value(&line).is_ok() {
                let mut output = OpenOptions::new()
                    .append(true)
                    .open(path)
                    .map_err(|error| {
                        EventLogFailure::new(
                            EventLogFailureKind::Recovery,
                            path,
                            prior_seq,
                            format!("opening event log to complete final newline failed: {error}"),
                        )
                    })?;
                output.write_all(b"\n").map_err(|error| {
                    EventLogFailure::new(
                        EventLogFailureKind::Recovery,
                        path,
                        prior_seq,
                        format!("completing final event log newline failed: {error}"),
                    )
                })?;
                output.sync_data().map_err(|error| {
                    EventLogFailure::new(
                        EventLogFailureKind::Recovery,
                        path,
                        prior_seq,
                        format!("synchronizing completed final newline failed: {error}"),
                    )
                })?;
                line.push(b'\n');
                summary.completed_final_newline = true;
            } else {
                let removed = original_bytes.saturating_sub(durable_bytes);
                let output = OpenOptions::new().write(true).open(path).map_err(|error| {
                    EventLogFailure::new(
                        EventLogFailureKind::Recovery,
                        path,
                        prior_seq,
                        format!("opening event log to remove truncated tail failed: {error}"),
                    )
                })?;
                output.set_len(durable_bytes).map_err(|error| {
                    EventLogFailure::new(
                        EventLogFailureKind::Recovery,
                        path,
                        prior_seq,
                        format!("removing truncated final event log tail failed: {error}"),
                    )
                })?;
                output.sync_data().map_err(|error| {
                    EventLogFailure::new(
                        EventLogFailureKind::Recovery,
                        path,
                        prior_seq,
                        format!("synchronizing event log tail recovery failed: {error}"),
                    )
                })?;
                summary.truncated_tail_bytes = removed;
                break;
            }
        }
        let raw = line.strip_suffix(b"\n").unwrap_or(&line);
        if raw.is_empty() {
            durable_bytes = durable_bytes.saturating_add(line.len() as u64);
            continue;
        }
        let parsed = parse_line_value(raw).map_err(|message| {
            EventLogFailure::new(
                EventLogFailureKind::Recovery,
                path,
                prior_seq,
                format!("invalid event log line {line_number}: {message}"),
            )
        })?;
        if parsed.get("event_seq").is_none_or(Value::is_null) {
            summary.legacy_lines += 1;
        }
        let expected = prior_seq.map_or(1_u64, |seq: u64| seq.saturating_add(1));
        let record = parse_indexed_record(path, raw, durable_bytes, expected)?;
        if prior_seq.is_some() && record.event.event_seq != expected {
            diagnostics.push(ReplayDiagnostic::SequenceGap {
                line: line_number,
                expected,
                found: record.event.event_seq,
            });
            failure.get_or_insert_with(|| {
                EventLogFailure::new(
                    EventLogFailureKind::Recovery,
                    path,
                    Some(record.event.event_seq),
                    format!(
                        "event sequence gap at line {line_number}: expected {expected}, found {}",
                        record.event.event_seq
                    ),
                )
            });
        }
        if record.wire_bytes > MAX_EVENT_WIRE_BYTES as u64 && failure.is_none() {
            failure = Some(EventLogFailure::new(
                EventLogFailureKind::EventTooLarge,
                path,
                Some(record.event.event_seq),
                format!(
                    "recovered worker event is {} bytes, above the {} byte per-event limit",
                    record.wire_bytes, MAX_EVENT_WIRE_BYTES
                ),
            ));
        }
        summary.available_from.get_or_insert(record.event.event_seq);
        summary.available_through = Some(record.event.event_seq);
        offsets.insert(
            record.event.event_seq,
            EventOffset {
                byte_offset: durable_bytes,
                line_bytes: line.len() as u64,
            },
        );
        prior_seq = Some(record.event.event_seq);
        durable_bytes = durable_bytes.saturating_add(line.len() as u64);
    }
    Ok(RecoveredPath {
        durable_bytes,
        summary,
        diagnostics,
        offsets,
        failure,
    })
}

fn parse_indexed_record(
    path: &Path,
    raw: &[u8],
    byte_offset: u64,
    expected_seq: u64,
) -> EventLogResult<ScannedRecord> {
    let value = parse_line_value(raw).map_err(|message| {
        EventLogFailure::new(
            EventLogFailureKind::Recovery,
            path,
            Some(expected_seq),
            format!("invalid event log line at byte {byte_offset}: {message}"),
        )
    })?;
    let object = value.as_object().expect("parse_line_value verifies object");
    let event_seq = match object.get("event_seq") {
        None | Some(Value::Null) => expected_seq,
        Some(value) => value.as_u64().filter(|seq| *seq > 0).ok_or_else(|| {
            EventLogFailure::new(
                EventLogFailureKind::Recovery,
                path,
                Some(expected_seq),
                format!("invalid event_seq at event log byte {byte_offset}"),
            )
        })?,
    };
    let occurred_at = object.get("ts").and_then(Value::as_str).ok_or_else(|| {
        EventLogFailure::new(
            EventLogFailureKind::Recovery,
            path,
            Some(event_seq),
            format!("event log line at byte {byte_offset} has no string ts"),
        )
    })?;
    let occurred_at_unix_ms = chrono::DateTime::parse_from_rfc3339(occurred_at)
        .map_err(|error| {
            EventLogFailure::new(
                EventLogFailureKind::Recovery,
                path,
                Some(event_seq),
                format!("event log line at byte {byte_offset} has invalid ts: {error}"),
            )
        })?
        .timestamp_millis()
        .try_into()
        .map_err(|_| {
            EventLogFailure::new(
                EventLogFailureKind::Recovery,
                path,
                Some(event_seq),
                format!("event log line at byte {byte_offset} has a pre-epoch ts"),
            )
        })?;
    let event = object.get("event").cloned().ok_or_else(|| {
        EventLogFailure::new(
            EventLogFailureKind::Recovery,
            path,
            Some(event_seq),
            format!("event log line at byte {byte_offset} has no event"),
        )
    })?;
    let event = CommittedEvent {
        event_seq,
        occurred_at_unix_ms,
        event,
    };
    let wire_bytes = serde_json::to_vec(&event).map_err(|error| {
        EventLogFailure::new(
            EventLogFailureKind::Recovery,
            path,
            Some(event_seq),
            format!("serializing replayed worker event failed: {error}"),
        )
    })?;
    Ok(ScannedRecord {
        event,
        wire_bytes: wire_bytes.len() as u64,
    })
}

fn replay_io_failure(
    path: &Path,
    event_seq: Option<u64>,
    operation: &str,
    error: std::io::Error,
) -> EventLogFailure {
    EventLogFailure::new(
        EventLogFailureKind::Recovery,
        path,
        event_seq,
        format!("{operation} for event log replay failed: {error}"),
    )
}

fn read_bounded_jsonl_line<R: BufRead>(
    reader: &mut R,
    max_bytes: usize,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok((!line.is_empty()).then_some(line));
        }
        let through_newline = buffer
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|position| position + 1);
        let consumed = through_newline.unwrap_or(buffer.len());
        if line.len().saturating_add(consumed) > max_bytes {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "JSONL line exceeds its configured bound",
            ));
        }
        line.extend_from_slice(&buffer[..consumed]);
        reader.consume(consumed);
        if through_newline.is_some() {
            return Ok(Some(line));
        }
    }
}

fn parse_line_value(raw: &[u8]) -> Result<Value, String> {
    let value: Value = serde_json::from_slice(raw).map_err(|error| error.to_string())?;
    if !value.is_object() {
        return Err("line is not a JSON object".to_string());
    }
    if value.get("event").is_none() {
        return Err("line has no event field".to_string());
    }
    Ok(value)
}

#[allow(clippy::disallowed_methods)]
fn open_append(path: &Path) -> std::io::Result<std::fs::File> {
    let created = !path.exists();
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    if let Ok(metadata) = std::fs::symlink_metadata(path)
        && (metadata.file_type().is_symlink() || !metadata.is_file())
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "event log path is not a regular file",
        ));
    }
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    if created {
        file.sync_all()?;
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            File::open(parent)?.sync_all()?;
        }
    }
    Ok(file)
}

fn now_timestamp() -> (String, u64) {
    let now = Utc::now();
    (
        now.to_rfc3339_opts(SecondsFormat::Millis, true),
        now.timestamp_millis().max(0) as u64,
    )
}

#[cfg(test)]
// Filesystem fixtures intentionally exercise append, repair, and replay semantics.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    fn read_lines(path: &Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn append_assigns_monotonic_sequence_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let log = EventLog::at_path(dir.path().join("s1.events.jsonl"));

        assert_eq!(
            log.try_append_milestone("session_start", "s1", json!({"transport": "anthropic"}))
                .unwrap(),
            1
        );
        assert_eq!(
            log.try_append_event(&json!({"type": "assistant", "session_id": "s1"}))
                .unwrap(),
            2
        );
        assert_eq!(
            log.try_append_event(&json!({"type": "result", "session_id": "s1"}))
                .unwrap(),
            3
        );
        log.flush_blocking_result().unwrap();

        let lines = read_lines(log.path());
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0]["event_seq"], 1);
        assert_eq!(lines[1]["event_seq"], 2);
        assert_eq!(lines[2]["event_seq"], 3);
        for line in &lines {
            chrono::DateTime::parse_from_rfc3339(line["ts"].as_str().unwrap()).unwrap();
            assert!(line["event"].is_object());
        }
    }

    #[test]
    fn startup_recovers_legacy_prefix_and_sequence_continuity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.events.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"ts\":\"2026-01-01T00:00:00Z\",\"event\":{\"type\":\"system\"}}\n",
                "{\"ts\":\"2026-01-01T00:00:01Z\",\"event\":{\"type\":\"assistant\"}}\n"
            ),
        )
        .unwrap();

        let log = EventLog::at_path(path);
        match log.health() {
            EventLogHealth::Healthy {
                next_event_seq,
                recovery,
                ..
            } => {
                assert_eq!(next_event_seq, 3);
                assert_eq!(recovery.legacy_lines, 2);
                assert_eq!(recovery.available_from, Some(1));
            }
            other => panic!("unexpected health: {other:?}"),
        }
        assert_eq!(log.try_append_event(&json!({"type": "result"})).unwrap(), 3);
        log.flush_blocking_result().unwrap();
        let lines = read_lines(log.path());
        assert!(lines[0].get("event_seq").is_none());
        assert!(lines[1].get("event_seq").is_none());
        assert_eq!(lines[2]["event_seq"], 3);
    }

    #[test]
    fn startup_removes_only_truncated_final_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("truncated.events.jsonl");
        let complete =
            "{\"ts\":\"2026-01-01T00:00:00Z\",\"event_seq\":1,\"event\":{\"type\":\"system\"}}\n";
        std::fs::write(&path, format!("{complete}{{\"ts\":\"cut")).unwrap();

        let log = EventLog::at_path(path);
        match log.health() {
            EventLogHealth::Healthy { recovery, .. } => {
                assert!(recovery.truncated_tail_bytes > 0);
            }
            other => panic!("unexpected health: {other:?}"),
        }
        assert_eq!(
            log.try_append_event(&json!({"type": "assistant"})).unwrap(),
            2
        );
        log.flush_blocking_result().unwrap();
        let bytes = std::fs::read(log.path()).unwrap();
        assert!(bytes.starts_with(complete.as_bytes()));
        assert_eq!(read_lines(log.path()).len(), 2);
    }

    #[tokio::test]
    async fn committed_subscription_fires_after_durable_append() {
        let dir = tempfile::tempdir().unwrap();
        let log = EventLog::at_path(dir.path().join("committed.events.jsonl"));
        let mut committed = log.subscribe_committed();

        let seq = log.try_append_event(&json!({"type": "assistant"})).unwrap();
        let delivered = tokio::time::timeout(Duration::from_secs(2), committed.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(delivered.event_seq, seq);
        assert_eq!(log.last_committed_event_seq(), seq);
        let line = read_lines(log.path()).pop().unwrap();
        assert_eq!(line["event_seq"], seq);
        assert!(matches!(log.health(), EventLogHealth::Healthy { .. }));
    }

    #[test]
    fn replay_reports_lower_bound_and_sequence_gaps() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gap.events.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"ts\":\"2026-01-01T00:00:00Z\",\"event_seq\":4,\"event\":{\"type\":\"system\"}}\n",
                "{\"ts\":\"2026-01-01T00:00:01Z\",\"event_seq\":6,\"event\":{\"type\":\"result\"}}\n"
            ),
        )
        .unwrap();

        let log = EventLog::at_path(path);
        assert!(matches!(log.health(), EventLogHealth::Fatal { .. }));
        let replay = log.replay_from(1).unwrap();
        assert_eq!(replay.events.len(), 2);
        assert!(replay.diagnostics.iter().any(|diagnostic| matches!(
            diagnostic,
            ReplayDiagnostic::RequestedBeforeAvailable {
                requested: 1,
                available_from: 4
            }
        )));
        assert!(replay.diagnostics.iter().any(|diagnostic| matches!(
            diagnostic,
            ReplayDiagnostic::SequenceGap {
                expected: 5,
                found: 6,
                ..
            }
        )));
    }

    #[test]
    fn replay_is_bounded_and_resumable() {
        let dir = tempfile::tempdir().unwrap();
        let log = EventLog::at_path(dir.path().join("replay.events.jsonl"));
        for index in 0..3 {
            log.try_append_event(&json!({"type": "assistant", "index": index}))
                .unwrap();
        }
        log.flush_blocking_result().unwrap();

        let first = log
            .replay_from_with_limits(
                1,
                ReplayLimits {
                    max_events: 2,
                    max_bytes: u64::MAX,
                },
            )
            .unwrap();
        assert_eq!(
            first
                .events
                .iter()
                .map(|event| event.event_seq)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(first.next_event_seq, Some(3));
        let second = log.replay_from(first.next_event_seq.unwrap()).unwrap();
        assert_eq!(second.events[0].event_seq, 3);
    }

    #[test]
    fn replay_index_is_recovered_and_extended_after_a_durable_append() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("indexed.events.jsonl");
        {
            let log = EventLog::at_path(path.clone());
            log.try_append_event(&json!({"type": "assistant", "index": 1}))
                .unwrap();
            log.try_append_event(&json!({"type": "assistant", "index": 2}))
                .unwrap();
            log.flush_blocking_result().unwrap();
        }

        let reopened = EventLog::at_path(path);
        let second_end = {
            let offsets = reopened.shared.offsets.read().unwrap();
            assert_eq!(offsets.keys().copied().collect::<Vec<_>>(), vec![1, 2]);
            let first = offsets.get(&1).unwrap();
            let second = offsets.get(&2).unwrap();
            assert_eq!(second.byte_offset, first.byte_offset + first.line_bytes);
            second.byte_offset + second.line_bytes
        };
        reopened
            .try_append_event(&json!({"type": "result", "index": 3}))
            .unwrap();
        reopened.flush_blocking_result().unwrap();

        let offsets = reopened.shared.offsets.read().unwrap();
        assert_eq!(offsets.get(&3).unwrap().byte_offset, second_end);
        drop(offsets);
        let replay = reopened.replay_from(3).unwrap();
        assert_eq!(
            replay
                .events
                .iter()
                .map(|event| event.event_seq)
                .collect::<Vec<_>>(),
            vec![3]
        );
    }

    #[test]
    fn replay_returns_one_bounded_event_when_it_exceeds_the_chunk_target() {
        let dir = tempfile::tempdir().unwrap();
        let log = EventLog::at_path(dir.path().join("progress.events.jsonl"));
        log.try_append_event(&json!({
            "type": "assistant",
            "text": "x".repeat(64 * 1024),
        }))
        .unwrap();
        log.try_append_event(&json!({"type": "result"})).unwrap();
        log.flush_blocking_result().unwrap();

        let limits = ReplayLimits {
            max_events: 0,
            max_bytes: 1,
        };
        let first = log.replay_from_with_limits(1, limits).unwrap();
        assert_eq!(first.events[0].event_seq, 1);
        assert_eq!(first.next_event_seq, Some(2));
        let second = log.replay_from_with_limits(2, limits).unwrap();
        assert_eq!(second.events[0].event_seq, 2);
        assert_eq!(second.next_event_seq, None);
    }

    #[test]
    fn append_rejects_an_event_that_cannot_fit_in_one_rpc_frame() {
        let dir = tempfile::tempdir().unwrap();
        let log = EventLog::at_path(dir.path().join("oversized.events.jsonl"));
        let error = log
            .try_append_event(&json!({
                "type": "assistant",
                "text": "x".repeat(MAX_EVENT_WIRE_BYTES),
            }))
            .unwrap_err();
        assert_eq!(error.kind, EventLogFailureKind::EventTooLarge);
        assert_eq!(log.last_committed_event_seq(), 0);
        assert!(!log.path().exists());
    }

    #[test]
    fn replay_ack_boundary_never_rewrites_committed_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let log = EventLog::at_path(dir.path().join("append-only.events.jsonl"));
        for index in 0..2 {
            log.try_append_event(&json!({"type": "assistant", "index": index}))
                .unwrap();
        }
        log.flush_blocking_result().unwrap();
        let acknowledged_prefix = std::fs::read(log.path()).unwrap();

        // A fleet acknowledgement is represented by replaying after the
        // acknowledged sequence. It must never compact or remove transcript
        // lines, even though those events no longer need network delivery.
        let replay = log.replay_from(3).unwrap();
        assert!(replay.events.is_empty());
        assert_eq!(std::fs::read(log.path()).unwrap(), acknowledged_prefix);

        assert_eq!(log.try_append_event(&json!({"type": "result"})).unwrap(), 3);
        log.flush_blocking_result().unwrap();
        let extended = std::fs::read(log.path()).unwrap();
        assert!(extended.starts_with(&acknowledged_prefix));
        assert!(extended.len() > acknowledged_prefix.len());
    }

    #[test]
    fn disk_budget_failure_is_typed_and_does_not_truncate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("budget.events.jsonl");
        let log = EventLog::at_path_with_limits(
            path,
            EventLogLimits {
                max_disk_bytes: 1,
                ..EventLogLimits::default()
            },
        );
        let error = log
            .try_append_event(&json!({"type": "assistant"}))
            .unwrap_err();
        assert_eq!(error.kind, EventLogFailureKind::DiskBudgetExceeded);
        assert!(matches!(log.health(), EventLogHealth::Fatal { .. }));
        assert!(!log.path().exists());
    }

    #[test]
    fn queue_budget_failure_is_typed() {
        let dir = tempfile::tempdir().unwrap();
        let log = EventLog::at_path_with_limits(
            dir.path().join("queue.events.jsonl"),
            EventLogLimits {
                max_queue_bytes: 1,
                ..EventLogLimits::default()
            },
        );
        let error = log
            .try_append_event(&json!({"type": "assistant"}))
            .unwrap_err();
        assert_eq!(error.kind, EventLogFailureKind::QueueBudgetExceeded);
        assert!(matches!(log.health(), EventLogHealth::Fatal { .. }));
    }

    #[test]
    fn write_failure_is_visible_through_result_and_health() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, "x").unwrap();
        let log = EventLog::at_path(blocker.join("s.events.jsonl"));

        log.try_append_event(&json!({"type": "assistant"})).unwrap();
        let error = log.flush_blocking_result().unwrap_err();
        assert_eq!(error.kind, EventLogFailureKind::Open);
        assert!(matches!(log.health(), EventLogHealth::Fatal { .. }));
    }

    #[test]
    fn disabled_log_is_inert_and_typed() {
        let log = EventLog::disabled();
        let error = log
            .try_append_event(&json!({"type": "assistant"}))
            .unwrap_err();
        assert_eq!(error.kind, EventLogFailureKind::Disabled);
        assert_eq!(log.health(), EventLogHealth::Disabled);
        assert_eq!(log.path(), Path::new(""));
    }
}
