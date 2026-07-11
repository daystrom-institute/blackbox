use std::path::Path;

use anyhow::Result;
use regex::Regex;

use super::{Chunk, Edge, SourceFormatChunker, placeholder_chunk};

/// Target size (bytes) for a merged `transcript_segment` chunk. Matches the
/// house chunk size used by `text::PlainTextChunker` (`TARGET_CHUNK_BYTES`
/// there) rather than inventing a new number for this format.
const TARGET_CHUNK_BYTES: usize = 1024;

/// Time-segmented chunker for external transcript producer output (X-AV,
/// `design/corpus/agentic-corpus/agentic-corpus-multimodal-chunkers.md`).
/// This chunker parses transcript FILES emitted by an external tool such as
/// whisper (WebVTT `.vtt`, SubRip `.srt`) — it never runs whisper and never
/// touches media (audio/video) files directly.
///
/// Consecutive cues are merged into `transcript_segment` chunks of roughly
/// `TARGET_CHUNK_BYTES`, mirroring `text::PlainTextChunker`'s paragraph
/// merging. There is no dedicated time-range field on `Chunk` (see the field
/// doc comments in `lib.rs`), so — following the precedent set by
/// `pdf::PdfChunker` repurposing `line_start`/`line_end` to carry page
/// numbers — this chunker carries the merged window's start/end timestamp in
/// those same fields, as **whole seconds (floor)**, not `hh:mm:ss`. Whole
/// seconds was chosen over a formatted timestamp string because
/// `line_start`/`line_end` are typed `Option<u32>`, so a numeric unit is the
/// only representation that fits without widening the `Chunk` schema for
/// this pass.
///
/// Per the X-AV spec, `AT_TIMESTAMP`/`IN_RECORDING` edges are deferred: this
/// pass emits zero edges, matching every other chunker in this crate's
/// registry (`SourceFormatChunker::chunk` always returns an empty edge
/// vec here). Wiring those edge families into the live EdgeIndex channel is
/// out of scope until that channel is confirmed live for this chunker.
///
/// Degradation posture matches `pdf::PdfChunker`: malformed input (invalid
/// UTF-8, missing `WEBVTT` header, unparseable cue timing, empty/garbage
/// content) degrades to zero chunks with a `tracing::warn!`, never a panic
/// and never an `Err` — `SourceFormatChunker::chunk` returning `Err`
/// propagates all the way up and aborts the entire background reindex pass,
/// not just this file.
pub struct AvTranscriptChunker;

impl SourceFormatChunker for AvTranscriptChunker {
    fn format_id(&self) -> &str {
        "av_transcript"
    }

    fn claims(&self, path: &Path, _sniff: &[u8]) -> bool {
        matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("vtt" | "srt")
        )
    }

    fn chunk(&self, path: &Path, bytes: &[u8]) -> Result<(Vec<Chunk>, Vec<Edge>)> {
        if bytes.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }

        let text = match std::str::from_utf8(bytes) {
            Ok(text) => text,
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "transcript file is not valid utf-8; skipping"
                );
                return Ok((Vec::new(), Vec::new()));
            }
        };

        let normalized = normalize_transcript_text(text);
        let is_srt = path.extension().and_then(|ext| ext.to_str()) == Some("srt");

        if !is_srt && !has_webvtt_header(&normalized) {
            tracing::warn!(
                path = %path.display(),
                "vtt file missing WEBVTT header; skipping"
            );
            return Ok((Vec::new(), Vec::new()));
        }

        // Constructed once per file (not per line/cue); this crate doesn't
        // use a lazy-static regex cache elsewhere (see
        // `markdown::markdown_edge_kinds`), so a fresh `Regex::new` per
        // `chunk()` call matches existing convention.
        let voice_re = Regex::new(r"^\s*<v[ \t]+([^>.]+)[^>]*>").expect("valid voice-tag regex");
        let tag_re = Regex::new(r"<[^>]*>").expect("valid markup-tag regex");

        let cues = parse_cue_blocks(&normalized, &voice_re, &tag_re);
        let chunks = merge_cues_into_chunks(path, &cues);
        if chunks.is_empty() {
            tracing::warn!(
                path = %path.display(),
                "no transcript cues extracted from file; producing zero chunks"
            );
        }
        Ok((chunks, Vec::new()))
    }
}

/// A single parsed cue: markup-stripped, speaker-prefixed text plus its time
/// window and its byte range within the normalized file text.
struct RawCue {
    start_secs: f64,
    end_secs: f64,
    text: String,
    byte_start: usize,
    byte_end: usize,
}

/// Strip a leading BOM and normalize line endings (CRLF and lone CR) to LF.
/// Both WebVTT and SRT files in the wild are produced by a mix of tools;
/// being lenient about BOM/CRLF avoids spuriously failing to find cue
/// boundaries on otherwise well-formed files.
fn normalize_transcript_text(text: &str) -> String {
    let without_bom = text.strip_prefix('\u{feff}').unwrap_or(text);
    without_bom.replace("\r\n", "\n").replace('\r', "\n")
}

fn has_webvtt_header(text: &str) -> bool {
    text.trim_start().starts_with("WEBVTT")
}

/// Split `text` into blank-line-separated blocks and parse each into a cue.
/// Blocks with no parseable timing line (the `WEBVTT` header block, `NOTE`/
/// `STYLE`/`REGION` blocks, or a block whose timing line is malformed) are
/// skipped rather than aborting the whole file — this is the "malformed cue
/// timing skips the cue, not the file" leniency the spec calls for.
fn parse_cue_blocks<'a>(text: &'a str, voice_re: &Regex, tag_re: &Regex) -> Vec<RawCue> {
    let mut cues = Vec::new();
    let mut offset = 0usize;
    let mut block: Vec<&'a str> = Vec::new();
    let mut block_start = 0usize;
    let mut block_end = 0usize;

    for line in text.split_inclusive('\n') {
        let line_start = offset;
        offset += line.len();
        let stripped = line.trim_end_matches('\n');
        if stripped.trim().is_empty() {
            // `parse_cue_block` returns `None` for an empty slice (no line
            // contains `-->`), so it is safe to call unconditionally here
            // rather than guarding on `!block.is_empty()` first.
            if let Some(cue) = parse_cue_block(&block, block_start, block_end, voice_re, tag_re) {
                cues.push(cue);
            }
            block.clear();
            continue;
        }
        if block.is_empty() {
            block_start = line_start;
        }
        block.push(stripped);
        block_end = line_start + stripped.len();
    }
    if let Some(cue) = parse_cue_block(&block, block_start, block_end, voice_re, tag_re) {
        cues.push(cue);
    }
    cues
}

/// Parse one blank-line-delimited block into a cue. Tolerant of an optional
/// leading cue-identifier line (SRT's numeric index, or a WebVTT cue id):
/// the timing line is located by scanning for the first line containing
/// `-->` rather than assuming a fixed line position.
fn parse_cue_block(
    lines: &[&str],
    byte_start: usize,
    byte_end: usize,
    voice_re: &Regex,
    tag_re: &Regex,
) -> Option<RawCue> {
    let timing_idx = lines.iter().position(|line| line.contains("-->"))?;
    let (start_secs, end_secs) = parse_timing_line(lines[timing_idx])?;
    let text_lines = &lines[timing_idx + 1..];
    if text_lines.is_empty() {
        return None;
    }
    let text = clean_cue_text(text_lines, voice_re, tag_re);
    if text.is_empty() {
        return None;
    }
    Some(RawCue {
        start_secs,
        end_secs,
        text,
        byte_start,
        byte_end,
    })
}

/// Parse a `<start> --> <end> [cue settings]` timing line. Trailing WebVTT
/// cue settings (e.g. `align:start line:0`) after the end timestamp are
/// ignored by taking only the first whitespace-delimited token.
fn parse_timing_line(line: &str) -> Option<(f64, f64)> {
    let (left, right) = line.split_once("-->")?;
    let start = parse_timestamp(left.trim())?;
    let end_token = right.split_whitespace().next()?;
    let end = parse_timestamp(end_token)?;
    if end < start {
        return None;
    }
    Some((start, end))
}

/// Parse a single timestamp in either `HH:MM:SS.mmm` (WebVTT) or
/// `HH:MM:SS,mmm` (SRT) form, or the shorter `MM:SS.mmm` form WebVTT also
/// permits. Returns `None` for anything that doesn't fit one of those
/// shapes rather than guessing.
fn parse_timestamp(raw: &str) -> Option<f64> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let normalized = s.replace(',', ".");
    let (main, frac) = match normalized.split_once('.') {
        Some((main, frac)) => (main, Some(frac)),
        None => (normalized.as_str(), None),
    };
    let parts: Vec<&str> = main.split(':').collect();
    let (hours, minutes, seconds) = match parts.as_slice() {
        [h, m, s] => (
            h.parse::<u64>().ok()?,
            m.parse::<u64>().ok()?,
            s.parse::<u64>().ok()?,
        ),
        [m, s] => (0, m.parse::<u64>().ok()?, s.parse::<u64>().ok()?),
        _ => return None,
    };
    if minutes >= 60 || seconds >= 60 {
        return None;
    }
    let frac_secs = match frac {
        Some(digits) if !digits.is_empty() => {
            let taken: String = digits.chars().take(3).collect();
            let padded = format!("{taken:0<3}");
            padded.parse::<u64>().ok()? as f64 / 1000.0
        }
        _ => 0.0,
    };
    Some((hours * 3600 + minutes * 60 + seconds) as f64 + frac_secs)
}

/// Strip inline VTT markup from a cue's text lines. `<v Speaker>` (and
/// `<v Speaker.class>`) voice tags at the start of a line are converted into
/// a plain `"Speaker: "` prefix instead of being dropped; every other tag
/// (`<c>`, `<i>`, `<b>`, `<u>`, in-cue `<00:00:02.500>` timestamp tags,
/// closing tags) is stripped outright. Common HTML entities are unescaped
/// afterward so `&amp;`/`&gt;`/etc. read as plain text.
fn clean_cue_text(lines: &[&str], voice_re: &Regex, tag_re: &Regex) -> String {
    let mut cleaned_lines: Vec<String> = Vec::with_capacity(lines.len());
    for line in lines {
        let speaker = voice_re
            .captures(line)
            .and_then(|caps| caps.get(1))
            .map(|m| m.as_str().trim().to_string())
            .filter(|name| !name.is_empty());
        let stripped = tag_re.replace_all(line, "");
        let unescaped = unescape_entities(&stripped);
        let text = unescaped.trim();
        if text.is_empty() {
            continue;
        }
        match speaker {
            Some(name) => cleaned_lines.push(format!("{name}: {text}")),
            None => cleaned_lines.push(text.to_string()),
        }
    }
    cleaned_lines.join(" ")
}

fn unescape_entities(text: &str) -> String {
    text.replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// Merge consecutive cues into `transcript_segment` chunks of roughly
/// `TARGET_CHUNK_BYTES`, the same greedy-append-then-flush policy
/// `text::PlainTextChunker` uses for paragraphs. A single cue larger than
/// the target still becomes its own chunk rather than being split further.
fn merge_cues_into_chunks(path: &Path, cues: &[RawCue]) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut current_text = String::new();
    let mut window_start_secs = 0.0f64;
    let mut window_end_secs = 0.0f64;
    let mut chunk_byte_start = 0usize;
    let mut chunk_byte_end = 0usize;

    for cue in cues {
        if !current_text.is_empty() && current_text.len() + cue.text.len() + 1 > TARGET_CHUNK_BYTES
        {
            push_transcript_chunk(
                path,
                &mut chunks,
                &current_text,
                window_start_secs,
                window_end_secs,
                chunk_byte_start,
                chunk_byte_end,
            );
            current_text.clear();
        }
        if current_text.is_empty() {
            window_start_secs = cue.start_secs;
            chunk_byte_start = cue.byte_start;
        } else {
            current_text.push('\n');
        }
        current_text.push_str(&cue.text);
        window_end_secs = cue.end_secs;
        chunk_byte_end = cue.byte_end;
    }

    if !current_text.trim().is_empty() {
        push_transcript_chunk(
            path,
            &mut chunks,
            &current_text,
            window_start_secs,
            window_end_secs,
            chunk_byte_start,
            chunk_byte_end,
        );
    }

    chunks
}

fn push_transcript_chunk(
    path: &Path,
    chunks: &mut Vec<Chunk>,
    text: &str,
    start_secs: f64,
    end_secs: f64,
    byte_start: usize,
    byte_end: usize,
) {
    let mut chunk = placeholder_chunk(
        path,
        "transcript_segment",
        None,
        text.trim().to_string(),
        byte_start as u64,
        byte_end as u64,
        chunks.len() as u32,
    );
    chunk.line_start = Some(start_secs.floor().max(0.0) as u32);
    chunk.line_end = Some(end_secs.floor().max(0.0) as u32);
    chunks.push(chunk);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claims_vtt_and_srt_extensions_only() {
        assert!(AvTranscriptChunker.claims(Path::new("clip.vtt"), b""));
        assert!(AvTranscriptChunker.claims(Path::new("clip.srt"), b""));
        assert!(!AvTranscriptChunker.claims(Path::new("clip.txt"), b""));
        assert!(!AvTranscriptChunker.claims(Path::new("clip.pdf"), b""));
    }

    #[test]
    fn three_vtt_cues_with_speaker_tags_merge_into_clean_chunk() {
        let vtt = "WEBVTT\n\n\
                   00:00:01.000 --> 00:00:04.000\n\
                   <v Alice>Hello there, how are you?\n\n\
                   00:00:04.500 --> 00:00:07.000\n\
                   <v Bob>General Kenobi!\n\n\
                   00:00:07.500 --> 00:00:10.000\n\
                   <v Alice>Good, thanks for asking.\n";
        let (chunks, edges) = AvTranscriptChunker
            .chunk(Path::new("clip.vtt"), vtt.as_bytes())
            .expect("well-formed vtt fixture must not error");
        assert!(edges.is_empty());
        assert_eq!(chunks.len(), 1, "small fixture fits in one merged window");

        let chunk = &chunks[0];
        assert_eq!(chunk.chunk_kind, "transcript_segment");
        assert_eq!(chunk.line_start, Some(1));
        assert_eq!(chunk.line_end, Some(10));
        assert!(chunk.content.contains("Alice: Hello there, how are you?"));
        assert!(chunk.content.contains("Bob: General Kenobi!"));
        assert!(chunk.content.contains("Alice: Good, thanks for asking."));
        assert!(!chunk.content.contains("<v"));
        assert!(!chunk.content.contains("-->"));
        assert!(chunk.language.is_none());
    }

    #[test]
    fn srt_literal_parses_into_chunk_with_clean_text() {
        let srt = "1\n\
                   00:00:01,000 --> 00:00:04,000\n\
                   Hello there.\n\n\
                   2\n\
                   00:00:04,500 --> 00:00:07,000\n\
                   General Kenobi!\n";
        let (chunks, edges) = AvTranscriptChunker
            .chunk(Path::new("clip.srt"), srt.as_bytes())
            .expect("well-formed srt fixture must not error");
        assert!(edges.is_empty());
        assert_eq!(chunks.len(), 1);

        let chunk = &chunks[0];
        assert_eq!(chunk.chunk_kind, "transcript_segment");
        assert_eq!(chunk.line_start, Some(1));
        assert_eq!(chunk.line_end, Some(7));
        assert!(chunk.content.contains("Hello there."));
        assert!(chunk.content.contains("General Kenobi!"));
        // SRT numeric cue indices must not leak into cleaned text.
        assert!(!chunk.content.lines().any(|line| line.trim() == "1"));
        assert!(!chunk.content.lines().any(|line| line.trim() == "2"));
    }

    #[test]
    fn malformed_cue_timing_skips_only_that_cue() {
        let vtt = "WEBVTT\n\n\
                   00:00:01.000 --> 00:00:04.000\n\
                   Good cue one.\n\n\
                   not-a-timestamp --> also-not-one\n\
                   Bad cue, should be skipped.\n\n\
                   00:00:08.000 --> 00:00:10.000\n\
                   Good cue two.\n";
        let (chunks, edges) = AvTranscriptChunker
            .chunk(Path::new("clip.vtt"), vtt.as_bytes())
            .expect("chunk() must never error on a malformed cue");
        assert!(edges.is_empty());
        let combined: String = chunks.iter().map(|c| c.content.as_str()).collect();
        assert!(combined.contains("Good cue one."));
        assert!(combined.contains("Good cue two."));
        assert!(!combined.contains("Bad cue"));
        assert_eq!(chunks.first().and_then(|c| c.line_start), Some(1));
        assert_eq!(chunks.last().and_then(|c| c.line_end), Some(10));
    }

    #[test]
    fn garbage_byte_stream_produces_no_chunks_and_does_not_panic() {
        let garbage = b"this is not a transcript, just random bytes \x00\x01\x02 garbage";
        let (chunks, edges) = AvTranscriptChunker
            .chunk(Path::new("clip.vtt"), garbage)
            .expect("chunk() must never return Err for malformed input");
        assert!(chunks.is_empty());
        assert!(edges.is_empty());
    }

    #[test]
    fn vtt_without_webvtt_header_produces_no_chunks() {
        let text = "this is prose, not a subtitle file\njust some lines\n";
        let (chunks, edges) = AvTranscriptChunker
            .chunk(Path::new("clip.vtt"), text.as_bytes())
            .expect("chunk() must never return Err for malformed input");
        assert!(chunks.is_empty());
        assert!(edges.is_empty());
    }

    #[test]
    fn srt_with_no_cue_markers_produces_no_chunks() {
        let text = "this is prose, not a subtitle file\njust some lines\n";
        let (chunks, edges) = AvTranscriptChunker
            .chunk(Path::new("clip.srt"), text.as_bytes())
            .expect("chunk() must never return Err for malformed input");
        assert!(chunks.is_empty());
        assert!(edges.is_empty());
    }

    #[test]
    fn empty_byte_stream_produces_no_chunks_and_does_not_panic() {
        let (chunks, edges) = AvTranscriptChunker
            .chunk(Path::new("clip.vtt"), b"")
            .expect("chunk() must never return Err for empty input");
        assert!(chunks.is_empty());
        assert!(edges.is_empty());
    }
}
