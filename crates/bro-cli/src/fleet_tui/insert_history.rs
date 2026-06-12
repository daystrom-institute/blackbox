//! Insert finalized transcript lines into the terminal's *real* scrollback,
//! above the inline viewport, so tmux / mouse-wheel / copy-mode scroll and
//! select them natively — the way codex CLI behaves.
//!
//! Slimmed port of codex insert_history.rs (Apache-2.0): the "Standard"
//! scroll-region insertion path. Two simplifications vs codex: lines are
//! pre-wrapped with our own `wrapping::word_wrap_line` (no adaptive/URL lane),
//! and the OSC-8 hyperlink decoration layer is dropped (orthogonal to
//! scrolling). The mechanism is raw terminal escapes (DECSTBM scroll region +
//! reverse-index), so it does not depend on newer ratatui Backend APIs.

use std::fmt;
use std::io::{self, Write};

use crossterm::Command;
use crossterm::cursor::MoveTo;
use crossterm::queue;
use crossterm::style::{Attribute, Color as CColor, Colors, Print, SetAttribute, SetColors};
use crossterm::terminal::{Clear, ClearType};
use ratatui::layout::Size;
use ratatui::prelude::Backend;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use unicode_width::UnicodeWidthStr;

use super::custom_terminal::Terminal;
use super::wrapping::word_wrap_line;

/// Pre-wrap `lines` to the viewport width and insert them into scrollback
/// directly above the inline viewport, scrolling the live viewport down to make
/// room when it isn't already at the bottom of the screen. Cursor-position
/// neutral. No-op when `lines` is empty.
pub(super) fn insert_history_lines<B>(
    terminal: &mut Terminal<B>,
    lines: Vec<Line<'static>>,
) -> io::Result<()>
where
    B: Backend + Write,
{
    let screen_size = terminal.backend().size().unwrap_or(Size::new(0, 0));
    let mut area = terminal.viewport_area;
    let last_cursor_pos = terminal.last_known_cursor_pos;
    let wrap_width = area.width.max(1) as usize;

    // Pre-wrap to the viewport width so scrollback holds properly wrapped,
    // style-preserved lines (not terminal hard-wraps mid-word).
    let mut wrapped: Vec<Line<'static>> = Vec::new();
    for line in &lines {
        wrapped.extend(word_wrap_line(line, wrap_width));
    }
    let wrapped_rows: usize = wrapped
        .iter()
        .map(|l| line_display_width(l).max(1).div_ceil(wrap_width))
        .sum();
    let wrapped_lines = wrapped_rows as u16;
    if wrapped_lines == 0 {
        return Ok(());
    }

    let mut should_update_area = false;
    {
        let writer = terminal.backend_mut();
        let cursor_top = if area.bottom() < screen_size.height {
            // Viewport isn't at the screen bottom: scroll it down (reverse-index
            // within a region anchored below the viewport top) to open room.
            let scroll_amount = wrapped_lines.min(screen_size.height - area.bottom());
            let top_1based = area.top() + 1;
            queue!(writer, SetScrollRegion(top_1based..screen_size.height))?;
            queue!(writer, MoveTo(0, area.top()))?;
            for _ in 0..scroll_amount {
                queue!(writer, Print("\x1bM"))?; // reverse index
            }
            queue!(writer, ResetScrollRegion)?;
            let cursor_top = area.top().saturating_sub(1);
            area.y += scroll_amount;
            should_update_area = true;
            cursor_top
        } else {
            area.top().saturating_sub(1)
        };

        // Constrain scrolling to the region above the viewport, place the cursor
        // at its bottom edge, and write lines there — they scroll up into
        // scrollback as we print.
        queue!(writer, SetScrollRegion(1..area.top()))?;
        queue!(writer, MoveTo(0, cursor_top))?;
        for line in &wrapped {
            queue!(writer, Print("\r\n"))?;
            write_history_line(writer, line, wrap_width)?;
        }
        queue!(writer, ResetScrollRegion)?;
        // MoveTo (not set_cursor_position) keeps the terminal's tracked cursor
        // position intact — insertion is cursor-neutral.
        queue!(writer, MoveTo(last_cursor_pos.x, last_cursor_pos.y))?;
        Write::flush(writer)?;
    }

    if should_update_area {
        terminal.set_viewport_area(area);
    }
    terminal.note_history_rows_inserted(wrapped_lines);
    Ok(())
}

fn line_display_width(line: &Line<'_>) -> usize {
    line.spans.iter().map(|s| s.content.width()).sum()
}

/// Write one already-wrapped line: paint the line background to EOL, then emit
/// each span with its (line-merged) style.
fn write_history_line<W: Write>(
    writer: &mut W,
    line: &Line<'_>,
    _wrap_width: usize,
) -> io::Result<()> {
    let line_fg = line.style.fg.map(Into::into).unwrap_or(CColor::Reset);
    let line_bg = line.style.bg.map(Into::into).unwrap_or(CColor::Reset);
    queue!(writer, SetColors(Colors::new(line_fg, line_bg)))?;
    queue!(writer, Clear(ClearType::UntilNewLine))?;
    for span in &line.spans {
        write_span(writer, span.content.as_ref(), span.style.patch(line.style))?;
    }
    queue!(writer, SetAttribute(Attribute::Reset))?;
    queue!(writer, SetColors(Colors::new(CColor::Reset, CColor::Reset)))?;
    Ok(())
}

fn write_span<W: Write>(writer: &mut W, content: &str, style: Style) -> io::Result<()> {
    let fg = style.fg.map(Into::into).unwrap_or(CColor::Reset);
    let bg = style.bg.map(Into::into).unwrap_or(CColor::Reset);
    queue!(writer, SetColors(Colors::new(fg, bg)))?;
    let m = style.add_modifier;
    if m.contains(Modifier::BOLD) {
        queue!(writer, SetAttribute(Attribute::Bold))?;
    }
    if m.contains(Modifier::DIM) {
        queue!(writer, SetAttribute(Attribute::Dim))?;
    }
    if m.contains(Modifier::ITALIC) {
        queue!(writer, SetAttribute(Attribute::Italic))?;
    }
    if m.contains(Modifier::UNDERLINED) {
        queue!(writer, SetAttribute(Attribute::Underlined))?;
    }
    queue!(writer, Print(content))?;
    queue!(writer, SetAttribute(Attribute::Reset))?;
    Ok(())
}

/// `\x1b[{top};{bottom}r` — set the DECSTBM scroll region (1-based, inclusive).
#[derive(Debug, Clone, PartialEq, Eq)]
struct SetScrollRegion(std::ops::Range<u16>);

impl Command for SetScrollRegion {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[{};{}r", self.0.start, self.0.end)
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        panic!("SetScrollRegion requires ANSI");
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

/// `\x1b[r` — reset the scroll region to the full screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ResetScrollRegion;

impl Command for ResetScrollRegion {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[r")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        panic!("ResetScrollRegion requires ANSI");
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}
