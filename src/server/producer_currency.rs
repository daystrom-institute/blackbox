//! Producer-reported currency: what each code-source producer last told the
//! daemon about each project it serves.
//!
//! A collector probes the daemon for every configured project on every pass,
//! before it walks anything: the code lane sends the scanned descriptor (its
//! HEAD) and the history lane sends the resolved history HEAD. Those probes,
//! and the upload begins that follow a probe miss, are the producer's reports.
//! This runtime keeps, per producer and project, the last contact time and the
//! last reported code and history HEAD, plus each producer's reported pass
//! interval. Doctor derives currency from these reports and the served state;
//! it never reads a checkout, and content age plays no part.
//!
//! The record is in memory: after a restart every project awaits its first
//! report, which a live collector delivers within one pass.

use std::collections::BTreeMap;

use parking_lot::Mutex;

/// A producer that reports no interval is judged by the collector default.
pub(crate) const DEFAULT_PRODUCER_INTERVAL_SECS: u64 =
    bbox_code_source::DEFAULT_COLLECTOR_INTERVAL_SECS;
/// A report is stale, and an unserved HEAD is behind, after this many of the
/// producer's intervals.
pub(crate) const CURRENCY_BOUND_INTERVALS: u64 = 3;
/// The bound never drops below this, which covers a collector's failure
/// backoff (capped at fifteen minutes) twice over.
pub(crate) const CURRENCY_BOUND_FLOOR_SECS: u64 = 30 * 60;

/// The staleness and convergence bound for a producer reporting every
/// `interval_secs`.
pub(crate) fn currency_bound_secs(interval_secs: u64) -> u64 {
    interval_secs
        .max(1)
        .saturating_mul(CURRENCY_BOUND_INTERVALS)
        .max(CURRENCY_BOUND_FLOOR_SECS)
}

/// The last HEAD a producer reported for one lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReportedHead {
    pub(crate) head: String,
    pub(crate) reported_secs: u64,
    /// When a report first found its HEAD not served, since the last report
    /// that found it served. `None` while every report has been served.
    pub(crate) unserved_since: Option<u64>,
}

impl ReportedHead {
    fn update(previous: Option<&Self>, head: &str, served: bool, now: u64) -> Self {
        let unserved_since = if served {
            None
        } else {
            Some(
                previous
                    .and_then(|previous| previous.unserved_since)
                    .unwrap_or(now),
            )
        };
        Self {
            head: head.to_string(),
            reported_secs: now,
            unserved_since,
        }
    }

    /// How long the reported HEAD has gone unserved, as of `now`.
    pub(crate) fn unserved_for(&self, now: u64) -> u64 {
        now.saturating_sub(self.unserved_since.unwrap_or(self.reported_secs))
    }
}

/// Producer and project, the key of every per-project report.
pub(crate) type ProducerProject = (String, String);

#[derive(Debug, Clone, Default)]
pub(crate) struct ProducerCurrencySnapshot {
    /// When this daemon started recording.
    pub(crate) started_secs: u64,
    /// producer -> the pass interval it last reported.
    pub(crate) intervals: BTreeMap<String, u64>,
    /// (producer, project) -> last contact.
    pub(crate) contacts: BTreeMap<ProducerProject, u64>,
    /// (producer, project) -> last reported code HEAD.
    pub(crate) code_heads: BTreeMap<ProducerProject, ReportedHead>,
    /// (producer, repository history) -> last reported history HEAD.
    pub(crate) history_heads: BTreeMap<ProducerProject, ReportedHead>,
}

impl ProducerCurrencySnapshot {
    /// The bound for `producer_id`, from its reported interval or the
    /// collector default.
    pub(crate) fn bound_secs(&self, producer_id: &str) -> u64 {
        currency_bound_secs(self.interval_secs(producer_id))
    }

    pub(crate) fn interval_secs(&self, producer_id: &str) -> u64 {
        self.intervals
            .get(producer_id)
            .copied()
            .unwrap_or(DEFAULT_PRODUCER_INTERVAL_SECS)
    }
}

pub(crate) struct ProducerCurrencyRuntime {
    reports: Mutex<ProducerCurrencySnapshot>,
}

impl ProducerCurrencyRuntime {
    pub(crate) fn new() -> Self {
        Self::started_at(now_secs())
    }

    pub(crate) fn started_at(started_secs: u64) -> Self {
        Self {
            reports: Mutex::new(ProducerCurrencySnapshot {
                started_secs,
                ..Default::default()
            }),
        }
    }

    pub(crate) fn record_interval(&self, producer_id: &str, interval_secs: u64) {
        self.reports
            .lock()
            .intervals
            .insert(producer_id.to_string(), interval_secs.max(1));
    }

    /// Record a code-lane report of `head` for `project_id`; `served` says
    /// whether the active generation already serves that HEAD.
    pub(crate) fn record_code_report(
        &self,
        producer_id: &str,
        project_id: &str,
        head: &str,
        served: bool,
        now: u64,
    ) {
        let key = (producer_id.to_string(), project_id.to_string());
        let mut reports = self.reports.lock();
        reports.contacts.insert(key.clone(), now);
        let next = ReportedHead::update(reports.code_heads.get(&key), head, served, now);
        reports.code_heads.insert(key, next);
    }

    /// Record a history-lane report of `head` for `project_id`, a member of
    /// `repo_history_id`; `served` says whether the served history overlay is
    /// already at that HEAD.
    pub(crate) fn record_history_report(
        &self,
        producer_id: &str,
        project_id: &str,
        repo_history_id: &str,
        head: &str,
        served: bool,
        now: u64,
    ) {
        let mut reports = self.reports.lock();
        reports
            .contacts
            .insert((producer_id.to_string(), project_id.to_string()), now);
        let key = (producer_id.to_string(), repo_history_id.to_string());
        let next = ReportedHead::update(reports.history_heads.get(&key), head, served, now);
        reports.history_heads.insert(key, next);
    }

    pub(crate) fn snapshot(&self) -> ProducerCurrencySnapshot {
        self.reports.lock().clone()
    }
}

pub(crate) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bound_is_three_intervals_with_a_thirty_minute_floor() {
        assert_eq!(currency_bound_secs(120), CURRENCY_BOUND_FLOOR_SECS);
        assert_eq!(currency_bound_secs(0), CURRENCY_BOUND_FLOOR_SECS);
        assert_eq!(currency_bound_secs(3_600), 3 * 3_600);
        // A collector that predates the interval report is judged at the
        // collector default.
        let runtime = ProducerCurrencyRuntime::started_at(0);
        assert_eq!(
            runtime.snapshot().bound_secs("legacy"),
            currency_bound_secs(DEFAULT_PRODUCER_INTERVAL_SECS)
        );
        runtime.record_interval("slow", 7_200);
        assert_eq!(runtime.snapshot().bound_secs("slow"), 3 * 7_200);
    }

    /// The unserved clock starts at the first report that finds its HEAD not
    /// served, survives HEAD moves, and resets only when a report is served.
    #[test]
    fn unserved_time_runs_from_the_first_unserved_report_until_one_is_served() {
        let runtime = ProducerCurrencyRuntime::started_at(0);
        runtime.record_code_report("producer", "p_a", "aaa", true, 10);
        let key = ("producer".to_string(), "p_a".to_string());
        assert_eq!(runtime.snapshot().code_heads[&key].unserved_since, None);
        assert_eq!(runtime.snapshot().contacts[&key], 10);

        runtime.record_code_report("producer", "p_a", "bbb", false, 20);
        runtime.record_code_report("producer", "p_a", "ccc", false, 30);
        let report = runtime.snapshot().code_heads[&key].clone();
        assert_eq!(report.head, "ccc");
        assert_eq!(report.unserved_since, Some(20));
        assert_eq!(report.unserved_for(50), 30);

        runtime.record_code_report("producer", "p_a", "ccc", true, 40);
        assert_eq!(runtime.snapshot().code_heads[&key].unserved_since, None);
        assert_eq!(runtime.snapshot().contacts[&key], 40);
    }

    #[test]
    fn a_history_report_is_contact_for_the_project_and_head_for_the_repository() {
        let runtime = ProducerCurrencyRuntime::started_at(0);
        runtime.record_history_report("producer", "p_member", "rh_one", "abc", false, 5);
        let reports = runtime.snapshot();
        assert_eq!(
            reports.contacts[&("producer".to_string(), "p_member".to_string())],
            5
        );
        let head = &reports.history_heads[&("producer".to_string(), "rh_one".to_string())];
        assert_eq!(head.head, "abc");
        assert_eq!(head.unserved_since, Some(5));
        assert!(reports.code_heads.is_empty());
    }
}
