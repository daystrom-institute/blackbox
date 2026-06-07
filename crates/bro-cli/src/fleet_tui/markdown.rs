use super::*;

#[cfg(test)]
pub(super) fn render_markdown(text: &str) -> Vec<Line<'static>> {
    render_markdown_with_limit(text, None)
}

pub(super) fn render_markdown_with_width(text: &str, width: usize) -> Vec<Line<'static>> {
    render_markdown_with_limit(text, Some(width.max(1)))
}

pub(super) fn render_markdown_with_limit(text: &str, max_width: Option<usize>) -> Vec<Line<'static>> {
    markdown_blocks_preserving_terminal_shapes(text)
        .into_iter()
        .flat_map(|block| render_markdown_block(block, max_width))
        .collect()
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
