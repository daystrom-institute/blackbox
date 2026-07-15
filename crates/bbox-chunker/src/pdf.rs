use std::path::Path;
use std::time::Instant;

use anyhow::Result;

use super::{Chunk, Edge, SourceFormatChunker, placeholder_chunk};

/// Per the PDF spec the header must appear within the first 1024 bytes of
/// the file (some producers prepend junk bytes before it), so `claims`
/// scans the sniff window rather than requiring an exact byte-0 prefix.
const PDF_MAGIC: &[u8] = b"%PDF-";
const MAGIC_SCAN_WINDOW: usize = 1024;

/// Hard cap on OCR'd pages per document. Scanned corpora can be hundreds of
/// pages; a full reindex pass must stay bounded even when every page needs
/// the rasterize + recognize round trip.
const MAX_OCR_PAGES: u32 = 100;

/// Pages per `pdftoppm` invocation. Batching contiguous runs keeps process
/// spawns low while keeping each child's timeout window meaningful.
const OCR_BATCH_PAGES: u32 = 10;

/// PDF chunker (X-PDF,
/// `design/corpus/agentic-corpus/agentic-corpus-multimodal-chunkers.md`).
/// Text extraction first (`pdf-extract`); pages with no extractable text
/// fall back to an OCR shell-out (`pdftoppm` rasterize + `tesseract`
/// recognize) when both binaries are on PATH. The OCR path is
/// availability-gated and budgeted (per-process timeouts, per-document
/// wall-clock budget, `MAX_OCR_PAGES` cap) so a scanned corpus can never
/// wedge a reindex pass. Hosts without poppler/tesseract keep the previous
/// text-first behavior unchanged.
///
/// Encrypted or corrupt PDFs degrade to zero chunks rather than an error:
/// `SourceFormatChunker::chunk` returning `Err` propagates all the way up
/// and aborts the entire background reindex pass (not just this file), so
/// extraction failures are caught and swallowed here instead of bubbling
/// out. When text extraction fails wholesale but the bytes may still be a
/// renderable PDF (scanned docs with exotic text encodings), OCR probes
/// pages 1..=`MAX_OCR_PAGES` and stops at the first empty batch.
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
        let mut chunks = extract_page_chunks(path, bytes, &ocr::SystemOcr);
        chunks.extend(super::pdf_figure::extract_figure_chunks(path, bytes));
        Ok((chunks, Vec::new()))
    }
}

/// Pluggable OCR backend so the trigger/merge logic stays unit-testable
/// without host binaries. The real implementation is `ocr::SystemOcr`.
pub(crate) trait OcrEngine {
    fn available(&self) -> bool;
    /// OCR pages `first..=last` (1-based) of the PDF at `pdf_path`,
    /// stopping early once `deadline` passes. Returns
    /// `(page_number, recognized_text)` pairs; pages that fail to
    /// rasterize, time out, or recognize nothing may be omitted.
    fn ocr_range(
        &self,
        pdf_path: &Path,
        first: u32,
        last: u32,
        deadline: Instant,
    ) -> Vec<(u32, String)>;
}

/// One page's text plus where it came from, before chunk emission.
struct PageText {
    page: u32,
    text: String,
    from_ocr: bool,
}

/// Extract one `pdf_page` chunk per page carrying text: extracted text when
/// the page has any, OCR output otherwise (engine permitting). Pages that
/// yield no text either way are skipped rather than emitted empty.
///
/// No dedicated page field exists on `Chunk` (see the field doc comments in
/// `lib.rs`); `line_start`/`line_end` are the existing per-chunk position
/// slots for "non-line oriented sources... chunks without source-line
/// metadata", so both are set to the 1-based page number here as the
/// closest existing analog to how line-oriented chunkers carry position.
/// OCR-sourced chunks carry `symbol = Some("ocr")` as a queryable
/// provenance marker (the `symbol` slot is already used for free-form
/// labels, e.g. sheet names in the xlsx chunker).
fn extract_page_chunks(path: &Path, bytes: &[u8], engine: &dyn OcrEngine) -> Vec<Chunk> {
    // pdf-extract has known panics on some malformed/adversarial inputs
    // rather than returning an Err (upstream parser issues), so the call is
    // wrapped in catch_unwind on top of its own Result so corrupt input
    // never panics the indexing pass.
    let extraction =
        std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem_by_pages(bytes));
    let extracted: Option<Vec<String>> = match extraction {
        Ok(Ok(pages)) => Some(pages),
        Ok(Err(err)) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "pdf text extraction failed (encrypted, corrupt, or unsupported structure); \
                 trying OCR fallback if available"
            );
            None
        }
        Err(_) => {
            tracing::warn!(
                path = %path.display(),
                "pdf text extraction panicked; trying OCR fallback if available"
            );
            None
        }
    };

    let mut pages: Vec<PageText> = extracted
        .as_deref()
        .unwrap_or_default()
        .iter()
        .enumerate()
        .map(|(idx, text)| PageText {
            page: idx as u32 + 1,
            text: text.trim().to_string(),
            from_ocr: false,
        })
        .collect();

    // Probing (extraction failed entirely, page count unknown): scan from
    // page 1 and let the rasterizer tell us where the document ends.
    let probing = extracted.is_none();
    let ocr_targets: Vec<u32> = if probing {
        (1..=MAX_OCR_PAGES).collect()
    } else {
        pages
            .iter()
            .filter(|p| p.text.is_empty())
            .map(|p| p.page)
            .collect()
    };

    if !ocr_targets.is_empty() && engine.available() {
        for (page, text) in ocr::run(path, bytes, &ocr_targets, probing, engine) {
            let text = text.trim().to_string();
            if text.is_empty() {
                continue;
            }
            if probing {
                pages.push(PageText {
                    page,
                    text,
                    from_ocr: true,
                });
            } else if let Some(slot) = pages.iter_mut().find(|p| p.page == page) {
                slot.text = text;
                slot.from_ocr = true;
            }
        }
        if probing {
            pages.sort_by_key(|p| p.page);
        }
    }

    let mut chunks = Vec::new();
    let mut byte_offset = 0u64;
    for page_text in &pages {
        if page_text.text.is_empty() {
            continue;
        }
        let byte_start = byte_offset;
        let byte_end = byte_start + page_text.text.len() as u64;
        let mut chunk = placeholder_chunk(
            path,
            "pdf_page",
            None,
            page_text.text.clone(),
            byte_start,
            byte_end,
            chunks.len() as u32,
        );
        chunk.line_start = Some(page_text.page);
        chunk.line_end = Some(page_text.page);
        if page_text.from_ocr {
            chunk.symbol = Some("ocr".to_string());
        }
        byte_offset = byte_end + 1;
        chunks.push(chunk);
    }
    chunks
}

/// Split a sorted list of 1-based page numbers into `(first, last)` ranges:
/// contiguous runs, each further split so no range exceeds `max` pages.
fn contiguous_batches(pages: &[u32], max: u32) -> Vec<(u32, u32)> {
    debug_assert!(max >= 1);
    let mut out = Vec::new();
    let mut iter = pages.iter().copied();
    let Some(first) = iter.next() else {
        return out;
    };
    let (mut start, mut prev) = (first, first);
    for page in iter {
        let run_len = prev - start + 1;
        if page == prev + 1 && run_len < max {
            prev = page;
        } else {
            out.push((start, prev));
            start = page;
            prev = page;
        }
    }
    out.push((start, prev));
    out
}

/// OCR shell-out machinery. Executes inside the IndexWriterActor pass
/// (sanctioned single-writer blocking context), same as the rest of the
/// chunker call tree, hence the module-level allow for process spawns and
/// scratch-file I/O.
#[allow(clippy::disallowed_methods)]
mod ocr {
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    use super::{MAX_OCR_PAGES, OCR_BATCH_PAGES, OcrEngine, contiguous_batches};

    /// Rasterization DPI. 200 gray is the OCR sweet spot between tesseract's
    /// recommended 300 and rasterization cost on multi-hundred-page docs.
    const RASTER_DPI: &str = "200";
    /// Timeout for one `pdftoppm` batch (at most `OCR_BATCH_PAGES` pages).
    const RASTERIZE_TIMEOUT: Duration = Duration::from_secs(60);
    /// Timeout for one `tesseract` page recognition.
    const TESSERACT_TIMEOUT: Duration = Duration::from_secs(60);
    /// Wall-clock budget for all OCR work on a single document.
    const DOC_OCR_BUDGET: Duration = Duration::from_secs(300);

    /// Run OCR for `targets` (sorted 1-based page numbers) against the
    /// document bytes. Writes the bytes to a scratch file so the rasterizer
    /// sees exactly what was indexed (the on-disk source may have changed
    /// since the walker read it). `probing` means the page count is unknown
    /// (text extraction failed): stop at the first batch that renders
    /// nothing, which is how the end of the document manifests.
    pub(super) fn run(
        path: &Path,
        bytes: &[u8],
        targets: &[u32],
        probing: bool,
        engine: &dyn OcrEngine,
    ) -> Vec<(u32, String)> {
        let capped = if targets.len() as u32 > MAX_OCR_PAGES {
            tracing::warn!(
                path = %path.display(),
                candidate_pages = targets.len(),
                cap = MAX_OCR_PAGES,
                "pdf OCR page cap reached; pages beyond the cap are skipped"
            );
            &targets[..MAX_OCR_PAGES as usize]
        } else {
            targets
        };

        let scratch = match tempfile::tempdir() {
            Ok(dir) => dir,
            Err(err) => {
                tracing::warn!(error = %err, "pdf OCR scratch dir creation failed; skipping OCR");
                return Vec::new();
            }
        };
        let pdf_path = scratch.path().join("doc.pdf");
        if let Err(err) = std::fs::write(&pdf_path, bytes) {
            tracing::warn!(error = %err, "pdf OCR scratch write failed; skipping OCR");
            return Vec::new();
        }

        let deadline = Instant::now() + DOC_OCR_BUDGET;
        let mut recognized = Vec::new();
        for (first, last) in contiguous_batches(capped, OCR_BATCH_PAGES) {
            if Instant::now() >= deadline {
                tracing::warn!(
                    path = %path.display(),
                    budget_secs = DOC_OCR_BUDGET.as_secs(),
                    "pdf OCR document budget exhausted; remaining pages skipped"
                );
                break;
            }
            let batch = engine.ocr_range(&pdf_path, first, last, deadline);
            let batch_empty = batch.is_empty();
            recognized.extend(batch);
            if probing && batch_empty {
                break;
            }
        }
        recognized
    }

    /// The real engine: `pdftoppm` (poppler) rasterizes page ranges to
    /// grayscale PNGs, `tesseract` recognizes each page into a text file.
    /// Both children run with null stdio and a kill-on-timeout poll loop,
    /// so a hung binary can never wedge the reindex pass.
    pub(crate) struct SystemOcr;

    impl OcrEngine for SystemOcr {
        fn available(&self) -> bool {
            static AVAILABLE: OnceLock<bool> = OnceLock::new();
            *AVAILABLE.get_or_init(|| {
                let ok = find_in_path("pdftoppm").is_some() && find_in_path("tesseract").is_some();
                if !ok {
                    tracing::info!(
                        "pdf OCR fallback disabled: pdftoppm and/or tesseract not on PATH"
                    );
                }
                ok
            })
        }

        fn ocr_range(
            &self,
            pdf_path: &Path,
            first: u32,
            last: u32,
            deadline: Instant,
        ) -> Vec<(u32, String)> {
            let Ok(scratch) = tempfile::tempdir() else {
                return Vec::new();
            };
            let prefix = scratch.path().join("page");

            // Exit status is deliberately ignored beyond logging: poppler
            // exits non-zero for out-of-range pages (the probing case) and
            // a timeout kill may still leave completed pages behind. The
            // PNGs present in the scratch dir are the ground truth.
            let mut rasterize = Command::new("pdftoppm");
            rasterize
                .arg("-f")
                .arg(first.to_string())
                .arg("-l")
                .arg(last.to_string())
                .arg("-r")
                .arg(RASTER_DPI)
                .arg("-gray")
                .arg("-png")
                .arg(pdf_path)
                .arg(&prefix);
            run_to_completion(rasterize, RASTERIZE_TIMEOUT.min(remaining(deadline)));

            let mut rendered = rendered_pages(scratch.path());
            rendered.sort_by_key(|(page, _)| *page);

            let mut out = Vec::new();
            for (page, png) in rendered {
                let left = remaining(deadline);
                if left.is_zero() {
                    break;
                }
                let text_base = scratch.path().join(format!("ocr-{page}"));
                let mut recognize = Command::new("tesseract");
                recognize.arg(&png).arg(&text_base);
                if !run_to_completion(recognize, TESSERACT_TIMEOUT.min(left)) {
                    continue;
                }
                let Ok(raw) = std::fs::read_to_string(text_base.with_extension("txt")) else {
                    continue;
                };
                let text = raw.replace('\u{c}', "\n").trim().to_string();
                if !text.is_empty() {
                    out.push((page, text));
                }
            }
            out
        }
    }

    fn remaining(deadline: Instant) -> Duration {
        deadline.saturating_duration_since(Instant::now())
    }

    /// Collect `page-<N>.png` outputs from a `pdftoppm` scratch dir,
    /// parsing the (zero-padded) page number pdftoppm appends.
    fn rendered_pages(dir: &Path) -> Vec<(u32, PathBuf)> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(number) = name
                .strip_prefix("page-")
                .and_then(|rest| rest.strip_suffix(".png"))
            else {
                continue;
            };
            if let Ok(page) = number.parse::<u32>() {
                out.push((page, path));
            }
        }
        out
    }

    /// Spawn with null stdio, poll `try_wait`, kill at the timeout. Returns
    /// whether the child exited successfully within the window.
    fn run_to_completion(mut cmd: Command, timeout: Duration) -> bool {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let Ok(mut child) = cmd.spawn() else {
            return false;
        };
        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return status.success(),
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        tracing::warn!(
                            program = ?cmd.get_program(),
                            timeout_secs = timeout.as_secs(),
                            "pdf OCR child process timed out and was killed"
                        );
                        return false;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(_) => return false,
            }
        }
    }

    fn find_in_path(bin: &str) -> Option<PathBuf> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|dir| dir.join(bin))
            .find(|candidate| candidate.is_file())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

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

    /// Hermetic engine: canned per-page text, no processes.
    struct FakeOcr {
        pages: HashMap<u32, String>,
        available: bool,
    }

    impl FakeOcr {
        fn with_pages(pages: &[(u32, &str)]) -> Self {
            Self {
                pages: pages
                    .iter()
                    .map(|(page, text)| (*page, text.to_string()))
                    .collect(),
                available: true,
            }
        }

        fn unavailable() -> Self {
            Self {
                pages: HashMap::new(),
                available: false,
            }
        }
    }

    impl OcrEngine for FakeOcr {
        fn available(&self) -> bool {
            self.available
        }

        fn ocr_range(
            &self,
            _pdf_path: &Path,
            first: u32,
            last: u32,
            _deadline: Instant,
        ) -> Vec<(u32, String)> {
            let mut out: Vec<(u32, String)> = self
                .pages
                .iter()
                .filter(|(page, _)| (first..=last).contains(page))
                .map(|(page, text)| (*page, text.clone()))
                .collect();
            out.sort_by_key(|(page, _)| *page);
            out
        }
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
        let chunks = extract_page_chunks(Path::new("doc.pdf"), &bytes, &FakeOcr::unavailable());
        assert_eq!(chunks.len(), 2);

        assert_eq!(chunks[0].chunk_kind, "pdf_page");
        assert_eq!(chunks[0].line_start, Some(1));
        assert_eq!(chunks[0].line_end, Some(1));
        assert!(chunks[0].content.contains("Page One"));
        assert_eq!(chunks[0].occurrence_idx, 0);
        assert_eq!(chunks[0].symbol, None);

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
        let chunks = extract_page_chunks(Path::new("doc.pdf"), garbage, &FakeOcr::unavailable());
        assert!(chunks.is_empty());
    }

    #[test]
    fn empty_byte_stream_produces_no_chunks_and_does_not_panic() {
        let chunks = extract_page_chunks(Path::new("doc.pdf"), b"", &FakeOcr::unavailable());
        assert!(chunks.is_empty());
    }

    #[test]
    fn empty_text_pdf_produces_no_chunks_when_ocr_unavailable() {
        // A structurally valid PDF whose single page's content stream shows
        // no text at all (no Tj/TJ operators): extractable text is empty,
        // and with no OCR engine the page must be skipped rather than
        // emitted as a blank chunk.
        let bytes = build_pdf(&[""]);
        let chunks = extract_page_chunks(Path::new("doc.pdf"), &bytes, &FakeOcr::unavailable());
        assert!(chunks.is_empty());
    }

    #[test]
    fn textless_page_falls_back_to_ocr_in_page_order() {
        // Page 2 has no extractable text; the engine recognizes it. The
        // OCR chunk must land between the text chunks in page order and
        // carry the provenance marker.
        let bytes = build_pdf(&["Page One", "", "Page Three"]);
        let engine = FakeOcr::with_pages(&[(2, "Scanned middle page")]);
        let chunks = extract_page_chunks(Path::new("doc.pdf"), &bytes, &engine);
        assert_eq!(chunks.len(), 3);

        assert!(chunks[0].content.contains("Page One"));
        assert_eq!(chunks[0].symbol, None);

        assert_eq!(chunks[1].line_start, Some(2));
        assert_eq!(chunks[1].content, "Scanned middle page");
        assert_eq!(chunks[1].symbol, Some("ocr".to_string()));
        assert_eq!(chunks[1].occurrence_idx, 1);

        assert!(chunks[2].content.contains("Page Three"));
        assert_eq!(chunks[2].line_start, Some(3));
    }

    #[test]
    fn extraction_failure_probes_pages_via_ocr() {
        // Garbage bytes: text extraction fails wholesale, so the chunker
        // probes from page 1 and takes whatever the engine renders. The
        // fake stands in for a scanned-but-renderable document.
        let garbage = b"not a pdf at all";
        let engine = FakeOcr::with_pages(&[(1, "First scanned page"), (2, "Second scanned page")]);
        let chunks = extract_page_chunks(Path::new("doc.pdf"), garbage, &engine);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].line_start, Some(1));
        assert_eq!(chunks[0].content, "First scanned page");
        assert_eq!(chunks[0].symbol, Some("ocr".to_string()));
        assert_eq!(chunks[1].line_start, Some(2));
        assert_eq!(chunks[1].content, "Second scanned page");
    }

    #[test]
    fn ocr_result_for_unknown_page_is_ignored_outside_probing() {
        // Defensive: an engine returning a page number the document does
        // not have (possible with a confused rasterizer) must not panic or
        // fabricate a chunk when extraction succeeded.
        let bytes = build_pdf(&["Page One", ""]);
        let engine = FakeOcr::with_pages(&[(2, "Real"), (9, "Phantom")]);
        let chunks = extract_page_chunks(Path::new("doc.pdf"), &bytes, &engine);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[1].content, "Real");
    }

    #[test]
    fn contiguous_batches_split_runs_and_gaps() {
        assert_eq!(contiguous_batches(&[], 10), Vec::<(u32, u32)>::new());
        assert_eq!(contiguous_batches(&[3], 10), vec![(3, 3)]);
        assert_eq!(contiguous_batches(&[1, 2, 3], 10), vec![(1, 3)]);
        assert_eq!(
            contiguous_batches(&[1, 2, 5, 6, 9], 10),
            vec![(1, 2), (5, 6), (9, 9)]
        );
        // Runs longer than the batch cap split at the cap.
        assert_eq!(
            contiguous_batches(&[1, 2, 3, 4, 5], 2),
            vec![(1, 2), (3, 4), (5, 5)]
        );
    }

    /// End-to-end against the real binaries when the host has them; a
    /// silent no-op otherwise so CI and minimal hosts stay green. The
    /// fixture draws real vector text, so rasterize + recognize must read
    /// it back.
    #[test]
    fn system_ocr_reads_rendered_text_when_binaries_present() {
        let engine = ocr::SystemOcr;
        if !engine.available() {
            return;
        }
        let bytes = build_pdf(&["HELLO OCR WORLD"]);
        let dir = tempfile::tempdir().expect("tempdir");
        let pdf_path = dir.path().join("fixture.pdf");
        std::fs::write(&pdf_path, &bytes).expect("write fixture");
        let recognized = engine.ocr_range(
            &pdf_path,
            1,
            1,
            Instant::now() + std::time::Duration::from_secs(120),
        );
        assert_eq!(recognized.len(), 1);
        assert_eq!(recognized[0].0, 1);
        assert!(
            recognized[0].1.to_uppercase().contains("HELLO"),
            "tesseract output was: {:?}",
            recognized[0].1
        );
    }
}
