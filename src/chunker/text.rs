use std::path::Path;

use anyhow::Result;

use super::{Chunk, Edge, SourceFormatChunker, placeholder_chunk};

const TARGET_CHUNK_BYTES: usize = 1024;

pub struct PlainTextChunker;

impl SourceFormatChunker for PlainTextChunker {
    fn format_id(&self) -> &str {
        "plain_text"
    }

    fn claims(&self, path: &Path, _sniff: &[u8]) -> bool {
        matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("txt" | "text" | "log")
        )
    }

    fn chunk(&self, path: &Path, bytes: &[u8]) -> Result<(Vec<Chunk>, Vec<Edge>)> {
        let text = std::str::from_utf8(bytes)?.replace("\r\n", "\n");
        let mut chunks = Vec::new();
        let mut current = String::new();
        let mut current_start = 0usize;
        let mut current_end = 0usize;

        for paragraph in split_paragraphs(&text) {
            if current.is_empty() {
                current_start = paragraph.start;
            }
            if !current.is_empty() && current.len() + paragraph.text.len() + 2 > TARGET_CHUNK_BYTES
            {
                chunks.push(placeholder_chunk(
                    path,
                    "paragraph",
                    None,
                    current.trim().to_string(),
                    current_start as u64,
                    current_end as u64,
                    chunks.len() as u32,
                ));
                current.clear();
                current_start = paragraph.start;
            }
            if !current.is_empty() {
                current.push_str("\n\n");
            }
            current.push_str(paragraph.text.trim());
            current_end = paragraph.end;
        }

        if !current.trim().is_empty() {
            chunks.push(placeholder_chunk(
                path,
                "paragraph",
                None,
                current.trim().to_string(),
                current_start as u64,
                current_end as u64,
                chunks.len() as u32,
            ));
        }

        Ok((chunks, Vec::new()))
    }
}

struct Paragraph<'a> {
    text: &'a str,
    start: usize,
    end: usize,
}

fn split_paragraphs(text: &str) -> Vec<Paragraph<'_>> {
    let mut out = Vec::new();
    let mut start = None;
    let mut offset = 0usize;
    for line in text.split_inclusive('\n') {
        let line_start = offset;
        offset += line.len();
        if line.trim().is_empty() {
            if let Some(paragraph_start) = start.take() {
                let paragraph = &text[paragraph_start..line_start];
                if !paragraph.trim().is_empty() {
                    out.push(Paragraph {
                        text: paragraph,
                        start: paragraph_start,
                        end: line_start,
                    });
                }
            }
        } else if start.is_none() {
            start = Some(line_start);
        }
    }
    if let Some(paragraph_start) = start {
        let paragraph = &text[paragraph_start..offset];
        if !paragraph.trim().is_empty() {
            out.push(Paragraph {
                text: paragraph,
                start: paragraph_start,
                end: offset,
            });
        }
    } else if !text.contains('\n') && !text.trim().is_empty() {
        out.push(Paragraph {
            text,
            start: 0,
            end: text.len(),
        });
    }
    out
}
