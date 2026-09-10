//! Final model-facing output backstop. Ordinary reads shape their own pages;
//! arbitrary overflow is explicitly omitted, never redirected into recursive dumps.

const DEFAULT_CAP_KB: usize = 16;

/// Model-facing result budget. Zero retains the explicit unbounded opt-out.
pub fn cap_bytes() -> usize {
    std::env::var("BRO_HARNESS_TOOL_RESULT_CAP_KB")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(DEFAULT_CAP_KB)
        .saturating_mul(1024)
}

/// Bound content while keeping shell status/session metadata outside truncation.
/// No filesystem side effects: request a narrower source range/query to recover
/// omitted text. A returned preview never claims to contain the full payload.
pub fn bound_tool_result(_tool: &str, content: String, cap: usize) -> String {
    if cap == 0 || content.len() <= cap {
        return content;
    }
    if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&content)
        && let Some(object) = value.as_object_mut()
    {
        // Reserve metadata first, then allocate serialized bytes to each stream.
        // Always truncate from the original text, not an already-truncated preview.
        let bodies: Vec<_> = ["stdout", "stderr", "output"]
            .into_iter()
            .filter_map(|key| object.get(key)?.as_str().map(|text| (key, text.to_owned())))
            .collect();
        for (key, _) in &bodies {
            object.insert((*key).into(), serde_json::json!(""));
        }
        let overhead = serde_json::to_string(&object).unwrap_or_default().len();
        if !bodies.is_empty() && overhead + bodies.len() * 128 <= cap {
            let allowance = (cap - overhead) / bodies.len() + 2;
            for (key, original) in bodies {
                let mut low = 0;
                let mut high = original.len().min(allowance);
                while low < high {
                    let mid = low + (high - low).div_ceil(2);
                    let candidate = bro_tools::output::truncate_text(&original, mid);
                    if serde_json::to_string(&candidate).unwrap().len() <= allowance {
                        low = mid;
                    } else {
                        high = mid - 1;
                    }
                }
                object.insert(
                    key.into(),
                    serde_json::json!(bro_tools::output::truncate_text(&original, low)),
                );
            }
            let rendered = serde_json::to_string(&object).unwrap();
            if rendered.len() <= cap {
                return rendered;
            }
        }
    }
    bro_tools::output::truncate_text(&content, cap)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn plain_overflow_keeps_continuation_without_creating_dump_instructions() {
        let content = format!(
            "SOURCE\n{}\ncontinue source start_line=201",
            "x".repeat(50_000)
        );
        let out = bound_tool_result("file_read", content, 1024);
        assert!(out.len() <= 1024);
        assert!(out.starts_with("SOURCE"));
        assert!(out.ends_with("continue source start_line=201"));
        assert!(out.contains("output truncated"));
        assert!(!out.contains("harness-dumps"));
    }

    #[test]
    fn shell_metadata_survives_escaped_dual_stream_overflow() {
        let input = json!({"exit_code":null,"running":true,"session_id":42,
            "stdout":"\n\"日".repeat(20_000),"stderr":"problem".repeat(10_000),
            "timed_out":false,"next_step":"shell_poll session_id=42"});
        let out = bound_tool_result("shell_run", input.to_string(), 4096);
        assert!(out.len() <= 4096);
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        for key in [
            "exit_code",
            "running",
            "session_id",
            "timed_out",
            "next_step",
        ] {
            assert_eq!(value[key], input[key]);
        }
        assert!(value["stdout"].as_str().unwrap().contains("truncated"));
        assert!(value["stderr"].as_str().unwrap().contains("truncated"));
    }

    #[test]
    fn small_results_and_explicit_unbounded_cap_pass_through() {
        assert_eq!(bound_tool_result("read", "small".into(), 100), "small");
        assert_eq!(
            bound_tool_result("read", "large".repeat(10_000), 0),
            "large".repeat(10_000)
        );
    }

    #[test]
    fn exact_budget_shell_page_preserves_escaped_bytes_and_continuation_metadata() {
        let cap = 4096;
        let mut page = json!({
            "exit_code": 0,
            "running": false,
            "session_id": 42,
            "stdout": "\n\"\\\t日".repeat(100),
            "stderr": "\u{0000}\r\"problem",
            "timed_out": false,
            "output_pending": true,
        });
        let serialized_len = page.to_string().len();
        assert!(serialized_len < cap);
        let mut stdout = page["stdout"].as_str().unwrap().to_owned();
        stdout.push_str(&"x".repeat(cap - serialized_len));
        page["stdout"] = json!(stdout);
        let serialized = page.to_string();
        assert_eq!(serialized.len(), cap);

        // The first producer bound and later final backstop both preserve a
        // page that already accounts for JSON escaping and status metadata.
        for tool in ["shell_run", "shell_poll", "tool_result"] {
            let output = bound_tool_result(tool, serialized.clone(), cap);
            assert_eq!(output, serialized);
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&output).unwrap(),
                page
            );
        }
    }
}
