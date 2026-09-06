//! Exact JSON bodies for explicit tool detail reads. Callers own visibility,
//! selection, and stable ordering before entering this transport-only helper.

use anyhow::{Result, bail};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub(super) fn json_body_page(
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
    fn exact_json_pages_bound_escaping_and_bind_scope_and_revision() {
        let source = json!({"evidence":"\u{0001}🦀\n".repeat(5000)});
        let mut reconstructed = String::new();
        let mut cursor: Option<String> = None;
        loop {
            let body = json_body_page("board-a:agent-a", &source, cursor.as_deref(), None).unwrap();
            assert!(serde_json::to_vec(&body).unwrap().len() <= 4096);
            reconstructed.push_str(body["text"].as_str().unwrap());
            cursor = body["next_cursor"].as_str().map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(
            serde_json::from_str::<Value>(&reconstructed).unwrap(),
            source
        );
        let first = json_body_page("board-a:agent-a", &source, None, None).unwrap();
        let cursor = first["next_cursor"].as_str();
        assert!(json_body_page("board-a:agent-b", &source, cursor, None).is_err());
        assert!(
            json_body_page(
                "board-a:agent-a",
                &json!({"evidence":"changed"}),
                cursor,
                None
            )
            .is_err()
        );
    }
}
