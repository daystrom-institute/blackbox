use super::*;

#[cfg(test)]
pub(super) fn render_markdown(text: &str) -> Vec<Line<'static>> {
    render_markdown_with_limit(text, None)
}

pub(super) fn render_markdown_with_width(text: &str, width: usize) -> Vec<Line<'static>> {
    render_markdown_with_limit(text, Some(width.max(1)))
}

/// Render markdown while preserving every author-supplied newline as a hard
/// line break. CommonMark collapses a single `\n` inside a paragraph into a
/// soft break (rendered as a space) — fine for prose, wrong for the fleet
/// cockpit transcript, where line structure is part of the message (e.g. one
/// number per line, stack traces, ASCII diagrams, list output).
///
/// The transformation prefixes each newline (outside fenced code blocks) with
/// two spaces, which CommonMark reads as a hard line break. Fenced blocks
/// (```` ``` ```` / `~~~`) and lines that look like one are passed through
/// untouched so a code block's content doesn't end up visibly padded with
/// trailing whitespace.
pub(super) fn render_markdown_preserving_breaks_with_width(
    text: &str,
    width: usize,
) -> Vec<Line<'static>> {
    render_markdown_with_limit(&harden_line_breaks(text), Some(width.max(1)))
}

pub(super) fn render_markdown_with_limit(text: &str, max_width: Option<usize>) -> Vec<Line<'static>> {
    let text = unwrap_markdown_table_fences(text);
    markdown_blocks_preserving_terminal_shapes(&text)
        .into_iter()
        .flat_map(|block| render_markdown_block(block, max_width))
        .collect()
}

/// Rewrite single newlines in `text` as markdown hard breaks (`  \n`),
/// skipping lines that are inside a fenced code block (```` ``` ```` /
/// `~~~`). Fence detection is intentionally minimal — we only need to
/// recognize "this line opens a fence" and "this line closes the current
/// fence" so we don't pad a code block's content. Anything more elaborate
/// (info strings, indented fences, nested fences) is left to the downstream
/// markdown parser.
fn harden_line_breaks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut fence_marker: Option<&'static str> = None;
    for segment in text.split_inclusive('\n') {
        let (body, had_nl) = match segment.strip_suffix('\n') {
            Some(b) => (b, true),
            None => (segment, false),
        };
        let trimmed = body.trim_start();
        match fence_marker {
            Some(marker) => {
                let is_close = trimmed.starts_with(marker)
                    && trimmed[marker.len()..].trim().is_empty();
                out.push_str(body);
                if had_nl {
                    out.push('\n');
                }
                if is_close {
                    fence_marker = None;
                }
            }
            None => {
                if let Some(marker) = opening_fence_marker(trimmed) {
                    fence_marker = Some(marker);
                    out.push_str(body);
                    if had_nl {
                        out.push('\n');
                    }
                } else {
                    out.push_str(body);
                    if had_nl {
                        out.push_str("  \n");
                    }
                }
            }
        }
    }
    out
}

fn opening_fence_marker(line: &str) -> Option<&'static str> {
    if line.starts_with("```") {
        Some("```")
    } else if line.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

/// Strip ` ```md `/` ```markdown ` fences whose body contains a markdown table,
/// so the table renders natively instead of as a monospace code block. Models
/// load tables inside markdown fences surprisingly often.
///
/// Adapted (simplified) from codex markdown.rs::unwrap_markdown_fences
/// (Apache-2.0). Conservative: only unwraps `md`/`markdown`-info fences whose
/// body actually contains a header+delimiter table; every other fence (and
/// non-table md fences) passes through verbatim. Does not handle nested fences
/// or blockquoted fences (codex does); good enough for live agent output.
fn unwrap_markdown_table_fences(text: &str) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    if !text.contains("```") && !text.contains("~~~") {
        return Cow::Borrowed(text);
    }
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<&str> = Vec::with_capacity(lines.len());
    let mut changed = false;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(info) = opening_fence_language(line) {
            let marker = fence_marker(line).unwrap_or("```");
            let body_start = i + 1;
            let mut j = body_start;
            while j < lines.len() && !is_closing_fence(lines[j], marker) {
                j += 1;
            }
            let body = &lines[body_start..j.min(lines.len())];
            let is_md = info.as_deref().is_some_and(|l| {
                let l = l.trim().to_ascii_lowercase();
                l == "md" || l == "markdown"
            });
            if is_md && body_has_table(body) {
                out.extend_from_slice(body); // unwrap: emit body without the fence
                changed = true;
            } else {
                out.push(line); // opening fence
                out.extend_from_slice(body);
                if j < lines.len() {
                    out.push(lines[j]); // closing fence
                }
            }
            i = if j < lines.len() { j + 1 } else { j };
            continue;
        }
        out.push(line);
        i += 1;
    }
    if changed {
        Cow::Owned(out.join("\n"))
    } else {
        Cow::Borrowed(text)
    }
}

fn body_has_table(body: &[&str]) -> bool {
    body.windows(2)
        .any(|w| is_table_header_line(w[0]) && is_table_separator_line(w[1]))
}

pub(super) enum MarkdownBlock {
    Markdown(String),
    Table(Vec<String>),
    Code {
        language: Option<String>,
        lines: Vec<String>,
    },
    Quote(Vec<String>),
    Rule,
}

pub(super) fn render_markdown_block(block: MarkdownBlock, max_width: Option<usize>) -> Vec<Line<'static>> {
    match block {
        MarkdownBlock::Markdown(text) => {
            let text = rewrite_task_list_markers(&text);
            let md = tui_markdown::from_str(&text);
            let owned: Vec<Line<'static>> =
                md.lines.into_iter().map(crate::line_into_owned).collect();
            crate::stitch_list_markers(owned)
        }
        MarkdownBlock::Table(lines) => render_table_block(lines, max_width),
        MarkdownBlock::Code { language, lines } => render_code_block(language, lines),
        MarkdownBlock::Quote(lines) => {
            render_quote_block(lines, max_width.map(|w| w.saturating_sub(2).max(1)))
        }
        MarkdownBlock::Rule => render_rule_block(),
    }
}

pub(super) fn markdown_blocks_preserving_terminal_shapes(text: &str) -> Vec<MarkdownBlock> {
    let lines: Vec<&str> = text.lines().collect();
    let mut blocks = Vec::new();
    let mut markdown = String::new();
    let mut i = 0usize;

    while i < lines.len() {
        let line = lines[i];
        if let Some(language) = opening_fence_language(line) {
            push_markdown_block(&mut blocks, &mut markdown);
            let fence = fence_marker(line).unwrap_or("```");
            i += 1;

            let mut code = Vec::new();
            while i < lines.len() && !is_closing_fence(lines[i], fence) {
                code.push(lines[i].to_string());
                i += 1;
            }
            if i < lines.len() {
                i += 1;
            }
            blocks.push(MarkdownBlock::Code {
                language,
                lines: code,
            });
            continue;
        }

        if i + 1 < lines.len()
            && is_table_header_line(line)
            && is_table_separator_line(lines[i + 1])
        {
            push_markdown_block(&mut blocks, &mut markdown);
            let mut table = Vec::new();
            while i < lines.len() && !lines[i].trim().is_empty() && lines[i].contains('|') {
                table.push(lines[i].to_string());
                i += 1;
            }
            blocks.push(MarkdownBlock::Table(table));
            continue;
        }

        if is_blockquote_line(line) {
            push_markdown_block(&mut blocks, &mut markdown);
            let mut quote = Vec::new();
            while i < lines.len() && is_blockquote_line(lines[i]) {
                quote.push(strip_blockquote_prefix(lines[i]).to_string());
                i += 1;
            }
            blocks.push(MarkdownBlock::Quote(quote));
            continue;
        }

        // A standalone thematic break (`---`, `***`, `___`). Guard against
        // setext heading underlines by requiring a preceding blank line so
        // `Title\n---` stays a heading rather than becoming a rule.
        if is_horizontal_rule_line(line) && (i == 0 || lines[i - 1].trim().is_empty()) {
            push_markdown_block(&mut blocks, &mut markdown);
            blocks.push(MarkdownBlock::Rule);
            i += 1;
            continue;
        }

        markdown.push_str(line);
        markdown.push('\n');
        i += 1;
    }

    push_markdown_block(&mut blocks, &mut markdown);
    blocks
}

/// Render a markdown table (header row, separator row, then data rows) as a
/// box-drawn grid with aligned columns. Falls back to styling the raw lines if
/// the block is malformed.
pub(super) fn render_table_block(lines: Vec<String>, max_width: Option<usize>) -> Vec<Line<'static>> {
    if lines.len() < 2 {
        return lines
            .into_iter()
            .map(|line| Line::from(Span::styled(line, Style::default().fg(Color::Gray))))
            .collect();
    }

    let aligns = table_column_aligns(&lines[1]);
    let header = table_cells(&lines[0])
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    let body: Vec<Vec<String>> = lines
        .iter()
        .skip(2)
        .map(|line| table_cells(line).into_iter().map(str::to_string).collect())
        .collect();

    let cols = std::iter::once(header.len())
        .chain(body.iter().map(Vec::len))
        .max()
        .unwrap_or(0);
    if cols == 0 {
        return Vec::new();
    }

    fn cell(row: &[String], c: usize) -> &str {
        row.get(c).map(String::as_str).unwrap_or("")
    }
    let align_at = |c: usize| aligns.get(c).copied().unwrap_or(CellAlign::Left);

    let mut widths = vec![0usize; cols];
    for (c, slot) in widths.iter_mut().enumerate() {
        let mut w = display_width(cell(&header, c));
        for row in &body {
            w = w.max(display_width(cell(row, c)));
        }
        *slot = w;
    }

    if let Some(max_width) = max_width
        && !fit_table_widths(&mut widths, max_width.max(1))
    {
        return lines
            .into_iter()
            .map(|line| {
                Line::from(Span::styled(
                    truncate_display(&line, max_width.max(1)),
                    Style::default().fg(Color::Gray),
                ))
            })
            .collect();
    }

    let border = Style::default().fg(Color::DarkGray);
    let head_style = Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    let body_style = Style::default().fg(Color::Gray);

    let rule = |left: &str, mid: &str, right: &str| -> Line<'static> {
        let mut s = String::from(left);
        for (c, w) in widths.iter().enumerate() {
            s.push_str(&"─".repeat(w + 2));
            s.push_str(if c + 1 == cols { right } else { mid });
        }
        Line::from(Span::styled(s, border))
    };

    let data_row = |row: &[String], style: Style| -> Line<'static> {
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(cols * 2 + 1);
        spans.push(Span::styled("│", border));
        for (c, &w) in widths.iter().enumerate() {
            let padded = pad_cell(cell(row, c), w, align_at(c));
            spans.push(Span::styled(format!(" {padded} "), style));
            spans.push(Span::styled("│", border));
        }
        Line::from(spans)
    };

    let mut out = Vec::with_capacity(body.len() + 4);
    out.push(rule("┌", "┬", "┐"));
    out.push(data_row(&header, head_style));
    out.push(rule("├", "┼", "┤"));
    for row in &body {
        out.push(data_row(row, body_style));
    }
    out.push(rule("└", "┴", "┘"));
    out
}

pub(super) fn render_code_block(language: Option<String>, lines: Vec<String>) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let border = Style::default().fg(Color::DarkGray);
    let lang = language.map(|l| l.trim().to_string()).filter(|l| !l.is_empty());
    let title = match &lang {
        Some(l) => format!("┌─ {l}"),
        None => "┌─ code".to_string(),
    };
    out.push(Line::from(Span::styled(title, border)));
    if lines.is_empty() {
        out.push(Line::from(Span::styled("│", border)));
    } else {
        // Syntect-highlight the whole block (needs full context), then prefix
        // each rendered line with the `│ ` gutter. Unknown/oversize input falls
        // back to plain lines inside highlight_code_to_lines.
        let code = lines.join("\n");
        let highlighted = highlight_code_to_lines(&code, lang.as_deref().unwrap_or(""));
        for hl in highlighted {
            let mut spans = vec![Span::styled("│ ", border)];
            spans.extend(hl.spans);
            out.push(Line::from(spans));
        }
    }
    out.push(Line::from(Span::styled("└─", border)));
    out
}

/// Render a blockquote: recursively render the (prefix-stripped) inner markdown,
/// then prepend a `▌ ` gutter to every produced line so multi-line quotes read
/// as a quote rather than collapsing into one run-on line.
pub(super) fn render_quote_block(lines: Vec<String>, max_width: Option<usize>) -> Vec<Line<'static>> {
    let gutter = Style::default().fg(Color::DarkGray);
    let inner = render_markdown_with_limit(&lines.join("\n"), max_width);
    inner
        .into_iter()
        .map(|line| {
            let mut spans = Vec::with_capacity(line.spans.len() + 1);
            spans.push(Span::styled("▌ ", gutter));
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
}

pub(super) fn render_rule_block() -> Vec<Line<'static>> {
    vec![Line::from(Span::styled(
        "─".repeat(HORIZONTAL_RULE_WIDTH),
        Style::default().fg(Color::DarkGray),
    ))]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unwraps_md_fenced_table() {
        let src = "```md\n| A | B |\n|---|---|\n| 1 | 2 |\n```";
        let out = unwrap_markdown_table_fences(src);
        assert!(!out.contains("```"), "fence should be stripped: {out:?}");
        assert!(out.contains("| A | B |"));
        // ...and it actually renders as a table (box-drawing), not a code block.
        let text: String = render_markdown_with_width(src, 40)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains('┌') && text.contains('│'), "expected a table: {text:?}");
    }

    #[test]
    fn leaves_code_fences_untouched() {
        let src = "```rust\nfn main() {}\n```";
        assert_eq!(unwrap_markdown_table_fences(src), src);
    }

    #[test]
    fn leaves_md_fence_without_table_untouched() {
        let src = "```md\njust some *text*\n```";
        assert_eq!(unwrap_markdown_table_fences(src), src);
    }

    #[test]
    fn plain_text_is_borrowed_unchanged() {
        let src = "plain text\n| A | B |\n|---|---|";
        assert_eq!(unwrap_markdown_table_fences(src), src);
    }
}
