//! Slice selector vocabulary + pure range resolvers, the harness-side mirror of
//! the daemon's `src/slices.rs`.
//!
//! **First-cut duplication, by design.** `crates/bro-tools` is deliberately
//! daemon-dependency-free (see `lib.rs`), and the daemon's selector types live
//! in the `blackbox` crate behind a wall of `RefactorPlan`/project-registry
//! machinery the harness has no business pulling in. The resolver itself is
//! ~200 lines of pure `&str` math with no daemon coupling, so we copy the two
//! selector enums + the resolver here and keep the wire vocabulary identical
//! (same serde `tag`/`rename_all`), so `clip_*` selectors read exactly like
//! `bbox_slice_*` selectors to the model. The clean end-state is a shared leaf
//! crate both depend on; that extraction is deferred until this is proven.
//! See `design/bro-harness/bro-harness-clipboard.md` §"Selector reuse".

use anyhow::{Result, anyhow, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Hex-encoded SHA-256 of a byte slice. Mirrors `blackbox::sha256_hex` so a
/// register's `file_sha256` is comparable against a daemon-side hash.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// How to select a source range for extraction. Wire-identical to the daemon's
/// `SliceRangeSelector` (`src/slices.rs`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SliceRangeSelector {
    /// 1-based inclusive line range. The selected bytes include line endings.
    Lines { start_line: usize, end_line: usize },
    /// Select text between two literal markers. Markers excluded by default.
    Markers {
        start_marker: String,
        end_marker: String,
        #[serde(default)]
        include_markers: bool,
        #[serde(default)]
        occurrence: Option<usize>,
    },
    /// Select a literal text occurrence. `occurrence` is 1-based; omit it only
    /// when the text is unique.
    ExactText {
        text: String,
        #[serde(default)]
        occurrence: Option<usize>,
    },
    /// Select a UTF-8-aligned byte range, end-exclusive.
    Bytes { start: usize, end: usize },
}

/// Where to insert into a target. Wire-identical to the daemon's
/// `InsertSelector`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InsertSelector {
    /// Insert before or after a 1-based line. On an empty target, line 1 maps
    /// to byte 0.
    Line {
        line: usize,
        placement: LinePlacement,
    },
    BeforeMarker {
        marker: String,
        #[serde(default)]
        occurrence: Option<usize>,
    },
    AfterMarker {
        marker: String,
        #[serde(default)]
        occurrence: Option<usize>,
    },
    Prepend,
    Append,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LinePlacement {
    Before,
    After,
}

/// MCP-friendly: selectors may arrive as a nested JSON object or as a JSON
/// string (some providers stringify nested objects). Accept both.
pub fn slice_range_from_value(value: Value) -> Result<SliceRangeSelector> {
    match value {
        Value::String(raw) => serde_json::from_str(&raw).map_err(Into::into),
        other => serde_json::from_value(other).map_err(Into::into),
    }
}

pub fn insert_from_value(value: Value) -> Result<InsertSelector> {
    match value {
        Value::String(raw) => serde_json::from_str(&raw).map_err(Into::into),
        other => serde_json::from_value(other).map_err(Into::into),
    }
}

/// A resolved source range plus the extracted text snapshot.
#[derive(Debug, Clone)]
pub struct ResolvedSlice {
    pub byte_start: usize,
    pub byte_end: usize,
    pub line_start: usize,
    pub line_end: usize,
    pub text: String,
}

/// Resolve a source range selector against `source` and copy out the slice.
pub fn resolve_slice(source: &str, selector: &SliceRangeSelector) -> Result<ResolvedSlice> {
    let (byte_start, byte_end) = match selector {
        SliceRangeSelector::Lines {
            start_line,
            end_line,
        } => resolve_line_range(source, *start_line, *end_line)?,
        SliceRangeSelector::Markers {
            start_marker,
            end_marker,
            include_markers,
            occurrence,
        } => resolve_marker_range(
            source,
            start_marker,
            end_marker,
            *include_markers,
            *occurrence,
        )?,
        SliceRangeSelector::ExactText { text, occurrence } => {
            let start = resolve_literal_occurrence(source, text, *occurrence, "exact_text")?;
            (start, start + text.len())
        }
        SliceRangeSelector::Bytes { start, end } => resolve_byte_range(source, *start, *end)?,
    };
    let (line_start, _) = line_col(source, byte_start);
    let (line_end, _) = line_col(source, byte_end);
    Ok(ResolvedSlice {
        byte_start,
        byte_end,
        line_start,
        line_end,
        text: source[byte_start..byte_end].to_string(),
    })
}

/// Resolve an insertion point to a byte offset in `source`.
pub fn resolve_insert(source: &str, selector: &InsertSelector) -> Result<usize> {
    match selector {
        InsertSelector::Line { line, placement } => match placement {
            LinePlacement::Before => line_start_offset(source, *line),
            LinePlacement::After => line_after_offset(source, *line),
        },
        InsertSelector::BeforeMarker { marker, occurrence } => {
            resolve_literal_occurrence(source, marker, *occurrence, "marker")
        }
        InsertSelector::AfterMarker { marker, occurrence } => {
            resolve_literal_occurrence(source, marker, *occurrence, "marker")
                .map(|start| start + marker.len())
        }
        InsertSelector::Prepend => Ok(0),
        InsertSelector::Append => Ok(source.len()),
    }
}

fn resolve_line_range(source: &str, start_line: usize, end_line: usize) -> Result<(usize, usize)> {
    if start_line == 0 || end_line == 0 || start_line > end_line {
        bail!("error.invalid_line_range: expected 1-based start_line <= end_line");
    }
    let start = line_start_offset(source, start_line)?;
    let end = match line_start_offset(source, end_line.saturating_add(1)) {
        Ok(offset) => offset,
        Err(_) => source.len(),
    };
    Ok((start, end))
}

fn resolve_byte_range(source: &str, start: usize, end: usize) -> Result<(usize, usize)> {
    if start > end || end > source.len() {
        bail!("error.invalid_byte_range: expected 0 <= start <= end <= file length");
    }
    if !source.is_char_boundary(start) || !source.is_char_boundary(end) {
        bail!("error.invalid_byte_range: range is not UTF-8 aligned");
    }
    Ok((start, end))
}

fn resolve_marker_range(
    source: &str,
    start_marker: &str,
    end_marker: &str,
    include_markers: bool,
    occurrence: Option<usize>,
) -> Result<(usize, usize)> {
    if start_marker.is_empty() || end_marker.is_empty() {
        bail!("error.empty_marker: markers must be non-empty");
    }
    let start = resolve_literal_occurrence(source, start_marker, occurrence, "start_marker")?;
    let after_start = start + start_marker.len();
    let Some(relative_end) = source[after_start..].find(end_marker) else {
        bail!("error.end_marker_not_found: end_marker was not found after start_marker");
    };
    let end_start = after_start + relative_end;
    if include_markers {
        Ok((start, end_start + end_marker.len()))
    } else {
        Ok((after_start, end_start))
    }
}

fn resolve_literal_occurrence(
    source: &str,
    needle: &str,
    occurrence: Option<usize>,
    label: &str,
) -> Result<usize> {
    if needle.is_empty() {
        bail!("error.empty_{label}: selector text must be non-empty");
    }
    let matches = source
        .match_indices(needle)
        .map(|(idx, _)| idx)
        .collect::<Vec<_>>();
    if matches.is_empty() {
        bail!("error.{label}_not_found: selector text was not found");
    }
    if let Some(occurrence) = occurrence {
        if occurrence == 0 {
            bail!("error.invalid_occurrence: occurrence is 1-based");
        }
        return matches.get(occurrence - 1).copied().ok_or_else(|| {
            anyhow!("error.{label}_not_found: occurrence {occurrence} was not found")
        });
    }
    if matches.len() > 1 {
        bail!(
            "error.ambiguous_{label}: selector matched {} times; pass occurrence",
            matches.len()
        );
    }
    Ok(matches[0])
}

fn line_start_offset(source: &str, line: usize) -> Result<usize> {
    if line == 0 {
        bail!("error.invalid_line: line is 1-based");
    }
    if source.is_empty() {
        if line == 1 {
            return Ok(0);
        }
        bail!("error.line_out_of_range: line {line} is outside the empty target");
    }
    let starts = line_starts(source);
    starts
        .get(line - 1)
        .copied()
        .ok_or_else(|| anyhow!("error.line_out_of_range: line {line} is outside the file"))
}

fn line_after_offset(source: &str, line: usize) -> Result<usize> {
    if line == 0 {
        bail!("error.invalid_line: line is 1-based");
    }
    if source.is_empty() {
        if line == 1 {
            return Ok(0);
        }
        bail!("error.line_out_of_range: line {line} is outside the empty target");
    }
    let starts = line_starts(source);
    if line > starts.len() {
        bail!("error.line_out_of_range: line {line} is outside the file");
    }
    Ok(starts.get(line).copied().unwrap_or(source.len()))
}

fn line_starts(source: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (idx, byte) in source.bytes().enumerate() {
        if byte == b'\n' && idx + 1 < source.len() {
            starts.push(idx + 1);
        }
    }
    starts
}

fn line_col(source: &str, idx: usize) -> (usize, usize) {
    let idx = idx.min(source.len());
    let line = source[..idx].bytes().filter(|b| *b == b'\n').count() + 1;
    let line_start = source[..idx].rfind('\n').map(|pos| pos + 1).unwrap_or(0);
    (line, idx - line_start + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_selector_includes_endings() {
        let src = "a\nb\nc\n";
        let r = resolve_slice(
            src,
            &SliceRangeSelector::Lines {
                start_line: 1,
                end_line: 2,
            },
        )
        .unwrap();
        assert_eq!(r.text, "a\nb\n");
        assert_eq!(r.line_start, 1);
    }

    #[test]
    fn exact_text_ambiguous_needs_occurrence() {
        let src = "foo bar foo";
        let err = resolve_slice(
            src,
            &SliceRangeSelector::ExactText {
                text: "foo".into(),
                occurrence: None,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("ambiguous"));
        let r = resolve_slice(
            src,
            &SliceRangeSelector::ExactText {
                text: "foo".into(),
                occurrence: Some(2),
            },
        )
        .unwrap();
        assert_eq!(r.byte_start, 8);
    }

    #[test]
    fn insert_append_and_prepend() {
        let src = "hello";
        assert_eq!(resolve_insert(src, &InsertSelector::Prepend).unwrap(), 0);
        assert_eq!(resolve_insert(src, &InsertSelector::Append).unwrap(), 5);
    }

    #[test]
    fn marker_range_excludes_markers_by_default() {
        let src = "<a>middle</a>";
        let r = resolve_slice(
            src,
            &SliceRangeSelector::Markers {
                start_marker: "<a>".into(),
                end_marker: "</a>".into(),
                include_markers: false,
                occurrence: None,
            },
        )
        .unwrap();
        assert_eq!(r.text, "middle");
    }

    #[test]
    fn sha256_is_stable() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
