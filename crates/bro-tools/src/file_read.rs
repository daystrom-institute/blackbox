//! Bounded exact source reads with byte continuations for long lines.

use crate::{Tool, ToolAnnotations, ToolCx, ToolResult, schema_for};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt};

const DEFAULT_LINES: usize = 200;
const MAX_LINES: usize = 2_000;

#[derive(Deserialize, JsonSchema)]
struct FileReadInput {
    /// File path, relative to the worktree or absolute. Supports @file mentions.
    file_path: String,
    /// First source line, 1-based (default 1). Cannot combine with start_byte.
    start_line: Option<usize>,
    /// Last source line, inclusive. Omit to continue toward EOF.
    end_line: Option<usize>,
    /// Maximum source lines (default 200, capped at 2000). Must be positive.
    max_lines: Option<usize>,
    /// Maximum source output bytes (default/max 8000, minimum 64).
    max_bytes: Option<usize>,
    /// Byte offset from a previous continuation. Use instead of start_line.
    /// May resume inside a long line; source line numbers remain unchanged.
    start_byte: Option<u64>,
    /// Prefix source lines with their original 1-based number. Default false.
    #[serde(default)]
    line_numbers: bool,
}

pub struct FileRead;

#[async_trait]
impl Tool for FileRead {
    fn name(&self) -> &str {
        "file_read"
    }
    fn description(&self) -> &str {
        "Read exact UTF-8 source text in bounded pages (default 200 lines, at most 8000 source-output bytes). Use start_line/end_line for a range. When truncated, follow the returned start_line or start_byte on the SAME original file_path; byte continuation handles long lines without losing text. line_numbers=true uses original source coordinates. Relative paths resolve from the worktree; absolute paths and @file mentions are supported."
    }
    fn input_schema(&self) -> Value {
        schema_for::<FileReadInput>()
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            ..Default::default()
        }
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        match read(input, cx).await {
            Ok(text) => ToolResult::Text(text),
            Err(error) => ToolResult::Error(error.to_string()),
        }
    }
}

async fn read(input: Value, cx: &ToolCx) -> anyhow::Result<String> {
    let args: FileReadInput = serde_json::from_value(input)?;
    let start = args.start_line.unwrap_or(1);
    let end = args.end_line.unwrap_or(usize::MAX);
    let max_lines = args.max_lines.unwrap_or(DEFAULT_LINES).min(MAX_LINES);
    let budget = args
        .max_bytes
        .unwrap_or(crate::output::DEFAULT_OUTPUT_BYTES)
        .clamp(64, crate::output::DEFAULT_OUTPUT_BYTES);
    anyhow::ensure!(
        start > 0 && start <= end,
        "expected 1-based start_line <= end_line"
    );
    anyhow::ensure!(max_lines > 0, "max_lines must be positive");
    anyhow::ensure!(
        args.start_byte.is_none() || args.start_line.is_none(),
        "use start_byte or start_line, not both"
    );
    let path = crate::workspace::resolve_read_path(&cx.root, &args.file_path)?;
    let mut file = tokio::fs::File::open(&path).await?;
    let size = file.metadata().await?.len();
    if let Some(offset) = args.start_byte {
        anyhow::ensure!(offset <= size, "start_byte is beyond EOF");
    }
    let line_known = args.start_byte.is_none() || args.line_numbers || args.end_line.is_some();
    let mut offset = 0u64;
    if !line_known {
        offset = args.start_byte.unwrap();
        file.seek(std::io::SeekFrom::Start(offset)).await?;
    }
    let mut reader = tokio::io::BufReader::new(file);
    let mut line = 1usize;
    let mut fragment = false;
    // Scan the prefix without allocating a complete line, including huge lines.
    loop {
        let needed = match args.start_byte {
            Some(target) => offset < target,
            None => line < start,
        };
        if !needed {
            break;
        }
        let buf = reader.fill_buf().await?;
        if buf.is_empty() {
            return Ok(String::new());
        }
        let mut consumed = 0;
        for &byte in buf {
            if args.start_byte.is_some_and(|target| offset == target)
                || (args.start_byte.is_none() && line == start)
            {
                break;
            }
            consumed += 1;
            offset += 1;
            fragment = byte != b'\n';
            if byte == b'\n' {
                line += 1;
            }
        }
        reader.consume(consumed);
    }
    let mut bytes = Vec::new();
    reader
        .take((budget + 4) as u64)
        .read_to_end(&mut bytes)
        .await?;
    let valid = match std::str::from_utf8(&bytes) {
        Ok(text) => text,
        Err(error) if error.error_len().is_none() && offset + (bytes.len() as u64) < size => {
            std::str::from_utf8(&bytes[..error.valid_up_to()])?
        }
        Err(error) => return Err(error.into()),
    };
    let mut output = String::new();
    let initial_line = line;
    let mut at_line_start = !fragment;
    for (count, piece) in valid.split_inclusive('\n').enumerate() {
        if count == max_lines || line > end {
            break;
        }
        let prefix = if args.line_numbers {
            format!("{line}\t")
        } else {
            String::new()
        };
        let remaining = budget.saturating_sub(output.len() + prefix.len());
        let part = crate::output::utf8_prefix(piece, remaining);
        if part.is_empty() {
            break;
        }
        output.push_str(&prefix);
        output.push_str(part);
        offset += part.len() as u64;
        at_line_start = part.ends_with('\n');
        if at_line_start {
            line += 1;
        }
        if part.len() < piece.len() {
            break;
        }
    }
    let more = offset < size && line <= end;
    if more {
        let continuation = if at_line_start && line_known {
            format!("start_line={line}")
        } else if line_known {
            format!("start_byte={offset} (continues source line {line})")
        } else {
            format!("start_byte={offset}")
        };
        output.push_str(&format!(
            "\n[truncated at max_lines={max_lines} or max_bytes={budget}; continue SAME file_path with {continuation}; keep end_line if set]"
        ));
    }
    if fragment && args.line_numbers && !output.is_empty() {
        output.push_str(&format!(
            "\n[first returned text is a continuation of source line {initial_line}]"
        ));
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    fn cx(root: &std::path::Path) -> ToolCx {
        ToolCx {
            root: root.to_owned(),
            safety: Arc::new(crate::SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: Arc::new(Mutex::new(Default::default())),
            shell_sessions: Arc::new(Mutex::new(Default::default())),
            edits: Arc::new(Mutex::new(Default::default())),
            session_env: Arc::new(Default::default()),
            shell_env: Arc::new(Default::default()),
            tool_arg_defaults: Arc::new(Default::default()),
        }
    }

    #[tokio::test]
    async fn long_unicode_line_pages_without_skipping_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let body = "日本語".repeat(3000);
        tokio::fs::write(root.join("long.txt"), &body)
            .await
            .unwrap();
        let cx = cx(&root);
        let mut offset = 0u64;
        let mut recovered = String::new();
        loop {
            let out = read(
                json!({"file_path":"long.txt","start_byte":offset,"max_bytes":100}),
                &cx,
            )
            .await
            .unwrap();
            let part = out.split("\n[truncated").next().unwrap();
            assert!(!part.is_empty());
            recovered.push_str(part);
            offset += part.len() as u64;
            assert!(out.len() < 400);
            if offset == body.len() as u64 {
                break;
            }
            assert!(out.contains(&format!("start_byte={offset}")));
        }
        assert_eq!(recovered, body);
    }

    #[tokio::test]
    async fn byte_pages_preserve_line_terminators_and_final_newlines() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let body = "abc\r\n".repeat(100);
        tokio::fs::write(root.join("lines.txt"), &body)
            .await
            .unwrap();
        let cx = cx(&root);
        let mut offset = 0u64;
        let mut recovered = String::new();
        while offset < body.len() as u64 {
            let output = read(
                json!({"file_path":"lines.txt","start_byte":offset,"max_bytes":65}),
                &cx,
            )
            .await
            .unwrap();
            let source = output.split("\n[truncated").next().unwrap();
            assert!(!source.is_empty());
            offset += source.len() as u64;
            recovered.push_str(source);
            if offset < body.len() as u64 {
                assert!(output.contains(&format!("start_byte={offset}")));
            }
        }
        assert_eq!(recovered, body);
        for ending in ["", "\n", "\r\n"] {
            let source = format!("a{ending}");
            tokio::fs::write(root.join("ending.txt"), &source)
                .await
                .unwrap();
            assert_eq!(
                read(json!({"file_path":"ending.txt"}), &cx).await.unwrap(),
                source
            );
        }
    }

    #[tokio::test]
    async fn numbered_byte_continuations_keep_source_line_and_reject_zero_pages() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        tokio::fs::write(
            root.join("source.txt"),
            format!("header\n{}\nlast", "x".repeat(200)),
        )
        .await
        .unwrap();
        let cx = cx(&root);
        let out = read(
            json!({"file_path":"source.txt","start_byte":100,"line_numbers":true,"max_bytes":64}),
            &cx,
        )
        .await
        .unwrap();
        assert!(out.starts_with("2\tx"));
        assert!(out.contains("start_byte=162"));
        assert!(out.contains("continuation of source line 2"));
        assert!(
            read(json!({"file_path":"source.txt","max_lines":0}), &cx)
                .await
                .is_err()
        );
        assert!(
            read(json!({"file_path":"source.txt","start_line":0}), &cx)
                .await
                .is_err()
        );
    }
}
