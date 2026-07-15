use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bro_core::{AtomRef, SessionId, WorkerId};
use bro_protocol::{
    CapabilityAuthorization, CapabilityError, CapabilityErrorCode, CapabilityRequest,
    CapabilityResponse, DownstreamServiceAvailability, ServiceAvailability,
};
use bro_rpc::ServiceToken;
use fleet_core::CapabilityRoute;
use serde::Serialize;

use crate::{FleetdError, FleetdResult};

const MAX_CAPABILITY_REQUEST_BYTES: usize = 512 * 1024;
const MAX_CAPABILITY_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

#[async_trait]
pub trait CapabilityBroker: Send + Sync {
    async fn call(
        &self,
        route: CapabilityRoute,
        worker_id: &WorkerId,
        session_id: &SessionId,
        authorization: CapabilityAuthorization,
        request: CapabilityRequest,
    ) -> FleetdResult<CapabilityResponse>;

    async fn downstream_availability(&self) -> DownstreamServiceAvailability {
        DownstreamServiceAvailability::default()
    }

    fn availability_poll_interval(&self) -> Duration {
        Duration::from_secs(1)
    }
}

#[derive(Debug, Default)]
pub struct UnavailableCapabilityBroker;

#[async_trait]
impl CapabilityBroker for UnavailableCapabilityBroker {
    async fn call(
        &self,
        route: CapabilityRoute,
        worker_id: &WorkerId,
        session_id: &SessionId,
        authorization: CapabilityAuthorization,
        request: CapabilityRequest,
    ) -> FleetdResult<CapabilityResponse> {
        if let Some(response) =
            rejected_authorization(worker_id, session_id, &authorization, &request)?
        {
            return Ok(response);
        }
        unavailable_response(
            request.call_id,
            format!("{route:?} capability service is not configured"),
        )
    }
}

#[derive(Clone)]
pub struct HttpCapabilityBroker {
    client: reqwest::Client,
    operational_url: Option<String>,
    operational_health_url: Option<String>,
    corpus_url: Option<String>,
    corpus_health_url: Option<String>,
    timeout: Duration,
    service_token: Arc<ServiceToken>,
}

impl HttpCapabilityBroker {
    pub fn new(
        operational_url: Option<String>,
        corpus_url: Option<String>,
        timeout: Duration,
        service_token: Arc<ServiceToken>,
    ) -> FleetdResult<Self> {
        if timeout.is_zero() {
            return Err(FleetdError::InvalidConfiguration(
                "capability timeout must be nonzero".into(),
            ));
        }
        let (operational_url, operational_health_url) = service_endpoints(operational_url);
        let (corpus_url, corpus_health_url) = service_endpoints(corpus_url);
        Ok(Self {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .build()
                .map_err(|error| FleetdError::CapabilityUnavailable(error.to_string()))?,
            operational_url,
            operational_health_url,
            corpus_url,
            corpus_health_url,
            timeout,
            service_token,
        })
    }

    fn endpoint(&self, route: CapabilityRoute) -> Option<&str> {
        match route {
            CapabilityRoute::Operational => self.operational_url.as_deref(),
            CapabilityRoute::Corpus => self.corpus_url.as_deref(),
        }
    }

    async fn probe_health(&self, endpoint: Option<&str>) -> ServiceAvailability {
        let Some(endpoint) = endpoint else {
            return ServiceAvailability::Unconfigured;
        };
        let timeout = self.timeout.min(Duration::from_secs(2));
        match tokio::time::timeout(timeout, self.client.get(endpoint).send()).await {
            Ok(Ok(response)) if response.status().is_success() => ServiceAvailability::Available,
            _ => ServiceAvailability::Unavailable,
        }
    }
}

#[derive(Serialize)]
struct RoutedCapabilityRequest<'a> {
    worker_id: &'a WorkerId,
    session_id: &'a SessionId,
    authorization: &'a CapabilityAuthorization,
    request: &'a CapabilityRequest,
}

#[async_trait]
impl CapabilityBroker for HttpCapabilityBroker {
    async fn downstream_availability(&self) -> DownstreamServiceAvailability {
        let (blackops, corpus) = tokio::join!(
            self.probe_health(self.operational_health_url.as_deref()),
            self.probe_health(self.corpus_health_url.as_deref())
        );
        DownstreamServiceAvailability { blackops, corpus }
    }

    async fn call(
        &self,
        route: CapabilityRoute,
        worker_id: &WorkerId,
        session_id: &SessionId,
        authorization: CapabilityAuthorization,
        request: CapabilityRequest,
    ) -> FleetdResult<CapabilityResponse> {
        let call_id = request.call_id.clone();
        if let Some(response) =
            rejected_authorization(worker_id, session_id, &authorization, &request)?
        {
            return Ok(response);
        }
        let Some(endpoint) = self.endpoint(route) else {
            return unavailable_response(
                call_id,
                format!("{route:?} capability service is not configured"),
            );
        };
        let body = RoutedCapabilityRequest {
            worker_id,
            session_id,
            authorization: &authorization,
            request: &request,
        };
        let encoded = serde_json::to_vec(&body)?;
        if encoded.len() > MAX_CAPABILITY_REQUEST_BYTES {
            return CapabilityResponse::error(
                call_id,
                CapabilityError {
                    code: CapabilityErrorCode::InvalidRequest,
                    message: format!(
                        "capability request exceeds {MAX_CAPABILITY_REQUEST_BYTES} bytes"
                    ),
                    retryable: false,
                    details: None,
                },
            )
            .map_err(Into::into);
        }
        if request
            .deadline_unix_ms
            .is_some_and(|deadline| deadline <= now_ms())
        {
            return CapabilityResponse::error(
                call_id,
                CapabilityError {
                    code: CapabilityErrorCode::DeadlineExceeded,
                    message: "capability deadline already elapsed".into(),
                    retryable: false,
                    details: None,
                },
            )
            .map_err(Into::into);
        }
        let deadline_timeout = request
            .deadline_unix_ms
            .map(|deadline| Duration::from_millis(deadline.saturating_sub(now_ms()).max(1)));
        let timeout = deadline_timeout
            .map(|deadline| deadline.min(self.timeout))
            .unwrap_or(self.timeout);
        let outcome = tokio::time::timeout(timeout, async {
            let mut response = match self
                .client
                .post(endpoint)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .header(
                    reqwest::header::AUTHORIZATION,
                    self.service_token.authorization_header(),
                )
                .body(encoded)
                .send()
                .await
            {
                Ok(response) => response,
                Err(error) => return unavailable_response(call_id.clone(), error.to_string()),
            };
            if !response.status().is_success() {
                return unavailable_response(
                    call_id.clone(),
                    format!("capability service returned {}", response.status()),
                );
            }
            if response
                .content_length()
                .is_some_and(|length| length > MAX_CAPABILITY_RESPONSE_BYTES as u64)
            {
                return unavailable_response(
                    call_id.clone(),
                    "capability response is oversized".into(),
                );
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|error| FleetdError::CapabilityUnavailable(error.to_string()))?
            {
                if bytes.len().saturating_add(chunk.len()) > MAX_CAPABILITY_RESPONSE_BYTES {
                    return unavailable_response(
                        call_id.clone(),
                        "capability response is oversized".into(),
                    );
                }
                bytes.extend_from_slice(&chunk);
            }
            let response: CapabilityResponse = serde_json::from_slice(&bytes)?;
            if response.call_id != request.call_id {
                return Err(FleetdError::CapabilityUnavailable(
                    "capability response correlation mismatch".into(),
                ));
            }
            Ok(response)
        })
        .await;
        match outcome {
            Ok(response) => response,
            Err(_) => CapabilityResponse::error(
                call_id,
                CapabilityError {
                    code: CapabilityErrorCode::DeadlineExceeded,
                    message: "capability service deadline elapsed".into(),
                    retryable: true,
                    details: None,
                },
            )
            .map_err(Into::into),
        }
    }
}

fn rejected_authorization(
    worker_id: &WorkerId,
    session_id: &SessionId,
    authorization: &CapabilityAuthorization,
    request: &CapabilityRequest,
) -> FleetdResult<Option<CapabilityResponse>> {
    let exact_operation = authorization.authorizes(
        worker_id,
        session_id,
        &request.capability,
        &request.operation,
    );
    let exact_atom_ref = if request.capability == "atom" {
        request
            .bounded_payload
            .get("atom")
            .and_then(serde_json::Value::as_str)
            .map(AtomRef::new)
            .is_some_and(|atom_ref| authorization.capability_policy.allows_atom_ref(&atom_ref))
    } else {
        true
    };
    if exact_operation && exact_atom_ref {
        return Ok(None);
    }
    CapabilityResponse::error(
        request.call_id.clone(),
        CapabilityError {
            code: CapabilityErrorCode::Unauthorized,
            message:
                "fleet authorization does not grant this exact capability operation or atom ref"
                    .into(),
            retryable: false,
            details: None,
        },
    )
    .map(Some)
    .map_err(Into::into)
}

fn service_endpoints(base: Option<String>) -> (Option<String>, Option<String>) {
    let Some(mut base) = base else {
        return (None, None);
    };
    while base.ends_with('/') {
        base.pop();
    }
    (
        Some(format!("{base}/internal/capability")),
        Some(format!("{base}/readyz")),
    )
}

fn unavailable_response(call_id: String, message: String) -> FleetdResult<CapabilityResponse> {
    CapabilityResponse::error(
        call_id,
        CapabilityError {
            code: CapabilityErrorCode::Unavailable,
            message,
            retryable: true,
            details: None,
        },
    )
    .map_err(Into::into)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::convert::Infallible;
    use std::time::Instant;

    use axum::Router;
    use axum::body::{Body, Bytes};
    use axum::http::HeaderMap;
    use axum::response::Response;
    use axum::routing::{get, post};
    use futures::{StreamExt as _, stream};
    use serde_json::Value;

    use super::*;

    fn authorization(capability: &str, operation: &str) -> CapabilityAuthorization {
        CapabilityAuthorization {
            worker_id: WorkerId::new("worker-1"),
            session_id: SessionId::new("session-1"),
            task_id: bro_core::TaskId::new("task-1"),
            attempt_id: bro_core::AttemptId::new("attempt-1"),
            session_attempt_generation: 1,
            policy: bro_protocol::PolicyIdentity {
                version: 1,
                digest: "sha256:test".into(),
            },
            capability_policy: bro_protocol::SessionCapabilityPolicy {
                allowed_operations: BTreeMap::from([(
                    capability.to_string(),
                    BTreeSet::from([operation.to_string()]),
                )]),
                allowed_atom_refs: BTreeSet::new(),
            },
        }
    }

    #[tokio::test]
    async fn absent_service_fails_closed_with_typed_response() {
        assert_eq!(
            UnavailableCapabilityBroker.downstream_availability().await,
            DownstreamServiceAvailability::default()
        );
        let response = UnavailableCapabilityBroker
            .call(
                CapabilityRoute::Corpus,
                &WorkerId::new("worker-1"),
                &SessionId::new("session-1"),
                authorization("corpus.search", "search"),
                CapabilityRequest {
                    call_id: "call-1".into(),
                    invocation_id: None,
                    capability: "corpus.search".into(),
                    operation: "search".into(),
                    bounded_payload: Value::Null,
                    deadline_unix_ms: None,
                },
            )
            .await
            .unwrap();
        assert!(response.is_error);
        assert_eq!(
            response.structured_error().unwrap().unwrap().code,
            CapabilityErrorCode::Unavailable
        );
    }

    #[tokio::test]
    async fn broker_rejects_wrong_operation_and_atom_ref_before_transport() {
        let wrong_operation = UnavailableCapabilityBroker
            .call(
                CapabilityRoute::Operational,
                &WorkerId::new("worker-1"),
                &SessionId::new("session-1"),
                authorization("blackops.agent", "status"),
                CapabilityRequest {
                    call_id: "call-wrong-operation".into(),
                    invocation_id: None,
                    capability: "blackops.agent".into(),
                    operation: "spawn".into(),
                    bounded_payload: serde_json::Value::Null,
                    deadline_unix_ms: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            wrong_operation.structured_error().unwrap().unwrap().code,
            CapabilityErrorCode::Unauthorized
        );

        let mut atom_authorization = authorization("atom", "invoke_atom");
        atom_authorization
            .capability_policy
            .allowed_atom_refs
            .insert(AtomRef::new("atom:allowed@v1"));
        let wrong_atom = UnavailableCapabilityBroker
            .call(
                CapabilityRoute::Operational,
                &WorkerId::new("worker-1"),
                &SessionId::new("session-1"),
                atom_authorization,
                CapabilityRequest {
                    call_id: "call-wrong-atom".into(),
                    invocation_id: None,
                    capability: "atom".into(),
                    operation: "invoke_atom".into(),
                    bounded_payload: serde_json::json!({"atom": "atom:other@v1"}),
                    deadline_unix_ms: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            wrong_atom.structured_error().unwrap().unwrap().code,
            CapabilityErrorCode::Unauthorized
        );
    }

    #[tokio::test]
    async fn response_body_and_authentication_share_the_total_deadline() {
        let token = Arc::new(ServiceToken::parse("a".repeat(64)).unwrap());
        let expected_authorization = token.authorization_header();
        let app = Router::new().route(
            "/internal/capability",
            post(move |headers: HeaderMap| {
                let expected_authorization = expected_authorization.clone();
                async move {
                    assert_eq!(
                        headers.get(axum::http::header::AUTHORIZATION),
                        Some(&expected_authorization)
                    );
                    let body = stream::iter([Ok::<Bytes, Infallible>(Bytes::from_static(b"{"))])
                        .chain(stream::pending());
                    Response::new(Body::from_stream(body))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let broker = HttpCapabilityBroker::new(
            None,
            Some(format!("http://{address}")),
            Duration::from_millis(80),
            token,
        )
        .unwrap();
        let started = Instant::now();
        let response = broker
            .call(
                CapabilityRoute::Corpus,
                &WorkerId::new("worker-1"),
                &SessionId::new("session-1"),
                authorization("corpus.search", "search"),
                CapabilityRequest {
                    call_id: "call-stalled-body".into(),
                    invocation_id: None,
                    capability: "corpus.search".into(),
                    operation: "search".into(),
                    bounded_payload: Value::Null,
                    deadline_unix_ms: None,
                },
            )
            .await
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(response.call_id, "call-stalled-body");
        assert_eq!(
            response.structured_error().unwrap().unwrap().code,
            CapabilityErrorCode::DeadlineExceeded
        );
        server.abort();
    }

    #[tokio::test]
    async fn readiness_probes_distinguish_blackops_and_corpus_state() {
        async fn serve(app: Router) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            (address, server)
        }

        let (blackops_address, blackops_server) =
            serve(Router::new().route("/readyz", get(|| async { axum::http::StatusCode::OK })))
                .await;
        let (corpus_address, corpus_server) = serve(Router::new().route(
            "/readyz",
            get(|| async { axum::http::StatusCode::SERVICE_UNAVAILABLE }),
        ))
        .await;
        let broker = HttpCapabilityBroker::new(
            Some(format!("http://{blackops_address}")),
            Some(format!("http://{corpus_address}")),
            Duration::from_millis(250),
            Arc::new(ServiceToken::parse("b".repeat(64)).unwrap()),
        )
        .unwrap();
        assert_eq!(
            broker.downstream_availability().await,
            DownstreamServiceAvailability {
                blackops: ServiceAvailability::Available,
                corpus: ServiceAvailability::Unavailable,
            }
        );
        blackops_server.abort();
        corpus_server.abort();
    }
}
