use super::*;

/// Soft caps for non-harness providers (the harness already spills oversized
/// results, §2.3); a render-side backstop so one huge block can't dominate.
const ARG_MAX_LINES: usize = 15;
const RESULT_MAX_LINES: usize = 25;
const TOOL_SOURCE_MAX_LINES: usize = 80;

#[derive(Debug, Clone, Copy)]
pub(super) struct TranscriptDisplayOptions {
    pub show_thinking_blocks: bool,
    pub show_tool_responses: bool,
    pub show_reports: bool,
}

impl Default for TranscriptDisplayOptions {
    fn default() -> Self {
        Self {
            show_thinking_blocks: true,
            show_tool_responses: true,
            show_reports: true,
        }
    }
}

/// Verbose inline transcript (§5.4): render the parsed [`TranscriptItem`]s in
/// temporal order, structure carried by markers + color rather than folding.
#[cfg(test)]
pub(super) fn render_transcript(
    items: &[TranscriptItem],
    initial_prompt: &str,
    queued_turns: &[&str],
    width: usize,
) -> Vec<Line<'static>> {
    render_transcript_with_options(
        items,
        initial_prompt,
        queued_turns,
        width,
        TranscriptDisplayOptions::default(),
    )
}

pub(super) fn render_transcript_with_options(
    items: &[TranscriptItem],
    initial_prompt: &str,
    queued_turns: &[&str],
    width: usize,
    display: TranscriptDisplayOptions,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    if !initial_prompt.is_empty() {
        let status = if items.is_empty() {
            TurnRenderStatus::Waiting
        } else {
            TurnRenderStatus::Normal
        };
        lines.extend(render_steer_with_status(initial_prompt, width, status));
        lines.push(Line::from(""));
    }
    if items.is_empty() && initial_prompt.is_empty() && queued_turns.is_empty() {
        return vec![Line::from(Span::styled(
            "  (no output yet)",
            Style::default().fg(Color::DarkGray),
        ))];
    }

    for (idx, item) in items.iter().enumerate() {
        let rendered = render_item(
            item,
            width,
            turn_render_status(items, idx, display),
            display,
        );
        // Only space items that actually rendered (a suppressed quiet result
        // adds nothing — no blank line either).
        if !rendered.is_empty() {
            let compact_tool_call = item_is_compact_tool_call(item, width);
            lines.extend(rendered);
            if !compact_tool_call {
                lines.push(Line::from(""));
            }
        }
    }
    for queued in queued_turns {
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        lines.extend(render_steer_with_status(
            queued,
            width,
            TurnRenderStatus::Queued,
        ));
    }
    lines
}

#[cfg(test)]
pub(super) fn render_committed_items(items: &[TranscriptItem], width: usize) -> Vec<Line<'static>> {
    render_committed_items_with_options(items, width, TranscriptDisplayOptions::default())
}

pub(super) fn render_committed_items_with_options(
    items: &[TranscriptItem],
    width: usize,
    display: TranscriptDisplayOptions,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for item in items {
        let rendered = render_item(item, width, TurnRenderStatus::Normal, display);
        if !rendered.is_empty() {
            let compact_tool_call = item_is_compact_tool_call(item, width);
            lines.extend(rendered);
            if !compact_tool_call {
                lines.push(Line::from(""));
            }
        }
    }
    lines
}

pub(super) fn render_item(
    item: &TranscriptItem,
    width: usize,
    status: TurnRenderStatus,
    display: TranscriptDisplayOptions,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    match item {
        TranscriptItem::UserSteer(t) => lines.extend(render_steer_with_status(t, width, status)),
        TranscriptItem::AssistantText(t) => {
            // Plain assistant text must keep the author's line structure.
            // The default markdown path collapses single `\n` into a soft
            // break (a space) per CommonMark, which mangles messages like
            // `1\n2\n3\n…` into `1 2 3 …`. `preserving_breaks` rewrites
            // each newline into a markdown hard break (`  \n`) outside
            // fenced code blocks, so the source layout survives while
            // inline markdown (bold, code, links, …) still renders.
            lines.extend(render_markdown_preserving_breaks_with_width(t, width))
        }
        TranscriptItem::Thinking(t) => {
            if !display.show_thinking_blocks {
                return Vec::new();
            }
            lines.extend(render_thinking_block(t, width));
        }
        TranscriptItem::ToolCall { name, args } => {
            if is_internal_tool(name) {
                return Vec::new();
            }
            if is_report_tool(name) && !display.show_reports {
                return Vec::new();
            }
            if let Some(report_lines) = render_report_tool_call(name, args, width) {
                lines.extend(report_lines);
            } else if let Some(edit_lines) = render_file_edit_call(name, args, width) {
                lines.extend(edit_lines);
            } else if let Some(source_lines) = render_source_tool_call(name, args, width) {
                lines.extend(source_lines);
            } else if let Some(compact_lines) = compact_tool_call_lines(name, args, width) {
                lines.extend(compact_lines);
            } else {
                lines.push(Line::from(Span::styled(
                    format!("{TOOL_CALL_GLYPH} {name}"),
                    tool_call_style(),
                )));
                lines.extend(monospace_block(args, ARG_MAX_LINES, Color::DarkGray));
            }
        }
        TranscriptItem::ToolResult {
            tool,
            content,
            is_error,
            rider,
        } => {
            if tool.as_deref().is_some_and(is_internal_tool) {
                return Vec::new();
            }
            if tool.as_deref().is_some_and(is_report_tool) && !display.show_reports {
                return Vec::new();
            }
            if !display.show_tool_responses {
                return Vec::new();
            }
            // Errors always show. Otherwise, show the body only for change-making
            // / opaque tools; suppress noisy output and quiet successes.
            if shell_result_tool(tool.as_deref()) {
                lines.extend(shell_result_block(content, *is_error, RESULT_MAX_LINES));
            } else if *is_error {
                lines.extend(monospace_block(content, RESULT_MAX_LINES, Color::Red));
            } else if !tool_result_suppress_ok(tool.as_deref())
                && tool_result_is_verbose(tool.as_deref())
            {
                lines.extend(monospace_block(content, RESULT_MAX_LINES, Color::Gray));
            }

            if let Some(r) = rider {
                let mut rl = r.lines();
                if let Some(summary) = rl.next() {
                    lines.push(Line::from(Span::styled(
                        format!("⚠ {summary}"),
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    )));
                }
                for l in rl {
                    lines.push(Line::from(Span::styled(
                        l.to_string(),
                        Style::default().fg(Color::Yellow),
                    )));
                }
            }
        }
        TranscriptItem::Report {
            message,
            needs_input,
        } => {
            if !display.show_reports {
                return Vec::new();
            }
            let color = if *needs_input {
                Color::Yellow
            } else {
                Color::LightYellow
            };
            let tag = if *needs_input { " (needs input)" } else { "" };
            lines.push(Line::from(Span::styled(
                format!("◆ {message}{tag}"),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )));
        }
        TranscriptItem::TodoState(todo) => {
            let text = if todo.items.is_empty() {
                "☑ todo cleared".to_string()
            } else {
                format!("☑ todo {} / {} updated", todo.completed, todo.total)
            };
            lines.push(Line::from(Span::styled(
                text,
                Style::default().fg(Color::LightYellow),
            )));
        }
        TranscriptItem::CompactBoundary { trigger } => {
            lines.push(Line::from(Span::styled(
                format!("── compacted ({trigger}) ──"),
                Style::default().fg(Color::DarkGray),
            )));
        }
        TranscriptItem::TurnFooter { .. } => {}
    }
    lines
}

fn render_thinking_block(text: &str, width: usize) -> Vec<Line<'static>> {
    let thinking_bg = Color::Rgb(29, 32, 38);
    let gutter = Style::default()
        .fg(Color::Rgb(112, 132, 154))
        .bg(thinking_bg)
        .add_modifier(Modifier::BOLD);
    let bg = Style::default()
        .fg(Color::Rgb(154, 162, 174))
        .bg(thinking_bg);
    let content_width = usable_content_width(width, 2).unwrap_or(1);
    let normalized = normalize_thinking_text(text);
    render_markdown_preserving_breaks_with_width(&normalized, content_width)
        .into_iter()
        .flat_map(|line| word_wrap_line(&line, content_width))
        .filter(|line| !line_plain_text(line).trim().is_empty())
        .map(|line| prepend_line_prefix(line, "▏ ", gutter, bg))
        .collect()
}

fn normalize_thinking_text(text: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut previous_blank = false;
    for line in text.trim_matches('\n').lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            if !previous_blank && !out.is_empty() {
                out.push(String::new());
                previous_blank = true;
            }
            continue;
        }
        out.push(line.to_string());
        previous_blank = false;
    }
    while out.last().is_some_and(String::is_empty) {
        out.pop();
    }
    if out.is_empty() {
        String::new()
    } else {
        out.join("\n")
    }
}

fn line_plain_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

fn item_is_compact_tool_call(item: &TranscriptItem, width: usize) -> bool {
    match item {
        TranscriptItem::ToolCall { name, args } if !is_internal_tool(name) => {
            render_report_tool_call(name, args, width).is_some()
                || render_file_edit_call(name, args, width).is_some()
                || render_source_tool_call(name, args, width).is_some()
                || compact_tool_call_text(name, args).is_some()
        }
        _ => false,
    }
}

pub(super) fn turn_render_status(
    items: &[TranscriptItem],
    idx: usize,
    display: TranscriptDisplayOptions,
) -> TurnRenderStatus {
    let mut saw_any = false;
    let mut saw_modelish = false;
    let mut saw_footer = false;
    for item in items.iter().skip(idx + 1) {
        if matches!(item, TranscriptItem::UserSteer(_)) {
            break;
        }
        if matches!(item, TranscriptItem::Thinking(_)) && !display.show_thinking_blocks {
            continue;
        }
        if matches!(item, TranscriptItem::ToolResult { .. }) && !display.show_tool_responses {
            continue;
        }
        if item_is_reportish(item) && !display.show_reports {
            continue;
        }
        saw_any = true;
        match item {
            TranscriptItem::TurnFooter { .. } => saw_footer = true,
            TranscriptItem::AssistantText(_)
            | TranscriptItem::Thinking(_)
            | TranscriptItem::ToolCall { .. }
            | TranscriptItem::ToolResult { .. }
            | TranscriptItem::Report { .. }
            | TranscriptItem::TodoState(_)
            | TranscriptItem::CompactBoundary { .. } => saw_modelish = true,
            TranscriptItem::UserSteer(_) => {}
        }
    }
    if !saw_any {
        TurnRenderStatus::Waiting
    } else if saw_footer && !saw_modelish {
        TurnRenderStatus::EmptyResult
    } else {
        TurnRenderStatus::Normal
    }
}

fn item_is_reportish(item: &TranscriptItem) -> bool {
    match item {
        TranscriptItem::Report { .. } => true,
        TranscriptItem::ToolCall { name, .. } => is_report_tool(name),
        TranscriptItem::ToolResult { tool, .. } => tool.as_deref().is_some_and(is_report_tool),
        _ => false,
    }
}

pub(super) fn tool_call_style() -> Style {
    Style::default().fg(Color::Rgb(118, 150, 124))
}

/// Show a tool's result body verbosely? Change-making and opaque tools
/// (Edit/Write/MultiEdit, MCP) → yes (we want to see what changed). Noisy
/// command/query output (Bash, Read, Grep, …) → no (operator feedback). Errors
/// bypass this entirely.
/// Tools whose successful result JSON (e.g. `{"ok":true,"replacements":1}`) is
/// noise — the compact call rendering already shows what changed. Only
/// suppress on success; errors still surface.
pub(super) fn tool_result_suppress_ok(name: Option<&str>) -> bool {
    matches!(name, Some("file_edit")) || name.is_some_and(is_report_tool)
}

pub(super) fn tool_result_is_verbose(name: Option<&str>) -> bool {
    let Some(n) = name else {
        return false;
    };
    let n = n.to_ascii_lowercase();
    n.starts_with("mcp__")
        || n.contains("mcp")
        || n.contains("edit")
        || n.contains("write")
        || n.contains("apply_patch")
        || n.contains("notebook")
}

pub(super) fn is_internal_tool(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == "report"
        || n == "todo_write"
        || n == "tool_search"
        || n == "tool_search_tool"
        || n.starts_with("tool_search.")
}

pub(super) fn shell_result_tool(name: Option<&str>) -> bool {
    matches!(name, Some("shell_run" | "shell_poll" | "shell_kill"))
}

pub(super) fn shell_result_block(
    content: &str,
    is_error: bool,
    max_lines: usize,
) -> Vec<Line<'static>> {
    const MAX_SHELL_RESULT_JSON_BYTES: usize = 200_000;
    if content.len() > MAX_SHELL_RESULT_JSON_BYTES {
        return vec![Line::from(Span::styled(
            format!(
                "↳ shell result too large for live render ({}); inspect transcript/tool dump",
                bytes_compact(content.len())
            ),
            Style::default().fg(if is_error {
                Color::Red
            } else {
                Color::DarkGray
            }),
        ))];
    }
    let value = match serde_json::from_str::<serde_json::Value>(content) {
        Ok(value) => value,
        Err(_) => {
            return monospace_block(
                content,
                max_lines,
                if is_error { Color::Red } else { Color::Gray },
            );
        }
    };
    let Some(obj) = value.as_object() else {
        return monospace_block(
            content,
            max_lines,
            if is_error { Color::Red } else { Color::Gray },
        );
    };
    let exit = obj
        .get("exit_code")
        .map(|v| {
            if v.is_null() {
                "exit=null".to_string()
            } else {
                format!("exit={}", v)
            }
        })
        .unwrap_or_else(|| "exit=?".to_string());
    let running = obj
        .get("running")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let timed_out = obj
        .get("timed_out")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let mut head = exit;
    if running {
        head.push_str(" running");
    }
    if timed_out {
        head.push_str(" timed_out");
    }
    if let Some(id) = obj.get("session_id").and_then(|v| v.as_str()) {
        head.push_str(&format!(" session={id}"));
    }

    let mut out = vec![Line::from(Span::styled(
        format!("↳ {head}"),
        Style::default().fg(if is_error {
            Color::Red
        } else {
            Color::DarkGray
        }),
    ))];
    if running
        && let Some(next_step) = obj.get("next_step").and_then(|v| v.as_str())
        && !next_step.is_empty()
    {
        out.push(Line::from(Span::styled(
            format!("next: {next_step}"),
            Style::default().fg(Color::Yellow),
        )));
    }
    for (label, color) in [("stdout", Color::Gray), ("stderr", Color::Red)] {
        if let Some(text) = obj.get(label).and_then(|v| v.as_str())
            && !text.is_empty()
        {
            out.push(Line::from(Span::styled(
                format!("{label}:"),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )));
            out.extend(monospace_block(text, max_lines, color));
        }
    }
    if let Some(register) = obj.get("stdout_register").and_then(|v| v.as_str()) {
        out.push(Line::from(Span::styled(
            format!("stdout → {register}"),
            Style::default().fg(Color::Gray),
        )));
    }
    out
}

pub(super) fn render_file_edit_call(
    name: &str,
    args: &str,
    width: usize,
) -> Option<Vec<Line<'static>>> {
    if name != "file_edit" {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(args).ok()?;
    let obj = value.as_object()?;
    let path = obj
        .get("file_path")
        .or_else(|| obj.get("path"))
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let old = obj.get("old_string").and_then(|v| v.as_str())?;
    let new = obj.get("new_string").and_then(|v| v.as_str())?;
    let replace_all = obj
        .get("replace_all")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let suffix = if replace_all {
        ", replace_all=true"
    } else {
        ""
    };
    let content_width = width.saturating_sub(2).max(12);
    let mut out = vec![Line::from(Span::styled(
        format!(
            "{TOOL_CALL_GLYPH} file_edit({}{suffix})",
            truncate(path, content_width)
        ),
        tool_call_style(),
    ))];
    out.extend(diff_side_lines(old, '-', Color::Red, content_width));
    out.extend(diff_side_lines(new, '+', Color::Green, content_width));
    Some(out)
}

pub(super) fn render_report_tool_call(
    name: &str,
    args: &str,
    width: usize,
) -> Option<Vec<Line<'static>>> {
    if !is_report_tool(name) {
        return None;
    }
    let message = extract_tool_string_arg(args, &["message", "body", "text"])?;
    let compact = quote_flat_string(&message);
    let max_width = width.saturating_sub(1).min(140).max(12);
    let prefix = format!("{TOOL_CALL_GLYPH} report(");
    let suffix = ")";
    let available = max_width
        .saturating_sub(prefix.chars().count() + suffix.chars().count())
        .max(1);
    let rendered = format!("{prefix}{}{suffix}", truncate(&compact, available));
    Some(vec![Line::from(Span::styled(rendered, tool_call_style()))])
}

pub(super) fn render_source_tool_call(
    name: &str,
    args: &str,
    width: usize,
) -> Option<Vec<Line<'static>>> {
    if is_apply_patch_tool(name) {
        let source = extract_tool_string_arg(args, &["source", "patch", "input"])
            .or_else(|| patch_source_from_raw_args(args))?;
        return Some(render_apply_patch_source(name, &source, width));
    }

    if is_exec_source_tool(name) {
        let source = extract_tool_string_arg(args, &["source", "code", "input"])?;
        return Some(render_code_source_tool_call(name, "js", &source, width));
    }

    None
}

fn render_apply_patch_source(name: &str, source: &str, width: usize) -> Vec<Line<'static>> {
    let content_width = width.saturating_sub(2).max(12);
    let mut out = vec![Line::from(Span::styled(
        format!("{TOOL_CALL_GLYPH} {name}"),
        tool_call_style(),
    ))];
    out.extend(diff_source_lines(source, content_width));
    out
}

fn render_code_source_tool_call(
    name: &str,
    lang: &str,
    source: &str,
    width: usize,
) -> Vec<Line<'static>> {
    let content_width = width.saturating_sub(2).max(12);
    let mut out = vec![Line::from(Span::styled(
        format!("{TOOL_CALL_GLYPH} {name}({lang})"),
        tool_call_style(),
    ))];
    let capped = cap_source_lines(source, TOOL_SOURCE_MAX_LINES, "// ... truncated ...");
    let fenced = format!("```{lang}\n{}\n```", capped.trim_matches('\n'));
    out.extend(render_markdown_with_width(&fenced, content_width));
    out
}

fn diff_source_lines(source: &str, width: usize) -> Vec<Line<'static>> {
    let line_width = width.saturating_sub(2).max(1);
    let mut out = Vec::new();
    let mut lines = source.lines().peekable();
    if lines.peek().is_none() {
        out.push(Line::from(Span::styled(
            "(empty patch)",
            Style::default().fg(Color::DarkGray),
        )));
        return out;
    }

    for line in lines.by_ref().take(TOOL_SOURCE_MAX_LINES) {
        let (color, bold) = apply_patch_line_style(line);
        let mut style = Style::default().fg(color);
        if bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        out.push(Line::from(Span::styled(truncate(line, line_width), style)));
    }
    if lines.next().is_some() {
        out.push(Line::from(Span::styled(
            "…",
            Style::default().fg(Color::DarkGray),
        )));
    }
    out
}

fn apply_patch_line_style(line: &str) -> (Color, bool) {
    if line.starts_with("*** Add File:") || line.starts_with('+') {
        (Color::Green, line.starts_with("***"))
    } else if line.starts_with("*** Delete File:") || line.starts_with('-') {
        (Color::Red, line.starts_with("***"))
    } else if line.starts_with("*** Update File:")
        || line.starts_with("*** Move to:")
        || line.starts_with("@@")
    {
        (Color::Yellow, true)
    } else if line.starts_with("*** Begin Patch") || line.starts_with("*** End Patch") {
        (Color::DarkGray, true)
    } else {
        (Color::Gray, false)
    }
}

fn cap_source_lines(source: &str, max_lines: usize, truncation_line: &str) -> String {
    let mut lines = source.lines();
    let mut out: Vec<&str> = lines.by_ref().take(max_lines).collect();
    if lines.next().is_some() {
        out.push(truncation_line);
    }
    out.join("\n")
}

fn extract_tool_string_arg(args: &str, keys: &[&str]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(args).ok()?;
    let obj = value.as_object()?;
    keys.iter()
        .find_map(|key| obj.get(*key).and_then(|value| value.as_str()))
        .map(ToString::to_string)
}

fn patch_source_from_raw_args(args: &str) -> Option<String> {
    args.trim_start()
        .starts_with("*** Begin Patch")
        .then(|| args.to_string())
}

fn is_apply_patch_tool(name: &str) -> bool {
    tool_name_leaf(name) == "apply_patch"
}

fn is_exec_source_tool(name: &str) -> bool {
    tool_name_leaf(name) == "exec"
}

fn is_report_tool(name: &str) -> bool {
    matches!(tool_name_leaf(name).as_str(), "bro_report" | "report")
}

fn tool_name_leaf(name: &str) -> String {
    let dotted = name.rsplit(['.', ':', '/']).next().unwrap_or(name);
    dotted
        .rsplit("__")
        .next()
        .unwrap_or(dotted)
        .to_ascii_lowercase()
}

pub(super) fn diff_side_lines(
    text: &str,
    marker: char,
    color: Color,
    width: usize,
) -> Vec<Line<'static>> {
    const MAX_DIFF_SIDE_LINES: usize = 12;
    let mut out = Vec::new();
    let mut lines = text.lines().peekable();
    if lines.peek().is_none() {
        out.push(Line::from(Span::styled(
            format!("{marker}"),
            Style::default().fg(color),
        )));
        return out;
    }
    let line_width = width.saturating_sub(2).max(1);
    for line in lines.by_ref().take(MAX_DIFF_SIDE_LINES) {
        out.push(Line::from(Span::styled(
            format!("{marker} {}", truncate(line, line_width)),
            Style::default().fg(color),
        )));
    }
    if lines.next().is_some() {
        out.push(Line::from(Span::styled(
            format!("{marker} …"),
            Style::default().fg(color),
        )));
    }
    out
}

#[cfg(test)]
pub(super) fn compact_tool_call_line(name: &str, args: &str, width: usize) -> Option<String> {
    let line = compact_tool_call_text(name, args)?;
    let max_width = width.saturating_sub(1);
    (max_width > 0 && line.chars().count() <= max_width).then_some(line)
}

pub(super) fn compact_tool_call_lines(
    name: &str,
    args: &str,
    width: usize,
) -> Option<Vec<Line<'static>>> {
    let (display_name, rendered) = compact_tool_call_components(name, args)?;
    let style = tool_call_style();
    let prefix = format!("{TOOL_CALL_GLYPH} {display_name}(");
    let suffix = ")";
    if rendered.is_empty() {
        return Some(vec![Line::from(Span::styled(
            format!("{prefix}{suffix}"),
            style,
        ))]);
    }
    let available = width.saturating_sub(prefix.chars().count()).max(1);
    let chunks = wrap_compact_arg_text(&format!("{rendered}{suffix}"), available);
    Some(
        chunks
            .into_iter()
            .enumerate()
            .map(|(idx, chunk)| {
                if idx == 0 {
                    Line::from(Span::styled(format!("{prefix}{chunk}"), style))
                } else {
                    Line::from(Span::styled(format!("  {chunk}"), style))
                }
            })
            .collect(),
    )
}

pub(super) fn compact_tool_call_text(name: &str, args: &str) -> Option<String> {
    let (display_name, rendered) = compact_tool_call_components(name, args)?;
    Some(format!("{TOOL_CALL_GLYPH} {display_name}({rendered})"))
}

fn compact_tool_call_components(name: &str, args: &str) -> Option<(String, String)> {
    let value: serde_json::Value = serde_json::from_str(args).ok()?;
    let rendered = if is_mcp_tool(name) {
        compact_mcp_tool_args(&value)?
    } else {
        compact_tool_args(name, &value)?
    };
    let display_name = compact_tool_display_name(name);
    Some((display_name, rendered))
}

fn wrap_compact_arg_text(text: &str, width: usize) -> Vec<String> {
    let opts = textwrap::Options::new(width.max(1))
        .break_words(true)
        .word_splitter(textwrap::WordSplitter::NoHyphenation);
    textwrap::wrap(text, &opts)
        .into_iter()
        .map(|chunk| chunk.into_owned())
        .collect()
}

pub(super) fn compact_tool_args(tool: &str, value: &serde_json::Value) -> Option<String> {
    if let Some(rendered) = compact_builtin_tool_args(tool, value) {
        return Some(rendered);
    }
    match value {
        serde_json::Value::Object(map) => {
            if map.is_empty() {
                return Some(String::new());
            }
            let mut entries: Vec<(&String, &serde_json::Value)> = map.iter().collect();
            entries.sort_by_key(|(k, _)| tool_arg_rank(tool, k));
            let positional_single = entries.len() == 1;
            let parts: Option<Vec<String>> = entries
                .into_iter()
                .map(|(key, value)| {
                    let rendered = compact_json_value(value)?;
                    if positional_single || positional_arg_key(key) {
                        Some(rendered)
                    } else {
                        Some(format!("{key}={rendered}"))
                    }
                })
                .collect();
            parts.map(|p| p.join(", "))
        }
        serde_json::Value::Array(items) => {
            let parts: Option<Vec<String>> = items.iter().map(compact_json_value).collect();
            parts.map(|p| p.join(", "))
        }
        serde_json::Value::Null => Some(String::new()),
        _ => compact_json_value(value),
    }
}

pub(super) fn compact_builtin_tool_args(tool: &str, value: &serde_json::Value) -> Option<String> {
    match tool {
        "shell_run" => compact_shell_run_args(value),
        "shell_poll" => compact_shell_poll_args(value),
        "shell_kill" => compact_shell_kill_args(value),
        "file_read" => compact_file_read_args(value),
        "file_write" => compact_file_write_args(value),
        "content_search" => compact_content_search_args(value),
        "glob" => compact_glob_args(value),
        "web_fetch" => compact_web_fetch_args(value),
        "git_diff" => compact_named_args(value, &["include_untracked"]),
        "git_show" => compact_named_args(value, &["rev"]),
        "git_commit" => compact_named_args(value, &["paths", "message"]),
        "enter_worktree" => compact_named_args(value, &["purpose", "base", "branch_prefix"]),
        "exit_worktree" => compact_named_args(
            value,
            &[
                "worktree",
                "disposition",
                "paths",
                "commit_message",
                "confirm",
            ],
        ),
        _ => None,
    }
}

fn compact_mcp_tool_args(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Object(map) => {
            if map.is_empty() {
                return Some(String::new());
            }
            let mut entries: Vec<(&String, &serde_json::Value)> = map
                .iter()
                .filter(|(_, value)| !matches!(value, serde_json::Value::Null))
                .collect();
            entries.sort_by_key(|(key, _)| tool_arg_rank("", key));
            let parts: Vec<String> = entries
                .into_iter()
                .map(|(key, value)| format!("{key}={}", compact_mcp_json_value(value)))
                .collect();
            Some(parts.join(", "))
        }
        serde_json::Value::Array(items) => Some(compact_mcp_json_value(&serde_json::Value::Array(
            items.clone(),
        ))),
        serde_json::Value::Null => Some(String::new()),
        _ => Some(compact_mcp_json_value(value)),
    }
}

pub(super) fn compact_shell_run_args(value: &serde_json::Value) -> Option<String> {
    let obj = value.as_object()?;
    let cmd = obj
        .get("cmd")
        .or_else(|| obj.get("command"))
        .and_then(|v| v.as_str())?;
    let cmd = quote_flat_string(cmd);
    let cwd = obj
        .get("cwd")
        .or_else(|| obj.get("workdir"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty());
    let mut parts = Vec::new();
    if let Some(cwd) = cwd {
        parts.push(format!("cwd: {}", compact_string_arg(cwd)));
    }
    parts.push(format!("cmd: {cmd}"));
    append_present_args(
        obj,
        &mut parts,
        &["timeout_ms", "yield_time_ms", "max_output_tokens"],
    );
    if let Some(stdin) = obj.get("stdin").and_then(|v| v.as_str()) {
        parts.push(format!("stdin={}", compact_text_summary(stdin)));
    }
    if let Some(env) = obj.get("env").and_then(|v| v.as_object())
        && !env.is_empty()
    {
        parts.push(format!("env={} vars", env.len()));
    }
    Some(parts.join(", "))
}

pub(super) fn compact_shell_poll_args(value: &serde_json::Value) -> Option<String> {
    let obj = value.as_object()?;
    let mut parts = Vec::new();
    push_string_arg(obj, &mut parts, "session_id", "session", false);
    append_present_args(
        obj,
        &mut parts,
        &[
            "signal",
            "yield_time_ms",
            "max_output_tokens",
            "close_stdin",
        ],
    );
    if let Some(stdin) = obj.get("stdin").and_then(|v| v.as_str()) {
        parts.push(format!("stdin={}", compact_text_summary(stdin)));
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

pub(super) fn compact_shell_kill_args(value: &serde_json::Value) -> Option<String> {
    let obj = value.as_object()?;
    let mut parts = Vec::new();
    push_string_arg(obj, &mut parts, "session_id", "session", false);
    append_present_args(
        obj,
        &mut parts,
        &["signal", "grace_ms", "max_output_tokens"],
    );
    (!parts.is_empty()).then(|| parts.join(", "))
}

pub(super) fn compact_file_read_args(value: &serde_json::Value) -> Option<String> {
    let obj = value.as_object()?;
    let mut parts = Vec::new();
    if !push_string_arg(obj, &mut parts, "file_path", "", true) {
        push_string_arg(obj, &mut parts, "path", "", true);
    }
    append_present_args(
        obj,
        &mut parts,
        &[
            "offset",
            "limit",
            "line_start",
            "line_end",
            "max_bytes",
            "max_lines",
        ],
    );
    (!parts.is_empty()).then(|| parts.join(", "))
}

pub(super) fn compact_file_write_args(value: &serde_json::Value) -> Option<String> {
    let obj = value.as_object()?;
    let mut parts = Vec::new();
    push_string_arg(obj, &mut parts, "file_path", "", true);
    if let Some(content) = obj.get("content").and_then(|v| v.as_str()) {
        parts.push(format!("content={}", compact_text_summary(content)));
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

pub(super) fn compact_content_search_args(value: &serde_json::Value) -> Option<String> {
    let obj = value.as_object()?;
    let mut parts = Vec::new();
    push_string_arg(obj, &mut parts, "pattern", "", true);
    // Only show path when explicitly present (i.e. not cwd)
    push_string_arg(obj, &mut parts, "path", "path", false);
    (!parts.is_empty()).then(|| parts.join(", "))
}

pub(super) fn compact_glob_args(value: &serde_json::Value) -> Option<String> {
    let obj = value.as_object()?;
    let mut parts = Vec::new();
    push_string_arg(obj, &mut parts, "pattern", "", true);
    // Only show path when explicitly present (i.e. not cwd)
    push_string_arg(obj, &mut parts, "path", "path", false);
    (!parts.is_empty()).then(|| parts.join(", "))
}

pub(super) fn compact_web_fetch_args(value: &serde_json::Value) -> Option<String> {
    let obj = value.as_object()?;
    let mut parts = Vec::new();
    push_string_arg(obj, &mut parts, "url", "", true);
    append_present_args(obj, &mut parts, &["max_chars"]);
    (!parts.is_empty()).then(|| parts.join(", "))
}

pub(super) fn compact_named_args(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    let obj = value.as_object()?;
    let mut parts = Vec::new();
    append_present_args(obj, &mut parts, keys);
    Some(parts.join(", "))
}

pub(super) fn append_present_args(
    obj: &serde_json::Map<String, serde_json::Value>,
    parts: &mut Vec<String>,
    keys: &[&str],
) {
    for key in keys {
        if matches!(obj.get(*key), None | Some(serde_json::Value::Null)) {
            continue;
        }
        if *key == "paths" {
            if let Some(paths) = obj.get(*key).and_then(|v| v.as_array()) {
                parts.push(format!("paths={}", compact_array_summary(paths, "path")));
                continue;
            }
        }
        if let Some(value) = obj.get(*key).and_then(compact_json_value) {
            parts.push(format!("{key}={value}"));
        }
    }
}

pub(super) fn push_string_arg(
    obj: &serde_json::Map<String, serde_json::Value>,
    parts: &mut Vec<String>,
    key: &str,
    label: &str,
    positional: bool,
) -> bool {
    let Some(value) = obj.get(key).and_then(|v| v.as_str()) else {
        return false;
    };
    let rendered = compact_string_arg(value);
    if positional || label.is_empty() {
        parts.push(rendered);
    } else {
        parts.push(format!("{label}={rendered}"));
    }
    true
}

pub(super) fn compact_array_summary(items: &[serde_json::Value], noun: &str) -> String {
    match items {
        [] => "[]".into(),
        [single] => compact_json_value(single).unwrap_or_else(|| format!("1 {noun}")),
        _ => format!("{} {noun}s", items.len()),
    }
}

pub(super) fn compact_text_summary(text: &str) -> String {
    let lines = text.lines().count().max(usize::from(!text.is_empty()));
    if lines > 1 {
        format!("{}, {lines} lines", bytes_compact(text.len()))
    } else {
        bytes_compact(text.len())
    }
}

pub(super) fn positional_arg_key(key: &str) -> bool {
    matches!(
        key,
        "path"
            | "file"
            | "file_path"
            | "source"
            | "target"
            | "url"
            | "command"
            | "cmd"
            | "query"
            | "pattern"
            | "text"
            | "input"
            | "register"
            | "session_id"
    )
}

pub(super) fn tool_arg_rank(tool: &str, key: &str) -> usize {
    let key_rank = match key {
        "path" | "file" | "file_path" | "source" | "target" | "url" => 0,
        "command" | "cmd" | "session_id" => 0,
        "query" | "pattern" => 0,
        "text" | "input" => 0,
        "register" => 0,
        "source_range" | "range" | "insert" => 1,
        "old_string" | "new_string" | "replacement" | "content" => 2,
        "line" | "line_start" | "line_end" | "limit" | "max_results" | "max_lines" => 3,
        "cwd" | "workdir" => 4,
        _ => 10,
    };
    if tool.contains("shell") && matches!(key, "command" | "cmd") {
        0
    } else {
        key_rank
    }
}

pub(super) fn compact_json_value(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => Some(compact_string_arg(s)),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        serde_json::Value::Null => Some("null".into()),
        serde_json::Value::Array(items) if items.len() <= 3 => {
            let parts: Option<Vec<String>> = items.iter().map(compact_json_value).collect();
            parts.map(|p| format!("[{}]", p.join(", ")))
        }
        serde_json::Value::Object(map) if map.len() <= 2 => {
            let mut parts = Vec::new();
            for (key, value) in map {
                parts.push(format!("{key}: {}", compact_json_value(value)?));
            }
            Some(format!("{{{}}}", parts.join(", ")))
        }
        _ => None,
    }
}

fn compact_mcp_json_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => {
            if s.len() > 80 || s.lines().count() > 1 {
                compact_text_summary(s)
            } else {
                compact_string_arg(s)
            }
        }
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Null => "null".into(),
        serde_json::Value::Array(items) => {
            if items.is_empty() {
                return "[]".into();
            }
            if items.len() <= 3
                && items.iter().all(|item| {
                    matches!(
                        item,
                        serde_json::Value::String(_)
                            | serde_json::Value::Number(_)
                            | serde_json::Value::Bool(_)
                            | serde_json::Value::Null
                    )
                })
            {
                let parts: Vec<String> = items.iter().map(compact_mcp_json_value).collect();
                format!("[{}]", parts.join(", "))
            } else {
                compact_array_summary(items, "item")
            }
        }
        serde_json::Value::Object(map) => {
            if map.is_empty() {
                "{}".into()
            } else if map.len() <= 2 {
                let parts: Vec<String> = map
                    .iter()
                    .map(|(key, value)| format!("{key}: {}", compact_mcp_json_value(value)))
                    .collect();
                format!("{{{}}}", parts.join(", "))
            } else {
                format!("{} fields", map.len())
            }
        }
    }
}

fn compact_tool_display_name(name: &str) -> String {
    if is_mcp_tool(name) {
        tool_name_leaf(name)
    } else {
        name.to_string()
    }
}

fn is_mcp_tool(name: &str) -> bool {
    name.to_ascii_lowercase().starts_with("mcp__")
}

pub(super) fn compact_string_arg(s: &str) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.is_empty() {
        return "\"\"".into();
    }
    let needs_quotes = flat.chars().any(char::is_whitespace)
        || flat.contains('"')
        || flat.contains('(')
        || flat.contains(')');
    if needs_quotes {
        serde_json::to_string(&flat).unwrap_or_else(|_| format!("{flat:?}"))
    } else {
        flat
    }
}

pub(super) fn quote_flat_string(s: &str) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    serde_json::to_string(&flat).unwrap_or_else(|_| format!("{flat:?}"))
}

#[cfg(test)]
pub(super) fn render_steer(text: &str, width: usize) -> Vec<Line<'static>> {
    render_steer_with_status(text, width, TurnRenderStatus::Normal)
}

pub(super) fn render_steer_with_status(
    text: &str,
    width: usize,
    status: TurnRenderStatus,
) -> Vec<Line<'static>> {
    let user_bg = Color::Rgb(38, 42, 46);
    let gutter = Style::default()
        .fg(Color::LightBlue)
        .bg(user_bg)
        .add_modifier(Modifier::BOLD);
    let bg = Style::default().bg(user_bg);
    // Reserve 2 cols for the "▌ " gutter; fall back to 1 col if width is tiny
    // rather than wrapping at zero width.
    let content_width = usable_content_width(width, 2).unwrap_or(1);
    let mut out: Vec<Line<'static>> =
        render_markdown_with_width(text.trim_matches('\n'), content_width)
            .into_iter()
            .flat_map(|line| word_wrap_line(&line, content_width))
            .map(|line| prepend_line_prefix(line, "▌ ", gutter, bg))
            .collect();
    let Some(label) = turn_status_label(status) else {
        return out;
    };
    out.push(prepend_line_prefix(
        Line::from(Span::styled(
            label,
            Style::default().fg(Color::DarkGray).bg(user_bg),
        )),
        "▌ ",
        gutter,
        bg,
    ));
    out
}

pub(super) fn turn_status_label(status: TurnRenderStatus) -> Option<&'static str> {
    match status {
        TurnRenderStatus::Normal => None,
        TurnRenderStatus::Queued => Some("queued to stdin; waiting for harness echo"),
        TurnRenderStatus::Waiting => Some("accepted; waiting for model output"),
        TurnRenderStatus::EmptyResult => Some("turn ended with no model output"),
    }
}
