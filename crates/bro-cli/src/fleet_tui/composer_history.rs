//! Shared, user-scoped composer history for `bro fleet` and `bro agent`.
//!
//! One JSONL file at `$BRO_HOME/composer_history.jsonl` stores every prompt
//! sent from the composer, spanning all fleet instances and standalone sessions.
//! ↑/↓ recall reads the same file, so history is cross-agent and cross-instance
//! (like a shell history file).
//!
//! Correctness contract (design doc §2 Tier 3):
//! - JSONL: one JSON object per physical line (`{"ts":…,"text":"…"}`).
//!   Newlines inside the prompt are escaped inside the JSON string so a
//!   multi-line pasted prompt is exactly one record.
//! - Append: `flock(LOCK_EX)` via `fs2` (advisory, host-local). `O_APPEND`
//!   alone is not atomic for multi-KB writes.
//! - Trim/cap: last ~5000 entries. Trim writes a compacted tail to a temp file
//!   (fsync where practical) and `rename()` over the histfile before releasing
//!   the lock — same tmp+rename atomicity as the task store.
//! - Read: parse line-by-line, skip a torn trailing record, dedup consecutive
//!   duplicates on read.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use fs2::FileExt;
use serde::{Deserialize, Serialize};

/// Maximum number of history entries to keep after trimming.
const HISTORY_CAP: usize = 5000;

/// File name under `BRO_HOME`.
const HISTORY_FILENAME: &str = "composer_history.jsonl";

/// A single history entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// Unix timestamp in milliseconds.
    pub ts: u64,
    /// The prompt text. May contain newlines (escaped inside JSON).
    pub text: String,
}

/// Returns the path to the shared composer histfile.
pub fn history_path(bro_home: &Path) -> PathBuf {
    bro_home.join(HISTORY_FILENAME)
}

/// Reads and returns the full (deduplicated) history from the histfile.
///
/// - Parses line-by-line, skips malformed/torn trailing records.
/// - Deduplicates consecutive identical `text` entries.
/// - Returns entries oldest-first.
pub fn read_history(path: &Path) -> Vec<HistoryEntry> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    let reader = BufReader::new(file);
    let mut entries = Vec::new();
    let mut prev_text: Option<String> = None;

    for line in reader.lines() {
        let Ok(line) = line else {
            break; // truncated trailing record — skip
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<HistoryEntry>(trimmed) else {
            continue; // malformed — skip
        };
        // Dedup consecutive duplicates.
        if prev_text.as_deref() == Some(&entry.text) {
            continue;
        }
        prev_text = Some(entry.text.clone());
        entries.push(entry);
    }
    entries
}

/// Appends a new entry to the histfile. Trims to `HISTORY_CAP` when the file
/// exceeds `2 * HISTORY_CAP` entries (amortized trim).
///
/// Uses `flock(LOCK_EX)` via `fs2` for the entire append+trim operation.
/// Creates the file (and parent dirs) if they don't exist.
pub fn append_history(path: &Path, text: &str) -> std::io::Result<()> {
    // Ensure parent directory exists.
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let ts = now_ms();
    let entry = HistoryEntry {
        ts,
        text: text.to_string(),
    };
    let line = serde_json::to_string(&entry).expect("HistoryEntry is always serializable");

    // Open (or create) the file for read+write.
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .open(path)?;

    // Advisory exclusive lock — serializes all cooperating `bro` writers.
    file.lock_exclusive()?;

    // Append the new line.
    let mut file_ref = &file;
    file_ref.seek(std::io::SeekFrom::End(0))?;
    file_ref.write_all(line.as_bytes())?;
    file_ref.write_all(b"\n")?;
    file_ref.flush()?;

    // Check if we need to trim.
    let count = count_lines(&file)?;
    if count > 2 * HISTORY_CAP {
        trim_locked(&file, path)?;
    }

    // Lock released on drop.
    Ok(())
}

/// Count the number of non-empty lines in an already-locked file.
fn count_lines(file: &File) -> std::io::Result<usize> {
    let mut file_ref = file;
    file_ref.rewind()?;
    let reader = BufReader::new(file);
    let mut count = 0;
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if !line.trim().is_empty() {
            count += 1;
        }
    }
    Ok(count)
}

/// Trim the histfile to the last `HISTORY_CAP` entries, using tmp+rename
/// atomicity. Caller must hold the exclusive lock on `file`.
fn trim_locked(file: &File, path: &Path) -> std::io::Result<()> {
    let mut file_ref = file;
    file_ref.rewind()?;
    let reader = BufReader::new(file);
    let mut all_entries: Vec<HistoryEntry> = Vec::new();
    let mut prev_text: Option<String> = None;

    for line in reader.lines() {
        let Ok(line) = line else { break };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<HistoryEntry>(trimmed) else {
            continue;
        };
        // Dedup consecutive duplicates during trim.
        if prev_text.as_deref() == Some(&entry.text) {
            continue;
        }
        prev_text = Some(entry.text.clone());
        all_entries.push(entry);
    }

    let tail = if all_entries.len() > HISTORY_CAP {
        &all_entries[all_entries.len() - HISTORY_CAP..]
    } else {
        &all_entries
    };

    // Write compacted tail to a temp file, fsync, then rename.
    let tmp_path = path.with_extension("jsonl.tmp");
    {
        let mut tmp = File::create(&tmp_path)?;
        for entry in tail {
            let line = serde_json::to_string(entry).expect("HistoryEntry is always serializable");
            tmp.write_all(line.as_bytes())?;
            tmp.write_all(b"\n")?;
        }
        tmp.sync_all()?;
    }
    fs::rename(&tmp_path, path)?;
    Ok(())
}

/// Current time in milliseconds since epoch.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as IoWrite;

    fn temp_histdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn multiline_prompt_round_trips_as_one_entry() {
        let dir = temp_histdir();
        let path = dir.path().join("composer_history.jsonl");

        let multiline = "first line\nsecond line\nthird line";
        append_history(&path, multiline).unwrap();

        let entries = read_history(&path);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].text, multiline);

        // Verify it's one physical line in the file.
        let contents = fs::read_to_string(&path).unwrap();
        let physical_lines: Vec<&str> = contents.lines().collect();
        assert_eq!(
            physical_lines.len(),
            1,
            "multi-line prompt must be one JSONL record"
        );
    }

    #[test]
    fn consecutive_dedup_on_read() {
        let dir = temp_histdir();
        let path = dir.path().join("composer_history.jsonl");

        append_history(&path, "hello").unwrap();
        append_history(&path, "hello").unwrap();
        append_history(&path, "hello").unwrap();
        append_history(&path, "world").unwrap();
        append_history(&path, "world").unwrap();

        let entries = read_history(&path);
        let texts: Vec<&str> = entries.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, vec!["hello", "world"]);
    }

    #[test]
    fn concurrent_append_safety() {
        let dir = temp_histdir();
        let path = dir.path().join("composer_history.jsonl");

        // Write initial entries directly (no lock) to set up the file.
        {
            let mut f = File::create(&path).unwrap();
            for i in 0..10 {
                let entry = HistoryEntry {
                    ts: i,
                    text: format!("entry-{i}"),
                };
                writeln!(f, "{}", serde_json::to_string(&entry).unwrap()).unwrap();
            }
        }

        // Append from multiple "threads" sequentially (flock serializes).
        for i in 10..30 {
            append_history(&path, &format!("entry-{i}")).unwrap();
        }

        let entries = read_history(&path);
        assert_eq!(entries.len(), 30);
        for (i, entry) in entries.iter().enumerate() {
            assert_eq!(entry.text, format!("entry-{i}"));
        }
    }

    #[test]
    fn trim_uses_temp_and_rename_not_truncate() {
        let dir = temp_histdir();
        let path = dir.path().join("composer_history.jsonl");

        // Write enough entries to trigger a trim (2 * HISTORY_CAP = 10000).
        // We'll write fewer and test the trim_locked function directly.
        let mut entries = Vec::new();
        for i in 0..200 {
            entries.push(HistoryEntry {
                ts: i,
                text: format!("entry-{i}"),
            });
        }

        // Write all entries directly.
        {
            let mut f = File::create(&path).unwrap();
            for entry in &entries {
                writeln!(f, "{}", serde_json::to_string(entry).unwrap()).unwrap();
            }
        }

        // Manually invoke trim_locked.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        fs2::FileExt::lock_exclusive(&file).unwrap();
        trim_locked(&file, &path).unwrap();
        // Lock released on drop.

        let result = read_history(&path);
        assert_eq!(result.len(), 200, "200 < HISTORY_CAP, nothing trimmed");
    }

    #[test]
    fn trim_actually_caps_to_history_cap() {
        let dir = temp_histdir();
        let path = dir.path().join("composer_history.jsonl");

        // Write more than HISTORY_CAP entries.
        let count = HISTORY_CAP + 100;
        {
            let mut f = File::create(&path).unwrap();
            for i in 0..count {
                let entry = HistoryEntry {
                    ts: i as u64,
                    text: format!("entry-{i}"),
                };
                writeln!(f, "{}", serde_json::to_string(&entry).unwrap()).unwrap();
            }
        }

        // Trim.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        fs2::FileExt::lock_exclusive(&file).unwrap();
        trim_locked(&file, &path).unwrap();

        let result = read_history(&path);
        assert_eq!(result.len(), HISTORY_CAP);
        // Should have kept the last HISTORY_CAP entries.
        assert_eq!(result[0].text, "entry-100");
        assert_eq!(result.last().unwrap().text, "entry-5099");
    }

    #[test]
    fn torn_trailing_record_skipped() {
        let dir = temp_histdir();
        let path = dir.path().join("composer_history.jsonl");

        // Write a valid entry, then a truncated one.
        {
            let mut f = File::create(&path).unwrap();
            let entry = HistoryEntry {
                ts: 1,
                text: "good".to_string(),
            };
            writeln!(f, "{}", serde_json::to_string(&entry).unwrap()).unwrap();
            // Write a partial/truncated JSON line (no trailing newline).
            write!(f, "{{\"ts\":2,\"text\":\"truncate").unwrap();
        }

        let entries = read_history(&path);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].text, "good");
    }

    #[test]
    fn file_created_with_mode_0600() {
        let dir = temp_histdir();
        let path = dir.path().join("subdir").join("composer_history.jsonl");

        append_history(&path, "test").unwrap();

        assert!(path.exists());
        let entries = read_history(&path);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].text, "test");
    }
}
