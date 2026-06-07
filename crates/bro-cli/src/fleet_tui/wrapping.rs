//! Style-preserving word wrapping.
//!
//! Adapted from the OpenAI codex TUI (`codex-rs/tui/src/wrapping.rs`,
//! Apache-2.0), reduced to the subset fleet currently needs. The technique:
//! flatten a styled `Line` into one string while recording each span's byte
//! range + style, run `textwrap` over the flat string, then re-slice the
//! original spans by byte range to rebuild styled lines. This preserves
//! per-span styling across wrap boundaries and breaks on word boundaries
//! (long unbreakable tokens are hard-broken, never hyphenated) — unlike the
//! previous char-counter, which broke mid-word and miscounted display width.
//!
//! Not yet ported: hanging indents (`initial_indent`/`subsequent_indent`) and
//! the URL-aware wrap lane. Add those when a call site needs them.

use std::borrow::Cow;
use std::ops::Range;

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use textwrap::{Options, WordSplitter};

/// Columns available for content after reserving `reserved_cols`.
///
/// Returns `None` (not `0`) when the reserved columns exhaust the width, so
/// callers fall back to a prefix-only / truncated render instead of wrapping at
/// zero width (which is unstable). Mirrors codex `width.rs::usable_content_width`.
pub(super) fn usable_content_width(total_width: usize, reserved_cols: usize) -> Option<usize> {
    total_width
        .checked_sub(reserved_cols)
        .filter(|remaining| *remaining > 0)
}

/// Word-wrap one styled line to `width`, preserving per-span styles across wrap
/// boundaries. Returns owned `'static` lines. An empty input yields a single
/// empty line so blank lines survive.
pub(super) fn word_wrap_line(line: &Line<'_>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let (flat, bounds) = flatten_line(line);
    if flat.is_empty() {
        return vec![Line::from(String::new())];
    }

    let opts = Options::new(width)
        .break_words(true)
        .word_splitter(WordSplitter::NoHyphenation);

    let mut out: Vec<Line<'static>> = Vec::new();
    for piece in textwrap::wrap(&flat, &opts) {
        match borrowed_slice_range(&flat, &piece) {
            // Common path: textwrap returns a borrowed sub-slice of `flat`; map
            // it to a byte range and re-slice the original spans (keeps styles).
            Some(range) => out.push(slice_line_spans(line, &bounds, &range)),
            // Fallback: textwrap materialized an owned line (not expected with
            // NoHyphenation). Emit the text unstyled rather than drop content.
            None => out.push(Line::from(piece.into_owned())),
        }
    }
    if out.is_empty() {
        out.push(Line::from(String::new()));
    }
    out
}

/// Concatenate a line's span contents into one string, recording each span's
/// byte range within that string alongside its style.
fn flatten_line(line: &Line<'_>) -> (String, Vec<(Range<usize>, Style)>) {
    let mut flat = String::new();
    let mut bounds = Vec::with_capacity(line.spans.len());
    let mut acc = 0usize;
    for span in &line.spans {
        let start = acc;
        flat.push_str(span.content.as_ref());
        acc += span.content.len();
        bounds.push((start..acc, span.style));
    }
    (flat, bounds)
}

/// If `slice` is a sub-slice of `text` (same allocation), return its byte range.
fn borrowed_slice_range(text: &str, slice: &str) -> Option<Range<usize>> {
    let text_start = text.as_ptr() as usize;
    let text_end = text_start.checked_add(text.len())?;
    let slice_start = slice.as_ptr() as usize;
    let slice_end = slice_start.checked_add(slice.len())?;
    if slice_start < text_start || slice_end > text_end {
        return None;
    }
    Some((slice_start - text_start)..(slice_end - text_start))
}

/// Rebuild a styled line covering `range` (a byte range into the flattened
/// text) by slicing each original span that overlaps it.
fn slice_line_spans(
    original: &Line<'_>,
    bounds: &[(Range<usize>, Style)],
    range: &Range<usize>,
) -> Line<'static> {
    let (start_byte, end_byte) = (range.start, range.end);
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (i, (bound, style)) in bounds.iter().enumerate() {
        if bound.end <= start_byte {
            continue;
        }
        if bound.start >= end_byte {
            break;
        }
        let seg_start = start_byte.max(bound.start);
        let seg_end = end_byte.min(bound.end);
        if seg_end > seg_start {
            let content = original.spans[i].content.as_ref();
            let slice = &content[seg_start - bound.start..seg_end - bound.start];
            spans.push(Span {
                style: *style,
                content: Cow::Owned(slice.to_string()),
            });
        }
        if bound.end >= end_byte {
            break;
        }
    }
    Line {
        style: original.style,
        alignment: original.alignment,
        spans,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Stylize};

    fn concat(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn no_wrap_when_it_fits() {
        let out = word_wrap_line(&Line::from("hello world"), 40);
        assert_eq!(out.len(), 1);
        assert_eq!(concat(&out[0]), "hello world");
    }

    #[test]
    fn breaks_on_word_boundary_not_mid_word() {
        let out = word_wrap_line(&Line::from("hello world"), 7);
        assert_eq!(out.len(), 2);
        assert_eq!(concat(&out[0]), "hello");
        assert_eq!(concat(&out[1]), "world");
    }

    #[test]
    fn preserves_span_styles_across_wrap() {
        let line = Line::from(vec!["hello ".red(), "world".into()]);
        let out = word_wrap_line(&line, 6);
        assert_eq!(out.len(), 2);
        assert_eq!(concat(&out[0]), "hello");
        assert_eq!(out[0].spans[0].style.fg, Some(Color::Red));
        assert_eq!(concat(&out[1]), "world");
    }

    #[test]
    fn long_unbreakable_token_is_hard_broken() {
        let out = word_wrap_line(&Line::from("abcdefghij"), 4);
        // 10 chars / width 4 -> three pieces, none lost.
        assert_eq!(out.iter().map(concat).collect::<String>(), "abcdefghij");
        assert!(out.len() >= 3);
    }

    #[test]
    fn usable_content_width_guards_zero() {
        assert_eq!(usable_content_width(10, 2), Some(8));
        assert_eq!(usable_content_width(2, 2), None);
        assert_eq!(usable_content_width(1, 2), None);
    }
}
