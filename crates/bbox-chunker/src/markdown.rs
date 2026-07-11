use std::path::Path;

use anyhow::Result;
use regex::Regex;

use super::{Chunk, Edge, SourceFormatChunker, placeholder_chunk};

pub struct MarkdownChunker;

impl SourceFormatChunker for MarkdownChunker {
    fn format_id(&self) -> &str {
        "markdown"
    }

    fn claims(&self, path: &Path, _sniff: &[u8]) -> bool {
        matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("md" | "markdown" | "mdown")
        )
    }

    fn chunk(&self, path: &Path, bytes: &[u8]) -> Result<(Vec<Chunk>, Vec<Edge>)> {
        let text = std::str::from_utf8(bytes)?.replace("\r\n", "\n");
        let mut chunks = Vec::new();
        let mut sections = h2_sections(&text);
        if sections.is_empty() && !text.trim().is_empty() {
            sections.push((0, text.len()));
        }
        for (start, end) in sections {
            let content = text[start..end].trim().to_string();
            if content.is_empty() {
                continue;
            }
            // Prose, not code: a language label here routes the chunk into
            // the CODE embedding bucket (see bbox-embed is_code_chunk).
            chunks.push(placeholder_chunk(
                path,
                "doc_section",
                None,
                content,
                start as u64,
                end as u64,
                chunks.len() as u32,
            ));
        }
        Ok((chunks, Vec::new()))
    }
}

fn h2_sections(text: &str) -> Vec<(usize, usize)> {
    let mut starts = Vec::new();
    let mut offset = 0usize;
    for line in text.split_inclusive('\n') {
        if line.starts_with("## ") {
            starts.push(offset);
        }
        offset += line.len();
    }
    if starts.is_empty() {
        return Vec::new();
    }
    if starts.first().copied().unwrap_or_default() != 0 {
        starts.insert(0, 0);
    }
    starts
        .iter()
        .enumerate()
        .map(|(idx, start)| {
            let end = starts.get(idx + 1).copied().unwrap_or(text.len());
            (*start, end)
        })
        .collect()
}

#[allow(dead_code)]
pub fn markdown_edge_kinds(content: &str) -> Vec<&'static str> {
    let mut kinds = Vec::new();
    if content.contains("```") {
        kinds.push("EMBEDS_CODE_FENCE");
    }
    let link_re = Regex::new(r"\[[^\]]+\]\(([^)]+)\)").expect("valid markdown link regex");
    for cap in link_re.captures_iter(content) {
        let target = cap.get(1).map(|m| m.as_str()).unwrap_or_default();
        if target.starts_with('#') {
            kinds.push("LINKS_TO_SECTION");
        } else if !target.contains("://") {
            kinds.push("LINKS_TO_FILE");
        }
    }
    kinds
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_splits_on_h2_boundaries() {
        let input = "# Title\nintro\n## A\nalpha\n## B\nbeta\n";
        let (chunks, _) = MarkdownChunker
            .chunk(Path::new("README.md"), input.as_bytes())
            .unwrap();
        assert_eq!(chunks.len(), 3);
        assert!(chunks[0].content.starts_with("# Title"));
        assert!(chunks[1].content.starts_with("## A"));
        assert!(chunks[2].content.starts_with("## B"));
    }
}
