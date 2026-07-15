use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::{Method, Response};
use reqwest::Url;

use crate::{FleetdError, FleetdResult};

const MAX_COMPATIBILITY_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const COMPATIBILITY_SUNSET: &str = "Thu, 31 Dec 2026 23:59:59 GMT";

#[derive(Debug, Clone, Copy)]
pub(crate) enum CompatibilityOwner {
    Blackopsd,
}

impl CompatibilityOwner {
    fn name(self) -> &'static str {
        match self {
            Self::Blackopsd => "blackopsd",
        }
    }
}

#[derive(Clone)]
pub(crate) struct CompatibilityProxy {
    client: reqwest::Client,
    blackopsd_url: Option<String>,
    timeout: Duration,
    service_token: Arc<bro_rpc::ServiceToken>,
}

impl CompatibilityProxy {
    pub(crate) fn new(
        blackopsd_url: Option<String>,
        timeout: Duration,
        service_token: Arc<bro_rpc::ServiceToken>,
    ) -> FleetdResult<Self> {
        if timeout.is_zero() {
            return Err(FleetdError::InvalidConfiguration(
                "compatibility proxy timeout must be nonzero".into(),
            ));
        }
        Ok(Self {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .build()
                .map_err(|error| FleetdError::CompatibilityUnavailable {
                    owner: "compatibility owner",
                    detail: error.to_string(),
                })?,
            blackopsd_url,
            timeout,
            service_token,
        })
    }

    pub(crate) async fn forward(
        &self,
        owner: CompatibilityOwner,
        method: Method,
        path_segments: &[&str],
        query: Option<&str>,
        body: Bytes,
    ) -> FleetdResult<Response<Body>> {
        let owner_name = owner.name();
        let base =
            self.blackopsd_url
                .as_deref()
                .ok_or_else(|| FleetdError::CompatibilityUnavailable {
                    owner: owner_name,
                    detail: "service URL is not configured".into(),
                })?;
        let endpoint = endpoint(base, path_segments, query).map_err(|detail| {
            FleetdError::CompatibilityUnavailable {
                owner: owner_name,
                detail,
            }
        })?;
        let mut request = self
            .client
            .request(method, endpoint)
            .header(
                reqwest::header::AUTHORIZATION,
                self.service_token.authorization_header(),
            )
            .timeout(self.timeout);
        if !body.is_empty() {
            request = request
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body);
        }
        let mut upstream =
            request
                .send()
                .await
                .map_err(|error| FleetdError::CompatibilityUnavailable {
                    owner: owner_name,
                    detail: error.to_string(),
                })?;
        if upstream
            .content_length()
            .is_some_and(|length| length > MAX_COMPATIBILITY_RESPONSE_BYTES as u64)
        {
            return Err(FleetdError::CompatibilityUnavailable {
                owner: owner_name,
                detail: "response exceeded the compatibility proxy limit".into(),
            });
        }
        let status = upstream.status();
        let content_type = upstream
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .cloned();
        let mut bytes = Vec::new();
        while let Some(chunk) =
            upstream
                .chunk()
                .await
                .map_err(|error| FleetdError::CompatibilityUnavailable {
                    owner: owner_name,
                    detail: error.to_string(),
                })?
        {
            if bytes.len().saturating_add(chunk.len()) > MAX_COMPATIBILITY_RESPONSE_BYTES {
                return Err(FleetdError::CompatibilityUnavailable {
                    owner: owner_name,
                    detail: "response exceeded the compatibility proxy limit".into(),
                });
            }
            bytes.extend_from_slice(&chunk);
        }
        let mut response = Response::builder()
            .status(status)
            .header("x-fleetd-compatibility-proxy", owner_name)
            .header("deprecation", "true")
            .header("sunset", COMPATIBILITY_SUNSET);
        if let Some(content_type) = content_type {
            response = response.header("content-type", content_type);
        }
        response
            .body(Body::from(bytes))
            .map_err(|error| FleetdError::CompatibilityUnavailable {
                owner: owner_name,
                detail: error.to_string(),
            })
    }
}

fn endpoint(base: &str, path_segments: &[&str], query: Option<&str>) -> Result<Url, String> {
    let mut url = Url::parse(base).map_err(|error| error.to_string())?;
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| "service URL cannot be used as a hierarchical endpoint".to_string())?;
        segments.pop_if_empty();
        segments.extend(path_segments.iter().copied());
    }
    url.set_query(query);
    Ok(url)
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    #[test]
    fn endpoint_preserves_prefix_and_escapes_dynamic_segment() {
        let url = endpoint(
            "http://127.0.0.1:7000/api/",
            &["control", "team", "name with spaces"],
            Some("tail=5"),
        )
        .unwrap();
        assert_eq!(
            url.as_str(),
            "http://127.0.0.1:7000/api/control/team/name%20with%20spaces?tail=5"
        );
    }

    #[test]
    fn compatibility_sunset_is_a_valid_header_value() {
        HeaderValue::from_str(COMPATIBILITY_SUNSET).unwrap();
    }
}
