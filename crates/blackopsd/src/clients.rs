use async_trait::async_trait;
use bro_capabilities::{
    AttemptOutcome, ExecutionAccepted, ExecutionCapability, ExecutionRequest,
    RecordIngestCapability, RecordIngestReceipt, RecordIngestRequest,
};
use bro_core::{AttemptId, BroError, TaskId};
use bro_protocol::{AgentMailboxDelivery, AgentMailboxDeliveryReceipt};
use serde_json::{Value, json};
use std::sync::Arc;

#[async_trait]
pub trait FleetControlCapability: Send + Sync {
    async fn interrupt_task(&self, task_id: TaskId) -> Result<Value, BroError>;

    async fn deliver_agent_mailbox(
        &self,
        _delivery: AgentMailboxDelivery,
    ) -> Result<AgentMailboxDeliveryReceipt, BroError> {
        Err(BroError::new(
            "fleet.mailbox.unavailable",
            "fleet mailbox delivery is unavailable",
        ))
    }
}

#[derive(Debug, Clone)]
pub struct FleetHttpClient {
    client: reqwest::Client,
    base_url: String,
    service_token: Arc<bro_rpc::ServiceToken>,
}

impl FleetHttpClient {
    pub fn new(
        base_url: impl Into<String>,
        timeout: std::time::Duration,
        service_token: Arc<bro_rpc::ServiceToken>,
    ) -> Result<Self, BroError> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|error| BroError::new("fleet.client", error.to_string()))?;
        Ok(Self {
            client,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            service_token,
        })
    }
}

#[async_trait]
impl ExecutionCapability for FleetHttpClient {
    async fn request_execution(
        &self,
        request: ExecutionRequest,
    ) -> Result<ExecutionAccepted, BroError> {
        let response = self
            .client
            .post(format!("{}/v1/executions", self.base_url))
            .header(
                reqwest::header::AUTHORIZATION,
                self.service_token.authorization_header(),
            )
            .json(&request)
            .send()
            .await
            .map_err(|error| BroError::new("fleet.unavailable", error.to_string()))?;
        decode(response, "fleet.execution").await
    }

    async fn attempt_outcome(&self, attempt_id: AttemptId) -> Result<AttemptOutcome, BroError> {
        let response = self
            .client
            .get(format!("{}/v1/attempts/{attempt_id}", self.base_url))
            .header(
                reqwest::header::AUTHORIZATION,
                self.service_token.authorization_header(),
            )
            .send()
            .await
            .map_err(|error| BroError::new("fleet.unavailable", error.to_string()))?;
        decode(response, "fleet.attempt").await
    }
}

#[async_trait]
impl FleetControlCapability for FleetHttpClient {
    async fn interrupt_task(&self, task_id: TaskId) -> Result<Value, BroError> {
        let response = self
            .client
            .post(format!("{}/control/interrupt", self.base_url))
            .header(
                reqwest::header::AUTHORIZATION,
                self.service_token.authorization_header(),
            )
            .json(&json!({"task_id": task_id}))
            .send()
            .await
            .map_err(|error| BroError::new("fleet.unavailable", error.to_string()))?;
        decode(response, "fleet.interrupt").await
    }

    async fn deliver_agent_mailbox(
        &self,
        delivery: AgentMailboxDelivery,
    ) -> Result<AgentMailboxDeliveryReceipt, BroError> {
        let response = self
            .client
            .post(format!("{}/internal/agents/mailbox", self.base_url))
            .header(
                reqwest::header::AUTHORIZATION,
                self.service_token.authorization_header(),
            )
            .json(&delivery)
            .send()
            .await
            .map_err(|error| BroError::new("fleet.unavailable", error.to_string()))?;
        decode(response, "fleet.mailbox").await
    }
}

#[derive(Debug, Clone)]
pub struct BlackboxRecordHttpClient {
    client: reqwest::Client,
    base_url: String,
    service_token: Arc<bro_rpc::ServiceToken>,
}

impl BlackboxRecordHttpClient {
    pub fn new(
        base_url: impl Into<String>,
        timeout: std::time::Duration,
        service_token: Arc<bro_rpc::ServiceToken>,
    ) -> Result<Self, BroError> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|error| BroError::new("records.client", error.to_string()))?;
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
        decode(response, "records.ingest").await
    }
}

async fn decode<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
    code: &str,
) -> Result<T, BroError> {
    let status = response.status();
    if status.is_success() {
        return response
            .json()
            .await
            .map_err(|error| BroError::new(format!("{code}.decode"), error.to_string()));
    }
    let detail = response.text().await.unwrap_or_default();
    Err(BroError::new(
        format!("{code}.http"),
        format!("upstream returned {status}: {detail}"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::post;
    use axum::{Json, Router};
    use std::collections::BTreeMap;

    const SECRET: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn authorized(headers: &HeaderMap) -> bool {
        headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            == Some(&format!("Bearer {SECRET}"))
    }

    #[tokio::test]
    async fn differentiated_http_clients_attach_the_shared_bearer() {
        let app = Router::new()
            .route(
                "/control/interrupt",
                post(|headers: HeaderMap| async move {
                    if authorized(&headers) {
                        (StatusCode::OK, Json(json!({"accepted": true})))
                    } else {
                        (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({"error": "unauthorized"})),
                        )
                    }
                }),
            )
            .route(
                "/internal/records",
                post(|headers: HeaderMap| async move {
                    let status = if authorized(&headers) {
                        StatusCode::OK
                    } else {
                        StatusCode::UNAUTHORIZED
                    };
                    (
                        status,
                        Json(RecordIngestReceipt {
                            accepted: 0,
                            deduplicated: 0,
                            producer_cursors: BTreeMap::new(),
                            transcript_cursors: BTreeMap::new(),
                        }),
                    )
                }),
            );
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let token = Arc::new(bro_rpc::ServiceToken::parse(SECRET).unwrap());
        let fleet = FleetHttpClient::new(
            format!("http://{address}"),
            std::time::Duration::from_secs(2),
            token.clone(),
        )
        .unwrap();
        assert!(
            fleet.interrupt_task(TaskId::new("task-1")).await.unwrap()["accepted"]
                .as_bool()
                .unwrap()
        );
        let records = BlackboxRecordHttpClient::new(
            format!("http://{address}"),
            std::time::Duration::from_secs(2),
            token,
        )
        .unwrap();
        records
            .ingest_records(RecordIngestRequest { records: vec![] })
            .await
            .unwrap();
        server.abort();
    }
}
