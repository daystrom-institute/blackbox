//! Rate discipline for a credential this process does not own alone.
//!
//! Design 5.5 asks for one token bucket per workspace credential, honored
//! `Retry-After` on every 429, and no parallelism beyond the workspace budget.
//! The ruled one-app posture (design 3.1) adds a constraint that outranks all
//! three: the interactive bot draws on the SAME bucket, from a different
//! process, and a human waiting on a mention response notices a delay that
//! nobody notices in a corpus.
//!
//! So this pacer is not a fair share of the budget. It is a deliberately small
//! minority of it, and the cross-process bucket that would let the two
//! processes negotiate properly is later work (design 3.1 names it as the S3
//! workspace token bucket). Until that exists, being a polite minority consumer
//! is the whole strategy: `conversations.history` and `conversations.replies`
//! sit in Slack's Tier 3 band around 50 requests per minute, and the default
//! here is 20, with no burst allowance and no parallelism at all.
//!
//! The pacer is a minimum-interval gate rather than a refilling bucket on
//! purpose. A bucket permits a burst by construction, and a burst is exactly
//! what steals the interactive process's headroom at the moment a human is
//! waiting.

use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;

/// Operator-tunable rate discipline.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RatePolicy {
    /// Requests per minute this satellite permits itself.
    ///
    /// Well under the vendor band on purpose; see the module note. Raising it
    /// toward the real ceiling is an operator act with a cost paid by the
    /// interactive bot, not by this process.
    #[serde(default = "default_requests_per_minute")]
    pub max_requests_per_minute: u32,
    /// Attempts per request, INCLUDING the first. `1` disables retry entirely.
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    /// The floor under every backoff, in seconds.
    ///
    /// This is what makes "never retries tightly" structural rather than
    /// hopeful: a 429 carrying `Retry-After: 0`, or carrying no `Retry-After`
    /// at all, still waits at least this long.
    #[serde(default = "default_min_backoff_secs")]
    pub min_backoff_secs: u64,
    /// The ceiling over every backoff, in seconds. A vendor asking for an
    /// implausible wait gets capped, and the cycle ends rather than parking a
    /// process for an hour.
    #[serde(default = "default_max_backoff_secs")]
    pub max_backoff_secs: u64,
}

fn default_requests_per_minute() -> u32 {
    20
}

fn default_max_attempts() -> u32 {
    4
}

fn default_min_backoff_secs() -> u64 {
    1
}

fn default_max_backoff_secs() -> u64 {
    60
}

impl Default for RatePolicy {
    fn default() -> Self {
        Self {
            max_requests_per_minute: default_requests_per_minute(),
            max_attempts: default_max_attempts(),
            min_backoff_secs: default_min_backoff_secs(),
            max_backoff_secs: default_max_backoff_secs(),
        }
    }
}

impl RatePolicy {
    /// The minimum spacing between two requests.
    pub fn min_interval(&self) -> Duration {
        let per_minute = self.max_requests_per_minute.max(1);
        Duration::from_secs_f64(60.0 / f64::from(per_minute))
    }

    /// How long to wait after a throttled response.
    ///
    /// `retry_after` is the vendor's own number when it sent one. It is
    /// clamped into `[min_backoff, max_backoff]`, which is where "never retries
    /// tightly" and "never parks forever" both live.
    pub fn backoff(&self, retry_after: Option<Duration>, attempt: u32) -> Duration {
        let floor = Duration::from_secs(self.min_backoff_secs);
        let ceiling = Duration::from_secs(self.max_backoff_secs.max(self.min_backoff_secs));
        let requested = match retry_after {
            Some(retry_after) => retry_after,
            // No `Retry-After` means the vendor did not say, so back off on our
            // own escalating schedule rather than hammering at the floor.
            None => floor.saturating_mul(1_u32 << attempt.min(5)),
        };
        requested.clamp(floor, ceiling)
    }
}

/// A minimum-interval gate over one credential.
#[derive(Debug)]
pub struct Pacer {
    interval: Duration,
    /// The earliest instant the next request may leave. `Mutex` rather than an
    /// async lock because the guard is never held across an await: the slot is
    /// claimed, the guard drops, and only then does the caller sleep.
    next_slot: Mutex<Option<Instant>>,
}

impl Pacer {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            next_slot: Mutex::new(None),
        }
    }

    /// Claim the next slot and wait for it.
    pub async fn acquire(&self) {
        let wait = {
            let now = Instant::now();
            let mut slot = self.next_slot.lock().expect("pacer mutex poisoned");
            let at = match *slot {
                Some(at) if at > now => at,
                _ => now,
            };
            *slot = Some(at + self.interval);
            at.saturating_duration_since(now)
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }

    /// Push every future slot out by `delay`, after a throttled response.
    ///
    /// Called on a 429 so the backoff applies to the whole credential rather
    /// than only to the request that was refused. Without this, a throttled
    /// sweep would wait politely and then immediately fire the next channel's
    /// request into the same closed window.
    pub fn penalize(&self, delay: Duration) {
        let target = Instant::now() + delay;
        let mut slot = self.next_slot.lock().expect("pacer mutex poisoned");
        *slot = Some(match *slot {
            Some(at) if at > target => at,
            _ => target,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_budget_is_a_minority_of_the_vendor_band() {
        // Tier 3 is roughly 50 requests per minute for the history methods and
        // the interactive bot shares it. If this default ever creeps toward the
        // ceiling, this test is the argument it has to answer.
        let policy = RatePolicy::default();
        assert!(policy.max_requests_per_minute <= 25);
        assert_eq!(policy.min_interval(), Duration::from_secs(3));
    }

    #[test]
    fn a_zero_retry_after_still_waits() {
        let policy = RatePolicy::default();
        assert_eq!(
            policy.backoff(Some(Duration::ZERO), 0),
            Duration::from_secs(policy.min_backoff_secs)
        );
    }

    #[test]
    fn a_vendor_retry_after_is_honored_within_the_ceiling() {
        let policy = RatePolicy::default();
        assert_eq!(policy.backoff(Some(Duration::from_secs(7)), 0), Duration::from_secs(7));
        assert_eq!(
            policy.backoff(Some(Duration::from_secs(3_600)), 0),
            Duration::from_secs(policy.max_backoff_secs)
        );
    }

    #[test]
    fn a_missing_retry_after_escalates_rather_than_repeating_the_floor() {
        let policy = RatePolicy::default();
        let first = policy.backoff(None, 0);
        let third = policy.backoff(None, 2);
        assert!(third > first, "{third:?} must exceed {first:?}");
    }
}
