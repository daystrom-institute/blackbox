//! Bounded, pageable stream capture. Preparation may decode or filter bytes,
//! but only commit removes selected text after its complete JSON receipt fits.
//! Byte counters describe buffer storage (raw input or decoded selected text),
//! not source offsets. invalid_utf8_bytes counts original invalid input bytes.
use regex::Regex;
use serde_json::{Value, json};
use std::collections::VecDeque;

pub(super) const MAX_BUF_BYTES: usize = 8 * 1024 * 1024;
const MAX_FILTER_LINE_BYTES: usize = 256 * 1024;

#[derive(Default)]
pub(super) struct OutBuf {
    raw: VecDeque<u8>,
    selected: VecDeque<u8>,
    eof: bool,
    partial_line: bool,
    oversized_line: bool,
    dropped_bytes: usize,
    invalid_utf8_bytes: usize,
    filtered_bytes: usize,
    kept_lines: usize,
    dropped_lines: usize,
    oversized_lines: usize,
    oversized_bytes: usize,
    partial_line_bytes: usize,
    reader_error: Option<String>,
}

fn scalar(bytes: &[u8], eof: bool) -> Option<(char, usize, bool)> {
    let first = *bytes.first()?;
    let width = match first {
        0..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return Some(('\u{fffd}', 1, true)),
    };
    if bytes.len() < width {
        // An invalid continuation is already decidable without awaiting EOF.
        if !eof && bytes[1..].iter().all(|b| b & 0xc0 == 0x80) {
            return None;
        }
        return Some(('\u{fffd}', 1, true));
    }
    match std::str::from_utf8(&bytes[..width]) {
        Ok(s) => Some((s.chars().next().unwrap(), width, false)),
        Err(_) => Some(('\u{fffd}', 1, true)),
    }
}

fn escaped_len(c: char) -> usize {
    match c {
        '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
        '\0'..='\u{1f}' => 6,
        _ => c.len_utf8(),
    }
}

impl OutBuf {
    pub(super) fn push(&mut self, chunk: &[u8]) {
        self.raw.extend(chunk);
        self.enforce_cap();
    }

    fn enforce_cap(&mut self) {
        let mut excess = self.pending_bytes().saturating_sub(MAX_BUF_BYTES);
        let selected_drop = excess.min(self.selected.len());
        self.selected.drain(..selected_drop);
        self.dropped_bytes += selected_drop;
        excess -= selected_drop;
        // Overflow must not manufacture invalid UTF-8 in already-decoded text.
        while self.selected.front().is_some_and(|b| b & 0xc0 == 0x80) {
            self.selected.pop_front();
            self.dropped_bytes += 1;
        }
        if excess > 0 {
            self.consume_raw(excess);
            self.dropped_bytes += excess;
        }
    }

    pub(super) fn finish(&mut self, error: Option<String>) {
        self.eof = true;
        if let Some(error) = error {
            self.reader_error = Some(error.chars().take(96).collect());
        }
    }

    fn consume_raw(&mut self, count: usize) {
        if count > 0 {
            self.partial_line = self.raw[count - 1] != b'\n';
            self.raw.drain(..count);
        }
    }

    pub(super) fn selected_bytes(&self) -> usize {
        self.selected.len()
    }

    pub(super) fn pending_bytes(&self) -> usize {
        self.raw.len() + self.selected.len()
    }

    /// Keep complete matching lines in selected storage before paging them.
    /// An oversized line is excluded in full, including later arriving pieces.
    pub(super) fn prepare(&mut self, target: usize, patterns: &[Regex]) {
        if target == 0 {
            return;
        }
        let mut scanned = 0;
        while self.selected.len() < target && !self.raw.is_empty() && scanned < MAX_BUF_BYTES {
            if self.oversized_line || (!patterns.is_empty() && self.partial_line) {
                let count = self
                    .raw
                    .iter()
                    .position(|b| *b == b'\n')
                    .map_or(self.raw.len(), |i| i + 1);
                if self.oversized_line {
                    self.oversized_bytes += count;
                } else {
                    self.partial_line_bytes += count;
                }
                self.consume_raw(count);
                scanned += count;
                if !self.partial_line {
                    self.oversized_line = false;
                }
                continue;
            }
            if patterns.is_empty() {
                let bytes = self.raw.make_contiguous();
                let Some((c, used, invalid)) = scalar(bytes, self.eof) else {
                    break;
                };
                let mut encoded = [0; 4];
                self.selected.extend(c.encode_utf8(&mut encoded).as_bytes());
                self.invalid_utf8_bytes += usize::from(invalid);
                self.consume_raw(used);
                scanned += used;
                continue;
            }
            let newline = self.raw.iter().position(|b| *b == b'\n');
            let count = newline.map_or(self.raw.len(), |i| i + 1);
            if count > MAX_FILTER_LINE_BYTES {
                self.oversized_lines += 1;
                self.oversized_bytes += count;
                self.consume_raw(count);
                self.oversized_line = self.partial_line;
                scanned += count;
                continue;
            }
            if newline.is_none() && !self.eof {
                break;
            }
            let raw: Vec<_> = self.raw.iter().take(count).copied().collect();
            let mut text = String::new();
            let mut offset = 0;
            while let Some((c, used, invalid)) = scalar(&raw[offset..], true) {
                text.push(c);
                offset += used;
                self.invalid_utf8_bytes += usize::from(invalid);
            }
            self.consume_raw(count);
            scanned += count;
            if patterns.iter().any(|p| p.is_match(&text)) {
                self.kept_lines += 1;
                self.selected.extend(text.as_bytes());
            } else {
                self.dropped_lines += 1;
                self.filtered_bytes += count;
            }
        }
        // Invalid-byte replacements can expand decoded storage; apply the
        // same newest-tail retention rule before exposing a page.
        self.enforce_cap();
    }

    pub(super) fn preview(&mut self, escaped_budget: usize) -> String {
        let text =
            std::str::from_utf8(self.selected.make_contiguous()).expect("selected UTF-8 invariant");
        let mut used = 0;
        let mut end = 0;
        for (index, c) in text.char_indices() {
            used += escaped_len(c);
            if used > escaped_budget {
                break;
            }
            end = index + c.len_utf8();
        }
        text[..end].to_owned()
    }

    pub(super) fn commit(&mut self, bytes: usize) {
        self.selected.drain(..bytes);
    }

    pub(super) fn metadata(&self, consumed: usize) -> Value {
        let mut result = json!({"pending_bytes": self.pending_bytes().saturating_sub(consumed)});
        for (key, count) in [
            ("dropped_bytes", self.dropped_bytes),
            ("invalid_utf8_bytes", self.invalid_utf8_bytes),
            ("filtered_bytes", self.filtered_bytes),
            ("oversized_lines", self.oversized_lines),
            ("oversized_bytes", self.oversized_bytes),
            ("partial_line_bytes", self.partial_line_bytes),
        ] {
            if count > 0 {
                result[key] = json!(count);
            }
        }
        if let Some(error) = &self.reader_error {
            result["reader_error"] = json!(error);
            result["capture_incomplete"] = json!(true);
        }
        result
    }

    pub(super) fn filter_report(&self) -> Value {
        json!({"mode": "matching_lines", "kept_lines": self.kept_lines, "dropped_lines": self.dropped_lines})
    }

    pub(super) fn discard(&mut self) -> usize {
        let count = self.pending_bytes();
        self.raw.clear();
        self.selected.clear();
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(buffer: &mut OutBuf, budget: usize, filter: &[Regex]) -> String {
        buffer.prepare(budget, filter);
        let text = buffer.preview(budget);
        buffer.commit(text.len());
        text
    }

    #[test]
    fn preserves_utf8_across_chunks_and_pages_and_finishes_invalid_tail() {
        let mut buffer = OutBuf::default();
        buffer.push(&[b'a', 0xf0, 0x9f]);
        assert_eq!(page(&mut buffer, 6, &[]), "a");
        buffer.push(&[0x98, 0x80, b'b', 0xe2]);
        assert_eq!(page(&mut buffer, 4, &[]), "😀");
        assert_eq!(page(&mut buffer, 6, &[]), "b");
        buffer.finish(None);
        assert_eq!(page(&mut buffer, 6, &[]), "�");
        assert_eq!(buffer.metadata(0)["invalid_utf8_bytes"], 1);
        assert_eq!(buffer.pending_bytes(), 0);
    }

    #[test]
    fn overflow_keeps_newest_tail_and_counts_loss() {
        let mut buffer = OutBuf::default();
        buffer.push(&vec![b'x'; MAX_BUF_BYTES]);
        buffer.push(b"FINAL_ERROR\n");
        assert_eq!(buffer.pending_bytes(), MAX_BUF_BYTES);
        assert_eq!(buffer.metadata(0)["dropped_bytes"], 12);
        buffer.prepare(MAX_BUF_BYTES, &[]);
        let result = buffer.preview(MAX_BUF_BYTES + 2);
        assert!(result.ends_with("FINAL_ERROR\n"));
    }

    #[test]
    fn invalid_byte_expansion_stays_bounded_and_keeps_valid_selected_text() {
        let mut buffer = OutBuf::default();
        buffer.push(&vec![0xff; MAX_BUF_BYTES]);
        buffer.prepare(8192, &[]);
        assert!(buffer.pending_bytes() <= MAX_BUF_BYTES);
        assert!(!buffer.preview(8192).is_empty());
        assert!(buffer.metadata(0)["dropped_bytes"].as_u64().unwrap() > 0);
        assert!(buffer.metadata(0)["invalid_utf8_bytes"].as_u64().unwrap() > 0);
    }

    #[test]
    fn full_line_filter_survives_chunk_and_page_boundaries() {
        let mut buffer = OutBuf::default();
        let patterns = [Regex::new("^keep.*end\\n$").unwrap()];
        buffer.push(b"keep ");
        assert_eq!(page(&mut buffer, 6, &patterns), "");
        buffer.push(b"middle end\nignore\n");
        let mut result = String::new();
        while buffer.pending_bytes() > 0 {
            result.push_str(&page(&mut buffer, 6, &patterns));
        }
        assert_eq!(result, "keep middle end\n");
        assert_eq!(buffer.filter_report()["kept_lines"], 1);
        assert_eq!(buffer.filter_report()["dropped_lines"], 1);
    }

    #[test]
    fn oversized_filter_line_is_never_matched_as_fragments() {
        let mut buffer = OutBuf::default();
        let patterns = [Regex::new("keep").unwrap()];
        buffer.push(&vec![b'x'; MAX_FILTER_LINE_BYTES + 1]);
        assert_eq!(page(&mut buffer, 128, &patterns), "");
        buffer.push(b"keep\nkeep next\n");
        assert_eq!(page(&mut buffer, 128, &patterns), "keep next\n");
        assert_eq!(buffer.metadata(0)["oversized_lines"], 1);
        assert_eq!(
            buffer.metadata(0)["oversized_bytes"],
            MAX_FILTER_LINE_BYTES + 6
        );
    }

    #[test]
    fn preview_is_nonconsuming_and_accounts_for_json_escaping() {
        let mut buffer = OutBuf::default();
        buffer.push(b"\x01\"\\");
        buffer.finish(None);
        buffer.prepare(10, &[]);
        assert_eq!(buffer.preview(0), "");
        assert_eq!(buffer.preview(5), "");
        let text = buffer.preview(6);
        assert_eq!(text, "\x01");
        assert_eq!(serde_json::to_string(&text).unwrap().len(), 8);
        assert_eq!(buffer.pending_bytes(), 3);
        buffer.commit(text.len());
        assert_eq!(buffer.preview(4), "\"\\");
    }
}
