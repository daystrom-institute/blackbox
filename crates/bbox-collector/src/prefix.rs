//! Strict-prefix byte reader for append-only JSONL transcripts.
//!
//! This is the SHIPPING read path, deliberately distinct from the lenient
//! adapter cursor in `bbox_transcript_read::interactive`. That adapter advances
//! its byte cursor to EOF even past a torn (crash-truncated) final line, which
//! is fine for reindex-time scans (the next pass re-reads the completed line)
//! but silently DROPS events in a shipper. See the footgun note in
//! `crates/bbox-transcript-read/AGENTS.md`.
//!
//! Here we read raw bytes from `start` and return only through the LAST
//! complete newline, so a torn tail is never shipped and the byte range always
//! ends on `\n` (the exact invariant the server enforces). Reads are bounded to
//! [`MAX_READ_WINDOW`] per call so a cold catch-up from cursor zero advances in
//! bounded steps instead of buffering an entire large file.

use std::io::SeekFrom;
use std::path::Path;

use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};

/// Upper bound on bytes buffered per stream per tick. Cold catch-up ships this
/// much (rounded down to the last complete line) then continues next tick.
pub const MAX_READ_WINDOW: u64 = 8 * 1024 * 1024;

/// Read the complete-line prefix of `path` starting at byte `start`.
///
/// Returns the raw bytes `[start, end)` where `end - 1` is a `\n`, bounded to
/// [`MAX_READ_WINDOW`]. Returns an empty vec when there is no complete line
/// beyond `start` (nothing to ship this tick: either EOF or only a torn tail).
pub async fn read_complete_line_prefix(path: &Path, start: u64) -> std::io::Result<Vec<u8>> {
    let mut file = tokio::fs::File::open(path).await?;
    file.seek(SeekFrom::Start(start)).await?;
    let mut buf = vec![0u8; 0];
    let mut window = (&mut file).take(MAX_READ_WINDOW);
    window.read_to_end(&mut buf).await?;

    match buf.iter().rposition(|byte| *byte == b'\n') {
        Some(last_newline) => {
            buf.truncate(last_newline + 1);
            Ok(buf)
        }
        None => Ok(Vec::new()),
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn ships_only_through_last_complete_newline() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("s.jsonl");
        // Two complete lines plus a torn (no trailing newline) tail.
        std::fs::write(&path, b"{\"a\":1}\n{\"b\":2}\ntorn-tail-no-newline").unwrap();

        let prefix = read_complete_line_prefix(&path, 0).await.unwrap();
        assert_eq!(prefix, b"{\"a\":1}\n{\"b\":2}\n");
        assert_eq!(*prefix.last().unwrap(), b'\n');
    }

    #[tokio::test]
    async fn resumes_from_a_mid_file_cursor() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("s.jsonl");
        std::fs::write(&path, b"{\"a\":1}\n{\"b\":2}\n").unwrap();
        let first_line_len = b"{\"a\":1}\n".len() as u64;

        let prefix = read_complete_line_prefix(&path, first_line_len)
            .await
            .unwrap();
        assert_eq!(prefix, b"{\"b\":2}\n");
    }

    #[tokio::test]
    async fn empty_when_only_a_torn_tail_remains() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("s.jsonl");
        std::fs::write(&path, b"{\"a\":1}\npartial-line").unwrap();
        let complete = b"{\"a\":1}\n".len() as u64;

        let prefix = read_complete_line_prefix(&path, complete).await.unwrap();
        assert!(prefix.is_empty());
    }
}
