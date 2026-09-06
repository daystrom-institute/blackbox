//! Pure response shaping for bounded collection discovery.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

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

/// Page an already filtered and deterministically ordered collection.
/// Producers own projections and exact-read semantics; this helper owns caps.
pub fn collection_page(
    rows: Vec<Value>,
    field: &str,
    limit: Option<usize>,
    offset: Option<usize>,
) -> Result<Value> {
    let total = rows.len();
    let offset = offset.unwrap_or(0);
    let limit = limit.unwrap_or(20).clamp(1, 100);
    let rows: Vec<_> = rows.into_iter().skip(offset).take(limit).collect();
    let mut page = json!({"offset": offset, "limit": limit, "total": total, "count": rows.len()});
    page[field] = json!(rows);
    bound_page(page, field)
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

/// Page one exact JSON value, binding continuation to selection and content.
pub fn json_body_page(
    scope: &str,
    value: &Value,
    cursor: Option<&str>,
    limit: Option<usize>,
) -> Result<Value> {
    let text = serde_json::to_string(value)?;
    let mut hash = Sha256::new();
    hash.update(scope.as_bytes());
    hash.update([0]);
    hash.update(text.as_bytes());
    let revision = format!("{:x}", hash.finalize());
    let offset = match cursor {
        None => 0,
        Some(cursor) => {
            let (expected, offset) = cursor
                .split_once(':')
                .ok_or_else(|| anyhow::anyhow!("invalid cursor; use body.next_cursor"))?;
            if expected != revision {
                bail!("evidence or selection changed; restart without cursor");
            }
            offset.parse::<usize>()?
        }
    };
    if offset > text.len() || !text.is_char_boundary(offset) {
        bail!("invalid cursor byte boundary; restart without cursor");
    }
    let mut budget = limit.unwrap_or(4096).clamp(4, 4096);
    loop {
        let mut end = offset.saturating_add(budget).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let mut body = json!({"text":&text[offset..end], "format":"json", "offset":offset, "total_bytes":text.len()});
        if end < text.len() {
            body["next_cursor"] = json!(format!("{revision}:{end}"));
        }
        // Account for JSON escaping as well as raw UTF-8. The outer MCP
        // envelope may mirror the exact body in text and structured content.
        if serde_json::to_vec(&body)?.len() <= 4096 || budget == 4 {
            return Ok(body);
        }
        budget = (budget / 2).max(4);
    }
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
