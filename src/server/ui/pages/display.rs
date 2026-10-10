//! Display pieces shared by the threads and bros pages: timestamps as a
//! relative age with the absolute value on hover, URL encoding for the ids
//! and filter values pages put into links, and SSE-safe fragment text.

use chrono::{DateTime, SecondsFormat, Utc};
use maud::{Markup, html};

/// A relative age for `seconds` elapsed.
pub(crate) fn age_label(seconds: i64) -> String {
    match seconds {
        s if s < 5 => "just now".to_string(),
        s if s < 60 => format!("{s}s ago"),
        s if s < 3_600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3_600),
        s => format!("{}d ago", s / 86_400),
    }
}

/// A timestamp rendered as its relative age, with the absolute UTC value in
/// the `title` attribute and the machine form in `datetime`.
pub(crate) fn age(at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Markup {
    match at {
        Some(at) => html! {
            time datetime=(at.to_rfc3339_opts(SecondsFormat::Secs, true))
                title=(at.format("%Y-%m-%d %H:%M:%S UTC").to_string()) {
                (age_label((now - at).num_seconds()))
            }
        },
        None => html! { span.quiet { "unknown" } },
    }
}

/// An RFC 3339 timestamp as stored by the thread store.
pub(crate) fn parse_rfc3339(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

/// A wall-clock millisecond stamp as stored on tasks.
pub(crate) fn from_millis(ms: u64) -> Option<DateTime<Utc>> {
    i64::try_from(ms)
        .ok()
        .and_then(DateTime::from_timestamp_millis)
}

/// Percent-encode everything outside the RFC 3986 unreserved set, for a
/// path segment or a query value.
pub(crate) fn encode_component(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// At most `max` characters of `text`, marked when cut.
pub(crate) fn preview(text: &str, max: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        format!("{head}...")
    } else {
        head
    }
}

/// Rendered markup as an SSE data payload. A carriage return cannot travel
/// in an SSE field, so it is dropped; line feeds split into data lines and
/// the browser joins them back.
pub(crate) fn sse_html(markup: Markup) -> String {
    markup.into_string().replace('\r', "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages_use_the_largest_whole_unit() {
        assert_eq!(age_label(-3), "just now");
        assert_eq!(age_label(42), "42s ago");
        assert_eq!(age_label(125), "2m ago");
        assert_eq!(age_label(7_300), "2h ago");
        assert_eq!(age_label(200_000), "2d ago");
    }

    #[test]
    fn age_markup_carries_the_absolute_time_on_hover() {
        let at = parse_rfc3339("2026-01-02T03:04:05Z").unwrap();
        let now = at + chrono::Duration::seconds(90);
        let markup = age(Some(at), now).into_string();
        assert!(
            markup.contains("title=\"2026-01-02 03:04:05 UTC\""),
            "{markup}"
        );
        assert!(markup.contains("datetime=\"2026-01-02T03:04:05Z\""));
        assert!(markup.contains("1m ago"));
        assert_eq!(from_millis(1_767_323_045_000), Some(at));
    }

    #[test]
    fn components_encode_reserved_bytes() {
        assert_eq!(encode_component("thread-1a2b"), "thread-1a2b");
        assert_eq!(encode_component("/work/a b?c"), "%2Fwork%2Fa%20b%3Fc");
        assert_eq!(encode_component("é"), "%C3%A9");
    }

    #[test]
    fn previews_mark_a_cut_and_sse_payloads_drop_carriage_returns() {
        assert_eq!(preview("abcdef", 3), "abc...");
        assert_eq!(preview("abc", 3), "abc");
        assert_eq!(sse_html(html! { p { "a\r\nb" } }), "<p>a\nb</p>");
    }
}
