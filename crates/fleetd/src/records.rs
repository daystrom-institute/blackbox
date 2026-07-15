use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bro_capabilities::{
    RecordIngestCapability, RecordIngestReceipt, RecordIngestRequest, transcript_record_targets,
};
use bro_core::BroError;
use tokio_util::sync::CancellationToken;

use crate::authority_actor::AuthorityActor;
use crate::roster::RosterHub;
use crate::{FleetdError, FleetdResult};

const RECORD_BATCH_SIZE: usize = 128;

#[derive(Debug, Clone)]
pub(crate) struct BlackboxRecordHttpClient {
    client: reqwest::Client,
    base_url: String,
    service_token: Arc<bro_rpc::ServiceToken>,
}

impl BlackboxRecordHttpClient {
    pub(crate) fn new(
        base_url: impl Into<String>,
        timeout: Duration,
        service_token: Arc<bro_rpc::ServiceToken>,
    ) -> FleetdResult<Self> {
        if timeout.is_zero() {
            return Err(FleetdError::InvalidConfiguration(
                "record ingest timeout must be nonzero".into(),
            ));
        }
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(timeout)
            .build()
            .map_err(|error| FleetdError::CapabilityUnavailable(error.to_string()))?;
        Ok(Self {
            client,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            service_token,
        })
    }
}

#[async_trait]
impl RecordIngestCapability for BlackboxRecordHttpClient {
    async fn ingest_records(
        &self,
        request: RecordIngestRequest,
    ) -> Result<RecordIngestReceipt, BroError> {
        let response = self
            .client
            .post(format!("{}/internal/records", self.base_url))
            .header(
                reqwest::header::AUTHORIZATION,
                self.service_token.authorization_header(),
            )
            .json(&request)
            .send()
            .await
            .map_err(|error| BroError::new("records.unavailable", error.to_string()))?;
        let status = response.status();
        if status.is_success() {
            return response
                .json()
                .await
                .map_err(|error| BroError::new("records.decode", error.to_string()));
        }
        let detail = response.text().await.unwrap_or_default();
        Err(BroError::new(
            "records.http",
            format!("blackboxd returned {status}: {detail}"),
        ))
    }
}

#[derive(Clone)]
pub(crate) struct RecordPump {
    authority: AuthorityActor,
    records: Arc<dyn RecordIngestCapability>,
    roster: RosterHub,
    interval: Duration,
}

impl RecordPump {
    pub(crate) fn new(
        authority: AuthorityActor,
        records: Arc<dyn RecordIngestCapability>,
        roster: RosterHub,
        interval: Duration,
    ) -> Self {
        Self {
            authority,
            records,
            roster,
            interval,
        }
    }

    pub(crate) async fn run(self, shutdown: CancellationToken) -> FleetdResult<()> {
        let mut ticker = tokio::time::interval(self.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return Ok(()),
                _ = ticker.tick() => {
                    loop {
                        match self.drain_once().await {
                            Ok(RECORD_BATCH_SIZE) => continue,
                            Ok(_) => break,
                            Err(error) => {
                                // Corpus availability is never on the worker event
                                // acknowledgement path. The durable outbox remains
                                // authoritative and the next tick retries it.
                                tracing::warn!(error = %error, "fleet record ingest deferred");
                                break;
                            }
                        }
                    }
                }
            }
        }
    }

    pub(crate) async fn drain_once(&self) -> FleetdResult<usize> {
        let records = self
            .authority
            .call(|authority| {
                authority
                    .pending_records(RECORD_BATCH_SIZE)
                    .map_err(Into::into)
            })
            .await?;
        let Some(last) = records.last() else {
            return Ok(0);
        };
        let through_cursor = last.cursor.parse::<u64>().map_err(|error| {
            FleetdError::Conflict(format!("fleet record cursor is malformed: {error}"))
        })?;
        let expected_transcripts = transcript_record_targets(&records).map_err(|error| {
            FleetdError::Conflict(format!(
                "fleet transcript record is malformed: {}: {}",
                error.code, error.message
            ))
        })?;
        let expected = records.len();
        let receipt = self
            .records
            .ingest_records(RecordIngestRequest { records })
            .await
            .map_err(|error| {
                FleetdError::CapabilityUnavailable(format!("{}: {}", error.code, error.message))
            })?;
        let covered = receipt.accepted.saturating_add(receipt.deduplicated);
        if covered != expected {
            return Err(FleetdError::Conflict(format!(
                "record ingest receipt covered {covered} of {expected} records"
            )));
        }
        let acknowledged = receipt
            .producer_cursors
            .get("fleetd")
            .and_then(|cursor| cursor.parse::<u64>().ok())
            .ok_or_else(|| {
                FleetdError::Conflict(
                    "record ingest receipt omitted the fleetd producer cursor".into(),
                )
            })?;
        if acknowledged < through_cursor {
            return Err(FleetdError::Conflict(format!(
                "record ingest cursor {acknowledged} trails submitted cursor {through_cursor}"
            )));
        }
        for (stream, target) in expected_transcripts {
            let indexed = receipt
                .transcript_cursors
                .get(&stream)
                .copied()
                .unwrap_or(0);
            if indexed < target.through_event_seq {
                return Err(FleetdError::Conflict(format!(
                    "record ingest receipt indexed transcript {stream} through {indexed}; expected at least {}",
                    target.through_event_seq
                )));
            }
        }
        let removed = self
            .authority
            .call(move |authority| {
                authority
                    .acknowledge_records(through_cursor)
                    .map_err(Into::into)
            })
            .await?;
        let snapshot = self
            .authority
            .call(|authority| authority.snapshot().map_err(Into::into))
            .await?;
        self.roster.publish_authority(&snapshot);
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use bro_capabilities::{
        ExecutionCodeMode, ExecutionDispatchContext, ExecutionKind, ExecutionRequest,
        ExecutionServiceTier, ExecutionToolPolicy, RecordEnvelope, WorkingSetIntent,
    };
    use bro_core::{OperationId, Provider};
    use fleet_core::{SessionEventDescriptor, TaskEventObservation};

    use super::*;

    #[derive(Default)]
    struct FakeRecords {
        unavailable: AtomicBool,
        fail_after_store: AtomicBool,
        seen: Mutex<HashMap<String, RecordEnvelope>>,
        producer_cursor: Mutex<u64>,
        transcript_cursors: Mutex<BTreeMap<String, u64>>,
        transcript_receipt: Mutex<TranscriptReceipt>,
    }

    #[derive(Default)]
    enum TranscriptReceipt {
        #[default]
        Confirm,
        Omit,
        Through(u64),
    }

    #[async_trait]
    impl RecordIngestCapability for FakeRecords {
        async fn ingest_records(
            &self,
            request: RecordIngestRequest,
        ) -> Result<RecordIngestReceipt, BroError> {
            if self.unavailable.load(Ordering::SeqCst) {
                return Err(BroError::new("records.unavailable", "simulated outage"));
            }
            let transcript_targets = transcript_record_targets(&request.records)?;
            let mut seen = self.seen.lock().unwrap();
            let mut accepted = 0;
            let mut deduplicated = 0;
            let mut producer_cursor = self.producer_cursor.lock().unwrap();
            for record in request.records {
                let cursor = record.cursor.parse::<u64>().unwrap();
                match seen.get(&record.record_id) {
                    Some(existing) if existing == &record => deduplicated += 1,
                    Some(_) => return Err(BroError::new("records.conflict", "changed record")),
                    None => {
                        seen.insert(record.record_id.clone(), record);
                        accepted += 1;
                    }
                }
                *producer_cursor = (*producer_cursor).max(cursor);
            }
            if self.fail_after_store.swap(false, Ordering::SeqCst) {
                return Err(BroError::new(
                    "records.ambiguous",
                    "stored records before simulated disconnect",
                ));
            }
            let mut transcript_cursors = self.transcript_cursors.lock().unwrap();
            match *self.transcript_receipt.lock().unwrap() {
                TranscriptReceipt::Confirm => {
                    for (stream, target) in transcript_targets {
                        let cursor = transcript_cursors.entry(stream).or_default();
                        *cursor = (*cursor).max(target.through_event_seq);
                    }
                }
                TranscriptReceipt::Omit => transcript_cursors.clear(),
                TranscriptReceipt::Through(through) => {
                    transcript_cursors.clear();
                    transcript_cursors.extend(
                        transcript_targets
                            .into_keys()
                            .map(|stream| (stream, through)),
                    );
                }
            }
            Ok(RecordIngestReceipt {
                accepted,
                deduplicated,
                producer_cursors: BTreeMap::from([("fleetd".into(), producer_cursor.to_string())]),
                transcript_cursors: transcript_cursors.clone(),
            })
        }
    }

    fn request(key: &str) -> ExecutionRequest {
        ExecutionRequest {
            operation_id: OperationId::new(format!("operation-{key}")),
            idempotency_key: key.into(),
            kind: ExecutionKind::Fresh {
                prompt: "run the test".into(),
            },
            provider: Provider::Glm,
            model: "glm-test".into(),
            effort: None,
            service_tier: ExecutionServiceTier::Default,
            code_mode: Some(ExecutionCodeMode::Only),
            dispatch_context: Some(ExecutionDispatchContext::default()),
            working_set: WorkingSetIntent::Scratch,
            shell_env: BTreeMap::new(),
            tool_policy: ExecutionToolPolicy::default(),
            system_prompt: None,
            output_schema: None,
            labels: BTreeMap::new(),
        }
    }

    async fn actor(root: &std::path::Path) -> AuthorityActor {
        AuthorityActor::open(root.join("state.json"), root.join("lock"), 1)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn http_client_posts_the_typed_internal_records_contract() {
        let app = axum::Router::new().route(
            "/internal/records",
            axum::routing::post(
                |headers: axum::http::HeaderMap,
                 axum::Json(request): axum::Json<RecordIngestRequest>| async move {
                    let authorization = headers
                        .get(axum::http::header::AUTHORIZATION)
                        .and_then(|value| value.to_str().ok())
                        .unwrap();
                    assert!(authorization.starts_with("Bearer "));
                    assert_eq!(authorization.len(), 71);
                    assert_eq!(request.records.len(), 1);
                    assert_eq!(request.records[0].record_id, "fleetd:test:1");
                    axum::Json(RecordIngestReceipt {
                        accepted: 1,
                        deduplicated: 0,
                        producer_cursors: BTreeMap::from([("fleetd".into(), "1".into())]),
                        transcript_cursors: BTreeMap::new(),
                    })
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let token = Arc::new(bro_rpc::ServiceToken::parse("a".repeat(64)).unwrap());
        let client = BlackboxRecordHttpClient::new(
            format!("http://{address}"),
            Duration::from_secs(1),
            token,
        )
        .unwrap();
        let receipt = client
            .ingest_records(RecordIngestRequest {
                records: vec![RecordEnvelope {
                    record_id: "fleetd:test:1".into(),
                    producer: "fleetd".into(),
                    cursor: "1".into(),
                    kind: "attempt.accepted".into(),
                    occurred_at: None,
                    subject: Some("attempt-1".into()),
                    attributes: BTreeMap::new(),
                    payload: serde_json::json!({"state": "accepted"}),
                }],
            })
            .await
            .unwrap();
        assert_eq!((receipt.accepted, receipt.deduplicated), (1, 0));
        server.abort();
    }

    #[tokio::test]
    async fn outage_recovery_drains_without_losing_the_durable_record() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let authority = actor(&root).await;
        authority
            .call(|authority| {
                authority
                    .admit_execution(request("outage"), 2)
                    .map(|_| ())
                    .map_err(Into::into)
            })
            .await
            .unwrap();
        let records = Arc::new(FakeRecords::default());
        records.unavailable.store(true, Ordering::SeqCst);
        let pump = RecordPump::new(
            authority.clone(),
            records.clone(),
            RosterHub::empty(),
            Duration::from_millis(10),
        );
        assert!(pump.drain_once().await.is_err());
        assert_eq!(
            authority
                .call(|authority| authority.pending_records(10).map_err(Into::into))
                .await
                .unwrap()
                .len(),
            1
        );

        records.unavailable.store(false, Ordering::SeqCst);
        assert_eq!(pump.drain_once().await.unwrap(), 1);
        let snapshot = authority
            .call(|authority| authority.snapshot().map_err(Into::into))
            .await
            .unwrap();
        assert!(snapshot.record_outbox.is_empty());
        assert_eq!(snapshot.acknowledged_record_cursor, 1);
        authority.shutdown().await;
    }

    #[tokio::test]
    async fn ambiguous_ingest_is_deduplicated_after_fleet_restart() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let records = Arc::new(FakeRecords::default());
        let authority = actor(&root).await;
        authority
            .call(|authority| {
                authority
                    .admit_execution(request("restart"), 2)
                    .map(|_| ())
                    .map_err(Into::into)
            })
            .await
            .unwrap();
        records.fail_after_store.store(true, Ordering::SeqCst);
        let pump = RecordPump::new(
            authority.clone(),
            records.clone(),
            RosterHub::empty(),
            Duration::from_millis(10),
        );
        assert!(pump.drain_once().await.is_err());
        drop(pump);
        authority.shutdown().await;

        let reopened = actor(&root).await;
        let pump = RecordPump::new(
            reopened.clone(),
            records.clone(),
            RosterHub::empty(),
            Duration::from_millis(10),
        );
        assert_eq!(pump.drain_once().await.unwrap(), 1);
        assert_eq!(records.seen.lock().unwrap().len(), 1);
        let snapshot = reopened
            .call(|authority| authority.snapshot().map_err(Into::into))
            .await
            .unwrap();
        assert!(snapshot.record_outbox.is_empty());
        assert_eq!(snapshot.acknowledged_record_cursor, 1);
        reopened.shutdown().await;

        let verified = actor(&root).await;
        let snapshot = verified
            .call(|authority| authority.snapshot().map_err(Into::into))
            .await
            .unwrap();
        assert!(snapshot.record_outbox.is_empty());
        assert_eq!(snapshot.acknowledged_record_cursor, 1);
        verified.shutdown().await;
    }

    #[tokio::test]
    async fn transcript_receipt_must_confirm_indexing_before_outbox_acknowledgement() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let authority = actor(&root).await;
        let accepted = authority
            .call(|authority| {
                authority
                    .admit_execution(request("cursor-gate"), 2)
                    .map_err(Into::into)
            })
            .await
            .unwrap();
        let task_id = accepted.task_id.clone();
        let provisioned = authority
            .call(move |authority| authority.provision_worker(&task_id, 3).map_err(Into::into))
            .await
            .unwrap();
        let task_id = accepted.task_id.clone();
        authority
            .call(move |authority| {
                authority
                    .record_transcript_path(&task_id, "/tmp/fleetd-cursor-test/events.jsonl", 3)
                    .map_err(Into::into)
            })
            .await
            .unwrap();
        let worker_id = provisioned.worker_id.clone();
        authority
            .call(move |authority| {
                authority
                    .observe_event_observation(
                        &worker_id,
                        0,
                        1,
                        4,
                        TaskEventObservation {
                            record: Some(SessionEventDescriptor::from_event(
                                &serde_json::json!({"type": "assistant", "text": "indexed"}),
                            )),
                            ..TaskEventObservation::default()
                        },
                    )
                    .map(|_| ())
                    .map_err(Into::into)
            })
            .await
            .unwrap();

        let before = authority
            .call(|authority| authority.snapshot().map_err(Into::into))
            .await
            .unwrap();
        let outbox_len = before.record_outbox.len();
        assert!(outbox_len >= 2);
        assert_eq!(before.acknowledged_record_cursor, 0);
        assert_eq!(
            before
                .workers
                .get(&provisioned.worker_id)
                .unwrap()
                .last_indexed_event_seq,
            0
        );

        let records = Arc::new(FakeRecords::default());
        let pump = RecordPump::new(
            authority.clone(),
            records.clone(),
            RosterHub::empty(),
            Duration::from_millis(10),
        );

        *records.transcript_receipt.lock().unwrap() = TranscriptReceipt::Omit;
        assert!(pump.drain_once().await.is_err());
        let missing = authority
            .call(|authority| authority.snapshot().map_err(Into::into))
            .await
            .unwrap();
        assert_eq!(missing.record_outbox.len(), outbox_len);
        assert_eq!(missing.acknowledged_record_cursor, 0);
        assert_eq!(
            missing
                .workers
                .get(&provisioned.worker_id)
                .unwrap()
                .last_indexed_event_seq,
            0
        );

        *records.transcript_receipt.lock().unwrap() = TranscriptReceipt::Through(0);
        assert!(pump.drain_once().await.is_err());
        let lagging = authority
            .call(|authority| authority.snapshot().map_err(Into::into))
            .await
            .unwrap();
        assert_eq!(lagging.record_outbox.len(), outbox_len);
        assert_eq!(lagging.acknowledged_record_cursor, 0);
        assert_eq!(
            lagging
                .workers
                .get(&provisioned.worker_id)
                .unwrap()
                .last_indexed_event_seq,
            0
        );

        *records.transcript_receipt.lock().unwrap() = TranscriptReceipt::Through(1);
        assert_eq!(pump.drain_once().await.unwrap(), outbox_len);
        let confirmed = authority
            .call(|authority| authority.snapshot().map_err(Into::into))
            .await
            .unwrap();
        assert!(confirmed.record_outbox.is_empty());
        assert!(confirmed.acknowledged_record_cursor > 0);
        assert_eq!(
            confirmed
                .workers
                .get(&provisioned.worker_id)
                .unwrap()
                .last_indexed_event_seq,
            1
        );
        authority.shutdown().await;
    }
}
