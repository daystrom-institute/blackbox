//! Advisory provider peak windows, independent of quota availability and scoring.
//!
//! Schedules verified against https://docs.z.ai/devpack/overview and
//! https://api-docs.deepseek.com/quick_start/pricing/ on 2026-09-06.
//! Rates and promotions are deliberately not inferred from these windows.

use chrono::{DateTime, Datelike, Timelike, Utc, Weekday};

use super::providers::Provider;

/// Evaluate at an explicit Unix millisecond timestamp. Unknown schedules and
/// unrepresentable timestamps have no advisory, rather than implying off-peak.
/// Intervals include their start and exclude their end. Singapore has no DST.
pub fn peak_usage(provider: Provider, at_ms: u64) -> Option<bool> {
    let at = DateTime::<Utc>::from_timestamp_millis(i64::try_from(at_ms).ok()?)?;
    let (local, windows): (_, &[(u32, u32)]) = match provider {
        Provider::Glm => (
            at.checked_add_signed(chrono::Duration::hours(8))?,
            &[(14, 18)],
        ),
        Provider::Deepseek => (at, &[(1, 4), (6, 10)]),
        _ => return None,
    };
    Some(
        !matches!(local.weekday(), Weekday::Sat | Weekday::Sun)
            && windows
                .iter()
                .any(|&(start, end)| (start..end).contains(&local.hour())),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(provider: Provider, timestamp: &str) -> Option<bool> {
        peak_usage(
            provider,
            timestamp
                .parse::<DateTime<Utc>>()
                .unwrap()
                .timestamp_millis() as u64,
        )
    }

    #[test]
    fn glm_weekday_boundaries_in_singapore() {
        for (timestamp, expected) in [
            ("2026-09-07T05:59:59.999Z", false),
            ("2026-09-07T06:00:00Z", true),
            ("2026-09-07T09:59:59.999Z", true),
            ("2026-09-07T10:00:00Z", false),
            ("2026-09-11T06:00:00Z", true),
            ("2026-09-12T06:00:00Z", false),
            ("2026-09-13T06:00:00Z", false),
            ("2026-09-13T16:00:00Z", false),
        ] {
            assert_eq!(at(Provider::Glm, timestamp), Some(expected), "{timestamp}");
        }
    }

    #[test]
    fn deepseek_split_windows_and_weekends() {
        for (timestamp, expected) in [
            ("2026-09-07T00:59:59.999Z", false),
            ("2026-09-07T01:00:00Z", true),
            ("2026-09-07T03:59:59.999Z", true),
            ("2026-09-07T04:00:00Z", false),
            ("2026-09-07T05:59:59.999Z", false),
            ("2026-09-07T06:00:00Z", true),
            ("2026-09-07T09:59:59.999Z", true),
            ("2026-09-07T10:00:00Z", false),
            ("2026-09-11T01:00:00Z", true),
            ("2026-09-12T01:00:00Z", false),
            ("2026-09-13T06:00:00Z", false),
        ] {
            assert_eq!(
                at(Provider::Deepseek, timestamp),
                Some(expected),
                "{timestamp}"
            );
        }
    }

    #[test]
    fn unknown_schedule_and_invalid_time_are_not_off_peak() {
        assert_eq!(at(Provider::Brodex, "2026-09-07T06:00:00Z"), None);
        assert_eq!(peak_usage(Provider::Glm, u64::MAX), None);
    }
}
