use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use blackbox_corpus_service::{CorpusService, CorpusServicePaths, RoutedCapabilityRequest, serve};
use bro_capabilities::{CorpusHit, RecordEnvelope, RecordIngestReceipt, RecordIngestRequest};
use bro_protocol::{
    CapabilityAuthorization, CapabilityRequest, CapabilityResponse, PolicyIdentity,
    SessionCapabilityPolicy,
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

struct RunningService {
    address: SocketAddr,
    authorization: reqwest::header::HeaderValue,
    shutdown: CancellationToken,
    task: JoinHandle<anyhow::Result<()>>,
}

impl RunningService {
    async fn stop(self) {
        self.shutdown.cancel();
        self.task.await.unwrap().unwrap();
    }
}

async fn start(root: &Path, address: Option<SocketAddr>) -> RunningService {
    let listener =
        tokio::net::TcpListener::bind(address.unwrap_or_else(|| "127.0.0.1:0".parse().unwrap()))
            .await
            .unwrap();
    let address = listener.local_addr().unwrap();
    let service = Arc::new(
        CorpusService::open(CorpusServicePaths::new(
            root.join("index"),
            root.join("records"),
        ))
        .unwrap(),
    );
    let authorization = service.authorization_header();
    let shutdown = CancellationToken::new();
    let task_shutdown = shutdown.clone();
    let task = tokio::spawn(async move { serve(listener, service, task_shutdown).await });
    let client = reqwest::Client::new();
    for _ in 0..100 {
        if client
            .get(format!("http://{address}/readyz"))
            .send()
            .await
            .is_ok_and(|response| response.status().is_success())
        {
            return RunningService {
                address,
                authorization,
                shutdown,
                task,
            };
        }
        tokio::task::yield_now().await;
    }
    panic!("corpus service did not become ready");
}

fn record_request() -> RecordIngestRequest {
    RecordIngestRequest {
        records: vec![RecordEnvelope {
            record_id: "operation:one:completed".into(),
            producer: "blackopsd".into(),
            cursor: "19".into(),
            kind: "operation.completed".into(),
            occurred_at: Some("2026-07-15T00:00:00Z".into()),
            subject: Some("operation-one".into()),
            attributes: BTreeMap::new(),
            payload: serde_json::json!({"summary": "restart needle"}),
        }],
    }
}

fn capability_authorization() -> CapabilityAuthorization {
    CapabilityAuthorization {
        worker_id: bro_core::WorkerId::new("worker-one"),
        session_id: bro_core::SessionId::new("session-one"),
        task_id: bro_core::TaskId::new("task-one"),
        attempt_id: bro_core::AttemptId::new("attempt-one"),
        session_attempt_generation: 1,
        policy: PolicyIdentity {
            version: 1,
            digest: "sha256:test-policy".into(),
        },
        capability_policy: SessionCapabilityPolicy {
            allowed_operations: BTreeMap::from([(
                "corpus".into(),
                BTreeSet::from(["search_corpus".into()]),
            )]),
            allowed_atom_refs: BTreeSet::new(),
        },
    }
}

async fn search(
    client: &reqwest::Client,
    address: SocketAddr,
    authorization: reqwest::header::HeaderValue,
) -> Vec<CorpusHit> {
    let routed = RoutedCapabilityRequest {
        worker_id: bro_core::WorkerId::new("worker-one"),
        session_id: bro_core::SessionId::new("session-one"),
        authorization: Some(capability_authorization()),
        request: CapabilityRequest {
            call_id: "call-one".into(),
            invocation_id: None,
            capability: "corpus".into(),
            operation: "search_corpus".into(),
            bounded_payload: serde_json::json!({"query": "restart needle", "limit": 10}),
            deadline_unix_ms: None,
        },
    };
    let response: CapabilityResponse = client
        .post(format!("http://{address}/internal/capability"))
        .header(reqwest::header::AUTHORIZATION, authorization)
        .json(&routed)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(!response.is_error, "{:?}", response.result_or_error);
    serde_json::from_value(response.result_or_error).unwrap()
}

#[tokio::test]
async fn outage_restart_and_replay_preserve_one_record() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();

    let first = start(&root, None).await;
    let address = first.address;
    let receipt: RecordIngestReceipt = client
        .post(format!("http://{address}/internal/records"))
        .header(reqwest::header::AUTHORIZATION, first.authorization.clone())
        .json(&record_request())
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!((receipt.accepted, receipt.deduplicated), (1, 0));
    assert_eq!(
        search(&client, address, first.authorization.clone())
            .await
            .len(),
        1
    );
    first.stop().await;

    assert!(
        client
            .get(format!("http://{address}/healthz"))
            .send()
            .await
            .is_err(),
        "the service must actually be unavailable during the outage"
    );

    let second = start(&root, Some(address)).await;
    let replay: RecordIngestReceipt = client
        .post(format!("http://{address}/internal/records"))
        .header(reqwest::header::AUTHORIZATION, second.authorization.clone())
        .json(&record_request())
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!((replay.accepted, replay.deduplicated), (0, 1));
    let hits = search(&client, address, second.authorization.clone()).await;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, "operation:one:completed");
    second.stop().await;
}
