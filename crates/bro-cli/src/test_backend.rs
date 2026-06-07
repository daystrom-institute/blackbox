//! Headless render harness for snapshot tests.
//!
//! Adopts the technique from the OpenAI codex TUI
//! (`codex-rs/tui/src/test_backend.rs`, Apache-2.0): render widgets into a real
//! terminal cell grid headlessly and snapshot the result, instead of asserting
//! against hand-written expected strings. Codex wraps a `vt100::Parser` to do
//! this; we use ratatui's built-in `TestBackend`, which yields the same grid
//! `Display` without pulling vt100 (whose unicode-width ^0.2.1 requirement
//! conflicts with the workspace's pinned 0.2.0).

use ratatui::backend::TestBackend;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};

/// Render lines into a `width`×`height` terminal grid and return the screen
/// contents as text, with trailing blank rows trimmed. Uses the same
/// `Wrap { trim: false }` the live draw path applies (see `draw_single_agent`),
/// so the grid faithfully reproduces on-screen layout — including soft-wrapping
/// of long logical lines — at the given width.
pub fn render_lines_to_grid(width: u16, height: u16, lines: Vec<Line<'static>>) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).expect("construct test terminal");
    terminal
        .draw(|f| {
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), f.area());
        })
        .expect("draw paragraph");
    let dump = format!("{}", terminal.backend());
    let mut rows: Vec<String> = dump.lines().map(|l| l.trim_end().to_string()).collect();
    while rows.last().is_some_and(|l| l.is_empty()) {
        rows.pop();
    }
    rows.join("\n")
}
