//! Syntax highlighting for fenced code blocks.
//!
//! Focused port of codex-rs/tui/src/render/highlight.rs (Apache-2.0): syntect +
//! two-face (bat's ~250 syntaxes), CatppuccinMocha theme, with size guardrails.
//! Deviations from codex: a single fixed dark theme (no terminal-bg adaptation
//! or runtime theme-picker), and the pure-Rust fancy-regex syntect backend
//! instead of oniguruma/C — `bro` is the thin client and stays C-toolchain-free.
//!
//! Style mapping is foreground-only (terminal background shows through), keeps
//! BOLD, and drops italic/underline (poorly/inconsistently rendered in
//! terminals). Falls back to plain unstyled lines when the language is unknown
//! or the input exceeds the guardrails.

use std::sync::OnceLock;

use ratatui::style::{Color as RtColor, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Color as SyntectColor, FontStyle, Style as SyntectStyle, Theme};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;
use two_face::theme::EmbeddedThemeName;

// Bat-compatible alpha-channel encoding used by the `ansi`/`base16` themes:
// alpha 0x00 => `r` is an ANSI palette index; 0x01 => use terminal default.
const ANSI_ALPHA_INDEX: u8 = 0x00;
const ANSI_ALPHA_DEFAULT: u8 = 0x01;
const OPAQUE_ALPHA: u8 = 0xFF;

/// Skip highlighting above these limits (fall back to plain text) to bound
/// CPU/memory on pathological inputs.
const MAX_HIGHLIGHT_BYTES: usize = 512 * 1024;
const MAX_HIGHLIGHT_LINES: usize = 10_000;

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
static THEME: OnceLock<Theme> = OnceLock::new();

fn syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(two_face::syntax::extra_newlines)
}

fn theme() -> &'static Theme {
    THEME.get_or_init(|| {
        two_face::theme::extra()
            .get(EmbeddedThemeName::CatppuccinMocha)
            .clone()
    })
}

/// Highlight `code` for `lang`, returning styled ratatui lines. Falls back to
/// plain unstyled lines when the language is unrecognized or the input exceeds
/// the guardrails — callers can render the result directly either way.
pub(super) fn highlight_code_to_lines(code: &str, lang: &str) -> Vec<Line<'static>> {
    if let Some(line_spans) = highlight_to_line_spans(code, lang) {
        line_spans.into_iter().map(Line::from).collect()
    } else {
        let mut result: Vec<Line<'static>> =
            code.lines().map(|l| Line::from(l.to_string())).collect();
        if result.is_empty() {
            result.push(Line::from(String::new()));
        }
        result
    }
}

fn highlight_to_line_spans(code: &str, lang: &str) -> Option<Vec<Vec<Span<'static>>>> {
    if code.is_empty() {
        return None;
    }
    // Count actual lines (not newline bytes) to avoid an off-by-one when the
    // input does not end with a newline.
    if code.len() > MAX_HIGHLIGHT_BYTES || code.lines().count() > MAX_HIGHLIGHT_LINES {
        return None;
    }

    let syntax = find_syntax(lang)?;
    let mut h = HighlightLines::new(syntax, theme());
    let mut lines: Vec<Vec<Span<'static>>> = Vec::new();
    for line in LinesWithEndings::from(code) {
        let ranges = h.highlight_line(line, syntax_set()).ok()?;
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (style, text) in ranges {
            let text = text.trim_end_matches(['\n', '\r']);
            if text.is_empty() {
                continue;
            }
            spans.push(Span::styled(text.to_string(), convert_style(style)));
        }
        if spans.is_empty() {
            spans.push(Span::raw(String::new()));
        }
        lines.push(spans);
    }
    Some(lines)
}

/// Resolve a `SyntaxReference` for a language identifier. two-face resolves most
/// names/extensions directly; we patch the few common aliases it misses.
fn find_syntax(lang: &str) -> Option<&'static SyntaxReference> {
    let ss = syntax_set();
    let patched = match lang {
        "csharp" | "c-sharp" => "c#",
        "golang" => "go",
        "python3" => "python",
        "shell" => "bash",
        other => other,
    };
    if let Some(s) = ss.find_syntax_by_token(patched) {
        return Some(s);
    }
    if let Some(s) = ss.find_syntax_by_name(patched) {
        return Some(s);
    }
    let lower = patched.to_ascii_lowercase();
    if let Some(s) = ss
        .syntaxes()
        .iter()
        .find(|s| s.name.to_ascii_lowercase() == lower)
    {
        return Some(s);
    }
    ss.find_syntax_by_extension(lang)
}

fn convert_style(syn_style: SyntectStyle) -> Style {
    let mut rt_style = Style::default();
    if let Some(fg) = convert_syntect_color(syn_style.foreground) {
        rt_style = rt_style.fg(fg);
    }
    // Skip background (preserve terminal bg). Skip italic/underline (poor
    // terminal support; underline collides with type-scope theming).
    if syn_style.font_style.contains(FontStyle::BOLD) {
        rt_style.add_modifier |= Modifier::BOLD;
    }
    rt_style
}

fn convert_syntect_color(color: SyntectColor) -> Option<RtColor> {
    match color.a {
        ANSI_ALPHA_INDEX => Some(ansi_palette_color(color.r)),
        ANSI_ALPHA_DEFAULT => None,
        OPAQUE_ALPHA => Some(RtColor::Rgb(color.r, color.g, color.b)),
        _ => Some(RtColor::Rgb(color.r, color.g, color.b)),
    }
}

fn ansi_palette_color(index: u8) -> RtColor {
    match index {
        0x00 => RtColor::Black,
        0x01 => RtColor::Red,
        0x02 => RtColor::Green,
        0x03 => RtColor::Yellow,
        0x04 => RtColor::Blue,
        0x05 => RtColor::Magenta,
        0x06 => RtColor::Cyan,
        0x07 => RtColor::Gray,
        n => RtColor::Indexed(n),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn concat(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn highlights_known_language_with_color() {
        let lines = highlight_code_to_lines("fn main() {}", "rust");
        assert_eq!(lines.len(), 1);
        assert_eq!(concat(&lines[0]), "fn main() {}");
        // Some span must carry a foreground color (it was actually highlighted).
        assert!(
            lines[0].spans.iter().any(|s| s.style.fg.is_some()),
            "expected highlighted spans to carry color"
        );
    }

    #[test]
    fn unknown_language_falls_back_to_plain_lines() {
        let lines = highlight_code_to_lines("a\nb\nc", "no-such-lang-xyz");
        assert_eq!(lines.len(), 3);
        assert!(lines.iter().all(|l| l.spans.iter().all(|s| s.style.fg.is_none())));
    }

    #[test]
    fn oversize_input_falls_back_to_plain() {
        let big = "x\n".repeat(MAX_HIGHLIGHT_LINES + 1);
        let lines = highlight_code_to_lines(&big, "rust");
        assert!(lines.iter().all(|l| l.spans.iter().all(|s| s.style.fg.is_none())));
    }

    #[test]
    fn preserves_text_content_exactly() {
        let src = "let x = 1;\nlet y = 2;";
        let lines = highlight_code_to_lines(src, "rust");
        let rebuilt = lines.iter().map(concat).collect::<Vec<_>>().join("\n");
        assert_eq!(rebuilt, src);
    }
}
