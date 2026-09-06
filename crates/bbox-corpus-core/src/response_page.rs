//! Pure response shaping for bounded collection discovery.

use anyhow::{Context, Result};
use serde_json::{Value, json};

/// Leave headroom below the transport safeguard for the MCP envelope.
pub const PAGE_BUDGET_BYTES: usize = 24 * 1024;

/// Truncate a display field at a UTF-8 boundary and mark the omitted suffix.
/// Stable ids used for exact reads must not be passed to this helper.
pub fn preview_field(row: &mut Value, field: &str, max_bytes: usize) {
    let Some(value) = row[field].as_str() else {
        return;
    };
    if value.len() <= max_bytes {
        return;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    let preview = value[..end].to_owned();
    row[field] = json!(preview);
    row[format!("{field}_truncated")] = json!(true);
}

/// Fit a complete JSON page within the discovery budget. The array is already
/// filtered and ordered; omitted tail rows remain reachable through next_offset.
/// An oversized first row fails explicitly rather than returning a nonadvancing
/// page or skipping data. Expanded detail can then be retried as a summary.
pub fn bound_page(mut page: Value, field: &str) -> Result<Value> {
    let offset = page["offset"].as_u64().context("page offset missing")?;
    let total = page["total"].as_u64().context("page total missing")?;
    let rows = page[field].as_array_mut().context("page rows missing")?;
    let candidates = std::mem::take(rows);
    page["byte_limited"] = json!(true);
    // A null continuation can become a 20-digit offset after cutting rows.
    let mut bytes = serde_json::to_vec(&page)?.len().saturating_add(32);
    if bytes > PAGE_BUDGET_BYTES {
        anyhow::bail!(
            "error.collection_metadata_too_large: page metadata exceeds the response budget"
        );
    }
    let mut selected = Vec::new();
    let mut limited = false;
    for row in candidates {
        let additional = serde_json::to_vec(&row)?.len() + usize::from(!selected.is_empty());
        if bytes.saturating_add(additional) > PAGE_BUDGET_BYTES {
            if selected.is_empty() {
                anyhow::bail!(
                    "error.collection_row_too_large: one {field} row exceeds the response budget; use this tool's documented summary projection or exact-read options"
                );
            }
            limited = true;
            break;
        }
        bytes += additional;
        selected.push(row);
    }
    let returned = selected.len();
    let next = offset.saturating_add(returned as u64);
    page["next_offset"] = json!((next < total).then_some(next));
    if page.get("count").is_some() {
        page["count"] = json!(returned);
    }
    page[field] = json!(selected);
    if !limited {
        page.as_object_mut().unwrap().remove("byte_limited");
    }
    Ok(page)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_budget_counts_escaped_unicode_and_nested_detail_without_skipping() {
        let rows: Vec<_> = (0..100)
            .map(|id| json!({"id": id, "nested": {"body": "界\n\"".repeat(100)}}))
            .collect();
        let first = bound_page(
            json!({"items": rows, "offset": 7, "total": 107, "count": 100, "next_offset": null}),
            "items",
        )
        .unwrap();
        let count = first["items"].as_array().unwrap().len();
        assert!(count > 0 && count < 100);
        assert!(serde_json::to_vec(&first).unwrap().len() <= PAGE_BUDGET_BYTES);
        assert_eq!(first["next_offset"], 7 + count);
        assert_eq!(first["count"], count);
        assert_eq!(first["items"][count - 1]["id"], count - 1);
        assert_eq!(first["byte_limited"], true);
        let error = bound_page(json!({"items": [{"body": "private-payload".repeat(PAGE_BUDGET_BYTES)}], "offset": 0, "total": 1}), "items").unwrap_err();
        assert!(!error.to_string().contains("private-payload"));
    }

    #[test]
    fn display_preview_respects_unicode_boundaries_and_marks_omission() {
        let mut row = json!({"name": "界".repeat(100)});
        preview_field(&mut row, "name", 200);
        assert_eq!(row["name"].as_str().unwrap().len(), 198);
        assert_eq!(row["name_truncated"], true);
    }
}
