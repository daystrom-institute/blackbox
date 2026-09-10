//! Small, explicit output budgets shared by model-facing tools.

pub const DEFAULT_OUTPUT_BYTES: usize = 8_000;

/// Keep both ends of text and disclose omitted bytes without exceeding `budget`.
pub fn truncate_text(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_owned();
    }
    let marker = format!(
        "\n[output truncated: {} input bytes; narrow the request for omitted content]\n",
        text.len()
    );
    if marker.len() >= budget {
        return utf8_prefix(&marker, budget).to_owned();
    }
    let remaining = budget - marker.len();
    let head = utf8_prefix(text, remaining / 2);
    let mut tail_start = text.len() - (remaining - head.len());
    while !text.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    format!("{head}{marker}{}", &text[tail_start..])
}

pub fn utf8_prefix(text: &str, budget: usize) -> &str {
    let mut end = budget.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_preserves_both_ends_and_utf8_under_small_budgets() {
        let text = format!("BEGIN{}END", "日本語".repeat(100));
        for budget in 0..=text.len() {
            let out = truncate_text(&text, budget);
            assert!(out.len() <= budget);
            if budget >= 150 {
                assert!(out.starts_with("BEGIN"));
                assert!(out.ends_with("END"));
            }
        }
    }
}
