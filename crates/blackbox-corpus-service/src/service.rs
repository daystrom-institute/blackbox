use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use bbox_corpus_index::index::TranscriptIndex;
use bro_capabilities::{
    CapabilityResult, CorpusCapability, CorpusHit, CorpusLookup, RecordIngestCapability,
    RecordIngestReceipt, RecordIngestRequest, TranscriptRecordTarget, transcript_record_targets,
};
use bro_protocol::{
    CapabilityAuthorization, CapabilityError, CapabilityErrorCode, CapabilityRequest,
    CapabilityResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tantivy::{IndexWriter, TantivyDocument, Term};
use tokio_util::sync::CancellationToken;

use crate::records::record_search_text;
use crate::{CorpusServicePaths, RecordStore};

const MAX_HTTP_BODY_BYTES: usize = 2 * 1024 * 1024;

/// Corpus authority with no dependency on execution or operational code.
pub struct CorpusService {
    index: TranscriptIndex,
    records: RecordStore,
    record_writer: parking_lot::Mutex<IndexWriter>,
    paths: CorpusServicePaths,
    service_token: Arc<bro_rpc::ServiceToken>,
}

impl CorpusService {
    // Service construction is a synchronous startup boundary: private storage,
    // the Tantivy handle, and initial reconciliation finish before publication.
    #[allow(clippy::disallowed_methods)]
    pub fn open(paths: CorpusServicePaths) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&paths.record_root)?;
        let service_token = Arc::new(
            bro_rpc::ServiceToken::load_or_create(&paths.service_token_path)
                .map_err(|error| anyhow::anyhow!("loading corpus service token: {error}"))?,
        );
        let mut index = TranscriptIndex::open_or_create(
            &paths.index_path,
            Vec::new(),
            None,
            paths.record_root.join("projects.json"),
            paths.record_root.join("blackbox-knowledge.json"),
            paths.record_root.join("blackbox-threads.json"),
            paths.record_root.join("blackbox-roadmap.json"),
        )?;
        index.add_harness_sessions_dir(paths.record_root.join("record-ingest/transcript-archive"));
        let records = RecordStore::open(&paths.record_root)?;
        let record_writer = index.index_handle().writer(50_000_000)?;
        let service = Self {
            index,
            records,
            record_writer: parking_lot::Mutex::new(record_writer),
            paths,
            service_token,
        };
        service.reconcile_record_index()?;
        Ok(service)
    }

    pub fn paths(&self) -> &CorpusServicePaths {
        &self.paths
    }

    pub fn authorization_header(&self) -> axum::http::HeaderValue {
        self.service_token.authorization_header()
    }

    fn search(&self, lookup: CorpusLookup) -> CapabilityResult<Vec<CorpusHit>> {
        lookup.validate()?;
        let indexed = self
            .index
            .hybrid_bm25_hits(&lookup.query, lookup.limit, None)
            .map_err(|error| bro_core::BroError::new("corpus.search_failed", error.to_string()))?;
        let mut seen = BTreeSet::new();
        let mut hits = indexed
            .into_iter()
            .filter_map(|hit| {
                seen.insert(hit.entity_id.clone()).then(|| CorpusHit {
                    id: hit.entity_id,
                    text: match hit.title {
                        Some(title) => format!("{title}\n{}", hit.excerpt),
                        None => hit.excerpt,
                    },
                })
            })
            .collect::<Vec<_>>();
        if hits.len() < lookup.limit {
            for hit in self
                .records
                .search(&lookup.query, lookup.limit.saturating_sub(hits.len()))
            {
                if seen.insert(hit.id.clone()) {
                    hits.push(hit);
                }
            }
        }
        hits.truncate(lookup.limit);
        Ok(hits)
    }

    fn ingest_and_index(
        &self,
        request: RecordIngestRequest,
    ) -> CapabilityResult<RecordIngestReceipt> {
        let records = request.records.clone();
        let targets = transcript_record_targets(&records)?;
        let mut receipt = self.records.ingest(request)?;
        // Inline transcript increments are archived by the store and indexed
        // from their archive files on the ordinary reindex pass; projecting
        // them as operational records would index base64 payload blobs.
        let records: Vec<_> = records
            .into_iter()
            .filter(|record| record.kind != bro_capabilities::TRANSCRIPT_INCREMENT_KIND)
            .collect();
        let pending = targets
            .iter()
            .filter(|(stream, target)| {
                receipt
                    .transcript_cursors
                    .get(stream.as_str())
                    .is_none_or(|cursor| *cursor < target.through_event_seq)
            })
            .map(|(stream, target)| (stream.clone(), target.clone()))
            .collect::<BTreeMap<_, _>>();
        let archived = self
            .records
            .archive_transcript_targets(&pending, &self.paths.transcript_roots)?;
        self.upsert_record_documents(&records, &archived)
            .map_err(|error| {
                bro_core::BroError::new("record_ingest.index_failed", error.to_string())
            })?;
        if !targets.is_empty() {
            let acknowledged = targets
                .into_iter()
                .map(|(stream, target)| (stream, target.through_event_seq))
                .collect();
            receipt.transcript_cursors = self.records.acknowledge_transcripts(&acknowledged)?;
        }
        Ok(receipt)
    }

    fn reconcile_record_index(&self) -> anyhow::Result<()> {
        let records = self.records.all_records();
        let targets = transcript_record_targets(&records)
            .map_err(|error| anyhow::anyhow!("{}: {}", error.code, error.message))?;
        let archived = self
            .records
            .ensure_archived_transcript_targets(&targets, &self.paths.transcript_roots)
            .map_err(|error| anyhow::anyhow!("{}: {}", error.code, error.message))?;
        self.upsert_record_documents(&records, &archived)
    }

    // This acknowledged commit boundary is serialized by record_writer. It
    // returns only after Tantivy commit and reader reload, before receipt state
    // can advance, so producer cursors never outrun searchable evidence.
    #[allow(clippy::disallowed_methods)]
    fn upsert_record_documents(
        &self,
        records: &[bro_capabilities::RecordEnvelope],
        transcript_targets: &BTreeMap<String, TranscriptRecordTarget>,
    ) -> anyhow::Result<()> {
        if records.is_empty() && transcript_targets.is_empty() {
            return Ok(());
        }
        let fields = self.index.field_handles();
        let archive_root = self.records.transcript_archive_root();
        let projections = transcript_targets
            .values()
            .map(|target| {
                bbox_corpus_index::transcripts::harness_sessions::project_fleet_event_log(
                    std::path::Path::new(&target.path),
                    &target.session_id,
                    target.through_event_seq,
                    std::slice::from_ref(&archive_root),
                    fields,
                )
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let mut writer = self.record_writer.lock();
        for projection in projections {
            writer.delete_term(Term::from_field_text(
                fields.file_path,
                &projection.canonical_path,
            ));
            for document in projection.documents {
                writer.add_document(document)?;
            }
        }
        for record in records {
            writer.delete_term(Term::from_field_text(fields.entity_id, &record.record_id));
            writer.add_document(record_document(record, fields))?;
        }
        writer.commit()?;
        self.index.reader_reload_for_test();
        *self.index.stats_cache_handle().lock() = None;
        Ok(())
    }
}

fn record_document(
    record: &bro_capabilities::RecordEnvelope,
    fields: bbox_corpus_index::index::FieldHandles,
) -> TantivyDocument {
    let record_path = format!("record://{}/{}", record.producer, record.cursor);
    let mut document = TantivyDocument::new();
    document.add_text(fields.doc_type, "operational_record");
    document.add_text(fields.parser_version, "record-v1");
    document.add_text(fields.content, record_search_text(record));
    document.add_text(
        fields.session_id,
        record.subject.as_deref().unwrap_or(&record.record_id),
    );
    document.add_text(fields.account, &record.producer);
    document.add_text(
        fields.project,
        record.subject.as_deref().unwrap_or(&record.producer),
    );
    document.add_text(fields.role, "record");
    document.add_text(fields.file_path, &record_path);
    document.add_text(fields.path_tokens, &record_path);
    document.add_u64(
        fields.byte_offset,
        record.cursor.parse::<u64>().unwrap_or_default(),
    );
    document.add_u64(fields.is_subagent, 0);
    document.add_text(fields.chunk_kind, &record.kind);
    document.add_text(fields.entity_id, &record.record_id);
    if let Some(timestamp) = &record.occurred_at {
        document.add_text(fields.timestamp, timestamp);
    }
    if let Some(task_id) = record.attributes.get("task_id") {
        document.add_text(fields.task_id, task_id);
    }
    document
}

#[async_trait]
impl CorpusCapability for CorpusService {
    async fn search_corpus(&self, lookup: CorpusLookup) -> CapabilityResult<Vec<CorpusHit>> {
        self.search(lookup)
    }
}

#[async_trait]
impl RecordIngestCapability for CorpusService {
    async fn ingest_records(
        &self,
        request: RecordIngestRequest,
    ) -> CapabilityResult<RecordIngestReceipt> {
        self.ingest_and_index(request)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutedCapabilityRequest {
    pub worker_id: bro_core::WorkerId,
    pub session_id: bro_core::SessionId,
    #[serde(default)]
    pub authorization: Option<CapabilityAuthorization>,
    pub request: CapabilityRequest,
}

#[derive(Debug, Serialize)]
struct HealthResponse<'a> {
    service: &'a str,
    version: &'a str,
    build_id: &'a str,
    ready: bool,
}

pub fn router(service: Arc<CorpusService>) -> Router {
    Router::new()
        .route("/internal/capability", post(internal_capability))
        .route("/internal/records", post(internal_records))
        .route("/healthz", get(health))
        .route("/readyz", get(health))
        .layer(DefaultBodyLimit::max(MAX_HTTP_BODY_BYTES))
        .route_layer(middleware::from_fn_with_state(
            service.clone(),
            require_service_auth,
        ))
        .with_state(service)
}

async fn require_service_auth(
    State(service): State<Arc<CorpusService>>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    if matches!(request.uri().path(), "/healthz" | "/readyz")
        || service.service_token.authorizes(request.headers())
    {
        next.run(request).await
    } else {
        (StatusCode::UNAUTHORIZED, "bearer token required").into_response()
    }
}

pub async fn serve(
    listener: tokio::net::TcpListener,
    service: Arc<CorpusService>,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    axum::serve(listener, router(service))
        .with_graceful_shutdown(shutdown.cancelled_owned())
        .await?;
    Ok(())
}

async fn health() -> Json<HealthResponse<'static>> {
    Json(HealthResponse {
        service: "blackbox-corpus-service",
        version: env!("CARGO_PKG_VERSION"),
        build_id: env!("BLACKBOX_CORPUS_BUILD_ID"),
        ready: true,
    })
}

async fn internal_capability(
    State(service): State<Arc<CorpusService>>,
    Json(routed): Json<RoutedCapabilityRequest>,
) -> Json<CapabilityResponse> {
    let request = routed.request;
    let call_id = request.call_id.clone();
    if routed.worker_id.as_str().is_empty() || routed.session_id.as_str().is_empty() {
        return Json(capability_error(
            call_id,
            CapabilityErrorCode::Unauthorized,
            "worker and session identity are required",
            false,
        ));
    }
    if request.call_id.trim().is_empty() {
        return Json(capability_error(
            call_id,
            CapabilityErrorCode::InvalidRequest,
            "call identity is required",
            false,
        ));
    }
    if !routed.authorization.as_ref().is_some_and(|authorization| {
        authorization.authorizes(
            &routed.worker_id,
            &routed.session_id,
            &request.capability,
            &request.operation,
        )
    }) {
        return Json(capability_error(
            call_id,
            CapabilityErrorCode::Unauthorized,
            "fleet authorization does not grant this exact corpus operation",
            false,
        ));
    }
    if request
        .deadline_unix_ms
        .is_some_and(|deadline| deadline <= unix_time_ms())
    {
        return Json(capability_error(
            call_id,
            CapabilityErrorCode::DeadlineExceeded,
            "corpus capability deadline elapsed before admission",
            false,
        ));
    }
    if request.capability != "corpus" || request.operation != "search_corpus" {
        return Json(capability_error(
            call_id,
            CapabilityErrorCode::Unauthorized,
            "blackboxd only serves corpus search on this endpoint",
            false,
        ));
    }
    let lookup = match serde_json::from_value::<CorpusLookup>(request.bounded_payload) {
        Ok(lookup) => lookup,
        Err(error) => {
            return Json(capability_error(
                call_id,
                CapabilityErrorCode::InvalidRequest,
                error.to_string(),
                false,
            ));
        }
    };
    let hits = match service.search(lookup) {
        Ok(hits) => hits,
        Err(error) => {
            let code = if error.code.starts_with("corpus.invalid_") {
                CapabilityErrorCode::InvalidRequest
            } else {
                CapabilityErrorCode::Internal
            };
            return Json(capability_error(
                call_id,
                code,
                error.message,
                code == CapabilityErrorCode::Internal,
            ));
        }
    };
    match serde_json::to_value(hits) {
        Ok(value) => Json(CapabilityResponse::success(call_id, value)),
        Err(error) => Json(capability_error(
            call_id,
            CapabilityErrorCode::Internal,
            error.to_string(),
            false,
        )),
    }
}

async fn internal_records(
    State(service): State<Arc<CorpusService>>,
    Json(request): Json<RecordIngestRequest>,
) -> impl IntoResponse {
    match service.ingest_and_index(request) {
        Ok(receipt) => (StatusCode::OK, Json(json!(receipt))).into_response(),
        Err(error) => {
            let (status, retryable) = match error.code.as_str() {
                "record_ingest.idempotency_conflict" => (StatusCode::CONFLICT, false),
                "record_ingest.persistence_failed" | "record_ingest.index_failed" => {
                    (StatusCode::SERVICE_UNAVAILABLE, true)
                }
                _ => (StatusCode::BAD_REQUEST, false),
            };
            (
                status,
                Json(json!({
                    "error": {
                        "code": error.code,
                        "message": error.message,
                        "retryable": retryable
                    }
                })),
            )
                .into_response()
        }
    }
}

fn capability_error(
    call_id: String,
    code: CapabilityErrorCode,
    message: impl Into<String>,
    retryable: bool,
) -> CapabilityResponse {
    CapabilityResponse::error(
        call_id.clone(),
        CapabilityError {
            code,
            message: message.into(),
            retryable,
            details: None,
        },
    )
    .unwrap_or_else(|_| CapabilityResponse::success(call_id, Value::Null))
}

pub fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
// Service fixtures intentionally build and mutate isolated corpus and fleet
// transcript trees to verify archive and restart durability.
#[allow(clippy::disallowed_methods)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use axum::body::Body;
    use axum::http::Request;
    use bro_capabilities::{RecordEnvelope, RecordIngestRequest};
    use tower::ServiceExt as _;

    use super::*;

    fn test_service() -> (tempfile::TempDir, Arc<CorpusService>) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let transcript_root = root.join("fleet-workers");
        std::fs::create_dir_all(&transcript_root).unwrap();
        let paths = CorpusServicePaths::new(root.join("index"), root.join("records"))
            .with_transcript_roots(vec![transcript_root]);
        let service = CorpusService::open(paths).unwrap();
        (dir, Arc::new(service))
    }

    fn record() -> RecordEnvelope {
        RecordEnvelope {
            record_id: "record-1".into(),
            producer: "blackopsd".into(),
            cursor: "1".into(),
            kind: "operation.completed".into(),
            occurred_at: None,
            subject: Some("operation-1".into()),
            attributes: BTreeMap::new(),
            payload: json!({"answer": "needle"}),
        }
    }

    fn capability_authorization(
        worker_id: &str,
        session_id: &str,
        operation: &str,
    ) -> CapabilityAuthorization {
        CapabilityAuthorization {
            worker_id: bro_core::WorkerId::new(worker_id),
            session_id: bro_core::SessionId::new(session_id),
            task_id: bro_core::TaskId::new("task-test"),
            attempt_id: bro_core::AttemptId::new("attempt-test"),
            session_attempt_generation: 1,
            policy: bro_protocol::PolicyIdentity {
                version: 1,
                digest: "sha256:test-policy".into(),
            },
            capability_policy: bro_protocol::SessionCapabilityPolicy {
                allowed_operations: BTreeMap::from([(
                    "corpus".into(),
                    BTreeSet::from([operation.into()]),
                )]),
                allowed_atom_refs: BTreeSet::new(),
            },
        }
    }

    #[tokio::test]
    async fn typed_traits_share_the_same_durable_projection() {
        let (_dir, service) = test_service();
        service
            .ingest_records(RecordIngestRequest {
                records: vec![record()],
            })
            .await
            .unwrap();
        let hits = service
            .search_corpus(CorpusLookup {
                query: "needle".into(),
                limit: 10,
            })
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "record-1");
        let indexed = service
            .index
            .hybrid_bm25_hits("needle", 10, Some("operational_record"))
            .unwrap();
        assert_eq!(indexed.len(), 1);
        assert_eq!(indexed[0].entity_id, "record-1");
    }

    #[tokio::test]
    async fn fleet_receipt_advances_only_after_transcript_content_is_indexed() {
        let (_dir, service) = test_service();
        let paths = service.paths().clone();
        let worker_root = &paths.transcript_roots[0];
        let worker_dir = worker_root.join("worker-1");
        std::fs::create_dir_all(&worker_dir).unwrap();
        let event_log = worker_dir.join("events.jsonl");
        let lines = [
            json!({
                "ts": "2026-07-14T12:00:00Z",
                "event_seq": 1,
                "event": {
                    "type": "harness_milestone",
                    "milestone": "session_start",
                    "session_id": "session-1",
                    "provider": "glm",
                    "transport": "anthropic",
                    "model": "glm-test",
                    "cwd": "/repo/test"
                }
            }),
            json!({
                "ts": "2026-07-14T12:00:01Z",
                "event_seq": 2,
                "event": {
                    "type": "user",
                    "session_id": "session-1",
                    "message": {
                        "role": "user",
                        "content": [{"type": "text", "text": "fleet-transcript-needle"}]
                    }
                }
            }),
        ];
        let body = lines
            .iter()
            .map(|line| format!("{line}\n"))
            .collect::<String>();
        std::fs::write(&event_log, body).unwrap();
        let transcript_record = RecordEnvelope {
            record_id: "fleetd:event:worker-1:2".into(),
            producer: "fleetd".into(),
            cursor: "1".into(),
            kind: "session.event_committed".into(),
            occurred_at: None,
            subject: Some("session-1".into()),
            attributes: BTreeMap::from([
                ("worker_id".into(), "worker-1".into()),
                ("session_id".into(), "session-1".into()),
                ("event_seq".into(), "2".into()),
            ]),
            payload: json!({
                "transcript_path": event_log,
                "through_event_seq": 2
            }),
        };

        let receipt = service
            .ingest_records(RecordIngestRequest {
                records: vec![transcript_record.clone()],
            })
            .await
            .unwrap();
        assert_eq!(receipt.transcript_cursors.get("worker-1"), Some(&2));
        let archives = std::fs::read_dir(service.records.transcript_archive_root())
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(archives.len(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                archives[0].metadata().unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let hits = service
            .index
            .hybrid_bm25_hits("fleet-transcript-needle", 10, Some("transcript"))
            .unwrap();
        assert_eq!(hits.len(), 1);

        std::fs::remove_file(&event_log).unwrap();
        let replay = service
            .ingest_records(RecordIngestRequest {
                records: vec![transcript_record],
            })
            .await
            .unwrap();
        assert_eq!((replay.accepted, replay.deduplicated), (0, 1));
        assert_eq!(replay.transcript_cursors.get("worker-1"), Some(&2));
        drop(service);

        let reopened = CorpusService::open(paths).unwrap();
        let hits = reopened
            .index
            .hybrid_bm25_hits("fleet-transcript-needle", 10, Some("transcript"))
            .unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[tokio::test]
    async fn startup_reconciles_durable_records_idempotently() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let paths = CorpusServicePaths::new(root.join("index"), root.join("records"));
        {
            let service = CorpusService::open(paths.clone()).unwrap();
            service
                .ingest_records(RecordIngestRequest {
                    records: vec![record()],
                })
                .await
                .unwrap();
        }

        let reopened = CorpusService::open(paths).unwrap();
        let indexed = reopened
            .index
            .hybrid_bm25_hits("needle", 10, Some("operational_record"))
            .unwrap();
        assert_eq!(indexed.len(), 1);
        assert_eq!(indexed[0].entity_id, "record-1");
    }

    #[tokio::test]
    async fn routed_endpoint_rejects_elapsed_deadline() {
        let (_dir, service) = test_service();
        let authorization = service.authorization_header();
        let routed = RoutedCapabilityRequest {
            worker_id: bro_core::WorkerId::new("worker-1"),
            session_id: bro_core::SessionId::new("session-1"),
            authorization: Some(capability_authorization(
                "worker-1",
                "session-1",
                "search_corpus",
            )),
            request: CapabilityRequest {
                call_id: "call-1".into(),
                invocation_id: None,
                capability: "corpus".into(),
                operation: "search_corpus".into(),
                bounded_payload: json!({"query": "needle", "limit": 10}),
                deadline_unix_ms: Some(unix_time_ms().saturating_sub(1)),
            },
        };
        let response = router(service)
            .oneshot(
                Request::post("/internal/capability")
                    .header("content-type", "application/json")
                    .header("authorization", authorization)
                    .body(Body::from(serde_json::to_vec(&routed).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let response: CapabilityResponse = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            response.structured_error().unwrap().unwrap().code,
            CapabilityErrorCode::DeadlineExceeded
        );
    }

    #[tokio::test]
    async fn routed_endpoint_rejects_missing_or_mismatched_authorization() {
        let (_dir, service) = test_service();
        let header = service.authorization_header();
        let cases = [
            ("missing", None),
            (
                "wrong-worker",
                Some(capability_authorization(
                    "worker-other",
                    "session-1",
                    "search_corpus",
                )),
            ),
            (
                "wrong-session",
                Some(capability_authorization(
                    "worker-1",
                    "session-other",
                    "search_corpus",
                )),
            ),
            (
                "wrong-operation",
                Some(capability_authorization("worker-1", "session-1", "other")),
            ),
        ];

        for (name, authorization) in cases {
            let routed = RoutedCapabilityRequest {
                worker_id: bro_core::WorkerId::new("worker-1"),
                session_id: bro_core::SessionId::new("session-1"),
                authorization,
                request: CapabilityRequest {
                    call_id: format!("call-{name}"),
                    invocation_id: None,
                    capability: "corpus".into(),
                    operation: "search_corpus".into(),
                    bounded_payload: json!({"query": "needle", "limit": 10}),
                    deadline_unix_ms: None,
                },
            };
            let response = router(service.clone())
                .oneshot(
                    Request::post("/internal/capability")
                        .header("content-type", "application/json")
                        .header("authorization", header.clone())
                        .body(Body::from(serde_json::to_vec(&routed).unwrap()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let response: CapabilityResponse = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(
                response.structured_error().unwrap().unwrap().code,
                CapabilityErrorCode::Unauthorized,
                "case {name}"
            );
        }
    }

    #[tokio::test]
    async fn health_is_public_but_internal_routes_require_the_service_token() {
        let (_dir, service) = test_service();
        let authorization = service.authorization_header();
        let app = router(service);

        let health = app
            .clone()
            .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(health.status(), StatusCode::OK);

        let denied = app
            .clone()
            .oneshot(
                Request::post("/internal/records")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);

        let admitted = app
            .oneshot(
                Request::post("/internal/records")
                    .header("content-type", "application/json")
                    .header("authorization", authorization)
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(admitted.status(), StatusCode::UNAUTHORIZED);
    }
}
