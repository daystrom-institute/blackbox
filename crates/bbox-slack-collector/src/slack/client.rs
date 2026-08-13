//! The allowlist Slack client: layer two of the write-safety contract.
//!
//! Every request this type composes is
//! `{base}/{method}` where `method` came from [`SlackReadMethod`], a closed
//! enum of reads. There is no string-taking entry point, no method parameter
//! reachable from config, and no vendor SDK underneath that would supply one.
//! A write is not "guarded against" here; it is unrepresentable.
//!
//! Beyond that, three behaviors are load-bearing and easy to get subtly wrong:
//!
//! 1. **`ok: false` arrives with HTTP 200.** Slack's error channel is in the
//!    body. A client that trusted the status line would read
//!    `{"ok":false,"error":"ratelimited"}` as a successful empty page and
//!    advance a watermark over messages it never saw, which is the exact
//!    failure mode design 5.3 calls "an ingestion lane that looks correct and
//!    is not".
//! 2. **429 is honored, never raced.** `Retry-After` is taken at face value
//!    within a clamp, the penalty applies to the whole credential through the
//!    pacer rather than to the one refused request, and the retry budget is
//!    small. A tight retry loop against a shared credential is how a corpus
//!    job gets an interactive bot rate-limited.
//! 3. **Responses are bounded before they are trusted.** A declared
//!    `Content-Length` over the cap is refused without reading the body, and an
//!    undeclared body is refused after the read. Neither check is the vendor's
//!    to make.

use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use reqwest::{Client, StatusCode, Url};
use serde::de::DeserializeOwned;

use super::method::SlackReadMethod;
use super::model::{
    AuthTestResponse, ConversationsHistoryResponse, ConversationsListResponse, RawChannel,
    RawMessage, SlackIdentity,
};
use super::throttle::{Pacer, RatePolicy};

/// The public Slack Web API root.
pub const DEFAULT_API_BASE_URL: &str = "https://slack.com/api";

/// The ceiling on one response body.
///
/// A page of 200 messages with generous text is far under this; the cap exists
/// so a pathological or hostile response cannot make the satellite allocate
/// without bound.
pub const MAX_RESPONSE_BYTES: u64 = 16 * 1024 * 1024;

/// The header carrying the token's granted scopes.
const GRANTED_SCOPES_HEADER: &str = "x-oauth-scopes";

/// Messages per page. Slack's documented maximum for the history methods is
/// 1000 and its own guidance is that large pages are unreliable; 200 is the
/// conventional safe page.
pub const DEFAULT_PAGE_LIMIT: u32 = 200;

/// What one sweep returned, and whether it finished.
///
/// `complete` is not decoration. A sweep that ran out of page budget holds the
/// NEWEST part of its window, not a contiguous run from the watermark, so
/// landing it would advance the cursor over a hole. [`crate::cycle`] refuses to
/// land an incomplete window for exactly that reason.
// No `PartialEq`: the vendor response shapes it carries are deliberately
// permissive (they gain fields on Slack's schedule) and comparing two sweeps
// structurally is not a thing any caller should want to do.
#[derive(Debug, Clone, Default)]
pub struct Sweep {
    pub messages: Vec<RawMessage>,
    pub complete: bool,
    pub pages: u32,
}

/// A bounded history request over one channel window.
#[derive(Debug, Clone)]
pub struct HistoryRequest {
    pub channel_id: String,
    /// Exclusive lower bound. The watermark itself already landed.
    pub oldest: Option<String>,
    /// Inclusive upper bound of the window being swept.
    pub latest: Option<String>,
    pub page_limit: u32,
    pub max_pages: u32,
}

#[derive(Debug, Clone)]
pub struct RepliesRequest {
    pub channel_id: String,
    pub parent_ts: String,
    pub oldest: Option<String>,
    pub page_limit: u32,
    pub max_pages: u32,
}

#[derive(Debug, Clone)]
pub struct ChannelListRequest {
    /// Private channels are a separate operator flag (design section 6), so the
    /// requested TYPES are a policy decision handed down rather than a constant.
    pub include_private: bool,
    pub exclude_archived: bool,
    pub page_limit: u32,
    pub max_pages: u32,
}

/// The read surface the publication cycle depends on.
///
/// The cycle is written against this rather than against [`SlackClient`] so its
/// decisions -- what to sweep, what to land, when to stop -- are separable from
/// the transport. The integration tests still drive the REAL client against a
/// fixture HTTP server, because pagination, `ok:false`, and 429 handling are
/// transport behaviors and a hand-written double would only prove itself.
#[async_trait]
pub trait SlackRead: Send + Sync {
    async fn auth_test(&self) -> Result<SlackIdentity>;
    async fn list_channels(&self, request: &ChannelListRequest) -> Result<Vec<RawChannel>>;
    async fn history(&self, request: &HistoryRequest) -> Result<Sweep>;
    async fn replies(&self, request: &RepliesRequest) -> Result<Sweep>;
}

/// Counters an operator reads to know whether the satellite is being throttled.
///
/// Design 5.5: a throttled producer shows up as LAG rather than as errors. That
/// is true corpus-side; producer-side it has to show up as something, and this
/// is it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClientStats {
    pub requests: u64,
    pub throttled: u64,
    pub retries: u64,
    pub last_retry_after_secs: Option<u64>,
    pub backoff_secs_total: u64,
}

pub struct SlackClient {
    base_url: String,
    token: String,
    http: Client,
    pacer: Pacer,
    policy: RatePolicy,
    stats: Mutex<ClientStats>,
}

impl std::fmt::Debug for SlackClient {
    /// Written by hand because this type RETAINS a bot token. The derive would
    /// put a live workspace credential into every panic message and every
    /// tracing field that rendered it, which is precisely the leak the config
    /// layer's secret reference exists to prevent.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SlackClient")
            .field("base_url", &self.base_url)
            .field("token", &"<redacted>")
            .field("policy", &self.policy)
            .finish()
    }
}

impl SlackClient {
    /// Build a client for one credential.
    ///
    /// The base URL is overridable so tests can point at a fixture server. It
    /// is validated the same way the corpus URL is: a non-loopback plain-HTTP
    /// base would put a bot token on the wire in clear text, so it is refused
    /// here rather than trusted to be a test.
    pub fn new(
        base_url: impl Into<String>,
        token: impl Into<String>,
        policy: RatePolicy,
    ) -> Result<Self> {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        crate::config::require_safe_transport(&base_url, "slack_api_base_url")?;
        let token = token.into();
        if token.trim().is_empty() {
            bail!("the Slack client needs a bot token");
        }
        let http = Client::builder()
            // Redirect following is off on both wires (design 4.1). A redirect
            // is a credential-forwarding primitive, and this request carries a
            // bearer.
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|error| anyhow!("building the Slack HTTP client: {error}"))?;
        Ok(Self {
            pacer: Pacer::new(policy.min_interval()),
            base_url,
            token,
            http,
            policy,
            stats: Mutex::new(ClientStats::default()),
        })
    }

    pub fn stats(&self) -> ClientStats {
        self.stats.lock().expect("stats mutex poisoned").clone()
    }

    fn record<F: FnOnce(&mut ClientStats)>(&self, update: F) {
        update(&mut self.stats.lock().expect("stats mutex poisoned"));
    }

    /// The one place a request URL is composed, from the closed enum only.
    fn url(&self, method: SlackReadMethod) -> Result<Url> {
        Url::parse(&format!("{}/{}", self.base_url, method.api_name()))
            .map_err(|error| anyhow!("composing a {method} URL: {error}"))
    }

    /// Send one allowlisted read, honoring the pacer and the retry budget.
    async fn send<T: DeserializeOwned>(
        &self,
        method: SlackReadMethod,
        params: &[(&str, String)],
    ) -> Result<(T, Vec<String>)> {
        let url = self.url(method)?;
        let mut attempt = 0_u32;
        loop {
            self.pacer.acquire().await;
            self.record(|stats| stats.requests += 1);
            let response = self
                .http
                .get(url.clone())
                .bearer_auth(&self.token)
                .query(params)
                .send()
                .await
                .map_err(|error| anyhow!("calling {method}: {error}"))?;
            let status = response.status();
            let retry_after = parse_retry_after(response.headers());
            let scopes = parse_scopes(response.headers());

            if status == StatusCode::TOO_MANY_REQUESTS {
                self.record(|stats| {
                    stats.throttled += 1;
                    stats.last_retry_after_secs = retry_after.map(|wait| wait.as_secs());
                });
                self.throttle(method, retry_after, &mut attempt).await?;
                continue;
            }
            if status.is_server_error() {
                self.throttle(method, retry_after, &mut attempt).await?;
                continue;
            }
            if !status.is_success() {
                bail!("{method} failed with {status}");
            }

            if let Some(length) = response.content_length()
                && length > MAX_RESPONSE_BYTES
            {
                bail!("{method} declared {length} bytes, over the {MAX_RESPONSE_BYTES} cap");
            }
            let bytes = response
                .bytes()
                .await
                .map_err(|error| anyhow!("reading the {method} body: {error}"))?;
            if bytes.len() as u64 > MAX_RESPONSE_BYTES {
                bail!(
                    "{method} returned {} bytes, over the {MAX_RESPONSE_BYTES} cap",
                    bytes.len()
                );
            }

            // The vendor's error channel is in the BODY, so it is checked
            // before the body is interpreted as data.
            let envelope: super::model::SlackEnvelope = serde_json::from_slice(&bytes)
                .map_err(|error| anyhow!("decoding the {method} envelope: {error}"))?;
            if !envelope.ok {
                let error = envelope.error.unwrap_or_else(|| "unknown".to_string());
                if error == "ratelimited" {
                    self.record(|stats| stats.throttled += 1);
                    self.throttle(method, retry_after, &mut attempt).await?;
                    continue;
                }
                bail!("{method} refused the request: {error}");
            }
            let decoded: T = serde_json::from_slice(&bytes)
                .map_err(|error| anyhow!("decoding the {method} response: {error}"))?;
            return Ok((decoded, scopes));
        }
    }

    /// Wait out a throttle, or give up when the budget is spent.
    ///
    /// The penalty goes on the PACER, not on this call site, so the next
    /// request for any channel also waits. That is the difference between
    /// backing off and taking turns being refused.
    async fn throttle(
        &self,
        method: SlackReadMethod,
        retry_after: Option<Duration>,
        attempt: &mut u32,
    ) -> Result<()> {
        *attempt += 1;
        if *attempt >= self.policy.max_attempts.max(1) {
            bail!(
                "{method} was throttled {} times; giving up this cycle rather than retrying tighter",
                attempt
            );
        }
        let delay = self.policy.backoff(retry_after, *attempt);
        self.record(|stats| {
            stats.retries += 1;
            stats.backoff_secs_total += delay.as_secs();
        });
        tracing::warn!(
            method = %method,
            attempt = *attempt,
            delay_secs = delay.as_secs(),
            retry_after_secs = retry_after.map(|wait| wait.as_secs()),
            "throttled by Slack; backing off the whole credential"
        );
        self.pacer.penalize(delay);
        tokio::time::sleep(delay).await;
        Ok(())
    }

    /// Page one cursor-paginated read to completion or to the page budget.
    async fn paginate<T, F>(
        &self,
        method: SlackReadMethod,
        base_params: &[(&str, String)],
        max_pages: u32,
        mut absorb: F,
    ) -> Result<(u32, bool)>
    where
        T: DeserializeOwned,
        F: FnMut(T) -> Option<String>,
    {
        let mut cursor: Option<String> = None;
        let mut pages = 0_u32;
        loop {
            let mut params = base_params.to_vec();
            if let Some(cursor) = &cursor {
                params.push(("cursor", cursor.clone()));
            }
            let (page, _) = self.send::<T>(method, &params).await?;
            pages += 1;
            cursor = absorb(page);
            match cursor.as_deref() {
                // Nothing more owed: the sweep is complete.
                None => return Ok((pages, true)),
                // More owed but no cursor to reach it with. This is the vendor
                // state a client cannot page out of, and calling it done would
                // be a silent hole in the window.
                Some("") => return Ok((pages, false)),
                Some(_) => {}
            }
            if pages >= max_pages.max(1) {
                // Budget spent with more owed. The caller decides what an
                // incomplete sweep means; it is never "land what we have".
                return Ok((pages, false));
            }
        }
    }
}

#[async_trait]
impl SlackRead for SlackClient {
    async fn auth_test(&self) -> Result<SlackIdentity> {
        let (response, scopes) = self
            .send::<AuthTestResponse>(SlackReadMethod::AuthTest, &[])
            .await?;
        let workspace_id = response.team_id.ok_or_else(|| {
            anyhow!("auth.test returned no team_id; refusing to guess an identity")
        })?;
        Ok(SlackIdentity {
            workspace_id,
            workspace_domain: response.url.map(|url| {
                url.trim_start_matches("https://")
                    .trim_start_matches("http://")
                    .trim_end_matches('/')
                    .to_string()
            }),
            workspace_name: response.team,
            bot_user_id: response.user_id,
            bot_id: response.bot_id,
            granted_scopes: scopes,
        })
    }

    async fn list_channels(&self, request: &ChannelListRequest) -> Result<Vec<RawChannel>> {
        let mut types = vec!["public_channel"];
        if request.include_private {
            types.push("private_channel");
        }
        // Note what is NOT requestable here: `im` and `mpim`. The deployed
        // posture has no direct messages (design 3.1 ruling) and the wire's
        // channel class enum is closed to channels, so the collector does not
        // ask for them at all rather than asking and filtering.
        let params = vec![
            ("types", types.join(",")),
            ("exclude_archived", request.exclude_archived.to_string()),
            ("limit", request.page_limit.to_string()),
        ];
        let mut channels: Vec<RawChannel> = Vec::new();
        let (_, complete) = self
            .paginate::<ConversationsListResponse, _>(
                SlackReadMethod::ConversationsList,
                &params,
                request.max_pages,
                |page| {
                    channels.extend(page.channels);
                    page.response_metadata.cursor().map(str::to_string)
                },
            )
            .await?;
        if !complete {
            // A truncated roster is reported, not silently accepted: an
            // operator whose allowlisted channel fell off the last page would
            // otherwise see an enrolled channel that never lands anything.
            tracing::warn!(
                channels = channels.len(),
                "the channel roster hit its page budget; some channels were not seen this cycle"
            );
        }
        Ok(channels)
    }

    async fn history(&self, request: &HistoryRequest) -> Result<Sweep> {
        let mut params = vec![
            ("channel", request.channel_id.clone()),
            ("limit", request.page_limit.to_string()),
            // BOTH bounds inclusive, with the caller filtering the lower
            // one. This is the only arrangement that closes the boundary hole:
            // Slack's `inclusive` flag applies to `oldest` and `latest`
            // together, so exclusive bounds would drop a message whose `ts`
            // fell exactly on a window edge -- excluded from the window that
            // ends there AND from the window that starts there. Inclusive plus
            // a caller-side filter on the resume mark lands every message
            // exactly once. [`crate::cycle`] owns that filter.
            ("inclusive", "true".to_string()),
        ];
        if let Some(oldest) = &request.oldest {
            params.push(("oldest", oldest.clone()));
        }
        if let Some(latest) = &request.latest {
            params.push(("latest", latest.clone()));
        }
        let mut messages: Vec<RawMessage> = Vec::new();
        let (pages, complete) = self
            .paginate::<ConversationsHistoryResponse, _>(
                SlackReadMethod::ConversationsHistory,
                &params,
                request.max_pages,
                |page| {
                    let has_more = page.has_more;
                    let cursor = page.response_metadata.cursor().map(str::to_string);
                    messages.extend(page.messages);
                    // `has_more` without a cursor is a vendor state we cannot
                    // page out of; treating it as done would be a silent hole,
                    // so it is surfaced as an incomplete sweep.
                    cursor.or(if has_more { Some(String::new()) } else { None })
                },
            )
            .await?;
        Ok(Sweep {
            messages,
            complete,
            pages,
        })
    }

    async fn replies(&self, request: &RepliesRequest) -> Result<Sweep> {
        let mut params = vec![
            ("channel", request.channel_id.clone()),
            ("ts", request.parent_ts.clone()),
            ("limit", request.page_limit.to_string()),
            // Inclusive for the same reason as the history sweep, plus one of
            // its own: `conversations.replies` returns the PARENT alongside the
            // replies, and the caller filters it out by `ts`. Making the bound
            // exclusive would hide a boundary reply instead.
            ("inclusive", "true".to_string()),
        ];
        if let Some(oldest) = &request.oldest {
            params.push(("oldest", oldest.clone()));
        }
        let mut messages: Vec<RawMessage> = Vec::new();
        let (pages, complete) = self
            .paginate::<ConversationsHistoryResponse, _>(
                SlackReadMethod::ConversationsReplies,
                &params,
                request.max_pages,
                |page| {
                    let has_more = page.has_more;
                    let cursor = page.response_metadata.cursor().map(str::to_string);
                    messages.extend(page.messages);
                    cursor.or(if has_more { Some(String::new()) } else { None })
                },
            )
            .await?;
        Ok(Sweep {
            messages,
            complete,
            pages,
        })
    }
}

/// Parse `Retry-After`, which Slack sends as whole seconds.
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

/// Parse the granted scope list from the response headers.
///
/// Sorted and deduplicated so the status surface is stable across calls and a
/// diff between two status reads means a real grant change.
fn parse_scopes(headers: &reqwest::header::HeaderMap) -> Vec<String> {
    let Some(value) = headers.get(GRANTED_SCOPES_HEADER) else {
        return Vec::new();
    };
    let Ok(value) = value.to_str() else {
        return Vec::new();
    };
    let mut scopes: Vec<String> = value
        .split(',')
        .map(|scope| scope.trim().to_string())
        .filter(|scope| !scope.is_empty())
        .collect();
    scopes.sort();
    scopes.dedup();
    scopes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_non_loopback_plain_http_slack_base_is_refused() {
        let error = SlackClient::new(
            "http://slack.example.com/api",
            "xoxb-fixture",
            RatePolicy::default(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("slack_api_base_url"), "{error}");
    }

    #[test]
    fn a_loopback_plain_http_base_is_allowed_for_fixtures() {
        SlackClient::new(
            "http://127.0.0.1:8080/api",
            "xoxb-fixture",
            RatePolicy::default(),
        )
        .unwrap();
    }

    #[test]
    fn an_empty_token_is_refused() {
        let error = SlackClient::new(DEFAULT_API_BASE_URL, "   ", RatePolicy::default())
            .unwrap_err()
            .to_string();
        assert!(error.contains("bot token"), "{error}");
    }

    #[test]
    fn the_debug_rendering_never_carries_the_token() {
        let client = SlackClient::new(
            DEFAULT_API_BASE_URL,
            "xoxb-super-secret",
            RatePolicy::default(),
        )
        .unwrap();
        let rendered = format!("{client:?}");
        assert!(!rendered.contains("xoxb-super-secret"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }

    #[test]
    fn every_composed_url_comes_from_the_allowlist() {
        let client =
            SlackClient::new(DEFAULT_API_BASE_URL, "xoxb-fixture", RatePolicy::default()).unwrap();
        for method in SlackReadMethod::ALL {
            let url = client.url(*method).unwrap();
            assert_eq!(
                url.as_str(),
                format!("https://slack.com/api/{}", method.api_name())
            );
        }
    }

    #[test]
    fn scopes_parse_sorted_and_deduplicated() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            GRANTED_SCOPES_HEADER,
            "chat:write,channels:history, channels:read ,chat:write"
                .parse()
                .unwrap(),
        );
        assert_eq!(
            parse_scopes(&headers),
            vec![
                "channels:history".to_string(),
                "channels:read".to_string(),
                "chat:write".to_string(),
            ]
        );
    }
}
