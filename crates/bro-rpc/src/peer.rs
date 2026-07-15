use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bro_protocol::{Envelope, ProtocolError, WorkerMessage};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::framing::{read_json_frame_with_len, serialize_bounded, write_frame};
use crate::{
    ConnectionBinding, DisconnectReason, NegotiatedIo, RpcError, RpcPhase, validate_envelope,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessagePriority {
    Control,
    Normal,
    Replay,
}

impl MessagePriority {
    fn as_str(self) -> &'static str {
        match self {
            Self::Control => "control",
            Self::Normal => "normal",
            Self::Replay => "replay",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PeerConfig {
    pub control_queue_capacity: usize,
    pub normal_queue_capacity: usize,
    pub replay_queue_capacity: usize,
    pub inbound_queue_capacity: usize,
    pub inbound_queue_bytes: usize,
    pub control_queue_bytes: usize,
    pub bulk_queue_bytes: usize,
    pub max_in_flight_requests: usize,
    pub request_timeout: Duration,
    pub write_timeout: Duration,
    pub read_idle_timeout: Option<Duration>,
}

impl Default for PeerConfig {
    fn default() -> Self {
        Self {
            control_queue_capacity: 128,
            normal_queue_capacity: 512,
            replay_queue_capacity: 512,
            inbound_queue_capacity: 512,
            inbound_queue_bytes: 16 * 1024 * 1024,
            control_queue_bytes: 2 * 1024 * 1024,
            bulk_queue_bytes: 16 * 1024 * 1024,
            max_in_flight_requests: 256,
            request_timeout: Duration::from_secs(30),
            write_timeout: Duration::from_secs(10),
            read_idle_timeout: Some(Duration::from_secs(60)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PendingKey {
    generation: u64,
    message_id: String,
}

type PendingSender = oneshot::Sender<Result<InboundEnvelope, RpcError>>;

#[derive(Debug)]
struct PeerState {
    binding: ConnectionBinding,
    max_frame_bytes: usize,
    control_queue_bytes: usize,
    bulk_queue_bytes: usize,
    inbound_queue_bytes: usize,
    max_in_flight_requests: usize,
    request_timeout: Duration,
    control_budget: Arc<Semaphore>,
    bulk_budget: Arc<Semaphore>,
    inbound_budget: Arc<Semaphore>,
    in_flight: Arc<Semaphore>,
    pending: Mutex<HashMap<PendingKey, PendingSender>>,
    next_message_id: AtomicU64,
    message_id_prefix: String,
    disconnected: AtomicBool,
    disconnect_tx: watch::Sender<Option<DisconnectReason>>,
}

impl PeerState {
    fn pending(&self) -> MutexGuard<'_, HashMap<PendingKey, PendingSender>> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn ensure_connected(&self) -> Result<(), RpcError> {
        if self.disconnected.load(Ordering::Acquire) {
            return Err(RpcError::disconnected(
                self.disconnect_reason()
                    .unwrap_or(DisconnectReason::PeerClosed),
            ));
        }
        Ok(())
    }

    fn disconnect_reason(&self) -> Option<DisconnectReason> {
        self.disconnect_tx.borrow().clone()
    }

    fn disconnect(&self, reason: DisconnectReason) {
        if self
            .disconnected
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        self.disconnect_tx.send_replace(Some(reason.clone()));
        let pending = std::mem::take(&mut *self.pending());
        for (_, sender) in pending {
            let _ = sender.send(Err(RpcError::disconnected(reason.clone())));
        }
    }
}

#[derive(Debug)]
struct OutboundFrame {
    bytes: Vec<u8>,
    _permit: OwnedSemaphorePermit,
}

#[derive(Debug)]
struct InboundEnvelope {
    envelope: Envelope,
    _permit: OwnedSemaphorePermit,
}

impl InboundEnvelope {
    fn into_envelope(self) -> Envelope {
        self.envelope
    }
}

#[derive(Debug, Clone)]
pub struct PeerHandle {
    state: Arc<PeerState>,
    control_tx: mpsc::Sender<OutboundFrame>,
    normal_tx: mpsc::Sender<OutboundFrame>,
    replay_tx: mpsc::Sender<OutboundFrame>,
}

impl PeerHandle {
    pub fn binding(&self) -> ConnectionBinding {
        self.state.binding
    }

    pub fn is_disconnected(&self) -> bool {
        self.state.disconnected.load(Ordering::Acquire)
    }

    pub fn pending_request_count(&self) -> usize {
        self.state.pending().len()
    }

    pub fn inbound_available_bytes(&self) -> usize {
        self.state.inbound_budget.available_permits()
    }

    pub fn inbound_byte_limit(&self) -> usize {
        self.state.inbound_queue_bytes
    }

    pub async fn wait_disconnected(&self) -> DisconnectReason {
        let mut receiver = self.state.disconnect_tx.subscribe();
        loop {
            if let Some(reason) = receiver.borrow().clone() {
                return reason;
            }
            if receiver.changed().await.is_err() {
                return DisconnectReason::PeerClosed;
            }
        }
    }

    pub fn shutdown(&self) {
        self.state.disconnect(DisconnectReason::LocalShutdown);
    }

    pub fn next_message_id(&self) -> String {
        let sequence = self.state.next_message_id.fetch_add(1, Ordering::Relaxed);
        format!(
            "{}-generation-{}-message-{sequence}",
            self.state.message_id_prefix, self.state.binding.connection_generation
        )
    }

    pub fn send(&self, body: WorkerMessage, priority: MessagePriority) -> Result<String, RpcError> {
        let message_id = self.next_message_id();
        self.send_with_id(message_id, body, priority)
    }

    pub fn send_with_id(
        &self,
        message_id: impl Into<String>,
        body: WorkerMessage,
        priority: MessagePriority,
    ) -> Result<String, RpcError> {
        let message_id = message_id.into();
        let envelope = Envelope {
            protocol_version: self.state.binding.protocol_version,
            connection_generation: self.state.binding.connection_generation,
            message_id: message_id.clone(),
            reply_to: None,
            body,
        };
        self.queue_envelope(envelope, priority)?;
        Ok(message_id)
    }

    pub fn respond(
        &self,
        request: &Envelope,
        body: WorkerMessage,
        priority: MessagePriority,
    ) -> Result<String, RpcError> {
        validate_envelope(request, self.state.binding)?;
        let message_id = self.next_message_id();
        let envelope = Envelope {
            protocol_version: self.state.binding.protocol_version,
            connection_generation: self.state.binding.connection_generation,
            message_id: message_id.clone(),
            reply_to: Some(request.message_id.clone()),
            body,
        };
        self.queue_envelope(envelope, priority)?;
        Ok(message_id)
    }

    pub fn send_protocol_error(&self, error: ProtocolError) -> Result<String, RpcError> {
        self.send(
            WorkerMessage::ProtocolError(error),
            MessagePriority::Control,
        )
    }

    pub async fn request(
        &self,
        body: WorkerMessage,
        priority: MessagePriority,
    ) -> Result<Envelope, RpcError> {
        let message_id = self.next_message_id();
        self.request_with_id(message_id, body, priority, self.default_request_timeout())
            .await
    }

    pub async fn request_with_id(
        &self,
        message_id: impl Into<String>,
        body: WorkerMessage,
        priority: MessagePriority,
        request_timeout: Duration,
    ) -> Result<Envelope, RpcError> {
        self.state.ensure_connected()?;
        if request_timeout.is_zero() {
            return Err(RpcError::InvalidConfiguration(
                "request timeout must be greater than zero".to_string(),
            ));
        }
        let _in_flight = self
            .state
            .in_flight
            .clone()
            .try_acquire_owned()
            .map_err(|_| RpcError::TooManyInFlightRequests {
                limit: self.state.max_in_flight_requests,
            })?;
        let message_id = message_id.into();
        let key = PendingKey {
            generation: self.state.binding.connection_generation,
            message_id: message_id.clone(),
        };
        let (sender, receiver) = oneshot::channel();
        {
            let mut pending = self.state.pending();
            if pending.contains_key(&key) {
                return Err(RpcError::DuplicateMessageId {
                    generation: key.generation,
                    message_id,
                });
            }
            pending.insert(key.clone(), sender);
        }
        let _pending_guard = PendingGuard {
            state: self.state.clone(),
            key,
        };
        self.send_with_id(message_id, body, priority)?;

        match timeout(request_timeout, receiver).await {
            Err(_) => Err(RpcError::Timeout {
                phase: RpcPhase::Request,
            }),
            Ok(Ok(result)) => result.map(InboundEnvelope::into_envelope),
            Ok(Err(_)) => {
                if let Some(reason) = self.state.disconnect_reason() {
                    Err(RpcError::disconnected(reason))
                } else {
                    Err(RpcError::ResponseChannelClosed)
                }
            }
        }
    }

    fn default_request_timeout(&self) -> Duration {
        self.state.request_timeout
    }

    fn queue_envelope(
        &self,
        envelope: Envelope,
        priority: MessagePriority,
    ) -> Result<(), RpcError> {
        self.state.ensure_connected()?;
        validate_envelope(&envelope, self.state.binding)?;
        let bytes = serialize_bounded(&envelope, self.state.max_frame_bytes)?;
        let framed_bytes = bytes.len().saturating_add(4);
        let (budget, byte_limit, sender) = match priority {
            MessagePriority::Control => (
                self.state.control_budget.clone(),
                self.state.control_queue_bytes,
                &self.control_tx,
            ),
            MessagePriority::Normal => (
                self.state.bulk_budget.clone(),
                self.state.bulk_queue_bytes,
                &self.normal_tx,
            ),
            MessagePriority::Replay => (
                self.state.bulk_budget.clone(),
                self.state.bulk_queue_bytes,
                &self.replay_tx,
            ),
        };
        let permits = u32::try_from(framed_bytes).map_err(|_| RpcError::QueueFull {
            priority: priority.as_str(),
            byte_limit,
        })?;
        let permit = budget
            .try_acquire_many_owned(permits)
            .map_err(|_| RpcError::QueueFull {
                priority: priority.as_str(),
                byte_limit,
            })?;
        let frame = OutboundFrame {
            bytes,
            _permit: permit,
        };
        sender.try_send(frame).map_err(|_| {
            if let Some(reason) = self.state.disconnect_reason() {
                RpcError::disconnected(reason)
            } else {
                RpcError::QueueFull {
                    priority: priority.as_str(),
                    byte_limit,
                }
            }
        })
    }
}

#[derive(Debug)]
struct PendingGuard {
    state: Arc<PeerState>,
    key: PendingKey,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.state.pending().remove(&self.key);
    }
}

#[derive(Debug)]
pub struct RpcPeer {
    handle: PeerHandle,
    inbound_rx: mpsc::Receiver<InboundEnvelope>,
    reader_task: JoinHandle<()>,
    writer_task: JoinHandle<()>,
}

impl RpcPeer {
    pub fn spawn<T>(negotiated: NegotiatedIo<T>, config: PeerConfig) -> Result<Self, RpcError>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        validate_config(&config)?;
        let (io, binding, max_frame_bytes) = negotiated.into_parts();
        if config.inbound_queue_bytes < max_frame_bytes.saturating_add(4) {
            return Err(RpcError::InvalidConfiguration(format!(
                "inbound_queue_bytes must hold one maximum frame (at least {} bytes)",
                max_frame_bytes.saturating_add(4)
            )));
        }
        let (reader, writer) = tokio::io::split(io);
        let (control_tx, control_rx) = mpsc::channel(config.control_queue_capacity);
        let (normal_tx, normal_rx) = mpsc::channel(config.normal_queue_capacity);
        let (replay_tx, replay_rx) = mpsc::channel(config.replay_queue_capacity);
        let (inbound_tx, inbound_rx) = mpsc::channel(config.inbound_queue_capacity);
        let (disconnect_tx, disconnect_rx) = watch::channel(None);
        let state = Arc::new(PeerState {
            binding,
            max_frame_bytes,
            control_queue_bytes: config.control_queue_bytes,
            bulk_queue_bytes: config.bulk_queue_bytes,
            inbound_queue_bytes: config.inbound_queue_bytes,
            max_in_flight_requests: config.max_in_flight_requests,
            request_timeout: config.request_timeout,
            control_budget: Arc::new(Semaphore::new(config.control_queue_bytes)),
            bulk_budget: Arc::new(Semaphore::new(config.bulk_queue_bytes)),
            inbound_budget: Arc::new(Semaphore::new(config.inbound_queue_bytes)),
            in_flight: Arc::new(Semaphore::new(config.max_in_flight_requests)),
            pending: Mutex::new(HashMap::new()),
            next_message_id: AtomicU64::new(1),
            message_id_prefix: next_peer_prefix(),
            disconnected: AtomicBool::new(false),
            disconnect_tx,
        });
        let handle = PeerHandle {
            state: state.clone(),
            control_tx,
            normal_tx,
            replay_tx,
        };
        let reader_task = tokio::spawn(run_reader(
            reader,
            binding,
            max_frame_bytes,
            config.read_idle_timeout,
            inbound_tx,
            state.clone(),
            disconnect_rx.clone(),
        ));
        let writer_task = tokio::spawn(run_writer(
            writer,
            config.write_timeout,
            control_rx,
            normal_rx,
            replay_rx,
            state,
            disconnect_rx,
        ));
        Ok(Self {
            handle,
            inbound_rx,
            reader_task,
            writer_task,
        })
    }

    pub fn handle(&self) -> PeerHandle {
        self.handle.clone()
    }

    pub async fn recv(&mut self) -> Result<Envelope, RpcError> {
        match self.inbound_rx.recv().await {
            Some(envelope) => Ok(envelope.into_envelope()),
            None => Err(RpcError::disconnected(
                self.handle
                    .state
                    .disconnect_reason()
                    .unwrap_or(DisconnectReason::PeerClosed),
            )),
        }
    }
}

impl Drop for RpcPeer {
    fn drop(&mut self) {
        self.handle
            .state
            .disconnect(DisconnectReason::LocalShutdown);
        self.reader_task.abort();
        self.writer_task.abort();
    }
}

async fn run_reader<R>(
    mut reader: R,
    binding: ConnectionBinding,
    max_frame_bytes: usize,
    read_idle_timeout: Option<Duration>,
    inbound_tx: mpsc::Sender<InboundEnvelope>,
    state: Arc<PeerState>,
    mut disconnect_rx: watch::Receiver<Option<DisconnectReason>>,
) where
    R: AsyncRead + Unpin,
{
    loop {
        let read = read_envelope(&mut reader, binding, max_frame_bytes, read_idle_timeout);
        tokio::pin!(read);
        let (envelope, frame_bytes) = tokio::select! {
            biased;
            changed = disconnect_rx.changed() => {
                if changed.is_err() || disconnect_rx.borrow().is_some() {
                    return;
                }
                continue;
            }
            result = &mut read => match result {
                Ok(envelope) => envelope,
                Err(error) => {
                    state.disconnect(read_disconnect_reason(&error));
                    return;
                }
            }
        };

        let permits = match u32::try_from(frame_bytes.saturating_add(4)) {
            Ok(permits) => permits,
            Err(_) => {
                state.disconnect(DisconnectReason::ReadFailed(
                    "inbound frame byte budget overflow".to_string(),
                ));
                return;
            }
        };
        let permit = tokio::select! {
            biased;
            changed = disconnect_rx.changed() => {
                if changed.is_err() || disconnect_rx.borrow().is_some() {
                    return;
                }
                continue;
            }
            permit = state.inbound_budget.clone().acquire_many_owned(permits) => {
                match permit {
                    Ok(permit) => permit,
                    Err(_) => return,
                }
            }
        };
        let inbound = InboundEnvelope {
            envelope,
            _permit: permit,
        };

        if let Some(reply_to) = inbound.envelope.reply_to.as_ref() {
            let key = PendingKey {
                generation: binding.connection_generation,
                message_id: reply_to.clone(),
            };
            if let Some(sender) = state.pending().remove(&key) {
                let _ = sender.send(Ok(inbound));
            }
            continue;
        }

        tokio::select! {
            biased;
            changed = disconnect_rx.changed() => {
                if changed.is_err() || disconnect_rx.borrow().is_some() {
                    return;
                }
            }
            result = inbound_tx.send(inbound) => {
                if result.is_err() {
                    state.disconnect(DisconnectReason::InboundConsumerClosed);
                    return;
                }
            }
        }
    }
}

async fn read_envelope<R: AsyncRead + Unpin>(
    reader: &mut R,
    binding: ConnectionBinding,
    max_frame_bytes: usize,
    read_idle_timeout: Option<Duration>,
) -> Result<(Envelope, usize), RpcError> {
    let read = read_json_frame_with_len::<_, Envelope>(reader, max_frame_bytes);
    let (envelope, frame_bytes) = match read_idle_timeout {
        Some(duration) => timeout(duration, read)
            .await
            .map_err(|_| RpcError::Timeout {
                phase: RpcPhase::Read,
            })??,
        None => read.await?,
    };
    validate_envelope(&envelope, binding)?;
    Ok((envelope, frame_bytes))
}

fn read_disconnect_reason(error: &RpcError) -> DisconnectReason {
    match error {
        RpcError::Io(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
            DisconnectReason::PeerClosed
        }
        RpcError::Timeout {
            phase: RpcPhase::Read,
        } => DisconnectReason::IdleTimeout,
        RpcError::ProtocolVersionMismatch { .. }
        | RpcError::StaleGeneration { .. }
        | RpcError::InvalidMessageId { .. }
        | RpcError::CorrelationMismatch(_) => {
            DisconnectReason::ProtocolViolation(error.to_string())
        }
        _ => DisconnectReason::ReadFailed(error.to_string()),
    }
}

async fn run_writer<W>(
    mut writer: W,
    write_timeout: Duration,
    mut control_rx: mpsc::Receiver<OutboundFrame>,
    mut normal_rx: mpsc::Receiver<OutboundFrame>,
    mut replay_rx: mpsc::Receiver<OutboundFrame>,
    state: Arc<PeerState>,
    mut disconnect_rx: watch::Receiver<Option<DisconnectReason>>,
) where
    W: AsyncWrite + Unpin,
{
    loop {
        let next = next_outbound(&mut control_rx, &mut normal_rx, &mut replay_rx);
        tokio::pin!(next);
        let frame = tokio::select! {
            biased;
            changed = disconnect_rx.changed() => {
                if changed.is_err() || disconnect_rx.borrow().is_some() {
                    return;
                }
                continue;
            }
            frame = &mut next => match frame {
                Some(frame) => frame,
                None => {
                    state.disconnect(DisconnectReason::LocalShutdown);
                    return;
                }
            }
        };

        let write = write_frame(&mut writer, &frame.bytes);
        tokio::pin!(write);
        let result = tokio::select! {
            biased;
            changed = disconnect_rx.changed() => {
                if changed.is_err() || disconnect_rx.borrow().is_some() {
                    return;
                }
                continue;
            }
            result = timeout(write_timeout, &mut write) => result,
        };
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                state.disconnect(DisconnectReason::WriteFailed(error.to_string()));
                return;
            }
            Err(_) => {
                state.disconnect(DisconnectReason::WriteFailed(
                    "write deadline exceeded".to_string(),
                ));
                return;
            }
        }
    }
}

async fn next_outbound(
    control_rx: &mut mpsc::Receiver<OutboundFrame>,
    normal_rx: &mut mpsc::Receiver<OutboundFrame>,
    replay_rx: &mut mpsc::Receiver<OutboundFrame>,
) -> Option<OutboundFrame> {
    loop {
        match control_rx.try_recv() {
            Ok(frame) => return Some(frame),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => {}
        }
        match normal_rx.try_recv() {
            Ok(frame) => return Some(frame),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => {}
        }
        match replay_rx.try_recv() {
            Ok(frame) => return Some(frame),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => {}
        }
        if control_rx.is_closed() && normal_rx.is_closed() && replay_rx.is_closed() {
            return None;
        }
        tokio::select! {
            biased;
            frame = control_rx.recv(), if !control_rx.is_closed() => {
                if frame.is_some() {
                    return frame;
                }
            }
            frame = normal_rx.recv(), if !normal_rx.is_closed() => {
                if frame.is_some() {
                    return frame;
                }
            }
            frame = replay_rx.recv(), if !replay_rx.is_closed() => {
                if frame.is_some() {
                    return frame;
                }
            }
        }
    }
}

fn validate_config(config: &PeerConfig) -> Result<(), RpcError> {
    for (name, value) in [
        ("control_queue_capacity", config.control_queue_capacity),
        ("normal_queue_capacity", config.normal_queue_capacity),
        ("replay_queue_capacity", config.replay_queue_capacity),
        ("inbound_queue_capacity", config.inbound_queue_capacity),
        ("inbound_queue_bytes", config.inbound_queue_bytes),
        ("control_queue_bytes", config.control_queue_bytes),
        ("bulk_queue_bytes", config.bulk_queue_bytes),
        ("max_in_flight_requests", config.max_in_flight_requests),
    ] {
        if value == 0 {
            return Err(RpcError::InvalidConfiguration(format!(
                "{name} must be greater than zero"
            )));
        }
    }
    if config.control_queue_bytes > u32::MAX as usize
        || config.bulk_queue_bytes > u32::MAX as usize
        || config.inbound_queue_bytes > u32::MAX as usize
    {
        return Err(RpcError::InvalidConfiguration(
            "queue byte budgets must fit in u32 semaphore permits".to_string(),
        ));
    }
    if config.request_timeout.is_zero() || config.write_timeout.is_zero() {
        return Err(RpcError::InvalidConfiguration(
            "request and write timeouts must be greater than zero".to_string(),
        ));
    }
    if config
        .read_idle_timeout
        .is_some_and(|duration| duration.is_zero())
    {
        return Err(RpcError::InvalidConfiguration(
            "read idle timeout must be greater than zero when set".to_string(),
        ));
    }
    Ok(())
}

fn next_peer_prefix() -> String {
    static NEXT_PEER_INSTANCE: AtomicU64 = AtomicU64::new(1);
    let instance = NEXT_PEER_INSTANCE.fetch_add(1, Ordering::Relaxed);
    let started_at_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("peer-{}-{started_at_nanos}-{instance}", std::process::id())
}
