//! Exact JSON bodies for explicit tool detail reads. Callers own visibility,
//! selection, and stable ordering before entering this transport-only helper.

pub(super) use bbox_corpus_core::response_page::json_body_page;

#[cfg(test)]
use serde_json::{Value, json};

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
