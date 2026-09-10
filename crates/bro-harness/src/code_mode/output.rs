//! Host-side shaping of code-mode responses. The vendored runtime collects
//! output; the host owns budgets, lifecycle metadata, and transport support.

use bro_code_mode::{FunctionCallOutputContentItem, RuntimeResponse};
use bro_tools::ToolResult;

pub(super) const MAX_RESULT_BYTES: usize = 12 * 1024;
const DIAGNOSTIC_BYTES: usize = 2 * 1024;
const DEFAULT_OUTPUT_TOKENS: usize = 10_000;
const TRUNCATION_MARKER: &str =
    "\n[output truncated; narrow the source read or print a smaller selection]\n";
const IMAGE_ERROR: &str = "image() output is unsupported by the harness text-only tool-result transport; no image was delivered.";

/// Keep both ends on UTF-8 boundaries. The marker is metadata, separate from
/// the requested content budget, and its space is reserved by the caller.
fn bounded_text(text: &str, payload_bytes: usize) -> String {
    if text.len() <= payload_bytes {
        return text.to_string();
    }
    let mut head_end = payload_bytes / 2;
    while !text.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = text.len() - (payload_bytes - head_end);
    while !text.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    format!(
        "{}{TRUNCATION_MARKER}{}",
        &text[..head_end],
        &text[tail_start..]
    )
}

/// Shape content before adding status, as in Codex's host adapter. This host
/// additionally reserves bounded diagnostic/notification sections, so even a
/// zero-token body cannot hide a failure or consume a running cell's handle.
pub(super) fn response_to_result(
    response: RuntimeResponse,
    notifications: Vec<String>,
    max_output_tokens: Option<usize>,
) -> ToolResult {
    let (mut status, items, error_text) = match response {
        RuntimeResponse::Result {
            content_items,
            error_text,
            ..
        } => (
            if error_text.is_some() {
                "Script failed\n".to_string()
            } else {
                "Script completed\n".to_string()
            },
            content_items,
            error_text,
        ),
        RuntimeResponse::Yielded {
            cell_id,
            content_items,
        } => (
            format!(
                "Script running with cell ID {cell_id}. Call `wait` with this cell_id for more output.\n"
            ),
            content_items,
            None,
        ),
        RuntimeResponse::Terminated { content_items, .. } => {
            ("Script terminated\n".to_string(), content_items, None)
        }
    };
    let mut texts = Vec::new();
    let mut nested_outcomes = Vec::new();
    let mut image_unsupported = false;
    for item in items {
        match item {
            FunctionCallOutputContentItem::InputText { text } => {
                if text.starts_with("[nested tool outcomes]\n") {
                    nested_outcomes.push(text);
                } else {
                    texts.push(text);
                }
            }
            FunctionCallOutputContentItem::InputImage { .. } => image_unsupported = true,
        }
    }
    let failed = error_text.is_some() || image_unsupported;
    if image_unsupported {
        if status == "Script completed\n" {
            status = "Script failed\n".to_string();
        }
        status.push_str(IMAGE_ERROR);
        status.push('\n');
    }
    if let Some(error) = error_text {
        status.push_str("Script error:\n");
        status.push_str(&bounded_text(&error, DIAGNOSTIC_BYTES));
        status.push('\n');
    }
    if !notifications.is_empty() {
        status.push_str("[notifications]\n");
        status.push_str(&bounded_text(&notifications.join("\n"), DIAGNOSTIC_BYTES));
        status.push('\n');
    }
    // Cancellation receipts are lifecycle evidence. Even max_tokens=0 must
    // retain their bounded actual outcomes rather than only a stopped label.
    if !nested_outcomes.is_empty() {
        status.push_str(&bounded_text(&nested_outcomes.join("\n"), DIAGNOSTIC_BYTES));
        status.push('\n');
    }
    status.push_str("Output:\n");
    let available_bytes = MAX_RESULT_BYTES
        .saturating_sub(status.len())
        .saturating_sub(TRUNCATION_MARKER.len());
    let payload_bytes = max_output_tokens
        .unwrap_or(DEFAULT_OUTPUT_TOKENS)
        .saturating_mul(4)
        .min(available_bytes);
    status.push_str(&bounded_text(&texts.join("\n"), payload_bytes));
    if failed {
        ToolResult::Error(status)
    } else {
        ToolResult::Text(status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bro_code_mode::CellId;

    fn text_item(text: String) -> FunctionCallOutputContentItem {
        FunctionCallOutputContentItem::InputText { text }
    }

    fn result_text(result: ToolResult) -> String {
        match result {
            ToolResult::Text(text) | ToolResult::Error(text) => text,
            other => panic!("expected text result, got {other:?}"),
        }
    }

    #[test]
    fn tiny_budget_keeps_status_and_both_ends_of_output() {
        let result = response_to_result(
            RuntimeResponse::Yielded {
                cell_id: CellId::new("123".into()),
                content_items: vec![text_item(format!("AB{}YZ", "x".repeat(20_000)))],
            },
            vec![],
            Some(1),
        );
        let text = result_text(result);
        assert!(text.starts_with("Script running with cell ID 123."));
        assert!(text.contains("Output:\nAB\n[output truncated;"));
        assert!(text.ends_with("\nYZ"));
        assert!(!text.contains('x'));
    }

    #[test]
    fn zero_budget_preserves_error_and_notification() {
        let result = response_to_result(
            RuntimeResponse::Result {
                cell_id: CellId::new("123".into()),
                content_items: vec![text_item("hidden payload".into())],
                error_text: Some("ReferenceError: missing is not defined".into()),
            },
            vec!["nested operation finished".into()],
            Some(0),
        );
        assert!(matches!(result, ToolResult::Error(_)));
        let text = result_text(result);
        assert!(text.starts_with("Script failed\n"));
        assert!(text.contains("Script error:\nReferenceError: missing is not defined"));
        assert!(text.contains("[notifications]\nnested operation finished"));
        assert!(!text.contains("hidden payload"));
    }

    #[test]
    fn terminated_cell_preserves_bounded_nested_receipts_with_zero_body_budget() {
        let result = response_to_result(
            RuntimeResponse::Terminated {
                cell_id: CellId::new("123".into()),
                content_items: vec![
                    text_item("ordinary output hidden".into()),
                    text_item(format!(
                        "[nested tool outcomes]\n{{\"tool\":\"mutation\",\"output\":\"finished\"}}\n{}\n[3 additional nested outcomes omitted]",
                        "界".repeat(4000)
                    )),
                ],
            },
            vec![],
            Some(0),
        );
        let text = result_text(result);
        assert!(text.starts_with("Script terminated\n"));
        assert!(text.contains("mutation") && text.contains("finished"));
        assert!(text.contains("3 additional nested outcomes omitted"));
        assert!(text.contains("output truncated"));
        assert!(!text.contains("ordinary output hidden"));
        assert!(text.len() <= MAX_RESULT_BYTES);
    }

    #[test]
    fn huge_multibyte_output_and_metadata_fit_the_envelope() {
        let result = response_to_result(
            RuntimeResponse::Result {
                cell_id: CellId::new("123".into()),
                content_items: vec![text_item("界".repeat(100_000))],
                error_text: Some(format!(
                    "diagnostic start {} diagnostic end",
                    "界".repeat(10_000)
                )),
            },
            vec![format!("notify start {} notify end", "界".repeat(10_000))],
            Some(usize::MAX),
        );
        let text = result_text(result);
        assert!(text.len() <= MAX_RESULT_BYTES, "{}", text.len());
        assert!(text.contains("diagnostic start") && text.contains("diagnostic end"));
        assert!(text.contains("notify start") && text.contains("notify end"));
        assert_eq!(text.matches("[output truncated;").count(), 3);
    }

    #[test]
    fn unsupported_image_keeps_a_yielded_cells_continuation() {
        let result = response_to_result(
            RuntimeResponse::Yielded {
                cell_id: CellId::new("123".into()),
                content_items: vec![FunctionCallOutputContentItem::InputImage {
                    image_url: "data:image/png;base64,AA==".into(),
                    detail: None,
                }],
            },
            vec![],
            Some(0),
        );
        assert!(matches!(result, ToolResult::Error(_)));
        let text = result_text(result);
        assert!(text.starts_with("Script running with cell ID 123."));
        assert!(text.contains("no image was delivered"));
        assert!(!text.contains("AA=="));
    }

    #[test]
    fn completed_and_terminated_output_are_capped_by_default() {
        for response in [
            RuntimeResponse::Result {
                cell_id: CellId::new("123".into()),
                content_items: vec![text_item("a".repeat(20_000))],
                error_text: None,
            },
            RuntimeResponse::Terminated {
                cell_id: CellId::new("123".into()),
                content_items: vec![text_item("a".repeat(20_000))],
            },
        ] {
            let text = result_text(response_to_result(response, vec![], None));
            assert!(text.len() <= MAX_RESULT_BYTES);
            assert!(text.contains("[output truncated;"));
        }
    }
}
