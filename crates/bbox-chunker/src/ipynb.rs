use std::path::Path;

use anyhow::Result;
use serde_json::Value as JsonValue;

use super::{Chunk, Edge, SourceFormatChunker, placeholder_chunk};

/// Cap on the projected output text appended to a code cell's chunk content
/// (design/corpus/agentic-corpus/agentic-corpus-multimodal-chunkers.md,
/// X-IPYNB: "cell-level chunks with cell index + outputs"). Applies to the
/// combined text across all of a cell's outputs, not per individual output,
/// so one chatty cell can't blow the chunk past `MAX_CHUNK_BYTES` on its own
/// (downstream `bound_chunks` in bbox-corpus-index still splits oversized
/// chunks generically; this cap just keeps output noise proportionate to the
/// source).
const MAX_OUTPUT_BYTES: usize = 2 * 1024;

/// Only nbformat 4 is supported. nbformat 3 used a different cell/output
/// shape (`worksheets[].cells[]`, `input` instead of `source`, etc.) that
/// this parser does not understand; rather than guess at a v3 shape and risk
/// silently mis-chunking, v3 and any other/missing version degrade to zero
/// chunks per the chunker's failure posture (see module doc).
const SUPPORTED_NBFORMAT: u64 = 4;

/// Jupyter notebook chunker (X-IPYNB,
/// `design/corpus/agentic-corpus/agentic-corpus-multimodal-chunkers.md`).
/// One `notebook_cell` chunk per non-empty code or markdown cell, carrying
/// the cell's position via `line_start`/`line_end` the same way
/// `pdf.rs::extract_page_chunks` carries page numbers (no dedicated
/// "cell index" field exists on `Chunk`; see the field doc comments in
/// `lib.rs`).
///
/// **Single `notebook_cell` kind, not a `notebook_cell`/`notebook_markdown`
/// split**: `design/corpus/agentic-corpus/agentic-corpus.md` §7.2 already
/// documents `notebook_cell` as the one chunk_kind for ipynb ("one chunk per
/// cell, carries cell index + outputs"), covering both code and markdown
/// cells. Code vs. markdown is distinguished by `Chunk.language` instead
/// (`Some(kernel_language)` for code cells, `None` for markdown cells),
/// the same signal `bbox-embed::enqueue_project_file` already uses to route
/// project-file chunks to the Code vs. Docs embedding bucket, so this reuses
/// existing routing rather than adding a second kind string with no
/// consumer.
///
/// **Edges**: this chunker emits zero edges from `chunk()`, matching every
/// other chunker in the registry (`pdf.rs`, `markdown.rs`, `config.rs`,
/// `code.rs`, all return `Vec::new()` for edges). The edge channel a
/// `SourceFormatChunker::chunk` call returns is not actually where
/// structural edges get materialized: `bbox-corpus-index`'s
/// `project_files::derive_edges` already synthesizes a generic
/// `NEXT_SECTION` edge between every pair of consecutive chunks in a file
/// regardless of format, which is `NEXT_CELL` in substance (ordered
/// same-file adjacency) even though it doesn't carry that literal edge-kind
/// string. `OUTPUT_OF` has no separate entity to point at in this
/// implementation, since per scope outputs are folded into the owning
/// cell's own chunk text rather than materialized as their own chunks (so
/// there is no output vertex to link). `IMPORTS_FROM_CELL` would need a new
/// heuristic resolution pass keyed on `chunk_kind == "notebook_cell"`
/// (parallel to `derive_code_edges`'s `CALLS`/`USES_TYPE` heuristics, which
/// are scoped to `chunk_kind == "code_block"` and won't fire here), which is
/// out of scope per "do not invent a parallel edge mechanism". Precedent:
/// `markdown.rs::markdown_edge_kinds` computes `LINKS_TO_FILE` /
/// `LINKS_TO_SECTION` / `EMBEDS_CODE_FENCE` candidates today but is
/// `#[allow(dead_code)]` and never wired into an emitted `Edge`; the
/// design doc's edge-kind columns are aspirational surface, not a contract
/// every chunker must fulfill on day one.
pub struct IpynbChunker;

impl SourceFormatChunker for IpynbChunker {
    fn format_id(&self) -> &str {
        "ipynb"
    }

    fn claims(&self, path: &Path, sniff: &[u8]) -> bool {
        if path.extension().and_then(|ext| ext.to_str()) != Some("ipynb") {
            return false;
        }
        // Loose JSON-object sniff (first non-whitespace byte is `{`) so an
        // `.ipynb`-named file that isn't actually JSON doesn't get claimed
        // away from a more appropriate chunker (there is none registered
        // for raw text with that extension, but the check is cheap and
        // matches the pattern of pdf.rs's magic-header claim).
        sniff
            .iter()
            .find(|byte| !byte.is_ascii_whitespace())
            .is_some_and(|byte| *byte == b'{')
    }

    fn chunk(&self, path: &Path, bytes: &[u8]) -> Result<(Vec<Chunk>, Vec<Edge>)> {
        Ok((extract_cell_chunks(path, bytes), Vec::new()))
    }
}

/// Extract one `notebook_cell` chunk per non-empty code/markdown cell.
/// Never returns `Err`: malformed JSON, an unsupported/missing nbformat
/// version, or a notebook with no usable `cells` array all degrade to zero
/// chunks with a `tracing::warn`, because `SourceFormatChunker::chunk`
/// returning `Err` aborts the entire background reindex pass rather than
/// just skipping this file (documented at the top of `pdf.rs` too).
/// Individual malformed cells (not an object, missing `cell_type`, unknown
/// `cell_type`, empty/missing `source`) are skipped without a warning:
/// that's normal notebook content (raw cells, empty scratch cells), not
/// corruption.
fn extract_cell_chunks(path: &Path, bytes: &[u8]) -> Vec<Chunk> {
    let root: JsonValue = match serde_json::from_slice(bytes) {
        Ok(value) => value,
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "ipynb JSON parse failed; skipping"
            );
            return Vec::new();
        }
    };

    let Some(nbformat) = root.get("nbformat").and_then(JsonValue::as_u64) else {
        tracing::warn!(
            path = %path.display(),
            "ipynb missing or non-numeric nbformat field; skipping"
        );
        return Vec::new();
    };
    if nbformat != SUPPORTED_NBFORMAT {
        tracing::warn!(
            path = %path.display(),
            nbformat,
            "unsupported ipynb nbformat version (only nbformat 4 is supported); skipping"
        );
        return Vec::new();
    }

    let Some(cells) = root.get("cells").and_then(JsonValue::as_array) else {
        tracing::warn!(
            path = %path.display(),
            "ipynb has no top-level `cells` array; skipping"
        );
        return Vec::new();
    };

    let kernel_language = notebook_language(&root);

    let mut chunks = Vec::new();
    let mut byte_offset = 0u64;
    for (idx, cell) in cells.iter().enumerate() {
        let Some(chunk) = cell_chunk(
            path,
            cell,
            idx,
            kernel_language.as_deref(),
            &mut byte_offset,
        ) else {
            continue;
        };
        chunks.push(chunk);
    }
    chunks
}

/// Resolve the notebook's kernel language from `metadata.kernelspec.language`
/// (the field nbformat 4 documents for this purpose), falling back to
/// `metadata.language_info.name` when kernelspec is absent or incomplete.
fn notebook_language(root: &JsonValue) -> Option<String> {
    let metadata = root.get("metadata")?;
    metadata
        .get("kernelspec")
        .and_then(|k| k.get("language"))
        .and_then(JsonValue::as_str)
        .or_else(|| {
            metadata
                .get("language_info")
                .and_then(|l| l.get("name"))
                .and_then(JsonValue::as_str)
        })
        .filter(|lang| !lang.is_empty())
        .map(str::to_string)
}

/// Build one chunk for `cell` at notebook position `idx` (0-based), or
/// `None` when the cell is structurally unusable or has no non-empty
/// source. `byte_offset` is threaded through and advanced past this cell's
/// content, mirroring `pdf.rs::extract_page_chunks`'s running offset (ipynb
/// chunks don't map back to original file byte ranges after a JSON parse,
/// so this just keeps ranges monotonic and non-overlapping like the other
/// chunkers that reconstruct/normalize content, e.g. `config.rs`).
fn cell_chunk(
    path: &Path,
    cell: &JsonValue,
    idx: usize,
    kernel_language: Option<&str>,
    byte_offset: &mut u64,
) -> Option<Chunk> {
    let cell_type = cell.get("cell_type")?.as_str()?;
    let source = cell_source_text(cell.get("source")?)?;
    let source = source.trim();
    if source.is_empty() {
        return None;
    }

    let (content, language) = match cell_type {
        "code" => (append_outputs(source, cell.get("outputs")), kernel_language),
        "markdown" => (source.to_string(), None),
        // "raw" and any unrecognized cell_type are not part of this pass's
        // scope (design doc only calls out code + markdown cell handling);
        // skip rather than guess at how to project them.
        _ => return None,
    };

    let byte_start = *byte_offset;
    let byte_end = byte_start + content.len() as u64;
    *byte_offset = byte_end + 1;

    let mut chunk = placeholder_chunk(
        path,
        "notebook_cell",
        language,
        content,
        byte_start,
        byte_end,
        idx as u32,
    );
    let cell_number = (idx + 1) as u32;
    chunk.line_start = Some(cell_number);
    chunk.line_end = Some(cell_number);
    Some(chunk)
}

/// nbformat 4 stores `source` as either a single string or an array of
/// line strings (each usually already newline-terminated, joined with no
/// separator). Non-string array entries are skipped defensively rather than
/// failing the whole cell.
fn cell_source_text(source: &JsonValue) -> Option<String> {
    match source {
        JsonValue::String(text) => Some(text.clone()),
        JsonValue::Array(lines) => {
            let mut joined = String::new();
            let mut saw_any = false;
            for line in lines {
                if let JsonValue::String(text) = line {
                    joined.push_str(text);
                    saw_any = true;
                }
            }
            saw_any.then_some(joined)
        }
        _ => None,
    }
}

/// Append a truncated text projection of a code cell's `stream` and
/// `execute_result`/`display_data` (`text/plain` only) outputs to `source`,
/// clearly delimited. Binary/image outputs (any other mime type, e.g.
/// `image/png`, `text/html`) are skipped entirely: no base64 blobs land in
/// text chunks. `error` outputs and unrecognized `output_type`s are also
/// skipped; scope is "text/plain and stream outputs" only.
fn append_outputs(source: &str, outputs: Option<&JsonValue>) -> String {
    let Some(outputs) = outputs.and_then(JsonValue::as_array) else {
        return source.to_string();
    };

    let mut projected = Vec::new();
    for output in outputs {
        if let Some(text) = output_text(output) {
            projected.push(text);
        }
    }
    if projected.is_empty() {
        return source.to_string();
    }

    let combined = projected.join("\n---\n");
    let combined = truncate_output(&combined);
    format!("{source}\n\n--- output ---\n{combined}\n--- end output ---")
}

/// Extract a text projection from one output object, or `None` for outputs
/// this pass doesn't project (errors, unknown output_type, or
/// execute_result/display_data with no `text/plain` entry in `data`).
fn output_text(output: &JsonValue) -> Option<String> {
    // Output `text` fields (stream `text`, `data["text/plain"]`) use the
    // same string-or-array-of-lines shape as cell `source`.
    match output.get("output_type").and_then(JsonValue::as_str)? {
        "stream" => {
            let text = cell_source_text(output.get("text")?)?;
            let name = output.get("name").and_then(JsonValue::as_str);
            Some(match name {
                Some(name) => format!("[{name}] {text}"),
                None => text,
            })
        }
        "execute_result" | "display_data" => {
            let data = output.get("data")?;
            let text = data.get("text/plain")?;
            cell_source_text(text)
        }
        _ => None,
    }
}

/// Truncate `text` to `MAX_OUTPUT_BYTES`, cutting at a UTF-8 char boundary
/// and appending a truncation marker so it's clear content was cut.
fn truncate_output(text: &str) -> String {
    if text.len() <= MAX_OUTPUT_BYTES {
        return text.to_string();
    }
    let mut end = MAX_OUTPUT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...[output truncated]", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notebook(cells_json: &str) -> Vec<u8> {
        format!(
            r#"{{
  "nbformat": 4,
  "nbformat_minor": 5,
  "metadata": {{
    "kernelspec": {{ "name": "python3", "language": "python" }}
  }},
  "cells": [{cells_json}]
}}"#
        )
        .into_bytes()
    }

    #[test]
    fn claims_ipynb_extension_with_json_sniff() {
        let bytes = notebook("");
        assert!(IpynbChunker.claims(Path::new("nb.ipynb"), &bytes));
        assert!(!IpynbChunker.claims(Path::new("nb.json"), &bytes));
        assert!(!IpynbChunker.claims(Path::new("nb.ipynb"), b"not json"));
    }

    #[test]
    fn two_code_cells_one_markdown_one_empty_produce_expected_chunks() {
        let cells = r##"
            {
              "cell_type": "code",
              "source": ["import os\n", "print('hi')\n"],
              "outputs": [
                { "output_type": "stream", "name": "stdout", "text": ["hi\n"] }
              ]
            },
            {
              "cell_type": "code",
              "source": "x = 1\n",
              "outputs": []
            },
            {
              "cell_type": "markdown",
              "source": ["# Title\n", "some prose\n"]
            },
            {
              "cell_type": "code",
              "source": ""
            }
        "##;
        let bytes = notebook(cells);
        let (chunks, edges) = IpynbChunker
            .chunk(Path::new("nb.ipynb"), &bytes)
            .expect("well-formed notebook must not error");
        assert!(edges.is_empty());
        assert_eq!(
            chunks.len(),
            3,
            "empty 4th cell must be skipped: {chunks:?}"
        );

        // Cell 1: code with stream output.
        assert_eq!(chunks[0].chunk_kind, "notebook_cell");
        assert_eq!(chunks[0].language.as_deref(), Some("python"));
        assert_eq!(chunks[0].line_start, Some(1));
        assert_eq!(chunks[0].line_end, Some(1));
        assert!(chunks[0].content.contains("import os"));
        assert!(chunks[0].content.contains("print('hi')"));
        assert!(chunks[0].content.contains("--- output ---"));
        assert!(chunks[0].content.contains("[stdout] hi"));

        // Cell 2: code with no outputs, content is just the source.
        assert_eq!(chunks[1].chunk_kind, "notebook_cell");
        assert_eq!(chunks[1].language.as_deref(), Some("python"));
        assert_eq!(chunks[1].line_start, Some(2));
        assert_eq!(chunks[1].content, "x = 1");
        assert!(!chunks[1].content.contains("--- output ---"));

        // Cell 3: markdown, no language, prose content, position is 3
        // (the empty 4th cell is not counted since it's dropped, but the
        // markdown cell's own position among the raw JSON cells is 3).
        assert_eq!(chunks[2].chunk_kind, "notebook_cell");
        assert_eq!(chunks[2].language, None);
        assert_eq!(chunks[2].line_start, Some(3));
        assert!(chunks[2].content.contains("# Title"));
        assert!(chunks[2].content.contains("some prose"));
    }

    #[test]
    fn execute_result_text_plain_is_projected_and_other_mime_types_are_skipped() {
        let cells = r#"
            {
              "cell_type": "code",
              "source": "df.head()\n",
              "outputs": [
                {
                  "output_type": "execute_result",
                  "data": {
                    "text/plain": ["   a  b\n", "0  1  2\n"],
                    "text/html": ["<table>...huge html blob...</table>"],
                    "image/png": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAAB"
                  }
                }
              ]
            }
        "#;
        let bytes = notebook(cells);
        let (chunks, _edges) = IpynbChunker
            .chunk(Path::new("nb.ipynb"), &bytes)
            .expect("well-formed notebook must not error");
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].content.contains("a  b"));
        assert!(!chunks[0].content.contains("html"));
        assert!(!chunks[0].content.contains("iVBORw0KGgo"));
    }

    #[test]
    fn oversized_stream_output_is_truncated() {
        let big = "x".repeat(MAX_OUTPUT_BYTES * 2);
        let cells = format!(
            r#"
            {{
              "cell_type": "code",
              "source": "print('x' * 100000)\n",
              "outputs": [
                {{ "output_type": "stream", "name": "stdout", "text": "{big}" }}
              ]
            }}
        "#
        );
        let bytes = notebook(&cells);
        let (chunks, _edges) = IpynbChunker
            .chunk(Path::new("nb.ipynb"), &bytes)
            .expect("well-formed notebook must not error");
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].content.contains("...[output truncated]"));
        // Output block is capped near MAX_OUTPUT_BYTES; total content stays
        // far under the full doubled output size.
        assert!(chunks[0].content.len() < MAX_OUTPUT_BYTES * 2);
    }

    #[test]
    fn garbage_byte_stream_produces_no_chunks_and_does_not_panic() {
        let garbage = b"this is not json, just some random bytes \x00\x01\x02 garbage";
        let (chunks, edges) = IpynbChunker
            .chunk(Path::new("nb.ipynb"), garbage)
            .expect("chunk() must never return Err for malformed input");
        assert!(chunks.is_empty());
        assert!(edges.is_empty());
    }

    #[test]
    fn empty_byte_stream_produces_no_chunks_and_does_not_panic() {
        let (chunks, edges) = IpynbChunker
            .chunk(Path::new("nb.ipynb"), b"")
            .expect("chunk() must never return Err for empty input");
        assert!(chunks.is_empty());
        assert!(edges.is_empty());
    }

    #[test]
    fn nbformat_v3_degrades_to_zero_chunks() {
        let bytes = br#"{
            "nbformat": 3,
            "nbformat_minor": 0,
            "worksheets": [{ "cells": [{ "cell_type": "code", "input": ["x = 1"] }] }]
        }"#;
        let (chunks, edges) = IpynbChunker
            .chunk(Path::new("nb.ipynb"), bytes)
            .expect("v3 notebook must degrade, not error");
        assert!(chunks.is_empty());
        assert!(edges.is_empty());
    }

    #[test]
    fn missing_nbformat_field_degrades_to_zero_chunks() {
        let bytes = br#"{ "cells": [] }"#;
        let (chunks, edges) = IpynbChunker
            .chunk(Path::new("nb.ipynb"), bytes)
            .expect("missing nbformat must degrade, not error");
        assert!(chunks.is_empty());
        assert!(edges.is_empty());
    }

    #[test]
    fn missing_cells_array_degrades_to_zero_chunks() {
        let bytes = br#"{ "nbformat": 4, "nbformat_minor": 5, "metadata": {} }"#;
        let (chunks, edges) = IpynbChunker
            .chunk(Path::new("nb.ipynb"), bytes)
            .expect("missing cells array must degrade, not error");
        assert!(chunks.is_empty());
        assert!(edges.is_empty());
    }

    #[test]
    fn weird_cell_shapes_are_skipped_without_panic() {
        let cells = r#"
            "not an object",
            { "cell_type": "code" },
            { "source": "no cell_type here" },
            { "cell_type": "raw", "source": "raw cells are out of scope" },
            { "cell_type": "code", "source": "y = 2\n" }
        "#;
        let bytes = notebook(cells);
        let (chunks, edges) = IpynbChunker
            .chunk(Path::new("nb.ipynb"), &bytes)
            .expect("weird cell shapes must be skipped, not error");
        assert!(edges.is_empty());
        assert_eq!(
            chunks.len(),
            1,
            "only the well-formed trailing cell should survive: {chunks:?}"
        );
        assert_eq!(chunks[0].content, "y = 2");
    }

    #[test]
    fn markdown_cell_without_kernelspec_has_no_language() {
        let bytes = br#"{
            "nbformat": 4,
            "nbformat_minor": 5,
            "metadata": {},
            "cells": [{ "cell_type": "markdown", "source": "no kernel here" }]
        }"#;
        let (chunks, _edges) = IpynbChunker
            .chunk(Path::new("nb.ipynb"), bytes)
            .expect("well-formed notebook must not error");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].language, None);
    }

    #[test]
    fn language_info_fallback_used_when_kernelspec_missing() {
        let bytes = br#"{
            "nbformat": 4,
            "nbformat_minor": 5,
            "metadata": { "language_info": { "name": "julia" } },
            "cells": [{ "cell_type": "code", "source": "println(1)" }]
        }"#;
        let (chunks, _edges) = IpynbChunker
            .chunk(Path::new("nb.ipynb"), bytes)
            .expect("well-formed notebook must not error");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].language.as_deref(), Some("julia"));
    }
}
