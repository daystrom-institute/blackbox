//! Operator-declared satellite configuration.
//!
//! One file per satellite process, naming the corpus host it publishes to, the
//! scope it publishes under, the workspace it expects to reach, the channels it
//! is allowed to observe, and how hard it is allowed to push a shared
//! credential. Deliberately `deny_unknown_fields` throughout: a typo in a
//! policy field must fail loudly at startup, because the failure it otherwise
//! produces -- silently ignoring an `exclude` rule -- is invisible until
//! messages an operator meant to exclude turn up in search results.
//!
//! Neither credential is inlined. Both are references
//! ([`crate::secret::SecretRef`]) resolved at startup.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use bbox_corpus_core::project_catalog::ConnectorScope;
use serde::Deserialize;

use crate::policy::ChannelPolicy;
use crate::secret::SecretRef;
use crate::slack::{DEFAULT_API_BASE_URL, DEFAULT_PAGE_LIMIT, RatePolicy};

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SatelliteConfig {
    /// Base URL of the corpus host, e.g. `http://127.0.0.1:7264`.
    pub corpus_url: String,
    /// The producer bearer that authenticates to the corpus host.
    pub producer_token: SecretRef,
    /// The Slack bot token. Under the ruled one-app posture this is the
    /// INTERACTIVE bot's own token (design 3.1), which is why write-safety
    /// lives in [`crate::slack::SlackReadMethod`] rather than in the grant.
    pub slack_token: SecretRef,
    /// The operator-minted opaque source id, matching a corpus-side grant on
    /// the conversation lane.
    pub connector_source_id: String,
    /// The declared connector kind, matching that grant.
    pub connector_kind: String,
    /// The workspace this satellite expects to reach.
    ///
    /// Optional, and checked against `auth.test` when set. A satellite pointed
    /// at the wrong workspace by a swapped credential would otherwise land
    /// another workspace's messages under this scope's identity, and every
    /// record it wrote would be durable and wrong.
    #[serde(default)]
    pub expected_workspace_id: Option<String>,
    /// Human-facing name presented at onboarding.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Producer working state: reconciliation baselines and thread watermarks.
    ///
    /// Losable and re-derivable by design. Deleting it costs edit and delete
    /// detection for one window and some redundant thread sweeps; it never
    /// costs corpus data, because the SERVER owns the cursor
    /// ([`crate::cycle`]).
    pub journal_path: PathBuf,
    /// The Slack API root. Overridable for fixtures; the same transport rule
    /// applies as to [`Self::corpus_url`].
    #[serde(default = "default_api_base_url")]
    pub slack_api_base_url: String,
    /// Seconds between cycles in `watch` mode.
    #[serde(default = "default_poll_interval_secs")]
    pub poll_interval_secs: u64,
    #[serde(default)]
    pub channels: ChannelPolicy,
    #[serde(default)]
    pub sweep: SweepPolicy,
    #[serde(default)]
    pub rate: RatePolicy,
    #[serde(default)]
    pub backfill: BackfillPolicy,
    #[serde(default)]
    pub reconciliation: ReconciliationPolicy,
}

fn default_api_base_url() -> String {
    DEFAULT_API_BASE_URL.to_string()
}

fn default_poll_interval_secs() -> u64 {
    120
}

/// How a forward sweep is bounded.
///
/// The window is the load-bearing field and the reason deserves stating.
/// `conversations.history` pages NEWEST first, so a sweep that ran out of page
/// budget holds the newest part of its range rather than a contiguous run from
/// the watermark. Landing that would advance the cursor over a hole that
/// nothing would ever come back for. So the sweep walks forward in bounded time
/// windows and lands a window only when that window was enumerated COMPLETELY;
/// an incomplete window is discarded and retried next cycle, leaving the
/// watermark exactly where it was.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SweepPolicy {
    /// Seconds of channel time one window covers.
    #[serde(default = "default_window_secs")]
    pub window_secs: u64,
    /// Windows one channel may advance per cycle. Bounds catch-up work so one
    /// busy channel cannot spend the whole cycle's rate budget.
    #[serde(default = "default_windows_per_cycle")]
    pub windows_per_cycle: u32,
    /// Messages per API page.
    #[serde(default = "default_page_limit")]
    pub page_limit: u32,
    /// Pages one window may consume before it is declared incomplete.
    #[serde(default = "default_max_pages_per_window")]
    pub max_pages_per_window: u32,
    /// Thread parents swept per channel per cycle.
    ///
    /// Thread sweeps multiply call count by thread CARDINALITY rather than by
    /// message count (design 5.5), so this is the number that actually governs
    /// the credential's exposure on a busy day.
    #[serde(default = "default_thread_parents_per_cycle")]
    pub thread_parents_per_cycle: u32,
    /// Pages one thread may consume.
    #[serde(default = "default_max_pages_per_thread")]
    pub max_pages_per_thread: u32,
    /// Cycles after which a quiet thread is resswept anyway.
    ///
    /// The cheap latest-reply test (design 5.3) only works while the parent is
    /// still inside a channel window the sweep re-reads: once it ages out,
    /// nothing advertises a new unbroadcast reply to anyone, and a
    /// latest-reply-only policy would never look again. So an idle thread costs
    /// nothing for this many cycles and then costs exactly one call. Set it
    /// higher to spend less rate budget and detect late thread activity slower.
    #[serde(default = "default_thread_rotation_cycles")]
    pub thread_rotation_cycles: u32,
    /// Pages the channel roster may consume.
    #[serde(default = "default_max_roster_pages")]
    pub max_roster_pages: u32,
}

fn default_window_secs() -> u64 {
    3_600
}

fn default_windows_per_cycle() -> u32 {
    6
}

fn default_page_limit() -> u32 {
    DEFAULT_PAGE_LIMIT
}

fn default_max_pages_per_window() -> u32 {
    10
}

fn default_thread_parents_per_cycle() -> u32 {
    25
}

fn default_max_pages_per_thread() -> u32 {
    5
}

fn default_thread_rotation_cycles() -> u32 {
    10
}

fn default_max_roster_pages() -> u32 {
    10
}

impl Default for SweepPolicy {
    fn default() -> Self {
        Self {
            window_secs: default_window_secs(),
            windows_per_cycle: default_windows_per_cycle(),
            page_limit: default_page_limit(),
            max_pages_per_window: default_max_pages_per_window(),
            thread_parents_per_cycle: default_thread_parents_per_cycle(),
            max_pages_per_thread: default_max_pages_per_thread(),
            thread_rotation_cycles: default_thread_rotation_cycles(),
            max_roster_pages: default_max_roster_pages(),
        }
    }
}

/// The operator-configured backfill horizon (design 5.1).
///
/// OFF by default, and that default is a policy statement rather than caution:
/// a full-history sweep of a busy workspace is a large rate-limited operation
/// against a credential an interactive bot shares, so it must be a choice
/// somebody made, not something that happens because a satellite was deployed.
// No `deny_unknown_fields` here, and that is a serde constraint rather than a
// looser contract: an internally-tagged enum deserializes through a buffering
// content path, so the strictness the rest of this file relies on does not
// compose with the tag. The variants themselves are closed, which is the part
// that matters -- an unrecognized `mode` is still a startup failure.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Default)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum BackfillHorizon {
    #[default]
    Off,
    /// Reach back this many days before the oldest record the corpus holds.
    Days { days: u32 },
    /// Reach back to an explicit message timestamp.
    Since { ts: String },
    /// Reach back to the beginning of each channel.
    All,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BackfillPolicy {
    #[serde(default)]
    pub horizon: BackfillHorizon,
    /// Windows the backfill lane may consume per channel per cycle.
    ///
    /// Its own budget, deliberately smaller than steady state's, because design
    /// 5.5 fixes lane priority as steady state, then reconciliation, then
    /// backfill, and a backfill that can starve steady state has inverted it.
    #[serde(default = "default_backfill_windows_per_cycle")]
    pub windows_per_cycle: u32,
}

fn default_backfill_windows_per_cycle() -> u32 {
    2
}

impl Default for BackfillPolicy {
    fn default() -> Self {
        Self {
            horizon: BackfillHorizon::Off,
            windows_per_cycle: default_backfill_windows_per_cycle(),
        }
    }
}

impl BackfillPolicy {
    pub fn enabled(&self) -> bool {
        !matches!(self.horizon, BackfillHorizon::Off)
    }
}

/// The trailing reconciliation window (design 5.4).
///
/// Delete detection is WINDOW-BOUNDED and this is the window. A message deleted
/// outside it is not detected and the corpus may retain text the workspace no
/// longer shows. That is a policy fact the operator accepts when enabling the
/// connector, which is why the window is configurable and why the satellite
/// reports its own window on status rather than implying full coverage.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReconciliationPolicy {
    #[serde(default = "default_reconciliation_enabled")]
    pub enabled: bool,
    /// Seconds of trailing channel time re-enumerated each cycle.
    #[serde(default = "default_reconciliation_window_secs")]
    pub window_secs: u64,
    /// Cycles between reconciliation passes. Reconciliation is a re-read of
    /// ground the sweep already covered, so it runs on a slower cadence than
    /// steady state (design 5.4).
    #[serde(default = "default_reconciliation_every_cycles")]
    pub every_cycles: u32,
}

fn default_reconciliation_enabled() -> bool {
    true
}

fn default_reconciliation_window_secs() -> u64 {
    2 * 86_400
}

fn default_reconciliation_every_cycles() -> u32 {
    5
}

impl Default for ReconciliationPolicy {
    fn default() -> Self {
        Self {
            enabled: default_reconciliation_enabled(),
            window_secs: default_reconciliation_window_secs(),
            every_cycles: default_reconciliation_every_cycles(),
        }
    }
}

impl SatelliteConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading satellite config {}", path.display()))?;
        let config: Self = toml::from_str(&text)
            .with_context(|| format!("parsing satellite config {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    /// Every startup refusal that can be decided from config alone.
    ///
    /// Decided HERE rather than at first use so a misconfigured satellite fails
    /// before it has touched a credential or opened a socket.
    pub fn validate(&self) -> Result<()> {
        self.scope()?;
        require_safe_transport(&self.corpus_url, "corpus_url")?;
        require_safe_transport(&self.slack_api_base_url, "slack_api_base_url")?;
        self.producer_token.validate("producer_token")?;
        self.slack_token.validate("slack_token")?;
        self.channels.validate()?;
        if self.sweep.window_secs == 0 {
            bail!("sweep.window_secs must be greater than zero");
        }
        if self.sweep.page_limit == 0 || self.sweep.page_limit > 1_000 {
            bail!("sweep.page_limit must be in 1..=1000");
        }
        if self.rate.max_requests_per_minute == 0 {
            bail!("rate.max_requests_per_minute must be greater than zero");
        }
        if let Some(workspace_id) = &self.expected_workspace_id
            && workspace_id.trim().is_empty()
        {
            bail!("expected_workspace_id is set but empty");
        }
        Ok(())
    }

    /// The connector scope this satellite publishes under.
    ///
    /// Parsed rather than stored so an invalid source id or kind is a startup
    /// failure with the operator's own words in it, not a 403 from the corpus
    /// host two steps later that reads like an authorization problem.
    pub fn scope(&self) -> Result<ConnectorScope> {
        ConnectorScope::try_new(&self.connector_source_id, &self.connector_kind)
            .map_err(|error| anyhow::anyhow!("invalid connector scope: {error}"))
    }

    pub fn display_name(&self) -> String {
        self.display_name
            .clone()
            .unwrap_or_else(|| self.connector_source_id.clone())
    }
}

/// Refuse a plain-HTTP URL that is not loopback.
///
/// Both wires carry a bearer, so both wires get the same rule (design 4.1:
/// non-loopback plain HTTP is refused producer-side). Loopback is permitted
/// because that is the deployed corpus posture and the fixture-test posture,
/// and refusing it would mean the tests exercised a transport the product does
/// not use.
pub fn require_safe_transport(url: &str, field: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|error| anyhow::anyhow!("{field}: {url} is not a URL: {error}"))?;
    match parsed.scheme() {
        "https" => Ok(()),
        "http" => {
            let host = parsed.host_str().unwrap_or_default();
            if is_loopback(host) {
                Ok(())
            } else {
                bail!(
                    "{field}: refusing plain HTTP to non-loopback host {host}; a bearer would \
                     cross the network in clear text"
                )
            }
        }
        scheme => bail!("{field}: unsupported URL scheme {scheme}"),
    }
}

fn is_loopback(host: &str) -> bool {
    // The bracket strip covers the `http://[::1]:7264` form, where the URL
    // parser hands back the address without brackets on some inputs and with
    // them on others depending on the source string.
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host == "localhost" {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(address) => address.is_loopback(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
corpus_url = "http://127.0.0.1:7264"
connector_source_id = "csrc_5f2c1d9a4b6e470e"
connector_kind = "slack"
journal_path = "/var/lib/bbox-slack-collector/journal.json"

[producer_token]
token_file = "/run/secrets/producer"

[slack_token]
token_secret_ref = "op://producer/slack-bot/token"
"#;

    fn parse(text: &str) -> Result<SatelliteConfig> {
        let config: SatelliteConfig = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    #[test]
    fn a_minimal_config_takes_the_conservative_defaults() {
        let config = parse(MINIMAL).unwrap();
        assert_eq!(config.slack_api_base_url, DEFAULT_API_BASE_URL);
        // Backfill OFF by default is the ruled posture, not an accident.
        assert!(!config.backfill.enabled());
        assert_eq!(config.backfill.horizon, BackfillHorizon::Off);
        // Private channels are a separate operator flag, default off.
        assert!(!config.channels.private_channels);
        assert_eq!(config.rate.max_requests_per_minute, 20);
    }

    #[test]
    fn a_non_loopback_plain_http_corpus_url_is_refused_at_startup() {
        let text = MINIMAL.replace("http://127.0.0.1:7264", "http://corpus.example.com");
        let error = parse(&text).unwrap_err().to_string();
        assert!(error.contains("corpus_url"), "{error}");
        assert!(error.contains("clear text"), "{error}");
    }

    #[test]
    fn https_to_a_remote_corpus_is_accepted() {
        let text = MINIMAL.replace("http://127.0.0.1:7264", "https://corpus.example.com");
        parse(&text).unwrap();
    }

    #[test]
    fn loopback_ipv6_is_loopback() {
        require_safe_transport("http://[::1]:7264", "corpus_url").unwrap();
        require_safe_transport("http://localhost:7264", "corpus_url").unwrap();
    }

    #[test]
    fn an_unknown_field_is_a_startup_failure() {
        let text = format!("{MINIMAL}\nunexpected_field = true\n");
        assert!(toml::from_str::<SatelliteConfig>(&text).is_err());
    }

    #[test]
    fn a_backfill_horizon_is_explicit_when_it_is_set() {
        let text = format!("{MINIMAL}\n[backfill.horizon]\nmode = \"days\"\ndays = 30\n");
        let config = parse(&text).unwrap();
        assert!(config.backfill.enabled());
        assert_eq!(config.backfill.horizon, BackfillHorizon::Days { days: 30 });
    }

    #[test]
    fn an_invalid_scope_fails_before_any_socket_opens() {
        let text = MINIMAL.replace("csrc_5f2c1d9a4b6e470e", "not a source id");
        let error = parse(&text).unwrap_err().to_string();
        assert!(error.contains("connector scope"), "{error}");
    }
}
