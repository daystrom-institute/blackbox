//! Bounded tool results (design/fleet-tui/fleet-tui.md §2.3).
//!
//! Any tool result over a configured size is spilled to a harness-owned dump
//! file and replaced inline with a head + a rider pointing at the file. This is:
//!
//! - **lossless** — the full payload is retrievable via `file_read`/`smart_read`
//!   at the dumped path;
//! - **context-hygienic** — the model (not just a TUI) isn't flooded;
//! - **uniform** — applied at the loop level to every tool result, built-in or
//!   MCP, instead of each tool self-truncating and discarding the overflow.
//!
//! `N` is a config knob (`BRO_HARNESS_TOOL_RESULT_CAP_KB`, default 16; `0`
//! disables bounding).

use std::path::{Path, PathBuf};

const DEFAULT_CAP_KB: usize = 16;
/// How much of an oversized payload to keep inline as a head preview.
const HEAD_BYTES: usize = 2048;

/// Spill threshold in bytes from `BRO_HARNESS_TOOL_RESULT_CAP_KB` (default
/// 16 KB). `0` disables bounding.
pub fn cap_bytes() -> usize {
    std::env::var("BRO_HARNESS_TOOL_RESULT_CAP_KB")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(DEFAULT_CAP_KB)
        .saturating_mul(1024)
}

/// Directory harness dumps are written to (`$BRO_HOME/harness-dumps`, else a
/// temp subdir).
pub fn dump_dir() -> PathBuf {
    if let Ok(home) = std::env::var("BRO_HOME") {
        PathBuf::from(home).join("harness-dumps")
    } else {
        std::env::temp_dir().join("bro-harness-dumps")
    }
}

/// If `content` exceeds `cap`, spill the full payload to a file under `dir` and
/// return a head + rider; otherwise return `content` unchanged. `cap == 0`
/// disables bounding. `id` (the tool_use id) makes the filename unique.
// oversized-result spill is rare; offload tracked in thread-935b467d.
#[allow(clippy::disallowed_methods)]
pub fn bound_tool_result(tool: &str, content: String, cap: usize, dir: &Path, id: &str) -> String {
    if cap == 0 || content.len() <= cap {
        return content;
    }
    let stem = sanitize(&format!("{tool}-{id}"));
    let path = dir.join(format!("{stem}.txt"));
    if std::fs::create_dir_all(dir)
        .and_then(|_| std::fs::write(&path, &content))
        .is_err()
    {
        // Spill failed: hard-truncate so we never flood context regardless.
        return format!(
            "{}\n\n[response truncated to {} bytes; dump write failed]",
            head(&content, HEAD_BYTES),
            HEAD_BYTES
        );
    }
    let kb = content.len().div_ceil(1024);
    format!(
        "{}\n\n[huge response ({kb} kB) written to disk; read the full payload with \
         file_read/smart_read at {}]",
        head(&content, HEAD_BYTES),
        path.display()
    )
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// First `max` bytes of `s`, snapped down to a char boundary.
fn head(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let end = (0..=max)
        .rev()
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(0);
    s[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn under_cap_passes_through() {
        let dir = std::env::temp_dir().join("bro-harness-bound-test-passthrough");
        let out = bound_tool_result("file_read", "small".into(), 1024, &dir, "tu_1");
        assert_eq!(out, "small");
        assert!(!dir.exists(), "no dump dir for small results");
    }

    #[test]
    fn cap_zero_disables() {
        let dir = std::env::temp_dir().join("bro-harness-bound-test-disabled");
        let big = "x".repeat(100_000);
        let out = bound_tool_result("file_read", big.clone(), 0, &dir, "tu_2");
        assert_eq!(out, big);
    }

    #[test]
    fn over_cap_spills_and_riders() {
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("bro-harness-bound-test-spill-{pid}"));
        let _ = std::fs::remove_dir_all(&dir);
        let big = "y".repeat(50_000);
        let out = bound_tool_result("mcp__srv__tool", big.clone(), 16 * 1024, &dir, "tu/3");
        assert!(out.contains("huge response"), "rider present: {out}");
        assert!(out.contains("file_read/smart_read"));
        assert!(out.len() < big.len(), "inline shrunk");
        // The full payload landed on disk, sanitized filename, fully recoverable.
        let path = dir.join("mcp__srv__tool-tu_3.txt");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), big);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
