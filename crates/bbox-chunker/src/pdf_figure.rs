//! `pdf_figure` - embedded raster XObject extraction for PDFs (X-PDF,
//! `design/corpus/agentic-corpus/agentic-corpus-multimodal-chunkers.md`).
//! Deliberately separate from `pdf.rs`'s text extraction: `pdf.rs` owns
//! `pdf_page` chunks via `pdf-extract`; this module owns `pdf_figure`
//! chunks via `lopdf`'s page/XObject reading API. `pdf.rs::PdfChunker::chunk`
//! calls [`extract_figure_chunks`] and appends the result - the only
//! coupling between the two modules is that one call site.
//!
//! Only the "easy encodings" per the design doc's scope are handled:
//!
//! - `DCTDecode` (baseline JPEG): the stream content IS already valid JPEG
//!   bytes - passthrough, no decode/re-encode.
//! - `FlateDecode` with `DeviceGray`/`DeviceRGB`, 8 bits per component, and
//!   NO PNG/TIFF predictor (`Predictor` absent or `1`): the stream
//!   decompresses to raw interleaved scanlines, which are re-encoded into a
//!   minimal hand-built PNG (a predictor means the decompressed bytes are
//!   already per-row-filtered in a scheme this module does not reverse,
//!   see [`has_non_trivial_predictor`]).
//!
//! Everything else (JPXDecode/JPEG2000, CCITTFax, indexed/ICC/CMYK color
//! spaces, 16-bit or 1-bit components, predictor-compressed Flate streams)
//! is skipped: the image is silently omitted rather than mis-decoded.
//!
//! Encrypted, corrupt, or unparsable PDFs degrade to zero figure chunks,
//! same convention as `pdf.rs`'s text extraction (a chunker `Err` aborts
//! the whole background reindex pass, not just this file).

use std::path::Path;

use lopdf::{Dictionary, Document, Object};

use super::{Chunk, placeholder_chunk};

pub const PDF_FIGURE_CHUNK_KIND: &str = "pdf_figure";

/// Extract `pdf_figure` chunks from a PDF's embedded raster XObjects,
/// storing each one's bytes in the visual payload sidecar. Never returns
/// `Err` - parse/extraction failures degrade to an empty result, matching
/// `pdf.rs`'s degrade-not-fail convention for background reindex safety.
pub fn extract_figure_chunks(path: &Path, bytes: &[u8]) -> Vec<Chunk> {
    // lopdf parses untrusted binary input; wrap in catch_unwind on top of
    // its own Result for the same reason pdf-extract's call is wrapped in
    // pdf.rs (upstream parser panics on adversarial input must not take
    // down the whole reindex pass).
    let extraction = std::panic::catch_unwind(|| extract_figure_chunks_inner(path, bytes));
    match extraction {
        Ok(chunks) => chunks,
        Err(_) => {
            tracing::warn!(
                path = %path.display(),
                "pdf figure extraction panicked; skipping figures for this file"
            );
            Vec::new()
        }
    }
}

fn extract_figure_chunks_inner(path: &Path, bytes: &[u8]) -> Vec<Chunk> {
    let document = match Document::load_mem(bytes) {
        Ok(document) => document,
        Err(err) => {
            tracing::debug!(
                path = %path.display(),
                error = %err,
                "pdf figure extraction: document did not parse (encrypted, corrupt, or unsupported structure); skipping"
            );
            return Vec::new();
        }
    };

    let mut chunks = Vec::new();
    let mut occurrence_idx = 0u32;
    for (page_number, page_id) in document.get_pages() {
        let images = match document.get_page_images(page_id) {
            Ok(images) => images,
            Err(err) => {
                tracing::debug!(
                    path = %path.display(),
                    page_number,
                    error = %err,
                    "pdf figure extraction: page image lookup failed; skipping this page's figures"
                );
                continue;
            }
        };
        for image in images {
            let Some((payload_bytes, media_type)) = decode_supported_image(&image) else {
                continue;
            };
            let payload = match bbox_visual_store::global().put(&payload_bytes, media_type) {
                Ok(payload) => payload,
                Err(err) => {
                    tracing::warn!(
                        path = %path.display(),
                        page_number,
                        error = %err,
                        "visual payload store write failed; skipping pdf figure"
                    );
                    continue;
                }
            };
            let content = format!("figure on page {page_number}");
            let byte_len = content.len() as u64;
            let mut chunk = placeholder_chunk(
                path,
                PDF_FIGURE_CHUNK_KIND,
                None,
                content,
                0,
                byte_len,
                occurrence_idx,
            );
            chunk.line_start = Some(page_number);
            chunk.line_end = Some(page_number);
            chunk.symbol = Some(payload.encode());
            chunk.visual_payload = Some(payload);
            chunks.push(chunk);
            occurrence_idx += 1;
        }
    }
    chunks
}

/// Decode one `lopdf::xobject::PdfImage` into `(bytes, media_type)` when its
/// filter/color-space/bit-depth combination is one of the "easy encodings";
/// `None` for anything else (see the module doc comment for the exact
/// supported matrix).
fn decode_supported_image(image: &lopdf::xobject::PdfImage<'_>) -> Option<(Vec<u8>, &'static str)> {
    let filters = image.filters.as_deref().unwrap_or_default();
    match filters {
        // DCTDecode content is already a complete baseline JPEG stream.
        [filter] if filter == "DCTDecode" => Some((image.content.to_vec(), "image/jpeg")),
        [filter] if filter == "FlateDecode" => decode_flate_raster(image),
        _ => None,
    }
}

/// FlateDecode raster passthrough: only 8-bit DeviceGray/DeviceRGB with no
/// predictor. A `Predictor` other than 1 (or a `Predictor` key at all) means
/// the decompressed bytes are per-row-filtered (PNG or TIFF predictor
/// scheme) rather than raw scanlines; reversing that is out of scope for
/// this pass, so those streams are skipped rather than mis-decoded into
/// garbage pixels.
fn decode_flate_raster(image: &lopdf::xobject::PdfImage<'_>) -> Option<(Vec<u8>, &'static str)> {
    if has_non_trivial_predictor(image.origin_dict) {
        return None;
    }
    let channels: usize = match image.color_space.as_deref() {
        Some("DeviceGray") => 1,
        Some("DeviceRGB") => 3,
        _ => return None,
    };
    if image.bits_per_component != Some(8) {
        return None;
    }
    let width = usize::try_from(image.width).ok()?;
    let height = usize::try_from(image.height).ok()?;
    if width == 0 || height == 0 {
        return None;
    }
    let raw = decode_zlib(image.content)?;
    let expected_len = width.checked_mul(height)?.checked_mul(channels)?;
    if raw.len() != expected_len {
        // Length mismatch means an assumption above doesn't hold for this
        // stream (e.g. an undeclared predictor, or a partial/corrupt
        // stream) - skip rather than encode a corrupt image.
        return None;
    }
    let color_type = if channels == 1 { 0 } else { 2 };
    let png = encode_png(width as u32, height as u32, color_type, channels, &raw)?;
    Some((png, "image/png"))
}

fn has_non_trivial_predictor(dict: &Dictionary) -> bool {
    let Ok(params) = dict.get(b"DecodeParms") else {
        return false;
    };
    let predictor_of =
        |dict: &Dictionary| -> Option<i64> { dict.get(b"Predictor").ok()?.as_i64().ok() };
    let predictor = match params {
        Object::Dictionary(dict) => predictor_of(dict),
        Object::Array(items) => items.iter().find_map(|item| match item {
            Object::Dictionary(dict) => predictor_of(dict),
            _ => None,
        }),
        _ => None,
    };
    !matches!(predictor, None | Some(1))
}

fn decode_zlib(bytes: &[u8]) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut decoder = flate2::read::ZlibDecoder::new(bytes);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).ok()?;
    Some(out)
}

/// Minimal PNG encoder: signature + IHDR + one IDAT (all scanlines, filter
/// type 0/None, single zlib stream) + IEND. `color_type` is PNG's own value
/// (0 = grayscale, 2 = truecolor); `channels` must match it (1 or 3). No
/// interlacing, no palette, no ancillary chunks - just enough for a
/// `voyage-multimodal-3.5` request to decode it as a normal image.
fn encode_png(
    width: u32,
    height: u32,
    color_type: u8,
    channels: usize,
    raw: &[u8],
) -> Option<Vec<u8>> {
    use std::io::Write;

    let stride = (width as usize).checked_mul(channels)?;
    if stride == 0 || raw.len() != stride.checked_mul(height as usize)? {
        return None;
    }
    let mut filtered = Vec::with_capacity(raw.len() + height as usize);
    for row in raw.chunks_exact(stride) {
        filtered.push(0u8); // filter type: None
        filtered.extend_from_slice(row);
    }
    let mut compressed = Vec::new();
    {
        let mut encoder =
            flate2::write::ZlibEncoder::new(&mut compressed, flate2::Compression::fast());
        encoder.write_all(&filtered).ok()?;
        encoder.finish().ok()?;
    }

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.push(8); // bit depth
    ihdr.push(color_type);
    ihdr.push(0); // compression method (deflate, the only defined value)
    ihdr.push(0); // filter method (adaptive, the only defined value)
    ihdr.push(0); // interlace method (none)

    let mut png = Vec::new();
    png.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    write_png_chunk(&mut png, b"IHDR", &ihdr);
    write_png_chunk(&mut png, b"IDAT", &compressed);
    write_png_chunk(&mut png, b"IEND", &[]);
    Some(png)
}

fn write_png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(kind);
    hasher.update(data);
    out.extend_from_slice(&hasher.finalize().to_be_bytes());
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn with_store<T>(f: impl FnOnce() -> T) -> T {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(bbox_visual_store::VisualPayloadStore::open(
            dir.path().to_path_buf(),
        ));
        let _guard = bbox_visual_store::install_test_global(store);
        f()
    }

    /// Hand-rolled minimal PDF with one page whose Resources carry a single
    /// image XObject. `image_dict_extra` is spliced into the XObject's
    /// dictionary (`/Filter ... /ColorSpace ...` etc); `image_bytes` is the
    /// raw (already-encoded per that filter) stream content. Byte-oriented
    /// throughout (unlike pdf.rs's text-only `build_pdf`) since stream
    /// content here is binary, not valid UTF-8.
    fn build_pdf_with_image(image_dict_extra: &str, image_bytes: &[u8]) -> Vec<u8> {
        let mut buf: Vec<u8> = Vec::new();
        let mut offsets: Vec<usize> = vec![0];

        let mut push_obj = |buf: &mut Vec<u8>, num: u32, head: &[u8], body: &[u8], tail: &[u8]| {
            offsets.push(buf.len());
            buf.extend_from_slice(format!("{num} 0 obj\n").as_bytes());
            buf.extend_from_slice(head);
            buf.extend_from_slice(body);
            buf.extend_from_slice(tail);
            buf.extend_from_slice(b"\nendobj\n");
        };

        buf.extend_from_slice(b"%PDF-1.4\n");
        push_obj(&mut buf, 1, b"<< /Type /Catalog /Pages 2 0 R >>", b"", b"");
        push_obj(
            &mut buf,
            2,
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            b"",
            b"",
        );
        push_obj(
            &mut buf,
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] \
               /Resources << /XObject << /Im1 5 0 R >> >> /Contents 4 0 R >>",
            b"",
            b"",
        );
        let content_stream = b"q 100 0 0 100 0 0 cm /Im1 Do Q";
        push_obj(
            &mut buf,
            4,
            format!("<< /Length {} >>\nstream\n", content_stream.len()).as_bytes(),
            content_stream,
            b"\nendstream",
        );
        let image_head = format!(
            "<< /Type /XObject /Subtype /Image {image_dict_extra} /Length {} >>\nstream\n",
            image_bytes.len()
        );
        push_obj(
            &mut buf,
            5,
            image_head.as_bytes(),
            image_bytes,
            b"\nendstream",
        );

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

    fn tiny_jpeg_bytes() -> Vec<u8> {
        // Not a decodable JPEG (no scan data) - this module never decodes
        // DCTDecode content, only passes it through, so a header-only
        // fixture is enough to prove the passthrough path.
        vec![
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0xFF, 0xD9,
        ]
    }

    fn flate_compress(raw: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(raw).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn dct_decode_image_passes_through_as_jpeg() {
        with_store(|| {
            let jpeg = tiny_jpeg_bytes();
            let bytes = build_pdf_with_image(
                "/Width 4 /Height 4 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode",
                &jpeg,
            );
            let chunks = extract_figure_chunks(Path::new("doc.pdf"), &bytes);
            assert_eq!(chunks.len(), 1);
            let chunk = &chunks[0];
            assert_eq!(chunk.chunk_kind, PDF_FIGURE_CHUNK_KIND);
            assert_eq!(chunk.line_start, Some(1));
            let payload = chunk.visual_payload.as_ref().unwrap();
            assert_eq!(payload.media_type, "image/jpeg");
            let stored =
                std::fs::read(bbox_visual_store::global().path_for(&payload.content_hash)).unwrap();
            assert_eq!(stored, jpeg);
        });
    }

    #[test]
    fn flate_decode_rgb_image_is_reencoded_as_png() {
        with_store(|| {
            let width = 2u32;
            let height = 2u32;
            // 2x2 RGB: red, green, blue, white.
            let raw: [u8; 12] = [255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255];
            let compressed = flate_compress(&raw);
            let bytes = build_pdf_with_image(
                &format!(
                    "/Width {width} /Height {height} /ColorSpace /DeviceRGB \
                     /BitsPerComponent 8 /Filter /FlateDecode"
                ),
                &compressed,
            );
            let chunks = extract_figure_chunks(Path::new("doc.pdf"), &bytes);
            assert_eq!(chunks.len(), 1);
            let payload = chunks[0].visual_payload.as_ref().unwrap();
            assert_eq!(payload.media_type, "image/png");
            let stored =
                std::fs::read(bbox_visual_store::global().path_for(&payload.content_hash)).unwrap();
            // PNG signature + IHDR present; width/height round-trip.
            assert!(stored.starts_with(&[0x89, b'P', b'N', b'G']));
            assert_eq!(&stored[16..20], &width.to_be_bytes());
            assert_eq!(&stored[20..24], &height.to_be_bytes());
        });
    }

    #[test]
    fn flate_decode_with_predictor_is_skipped() {
        with_store(|| {
            let raw = [0u8; 12];
            let compressed = flate_compress(&raw);
            let bytes = build_pdf_with_image(
                "/Width 2 /Height 2 /ColorSpace /DeviceRGB /BitsPerComponent 8 \
                 /Filter /FlateDecode /DecodeParms << /Predictor 15 /Colors 3 /Columns 2 >>",
                &compressed,
            );
            let chunks = extract_figure_chunks(Path::new("doc.pdf"), &bytes);
            assert!(
                chunks.is_empty(),
                "predictor-compressed streams are out of scope; must not emit a corrupt image"
            );
        });
    }

    #[test]
    fn unsupported_color_space_is_skipped() {
        with_store(|| {
            let raw = [0u8; 4]; // 2x2 indexed, 1 byte/pixel
            let compressed = flate_compress(&raw);
            let bytes = build_pdf_with_image(
                "/Width 2 /Height 2 /ColorSpace /Indexed /BitsPerComponent 8 /Filter /FlateDecode",
                &compressed,
            );
            let chunks = extract_figure_chunks(Path::new("doc.pdf"), &bytes);
            assert!(chunks.is_empty());
        });
    }

    #[test]
    fn garbage_bytes_produce_no_figure_chunks_and_do_not_panic() {
        let garbage = b"this is not a pdf at all \x00\x01\x02";
        let chunks = extract_figure_chunks(Path::new("doc.pdf"), garbage);
        assert!(chunks.is_empty());
    }

    #[test]
    fn grayscale_flate_image_is_reencoded_as_png() {
        with_store(|| {
            let raw: [u8; 4] = [0, 85, 170, 255]; // 2x2 grayscale gradient
            let compressed = flate_compress(&raw);
            let bytes = build_pdf_with_image(
                "/Width 2 /Height 2 /ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode",
                &compressed,
            );
            let chunks = extract_figure_chunks(Path::new("doc.pdf"), &bytes);
            assert_eq!(chunks.len(), 1);
            let payload = chunks[0].visual_payload.as_ref().unwrap();
            assert_eq!(payload.media_type, "image/png");
            let stored =
                std::fs::read(bbox_visual_store::global().path_for(&payload.content_hash)).unwrap();
            assert_eq!(stored[25], 0, "grayscale PNG color type is 0");
        });
    }
}
