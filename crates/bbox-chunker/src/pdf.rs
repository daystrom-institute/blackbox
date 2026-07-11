use std::path::Path;

use anyhow::Result;

use super::{Chunk, Edge, SourceFormatChunker, placeholder_chunk};

/// Per the PDF spec the header must appear within the first 1024 bytes of
/// the file (some producers prepend junk bytes before it), so `claims`
/// scans the sniff window rather than requiring an exact byte-0 prefix.
const PDF_MAGIC: &[u8] = b"%PDF-";
const MAGIC_SCAN_WINDOW: usize = 1024;

/// Text-first PDF chunker (X-PDF,
/// `design/corpus/agentic-corpus/agentic-corpus-multimodal-chunkers.md`).
/// Ships without a visual embedding model or OCR shell-out, per the
/// acceptance criterion in
/// `design/corpus/agentic-corpus/multimodal-embedding-routing.md` ("X-PDF
/// ships text-first without a visual embedding model"). Encrypted, scanned
/// (no extractable text), or corrupt PDFs degrade to zero chunks rather than
/// an error: `SourceFormatChunker::chunk` returning `Err` propagates all the
/// way up and aborts the entire background reindex pass (not just this
/// file), so extraction failures are caught and swallowed here instead of
/// bubbling out.
///
/// `pdf_table` chunks are intentionally not implemented in this pass:
/// `pdf-extract`'s plain-text output discards layout/column structure, so
/// any table detector built on the extracted text alone would be a string
/// heuristic (repeated whitespace runs, aligned columns) prone to misfiring
/// on ordinary prose and code listings. Shipping that would violate the
/// "no flaky heuristics" scope constraint, so table chunks are deferred to
/// a future pass with real layout access (e.g. glyph positions).
///
/// `pdf_figure` chunks (embedded raster XObjects) are a separate module,
/// `pdf_figure.rs`: text extraction here stays OCR-free and independent of
/// any visual embedding model; figure extraction is additive, appended
/// below, and never affects `pdf_page` chunk output.
pub struct PdfChunker;

impl SourceFormatChunker for PdfChunker {
    fn format_id(&self) -> &str {
        "pdf"
    }

    fn claims(&self, path: &Path, sniff: &[u8]) -> bool {
        if path.extension().and_then(|ext| ext.to_str()) != Some("pdf") {
            return false;
        }
        let window = &sniff[..sniff.len().min(MAGIC_SCAN_WINDOW)];
        window.windows(PDF_MAGIC.len()).any(|w| w == PDF_MAGIC)
    }

    fn chunk(&self, path: &Path, bytes: &[u8]) -> Result<(Vec<Chunk>, Vec<Edge>)> {
        let mut chunks = extract_page_chunks(path, bytes);
        chunks.extend(super::pdf_figure::extract_figure_chunks(path, bytes));
        Ok((chunks, Vec::new()))
    }
}

/// Extract one `pdf_page` chunk per page carrying extractable text. Pages
/// with no text (e.g. a scanned/image-only page) are skipped rather than
/// emitted empty.
///
/// No dedicated page field exists on `Chunk` (see the field doc comments in
/// `lib.rs`); `line_start`/`line_end` are the existing per-chunk position
/// slots for "non-line oriented sources... chunks without source-line
/// metadata", so both are set to the 1-based page number here as the
/// closest existing analog to how line-oriented chunkers carry position,
/// rather than adding a new struct field for this text-first pass.
fn extract_page_chunks(path: &Path, bytes: &[u8]) -> Vec<Chunk> {
    // pdf-extract has known panics on some malformed/adversarial inputs
    // rather than returning an Err (upstream parser issues), so the call is
    // wrapped in catch_unwind on top of its own Result so corrupt input
    // never panics the indexing pass.
    let extraction =
        std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem_by_pages(bytes));
    let pages = match extraction {
        Ok(Ok(pages)) => pages,
        Ok(Err(err)) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "pdf text extraction failed (encrypted, corrupt, or unsupported structure); skipping"
            );
            return Vec::new();
        }
        Err(_) => {
            tracing::warn!(
                path = %path.display(),
                "pdf text extraction panicked; skipping"
            );
            return Vec::new();
        }
    };

    let mut chunks = Vec::new();
    let mut byte_offset = 0u64;
    for (idx, page_text) in pages.iter().enumerate() {
        let trimmed = page_text.trim();
        if trimmed.is_empty() {
            continue;
        }
        let page_number = (idx + 1) as u32;
        let byte_start = byte_offset;
        let byte_end = byte_start + trimmed.len() as u64;
        let mut chunk = placeholder_chunk(
            path,
            "pdf_page",
            None,
            trimmed.to_string(),
            byte_start,
            byte_end,
            chunks.len() as u32,
        );
        chunk.line_start = Some(page_number);
        chunk.line_end = Some(page_number);
        byte_offset = byte_end + 1;
        chunks.push(chunk);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assemble a minimal, spec-valid PDF byte-for-byte in the test itself
    /// (no fixture file, no build step) so xref offsets are computed rather
    /// than hand-counted. `pages` is a list of content-stream text-show
    /// bodies (already-escaped PDF string literal content); an empty
    /// content stream renders a page with no extractable text.
    fn build_pdf(pages: &[&str]) -> Vec<u8> {
        fn push_obj(buf: &mut Vec<u8>, offsets: &mut Vec<usize>, num: u32, body: &str) {
            offsets.push(buf.len());
            buf.extend_from_slice(format!("{num} 0 obj\n").as_bytes());
            buf.extend_from_slice(body.as_bytes());
            buf.extend_from_slice(b"\nendobj\n");
        }

        let mut buf: Vec<u8> = Vec::new();
        let mut offsets: Vec<usize> = vec![0]; // index 0 unused (object numbers are 1-based)

        buf.extend_from_slice(b"%PDF-1.4\n");

        let page_count = pages.len() as u32;
        let font_obj = 2 + page_count + 1;
        let kids: Vec<String> = (0..page_count)
            .map(|idx| format!("{} 0 R", 2 + idx + 1))
            .collect();

        push_obj(
            &mut buf,
            &mut offsets,
            1,
            "<< /Type /Catalog /Pages 2 0 R >>",
        );
        push_obj(
            &mut buf,
            &mut offsets,
            2,
            &format!(
                "<< /Type /Pages /Kids [{}] /Count {} >>",
                kids.join(" "),
                page_count
            ),
        );

        let content_obj_start = font_obj + 1;
        for (idx, _) in pages.iter().enumerate() {
            let page_num = 2 + idx as u32 + 1;
            let content_num = content_obj_start + idx as u32;
            push_obj(
                &mut buf,
                &mut offsets,
                page_num,
                &format!(
                    "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] \
                     /Resources << /Font << /F1 {font_obj} 0 R >> >> /Contents {content_num} 0 R >>"
                ),
            );
        }

        push_obj(
            &mut buf,
            &mut offsets,
            font_obj,
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
        );

        for (idx, page_body) in pages.iter().enumerate() {
            let content_num = content_obj_start + idx as u32;
            let stream = if page_body.is_empty() {
                String::from("BT ET")
            } else {
                format!("BT /F1 24 Tf 20 100 Td ({page_body}) Tj ET")
            };
            let body = format!(
                "<< /Length {} >>\nstream\n{stream}\nendstream",
                stream.len()
            );
            push_obj(&mut buf, &mut offsets, content_num, &body);
        }

        let xref_offset = buf.len();
        buf.extend_from_slice(b"xref\n");
        buf.extend_from_slice(format!("0 {}\n", offsets.len()).as_bytes());
        buf.extend_from_slice(b"0000000000 65535 f \n");
        for &off in &offsets[1..] {
            buf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        buf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF",
                offsets.len(),
                xref_offset
            )
            .as_bytes(),
        );

        buf
    }

    #[test]
    fn claims_pdf_extension_with_magic_header() {
        let bytes = build_pdf(&["hello"]);
        assert!(PdfChunker.claims(Path::new("doc.pdf"), &bytes));
        assert!(!PdfChunker.claims(Path::new("doc.txt"), &bytes));
        assert!(!PdfChunker.claims(Path::new("doc.pdf"), b"not a pdf"));
    }

    #[test]
    fn two_pages_produce_two_pdf_page_chunks_with_correct_page_numbers() {
        let bytes = build_pdf(&["Page One", "Page Two"]);
        let (chunks, edges) = PdfChunker
            .chunk(Path::new("doc.pdf"), &bytes)
            .expect("well-formed fixture PDF must not error");
        assert!(edges.is_empty());
        assert_eq!(chunks.len(), 2);

        assert_eq!(chunks[0].chunk_kind, "pdf_page");
        assert_eq!(chunks[0].line_start, Some(1));
        assert_eq!(chunks[0].line_end, Some(1));
        assert!(chunks[0].content.contains("Page One"));
        assert_eq!(chunks[0].occurrence_idx, 0);

        assert_eq!(chunks[1].chunk_kind, "pdf_page");
        assert_eq!(chunks[1].line_start, Some(2));
        assert_eq!(chunks[1].line_end, Some(2));
        assert!(chunks[1].content.contains("Page Two"));
        assert_eq!(chunks[1].occurrence_idx, 1);

        // Text-first: no language tag, so bucket routing (bbox-embed
        // enqueue_project_file) falls through to Docs rather than Code.
        assert!(chunks[0].language.is_none());
    }

    #[test]
    fn garbage_byte_stream_produces_no_chunks_and_does_not_panic() {
        let garbage = b"this is not a pdf, just some random bytes \x00\x01\x02 garbage";
        let (chunks, edges) = PdfChunker
            .chunk(Path::new("doc.pdf"), garbage)
            .expect("chunk() must never return Err for malformed input");
        assert!(chunks.is_empty());
        assert!(edges.is_empty());
    }

    #[test]
    fn empty_byte_stream_produces_no_chunks_and_does_not_panic() {
        let (chunks, edges) = PdfChunker
            .chunk(Path::new("doc.pdf"), b"")
            .expect("chunk() must never return Err for empty input");
        assert!(chunks.is_empty());
        assert!(edges.is_empty());
    }

    #[test]
    fn empty_text_pdf_produces_no_chunks() {
        // A structurally valid PDF whose single page's content stream shows
        // no text at all (no Tj/TJ operators): extractable text is empty,
        // so the page must be skipped rather than emitted as a blank chunk.
        let bytes = build_pdf(&[""]);
        let (chunks, edges) = PdfChunker
            .chunk(Path::new("doc.pdf"), &bytes)
            .expect("well-formed fixture PDF must not error");
        assert!(chunks.is_empty());
        assert!(edges.is_empty());
    }
}
