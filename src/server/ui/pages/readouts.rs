//! Small renderers shared by the projects, system and configuration pages:
//! state labels, relative ages, byte sizes and key/value tables.

use std::time::{SystemTime, UNIX_EPOCH};

use maud::{Markup, html};

/// A health or state verdict: the `.state-*` class beside its text label, so
/// color is never the only signal.
pub(crate) fn state_label(class: &str, label: &str) -> Markup {
    html! { span class=(format!("state state-{class}")) { (label) } }
}

/// A unix timestamp as relative age, with the absolute UTC value on hover.
pub(crate) fn unix_age(unix_secs: u64) -> Markup {
    match chrono::DateTime::<chrono::Utc>::from_timestamp(unix_secs as i64, 0) {
        Some(at) => timestamp(at),
        None => html! { (unix_secs) },
    }
}

/// An RFC 3339 timestamp as relative age; text that does not parse renders
/// verbatim.
pub(crate) fn rfc3339_age(raw: &str) -> Markup {
    match chrono::DateTime::parse_from_rfc3339(raw) {
        Ok(at) => timestamp(at.with_timezone(&chrono::Utc)),
        Err(_) => html! { (raw) },
    }
}

fn timestamp(at: chrono::DateTime<chrono::Utc>) -> Markup {
    let absolute = at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default();
    let delta = now - at.timestamp();
    let relative = if delta >= 0 {
        format!("{} ago", span(delta.unsigned_abs()))
    } else {
        format!("in {}", span(delta.unsigned_abs()))
    };
    html! { time datetime=(absolute) title=(absolute) { (relative) } }
}

/// A duration as its most significant unit.
pub(crate) fn span(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3_600 => format!("{}m", secs / 60),
        3_600..86_400 => format!("{}h", secs / 3_600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// A byte count in binary units, with the exact count on hover.
pub(crate) fn bytes(count: u64) -> Markup {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = count as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    let text = if unit == 0 {
        format!("{count} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    };
    html! { span title=(format!("{count} bytes")) { (text) } }
}

/// A two-column table of labelled values.
pub(crate) fn kv_table(rows: Vec<(String, Markup)>) -> Markup {
    html! {
        table.kv-table {
            tbody {
                @for (key, value) in rows {
                    tr { th scope="row" { (key) } td { (value) } }
                }
            }
        }
    }
}

/// The source line under a panel.
pub(crate) fn source(reader: &str) -> Markup {
    html! { p.panel-source { "Source: " code { (reader) } } }
}

/// A serde enum's wire label (its serialized string), or `unknown`.
pub(crate) fn serde_label<T: serde::Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(label)) => label,
        _ => "unknown".to_string(),
    }
}

/// Flatten a JSON value into dotted key paths and scalar text, in key order.
pub(crate) fn flatten_json(prefix: &str, value: &serde_json::Value) -> Vec<(String, String)> {
    let mut rows = Vec::new();
    flatten_into(prefix, value, &mut rows);
    rows
}

fn flatten_into(prefix: &str, value: &serde_json::Value, rows: &mut Vec<(String, String)>) {
    let join = |key: &str| {
        if prefix.is_empty() {
            key.to_string()
        } else {
            format!("{prefix}.{key}")
        }
    };
    match value {
        serde_json::Value::Object(fields) => {
            for (key, inner) in fields {
                flatten_into(&join(key), inner, rows);
            }
        }
        serde_json::Value::Array(items) => {
            for (index, inner) in items.iter().enumerate() {
                flatten_into(&format!("{prefix}[{index}]"), inner, rows);
            }
        }
        serde_json::Value::Null => rows.push((prefix.to_string(), "unset".to_string())),
        serde_json::Value::String(text) => rows.push((prefix.to_string(), text.clone())),
        other => rows.push((prefix.to_string(), other.to_string())),
    }
}

/// `bbox_stats` text as key/value rows: one `key: value` per line.
pub(crate) fn stats_rows(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| match line.split_once(": ") {
            Some((key, value)) => (key.to_string(), value.to_string()),
            None => (line.to_string(), String::new()),
        })
        .collect()
}

/// Router requests for page tests: a loopback peer with a forwarded identity.
#[cfg(test)]
pub(crate) mod test_support {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::Arc;

    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use super::super::super::{UiState, build_ui_router};
    use crate::server::SharedState;

    pub(crate) const PRINCIPAL: &str = "operator@example.test";

    pub(crate) async fn get(shared: Arc<SharedState>, uri: &str) -> (StatusCode, String) {
        let request = Request::builder()
            .uri(uri)
            .extension(ConnectInfo(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                53100,
            )))
            .header("x-forwarded-email", PRINCIPAL)
            .body(Body::empty())
            .unwrap();
        let response = build_ui_router(UiState::new(shared))
            .oneshot(request)
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
            .await
            .unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_and_sizes_use_their_most_significant_unit() {
        assert_eq!(span(59), "59s");
        assert_eq!(span(61), "1m");
        assert_eq!(span(7_200), "2h");
        assert_eq!(span(172_800), "2d");
        assert!(bytes(512).into_string().contains("512 B"));
        let mib = bytes(3 * 1024 * 1024).into_string();
        assert!(
            mib.contains("3.0 MiB") && mib.contains("3145728 bytes"),
            "{mib}"
        );
    }

    #[test]
    fn flattened_json_keeps_paths_and_marks_null_unset() {
        let rows = flatten_json(
            "",
            &serde_json::json!({"a": {"b": 1, "c": null}, "d": ["x"]}),
        );
        assert_eq!(
            rows,
            vec![
                ("a.b".to_string(), "1".to_string()),
                ("a.c".to_string(), "unset".to_string()),
                ("d[0]".to_string(), "x".to_string()),
            ]
        );
    }
}
